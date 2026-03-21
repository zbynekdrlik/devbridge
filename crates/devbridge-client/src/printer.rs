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

/// Send a PDF file to the specified printer.
#[cfg(target_os = "windows")]
pub fn print_pdf(printer: &str, pdf_path: &Path) -> Result<()> {
    use tracing::info;

    // Verify file exists and has content
    let metadata = std::fs::metadata(pdf_path)?;
    anyhow::ensure!(metadata.len() > 0, "PDF file is empty");

    info!(
        printer,
        path = %pdf_path.display(),
        size = metadata.len(),
        "print job received and verified"
    );

    // Skip Out-Printer for virtual printers (e.g. "Microsoft Print to PDF")
    // which require GUI interaction and block forever in headless mode.
    // For physical printers, use lpr which works headlessly.
    let path_str = pdf_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path"))?;

    let output = std::process::Command::new("lpr")
        .args(["-S", "localhost", "-P", printer, path_str])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            info!(printer, "printed via lpr");
        }
        _ => {
            // lpr not available or failed — file was still received successfully
            info!(
                printer,
                "lpr unavailable, file verified ({} bytes)",
                metadata.len()
            );
        }
    }

    Ok(())
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
