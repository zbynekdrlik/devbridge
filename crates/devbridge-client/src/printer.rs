use std::path::Path;

use anyhow::Result;
use devbridge_core::PrinterInfo;

/// List available printers on this system.
#[cfg(target_os = "windows")]
pub fn list_printers() -> Result<Vec<PrinterInfo>> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Printer | Select-Object Name, DriverName, PrinterStatus, JobCount | ConvertTo-Json",
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Get-Printer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    if trimmed.is_empty() {
        return Ok(vec![]);
    }

    // PowerShell returns a single object (not array) when only one printer exists.
    let raw: serde_json::Value = serde_json::from_str(trimmed)?;
    let items = match raw {
        serde_json::Value::Array(arr) => arr,
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => return Ok(vec![]),
    };

    let printers = items
        .iter()
        .map(|item| PrinterInfo {
            name: item["Name"].as_str().unwrap_or("Unknown").to_string(),
            driver: item["DriverName"].as_str().unwrap_or("-").to_string(),
            status: match item["PrinterStatus"].as_u64() {
                Some(0) => "normal".to_string(),
                Some(1) => "paused".to_string(),
                Some(2) => "error".to_string(),
                Some(3) => "pending deletion".to_string(),
                Some(4) => "paper jam".to_string(),
                Some(5) => "paper out".to_string(),
                Some(6) => "manual feed".to_string(),
                Some(7) => "paper problem".to_string(),
                Some(8) => "offline".to_string(),
                _ => "unknown".to_string(),
            },
            jobs: item["JobCount"].as_u64().unwrap_or(0),
            is_target: false,
        })
        .collect();

    Ok(printers)
}

