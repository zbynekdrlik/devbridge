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
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            // Accept self-signed certs for Epson IPPS printers over WireGuard VPN
            .danger_accept_invalid_certs(self.use_tls)
            .build()?;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/ipp")
            .body(body)
            .send()?;

        let resp_bytes = resp.bytes()?;
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
        let client = reqwest::blocking::Client::builder()
            // Accept self-signed certs for Epson IPPS printers over WireGuard VPN
            .danger_accept_invalid_certs(self.use_tls)
            .build()?;
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

            let resp = client
                .post(&url)
                .header("Content-Type", "application/ipp")
                .body(req_bytes)
                .send()?;

            let body = resp.bytes()?;
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
