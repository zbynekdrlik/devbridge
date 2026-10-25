//! E2E coverage for the direct_ipp path (issues #111 and #112).
//!
//! `deploy/e2e-setup-client-local.ps1` starts a THIRD isolated client on the
//! client runner (`e2e-ipp-client`, data dir `C:\ProgramData\DevBridge-E2E-Ipp`,
//! dashboard 9224) whose `[client]` keys come from the REAL installer function
//! `Get-DevBridgeClientConfigExtras`: `print_backend = "direct_ipp"`,
//! `ghostscript_device = "urfgray"`, 600 dpi — the HP LaserJet M110w stores'
//! settings — and `printer_address` = THIS binary's fake IPP printer on the
//! server runner (TCP 9232, opened by `deploy/e2e-setup-server.ps1`). No paper:
//! the fake only parses what it receives. `deploy/e2e-wait-ready.ps1` leaves
//! the client PENDING; this step approves it and rejects it on every path.
//!
//! 1. #111: a P / L / A5 PDF goes through pz-server to the client, which
//!    renders it with the INSTALLED Windows Ghostscript (10.04.0) and sends it:
//!    the fake must get ONE Print-Job (`image/urf`, copies 3) whose URF has 3
//!    pages, all PORTRAIT, the landscape page at the A4 size of page 1, the A5
//!    page smaller (never enlarged); the `rendered` event names the
//!    orientation.
//! 2. #114: the fake goes SILENT — it accepts the next Print-Job and answers
//!    every poll "processing": after the client's 60 s poll deadline the
//!    server must show the job `unconfirmed` („možno vytlačené") with
//!    retry_count 0, its event must say why, and the fake must have got
//!    exactly ONE Print-Job for it — also after the server's retry delay.
//! 3. #112: the fake is stopped and a third job is sent: the job's events
//!    must carry `IPP Print-Job send failed (<printer>): …` with the cause
//!    (the OS error), not stop at `sending`.

use anyhow::{Context, Result, bail, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::post;

use crate::raw_passthrough::{get_json, post_json, slugify};
use crate::urf::{bar_area_pt2, check_portrait_pages, expected_dark, urf_pages};

/// machine_id (`client_id`) of the direct_ipp E2E client. Must match
/// `deploy/e2e-setup-client-local.ps1` and `deploy/e2e-wait-ready.ps1`.
pub const E2E_IPP_CLIENT_ID: &str = "e2e-ipp-client";

/// Virtual printer name the direct_ipp client asks for.
pub const E2E_IPP_VP_NAME: &str = "E2E Ipp";

/// The fake IPP printer's port on the server runner. Must match
/// `deploy/e2e-setup-client-local.ps1` (`printer_address`) and the firewall
/// rule in `deploy/e2e-setup-server.ps1`.
pub const FAKE_IPP_PRINTER_PORT: u16 = 9232;

/// Copies `crate::build_ipp_print_job` asks for (forwarded to the printer).
const EXPECTED_COPIES: i32 = crate::E2E_COPIES as i32;

const CLIENT_WAIT: Duration = Duration::from_secs(90);
const JOB_ARRIVE_WAIT: Duration = Duration::from_secs(60);
/// Download + Windows Ghostscript (PDFINFO + 3 runs at 600 dpi) + send + poll.
const JOB_COMPLETE_WAIT: Duration = Duration::from_secs(180);
/// A dropped SYN times out after ~21 s on Windows; the send itself is bounded
/// by direct_ipp's 120 s HTTP timeout.
const SEND_FAILURE_WAIT: Duration = Duration::from_secs(150);
/// #114: download + render + send + the client's 60 s poll deadline.
const UNCONFIRMED_WAIT: Duration = Duration::from_secs(240);
/// #114: longer than the E2E server's `retry_delay_secs` (30, the first
/// retry's backoff) — a resend would have reached the fake by then.
const RESEND_WATCH: Duration = Duration::from_secs(45);
/// The job-state the silent fake answers every poll with (5 = processing).
const STATE_PROCESSING: i32 = 5;

// ── IPP wire format (what the fake printer reads and answers) ─────────────

const OP_PRINT_JOB: u16 = 0x0002;
const OP_GET_JOB_ATTRIBUTES: u16 = 0x0009;

/// One parsed IPP request.
#[derive(Debug, Clone, PartialEq)]
pub struct IppRequest {
    pub operation: u16,
    pub request_id: u32,
    /// (name, value bytes) in wire order; an additional value of a 1setOf
    /// attribute (empty name on the wire) repeats the previous name.
    pub attributes: Vec<(String, Vec<u8>)>,
    /// Everything after the end-of-attributes tag.
    pub document: Vec<u8>,
}

impl IppRequest {
    pub fn text(&self, name: &str) -> Option<String> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    }

    pub fn integer(&self, name: &str) -> Option<i32> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| <[u8; 4]>::try_from(v.as_slice()).ok())
            .map(i32::from_be_bytes)
    }
}

