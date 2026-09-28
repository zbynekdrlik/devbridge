//! Windows half of the heartbeat printer-status read (#90). Compiled only on
//! Windows, so `.cargo/mutants.toml` excludes it; the query text and the
//! status mapping are tested helpers in `printer_status.rs`.
//!
//! The `Get-Printer` call is bounded: a hung spooler — exactly when the
//! status matters — must not stall the heartbeat, so the PowerShell child is
//! killed after [`QUERY_TIMEOUT`] and the status reported as unknown.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

pub(super) fn query(printer: &str) -> Option<String> {
    let mut child = match Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &super::status_query(printer),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(printer, error = %e, "could not run powershell for printer status");
            return None;
        }
    };
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    printer,
                    timeout_secs = QUERY_TIMEOUT.as_secs(),
                    "Get-Printer did not answer in time (spooler hung?) — heartbeat reports status unknown"
                );
                return None;
            }
            Err(e) => {
                tracing::warn!(printer, error = %e, "waiting for Get-Printer failed");
                let _ = child.kill();
                return None;
            }
        }
    };
    // The output is one short line, far below the pipe buffer, so reading
    // after exit cannot deadlock.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    if !status.success() {
        tracing::warn!(
            printer,
            stderr = %stderr.trim(),
            "Get-Printer failed — heartbeat reports printer status unknown"
        );
        return None;
    }
    let s = stdout.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
