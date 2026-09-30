//! Integration tests for the Odoo label source (#90) against a fake Odoo
//! HTTP server that speaks the agreed contract (JSON-RPC 2.0 envelope,
//! Bearer key, both error kinds with HTTP 200, `{"lines": []}` when empty,
//! idempotent ack with `duplicate: true`).
//!
//! The Windows spooler is the one external system replaced here (it does not
//! exist on the Linux CI runner): `FakeSpooler` records the exact bytes the
//! source hands to the print backend and reports an EventID 307-shaped
//! verification. The real spooler path is covered by the E2E step on pz-snv.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use devbridge_client::odoo_source::ledger::{INTERRUPTED_REASON, Ledger};
use devbridge_client::odoo_source::tspl::{self, LabelGeometry};
use devbridge_client::odoo_source::{OdooSource, OdooSourceDeps, PollOutcome, timed_out_ack_text};
use devbridge_client::print_backend::{PrintBackend, PrintJobInfo};
use devbridge_client::print_lock::PrintLock;
use devbridge_core::config::OdooClientConfig;
use devbridge_core::job::JobState;
use devbridge_core::job_event::EventEmitter;
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

const KEY: &str = "fake-odoo-test-key-7f3a";
const SPISSKA: LabelGeometry = LabelGeometry {
    width_mm: 72.7,
    height_mm: 110.1,
    dpi: 203,
    rotate_180: true,
    x_offset_dots: 0,
    y_offset_dots: 0,
};

// ── fake Odoo ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeOdoo {
    batch_id: i64,
    /// Lines `/next` still returns (removed once acked, unless `keep_after_ack`).
    lines: Vec<Value>,
    acks: Vec<Value>,
    heartbeats: Vec<Value>,
    /// Final state already stored per line (drives `duplicate: true`).
    stored: HashMap<i64, Value>,
    /// Answer the next N acks with HTTP 502.
    fail_acks: usize,
    /// Answer every ack with `result.error` (business refusal).
    ack_refusal: Option<String>,
    /// Accept acks but keep returning the lines (a person re-queued them).
    keep_after_ack: bool,
    /// Answer acks of these lines with a top-level JSON-RPC error (the line
    /// stays waiting in Odoo).
    ack_rpc_error_for: HashSet<i64>,
}

type Shared = Arc<Mutex<FakeOdoo>>;

fn rpc_ok(result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": result})).into_response()
}

/// Auth failure the Odoo way: HTTP 200 with a top-level JSON-RPC `error`.
fn auth(headers: &HeaderMap) -> Option<Response> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {KEY}"));
    (!ok).then(|| {
        Json(json!({"jsonrpc": "2.0", "id": null, "error": {
            "code": 100, "message": "Odoo Server Error",
            "data": {"name": "odoo.exceptions.AccessDenied", "message": "Access Denied"}}}))
        .into_response()
    })
}

fn params(body: &Value) -> Value {
    assert_eq!(
        body["jsonrpc"], "2.0",
        "request must be a JSON-RPC 2.0 envelope: {body}"
    );
    assert_eq!(body["method"], "call", "{body}");
    body["params"].clone()
}

async fn next(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = auth(&headers) {
        return r;
    }
    assert_eq!(params(&body), json!({}));
    let s = st.lock().unwrap();
    if s.lines.is_empty() {
        return rpc_ok(json!({"lines": []}));
    }
    rpc_ok(json!({"batch_id": s.batch_id, "production_date": "2026-09-29", "lines": s.lines}))
}

