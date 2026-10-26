use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

use crate::ipp_codec;
use crate::print_backend::{PrintBackend, PrintJobInfo, Unconfirmed, bail_if_cancelled};

/// How long a job the printer accepted is polled for its final job-state.
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Map a Ghostscript output-device name to the matching IPP
/// `document-format` MIME type.
///
/// Printer rejects with 0x040a client-error-document-format-not-supported
/// if we lie about the content type. Previous fall-through for ps2write /
/// pxlmono was `image/pwg-raster`, which any strict printer rejected.
/// Registered MIMEs (IANA / RFC 3380 / HP vendor prefixes):
/// - PostScript: application/postscript
/// - PDF: application/pdf
/// - PCL 5: application/vnd.hp-PCL
/// - PCL XL: application/vnd.hp-PCLXL (no hyphen — IANA registered form)
/// - PWG raster: image/pwg-raster (safe default for unknown devices)
pub(crate) fn gs_device_to_ipp_mime(gs_device: &str) -> &'static str {
    match gs_device {
        // Raster formats
        "urfrgb" | "urfcmyk" | "urfgray" => "image/urf",
        "pclm" | "pclm8" => "application/PCLm",
        "jpeg" | "jpeggray" | "jpegcmyk" => "image/jpeg",
        "png16m" | "pnggray" | "pngmono" | "pngalpha" => "image/png",
        "tiffg3" | "tiffg4" | "tiff24nc" | "tiff32nc" | "tiffgray" => "image/tiff",
        "bmp256" | "bmp16m" | "bmpmono" | "bmpgray" => "image/bmp",
        // Page description languages
        "pdfwrite" => "application/pdf",
        "ps2write" | "pswrite" => "application/postscript",
        // PCL XL (PCL 6). IANA form has no hyphen between PCL and XL.
        "pxlmono" | "pxlcolor" => "application/vnd.hp-PCLXL",
        // Classic PCL 5
        "ljet4" | "ljet4d" | "ljet2p" | "laserjet" | "cljet5" | "cljet5c" | "cljet5pr"
        | "pcl5e" | "pcl5c" => "application/vnd.hp-PCL",
        // Unknown: default to PWG raster (safe for modern printers)
        _ => "image/pwg-raster",
    }
}

/// Build the `reqwest::blocking::Client` used for both the Print-Job send
/// and the Get-Job-Attributes poll, so the two call sites can never drift
/// out of sync on connection options (see #71).
///
/// `timeout` bounds one HTTP request: 120 s for the Print-Job send (large
/// raster bodies), 30 s for each Get-Job-Attributes poll — the same value
/// reqwest::blocking applied implicitly before, now stated explicitly so a
/// hung poll POST is visibly bounded (the outer cancel token is only observed
/// between poll ticks, never inside an in-flight `send()`).
///
/// # Title-Case headers (#71)
///
/// hyper — reqwest's HTTP/1.1 engine — sends header *names* lowercase
/// (`content-length:`) by default. Most printers tolerate this, but the
/// HP LaserJet M110w (verified live, store pjzav, 2026-09-09) does not:
/// its embedded HTTP parser treats header names case-sensitively, so it
/// never recognizes a lowercase `content-length:`, accepts the Print-Job
/// anyway (job-id assigned, job-state 3 pending), then aborts the job
/// (job-state 8, aborted-by-system) once it realizes it never got a body
/// length it understood. A byte-identical request with `Content-Length:`
/// title-cased completes normally (job-state 9). `http1_title_case_headers`
/// makes reqwest send `Content-Type:` / `Content-Length:` / `Accept:` /
/// `Host:` title-cased, which every printer DevBridge targets tolerates.
fn http_client(
    use_tls: bool,
    timeout: std::time::Duration,
) -> reqwest::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .http1_title_case_headers()
        // Accept self-signed certs for Epson IPPS printers over WireGuard VPN
        .danger_accept_invalid_certs(use_tls)
        .timeout(timeout)
        .build()
}

/// POST an IPP request `body` to `url` on `client` and return the raw
/// response bytes. Shared by [`DirectIpp::send_ipp_job`] and
/// [`DirectIpp::poll_job_completion`] so the `Content-Type` header can never
/// drift between the two call sites either.
fn post_ipp(client: &reqwest::blocking::Client, url: &str, body: Vec<u8>) -> Result<Vec<u8>> {
    let resp = client
        .post(url)
        .header("Content-Type", "application/ipp")
        .body(body)
        .send()?;
    Ok(resp.bytes()?.to_vec())
}