/// Parse an IPP/1.x request (RFC 8010 §3.1).
pub fn parse_ipp_request(body: &[u8]) -> Result<IppRequest> {
    ensure!(body.len() >= 9, "IPP request of {} bytes", body.len());
    let operation = u16::from_be_bytes([body[2], body[3]]);
    let request_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let mut attributes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pos = 8;
    loop {
        let tag = *body
            .get(pos)
            .context("IPP request ends before end-of-attributes")?;
        pos += 1;
        if tag == 0x03 {
            break;
        }
        if tag < 0x10 {
            continue; // begin-attribute-group delimiter
        }
        let read_u16 = |at: usize| -> Result<usize> {
            let b = body.get(at..at + 2).context("truncated IPP length")?;
            Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
        };
        let name_len = read_u16(pos)?;
        pos += 2;
        let name = body
            .get(pos..pos + name_len)
            .context("truncated IPP attribute name")?;
        pos += name_len;
        let value_len = read_u16(pos)?;
        pos += 2;
        let value = body
            .get(pos..pos + value_len)
            .context("truncated IPP attribute value")?
            .to_vec();
        pos += value_len;
        let name = if name.is_empty() {
            attributes
                .last()
                .map(|(n, _)| n.clone())
                .context("additional value without an attribute")?
        } else {
            String::from_utf8_lossy(name).into_owned()
        };
        attributes.push((name, value));
    }
    Ok(IppRequest {
        operation,
        request_id,
        attributes,
        document: body[pos..].to_vec(),
    })
}

fn write_attribute(buf: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
}

/// A successful-ok IPP response carrying `job-id` / `job-state`.
pub fn ipp_response(request_id: u32, job_id: i32, job_state: i32) -> Vec<u8> {
    let mut buf = vec![1, 1, 0, 0];
    buf.extend_from_slice(&request_id.to_be_bytes());
    buf.push(0x01);
    write_attribute(&mut buf, 0x47, "attributes-charset", b"utf-8");
    write_attribute(&mut buf, 0x48, "attributes-natural-language", b"en-us");
    buf.push(0x02);
    write_attribute(&mut buf, 0x21, "job-id", &job_id.to_be_bytes());
    write_attribute(&mut buf, 0x23, "job-state", &job_state.to_be_bytes());
    write_attribute(&mut buf, 0x44, "job-state-reasons", b"none");
    buf.push(0x03);
    buf
}

// ── Test PDF ───────────────────────────────────────────────────────────────

/// A minimal valid PDF: one page per (width, height) in points, each with a
/// solid bar ([`crate::urf::bar_area_pt2`]); page 1 also carries `marker` as text, which
/// keeps every run's document unique. The other pages hold ONLY the bar, so
/// their dark pixels are exactly known.
pub fn test_pdf(pages: &[(u32, u32)], marker: &str) -> Vec<u8> {
    let n = pages.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, (width, height)) in pages.iter().enumerate() {
        let text = if i == 0 {
            format!("BT /F1 24 Tf 40 {} Td ({marker}) Tj ET ", height - 80)
        } else {
            String::new()
        };
        let content = format!("{text}20 20 {} 30 re f", width - 40);
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            )
            .into_bytes(),
        );
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
            .into_bytes(),
        );
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(obj);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

// ── The fake IPP printer ──────────────────────────────────────────────────

/// A Print-Job the fake received.
#[derive(Debug, Clone)]
struct ReceivedJob {
    format: String,
    copies: Option<i32>,
    document: Vec<u8>,
}

