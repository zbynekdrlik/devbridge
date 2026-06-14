use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintStage};

/// Result of a Ghostscript rendering operation.
pub struct RenderResult {
    /// For multi-page devices: the single output file.
    /// For single-page devices (jpeg/png): the first page file (backward compat).
    pub output_path: PathBuf,
    /// All output page files. For multi-page devices, this contains one entry
    /// (same as `output_path`). For single-page devices, one file per page.
    pub page_files: Vec<PathBuf>,
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
/// * `cancel` - Cancellation token; if set (outer per-job timeout fired), the
///   Ghostscript child process is killed and the render bails. This stops a
///   GS run wedged on a malformed PDF from outliving the print task (#51).
pub fn render(
    pdf_path: &Path,
    output_path: &Path,
    device: &str,
    resolution: u32,
    job_id: &str,
    events: &EventEmitter,
    cancel: &CancellationToken,
) -> Result<RenderResult> {
    let gs_path = find_ghostscript()
        .ok_or_else(|| anyhow::anyhow!("Ghostscript not found — install to C:\\Program Files\\DevBridge\\ghostscript\\bin\\gswin64c.exe or add to PATH"))?;

    events.emit_ok(
        job_id,
        PrintStage::Rendering,
        format!("Ghostscript {} {}dpi started", device, resolution),
    );

    let start = Instant::now();

    // JPEG/PNG devices produce a single image per page. Use %d in the
    // output filename so Ghostscript writes page-001.jpg, page-002.jpg, etc.
    // Multi-page devices (pwgraster, pdfwrite, tiff) keep a single file.
    let is_single_page_device = matches!(
        device,
        "jpeg" | "jpeggray" | "jpegcmyk" | "png16m" | "pnggray" | "pngmono" | "pngalpha"
    );
    let output_file_arg = if is_single_page_device {
        // Insert %03d before the extension so pages are numbered
        let stem = output_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let parent = output_path.parent().unwrap_or(Path::new("."));
        let ext = match device {
            "jpeg" | "jpeggray" | "jpegcmyk" => "jpg",
            _ => "png",
        };
        format!(
            "{}",
            parent.join(format!("{}-%03d.{}", stem, ext)).display()
        )
    } else {
        format!("{}", output_path.display())
    };

    // Spawn Ghostscript as a child we can KILL on cancellation, rather than a
    // blocking `.output()` that would keep running until GS returns on its own
    // (issue #51 — a GS run wedged on a malformed PDF must not outlive the
    // print task and race a requeued retry). We poll `try_wait` while watching
    // the cancel token.
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(&gs_path)
        .args([
            "-dNOPAUSE",
            "-dBATCH",
            "-dSAFER",
            &format!("-sDEVICE={}", device),
            &format!("-r{}", resolution),
            &format!("-sOutputFile={}", output_file_arg),
        ])
        .arg(pdf_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    debug!(
        job_id,
        device,
        pid = child.id(),
        "Ghostscript child spawned (cancellable)"
    );

    // Drain stdout/stderr in background threads. This is REQUIRED for
    // correctness: a multi-hundred-page job writes more "Page N" lines to
    // stderr than the OS pipe buffer (~64 KB) holds, and if we don't drain it
    // GS blocks on write, never exits, and `try_wait` spins forever — turning
    // every large job into a timeout. `.output()` drains concurrently for free;
    // since we replaced it with a poll loop we must drain ourselves.
    let stdout_drainer = child.stdout.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_drainer = child.stderr.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    });

    // Poll for completion, checking the cancellation token each tick.
    let status = loop {
        if cancel.is_cancelled() {
            // Outer timeout fired — kill the GS child so it stops rendering.
            warn!(
                job_id,
                device,
                pid = child.id(),
                "cancellation observed during Ghostscript render — killing child process (issue #51)"
            );
            match child.kill() {
                Ok(()) => {
                    let _ = child.wait();
                    info!(job_id, "Ghostscript child killed after cancellation");
                }
                Err(e) => warn!(job_id, error = %e, "failed to kill Ghostscript child on cancel"),
            }
            // Join the drainer threads so their pipe handles close cleanly.
            if let Some(h) = stdout_drainer {
                let _ = h.join();
            }
            if let Some(h) = stderr_drainer {
                let _ = h.join();
            }
            let detail = "Ghostscript render cancelled by outer print timeout";
            events.emit_fail(job_id, PrintStage::Failed, detail);
            anyhow::bail!("{}", detail);
        }
        match child.try_wait()? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    // Child exited on its own. Collect the drained stderr ("Page N" progress
    // and error detail); stdout is drained too so GS never blocked on write.
    let stderr_buf = stderr_drainer
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if let Some(h) = stdout_drainer {
        let _ = h.join();
    }
    let duration_ms = start.elapsed().as_millis() as u64;
    let stderr_str = String::from_utf8_lossy(&stderr_buf);

    debug!(
        exit_code = status.code(),
        stderr = %stderr_str,
        "Ghostscript output"
    );

    if !status.success() {
        let detail = format!(
            "Ghostscript exit code {}: {}",
            status.code().unwrap_or(-1),
            stderr_str.lines().last().unwrap_or("unknown error")
        );
        events.emit_fail(job_id, PrintStage::Failed, &detail);
        anyhow::bail!("{}", detail);
    }

    // Collect output files. For single-page devices, Ghostscript writes
    // stem-001.ext, stem-002.ext, etc. For multi-page devices, one file.
    let (page_files, output_size) = if is_single_page_device {
        let stem = output_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let parent = output_path.parent().unwrap_or(Path::new("."));
        let ext = match device {
            "jpeg" | "jpeggray" | "jpegcmyk" => "jpg",
            _ => "png",
        };
        let mut files = Vec::new();
        let mut total_size = 0u64;
        for page_num in 1u32.. {
            let page_path = parent.join(format!("{}-{:03}.{}", stem, page_num, ext));
            if page_path.exists() {
                total_size += std::fs::metadata(&page_path).map(|m| m.len()).unwrap_or(0);
                files.push(page_path);
            } else {
                break;
            }
        }
        if files.is_empty() {
            let detail = "Ghostscript produced no output pages";
            events.emit_fail(job_id, PrintStage::Failed, detail);
            anyhow::bail!("{}", detail);
        }
        (files, total_size)
    } else {
        let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let detail = "Ghostscript produced empty output file";
            events.emit_fail(job_id, PrintStage::Failed, detail);
            anyhow::bail!("{}", detail);
        }
        (vec![output_path.to_path_buf()], size)
    };

    let pages = parse_page_count(&stderr_str);
    let pages = if pages == 0 {
        page_files.len() as u32
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

    let first_file = page_files[0].clone();
    Ok(RenderResult {
        output_path: first_file,
        page_files,
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