/// The events of ONE page's Print-Job: its progress (sending, acknowledged,
/// completed) always goes to `events`; its failure events (`failed`,
/// `unconfirmed`) only when `records_failures`.
struct PageEvents<'a> {
    events: &'a EventEmitter,
    records_failures: bool,
}

impl<'a> PageEvents<'a> {
    /// A page whose failure is the job's failure: its failure events are
    /// recorded.
    fn recording(events: &'a EventEmitter) -> Self {
        Self {
            events,
            records_failures: true,
        }
    }

    /// Record a failure event of this page (see [`PageEvents`]).
    fn failure(&self, event: PrintJobEvent) {
        if self.records_failures {
            self.events.emit(event);
        }
    }
}

/// Record a direct_ipp transport / protocol failure where the store sees it
/// (#112): ONE `Failed` event and a WARN in the client log, both with the
/// whole `source()` chain — reqwest's top-level "error sending request for
/// url (…)" names neither "connect timed out" nor "refused" nor "reset".
/// `what` starts the event detail (e.g. "IPP Print-Job send failed").
/// Returns `err` with that context, so the job's `error_detail` sent to the
/// server (and its retry log) carries the cause too.
fn fail_with_cause(
    page: &PageEvents,
    job_id: &str,
    address: &str,
    url: &str,
    what: &str,
    err: anyhow::Error,
) -> anyhow::Error {
    let cause = format!("{err:#}");
    warn!(job_id, printer = %address, url = %url, error = %cause, "{what}");
    page.failure(PrintJobEvent::fail(
        job_id,
        PrintStage::Failed,
        format!("{what} ({address}): {cause}"),
    ));
    err.context(format!("{what} ({address})"))
}

/// The ONE `unconfirmed` event of `outcome` (#114).
fn unconfirmed_event(job_id: &str, outcome: &Unconfirmed) -> PrintJobEvent {
    let mut event = PrintJobEvent::fail(job_id, PrintStage::Unconfirmed, outcome.detail());
    event.verification_method = "ipp_job_state".into();
    event.verification_evidence = outcome.detail().to_string();
    event
}

/// #114: the printer ACCEPTED job `printer_job_id` but devbridge lost its end
/// (`why`). ONE `unconfirmed` event + a WARN, and an [`Unconfirmed`] error so
/// the server never resends the job (no `Failed` event: the status stream
/// must not call it a failure).
fn unconfirmed(
    page: &PageEvents,
    job_id: &str,
    address: &str,
    printer_job_id: u32,
    why: &str,
) -> anyhow::Error {
    let outcome = Unconfirmed::after_acceptance(printer_job_id, why);
    warn!(job_id, printer = %address, printer_job_id, why, "{}", outcome.detail());
    page.failure(unconfirmed_event(job_id, &outcome));
    anyhow::Error::new(outcome)
}

/// Direct IPP backend — Ghostscript renders PDF to raster, sends via IPP Print-Job.
pub struct DirectIpp {
    address: String,
    gs_device: String,
    gs_resolution: u32,
    use_tls: bool,
    /// How long an accepted job is polled for its final job-state
    /// ([`POLL_TIMEOUT`]; shorter in tests).
    poll_timeout: std::time::Duration,
}

impl DirectIpp {
    pub fn new(address: String, gs_device: String, gs_resolution: u32, use_tls: bool) -> Self {
        if !address.contains(':') && !address.contains('/') {
            warn!(
                address = %address,
                "printer_address has no port, defaulting to :631 (IPP default)"
            );
        }
        Self {
            address,
            gs_device,
            gs_resolution,
            use_tls,
            poll_timeout: POLL_TIMEOUT,
        }
    }

    /// Returns `address` unchanged if it already contains a port (`host:port`)
    /// or a path (`host/path`); otherwise appends `:631`, the default IPP
    /// port (RFC 8011).
    ///
    /// Assumes IPv4 addresses or bare hostnames. A bare IPv6 literal like
    /// `2001:db8::1` would be treated as "already has port" because it
    /// contains `:` — DevBridge deployments use IPv4 WireGuard exclusively,
    /// so this is intentional.
    fn normalized_address(&self) -> String {
        if self.address.contains(':') || self.address.contains('/') {
            self.address.clone()
        } else {
            format!("{}:631", self.address)
        }
    }

    fn ipp_url(&self) -> String {
        let scheme = if self.use_tls { "https" } else { "http" };
        let addr = self.normalized_address();
        if addr.contains('/') {
            format!("{}://{}", scheme, addr)
        } else {
            format!("{}://{}/ipp/print", scheme, addr)
        }
    }

