use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

use crate::ipp_codec;
use crate::print_backend::{PrintBackend, PrintJobInfo, bail_if_cancelled};

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
/// `timeout` is `None` for the poll client (its own tick loop already bounds
/// total wait time) and `Some(120s)` for the Print-Job send.
fn http_client(
    use_tls: bool,
    timeout: Option<std::time::Duration>,
) -> reqwest::Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        // Accept self-signed certs for Epson IPPS printers over WireGuard VPN
        .danger_accept_invalid_certs(use_tls);
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    builder.build()
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

/// Direct IPP backend — Ghostscript renders PDF to raster, sends via IPP Print-Job.
pub struct DirectIpp {
    address: String,
    gs_device: String,
    gs_resolution: u32,
    use_tls: bool,
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
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Bail before opening a connection if the outer timeout already fired.
        bail_if_cancelled(cancel, &job.job_id, events, "before IPP Print-Job send")?;

        // Step 2: Build IPP Print-Job request
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let raster_data = std::fs::read(output_path)?;

        events.emit_ok(
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
        let client = http_client(self.use_tls, Some(std::time::Duration::from_secs(120)))?;

        let resp_bytes = post_ipp(&client, &url, body)?;
        let ipp_resp = ipp_codec::parse_response(&resp_bytes)?;

        if !ipp_resp.is_success() {
            let detail = format!("IPP error status: 0x{:04x}", ipp_resp.status_code);
            events.emit_fail(&job.job_id, PrintStage::Failed, &detail);
            anyhow::bail!("{}", detail);
        }

        let printer_job_id = ipp_resp.get("job-id").and_then(|a| a.as_i32()).unwrap_or(0) as u32;

        let job_state = ipp_resp
            .get("job-state")
            .and_then(|a| a.as_i32())
            .unwrap_or(0);

        events.emit_ok(
            &job.job_id,
            PrintStage::Acknowledged,
            format!("{} accepted job-id={}, processing", display, printer_job_id),
        );

        info!(job_id = %job.job_id, printer_job_id, job_state,
            address = %self.address, "IPP Print-Job accepted");

        // Step 4: Poll for completion
        self.poll_job_completion(printer_job_id, &job.job_id, display, events, cancel)?;

        Ok(())
    }

