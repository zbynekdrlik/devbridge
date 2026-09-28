//! Windows half of the heartbeat printer-status read (#90). Compiled only on
//! Windows, so `.cargo/mutants.toml` excludes it; the query text and the
//! status mapping are tested helpers in `printer_status.rs`.

use std::process::Command;

pub(super) fn query(printer: &str) -> Option<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &super::status_query(printer)])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        Ok(o) => {
            tracing::warn!(
                printer,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "Get-Printer failed — heartbeat reports printer status unknown"
            );
            None
        }
        Err(e) => {
            tracing::warn!(printer, error = %e, "could not run powershell for printer status");
            None
        }
    }
}
