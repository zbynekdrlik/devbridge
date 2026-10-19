//! E2E coverage for the Odoo label source (issue #90).
//!
//! The RAW E2E client (`e2e-raw-client`, `deploy/e2e-setup-client-local.ps1`)
//! runs with a `[client.odoo]` block written by the REAL installer functions
//! (`Get-DevBridgeOdooToml`): url = this fake Odoo on the server runner
//! (`http://<server>:9230`, port opened by `deploy/e2e-setup-server.ps1`),
//! the E2E key, and a printer name with diacritics. The client polls it on
//! its own — pz-server's DevBridge instance is NOT in this path.
//!
//! The step hosts the fake Odoo (JSON-RPC 2.0 envelope, Bearer key, HTTP 200
//! errors, `{"lines": []}` when empty — the #90 contract), queues ONE batch
//! (2 products + a separator + a separator with an empty PNG), and asserts:
//! 1. every line is acked: printed lines with `printed_qty = print_qty` and
//!    `error: null`, the empty-PNG line with `error: "empty png"`;
//! 2. the RAW client printed the batch as ONE spooler document on its
//!    Generic / Text Only `NUL:` printer, confirmed by EventID 307 with a byte
//!    count equal to the TSPL size computed here independently;
//! 3. a second batch names its own roll, the 50 × 30 mm crate labels
//!    (issue #108: `label_width_mm` / `label_height_mm` at batch level, a
//!    399 × 240 PNG): it is printed as its own document whose EventID 307
//!    byte count equals the TSPL size computed here for `SIZE 50 mm, 30 mm`
//!    on 400 × 240 dots;
//! 4. a heartbeat arrived with the configured (non-ASCII) printer name, a
//!    READ queue state — `paper_status` / `error_status` are not `unknown`
//!    (the client's native winspool read of its Generic / Text Only queue
//!    answers in milliseconds, #97) — and `paper_size` = the crate roll of
//!    the last document (#108);
//! 5. no request came with a wrong / missing Bearer key.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Map, Value, json};

/// Must match `deploy/e2e-setup-client-local.ps1` + `deploy/e2e-setup-server.ps1`.
pub const FAKE_ODOO_PORT: u16 = 9230;
pub const E2E_ODOO_KEY: &str = "e2e-fake-odoo-key-devbridge-90";
/// Printer name; the setup writes it with TOML \uXXXX escapes (ASCII config).
pub const E2E_ODOO_PRINTER_NAME: &str = "TSC E2E Odoo Spišská";
/// Dashboard of the RAW E2E client (issue #88 setup).
pub const RAW_DASHBOARD_PORT: u16 = 9222;

/// Client poll backoff is capped at 60 s and it has been failing to reach
/// this (not yet running) fake Odoo since its start — wait well past that.
const ACK_WAIT: Duration = Duration::from_secs(240);
/// Heartbeat interval is 30 s.
const HEARTBEAT_WAIT: Duration = Duration::from_secs(90);

/// The document header the client must send (mirrors the BarTender driver).
pub const TSPL_HEADER: &str =
    "SIZE 72.7 mm, 110.1 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\n";

/// The `SIZE 72.7 mm, 110.1 mm` canvas in dots at 203 dpi (8 dots/mm).
pub const CANVAS_DOTS: (u32, u32) = (581, 880);

/// Header of a batch that names the 50 × 30 mm crate roll (issue #108).
pub const CRATE_TSPL_HEADER: &str =
    "SIZE 50 mm, 30 mm\r\nDIRECTION 0,0\r\nREFERENCE 0,0\r\nOFFSET 0 mm\r\nSET TEAR ON\r\n";

/// The `SIZE 50 mm, 30 mm` canvas in dots at 203 dpi.
pub const CRATE_CANVAS_DOTS: (u32, u32) = (400, 240);