async fn ack(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = auth(&headers) {
        return r;
    }
    let p = params(&body);
    let mut s = st.lock().unwrap();
    s.acks.push(p.clone());
    if s.fail_acks > 0 {
        s.fail_acks -= 1;
        return (StatusCode::BAD_GATEWAY, "upstream down").into_response();
    }
    if let Some(text) = s.ack_refusal.clone() {
        return rpc_ok(json!({"error": text}));
    }
    let line_id = p["line_id"].as_i64().expect("ack line_id");
    if s.ack_rpc_error_for.contains(&line_id) {
        return Json(json!({"jsonrpc": "2.0", "id": null, "error": {
            "code": 200, "message": "Odoo Server Error",
            "data": {"message": "MissingError: record deleted"}}}))
        .into_response();
    }
    let state = json!({"printed_qty": p["printed_qty"], "error": p["error"]});
    let duplicate = s.stored.get(&line_id) == Some(&state);
    s.stored.insert(line_id, state);
    if !s.keep_after_ack {
        s.lines.retain(|l| l["line_id"].as_i64() != Some(line_id));
    }
    if duplicate {
        rpc_ok(json!({"ok": true, "duplicate": true}))
    } else {
        rpc_ok(json!({"ok": true}))
    }
}

async fn heartbeat(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = auth(&headers) {
        return r;
    }
    st.lock().unwrap().heartbeats.push(params(&body));
    rpc_ok(json!({"ok": true, "printer_id": 7}))
}