#[derive(Clone, Default)]
struct FakeState {
    jobs: Arc<Mutex<Vec<ReceivedJob>>>,
    /// #114: accept Print-Jobs but never confirm their end.
    silent: Arc<AtomicBool>,
}

/// The job-state the fake answers a Get-Job-Attributes with: 9 (completed),
/// or 5 (processing) for ever once it is silent (#114).
pub fn poll_answer_state(silent: bool) -> i32 {
    if silent { STATE_PROCESSING } else { 9 }
}

async fn fake_ipp(State(state): State<FakeState>, body: Bytes) -> impl IntoResponse {
    let reply = match parse_ipp_request(&body) {
        Ok(req) if req.operation == OP_PRINT_JOB => {
            let job = ReceivedJob {
                format: req.text("document-format").unwrap_or_default(),
                copies: req.integer("copies"),
                document: req.document.clone(),
            };
            let job_id = {
                let mut jobs = state.jobs.lock().expect("fake IPP state");
                jobs.push(job);
                jobs.len() as i32
            };
            ipp_response(req.request_id, job_id, 3)
        }
        Ok(req) if req.operation == OP_GET_JOB_ATTRIBUTES => {
            let job_id = req.integer("job-id").unwrap_or(0);
            let silent = state.silent.load(Ordering::SeqCst);
            ipp_response(req.request_id, job_id, poll_answer_state(silent))
        }
        Ok(req) => ipp_response(req.request_id, 0, 9),
        Err(e) => {
            eprintln!(
                "  fake IPP printer: unreadable request ({} B): {e:#}",
                body.len()
            );
            ipp_response(0, 0, 8)
        }
    };
    ([(CONTENT_TYPE, "application/ipp")], reply)
}

struct FakeIppPrinter {
    state: FakeState,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl FakeIppPrinter {
    async fn start() -> Result<Self> {
        let state = FakeState::default();
        let app = Router::new()
            .route("/ipp/print", post(fake_ipp))
            .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", FAKE_IPP_PRINTER_PORT))
            .await
            .with_context(|| format!("bind the fake IPP printer on :{FAKE_IPP_PRINTER_PORT}"))?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Self {
            state,
            server: Some(server),
        })
    }

    fn jobs(&self) -> Vec<ReceivedJob> {
        self.state.jobs.lock().expect("fake IPP state").clone()
    }

    /// #114: from now on accept every Print-Job but never confirm its end.
    fn go_silent(&self) {
        self.state.silent.store(true, Ordering::SeqCst);
    }

    /// Close the port: the next send must fail to connect (#112).
    async fn stop(mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
    }
}