    fn poll_job_completion(
        &self,
        printer_job_id: u32,
        job_id: &str,
        display: &str,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let client = http_client(self.use_tls, None)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut request_id = 100u32;

        loop {
            // Cancellation check each tick: if the outer timeout fired, drop the
            // reqwest client (closing the in-flight HTTP keep-alive connection
            // to the printer) and bail so we stop polling a dead/hung job.
            if cancel.is_cancelled() {
                warn!(
                    job_id,
                    printer_job_id,
                    "cancellation observed during IPP poll — dropping client connection and bailing (issue #51)"
                );
                drop(client);
                let detail = "IPP poll cancelled by outer print timeout";
                events.emit_fail(job_id, PrintStage::Failed, detail);
                anyhow::bail!("{}", detail);
            }
            request_id += 1;
            let req_bytes = ipp_codec::build_get_job_attributes_request(
                &printer_uri,
                printer_job_id as i32,
                request_id,
            );

            let body = post_ipp(&client, &url, req_bytes)?;
            let ipp_resp = ipp_codec::parse_response(&body)?;

            let job_state = ipp_resp
                .get("job-state")
                .and_then(|a| a.as_i32())
                .unwrap_or(0);
            let state_reasons = ipp_resp
                .get("job-state-reasons")
                .and_then(|a| a.as_str())
                .unwrap_or("none")
                .to_string();

            debug!(printer_job_id, job_state, state_reasons = %state_reasons, "IPP job state poll");

            // IPP: 3=pending, 5=processing, 7=canceled, 8=aborted, 9=completed
            match job_state {
                9 => {
                    let evidence =
                        format!("IPP job-state=9 (completed), job-id={}", printer_job_id);
                    events.emit_verified(job_id, "ipp_job_state", &evidence);
                    events.emit_ok(
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
                    events.emit(fail_event);
                    anyhow::bail!("{}", evidence);
                }
                _ => {
                    if std::time::Instant::now() > deadline {
                        let evidence = format!(
                            "IPP job-state polling timeout after 60s, job-id={}, last state={}",
                            printer_job_id, job_state
                        );
                        warn!("{}", evidence);
                        let mut fail_event =
                            PrintJobEvent::fail(job_id, PrintStage::Failed, &evidence);
                        fail_event.verification_method = "ipp_job_state".into();
                        fail_event.verification_evidence = evidence.clone();
                        events.emit(fail_event);
                        anyhow::bail!("{}", evidence);
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
        // For multi-page devices (pwgraster), page_files has one entry containing all pages.
        // For single-page devices (jpeg/png), each page is a separate file.
        let mut last_err = None;
        for (i, page_file) in render_result.page_files.iter().enumerate() {
            // Check cancellation BETWEEN page sends — a multi-page job that
            // overran the outer timeout must not keep streaming pages to the
            // printer (issue #51). bail_if_cancelled emits Failed + errors.
            if let Err(e) = bail_if_cancelled(cancel, &job.job_id, events, "between IPP page sends")
            {
                last_err = Some(e);
                break;
            }
            if render_result.page_files.len() > 1 {
                info!(job_id = %job.job_id, page = i + 1, total = render_result.page_files.len(),
                    "sending page via IPP");
            }
            match self.send_ipp_job(job, page_file, display, events, cancel) {
                Ok(()) => {}
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }

        // Clean up all temp raster files
        for page_file in &render_result.page_files {
            let _ = std::fs::remove_file(page_file);
        }
        // Also clean up the original output_path in case it was used (multi-page device)
        let _ = std::fs::remove_file(&output_path);

        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for #71 (HP LaserJet M110w, store pjzav): the
    /// printer's embedded HTTP parser treats header *names*
    /// case-sensitively. hyper (reqwest's HTTP/1.1 engine) lowercases
    /// header names by default, so `content-length:` reaches the printer
    /// instead of `Content-Length:` — the HP accepts the Print-Job
    /// (job-id assigned, job-state 3 pending) but never sees the body
    /// length and aborts the job (job-state 8, aborted-by-system).
    ///
    /// This spins up a raw `TcpListener` mock IPP server, sends one
    /// Print-Job request through [`http_client`] + [`post_ipp`] — the
    /// exact helpers [`DirectIpp::send_ipp_job`] and
    /// [`DirectIpp::poll_job_completion`] both use — and asserts the raw
    /// request head carries Title-Case header names.
    ///
    /// One test covers both call sites: they share `http_client` and
    /// `post_ipp`, so there is nothing left that could differ between
    /// the Print-Job send and the Get-Job-Attributes poll (that is the
    /// whole point of extracting the shared helpers).
    #[test]
    fn test_http_client_sends_title_case_headers() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock IPP server");
        let addr = listener.local_addr().expect("mock server local_addr");
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock connection");

            // Read the raw request head (method/headers) up to the blank line.
            let mut head_buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).expect("read request byte");
                head_buf.push(byte[0]);
                if head_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&head_buf).to_string();

            // Case-insensitive Content-Length lookup so this drains the
            // body correctly whether the header name is lower- or
            // Title-Case — that casing is exactly what this test checks.
            let content_length = head
                .lines()
                .find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if content_length > 0 {
                let mut body_buf = vec![0u8; content_length];
                stream
                    .read_exact(&mut body_buf)
                    .expect("drain mock request body");
            }

            // Minimal valid IPP 1.1 successful-ok response: job-id=1, job-state=9 (completed).
            let mut ipp_body = Vec::new();
            ipp_body.push(1u8); // version-major
            ipp_body.push(1u8); // version-minor
            ipp_body.extend_from_slice(&0x0000u16.to_be_bytes()); // status-code: successful-ok
            ipp_body.extend_from_slice(&1u32.to_be_bytes()); // request-id
            ipp_body.push(0x01); // operation-attributes-tag
            ipp_codec::write_attribute(&mut ipp_body, 0x47, "attributes-charset", b"utf-8");
            ipp_codec::write_attribute(
                &mut ipp_body,
                0x48,
                "attributes-natural-language",
                b"en-us",
            );
            ipp_body.push(0x02); // job-attributes-tag
            ipp_codec::write_integer_attribute(&mut ipp_body, 0x21, "job-id", 1); // integer
            ipp_codec::write_integer_attribute(&mut ipp_body, 0x23, "job-state", 9); // enum
            ipp_body.push(0x03); // end-of-attributes-tag

            let response_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                ipp_body.len()
            );
            stream
                .write_all(response_head.as_bytes())
                .expect("write mock response head");
            stream
                .write_all(&ipp_body)
                .expect("write mock response body");
            stream.flush().ok();

            tx.send(head)
                .expect("send recorded request head to test thread");
        });

        let client =
            http_client(false, Some(std::time::Duration::from_secs(5))).expect("build http_client");
        let url = format!("http://{}/ipp/print", addr);
        let body = ipp_codec::build_print_job_request(
            "ipp://127.0.0.1/ipp/print",
            "application/PCLm",
            "regression-test-71",
            1,
            1,
        );

        let resp_bytes = post_ipp(&client, &url, body).expect("post_ipp to mock server");
        let ipp_resp = ipp_codec::parse_response(&resp_bytes).expect("parse mock IPP response");
        assert!(
            ipp_resp.is_success(),
            "mock server response must parse as success"
        );

        server.join().expect("join mock server thread");
        let head = rx.recv().expect("recv recorded request head");

        assert!(
            head.contains("Content-Length:"),
            "expected Title-Case 'Content-Length:' header, got request head:\n{head}"
        );
        assert!(
            head.contains("Content-Type: application/ipp"),
            "expected Title-Case 'Content-Type:' header, got request head:\n{head}"
        );
        assert!(
            !head.contains("content-length:"),
            "HP LaserJet M110w (#71) treats header names case-sensitively and \
             silently aborts the job on a lowercase content-length: header \
             — got request head:\n{head}"
        );
    }

    #[test]
    fn test_direct_ipp_name() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
        assert_eq!(backend.name(), "direct_ipp");
    }