async fn start_fake(state: Shared) -> String {
    let app = Router::new()
        .route("/food/print/next", post(next))
        .route("/food/print/ack", post(ack))
        .route("/food/print/heartbeat", post(heartbeat))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ── fake spooler (the one non-HTTP external system) ────────────────────────

struct FakeSpooler {
    docs: Mutex<Vec<Vec<u8>>>,
    fail: Mutex<Option<String>>,
    /// Simulated spooler hang (sleep after taking the bytes).
    delay: Mutex<Option<Duration>>,
    lock: PrintLock,
    lock_held_during_print: Mutex<Vec<bool>>,
}

impl FakeSpooler {
    fn new(lock: PrintLock) -> Arc<Self> {
        Arc::new(Self {
            docs: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            delay: Mutex::new(None),
            lock,
            lock_held_during_print: Mutex::new(Vec::new()),
        })
    }
    fn docs(&self) -> Vec<Vec<u8>> {
        self.docs.lock().unwrap().clone()
    }
}

impl PrintBackend for FakeSpooler {
    fn name(&self) -> &str {
        "fake_spooler"
    }
    fn print(
        &self,
        job: &PrintJobInfo,
        path: &Path,
        events: &EventEmitter,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        self.lock_held_during_print
            .lock()
            .unwrap()
            .push(self.lock.is_held());
        let bytes = std::fs::read(path)?;
        let len = bytes.len();
        self.docs.lock().unwrap().push(bytes);
        let delay = *self.delay.lock().unwrap();
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        if let Some(reason) = self.fail.lock().unwrap().clone() {
            anyhow::bail!("{reason}");
        }
        events.emit_verified(
            &job.job_id,
            "eventid_307",
            format!(
                "EventID 307: spooler job 9 on {} via port NUL:, {len} bytes (expected {len})",
                job.printer_name
            ),
        );
        Ok(())
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("devbridge-odoo-it-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 1-bit grayscale PNG, `black_rows` of black at the top, rest white.
fn png_b64(width: u32, height: u32, black_rows: u32) -> String {
    let wb = width.div_ceil(8) as usize;
    let mut data = vec![0xFFu8; wb * height as usize];
    for b in data.iter_mut().take(wb * black_rows as usize) {
        *b = 0x00;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        enc.write_header().unwrap().write_image_data(&data).unwrap();
    }
    base64::engine::general_purpose::STANDARD.encode(out)
}

fn line(line_id: i64, sequence: i64, qty: i64, png: &str, line_type: &str) -> Value {
    json!({"line_id": line_id, "sequence": sequence, "product_name": format!("Produkt {line_id}"),
           "print_qty": qty, "best_before": "03.10.2026", "label_png_base64": png,
           "line_type": line_type})
}

fn config(url: &str) -> OdooClientConfig {
    OdooClientConfig {
        enabled: true,
        url: url.to_string(),
        api_key: KEY.to_string(),
        poll_interval_secs: 1,
        heartbeat_interval_secs: 1,
        printer_name: "TSC ML241P Spišská".to_string(),
        ..Default::default()
    }
}

struct Rig {
    odoo: Shared,
    url: String,
    spooler: Arc<FakeSpooler>,
    lock: PrintLock,
    dir: PathBuf,
    queue: Arc<JobQueue>,
    print_timeout: Duration,
}

impl Rig {
    async fn new(tag: &str) -> Self {
        let odoo: Shared = Arc::new(Mutex::new(FakeOdoo {
            batch_id: 12,
            ..Default::default()
        }));
        let url = start_fake(Arc::clone(&odoo)).await;
        let lock = PrintLock::new();
        let dir = temp_dir(tag);
        let queue =
            Arc::new(JobQueue::new(Storage::new(&dir.join("devbridge.db")).unwrap()).unwrap());
        Self {
            odoo,
            url,
            spooler: FakeSpooler::new(lock.clone()),
            lock,
            dir,
            queue,
            print_timeout: Duration::from_secs(30),
        }
    }

    fn source_with_key(&self, key: &str) -> OdooSource {
        let ledger = Ledger::open(&self.dir.join("odoo-ledger.db")).unwrap();
        let mut cfg = config(&self.url);
        cfg.api_key = key.to_string();
        let backend: Arc<dyn PrintBackend> = self.spooler.clone();
        OdooSource::new(
            cfg,
            OdooSourceDeps {
                backend,
                target_printer: Arc::new(RwLock::new("DevBridge-E2E-Raw".to_string())),
                print_lock: self.lock.clone(),
                queue: Some(Arc::clone(&self.queue)),
                spool_dir: self.dir.join("spool"),
                ledger: Arc::new(ledger),
                print_timeout: self.print_timeout,
            },
        )
        .unwrap()
    }

    fn source(&self) -> OdooSource {
        self.source_with_key(KEY)
    }

    fn set_lines(&self, lines: Vec<Value>) {
        self.odoo.lock().unwrap().lines = lines;
    }

    fn acks(&self) -> Vec<Value> {
        self.odoo.lock().unwrap().acks.clone()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn expected_doc(labels: &[(&str, u32)]) -> Vec<u8> {
    let bitmaps: Vec<_> = labels
        .iter()
        .map(|(b64, _)| tspl::decode_label(b64, &SPISSKA).unwrap())
        .collect();
    let pairs: Vec<_> = bitmaps
        .iter()
        .zip(labels)
        .map(|(b, (_, q))| (b, *q))
        .collect();
    tspl::build_document(&SPISSKA, &pairs)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_batch_prints_as_one_tspl_document_and_every_line_is_acked() {
    let rig = Rig::new("happy").await;
    let product_a = png_b64(576, 880, 10);
    let product_b = png_b64(576, 880, 20);
    let separator = png_b64(576, 880, 440);
    let too_big = png_b64(584, 880, 1);
    rig.set_lines(vec![
        // deliberately out of order — printed by `sequence`
        line(302, 20, 1, &product_b, "product"),
        line(301, 10, 2, &product_a, "product"),
        line(303, 30, 1, &separator, "separator"),
        line(304, 40, 1, "", "separator"),
        line(305, 50, 1, &too_big, "adhoc"),
    ]);
    let source = rig.source();

    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 3,
            rejected: 2,
            reacked: 0
        }
    );

    // ONE spooler document, header once, labels in sequence order.
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1, "one Odoo batch = one spooler document");
    let want = expected_doc(&[
        (product_a.as_str(), 2),
        (product_b.as_str(), 1),
        (separator.as_str(), 1),
    ]);
    assert_eq!(docs[0], want);
    let text = String::from_utf8_lossy(&docs[0]);
    assert!(text.starts_with("SIZE 72.7 mm, 110.1 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\nCLS\r\nBITMAP 2,0,72,880,0,"));
    assert_eq!(text.matches("SIZE ").count(), 1);
    assert_eq!(text.matches("BITMAP 2,0,72,880,0,").count(), 3);
    let prints: Vec<&str> = text.matches("PRINT 1,").collect();
    assert_eq!(prints.len(), 3);
    assert!(text.contains("PRINT 1,2\r\nCLS\r\n") && text.ends_with("PRINT 1,1\r\n"));
    assert!(!text.contains("GAP") && !text.contains("DENSITY") && !text.contains("SPEED"));
    assert_eq!(
        rig.spooler.lock_held_during_print.lock().unwrap().clone(),
        vec![true]
    );
    assert!(!rig.lock.is_held(), "lock released after the print");

    // Every line acked exactly once with the agreed mapping.
    let acks: HashMap<i64, Value> = rig
        .acks()
        .into_iter()
        .map(|a| (a["line_id"].as_i64().unwrap(), a))
        .collect();
    assert_eq!(rig.acks().len(), 5);
    assert_eq!(
        acks[&301],
        json!({"line_id": 301, "printed_qty": 2, "error": null})
    );
    assert_eq!(
        acks[&302],
        json!({"line_id": 302, "printed_qty": 1, "error": null})
    );
    assert_eq!(
        acks[&303],
        json!({"line_id": 303, "printed_qty": 1, "error": null})
    );
    assert_eq!(
        acks[&304],
        json!({"line_id": 304, "printed_qty": 0, "error": "empty png"})
    );
    assert_eq!(
        acks[&305],
        json!({"line_id": 305, "printed_qty": 0, "error": "size"})
    );

    // Client history shows the batch job with the 307 evidence.
    let jobs = rig.queue.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert!(job.job_id.starts_with("odoo-12-"), "{}", job.job_id);
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(job.payload_size, want.len() as u64);
    assert_eq!(job.target_printer, "DevBridge-E2E-Raw");
    assert!(
        job.document_name
            .contains("Odoo batch 12 (3 labels, 4 copies)"),
        "{}",
        job.document_name
    );
    let events = rig.queue.get_job_events(&job.job_id).unwrap();
    assert!(
        events.iter().any(|e| e.verification_method == "eventid_307"
            && e.verification_evidence
                .contains(&format!("{} bytes", want.len()))),
        "{events:?}"
    );
    // spool file removed after the print
    let left: Vec<_> = std::fs::read_dir(rig.dir.join("spool")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");

    let status = source.status();
    assert_eq!(status.last_job_id, Some(12));
    assert!(
        status
            .last_result
            .starts_with("printed 3 labels (4 copies)"),
        "{}",
        status.last_result
    );

    // Queue drained → idle, nothing printed again.
    assert_eq!(source.poll_once().await.unwrap(), PollOutcome::Idle);
    assert_eq!(rig.spooler.docs().len(), 1);
}

#[tokio::test]
async fn test_ack_network_drop_is_retried_without_reprinting() {
    let rig = Rig::new("netdrop").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![
        line(401, 10, 3, &png, "product"),
        line(402, 20, 1, &png, "product"),
    ]);
    rig.odoo.lock().unwrap().fail_acks = 1;
    let source = rig.source();

    let err = source.poll_once().await.unwrap_err();
    assert!(format!("{err:#}").contains("kept for retry"), "{err:#}");
    assert!(format!("{err:#}").contains("502"), "{err:#}");
    assert_eq!(rig.spooler.docs().len(), 1);

    // Next cycle: owed acks go out first, then Odoo has nothing left.
    assert_eq!(source.poll_once().await.unwrap(), PollOutcome::Idle);
    assert_eq!(
        rig.spooler.docs().len(),
        1,
        "a lost ack must never cause a reprint"
    );
    let acked: HashSet<i64> = rig.odoo.lock().unwrap().stored.keys().copied().collect();
    assert_eq!(acked, HashSet::from([401, 402]));
}

#[tokio::test]
async fn test_reoffered_sent_line_is_never_reprinted_but_an_unsent_one_is_reprocessed() {
    let rig = Rig::new("reoffer").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![
        line(501, 10, 2, &png, "product"),
        line(502, 20, 1, "", "separator"),
    ]);
    // Odoo stores every ack but keeps offering the lines (a person re-queued
    // them, or an Odoo-side bug — the client cannot tell).
    rig.odoo.lock().unwrap().keep_after_ack = true;
    let source = rig.source();

    source.poll_once().await.unwrap();
    assert_eq!(rig.acks().len(), 2);
    assert_eq!(rig.spooler.docs().len(), 1);
    // 501 reached the printer: never again, its result is re-sent. 502 never
    // did: looked at anew (still an empty PNG → rejected again).
    let again = source.poll_once().await.unwrap();
    assert_eq!(
        again,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 0,
            rejected: 1,
            reacked: 1
        }
    );
    assert_eq!(
        rig.spooler.docs().len(),
        1,
        "a sent line is never reprinted"
    );
    let acks = rig.acks();
    assert_eq!(acks.len(), 4, "{acks:?}");
    assert_eq!(acks[2], acks[0]);
    assert_eq!(acks[3], acks[1]);
    // Odoo fixes 502's PNG: now it prints — once — and 501 still does not.
    {
        let mut o = rig.odoo.lock().unwrap();
        o.lines[1]["label_png_base64"] = json!(png.clone());
    }
    let fixed = source.poll_once().await.unwrap();
    assert_eq!(
        fixed,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 1,
            rejected: 0,
            reacked: 1
        }
    );
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 2);
    assert_eq!(
        docs[1],
        expected_doc(&[(png.as_str(), 1)]),
        "only the fixed line"
    );
    // ...and from now on both are "sent": the next re-offer prints nothing.
    assert_eq!(
        source.poll_once().await.unwrap(),
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 2
        }
    );
    assert_eq!(rig.spooler.docs().len(), 2);
}