impl Drop for FakeIppPrinter {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

// ── The step ───────────────────────────────────────────────────────────────

fn marker(tag: &str) -> String {
    format!(
        "devbridge-e2e-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

async fn printer_job_ids(
    client: &reqwest::Client,
    server_base: &str,
    ipp_name: &str,
) -> Result<Vec<String>> {
    let jobs = get_json(client, &format!("{server_base}/api/jobs")).await?;
    Ok(jobs
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|j| j["printer"] == ipp_name)
                .filter_map(|j| j["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// Submit `pdf` to the VP `ipp_name` over the server's IPP port and return
/// the new server job id.
async fn submit(
    client: &reqwest::Client,
    server_base: &str,
    ipp_base: &str,
    ipp_name: &str,
    pdf: &[u8],
) -> Result<String> {
    let before = printer_job_ids(client, server_base, ipp_name).await?;
    let url = format!("{ipp_base}/printers/{ipp_name}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/ipp")
        .body(crate::build_ipp_print_job(pdf))
        .send()
        .await
        .with_context(|| format!("IPP Print-Job to {url}"))?;
    ensure!(
        resp.status().is_success(),
        "IPP Print-Job to {url} -> {}",
        resp.status()
    );
    let start = Instant::now();
    loop {
        let now = printer_job_ids(client, server_base, ipp_name).await?;
        if let Some(id) = now.into_iter().find(|id| !before.contains(id)) {
            return Ok(id);
        }
        if start.elapsed() > JOB_ARRIVE_WAIT {
            bail!(
                "no new job on '{ipp_name}' reached the server within {}s",
                JOB_ARRIVE_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn job_events(
    client: &reqwest::Client,
    server_base: &str,
    job_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let events = get_json(client, &format!("{server_base}/api/jobs/{job_id}/events")).await?;
    Ok(events.as_array().cloned().unwrap_or_default())
}

/// Is `detail` the #112 send-failure event of a job printed to the fake
/// printer's port, with the OS-level cause at the end of the chain (the
/// client names the printer by its `printer_address`, pz-server's VPN IP)?
pub fn is_send_failure_with_cause(detail: &str) -> bool {
    detail.starts_with("IPP Print-Job send failed (")
        && detail.contains(&format!(":{FAKE_IPP_PRINTER_PORT}): "))
        && detail.contains("os error")
}

/// Is `detail` the #114 event of a job the silent fake accepted: the
/// printer's job-id, the poll deadline with the last state the fake
/// answered, and "not resent"?
pub fn is_unconfirmed_after_deadline(detail: &str) -> bool {
    detail.starts_with("printer accepted job-id=")
        && detail.contains(&format!(
            ", completion not confirmed (no final job-state within 60s, last state={STATE_PROCESSING})"
        ))
        && detail.ends_with(" — not resent")
}

/// `(status, retry_count)` of server job `job_id`.
async fn job_status(
    client: &reqwest::Client,
    server_base: &str,
    job_id: &str,
) -> Result<(String, u64)> {
    let jobs = get_json(client, &format!("{server_base}/api/jobs")).await?;
    let job = jobs
        .as_array()
        .and_then(|a| a.iter().find(|j| j["id"] == job_id))
        .with_context(|| format!("job {job_id} is not on the server any more"))?;
    Ok((
        job["status"].as_str().unwrap_or_default().to_string(),
        job["retry_count"].as_u64().unwrap_or(u64::MAX),
    ))
}

/// #114 part of the step: the fake goes silent; the next job must end
/// `unconfirmed` on the server, never resent.
async fn verify_unconfirmed_never_resent(
    client: &reqwest::Client,
    server_base: &str,
    ipp_base: &str,
    ipp_name: &str,
    fake: &FakeIppPrinter,
) -> Result<()> {
    fake.go_silent();
    let before = fake.jobs().len();
    let pdf = test_pdf(&[(595, 842)], &marker("ipp-unconfirmed"));
    let job_id = submit(client, server_base, ipp_base, ipp_name, &pdf).await?;
    println!(
        "  server job {} sent to a printer that never confirms",
        &job_id[..8]
    );
    let start = Instant::now();
    loop {
        let (status, retries) = job_status(client, server_base, &job_id).await?;
        if status == "unconfirmed" {
            break;
        }
        // The client's `unconfirmed` event travels as FAILED on the status
        // stream, but this server reads its stage and stores `unconfirmed`
        // at once: a `failed` here is a bug, a resend shows as retries > 0.
        if status == "completed" || status == "failed" || retries > 0 {
            let events = job_events(client, server_base, &job_id).await?;
            bail!(
                "job {job_id} accepted by a printer that never confirms: status '{status}', retry_count {retries} — expected unconfirmed, never resent (events: {events:?})"
            );
        }
        if start.elapsed() > UNCONFIRMED_WAIT {
            let events = job_events(client, server_base, &job_id).await?;
            bail!(
                "job {job_id} not unconfirmed within {}s (status '{status}', events: {events:?})",
                UNCONFIRMED_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let events = job_events(client, server_base, &job_id).await?;
    let detail = events
        .iter()
        .filter(|e| e["stage"] == "unconfirmed")
        .filter_map(|e| e["detail"].as_str())
        .find(|d| is_unconfirmed_after_deadline(d))
        .with_context(|| format!("no unconfirmed event naming why: {events:?}"))?
        .to_string();
    println!("  unconfirmed: {detail}");

    // a resend would reach the fake after the server's retry delay
    tokio::time::sleep(RESEND_WATCH).await;
    let (status, retries) = job_status(client, server_base, &job_id).await?;
    let print_jobs = fake.jobs().len() - before;
    ensure!(
        status == "unconfirmed" && retries == 0,
        "job {job_id} after {}s: status '{status}', retry_count {retries} — expected unconfirmed, 0",
        RESEND_WATCH.as_secs()
    );
    ensure!(
        print_jobs == 1,
        "the printer got {print_jobs} Print-Jobs for job {job_id} — expected exactly ONE (never resent)"
    );
    println!(
        "  never resent: after {}s still unconfirmed, retry_count 0, ONE Print-Job",
        RESEND_WATCH.as_secs()
    );
    Ok(())
}

/// Step body — see the module docs.
pub async fn test_direct_ipp(
    client: &reqwest::Client,
    server_base: &str,
    server_host: &str,
    ipp_port: &str,
) -> Result<()> {
    let fake = FakeIppPrinter::start().await?;

    // 1. Registered, still pending, asking for its VP.
    let start = Instant::now();
    let ipp_client = loop {
        let clients = get_json(client, &format!("{server_base}/api/clients")).await?;
        if let Some(c) = clients
            .as_array()
            .and_then(|a| a.iter().find(|c| c["machine_id"] == E2E_IPP_CLIENT_ID))
        {
            break c.clone();
        }
        if start.elapsed() > CLIENT_WAIT {
            bail!(
                "{E2E_IPP_CLIENT_ID} never registered with the server within {}s (clients: {clients})",
                CLIENT_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    ensure!(
        ipp_client["virtual_printer_name"] == E2E_IPP_VP_NAME,
        "client did not send its virtual_printer_name: {ipp_client}"
    );
    ensure!(
        ipp_client["pairing_state"] == "pending",
        "{E2E_IPP_CLIENT_ID} must still be pending (e2e-wait-ready must not auto-approve it): {ipp_client}"
    );

    // 2. Approve -> VP paired to it. Rejected again on EVERY path, so it can
    // never take a default-queue job of the retry test.
    let approved = post_json(
        client,
        &format!("{server_base}/api/clients/{E2E_IPP_CLIENT_ID}/approve"),
    )
    .await?;
    let result =
        verify_approved_ipp_lane(client, server_base, server_host, ipp_port, &approved, fake).await;
    let rejected = post_json(
        client,
        &format!("{server_base}/api/clients/{E2E_IPP_CLIENT_ID}/reject"),
    )
    .await;
    result?;
    rejected.context("could not reject the direct_ipp E2E client after its step")?;
    println!("PASS");
    Ok(())
}

async fn verify_approved_ipp_lane(
    client: &reqwest::Client,
    server_base: &str,
    server_host: &str,
    ipp_port: &str,
    approved: &serde_json::Value,
    fake: FakeIppPrinter,
) -> Result<()> {
    let ipp_name = approved["virtual_printer"]["ipp_name"]
        .as_str()
        .with_context(|| format!("approval created no VP: {approved}"))?
        .to_string();
    ensure!(
        ipp_name == slugify(E2E_IPP_VP_NAME),
        "unexpected ipp_name {ipp_name}"
    );
    let ipp_base = format!("http://{server_host}:{ipp_port}");

    // 3. #111: P / L / A5 through the real direct path.
    let pdf = test_pdf(
        &[(595, 842), (842, 595), (420, 595)],
        &marker("ipp-orientation"),
    );
    let job_id = submit(client, server_base, &ipp_base, &ipp_name, &pdf).await?;
    println!(
        "  server job {} (P / L / A5 PDF, {} B)",
        &job_id[..8],
        pdf.len()
    );
    let start = Instant::now();
    loop {
        let jobs = get_json(client, &format!("{server_base}/api/jobs")).await?;
        let status = jobs
            .as_array()
            .and_then(|a| a.iter().find(|j| j["id"] == job_id.as_str()))
            .and_then(|j| j["status"].as_str().map(String::from))
            .unwrap_or_default();
        if status == "completed" {
            break;
        }
        if status == "failed" || start.elapsed() > JOB_COMPLETE_WAIT {
            let events = job_events(client, server_base, &job_id).await?;
            bail!("direct_ipp job {job_id} status '{status}' (events: {events:?})");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let received = fake.jobs();
    ensure!(
        received.len() == 1,
        "the 3-page document must reach the printer as ONE Print-Job, got {}",
        received.len()
    );
    let job = &received[0];
    ensure!(
        job.format == "image/urf",
        "document-format {:?}",
        job.format
    );
    ensure!(
        job.copies == Some(EXPECTED_COPIES),
        "copies {:?} (expected {EXPECTED_COPIES})",
        job.copies
    );
    let (count, pages) = urf_pages(&job.document)?;
    check_portrait_pages(&pages)?;
    println!(
        "  printer got ONE URF Print-Job: {} B, header count {count}, pages {}",
        job.document.len(),
        pages
            .iter()
            .map(|p| format!("{}x{} dark {}", p.width, p.height, p.dark))
            .collect::<Vec<_>>()
            .join(" / ")
    );
    println!(
        "  scale 1: page 2 dark {} vs its bar {}, page 3 dark {} vs its bar {} (within 2 %)",
        pages[1].dark,
        expected_dark(bar_area_pt2(842), pages[1].dpi),
        pages[2].dark,
        expected_dark(bar_area_pt2(420), pages[2].dpi)
    );
    let events = job_events(client, server_base, &job_id).await?;
    let rendered = events
        .iter()
        .filter(|e| e["stage"] == "rendered")
        .filter_map(|e| e["detail"].as_str())
        .find(|d| d.contains("orientation: 2 portrait, 1 landscape (rotated 90°)"))
        .with_context(|| format!("no rendered event naming the orientation: {events:?}"))?;
    println!("  rendered: {rendered}");

    // 4. #114: accepted, never confirmed -> unconfirmed, never resent.
    verify_unconfirmed_never_resent(client, server_base, &ipp_base, &ipp_name, &fake).await?;

    // 5. #112: the printer is gone -> the send error with its cause.
    fake.stop().await;
    let pdf = test_pdf(&[(595, 842)], &marker("ipp-unreachable"));
    let job_id = submit(client, server_base, &ipp_base, &ipp_name, &pdf).await?;
    println!("  server job {} sent to the stopped printer", &job_id[..8]);
    let start = Instant::now();
    loop {
        let events = job_events(client, server_base, &job_id).await?;
        if let Some(detail) = events
            .iter()
            .filter_map(|e| e["detail"].as_str())
            .find(|d| is_send_failure_with_cause(d))
        {
            println!("  failed: {detail}");
            return Ok(());
        }
        if start.elapsed() > SEND_FAILURE_WAIT {
            bail!(
                "job {job_id} to the stopped printer never recorded 'IPP Print-Job send failed (…:{FAKE_IPP_PRINTER_PORT}): … os error …' within {}s (events: {events:?})",
                SEND_FAILURE_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print_job_request(copies: Option<i32>, document: &[u8]) -> Vec<u8> {
        let mut b = vec![2, 0, 0, 2, 0, 0, 0, 42, 0x01];
        write_attribute(&mut b, 0x47, "attributes-charset", b"utf-8");
        write_attribute(&mut b, 0x49, "document-format", b"image/urf");
        write_attribute(&mut b, 0x44, "requested-attributes", b"job-id");
        write_attribute(&mut b, 0x44, "", b"job-state");
        if let Some(c) = copies {
            b.push(0x02);
            write_attribute(&mut b, 0x21, "copies", &c.to_be_bytes());
        }
        b.push(0x03);
        b.extend_from_slice(document);
        b
    }

    #[test]
    fn parses_a_print_job_request() {
        let req = parse_ipp_request(&print_job_request(Some(3), b"UNIRAST\0data")).unwrap();
        assert_eq!(req.operation, OP_PRINT_JOB);
        assert_eq!(req.request_id, 42);
        assert_eq!(req.text("document-format").as_deref(), Some("image/urf"));
        assert_eq!(req.integer("copies"), Some(3));
        assert_eq!(req.document, b"UNIRAST\0data");
        // 1setOf: the additional value keeps the attribute's name
        let values: Vec<&[u8]> = req
            .attributes
            .iter()
            .filter(|(n, _)| n == "requested-attributes")
            .map(|(_, v)| v.as_slice())
            .collect();
        assert_eq!(values, vec![&b"job-id"[..], &b"job-state"[..]]);
        let req = parse_ipp_request(&print_job_request(None, b"")).unwrap();
        assert_eq!(req.integer("copies"), None);
        assert!(req.document.is_empty());
    }

    #[test]
    fn refuses_a_truncated_request() {
        let full = print_job_request(Some(3), b"");
        assert!(parse_ipp_request(&full[..8]).is_err());
        assert!(parse_ipp_request(&full[..full.len() - 1]).is_err());
        assert!(parse_ipp_request(&full[..12]).is_err());
        let mut orphan = vec![2, 0, 0, 2, 0, 0, 0, 1, 0x01];
        write_attribute(&mut orphan, 0x44, "", b"x");
        orphan.push(0x03);
        assert!(parse_ipp_request(&orphan).is_err());
    }

    #[test]
    fn response_reads_back_as_job_id_and_state() {
        let resp = ipp_response(7, 5, 9);
        assert_eq!(&resp[..4], &[1, 1, 0, 0], "IPP/1.1 successful-ok");
        // A response has the request's layout: parse it with the same parser.
        let parsed = parse_ipp_request(&resp).unwrap();
        assert_eq!(parsed.request_id, 7);
        assert_eq!(parsed.integer("job-id"), Some(5));
        assert_eq!(parsed.integer("job-state"), Some(9));
        assert_eq!(parsed.text("job-state-reasons").as_deref(), Some("none"));
        assert!(parsed.document.is_empty());
    }

    #[test]
    fn test_pdf_has_one_page_object_per_size_and_a_valid_xref() {
        let pdf = test_pdf(&[(595, 842), (842, 595), (420, 595)], "m-111");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF-1.4\n") && text.ends_with("%%EOF\n"));
        assert!(text.contains("/Count 3"));
        for media in ["[0 0 595 842]", "[0 0 842 595]", "[0 0 420 595]"] {
            assert!(text.contains(media), "{media}");
        }
        assert!(text.contains("(m-111) Tj"));
        // only page 1 carries text: the other pages' ink is exactly their bar
        assert_eq!(text.matches(" Tj ").count(), 1);
        assert!(text.contains("20 20 802 30 re f") && text.contains("20 20 380 30 re f"));
        // every xref offset points at its "N 0 obj"
        let xref = text.find("xref\n").unwrap();
        for (i, line) in text[xref..].lines().skip(3).take(9).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(
                text[off..].starts_with(&format!("{} 0 obj", i + 1)),
                "object {} offset {off}",
                i + 1
            );
        }
    }

    #[test]
    fn silent_fake_answers_processing() {
        assert_eq!(poll_answer_state(false), 9);
        assert_eq!(poll_answer_state(true), 5);
        let resp = ipp_response(3, 7, poll_answer_state(true));
        assert_eq!(
            parse_ipp_request(&resp).unwrap().integer("job-state"),
            Some(5)
        );
    }

    #[test]
    fn unconfirmed_matcher() {
        assert!(is_unconfirmed_after_deadline(
            "printer accepted job-id=3, completion not confirmed \
             (no final job-state within 60s, last state=5) — not resent"
        ));
        // a lost poll is unconfirmed too, but not what the silent fake causes
        assert!(!is_unconfirmed_after_deadline(
            "printer accepted job-id=3, completion not confirmed \
             (Get-Job-Attributes poll failed: os error 10061) — not resent"
        ));
        assert!(!is_unconfirmed_after_deadline(
            "IPP job-state polling timeout after 60s, job-id=3, last state=5"
        ));
        assert!(!is_unconfirmed_after_deadline(
            "printer accepted job-id=3, completion not confirmed \
             (no final job-state within 60s, last state=5)"
        ));
    }

    #[test]
    fn send_failure_matcher() {
        assert!(is_send_failure_with_cause(
            "IPP Print-Job send failed (10.88.1.100:9232): error sending request for url \
             (http://10.88.1.100:9232/ipp/print): client error (Connect): tcp connect error: \
             No connection could be made because the target machine actively refused it. \
             (os error 10061)"
        ));
        // reqwest's top-level message alone = the #112 bug
        assert!(!is_send_failure_with_cause(
            "IPP Print-Job send failed (10.88.1.100:9232): error sending request for url \
             (http://10.88.1.100:9232/ipp/print)"
        ));
        assert!(!is_send_failure_with_cause(
            "IPP Print-Job send failed (10.78.10.9:631): tcp connect error (os error 10060)"
        ));
        assert!(!is_send_failure_with_cause(
            "error sending request for url (http://10.88.1.100:9232/ipp/print): (os error 10061)"
        ));
    }
}
