//! The fake Odoo, the fake spooler and the test rig shared by every Odoo
//! label source scenario.

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
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use devbridge_client::odoo_source::ledger::Ledger;
use devbridge_client::odoo_source::tspl::{self, LabelGeometry};
use devbridge_client::odoo_source::{OdooSource, OdooSourceDeps};
use devbridge_client::print_backend::{PrintBackend, PrintJobInfo};
use devbridge_client::print_lock::PrintLock;
use devbridge_core::config::OdooClientConfig;
use devbridge_core::job_event::EventEmitter;
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

pub const KEY: &str = "fake-odoo-test-key-7f3a";
pub const SPISSKA: LabelGeometry = LabelGeometry {
    width_mm: 72.7,
    height_mm: 110.1,
    dpi: 203,
    rotate_180: true,
    x_offset_dots: 0,
    y_offset_dots: 0,
};

// ── fake Odoo ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeOdoo {
    pub batch_id: i64,
    /// Batch-level keys `/next` sends next to `batch_id` / `lines` (#108:
    /// `label_width_mm`, `label_height_mm`, `dpi`).
    pub batch_fields: Map<String, Value>,
    /// Lines `/next` still returns (removed once acked, unless `keep_after_ack`).
    pub lines: Vec<Value>,
    pub acks: Vec<Value>,
    pub heartbeats: Vec<Value>,
    /// Final state already stored per line (drives `duplicate: true`).
    pub stored: HashMap<i64, Value>,
    /// Answer the next N acks with HTTP 502.
    pub fail_acks: usize,
    /// Answer every ack with `result.error` (business refusal).
    pub ack_refusal: Option<String>,
    /// Accept acks but keep returning the lines (a person re-queued them).
    pub keep_after_ack: bool,
    /// Answer acks of these lines with a top-level JSON-RPC error (the line
    /// stays waiting in Odoo).
    pub ack_rpc_error_for: HashSet<i64>,
}

pub type Shared = Arc<Mutex<FakeOdoo>>;

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
    let mut result = s.batch_fields.clone();
    result.insert("batch_id".into(), json!(s.batch_id));
    result.insert("production_date".into(), json!("2026-09-29"));
    result.insert("lines".into(), json!(s.lines));
    rpc_ok(Value::Object(result))
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

pub async fn start_fake(state: Shared) -> String {
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

pub struct FakeSpooler {
    pub docs: Mutex<Vec<Vec<u8>>>,
    pub fail: Mutex<Option<String>>,
    /// Simulated spooler hang (sleep after taking the bytes).
    pub delay: Mutex<Option<Duration>>,
    pub lock: PrintLock,
    pub lock_held_during_print: Mutex<Vec<bool>>,
}

impl FakeSpooler {
    pub fn new(lock: PrintLock) -> Arc<Self> {
        Arc::new(Self {
            docs: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            delay: Mutex::new(None),
            lock,
            lock_held_during_print: Mutex::new(Vec::new()),
        })
    }
    pub fn docs(&self) -> Vec<Vec<u8>> {
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

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("devbridge-odoo-it-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 1-bit grayscale PNG, `black_rows` of black at the top, rest white.
pub fn png_b64(width: u32, height: u32, black_rows: u32) -> String {
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

pub fn line(line_id: i64, sequence: i64, qty: i64, png: &str, line_type: &str) -> Value {
    json!({"line_id": line_id, "sequence": sequence, "product_name": format!("Produkt {line_id}"),
           "print_qty": qty, "best_before": "03.10.2026", "label_png_base64": png,
           "line_type": line_type})
}

pub fn config(url: &str) -> OdooClientConfig {
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

pub struct Rig {
    pub odoo: Shared,
    pub url: String,
    pub spooler: Arc<FakeSpooler>,
    pub lock: PrintLock,
    pub dir: PathBuf,
    pub queue: Arc<JobQueue>,
    pub print_timeout: Duration,
}

impl Rig {
    pub async fn new(tag: &str) -> Self {
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

    pub fn source_with_key(&self, key: &str) -> OdooSource {
        let mut cfg = config(&self.url);
        cfg.api_key = key.to_string();
        self.source_with(cfg)
    }

    pub fn source_with(&self, cfg: OdooClientConfig) -> OdooSource {
        let ledger = Ledger::open(&self.dir.join("odoo-ledger.db")).unwrap();
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

    pub fn source(&self) -> OdooSource {
        self.source_with_key(KEY)
    }

    pub fn set_lines(&self, lines: Vec<Value>) {
        self.odoo.lock().unwrap().lines = lines;
    }

    /// The next batch Odoo offers: its id, its batch-level size keys (#108)
    /// and its lines.
    pub fn set_batch(&self, batch_id: i64, fields: Value, lines: Vec<Value>) {
        let mut odoo = self.odoo.lock().unwrap();
        odoo.batch_id = batch_id;
        odoo.batch_fields = fields.as_object().cloned().expect("batch fields object");
        odoo.lines = lines;
    }

    pub fn acks(&self) -> Vec<Value> {
        self.odoo.lock().unwrap().acks.clone()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The TSPL document the source must send for `labels` on `geometry`.
pub fn expected_doc_on(geometry: &LabelGeometry, labels: &[(&str, u32)]) -> Vec<u8> {
    let bitmaps: Vec<_> = labels
        .iter()
        .map(|(b64, _)| tspl::decode_label(b64, geometry).unwrap())
        .collect();
    let pairs: Vec<_> = bitmaps
        .iter()
        .zip(labels)
        .map(|(b, (_, q))| (b, *q))
        .collect();
    tspl::build_document(geometry, &pairs)
}

/// The same on the `[client.odoo]` (Spišská) roll.
pub fn expected_doc(labels: &[(&str, u32)]) -> Vec<u8> {
    expected_doc_on(&SPISSKA, labels)
}