#[tokio::test]
async fn test_malformed_line_with_owed_ack_is_reacked_not_rejected_twice() {
    let rig = Rig::new("badowed").await;
    rig.set_lines(vec![json!({"line_id": 1701, "print_qty": "many"})]);
    rig.odoo.lock().unwrap().ack_rpc_error_for.insert(1701);
    let source = rig.source();
    let first = source.poll_once().await.unwrap();
    assert_eq!(
        first,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 0,
            rejected: 1,
            reacked: 0
        }
    );
    let second = source.poll_once().await.unwrap();
    assert_eq!(
        second,
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 1
        }
    );
    assert!(rig.spooler.docs().is_empty());
}

#[tokio::test]
async fn test_line_without_line_id_reports_a_blocked_batch() {
    let rig = Rig::new("noid").await;
    rig.set_lines(vec![json!({"sequence": 1, "print_qty": 1})]);
    let source = rig.source();
    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 0
        }
    );
    assert!(rig.acks().is_empty());
    let status = source.status().last_result;
    assert!(status.starts_with("1 lines without line_id"), "{status}");
}

#[tokio::test]
async fn test_line_with_undelivered_ack_is_never_reprinted_and_never_blocks_new_lines() {
    let rig = Rig::new("owedack").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(1301, 10, 1, &png, "product")]);
    rig.odoo.lock().unwrap().ack_rpc_error_for.insert(1301);
    let source = rig.source();

    // Printed, but Odoo answers its ack with a JSON-RPC error: owed, not fatal.
    let first = source.poll_once().await.unwrap();
    assert_eq!(
        first,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 1,
            rejected: 0,
            reacked: 0
        }
    );
    // Odoo still offers it (never got our result): re-acked, NOT reprinted.
    let second = source.poll_once().await.unwrap();
    assert_eq!(
        second,
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 1
        }
    );
    assert_eq!(rig.spooler.docs().len(), 1);
    // A new line in the same batch still prints while 1301's ack is stuck.
    rig.odoo
        .lock()
        .unwrap()
        .lines
        .push(line(1302, 20, 3, &png, "product"));
    let third = source.poll_once().await.unwrap();
    assert_eq!(
        third,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 1,
            rejected: 0,
            reacked: 1
        }
    );
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 2);
    assert_eq!(
        docs[1],
        expected_doc(&[(png.as_str(), 3)]),
        "only the new line"
    );
    // Odoo stops offering 1301 but still refuses its ack: owed → back off.
    rig.odoo.lock().unwrap().lines.clear();
    assert_eq!(
        source.poll_once().await.unwrap(),
        PollOutcome::AcksOwed { owed: 1 }
    );
    // Odoo recovers: the owed ack goes out, nothing else happens.
    rig.odoo.lock().unwrap().ack_rpc_error_for.clear();
    assert_eq!(source.poll_once().await.unwrap(), PollOutcome::Idle);
    let last = rig.acks().last().cloned().unwrap();
    assert_eq!(
        last,
        json!({"line_id": 1301, "printed_qty": 1, "error": null})
    );
    assert_eq!(rig.spooler.docs().len(), 2);
}