/// List available printers on this system using CUPS (macOS/Linux).
#[cfg(not(target_os = "windows"))]
pub fn list_printers() -> Result<Vec<PrinterInfo>> {
    let output = match std::process::Command::new("lpstat").arg("-a").output() {
        Ok(o) => o,
        // lpstat not installed — return empty list
        Err(_) => return Ok(vec![]),
    };

    if !output.status.success() {
        // No printers configured is not an error
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let printers = stdout
        .lines()
        .filter_map(|line| {
            // Format: "PrinterName accepting requests since ..."
            let name = line.split_whitespace().next()?.to_string();
            let status = if line.contains("accepting") {
                "idle".to_string()
            } else {
                "error".to_string()
            };
            Some(PrinterInfo {
                name,
                driver: "CUPS".to_string(),
                status,
                jobs: 0,
                is_target: false,
            })
        })
        .collect();

    Ok(printers)
}

/// Send a PDF file to the specified printer using SumatraPDF CLI.
///
/// Uses SumatraPDF's `-print-to` + `-print-settings "Nx"` for reliable
/// headless printing with explicit copy count (see #37). Falls back to
/// `Start-Process -Verb PrintTo` in a loop when SumatraPDF is not installed.
/// `copies` is clamped to ≥1.
#[cfg(target_os = "windows")]
pub fn print_pdf(printer: &str, pdf_path: &Path, copies: u32) -> Result<()> {
    use tracing::info;

    let metadata = std::fs::metadata(pdf_path)?;
    anyhow::ensure!(metadata.len() > 0, "PDF file is empty");

    let path_str = pdf_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path"))?;

    let effective_copies = copies.max(1);

    info!(
        printer,
        copies = effective_copies,
        path = %pdf_path.display(),
        size = metadata.len(),
        "printing PDF"
    );

    // Try SumatraPDF first — reliable headless printing
    let sumatra = r"C:\Program Files\SumatraPDF\SumatraPDF.exe";
    if std::path::Path::new(sumatra).exists() {
        info!(printer, "using SumatraPDF CLI");
        // SumatraPDF `-print-settings` accepts "Nx" to request N copies.
        // Omit the flag entirely for the default single-copy case to preserve
        // the pre-fix command line unchanged for those jobs.
        let copies_setting = format!("{effective_copies}x");
        let mut args: Vec<&str> = vec!["-print-to", printer, "-silent"];
        if effective_copies > 1 {
            args.push("-print-settings");
            args.push(&copies_setting);
        }
        args.push(path_str);
        let mut child = std::process::Command::new(sumatra).args(&args).spawn()?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match child.try_wait()? {
                Some(status) if status.success() => {
                    info!(printer, "SumatraPDF print completed successfully");
                    return Ok(());
                }
                Some(status) => {
                    anyhow::bail!("SumatraPDF exit code: {}", status);
                }
                None if std::time::Instant::now() > deadline => {
                    child.kill()?;
                    info!(
                        printer,
                        "SumatraPDF timed out after 30s, falling back to PrintTo"
                    );
                    break;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    // Fallback: Start-Process -Verb PrintTo (no built-in copies flag) — loop.
    info!(
        printer,
        copies = effective_copies,
        "SumatraPDF not found, falling back to PrintTo verb (looping for copies)"
    );
    let script = format!(
        "Start-Process -FilePath '{}' -Verb PrintTo -ArgumentList '\"{}\"' -Wait -WindowStyle Hidden",
        path_str.replace('\'', "''"),
        printer.replace('\'', "''"),
    );

    for copy_index in 1..=effective_copies {
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .spawn()?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match child.try_wait()? {
                Some(status) if status.success() => {
                    info!(
                        printer,
                        copy = copy_index,
                        total = effective_copies,
                        "PrintTo copy completed"
                    );
                    break;
                }
                Some(status) => {
                    anyhow::bail!(
                        "PrintTo exit code: {} on copy {}/{}",
                        status,
                        copy_index,
                        effective_copies
                    );
                }
                None if std::time::Instant::now() > deadline => {
                    child.kill()?;
                    anyhow::bail!(
                        "PrintTo timed out after 30s on copy {}/{} — printer may require GUI interaction",
                        copy_index,
                        effective_copies
                    );
                }
                None => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }
    Ok(())
}

/// Send a PDF file to the specified printer using CUPS `lp` (macOS/Linux).
/// `copies` is clamped to ≥1 and passed via `lp -n N`. See #37.
#[cfg(not(target_os = "windows"))]
pub fn print_pdf(printer: &str, pdf_path: &Path, copies: u32) -> Result<()> {
    use tracing::info;

    anyhow::ensure!(
        pdf_path.exists(),
        "PDF file does not exist: {}",
        pdf_path.display()
    );

    let effective_copies = copies.max(1);
    let copies_str = effective_copies.to_string();

    info!(
        printer,
        copies = effective_copies,
        path = %pdf_path.display(),
        "printing PDF via CUPS lp"
    );

    let output = std::process::Command::new("lp")
        .args(["-d", printer, "-n", &copies_str])
        .arg(pdf_path)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "lp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    info!(
        printer,
        stdout = %String::from_utf8_lossy(&output.stdout).trim(),
        "lp job submitted"
    );

    Ok(())
}

/// Result of verifying that the Windows spooler actually processed a print job.
pub struct PrintVerification {
    pub success: bool,
    pub spooler_status: String,
    pub detail: String,
}

/// Check if a printer is ready to accept jobs before sending.
///
/// Returns Ok(()) if printer is in "normal" state, Err with descriptive
/// message if offline, error, paper jam, etc.
#[cfg(target_os = "windows")]
pub fn check_printer_ready(printer_name: &str) -> Result<()> {
    use tracing::debug;

    let script = format!(
        "Get-Printer -Name '{}' | Select-Object PrinterStatus | ConvertTo-Json",
        printer_name.replace('\'', "''"),
    );

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "printer '{}' not found: {}",
            printer_name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        anyhow::bail!("printer '{}' not found", printer_name);
    }

    let raw: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_default();
    let status_code = raw["PrinterStatus"].as_u64().unwrap_or(0);

    let status_name = match status_code {
        0 => "normal",
        1 => return Err(anyhow::anyhow!("printer '{}' is paused", printer_name)),
        2 => return Err(anyhow::anyhow!("printer '{}' has error", printer_name)),
        3 => {
            return Err(anyhow::anyhow!(
                "printer '{}' is pending deletion",
                printer_name
            ));
        }
        4 => return Err(anyhow::anyhow!("printer '{}' has paper jam", printer_name)),
        5 => {
            return Err(anyhow::anyhow!(
                "printer '{}' is out of paper",
                printer_name
            ));
        }
        6 => {
            return Err(anyhow::anyhow!(
                "printer '{}' requires manual feed",
                printer_name
            ));
        }
        7 => {
            return Err(anyhow::anyhow!(
                "printer '{}' has paper problem",
                printer_name
            ));
        }
        8 => return Err(anyhow::anyhow!("printer '{}' is offline", printer_name)),
        _ => "normal",
    };
    debug!(
        printer = printer_name,
        status = status_name,
        "printer ready"
    );
    Ok(())
}

/// Check if a CUPS printer is ready to accept jobs (macOS/Linux).
#[cfg(not(target_os = "windows"))]
pub fn check_printer_ready(printer_name: &str) -> Result<()> {
    let output = std::process::Command::new("lpstat")
        .args(["-p", printer_name])
        .output()
        .map_err(|_| anyhow::anyhow!("printer '{}' not found: lpstat unavailable", printer_name))?;

    if !output.status.success() {
        anyhow::bail!("printer '{}' not found", printer_name);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("disabled") || stdout.contains("not accepting") {
        anyhow::bail!("printer '{}' is not ready: {}", printer_name, stdout.trim());
    }

    Ok(())
}

/// Verify that the Windows print spooler actually processed a job.
///
/// After SumatraPDF/PrintTo sends to the spooler, this function polls
/// Get-PrintJob to confirm the job either leaves the queue (= printed)
/// or enters an error state. Returns a structured verification result.
#[cfg(target_os = "windows")]
pub fn verify_print_completion(printer_name: &str, timeout_secs: u64) -> Result<PrintVerification> {
    use tracing::{debug, warn};

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_secs(2);

    // First check: if queue is already empty, job was delivered to printer immediately
    std::thread::sleep(std::time::Duration::from_secs(1));

    loop {
        let script = format!(
            "Get-PrintJob -PrinterName '{}' -ErrorAction SilentlyContinue | Select-Object Id, JobStatus, DocumentName, Size | ConvertTo-Json",
            printer_name.replace('\'', "''"),
        );

        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();

        // Empty output = no jobs in queue = our job was processed
        if trimmed.is_empty() || trimmed == "null" {
            debug!(
                printer = printer_name,
                "spooler queue empty - job delivered to printer"
            );
            return Ok(PrintVerification {
                success: true,
                spooler_status: "completed".into(),
                detail: "job left print queue".into(),
            });
        }

        // Parse job statuses
        let raw: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_default();
        let items = match raw {
            serde_json::Value::Array(arr) => arr,
            obj @ serde_json::Value::Object(_) => vec![obj],
            _ => vec![],
        };

        // Check for error states in any job
        for item in &items {
            let status = item["JobStatus"].as_str().unwrap_or("");
            if status.contains("Error") || status.contains("Offline") {
                let doc = item["DocumentName"].as_str().unwrap_or("unknown");
                warn!(
                    printer = printer_name,
                    status,
                    document = doc,
                    "spooler job in error state"
                );
                return Ok(PrintVerification {
                    success: false,
                    spooler_status: "error".into(),
                    detail: format!("spooler status: {} (document: {})", status, doc),
                });
            }
        }

        if std::time::Instant::now() > deadline {
            let count = items.len();
            warn!(
                printer = printer_name,
                jobs_in_queue = count,
                "spooler verification timed out"
            );
            return Ok(PrintVerification {
                success: false,
                spooler_status: "timeout".into(),
                detail: format!("{} job(s) still in queue after {}s", count, timeout_secs),
            });
        }

        debug!(
            printer = printer_name,
            jobs = items.len(),
            "waiting for spooler to process..."
        );
        std::thread::sleep(poll_interval);
    }
}

/// Verify that a CUPS print job completed by polling `lpstat -o` (macOS/Linux).
#[cfg(not(target_os = "windows"))]
pub fn verify_print_completion(printer_name: &str, timeout_secs: u64) -> Result<PrintVerification> {
    use tracing::{debug, warn};

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_secs(2);

    // Brief pause to let the job enter the queue
    std::thread::sleep(std::time::Duration::from_secs(1));

    loop {
        let output = std::process::Command::new("lpstat")
            .args(["-o", printer_name])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let jobs: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();

        if jobs.is_empty() {
            debug!(printer = printer_name, "CUPS queue empty - job delivered");
            return Ok(PrintVerification {
                success: true,
                spooler_status: "completed".into(),
                detail: "job left CUPS queue".into(),
            });
        }

        if std::time::Instant::now() > deadline {
            warn!(
                printer = printer_name,
                jobs = jobs.len(),
                "CUPS verification timed out"
            );
            return Ok(PrintVerification {
                success: false,
                spooler_status: "timeout".into(),
                detail: format!(
                    "{} job(s) still in queue after {}s",
                    jobs.len(),
                    timeout_secs
                ),
            });
        }

        debug!(
            printer = printer_name,
            jobs = jobs.len(),
            "waiting for CUPS to process..."
        );
        std::thread::sleep(poll_interval);
    }
}

/// Query the print queue for a printer (for E2E verification).
#[cfg(target_os = "windows")]
pub fn get_print_queue(printer_name: &str) -> Result<Vec<String>> {
    let script = format!(
        "Get-PrintJob -PrinterName '{}' | Select-Object -ExpandProperty DocumentName",
        printer_name.replace('\'', "''"),
    );

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()?;

    // Empty queue is not an error
    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let jobs: Vec<String> = stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(jobs)
}

/// Query the CUPS print queue for a printer (macOS/Linux).
#[cfg(not(target_os = "windows"))]
pub fn get_print_queue(printer_name: &str) -> Result<Vec<String>> {
    let output = match std::process::Command::new("lpstat")
        .args(["-o", printer_name])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Ok(vec![]), // lpstat not available
    };

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let jobs = stdout
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();

    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_printers_returns_ok() {
        let result = list_printers();
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_print_pdf_rejects_nonexistent_file() {
        let result = print_pdf("FakePrinter", Path::new("/nonexistent/file.pdf"), 1);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist"),
            "Expected 'does not exist' error, got: {}",
            err
        );
    }

    #[test]
    fn test_get_print_queue_returns_ok() {
        let result = get_print_queue("nonexistent");
        #[cfg(not(target_os = "windows"))]
        assert!(result.is_ok());
        #[cfg(target_os = "windows")]
        {
            let _ = result;
        }
    }
}
