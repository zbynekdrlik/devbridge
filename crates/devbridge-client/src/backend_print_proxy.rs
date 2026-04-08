//! HTTP print proxy backend.
//!
//! Sends PDF files to a remote print proxy via HTTP POST using curl.
//! Used when the client cannot reach the printer directly (e.g., network
//! isolation) but can reach a proxy that handles the actual printing.

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use devbridge_core::job_event::{EventEmitter, PrintStage};
use tracing::{info, warn};

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct PrintProxyBackend {
    proxy_url: String,
}

impl PrintProxyBackend {
    pub fn new(proxy_url: String) -> Self {
        Self { proxy_url }
    }
}

impl PrintBackend for PrintProxyBackend {
    fn name(&self) -> &str {
        "print_proxy"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let display = job
            .printer_display_name
            .as_deref()
            .unwrap_or(&job.printer_name);

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("Proxy → {}", display),
        );

        info!(
            proxy_url = %self.proxy_url,
            pdf_path = %pdf_path.display(),
            "sending PDF to print proxy"
        );

        let output = Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "--data-binary",
                &format!("@{}", pdf_path.display()),
                "-H",
                "Content-Type: application/pdf",
                "-w",
                "\n%{http_code}",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                &self.proxy_url,
            ])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.trim().lines().collect();
        let http_code = lines
            .last()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = if lines.len() > 1 {
            lines[..lines.len() - 1].join("\n")
        } else {
            String::new()
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                proxy_url = %self.proxy_url,
                stderr = %stderr,
                "curl failed to reach print proxy"
            );
            events.emit_fail(
                &job.job_id,
                PrintStage::Failed,
                format!(
                    "Proxy unreachable: {}",
                    stderr.chars().take(200).collect::<String>()
                ),
            );
            anyhow::bail!("print proxy unreachable: {}", stderr);
        }

        if http_code == 200 {
            info!(proxy_url = %self.proxy_url, "print proxy accepted job");
            events.emit_verified(
                &job.job_id,
                "none",
                format!("Proxied to {} — no local verification", self.proxy_url),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Printed via proxy for {}", display),
            );
            Ok(())
        } else {
            warn!(
                proxy_url = %self.proxy_url,
                http_code,
                body = %body,
                "print proxy rejected job"
            );
            events.emit_fail(
                &job.job_id,
                PrintStage::Failed,
                format!(
                    "Proxy error {}: {}",
                    http_code,
                    body.chars().take(200).collect::<String>()
                ),
            );
            anyhow::bail!("print proxy returned {}: {}", http_code, body)
        }
    }
}