#[tokio::test]
async fn test_timed_out_print_is_acked_as_unknown_outcome_not_as_failed() {
    let mut rig = Rig::new("timeout").await;
    rig.print_timeout = Duration::from_secs(1);
    *rig.spooler.delay.lock().unwrap() = Some(Duration::from_secs(3));
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(1601, 10, 2, &png, "product")]);
    let source = rig.source();

    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 1,
            rejected: 0,
            reacked: 0
        }
    );
    let ack = rig.acks()[0].clone();
    assert_eq!(ack["printed_qty"], 0);
    assert_eq!(ack["error"], timed_out_ack_text(1));
    assert!(
        ack["error"]
            .as_str()
            .unwrap()
            .contains("check the printer before reprinting")
    );
    assert_eq!(rig.queue.get_all_jobs().unwrap()[0].state, JobState::Failed);
    // The abandoned print task removes its own spool file once it is done.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let left = std::fs::read_dir(rig.dir.join("spool")).unwrap().count();
    assert_eq!(left, 0, "abandoned task cleans up its spool file");
}

#[tokio::test]
async fn test_line_left_sending_by_another_writer_is_closed_not_printed() {
    let rig = Rig::new("straggler").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(1401, 10, 1, &png, "product")]);
    let source = rig.source();
    // Same ledger file, second connection: the line is mid-print.
    Ledger::open(&rig.dir.join("odoo-ledger.db"))
        .unwrap()
        .mark_sending(12, &[1401])
        .unwrap();

    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 1
        }
    );
    assert!(rig.spooler.docs().is_empty());
    let ack = rig.acks()[0].clone();
    assert_eq!(
        ack,
        json!({"line_id": 1401, "printed_qty": 0, "error": INTERRUPTED_REASON})
    );
}

