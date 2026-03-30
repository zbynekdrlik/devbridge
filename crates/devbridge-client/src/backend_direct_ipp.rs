use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::ipp_codec;
use crate::print_backend::{PrintBackend, PrintJobInfo};

/// Direct IPP backend — Ghostscript renders PDF to PWG-Raster, sends via IPP Print-Job.
pub struct DirectIpp {
    address: String,
    gs_device: String,
    gs_resolution: u32,
}

impl DirectIpp {
    pub fn new(address: String, gs_device: String, gs_resolution: u32) -> Self {
        Self {
            address,
            gs_device,
            gs_resolution,
        }
    }

    fn ipp_url(&self) -> String {
        if self.address.contains('/') {
            format!("http://{}", self.address)
        } else {
            format!("http://{}/ipp/print", self.address)
        }
    }

    fn printer_uri(&self) -> String {
        if self.address.contains('/') {
            format!("ipp://{}", self.address)
        } else {
            format!("ipp://{}/ipp/print", self.address)
        }
    }

    fn poll_job_completion(
        &self,
        printer_job_id: u32,
        job_id: &str,
        events: &EventEmitter,
    ) -> Result<()> {
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let client = reqwest::blocking::Client::new();
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
                    events.emit_ok(
                        job_id,
                        PrintStage::Completed,
                        format!("printer job-id={}, state=completed", printer_job_id),
                    );
                    return Ok(());
                }
                7 | 8 => {
                    let detail = format!(
                        "printer job-id={}, state={}, reasons={}",
                        printer_job_id,
                        if job_state == 7 {
                            "canceled"
                        } else {
                            "aborted"
                        },
                        state_reasons
                    );
                    events.emit_fail(job_id, PrintStage::Failed, &detail);
                    anyhow::bail!("{}", detail);
                }
                _ => {
                    if std::time::Instant::now() > deadline {
                        let detail = format!(
                            "printer job-id={} still in state {} after 60s",
                            printer_job_id, job_state
                        );
                        warn!("{}", detail);
                        events.emit_ok(
                            job_id,
                            PrintStage::Completed,
                            format!(
                                "printer job-id={}, state={} (poll timeout, likely printing)",
                                printer_job_id, job_state
                            ),
                        );
                        return Ok(());
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
        // Step 1: Render PDF → PWG-Raster
        let output_path = pdf_path.with_extension("pwg");
        let _render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
        )?;

        // Step 2: Build IPP Print-Job request
        let url = self.ipp_url();
        let printer_uri = self.printer_uri();
        let raster_data = std::fs::read(&output_path)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("IPP Print-Job to {}", self.address),
        );

        let ipp_header = ipp_codec::build_print_job_request(
            &printer_uri,
            "image/pwg-raster",
            &job.document_name,
            1,
        );

        let mut body = ipp_header;
        body.extend_from_slice(&raster_data);

        // Step 3: Send via HTTP POST
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
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
            format!("printer job-id={}, state={}", printer_job_id, job_state),
        );

        info!(job_id = %job.job_id, printer_job_id, job_state,
            address = %self.address, "IPP Print-Job accepted");

        // Step 4: Poll for completion
        self.poll_job_completion(printer_job_id, &job.job_id, events)?;

        // Clean up temp raster file
        let _ = std::fs::remove_file(&output_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direct_ipp_name() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600);
        assert_eq!(backend.name(), "direct_ipp");
    }

    #[test]
    fn test_ipp_url_without_path() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600);
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_ipp_url_with_path() {
        let backend = DirectIpp::new("10.78.2.9:631/ipp/print".into(), "pwgraster".into(), 600);
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_printer_uri() {
        let backend = DirectIpp::new("10.78.2.9:631".into(), "pwgraster".into(), 600);
        assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
    }
}