    #[test]
    fn test_ipp_url_without_path() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_ipp_url_with_path() {
        let backend = DirectIpp::new(
            "10.78.2.9:631/ipp/print".into(),
            "pwgraster".into(),
            600,
            false,
        );
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_printer_uri() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600, false);
        assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_tls_url_uses_https() {
        let backend = DirectIpp::new("10.78.5.9:631".into(), "jpeg".into(), 360, true);
        assert_eq!(backend.ipp_url(), "https://10.78.5.9:631/ipp/print");
        assert_eq!(backend.printer_uri(), "ipps://10.78.5.9:631/ipp/print");
    }

    #[test]
    fn test_normalized_address_appends_default_port() {
        let backend = DirectIpp::new("10.78.2.9".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9:631");
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
        assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_normalized_address_keeps_explicit_port() {
        let backend = DirectIpp::new("10.78.2.9:9100".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9:9100");
    }

    #[test]
    fn test_normalized_address_keeps_path() {
        let backend = DirectIpp::new("10.78.2.9/printers/foo".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9/printers/foo");
    }

    #[test]
    fn test_normalized_address_with_tls_uses_https() {
        let backend = DirectIpp::new("10.78.5.9".into(), "jpeg".into(), 360, true);
        assert_eq!(backend.ipp_url(), "https://10.78.5.9:631/ipp/print");
    }

    #[test]
    fn test_gs_device_to_ipp_mime_raster() {
        assert_eq!(gs_device_to_ipp_mime("urfrgb"), "image/urf");
        assert_eq!(gs_device_to_ipp_mime("urfgray"), "image/urf");
        assert_eq!(gs_device_to_ipp_mime("pclm"), "application/PCLm");
        assert_eq!(gs_device_to_ipp_mime("jpeg"), "image/jpeg");
        assert_eq!(gs_device_to_ipp_mime("jpeggray"), "image/jpeg");
        assert_eq!(gs_device_to_ipp_mime("png16m"), "image/png");
        assert_eq!(gs_device_to_ipp_mime("pngmono"), "image/png");
        assert_eq!(gs_device_to_ipp_mime("tiffg4"), "image/tiff");
        assert_eq!(gs_device_to_ipp_mime("bmp256"), "image/bmp");
    }

    #[test]
    fn test_gs_device_to_ipp_mime_page_description_languages() {
        // The exact bug this function exists to fix: pxlmono and ps2write
        // must NOT fall through to image/pwg-raster. Xerox Phaser 3020
        // and other strict printers reject 0x040a otherwise.
        assert_eq!(gs_device_to_ipp_mime("ps2write"), "application/postscript");
        assert_eq!(gs_device_to_ipp_mime("pswrite"), "application/postscript");
        assert_eq!(gs_device_to_ipp_mime("pdfwrite"), "application/pdf");
        // IANA-registered MIME for PCL XL has no hyphen between PCL and XL.
        assert_eq!(gs_device_to_ipp_mime("pxlmono"), "application/vnd.hp-PCLXL");
        assert_eq!(
            gs_device_to_ipp_mime("pxlcolor"),
            "application/vnd.hp-PCLXL"
        );
        assert_eq!(gs_device_to_ipp_mime("ljet4"), "application/vnd.hp-PCL");
        assert_eq!(gs_device_to_ipp_mime("pcl5e"), "application/vnd.hp-PCL");
    }

    #[test]
    fn test_gs_device_to_ipp_mime_unknown_falls_back_to_pwg_raster() {
        assert_eq!(gs_device_to_ipp_mime(""), "image/pwg-raster");
        assert_eq!(gs_device_to_ipp_mime("pwgraster"), "image/pwg-raster");
        assert_eq!(
            gs_device_to_ipp_mime("whatever-new-device"),
            "image/pwg-raster"
        );
    }
}