#[tokio::test]
async fn test_malformed_and_repeated_lines_do_not_block_the_batch() {
    let rig = Rig::new("malformed").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![
        json!({"line_id": 1501, "sequence": 10, "print_qty": 1.5, "label_png_base64": png}),
        json!({"sequence": 15, "print_qty": 1, "label_png_base64": png}),
        line(1502, 20, 2, &png, "product"),
        line(1502, 30, 2, &png, "product"),
    ]);
    let source = rig.source();

    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 1,
            rejected: 1,
            reacked: 0
        }
    );
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0],
        expected_doc(&[(png.as_str(), 2)]),
        "the repeat is not printed"
    );
    let acks: HashMap<i64, Value> = rig
        .acks()
        .into_iter()
        .map(|a| (a["line_id"].as_i64().unwrap(), a))
        .collect();
    assert_eq!(acks.len(), 2);
    assert_eq!(acks[&1501]["printed_qty"], 0);
    assert!(
        acks[&1501]["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid line: "),
        "{}",
        acks[&1501]
    );
    assert_eq!(
        acks[&1502],
        json!({"line_id": 1502, "printed_qty": 2, "error": null})
    );
}

#[tokio::test]
async fn test_restart_with_owed_acks_never_reprints() {
    let rig = Rig::new("restart").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(601, 10, 1, &png, "product")]);
    rig.odoo.lock().unwrap().fail_acks = 1000;
    {
        let first = rig.source();
        assert!(first.poll_once().await.is_err());
        assert_eq!(rig.spooler.docs().len(), 1);
    } // client process "dies" with the ack still owed

    rig.odoo.lock().unwrap().fail_acks = 0;
    let second = rig.source(); // same ledger file
    assert_eq!(second.poll_once().await.unwrap(), PollOutcome::Idle);
    assert_eq!(rig.spooler.docs().len(), 1);
    let last = rig.acks().last().cloned().unwrap();
    assert_eq!(
        last,
        json!({"line_id": 601, "printed_qty": 1, "error": null})
    );
}