/// The crate roll as the batch names it, and as the heartbeat reports it.
pub const CRATE_BATCH_SIZE: (f64, f64) = (50.0, 30.0);
pub const CRATE_PAPER_SIZE: &str = "50x30 mm";

/// Size of the TSPL document for labels `(width_px, height_px, copies)` on
/// the 72.7 × 110.1 mm roll (see [`tspl_len`]).
pub fn expected_tspl_len(labels: &[(u32, u32, u32)]) -> u64 {
    tspl_len(TSPL_HEADER, CANVAS_DOTS, labels)
}

/// Size of a TSPL document, computed from the TSPL format itself (not from
/// devbridge code): header + per label `CLS\r\n`,
/// `BITMAP <x>,<y>,<wb>,<h>,0,` + wb·h data bytes + `\r\n`,
/// `PRINT 1,<copies>\r\n` — the label centred on the canvas,
/// `x = (canvas_w − w) / 2`, `y = (canvas_h − h) / 2` (issue #95; the 180°
/// turn does not change the size).
pub fn tspl_len(header: &str, canvas: (u32, u32), labels: &[(u32, u32, u32)]) -> u64 {
    let mut len = header.len() as u64;
    for &(w, h, copies) in labels {
        let wb = w.div_ceil(8);
        let (x, y) = (
            canvas.0.saturating_sub(w) / 2,
            canvas.1.saturating_sub(h) / 2,
        );
        len += "CLS\r\n".len() as u64;
        len += format!("BITMAP {x},{y},{wb},{h},0,").len() as u64;
        len += u64::from(wb) * u64::from(h);
        len += 2;
        len += format!("PRINT 1,{copies}\r\n").len() as u64;
    }
    len
}

/// A 1-bit grayscale PNG (base64) with `black_rows` black rows on top.
pub fn label_png_base64(width: u32, height: u32, black_rows: u32) -> String {
    use base64::Engine as _;
    let wb = width.div_ceil(8) as usize;
    let mut data = vec![0xFFu8; wb * height as usize];
    for b in data.iter_mut().take(wb * black_rows as usize) {
        *b = 0;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&data).expect("png data");
    }
    base64::engine::general_purpose::STANDARD.encode(out)
}

#[derive(Default)]
struct Fake {
    batch_id: i64,
    /// Batch-level keys next to `batch_id` / `lines` (#108: the roll).
    batch_fields: Map<String, Value>,
    lines: Vec<Value>,
    acks: Vec<Value>,
    heartbeats: Vec<Value>,
    unauthorized: usize,
}

type Shared = Arc<Mutex<Fake>>;

fn authorized(st: &Shared, headers: &HeaderMap) -> bool {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {E2E_ODOO_KEY}"));
    if !ok {
        st.lock().unwrap().unauthorized += 1;
    }
    ok
}

fn access_denied() -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 100,
        "message": "Odoo Server Error", "data": {"message": "Access Denied"}}}))
}

fn result(v: Value) -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": v}))
}

async fn next(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    let s = st.lock().unwrap();
    if s.lines.is_empty() {
        return result(json!({"lines": []}));
    }
    let mut batch = s.batch_fields.clone();
    batch.insert("batch_id".into(), json!(s.batch_id));
    batch.insert("production_date".into(), json!("2026-09-29"));
    batch.insert("lines".into(), json!(s.lines));
    result(Value::Object(batch))
}

async fn ack(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    let p = body["params"].clone();
    let mut s = st.lock().unwrap();
    let line_id = p["line_id"].as_i64();
    s.lines.retain(|l| l["line_id"].as_i64() != line_id);
    s.acks.push(p);
    result(json!({"ok": true}))
}

async fn heartbeat(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    st.lock().unwrap().heartbeats.push(body["params"].clone());
    result(json!({"ok": true, "printer_id": 1}))
}

/// Line ids unique per run (the client ledger never reprints a known id).
fn run_base_id() -> i64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // keep it comfortably inside i64 while unique per run
    (ms % 1_000_000_000) * 10
}

