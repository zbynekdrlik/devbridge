use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

use crate::ipp_codec;
use crate::print_backend::{PrintBackend, PrintJobInfo};

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
    ) -> Result<()> {
        // Step 2: Build IPP Print-Job request
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let raster_data = std::fs::read(output_path)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("IPP Print-Job → {} ({})", display, self.address),
        );

        // Map Ghostscript device to IPP document-format MIME type
        let doc_format = match self.gs_device.as_str() {
            "urfrgb" | "urfcmyk" | "urfgray" => "image/urf",
            "pclm" | "pclm8" => "application/PCLm",
            "jpeg" | "jpeggray" | "jpegcmyk" => "image/jpeg",
            "png16m" | "pnggray" | "pngmono" | "pngalpha" => "image/png",
            _ => "image/pwg-raster",
        };

        let ipp_header =
            ipp_codec::build_print_job_request(&printer_uri, doc_format, &job.document_name, 1);

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
        self.poll_job_completion(printer_job_id, &job.job_id, display, events)?;

        Ok(())
    }

    fn poll_job_completion(
        &self,
        printer_job_id: u32,
        job_id: &str,
        display: &str,
        events: &EventEmitter,
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

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let display = job
            .printer_display_name
            .as_deref()
            .unwrap_or(&job.printer_name);

        // Step 1: Render PDF → PWG-Raster
        let output_path = pdf_path.with_extension("pwg");
        let render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
        )?;

        info!(job_id = %job.job_id, pages = render_result.pages,
            size = render_result.output_size, device = %self.gs_device, "rendered for IPP");

        // Steps 2-4: Send IPP job and poll for completion
        let result = self.send_ipp_job(job, &output_path, display, events);

        // Clean up temp raster file regardless of success or failure
        let _ = std::fs::remove_file(&output_path);

        result
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
}