#[tokio::test]
async fn test_run_recovers_interrupted_line_and_sends_heartbeat() {
    let rig = Rig::new("run").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![
        line(701, 10, 1, &png, "product"),
        line(702, 20, 2, &png, "product"),
    ]);
    {
        // A previous process recorded 701 as `sending` and died mid-print.
        let ledger = Ledger::open(&rig.dir.join("odoo-ledger.db")).unwrap();
        ledger.mark_sending(12, &[701]).unwrap();
    }
    let source = rig.source();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(source.run(shutdown.clone()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let done = {
            let s = rig.odoo.lock().unwrap();
            s.lines.is_empty() && s.heartbeats.iter().any(|h| h["last_job_id"] == 12)
        };
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run loop made no progress"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("run stops on shutdown")
        .unwrap()
        .unwrap();

    let acks = rig.acks();
    let a701 = acks.iter().find(|a| a["line_id"] == 701).unwrap();
    assert_eq!(a701["printed_qty"], 0);
    assert_eq!(a701["error"], INTERRUPTED_REASON);
    let a702 = acks.iter().find(|a| a["line_id"] == 702).unwrap();
    assert_eq!(
        a702,
        &json!({"line_id": 702, "printed_qty": 2, "error": null})
    );
    // Only 702 was printed; the interrupted line was NOT reprinted.
    let docs = rig.spooler.docs();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0], expected_doc(&[(png.as_str(), 2)]));

    let hbs = rig.odoo.lock().unwrap().heartbeats.clone();
    let first = &hbs[0];
    assert_eq!(first["printer_name"], "TSC ML241P Spišská");
    assert_eq!(first["client_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        first["paper_status"], "unknown",
        "no Windows queue on the test runner"
    );
    assert_eq!(first["error_status"], "unknown");
    assert!(
        first.get("last_job_id").is_some() && first.get("last_result").is_some(),
        "{first}"
    );
    let later = hbs.last().unwrap();
    assert_eq!(later["last_job_id"], 12, "{later}");
}

#[tokio::test]
async fn test_business_refusal_of_an_ack_is_final() {
    let rig = Rig::new("refusal").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(801, 10, 1, &png, "product")]);
    rig.odoo.lock().unwrap().ack_refusal = Some("Dávka nie je v stave Tlač.".into());
    let source = rig.source();

    source.poll_once().await.unwrap();
    assert_eq!(rig.acks().len(), 1);
    // Odoo did not store the result and offers the line again: re-acked,
    // never reprinted.
    assert_eq!(
        source.poll_once().await.unwrap(),
        PollOutcome::OnlyKnownLines {
            batch_id: 12,
            reacked: 1
        }
    );
    assert_eq!(rig.acks().len(), 2);
    assert_eq!(rig.spooler.docs().len(), 1);
    // Once Odoo stops offering it, the refusal is final: not re-sent.
    rig.odoo.lock().unwrap().lines.clear();
    assert_eq!(source.poll_once().await.unwrap(), PollOutcome::Idle);
    assert_eq!(rig.acks().len(), 2);
}