/// Step body — see the module docs.
pub async fn test_odoo_source(client: &reqwest::Client, client_host: &str) -> Result<()> {
    let base = run_base_id();
    let batch_id = base / 10;
    let product_a = label_png_base64(576, 880, 40);
    let product_b = label_png_base64(576, 880, 80);
    let separator = label_png_base64(576, 880, 440);
    let lines = vec![
        json!({"line_id": base + 1, "sequence": 10, "product_name": "E2E chlieb", "print_qty": 2,
               "best_before": "03.10.2026", "label_png_base64": product_a, "line_type": "product"}),
        json!({"line_id": base + 2, "sequence": 20, "product_name": "E2E rožok", "print_qty": 1,
               "best_before": "01.10.2026", "label_png_base64": product_b, "line_type": "product"}),
        json!({"line_id": base + 3, "sequence": 30, "product_name": false, "print_qty": 1,
               "best_before": false, "label_png_base64": separator, "line_type": "separator"}),
        json!({"line_id": base + 4, "sequence": 40, "product_name": false, "print_qty": 1,
               "best_before": false, "label_png_base64": "", "line_type": "separator"}),
    ];
    let want_len = expected_tspl_len(&[(576, 880, 2), (576, 880, 1), (576, 880, 1)]);

    let state: Shared = Arc::new(Mutex::new(Fake {
        batch_id,
        lines,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/food/print/next", post(next))
        .route("/food/print/ack", post(ack))
        .route("/food/print/heartbeat", post(heartbeat))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", FAKE_ODOO_PORT))
        .await
        .with_context(|| format!("bind fake Odoo on port {FAKE_ODOO_PORT}"))?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let outcome = verify(client, client_host, &state, batch_id, base, want_len).await;
    server.abort();
    outcome?;
    println!("PASS");
    Ok(())
}

/// The batch-level keys of the crate batch (#108).
pub fn crate_batch_fields() -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("label_width_mm".into(), json!(CRATE_BATCH_SIZE.0));
    fields.insert("label_height_mm".into(), json!(CRATE_BATCH_SIZE.1));
    fields
}

async fn verify(
    client: &reqwest::Client,
    client_host: &str,
    state: &Shared,
    batch_id: i64,
    base: i64,
    want_len: u64,
) -> Result<()> {
    // 1. All four lines acked (the client finds us within its backoff).
    let acks = wait_for_acks(state, 4).await?;
    let ack_of = |id: i64| {
        acks.iter()
            .find(|a| a["line_id"].as_i64() == Some(id))
            .cloned()
            .with_context(|| format!("no ack for line {id}: {acks:?}"))
    };
    ensure!(
        ack_of(base + 1)? == json!({"line_id": base + 1, "printed_qty": 2, "error": null}),
        "product A ack wrong: {:?}",
        ack_of(base + 1)
    );
    ensure!(
        ack_of(base + 2)? == json!({"line_id": base + 2, "printed_qty": 1, "error": null}),
        "product B ack wrong: {:?}",
        ack_of(base + 2)
    );
    ensure!(
        ack_of(base + 3)? == json!({"line_id": base + 3, "printed_qty": 1, "error": null}),
        "separator ack wrong: {:?}",
        ack_of(base + 3)
    );
    ensure!(
        ack_of(base + 4)? == json!({"line_id": base + 4, "printed_qty": 0, "error": "empty png"}),
        "empty-PNG line ack wrong: {:?}",
        ack_of(base + 4)
    );
    ensure!(
        acks.len() == 4,
        "each line must be acked exactly once: {acks:?}"
    );
    println!("  4 Odoo lines acked (3 printed, 1 'empty png')");

    // 2. ONE spooler document, EventID 307 byte count == the TSPL size.
    let raw_base = format!("http://{client_host}:{RAW_DASHBOARD_PORT}");
    let evidence = check_batch_document(client, &raw_base, batch_id, want_len).await?;
    println!("  client: {evidence}");

    // 3. A crate batch that names its own 50 x 30 mm roll (#108).
    let crate_batch = batch_id + 1;
    let crate_line = base + 5;
    {
        let mut s = state.lock().unwrap();
        s.batch_id = crate_batch;
        s.batch_fields = crate_batch_fields();
        s.lines = vec![
            json!({"line_id": crate_line, "sequence": 10, "product_name": "E2E prepravka",
                   "print_qty": 1, "best_before": false,
                   "label_png_base64": label_png_base64(399, 240, 24), "line_type": "code_label"}),
        ];
    }
    let acks = wait_for_acks(state, 5).await?;
    let crate_ack = acks
        .iter()
        .find(|a| a["line_id"].as_i64() == Some(crate_line))
        .with_context(|| format!("no ack for the crate line {crate_line}: {acks:?}"))?;
    ensure!(
        *crate_ack == json!({"line_id": crate_line, "printed_qty": 1, "error": null}),
        "crate label ack wrong: {crate_ack}"
    );
    ensure!(
        acks.len() == 5,
        "each line must be acked exactly once: {acks:?}"
    );
    let crate_len = tspl_len(CRATE_TSPL_HEADER, CRATE_CANVAS_DOTS, &[(399, 240, 1)]);
    let evidence = check_batch_document(client, &raw_base, crate_batch, crate_len).await?;
    println!("  crate batch 50x30 mm (SIZE 50 mm, 30 mm): {evidence}");

    // 4. Heartbeat with the configured printer name and the crate roll (sent
    //    every 30 s).
    let start = Instant::now();
    let hb = loop {
        let found = state
            .lock()
            .unwrap()
            .heartbeats
            .iter()
            .rev()
            .find(|h| h["last_job_id"].as_i64() == Some(crate_batch))
            .cloned();
        if let Some(h) = found {
            break h;
        }
        if start.elapsed() > HEARTBEAT_WAIT {
            let seen = state.lock().unwrap().heartbeats.clone();
            bail!(
                "no heartbeat reporting batch {crate_batch} within {}s (seen: {seen:?})",
                HEARTBEAT_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    ensure!(
        hb["printer_name"] == E2E_ODOO_PRINTER_NAME,
        "heartbeat printer_name: {hb}"
    );
    ensure!(
        hb["client_version"].as_str().is_some_and(|v| !v.is_empty()),
        "heartbeat without client_version: {hb}"
    );
    ensure!(
        hb["paper_size"] == CRATE_PAPER_SIZE,
        "heartbeat paper_size must be the crate roll {CRATE_PAPER_SIZE}: {hb}"
    );
    check_heartbeat_status(&hb)?;
    println!(
        "  heartbeat: printer_name={} paper_status={} error_status={} paper_size={} last_result={}",
        hb["printer_name"],
        hb["paper_status"],
        hb["error_status"],
        hb["paper_size"],
        hb["last_result"]
    );

    // 5. Every request carried the right Bearer key.
    let unauthorized = state.lock().unwrap().unauthorized;
    ensure!(
        unauthorized == 0,
        "{unauthorized} request(s) without the right Bearer key"
    );
    Ok(())
}

/// Wait until the fake holds `count` acks (the client finds it within its
/// poll backoff).
async fn wait_for_acks(state: &Shared, count: usize) -> Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let acks = state.lock().unwrap().acks.clone();
        if acks.len() >= count {
            return Ok(acks);
        }
        if start.elapsed() > ACK_WAIT {
            bail!(
                "RAW client acked {} of {count} Odoo lines within {}s (acks: {acks:?})",
                acks.len(),
                ACK_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The batch went out as ONE completed spooler document of `want_len`
/// bytes, confirmed by EventID 307 with that byte count. Returns the
/// evidence.
async fn check_batch_document(
    client: &reqwest::Client,
    raw_base: &str,
    batch_id: i64,
    want_len: u64,
) -> Result<String> {
    let jobs: Value = client
        .get(format!("{raw_base}/api/jobs"))
        .send()
        .await?
        .json()
        .await?;
    let prefix = format!("Odoo batch {batch_id} ");
    let matching: Vec<&Value> = jobs
        .as_array()
        .context("RAW client /api/jobs is not an array")?
        .iter()
        .filter(|j| j["name"].as_str().is_some_and(|n| n.starts_with(&prefix)))
        .collect();
    ensure!(
        matching.len() == 1,
        "expected ONE spooler document for batch {batch_id}, got {}: {jobs}",
        matching.len()
    );
    let job = matching[0];
    ensure!(
        job["status"] == "completed",
        "Odoo batch job not completed: {job}"
    );
    ensure!(
        job["payload_size"].as_u64() == Some(want_len),
        "TSPL size {} != expected {want_len}: {job}",
        job["payload_size"]
    );
    let job_id = job["id"].as_str().context("job has no id")?;
    let events: Value = client
        .get(format!("{raw_base}/api/jobs/{job_id}/events"))
        .send()
        .await?
        .json()
        .await?;
    let evidence = events
        .as_array()
        .and_then(|a| a.iter().find(|e| e["verification_method"] == "eventid_307"))
        .and_then(|e| e["verification_evidence"].as_str())
        .with_context(|| format!("no EventID 307 verification on {job_id}: {events}"))?;
    ensure!(
        crate::raw_passthrough::evidence_has_byte_count(evidence, want_len),
        "EventID 307 byte count does not match the {want_len}-byte TSPL: {evidence}"
    );
    Ok(evidence.to_string())
}

/// The heartbeat must carry a READ queue state (#97): `paper_status` and
/// `error_status` are non-empty strings and neither is `unknown` (= the
/// client could not read its printer's queue).
pub fn check_heartbeat_status(hb: &Value) -> Result<()> {
    for field in ["paper_status", "error_status"] {
        let value = hb[field]
            .as_str()
            .with_context(|| format!("heartbeat without {field}: {hb}"))?;
        ensure!(
            !value.is_empty() && value != "unknown",
            "heartbeat {field} is {value:?} - the client could not read its printer queue: {hb}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_heartbeat_status_needs_a_read_queue_state() {
        let hb = |paper: Value, error: Value| json!({"paper_status": paper, "error_status": error});
        assert!(check_heartbeat_status(&hb(json!("ok"), json!("ok"))).is_ok());
        assert!(
            check_heartbeat_status(&hb(json!("paper_out"), json!("paper_out,offline"))).is_ok()
        );
        assert!(check_heartbeat_status(&hb(json!("ok"), json!("printing"))).is_ok());
        for (paper, error) in [
            (json!("unknown"), json!("unknown")),
            (json!("ok"), json!("unknown")),
            (json!("unknown"), json!("ok")),
            (json!(""), json!("ok")),
            (json!("ok"), json!("")),
            (json!(null), json!("ok")),
            (json!("ok"), json!(0)),
        ] {
            let body = hb(paper, error);
            assert!(check_heartbeat_status(&body).is_err(), "{body}");
        }
        let missing = json!({"printer_name": "x"});
        let err = check_heartbeat_status(&missing).unwrap_err().to_string();
        assert!(err.contains("without paper_status"), "{err}");
        let unknown = check_heartbeat_status(&hb(json!("ok"), json!("unknown")))
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("error_status is \"unknown\""), "{unknown}");
    }

    #[test]
    fn test_expected_tspl_len_matches_the_format() {
        // header only
        assert_eq!(expected_tspl_len(&[]), TSPL_HEADER.len() as u64);
        // one 16x2 label, 3 copies, centred at (282, 439): CLS\r\n (5) +
        // "BITMAP 282,439,2,2,0," (21) + 4 data + \r\n (2) + "PRINT 1,3\r\n" (11)
        assert_eq!(
            expected_tspl_len(&[(16, 2, 3)]),
            TSPL_HEADER.len() as u64 + 5 + 21 + 4 + 2 + 11
        );
        // width padded to whole bytes: 9 px -> 2 bytes per row
        assert_eq!(
            expected_tspl_len(&[(9, 1, 1)]) - TSPL_HEADER.len() as u64,
            5 + "BITMAP 286,439,2,1,0,".len() as u64 + 2 + 2 + "PRINT 1,1\r\n".len() as u64
        );
        // Odoo's 576 x 880 label: BITMAP 2,0 -- the same size as 0.8.42's 0,0
        assert_eq!(
            expected_tspl_len(&[(576, 880, 1)]) - TSPL_HEADER.len() as u64,
            5 + "BITMAP 2,0,72,880,0,".len() as u64 + 63_360 + 2 + 11
        );
        // the E2E batch: 3 full labels of 576x880 = 63 360 data bytes each
        let e2e = expected_tspl_len(&[(576, 880, 2), (576, 880, 1), (576, 880, 1)]);
        assert!(e2e > 3 * 63_360 && e2e < 3 * 63_360 + 200, "{e2e}");
    }

    #[test]
    fn test_crate_roll_document_size_and_header() {
        // #108: only SIZE differs from the bakery header.
        assert!(CRATE_TSPL_HEADER.starts_with("SIZE 50 mm, 30 mm\r\n"));
        assert_eq!(
            CRATE_TSPL_HEADER.split_once("\r\n").unwrap().1,
            TSPL_HEADER.split_once("\r\n").unwrap().1
        );
        // 50 x 30 mm at 8 dots/mm.
        assert_eq!(CRATE_CANVAS_DOTS, (50 * 8, 30 * 8));
        // Odoo's 399 x 240 crate PNG: 50 bytes per row, centred at 0,0
        // (one dot of slack across 400 dots rounds down to 0).
        assert_eq!(
            tspl_len(CRATE_TSPL_HEADER, CRATE_CANVAS_DOTS, &[(399, 240, 1)])
                - CRATE_TSPL_HEADER.len() as u64,
            5 + "BITMAP 0,0,50,240,0,".len() as u64 + 50 * 240 + 2 + 11
        );
        // The bakery wrapper is the same formula on the bakery roll.
        let labels = [(576, 880, 2), (16, 2, 3)];
        assert_eq!(
            expected_tspl_len(&labels),
            tspl_len(TSPL_HEADER, CANVAS_DOTS, &labels)
        );
        assert_eq!(
            tspl_len(TSPL_HEADER, CANVAS_DOTS, &[(399, 240, 1)]) - TSPL_HEADER.len() as u64,
            5 + "BITMAP 91,320,50,240,0,".len() as u64 + 50 * 240 + 2 + 11
        );
    }

    #[test]
    fn test_crate_batch_fields_name_the_50x30_roll() {
        assert_eq!(
            Value::Object(crate_batch_fields()),
            json!({"label_width_mm": 50.0, "label_height_mm": 30.0})
        );
        assert_eq!(CRATE_PAPER_SIZE, "50x30 mm");
    }

    #[test]
    fn test_label_png_is_a_decodable_1_bit_png_of_the_right_size() {
        use base64::Engine as _;
        let b64 = label_png_base64(576, 880, 3);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (576, 880));
        assert_eq!(info.bit_depth, png::BitDepth::One);
        assert_eq!(info.color_type, png::ColorType::Grayscale);
    }

    #[test]
    fn test_run_base_id_leaves_room_for_line_offsets() {
        let a = run_base_id();
        assert!(a >= 0 && a % 10 == 0);
        assert!(a + 9 < i64::MAX);
    }

    #[test]
    fn test_printer_name_matches_the_setup_escape() {
        assert_eq!(E2E_ODOO_PRINTER_NAME, "TSC E2E Odoo Spi\u{161}sk\u{e1}");
    }
}
