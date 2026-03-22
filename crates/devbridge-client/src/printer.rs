use std::path::Path;

use anyhow::Result;

/// List available printers on this system.
#[cfg(target_os = "windows")]
pub fn list_printers() -> Result<Vec<String>> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Printer | Select-Object -ExpandProperty Name",
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Get-Printer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let printers: Vec<String> = stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(printers)
}

/// List available printers on this system (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn list_printers() -> Result<Vec<String>> {
    Ok(vec![])
}

/// Send a PDF file to the specified printer using SumatraPDF CLI.
///
/// Uses SumatraPDF's `-print-to` flag for reliable headless printing.
/// Falls back to `Start-Process -Verb PrintTo` if SumatraPDF is not installed.
#[cfg(target_os = "windows")]
pub fn print_pdf(printer: &str, pdf_path: &Path) -> Result<()> {
    use tracing::info;

    let metadata = std::fs::metadata(pdf_path)?;
    anyhow::ensure!(metadata.len() > 0, "PDF file is empty");

    let path_str = pdf_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path"))?;

    info!(
        printer,
        path = %pdf_path.display(),
        size = metadata.len(),
        "printing PDF"
    );

    // Try SumatraPDF first — reliable headless printing
    let sumatra = r"C:\Program Files\SumatraPDF\SumatraPDF.exe";
    if std::path::Path::new(sumatra).exists() {
        info!(printer, "using SumatraPDF CLI");
        let status = std::process::Command::new(sumatra)
            .args(["-print-to", printer, "-silent", path_str])
            .status()?;
        if status.success() {
            info!(printer, "SumatraPDF print completed successfully");
            return Ok(());
        }
        anyhow::bail!("SumatraPDF exit code: {}", status);
    }

    // Fallback: Start-Process -Verb PrintTo
    info!(
        printer,
        "SumatraPDF not found, falling back to PrintTo verb"
    );
    let script = format!(
        "Start-Process -FilePath '{}' -Verb PrintTo -ArgumentList '\"{}\"' -Wait -WindowStyle Hidden",
        path_str.replace('\'', "''"),
        printer.replace('\'', "''"),
    );

    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .spawn()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => {
                info!(printer, "PrintTo completed successfully");
                return Ok(());
            }
            Some(status) => {
                anyhow::bail!("PrintTo exit code: {}", status);
            }
            None if std::time::Instant::now() > deadline => {
                child.kill()?;
                anyhow::bail!("PrintTo timed out after 30s — printer may require GUI interaction");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

/// Send a PDF file to the specified printer (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn print_pdf(_printer: &str, _pdf_path: &Path) -> Result<()> {
    anyhow::bail!("printing is only supported on Windows")
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

/// Query print queue (non-Windows stub).
#[cfg(not(target_os = "windows"))]
pub fn get_print_queue(_printer_name: &str) -> Result<Vec<String>> {
    Ok(vec![])
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
    fn test_print_pdf_errors_on_non_windows() {
        let result = print_pdf("test", Path::new("/tmp/test.pdf"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only supported on Windows")
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
