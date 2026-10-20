use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use devbridge_core::job_event::{EventEmitter, PrintStage};

pub use crate::gs_plan::parse_page_count;
use crate::gs_plan::{self, GsCalls, Orientation, is_single_page_device, page_file_ext};
use crate::pdf_pages::{self, RenderRun};
use crate::raster_merge::{self, RasterKind};

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

/// One Ghostscript job context: the executable and what every call of the
/// job logs, reports and obeys.
struct GsCall<'a> {
    gs_path: &'a Path,
    job_id: &'a str,
    device: &'a str,
    events: &'a EventEmitter,
    cancel: &'a CancellationToken,
}

/// Render a PDF to raster format using Ghostscript.
///
/// For a mergeable raster device (`urfgray` / `urfrgb` / `urfcmyk` /
/// `pwgraster`, see [`RasterKind`]) every page is put on portrait-fed media
/// at scale 1 (#111): the page geometry comes from `gs -dPDFINFO`, landscape
/// pages are rendered onto their own size swapped (Ghostscript rotates them
/// 90°), consecutive pages of one kind share a Ghostscript run, and the runs
/// are merged into ONE raster document (one IPP Print-Job). A document whose
/// pages are all portrait is rendered by exactly the pre-#111 call
/// (byte-identical). Every other device keeps that single plain call. The
/// decisions live in [`crate::gs_plan`]; this function runs the processes.
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
    let call = GsCall {
        gs_path: &gs_path,
        job_id,
        device,
        events,
        cancel,
    };

    events.emit_ok(
        job_id,
        PrintStage::Rendering,
        format!("Ghostscript {} {}dpi started", device, resolution),
    );

    let start = Instant::now();

    // JPEG/PNG devices produce a single image per page. Use %d in the
    // output filename so Ghostscript writes page-001.jpg, page-002.jpg, etc.
    // Multi-page devices (pwgraster, pdfwrite, tiff) keep a single file.
    let single_page_device = is_single_page_device(device);
    let output_file_arg = if single_page_device {
        // Insert %03d before the extension so pages are numbered
        let stem = output_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let parent = output_path.parent().unwrap_or(Path::new("."));
        format!(
            "{}",
            parent
                .join(format!("{}-%03d.{}", stem, page_file_ext(device)))
                .display()
        )
    } else {
        format!("{}", output_path.display())
    };

    // #111: page orientation, only for the mergeable raster devices.
    let orientation = match RasterKind::for_device(device) {
        Some(kind) => read_orientation(&call, pdf_path, kind)?,
        None => Orientation::NotApplicable,
    };

    let base_args = [
        "-dNOPAUSE".to_string(),
        "-dBATCH".to_string(),
        "-dSAFER".to_string(),
        format!("-sDEVICE={}", device),
        format!("-r{}", resolution),
    ];
    let page_lines = match orientation.calls() {
        GsCalls::Runs(kind, runs) => {
            render_runs(&call, &base_args, pdf_path, output_path, kind, runs)?
        }
        GsCalls::Single(extra_args) => {
            // ONE Ghostscript call. Without extra arguments this is exactly
            // the pre-#111 call: same arguments, byte-identical output.
            let mut args = base_args.to_vec();
            args.extend(extra_args);
            args.push(format!("-sOutputFile={}", output_file_arg));
            let out = run_gs(&call, &args, pdf_path)?;
            fail_unless_success(&call, &out)?;
            out.page_lines()
        }
    };

    // Collect output files. For single-page devices, Ghostscript writes
    // stem-001.ext, stem-002.ext, etc. For multi-page devices, one file.
    let (page_files, output_size) = if single_page_device {
        let stem = output_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let parent = output_path.parent().unwrap_or(Path::new("."));
        let ext = page_file_ext(device);
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

    let pages = orientation.page_count(page_lines, page_files.len());
    let duration_ms = start.elapsed().as_millis() as u64;
    let duration_secs = duration_ms as f64 / 1000.0;
    let size_mb = output_size as f64 / (1024.0 * 1024.0);

    let detail = format!(
        "{} pages, {:.1}MB, {:.1}s, device={}{}",
        pages,
        size_mb,
        duration_secs,
        device,
        orientation.event_suffix()
    );
    events.emit_ok(job_id, PrintStage::Rendered, &detail);

    info!(
        job_id,
        pages,
        output_size,
        duration_ms,
        device,
        orientation = %orientation.event_suffix(),
        "Ghostscript rendering complete"
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

/// Read the page geometry with `gs -dPDFINFO` and plan the runs. A failure
/// to read it is NOT a print failure: the document is rendered as before
/// #111 (`Orientation::Unknown`). Only a cancellation propagates.
fn read_orientation(call: &GsCall<'_>, pdf_path: &Path, kind: RasterKind) -> Result<Orientation> {
    let args: Vec<String> = [
        "-dNODISPLAY",
        "-dNOPAUSE",
        "-dBATCH",
        "-dSAFER",
        "-q",
        "-dPDFINFO",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let started = Instant::now();
    let orientation = match run_gs(call, &args, pdf_path) {
        Ok(out) => Orientation::from_pdfinfo(
            kind,
            out.status.success(),
            out.status.code(),
            &out.stdout,
            &out.stderr,
        ),
        Err(e) if call.cancel.is_cancelled() => return Err(e),
        Err(e) => Orientation::Unknown(format!("gs -dPDFINFO: {e:#}")),
    };
    let pdfinfo_ms = started.elapsed().as_millis() as u64;
    match &orientation {
        Orientation::Planned { runs, summary, .. } => info!(
            job_id = call.job_id,
            device = call.device,
            portrait = summary.portrait,
            landscape = summary.landscape,
            runs = runs.len(),
            pdfinfo_ms,
            "page orientation read (landscape pages are rotated 90° onto portrait media, #111)"
        ),
        Orientation::Unknown(reason) => warn!(
            job_id = call.job_id,
            device = call.device,
            reason = %reason,
            pdfinfo_ms,
            "page orientation unknown — rendering the document as-is (pre-#111 behaviour)"
        ),
        Orientation::NotApplicable => {}
    }
    Ok(orientation)
}

/// Temp files of the individual runs, deleted however the render ends.
struct RunFiles(Vec<PathBuf>);

impl Drop for RunFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Render each run into its own file, then merge them into `output_path` as
/// ONE `kind` document. Returns the "Page N" lines Ghostscript printed.
fn render_runs(
    call: &GsCall<'_>,
    base_args: &[String],
    pdf_path: &Path,
    output_path: &Path,
    kind: RasterKind,
    runs: &[RenderRun],
) -> Result<u32> {
    let files = RunFiles(
        (1..=runs.len())
            .map(|k| output_path.with_extension(format!("run{k}")))
            .collect(),
    );
    let mut page_lines = 0;
    for (run, file) in runs.iter().zip(&files.0) {
        let mut args = base_args.to_vec();
        args.extend(pdf_pages::run_args(run, false));
        args.push(format!("-sOutputFile={}", file.display()));
        debug!(
            job_id = call.job_id,
            first_page = run.first_page,
            last_page = run.last_page,
            media = ?run.media,
            "Ghostscript run"
        );
        let out = run_gs(call, &args, pdf_path)?;
        fail_unless_success(call, &out)?;
        page_lines += out.page_lines();
    }
    let bytes = match merge_files(kind, &files.0, output_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            let detail = format!("merging the {} Ghostscript runs failed: {e:#}", runs.len());
            call.events
                .emit_fail(call.job_id, PrintStage::Failed, &detail);
            anyhow::bail!("{}", detail);
        }
    };
    info!(
        job_id = call.job_id,
        runs = runs.len(),
        bytes,
        "Ghostscript runs merged into ONE raster document (one IPP Print-Job)"
    );
    Ok(page_lines)
}

fn merge_files(kind: RasterKind, files: &[PathBuf], output_path: &Path) -> Result<u64> {
    let mut inputs = files
        .iter()
        .map(|f| {
            std::fs::File::open(f)
                .map(std::io::BufReader::new)
                .with_context(|| format!("open {}", f.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(output_path)
            .with_context(|| format!("create {}", output_path.display()))?,
    );
    raster_merge::merge(kind, &mut inputs, &mut out)
}

/// What one Ghostscript child process printed.
struct GsOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl GsOutput {
    fn page_lines(&self) -> u32 {
        gs_plan::page_lines(&self.stdout, &self.stderr)
    }

    fn last_message(&self) -> &str {
        gs_plan::last_message(&self.stderr, &self.stdout)
    }
}

/// Emit `Failed` and bail when Ghostscript exited with an error.
fn fail_unless_success(call: &GsCall<'_>, out: &GsOutput) -> Result<()> {
    if out.status.success() {
        return Ok(());
    }
    let detail = format!(
        "Ghostscript exit code {}: {}",
        out.status.code().unwrap_or(-1),
        out.last_message()
    );
    call.events
        .emit_fail(call.job_id, PrintStage::Failed, &detail);
    anyhow::bail!("{}", detail);
}

/// Run Ghostscript with `args` + `input`.
///
/// Spawns Ghostscript as a child we can KILL on cancellation, rather than a
/// blocking `.output()` that would keep running until GS returns on its own
/// (issue #51 — a GS run wedged on a malformed PDF must not outlive the
/// print task and race a requeued retry). We poll `try_wait` while watching
/// the cancel token.
fn run_gs(call: &GsCall<'_>, args: &[String], input: &Path) -> Result<GsOutput> {
    use std::io::Read;
    use std::process::Stdio;

    let (job_id, device) = (call.job_id, call.device);
    let mut child = std::process::Command::new(call.gs_path)
        .args(args)
        .arg(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    debug!(
        job_id,
        device,
        pid = child.id(),
        args = ?args,
        "Ghostscript child spawned (cancellable)"
    );

    // Drain stdout/stderr in background threads. This is REQUIRED for
    // correctness: a multi-hundred-page job writes more "Page N" lines than
    // the OS pipe buffer (~64 KB) holds, and if we don't drain it GS blocks on
    // write, never exits, and `try_wait` spins forever — turning every large
    // job into a timeout. `.output()` drains concurrently for free; since we
    // replaced it with a poll loop we must drain ourselves.
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
        if call.cancel.is_cancelled() {
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
            call.events.emit_fail(job_id, PrintStage::Failed, detail);
            anyhow::bail!("{}", detail);
        }
        match child.try_wait()? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    // Child exited on its own. Collect what it printed: "Page N" progress on
    // stdout, error detail on stderr.
    let collect = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        let buf = h.and_then(|h| h.join().ok()).unwrap_or_default();
        String::from_utf8_lossy(&buf).into_owned()
    };
    let stdout = collect(stdout_drainer);
    let stderr = collect(stderr_drainer);

    debug!(
        job_id,
        exit_code = status.code(),
        stdout = %stdout,
        stderr = %stderr,
        "Ghostscript output"
    );

    Ok(GsOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_page_count_is_reexported() {
        assert_eq!(parse_page_count("Page 1\nPage 2\n"), 2);
    }

    #[test]
    fn test_find_ghostscript_returns_option() {
        // On CI without GS installed, returns None — that's valid
        let result = find_ghostscript();
        let _ = result; // just verify it doesn't panic
    }
}
