use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info};

use devbridge_core::job_event::{EventEmitter, PrintStage};

/// Result of a Ghostscript rendering operation.
pub struct RenderResult {
    pub output_path: PathBuf,
    pub pages: u32,
    pub output_size: u64,
    pub duration_ms: u64,
    pub device: String,
}

/// Parse page count from Ghostscript stderr output.
/// Ghostscript emits "Page N" for each rendered page.
pub fn parse_page_count(stderr: &str) -> u32 {
    stderr
        .lines()
        .filter(|line| line.starts_with("Page "))
        .count() as u32
}

/// Find the Ghostscript executable.
/// Looks in bundled location first, then system PATH.
pub fn find_ghostscript() -> Option<PathBuf> {
    // Bundled location from NSIS installer
    let bundled = PathBuf::from(r"C:\Program Files\DevBridge\ghostscript\bin\gswin64c.exe");
    if bundled.exists() {
        return Some(bundled);
    }

    // NSIS installer extracts to a versioned subdirectory
    let gs_dir = PathBuf::from(r"C:\Program Files\DevBridge\ghostscript");
    if gs_dir.exists() {
        // Search recursively for gswin64c.exe (handles versioned subdirs like gs10.04.0/bin/)
        if let Ok(entries) = std::fs::read_dir(&gs_dir) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("bin").join("gswin64c.exe");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    // Try common system-wide locations
    let alt = PathBuf::from(r"C:\Program Files\gs\bin\gswin64c.exe");
    if alt.exists() {
        return Some(alt);
    }

    // macOS/Linux: check Homebrew and standard paths
    #[cfg(not(target_os = "windows"))]
    {
        let unix_paths = [
            PathBuf::from("/opt/homebrew/bin/gs"), // Homebrew Apple Silicon
            PathBuf::from("/usr/local/bin/gs"),    // Homebrew Intel / manual
            PathBuf::from("/usr/bin/gs"),          // System package manager
        ];
        for path in &unix_paths {
            if path.exists() {
                tracing::info!("Found Ghostscript at {}", path.display());
                return Some(path.clone());
            }
        }
    }

    // Fallback: check PATH (works on Linux too for "gs")
    which_ghostscript()
}

/// Search PATH for ghostscript binary.
fn which_ghostscript() -> Option<PathBuf> {
    // Try gswin64c (Windows), then gs (Linux/Mac)
    for name in &["gswin64c", "gswin64c.exe", "gs"] {
        if let Ok(path) = std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
            .arg(name)
            .output()
            && path.status.success()
        {
            let stdout = String::from_utf8_lossy(&path.stdout);
            let first_line = stdout.lines().next().unwrap_or("").trim();
            if !first_line.is_empty() {
                let p = PathBuf::from(first_line);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Render a PDF to raster format using Ghostscript.
///
/// # Arguments
/// * `pdf_path` - Input PDF file
/// * `output_path` - Output raster file
/// * `device` - Ghostscript device name ("pwgraster", "ppmraw", etc.)
/// * `resolution` - DPI (e.g., 600)
/// * `job_id` - For event emission
/// * `events` - Event emitter for audit trail
pub fn render(
    pdf_path: &Path,
    output_path: &Path,
    device: &str,
    resolution: u32,
    job_id: &str,
    events: &EventEmitter,
) -> Result<RenderResult> {
    let gs_path = find_ghostscript()
        .ok_or_else(|| anyhow::anyhow!("Ghostscript not found — install to C:\\Program Files\\DevBridge\\ghostscript\\bin\\gswin64c.exe or add to PATH"))?;

    events.emit_ok(
        job_id,
        PrintStage::Rendering,
        format!("Ghostscript {} {}dpi started", device, resolution),
    );

    let start = Instant::now();

    let output = std::process::Command::new(&gs_path)
        .args([
            "-dNOPAUSE",
            "-dBATCH",
            "-dSAFER",
            &format!("-sDEVICE={}", device),
            &format!("-r{}", resolution),
            &format!("-sOutputFile={}", output_path.display()),
        ])
        .arg(pdf_path)
        .output()?;

    let duration_ms = start.elapsed().as_millis() as u64;
    let stderr_str = String::from_utf8_lossy(&output.stderr);

    debug!(
        exit_code = output.status.code(),
        stderr = %stderr_str,
        "Ghostscript output"
    );

    if !output.status.success() {
        let detail = format!(
            "Ghostscript exit code {}: {}",
            output.status.code().unwrap_or(-1),
            stderr_str.lines().last().unwrap_or("unknown error")
        );
        events.emit_fail(job_id, PrintStage::Failed, &detail);
        anyhow::bail!("{}", detail);
    }

    // Verify output file exists and is non-empty
    let output_size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if output_size == 0 {
        let detail = "Ghostscript produced empty output file";
        events.emit_fail(job_id, PrintStage::Failed, detail);
        anyhow::bail!("{}", detail);
    }

    let pages = parse_page_count(&stderr_str);
    // Some devices (jpeg, png) don't output "Page N" lines — assume 1 page if output exists
    let pages = if pages == 0 && output_size > 0 {
        1
    } else {
        pages
    };
    let duration_secs = duration_ms as f64 / 1000.0;
    let size_mb = output_size as f64 / (1024.0 * 1024.0);

    let detail = format!(
        "{} pages, {:.1}MB, {:.1}s, device={}",
        pages, size_mb, duration_secs, device
    );
    events.emit_ok(job_id, PrintStage::Rendered, &detail);

    info!(
        job_id,
        pages, output_size, duration_ms, device, "Ghostscript rendering complete"
    );

    Ok(RenderResult {
        output_path: output_path.to_path_buf(),
        pages,
        output_size,
        duration_ms,
        device: device.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_page_count_multiple() {
        let stderr = "GPL Ghostscript 10.04.0 (2024-09-18)\n\
                       Copyright (C) 2024 Artifex Software, Inc.\n\
                       Page 1\n\
                       Page 2\n\
                       Page 3\n";
        assert_eq!(parse_page_count(stderr), 3);
    }

    #[test]
    fn test_parse_page_count_single() {
        let stderr = "Page 1\n";
        assert_eq!(parse_page_count(stderr), 1);
    }

    #[test]
    fn test_parse_page_count_empty() {
        assert_eq!(parse_page_count(""), 0);
        assert_eq!(parse_page_count("some error output\n"), 0);
    }

    #[test]
    fn test_find_ghostscript_returns_option() {
        // On CI without GS installed, returns None — that's valid
        let result = find_ghostscript();
        let _ = result; // just verify it doesn't panic
    }
}