#[tokio::test]
async fn test_duplicate_ack_answer_is_accepted() {
    let rig = Rig::new("dup").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![line(901, 10, 1, &png, "product")]);
    rig.odoo
        .lock()
        .unwrap()
        .stored
        .insert(901, json!({"printed_qty": 1, "error": null}));
    let source = rig.source();
    source.poll_once().await.unwrap();
    assert_eq!(rig.acks().len(), 1);
    // accepted as done → nothing owed on the next cycle
    assert_eq!(source.poll_once().await.unwrap(), PollOutcome::Idle);
    assert_eq!(rig.acks().len(), 1);
}

#[tokio::test]
async fn test_auth_error_prints_nothing_and_never_echoes_the_key() {
    let rig = Rig::new("auth").await;
    rig.set_lines(vec![line(1001, 10, 1, &png_b64(8, 8, 1), "product")]);
    let source = rig.source_with_key("wrong-key-value-123");
    let err = format!("{:#}", source.poll_once().await.unwrap_err());
    assert!(err.contains("Access Denied"), "{err}");
    assert!(
        !err.contains("wrong-key-value-123") && !err.contains(KEY),
        "{err}"
    );
    assert!(rig.spooler.docs().is_empty());
    assert!(rig.acks().is_empty());
}

#[tokio::test]
async fn test_spooler_failure_acks_every_line_as_error() {
    let rig = Rig::new("spoolfail").await;
    let png = png_b64(576, 880, 5);
    rig.set_lines(vec![
        line(1101, 10, 4, &png, "product"),
        line(1102, 20, 1, &png, "separator"),
    ]);
    *rig.spooler.fail.lock().unwrap() = Some(format!(
        "EventID 842: print processor refused RAW job {}",
        "x".repeat(300)
    ));
    let source = rig.source();
    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 2,
            rejected: 0,
            reacked: 0
        }
    );
    let acks = rig.acks();
    assert_eq!(acks.len(), 2);
    for a in &acks {
        assert_eq!(a["printed_qty"], 0, "{a}");
        let e = a["error"].as_str().unwrap();
        assert!(e.starts_with("EventID 842"), "{e}");
        assert_eq!(e.chars().count(), 200, "Odoo error_text limit");
    }
    let job = &rig.queue.get_all_jobs().unwrap()[0];
    assert_eq!(job.state, JobState::Failed);
    assert!(
        source
            .status()
            .last_result
            .starts_with("error: EventID 842")
    );
}

#[tokio::test]
async fn test_batch_with_only_unprintable_lines_prints_nothing() {
    let rig = Rig::new("unprintable").await;
    rig.set_lines(vec![
        line(1201, 10, 1, "", "separator"),
        line(1202, 20, 0, &png_b64(8, 8, 1), "product"),
        line(1203, 30, -1, &png_b64(8, 8, 1), "product"),
    ]);
    let source = rig.source();
    let outcome = source.poll_once().await.unwrap();
    assert_eq!(
        outcome,
        PollOutcome::Batch {
            batch_id: 12,
            printed: 0,
            rejected: 3,
            reacked: 0
        }
    );
    assert!(rig.spooler.docs().is_empty());
    let acks: HashMap<i64, Value> = rig
        .acks()
        .into_iter()
        .map(|a| (a["line_id"].as_i64().unwrap(), a))
        .collect();
    assert_eq!(acks[&1201]["error"], "empty png");
    assert_eq!(
        acks[&1202],
        json!({"line_id": 1202, "printed_qty": 0, "error": null})
    );
    assert_eq!(acks[&1203]["error"], "invalid print_qty -1");
    assert_eq!(
        source.status().last_result,
        "nothing printable (3 lines rejected)"
    );
}