    fn printer_uri(&self) -> String {
        let scheme = if self.use_tls { "ipps" } else { "ipp" };
        let addr = self.normalized_address();
        if addr.contains('/') {
            format!("{}://{}", scheme, addr)
        } else {
            format!("{}://{}/ipp/print", scheme, addr)
        }
    }

    fn send_ipp_job(
        &self,
        job: &PrintJobInfo,
        output_path: &Path,
        display: &str,
        page: &PageEvents,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Bail before opening a connection if the outer timeout already fired.
        bail_if_cancelled(
            cancel,
            &job.job_id,
            page.events,
            "before IPP Print-Job send",
        )?;

        // Step 2: Build IPP Print-Job request
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let raster_data = std::fs::read(output_path)?;

        page.events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("IPP Print-Job → {} ({})", display, self.address),
        );

        let doc_format = gs_device_to_ipp_mime(self.gs_device.as_str());

        // `job.copies` comes from the server-side JobMetadata captured out of
        // the originating IPP Print-Job / Create-Job request. Forward it so
        // the target printer produces the right number of sheets. See #37.
        let ipp_header = ipp_codec::build_print_job_request(
            &printer_uri,
            doc_format,
            &job.document_name,
            1,
            job.copies,
        );

        let mut body = ipp_header;
        body.extend_from_slice(&raster_data);

        // Step 3: Send via HTTP(S) POST
        let client = http_client(self.use_tls, std::time::Duration::from_secs(120))?;

        let fail = |what: &str, e| fail_with_cause(page, &job.job_id, &self.address, &url, what, e);
        let resp_bytes =
            post_ipp(&client, &url, body).map_err(|e| fail("IPP Print-Job send failed", e))?;
        let ipp_resp = ipp_codec::parse_response(&resp_bytes)
            .map_err(|e| fail("IPP Print-Job response unreadable", e))?;

        if !ipp_resp.is_success() {
            let detail = format!("IPP error status: 0x{:04x}", ipp_resp.status_code);
            page.failure(PrintJobEvent::fail(
                &job.job_id,
                PrintStage::Failed,
                &detail,
            ));
            anyhow::bail!("{}", detail);
        }

        let printer_job_id = ipp_resp.get("job-id").and_then(|a| a.as_i32()).unwrap_or(0) as u32;

        let job_state = ipp_resp
            .get("job-state")
            .and_then(|a| a.as_i32())
            .unwrap_or(0);

        page.events.emit_ok(
            &job.job_id,
            PrintStage::Acknowledged,
            format!("{} accepted job-id={}, processing", display, printer_job_id),
        );

        info!(job_id = %job.job_id, printer_job_id, job_state,
            address = %self.address, "IPP Print-Job accepted");

        // Step 4: Poll for completion. From here on the printer HOLDS the
        // job: a lost outcome is unconfirmed, never resent (#114).
        self.poll_job_completion(
            printer_job_id,
            job_state,
            &job.job_id,
            display,
            page,
            cancel,
        )?;

        Ok(())
    }

    /// Poll the ACCEPTED job `printer_job_id` (job-state `accepted_state` in
    /// the Print-Job answer) until the printer gives its final job-state.
    /// 9 = printed; 7 / 8 = a definite failure (nothing printed, the server
    /// retries it). Losing the outcome — the poll deadline, a poll that fails
    /// or cannot be read, the outer timeout's cancel — is [`Unconfirmed`]:
    /// the printer has the job, so it is never resent (#114).
    fn poll_job_completion(
        &self,
        printer_job_id: u32,
        accepted_state: i32,
        job_id: &str,
        display: &str,
        page: &PageEvents,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let client = http_client(self.use_tls, std::time::Duration::from_secs(30))?;
        let deadline = std::time::Instant::now() + self.poll_timeout;
        let mut request_id = 100u32;
        let mut last_state = accepted_state;
        let lost = |why: &str| unconfirmed(page, job_id, &self.address, printer_job_id, why);

        loop {
            // Cancellation check each tick: if the outer timeout fired, drop the
            // reqwest client (closing the in-flight HTTP keep-alive connection
            // to the printer) and stop polling a dead/hung job (issue #51).
            if cancel.is_cancelled() {
                warn!(
                    job_id,
                    printer_job_id,
                    "cancellation observed during IPP poll — dropping client connection and bailing (issue #51)"
                );
                drop(client);
                return Err(lost(&format!(
                    "print cancelled by the outer timeout while polling, last state={last_state}"
                )));
            }
            request_id += 1;
            let req_bytes = ipp_codec::build_get_job_attributes_request(
                &printer_uri,
                printer_job_id as i32,
                request_id,
            );

            let body = post_ipp(&client, &url, req_bytes)
                .map_err(|e| lost(&format!("Get-Job-Attributes poll failed: {e:#}")))?;
            let ipp_resp = ipp_codec::parse_response(&body)
                .map_err(|e| lost(&format!("Get-Job-Attributes response unreadable: {e:#}")))?;

            let job_state = ipp_resp
                .get("job-state")
                .and_then(|a| a.as_i32())
                .unwrap_or(0);
            let state_reasons = ipp_resp
                .get("job-state-reasons")
                .and_then(|a| a.as_str())
                .unwrap_or("none")
                .to_string();
            last_state = job_state;

            debug!(printer_job_id, job_state, state_reasons = %state_reasons, "IPP job state poll");

            // IPP: 3=pending, 5=processing, 7=canceled, 8=aborted, 9=completed
            match job_state {
                9 => {
                    let evidence =
                        format!("IPP job-state=9 (completed), job-id={}", printer_job_id);
                    page.events
                        .emit_verified(job_id, "ipp_job_state", &evidence);
                    page.events.emit_ok(
                        job_id,
                        PrintStage::Completed,
                        format!("{} confirmed printed", display),
                    );
                    return Ok(());
                }
                7 | 8 => {
                    let state_name = if job_state == 7 {
                        "canceled"
                    } else {
                        "aborted"
                    };
                    let evidence = format!(
                        "IPP job-state={} ({}), job-id={}, reasons={}",
                        job_state, state_name, printer_job_id, state_reasons
                    );
                    let mut fail_event = PrintJobEvent::fail(job_id, PrintStage::Failed, &evidence);
                    fail_event.verification_method = "ipp_job_state".into();
                    fail_event.verification_evidence = evidence.clone();
                    page.failure(fail_event);
                    anyhow::bail!("{}", evidence);
                }
                _ => {
                    if std::time::Instant::now() > deadline {
                        return Err(lost(&format!(
                            "no final job-state within {}s, last state={job_state}",
                            self.poll_timeout.as_secs()
                        )));
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}

impl PrintBackend for DirectIpp {
    fn name(&self) -> &str {
        "direct_ipp"
    }

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let display = job
            .printer_display_name
            .as_deref()
            .unwrap_or(&job.printer_name);

        // Step 1: Render PDF → raster (JPEG/PNG produces per-page files).
        // render() kills the Ghostscript child if `cancel` fires.
        let output_path = pdf_path.with_extension("pwg");
        let render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
            cancel,
        )?;

        info!(job_id = %job.job_id, pages = render_result.pages,
            page_files = render_result.page_files.len(),
            size = render_result.output_size, device = %self.gs_device, "rendered for IPP");

        // Steps 2-4: Send each page file as a separate IPP Print-Job.
        let sent = self.send_pages(job, &render_result.page_files, display, events, cancel);

        // Clean up all temp raster files
        for page_file in &render_result.page_files {
            let _ = std::fs::remove_file(page_file);
        }
        // Also clean up the original output_path in case it was used (multi-page device)
        let _ = std::fs::remove_file(&output_path);

        sent
    }
}

impl DirectIpp {
    /// Send each rendered page file as its own IPP Print-Job, in order,
    /// stopping at the first failure. A multi-page device (`urfgray`,
    /// `pwgraster`) renders ONE file holding every page; a per-page device
    /// (`jpeg` / `png`, e.g. pjpos' Epson L3260) one file per page.
    fn send_pages(
        &self,
        job: &PrintJobInfo,
        page_files: &[std::path::PathBuf],
        display: &str,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        for (i, page_file) in page_files.iter().enumerate() {
            // Check cancellation BETWEEN page sends — a multi-page job that
            // overran the outer timeout must not keep streaming pages to the
            // printer (issue #51). bail_if_cancelled emits Failed + errors.
            bail_if_cancelled(cancel, &job.job_id, events, "between IPP page sends")?;
            if page_files.len() > 1 {
                info!(job_id = %job.job_id, page = i + 1, total = page_files.len(),
                    "sending page via IPP");
            }
            self.send_ipp_job(
                job,
                page_file,
                display,
                &PageEvents::recording(events),
                cancel,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "backend_direct_ipp_tests.rs"]
mod tests;
