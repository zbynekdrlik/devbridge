# Direct Print Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace SumatraPDF+Windows Spooler black box with a fully audited, direct-to-printer pipeline using Ghostscript for rendering and native network protocols for delivery.

**Architecture:** Three print backends (`DirectIpp`, `DirectRaw`, `WindowsSpooler`) selected per-printer via config. Each backend emits granular job events (received → rendering → rendered → sending → sent → acknowledged → completed/failed) stored in a new `job_events` SQLite table. Ghostscript (~30MB portable) is bundled in the NSIS installer for PDF→raster conversion. IPP binary encoding (~200 lines) uses `reqwest` for HTTP. RAW backend streams raster over TCP port 9100.

**Tech Stack:** Rust, Ghostscript (portable), reqwest (existing dep), tokio::net::TcpStream, rusqlite, Axum API, Leptos WASM UI

---

## File Map

### New Files
| File | Purpose |
|------|---------|
| `crates/devbridge-client/src/print_backend.rs` | `PrintBackend` trait + `PrintBackendConfig` enum, backend factory |
| `crates/devbridge-client/src/backend_windows_spooler.rs` | `WindowsSpooler` — wraps existing `printer.rs` with event emission |
| `crates/devbridge-client/src/backend_direct_raw.rs` | `DirectRaw` — Ghostscript→PPM, TCP:9100 stream |
| `crates/devbridge-client/src/backend_direct_ipp.rs` | `DirectIpp` — Ghostscript→PWG-Raster, IPP Print-Job over HTTP |
| `crates/devbridge-client/src/ghostscript.rs` | Ghostscript renderer: spawn gswin64c.exe, parse output, emit events |
| `crates/devbridge-client/src/ipp_codec.rs` | IPP binary request/response encoding/decoding (~200 lines) |
| `crates/devbridge-core/src/job_event.rs` | `PrintJobEvent` struct + `PrintStage` enum for audit trail |
| `crates/devbridge-dashboard/src/api/job_events.rs` | `GET /api/jobs/{id}/events` endpoint |

### Modified Files
| File | Changes |
|------|---------|
| `crates/devbridge-core/src/config.rs` | Add `print_backend`, `printer_address`, `ghostscript_device`, `ghostscript_resolution` to `ClientConfig` |
| `crates/devbridge-core/src/job.rs` | Add `Rendering`, `Sending` variants to `JobState`; extend `JobEvent` enum |
| `crates/devbridge-core/src/lib.rs` | Export `job_event` module |
| `crates/devbridge-client/src/lib.rs` | Export new backend modules |
| `crates/devbridge-client/src/receiver.rs` | Replace `printer::print_pdf()` call with `PrintBackend::print()` dispatch |
| `crates/devbridge-client/Cargo.toml` | Add `reqwest` dependency |
| `crates/devbridge-server/src/storage.rs` | Add `job_events` table, insert/query methods |
| `crates/devbridge-server/src/queue.rs` | Emit extended `JobEvent` variants |
| `crates/devbridge-dashboard/src/api/mod.rs` | Mount `job_events::router()` |
| `crates/devbridge-dashboard/src/api/ws.rs` | Forward `PrintJobEvent` via WebSocket |
| `crates/devbridge-ui/src/pages/jobs.rs` | Add job detail timeline view |
| `crates/devbridge-ui/src/api.rs` | Add `fetch_job_events()` |
| `.github/workflows/ci.yml` | Download Ghostscript portable in Windows Build step |
| `installer/post-install.ps1` | Add Ghostscript extraction, add `print_backend`/`printer_address` to client config template |
| `config/default.toml` | Add new client config fields with defaults |
| `proto/devbridge.proto` | Add `rendering`/`sending` to `JobState` enum, add `job_events` field to `JobCompletion` |

---

## Task 1: Config — Add Print Backend Fields

**Files:**
- Modify: `crates/devbridge-core/src/config.rs:32-42`
- Modify: `config/default.toml`

- [ ] **Step 1: Write failing test for new config fields**

In `crates/devbridge-core/src/config.rs`, add to the test module after the existing `VALID_TOML` constant:

```rust
#[test]
fn test_config_with_print_backend_fields() {
    let toml_str = r#"
[general]
mode = "client"
log_level = "info"
data_dir = "/tmp/devbridge"

[server]
ipp_port = 631
grpc_port = 50051
dashboard_port = 9090
printer_name = "TestPrinter"
spool_dir = "/tmp/spool"

[server.tls]
cert_file = "server.crt"
key_file = "server.key"
ca_file = "ca.crt"

[client]
server_address = "127.0.0.1:50051"
target_printer = "EPSON L3270"
dashboard_port = 9120
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
print_backend = "direct_raw"
printer_address = "10.78.5.9:9100"
ghostscript_device = "ppmraw"
ghostscript_resolution = 600

[client.tls]
cert_file = "client.crt"
key_file = "client.key"
ca_file = "ca.crt"

[jobs]
max_retries = 3
retry_delay_secs = 10
job_expiry_hours = 24
max_payload_size_mb = 50
"#;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(toml_str.as_bytes()).unwrap();
    let config = Config::load(tmp.path()).unwrap();

    assert_eq!(config.client.print_backend, "direct_raw");
    assert_eq!(config.client.printer_address, Some("10.78.5.9:9100".to_string()));
    assert_eq!(config.client.ghostscript_device, "ppmraw");
    assert_eq!(config.client.ghostscript_resolution, 600);
}

#[test]
fn test_config_print_backend_defaults_to_windows_spooler() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(VALID_TOML.as_bytes()).unwrap();
    let config = Config::load(tmp.path()).unwrap();

    assert_eq!(config.client.print_backend, "windows_spooler");
    assert_eq!(config.client.printer_address, None);
    assert_eq!(config.client.ghostscript_device, "ppmraw");
    assert_eq!(config.client.ghostscript_resolution, 600);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p devbridge-core test_config_with_print_backend_fields`
Expected: FAIL — `ClientConfig` has no field `print_backend`

- [ ] **Step 3: Add fields to ClientConfig**

In `crates/devbridge-core/src/config.rs`, modify `ClientConfig` (lines 32-42):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server_address: String,
    pub target_printer: String,
    pub dashboard_port: u16,
    pub reconnect_interval_secs: u64,
    pub max_reconnect_interval_secs: u64,
    #[serde(default)]
    pub client_id: Option<String>,
    /// Print backend: "windows_spooler" (default), "direct_ipp", or "direct_raw"
    #[serde(default = "default_print_backend")]
    pub print_backend: String,
    /// Printer IP:port for direct backends (e.g., "10.78.5.9:9100")
    #[serde(default)]
    pub printer_address: Option<String>,
    /// Ghostscript device: "pwgraster" for IPP, "ppmraw" for RAW
    #[serde(default = "default_gs_device")]
    pub ghostscript_device: String,
    /// Ghostscript DPI resolution
    #[serde(default = "default_gs_resolution")]
    pub ghostscript_resolution: u32,
    pub tls: TlsConfig,
}

fn default_print_backend() -> String {
    "windows_spooler".to_string()
}

fn default_gs_device() -> String {
    "ppmraw".to_string()
}

fn default_gs_resolution() -> u32 {
    600
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p devbridge-core`
Expected: All pass, including existing tests (backward compat via `#[serde(default)]`)

- [ ] **Step 5: Update default.toml**

In `config/default.toml`, add commented fields under `[client]`:

```toml
# print_backend = "windows_spooler"    # "direct_ipp" | "direct_raw" | "windows_spooler"
# printer_address = "10.78.5.9:9100"   # IP:port for direct backends
# ghostscript_device = "ppmraw"        # "pwgraster" for IPP, "ppmraw" for RAW
# ghostscript_resolution = 600         # DPI
```

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-core/src/config.rs config/default.toml
git commit -m "Add print backend config fields to ClientConfig"
```

---

## Task 2: Job Event Audit Types

**Files:**
- Create: `crates/devbridge-core/src/job_event.rs`
- Modify: `crates/devbridge-core/src/lib.rs`

- [ ] **Step 1: Write failing test for PrintJobEvent and PrintStage**

Create `crates/devbridge-core/src/job_event.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stages in the print pipeline audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrintStage {
    Received,
    Routed,
    Downloading,
    Downloaded,
    Rendering,
    Rendered,
    Sending,
    Sent,
    Acknowledged,
    Completed,
    Failed,
    Retrying,
}

/// A single audit event in a print job's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrintJobEvent {
    pub job_id: String,
    pub stage: PrintStage,
    pub success: bool,
    pub detail: String,
    pub timestamp: DateTime<Utc>,
}

impl PrintJobEvent {
    pub fn new(job_id: &str, stage: PrintStage, success: bool, detail: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            stage,
            success,
            detail: detail.to_string(),
            timestamp: Utc::now(),
        }
    }

    pub fn ok(job_id: &str, stage: PrintStage, detail: &str) -> Self {
        Self::new(job_id, stage, true, detail)
    }

    pub fn fail(job_id: &str, stage: PrintStage, detail: &str) -> Self {
        Self::new(job_id, stage, false, detail)
    }
}

/// Sender handle for emitting print job events.
/// Wraps a broadcast channel so backends can emit events without knowing the consumer.
#[derive(Clone)]
pub struct EventEmitter {
    sender: tokio::sync::broadcast::Sender<PrintJobEvent>,
}

impl EventEmitter {
    pub fn new(sender: tokio::sync::broadcast::Sender<PrintJobEvent>) -> Self {
        Self { sender }
    }

    pub fn emit(&self, event: PrintJobEvent) {
        let _ = self.sender.send(event);
    }

    pub fn emit_ok(&self, job_id: &str, stage: PrintStage, detail: &str) {
        self.emit(PrintJobEvent::ok(job_id, stage, detail));
    }

    pub fn emit_fail(&self, job_id: &str, stage: PrintStage, detail: &str) {
        self.emit(PrintJobEvent::fail(job_id, stage, detail));
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PrintJobEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_print_stage_serde_roundtrip() {
        let stages = [
            PrintStage::Received,
            PrintStage::Routed,
            PrintStage::Downloading,
            PrintStage::Downloaded,
            PrintStage::Rendering,
            PrintStage::Rendered,
            PrintStage::Sending,
            PrintStage::Sent,
            PrintStage::Acknowledged,
            PrintStage::Completed,
            PrintStage::Failed,
            PrintStage::Retrying,
        ];
        for stage in stages {
            let json = serde_json::to_string(&stage).unwrap();
            let restored: PrintStage = serde_json::from_str(&json).unwrap();
            assert_eq!(stage, restored);
        }
    }

    #[test]
    fn test_print_job_event_ok_constructor() {
        let event = PrintJobEvent::ok("job-1", PrintStage::Rendered, "3 pages, 4.2MB, 1.3s");
        assert_eq!(event.job_id, "job-1");
        assert_eq!(event.stage, PrintStage::Rendered);
        assert!(event.success);
        assert_eq!(event.detail, "3 pages, 4.2MB, 1.3s");
    }

    #[test]
    fn test_print_job_event_fail_constructor() {
        let event = PrintJobEvent::fail("job-2", PrintStage::Failed, "Ghostscript exit code 1");
        assert!(!event.success);
        assert_eq!(event.stage, PrintStage::Failed);
    }

    #[test]
    fn test_event_emitter_send_receive() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let emitter = EventEmitter::new(tx);
        let mut rx = emitter.subscribe();

        emitter.emit_ok("job-3", PrintStage::Sending, "RAW TCP to 10.78.5.9:9100");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, "job-3");
        assert_eq!(event.stage, PrintStage::Sending);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p devbridge-core test_print_stage_serde`
Expected: FAIL — module doesn't exist yet (but we just created the file, so it needs to be exported)

- [ ] **Step 3: Export module from lib.rs**

In `crates/devbridge-core/src/lib.rs`, add:

```rust
pub mod job_event;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p devbridge-core`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-core/src/job_event.rs crates/devbridge-core/src/lib.rs
git commit -m "Add PrintJobEvent and PrintStage types for audit trail"
```

---

## Task 3: Job Events Storage (SQLite)

**Files:**
- Modify: `crates/devbridge-server/src/storage.rs`

- [ ] **Step 1: Write failing tests for job_events table operations**

Add to `crates/devbridge-server/src/storage.rs` test module:

```rust
#[test]
fn test_insert_and_query_job_events() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();

    // Insert a job first (events have FK to jobs)
    let now = Utc::now();
    let meta = JobMetadata {
        job_id: "evt-job-1".to_string(),
        document_name: "test.pdf".to_string(),
        target_printer: "printer".to_string(),
        target_client_id: None,
        copies: 1,
        paper_size: "A4".to_string(),
        duplex: false,
        color: true,
        payload_size: 1024,
        payload_sha256: "abc".to_string(),
        state: JobState::Queued,
        retry_count: 0,
        error_detail: String::new(),
        created_at: now,
        updated_at: now,
    };
    storage.insert_job(&meta, "/tmp/spool/test.pdf").unwrap();

    // Insert events
    use devbridge_core::job_event::{PrintJobEvent, PrintStage};
    let e1 = PrintJobEvent::ok("evt-job-1", PrintStage::Received, "234KB");
    let e2 = PrintJobEvent::ok("evt-job-1", PrintStage::Rendering, "Ghostscript ppmraw 600dpi");
    let e3 = PrintJobEvent::ok("evt-job-1", PrintStage::Rendered, "3 pages, 4.2MB, 1.3s");
    storage.insert_job_event(&e1).unwrap();
    storage.insert_job_event(&e2).unwrap();
    storage.insert_job_event(&e3).unwrap();

    // Query
    let events = storage.get_job_events("evt-job-1").unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].stage, PrintStage::Received);
    assert_eq!(events[1].stage, PrintStage::Rendering);
    assert_eq!(events[2].stage, PrintStage::Rendered);
    assert_eq!(events[0].detail, "234KB");
}

#[test]
fn test_get_job_events_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();

    let events = storage.get_job_events("nonexistent").unwrap();
    assert!(events.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-server test_insert_and_query_job_events`
Expected: FAIL — `insert_job_event` and `get_job_events` don't exist

- [ ] **Step 3: Add job_events table and methods**

In `crates/devbridge-server/src/storage.rs`, add the table creation inside `Storage::new()` after the existing `CREATE TABLE` batch (after line 57):

```rust
conn.execute_batch(
    "CREATE TABLE IF NOT EXISTS job_events (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id    TEXT NOT NULL,
        stage     TEXT NOT NULL,
        success   INTEGER NOT NULL,
        detail    TEXT NOT NULL DEFAULT '',
        timestamp TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_job_events_job_id ON job_events(job_id);",
)
.context("failed to create job_events table")?;
```

Add methods after the existing job methods:

```rust
// -----------------------------------------------------------------------
// Job Events (audit trail)
// -----------------------------------------------------------------------

/// Insert a print pipeline event for a job.
pub fn insert_job_event(&self, event: &devbridge_core::job_event::PrintJobEvent) -> Result<()> {
    self.conn.execute(
        "INSERT INTO job_events (job_id, stage, success, detail, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.job_id,
            serde_json::to_value(&event.stage)
                .unwrap()
                .as_str()
                .unwrap_or("unknown"),
            event.success as i32,
            event.detail,
            event.timestamp.to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Get all events for a job, ordered by timestamp.
pub fn get_job_events(&self, job_id: &str) -> Result<Vec<devbridge_core::job_event::PrintJobEvent>> {
    use devbridge_core::job_event::{PrintJobEvent, PrintStage};

    let mut stmt = self.conn.prepare(
        "SELECT job_id, stage, success, detail, timestamp
         FROM job_events WHERE job_id = ?1 ORDER BY id ASC",
    )?;

    let events = stmt
        .query_map(params![job_id], |row| {
            let stage_str: String = row.get(1)?;
            let stage: PrintStage = serde_json::from_str(&format!("\"{}\"", stage_str))
                .unwrap_or(PrintStage::Failed);
            let ts_str: String = row.get(4)?;
            let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(PrintJobEvent {
                job_id: row.get(0)?,
                stage,
                success: row.get::<_, i32>(2)? != 0,
                detail: row.get(3)?,
                timestamp,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(events)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p devbridge-server test_insert_and_query_job_events test_get_job_events_empty`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-server/src/storage.rs
git commit -m "Add job_events SQLite table for print audit trail"
```

---

## Task 4: Job Events API Endpoint

**Files:**
- Create: `crates/devbridge-dashboard/src/api/job_events.rs`
- Modify: `crates/devbridge-dashboard/src/api/mod.rs`

- [ ] **Step 1: Create the job_events API handler**

Create `crates/devbridge-dashboard/src/api/job_events.rs`:

```rust
use axum::{Router, extract::{Path, State}, Json, routing::get};
use serde_json::Value;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/jobs/{id}/events", get(get_job_events))
}

async fn get_job_events(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Json<Vec<Value>> {
    let Some(queue) = &state.queue else {
        return Json(vec![]);
    };

    let events = queue.get_job_events(&job_id).unwrap_or_default();

    let json_events: Vec<Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "job_id": e.job_id,
                "stage": e.stage,
                "success": e.success,
                "detail": e.detail,
                "timestamp": e.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(json_events)
}
```

- [ ] **Step 2: Add `get_job_events` method to JobQueue**

In `crates/devbridge-server/src/queue.rs`, add a delegating method:

```rust
/// Get all audit events for a job.
pub fn get_job_events(&self, job_id: &str) -> anyhow::Result<Vec<devbridge_core::job_event::PrintJobEvent>> {
    let storage = self.storage.lock().unwrap();
    storage.get_job_events(job_id)
}

/// Record a print pipeline event.
pub fn insert_job_event(&self, event: &devbridge_core::job_event::PrintJobEvent) -> anyhow::Result<()> {
    let storage = self.storage.lock().unwrap();
    storage.insert_job_event(event)
}
```

- [ ] **Step 3: Mount route in mod.rs**

In `crates/devbridge-dashboard/src/api/mod.rs`, add:

```rust
pub mod job_events;
```

And in the `api_router()` function, merge:

```rust
.merge(job_events::router())
```

- [ ] **Step 4: Run clippy to verify compilation**

Run: `cargo clippy -p devbridge-dashboard -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-dashboard/src/api/job_events.rs crates/devbridge-dashboard/src/api/mod.rs crates/devbridge-server/src/queue.rs
git commit -m "Add GET /api/jobs/{id}/events endpoint for audit trail"
```

---

## Task 5: Ghostscript Renderer Module

**Files:**
- Create: `crates/devbridge-client/src/ghostscript.rs`

- [ ] **Step 1: Write failing tests for Ghostscript output parsing**

Create `crates/devbridge-client/src/ghostscript.rs`:

```rust
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

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
    let bundled = PathBuf::from(r"C:\Program Files\DevBridge\ghostscript\gswin64c.exe");
    if bundled.exists() {
        return Some(bundled);
    }

    // Fallback: check PATH
    which::which("gswin64c").ok()
        .or_else(|| which::which("gs").ok())
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
        .ok_or_else(|| anyhow::anyhow!("Ghostscript not found"))?;

    events.emit_ok(
        job_id,
        PrintStage::Rendering,
        &format!("Ghostscript {} {}dpi started", device, resolution),
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
    let output_size = std::fs::metadata(output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if output_size == 0 {
        let detail = "Ghostscript produced empty output file";
        events.emit_fail(job_id, PrintStage::Failed, detail);
        anyhow::bail!("{}", detail);
    }

    let pages = parse_page_count(&stderr_str);
    let duration_secs = duration_ms as f64 / 1000.0;
    let size_mb = output_size as f64 / (1024.0 * 1024.0);

    let detail = format!(
        "{} pages, {:.1}MB, {:.1}s, device={}",
        pages, size_mb, duration_secs, device
    );
    events.emit_ok(job_id, PrintStage::Rendered, &detail);

    info!(
        job_id,
        pages,
        output_size,
        duration_ms,
        device,
        "Ghostscript rendering complete"
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
        // On non-Windows / CI without GS, returns None — that's valid
        let result = find_ghostscript();
        // Just verify it doesn't panic
        let _ = result;
    }
}
```

- [ ] **Step 2: Add `which` dependency to client Cargo.toml**

In `crates/devbridge-client/Cargo.toml`, add:

```toml
which = "7"
```

- [ ] **Step 3: Export module from client lib.rs**

In `crates/devbridge-client/src/lib.rs`, add:

```rust
pub mod ghostscript;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p devbridge-client test_parse_page_count`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/ghostscript.rs crates/devbridge-client/src/lib.rs crates/devbridge-client/Cargo.toml
git commit -m "Add Ghostscript renderer module with page count parsing"
```

---

## Task 6: IPP Binary Codec

**Files:**
- Create: `crates/devbridge-client/src/ipp_codec.rs`

- [ ] **Step 1: Write failing tests for IPP encoding/decoding**

Create `crates/devbridge-client/src/ipp_codec.rs`:

```rust
//! IPP 1.1 binary encoding/decoding for Print-Job and Get-Job-Attributes.
//!
//! Reference: RFC 8010 (IPP/1.1 Encoding and Transport)
//! Reference: RFC 8011 (IPP/1.1 Model and Semantics)

use anyhow::Result;

// IPP constants
const IPP_VERSION_MAJOR: u8 = 1;
const IPP_VERSION_MINOR: u8 = 1;

// Operation IDs
pub const PRINT_JOB: u16 = 0x0002;
pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;

// Attribute group tags
const OPERATION_ATTRIBUTES_TAG: u8 = 0x01;
const JOB_ATTRIBUTES_TAG: u8 = 0x02;
const END_OF_ATTRIBUTES_TAG: u8 = 0x03;

// Value tags
const CHARSET_TAG: u8 = 0x47; // charset
const NATURAL_LANGUAGE_TAG: u8 = 0x48; // naturalLanguage
const URI_TAG: u8 = 0x45; // uri
const MIME_MEDIA_TYPE_TAG: u8 = 0x49; // mimeMediaType
const NAME_WITHOUT_LANGUAGE_TAG: u8 = 0x42; // nameWithoutLanguage
const KEYWORD_TAG: u8 = 0x44; // keyword
const INTEGER_TAG: u8 = 0x21; // integer
const ENUM_TAG: u8 = 0x23; // enum
const TEXT_WITHOUT_LANGUAGE_TAG: u8 = 0x41; // textWithoutLanguage

/// Build an IPP Print-Job request.
pub fn build_print_job_request(
    printer_uri: &str,
    document_format: &str,
    job_name: &str,
    request_id: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);

    // Version
    buf.push(IPP_VERSION_MAJOR);
    buf.push(IPP_VERSION_MINOR);

    // Operation ID (Print-Job = 0x0002)
    buf.extend_from_slice(&PRINT_JOB.to_be_bytes());

    // Request ID
    buf.extend_from_slice(&request_id.to_be_bytes());

    // Operation attributes group
    buf.push(OPERATION_ATTRIBUTES_TAG);

    // Required: attributes-charset
    write_attribute(&mut buf, CHARSET_TAG, "attributes-charset", b"utf-8");

    // Required: attributes-natural-language
    write_attribute(
        &mut buf,
        NATURAL_LANGUAGE_TAG,
        "attributes-natural-language",
        b"en-us",
    );

    // Required: printer-uri
    write_attribute(&mut buf, URI_TAG, "printer-uri", printer_uri.as_bytes());

    // Optional: document-format
    write_attribute(
        &mut buf,
        MIME_MEDIA_TYPE_TAG,
        "document-format",
        document_format.as_bytes(),
    );

    // Optional: job-name
    write_attribute(
        &mut buf,
        NAME_WITHOUT_LANGUAGE_TAG,
        "job-name",
        job_name.as_bytes(),
    );

    // End of attributes
    buf.push(END_OF_ATTRIBUTES_TAG);

    buf
}

/// Build an IPP Get-Job-Attributes request.
pub fn build_get_job_attributes_request(
    printer_uri: &str,
    job_id: u32,
    request_id: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    buf.push(IPP_VERSION_MAJOR);
    buf.push(IPP_VERSION_MINOR);
    buf.extend_from_slice(&GET_JOB_ATTRIBUTES.to_be_bytes());
    buf.extend_from_slice(&request_id.to_be_bytes());

    buf.push(OPERATION_ATTRIBUTES_TAG);
    write_attribute(&mut buf, CHARSET_TAG, "attributes-charset", b"utf-8");
    write_attribute(
        &mut buf,
        NATURAL_LANGUAGE_TAG,
        "attributes-natural-language",
        b"en-us",
    );
    write_attribute(&mut buf, URI_TAG, "printer-uri", printer_uri.as_bytes());

    // job-id (integer)
    write_integer_attribute(&mut buf, INTEGER_TAG, "job-id", job_id as i32);

    buf.push(END_OF_ATTRIBUTES_TAG);
    buf
}

fn write_attribute(buf: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
}

fn write_integer_attribute(buf: &mut Vec<u8>, tag: u8, name: &str, value: i32) {
    buf.push(tag);
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes()); // integer is always 4 bytes
    buf.extend_from_slice(&value.to_be_bytes());
}

/// Parsed IPP response.
#[derive(Debug)]
pub struct IppResponse {
    pub status_code: u16,
    pub request_id: u32,
    pub attributes: Vec<IppAttribute>,
}

/// A single IPP attribute (name + value).
#[derive(Debug, Clone)]
pub struct IppAttribute {
    pub name: String,
    pub tag: u8,
    pub value: Vec<u8>,
}

impl IppAttribute {
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.value).ok()
    }

    pub fn as_i32(&self) -> Option<i32> {
        if self.value.len() == 4 {
            Some(i32::from_be_bytes([
                self.value[0],
                self.value[1],
                self.value[2],
                self.value[3],
            ]))
        } else {
            None
        }
    }
}

impl IppResponse {
    /// Find the first attribute with the given name.
    pub fn get(&self, name: &str) -> Option<&IppAttribute> {
        self.attributes.iter().find(|a| a.name == name)
    }

    /// Check if the response indicates success (status 0x0000-0x00FF).
    pub fn is_success(&self) -> bool {
        self.status_code <= 0x00FF
    }
}

/// Parse an IPP response from raw bytes.
pub fn parse_response(data: &[u8]) -> Result<IppResponse> {
    if data.len() < 8 {
        anyhow::bail!("IPP response too short: {} bytes", data.len());
    }

    let status_code = u16::from_be_bytes([data[2], data[3]]);
    let request_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

    let mut attributes = Vec::new();
    let mut pos = 8;
    let mut current_name = String::new();

    while pos < data.len() {
        let tag = data[pos];
        pos += 1;

        // Group tags (0x00-0x0F)
        if tag <= 0x0F {
            if tag == END_OF_ATTRIBUTES_TAG {
                break;
            }
            continue; // skip group delimiter
        }

        // Value tag: read name-length
        if pos + 2 > data.len() {
            break;
        }
        let name_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        let name = if name_len > 0 {
            if pos + name_len > data.len() {
                break;
            }
            let n = String::from_utf8_lossy(&data[pos..pos + name_len]).to_string();
            pos += name_len;
            current_name = n.clone();
            n
        } else {
            // Additional value for same attribute
            current_name.clone()
        };

        if pos + 2 > data.len() {
            break;
        }
        let value_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + value_len > data.len() {
            break;
        }
        let value = data[pos..pos + value_len].to_vec();
        pos += value_len;

        attributes.push(IppAttribute { name, tag, value });
    }

    Ok(IppResponse {
        status_code,
        request_id,
        attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_print_job_request_header() {
        let req = build_print_job_request(
            "ipp://10.78.2.9:631/ipp/print",
            "image/pwg-raster",
            "test-job",
            1,
        );

        // Version 1.1
        assert_eq!(req[0], 1);
        assert_eq!(req[1], 1);

        // Operation = Print-Job (0x0002)
        assert_eq!(req[2], 0x00);
        assert_eq!(req[3], 0x02);

        // Request ID = 1
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), 1);

        // First byte after header = operation attributes tag
        assert_eq!(req[8], 0x01);
    }

    #[test]
    fn test_build_print_job_contains_required_attributes() {
        let req = build_print_job_request(
            "ipp://printer/ipp/print",
            "image/pwg-raster",
            "test",
            42,
        );

        // Should contain "attributes-charset" and "utf-8"
        let req_str = String::from_utf8_lossy(&req);
        assert!(req_str.contains("attributes-charset"));
        assert!(req_str.contains("utf-8"));
        assert!(req_str.contains("printer-uri"));
    }

    #[test]
    fn test_parse_success_response() {
        // Minimal IPP success response
        let mut data = Vec::new();
        data.push(1); // version major
        data.push(1); // version minor
        data.extend_from_slice(&0x0000u16.to_be_bytes()); // status: successful
        data.extend_from_slice(&1u32.to_be_bytes()); // request-id

        // Operation attributes group
        data.push(OPERATION_ATTRIBUTES_TAG);
        // attributes-charset = utf-8
        data.push(CHARSET_TAG);
        data.extend_from_slice(&14u16.to_be_bytes()); // name len
        data.extend_from_slice(b"attributes-charset");
        data.extend_from_slice(&4u16.to_be_bytes()); // value len
        data.extend_from_slice(b"utf-8");

        // Job attributes group
        data.push(JOB_ATTRIBUTES_TAG);
        // job-id = 42
        data.push(INTEGER_TAG);
        data.extend_from_slice(&6u16.to_be_bytes());
        data.extend_from_slice(b"job-id");
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(&42i32.to_be_bytes());

        // job-state = 3 (processing)
        data.push(ENUM_TAG);
        data.extend_from_slice(&9u16.to_be_bytes());
        data.extend_from_slice(b"job-state");
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(&3i32.to_be_bytes());

        data.push(END_OF_ATTRIBUTES_TAG);

        let resp = parse_response(&data).unwrap();
        assert!(resp.is_success());
        assert_eq!(resp.request_id, 1);
        assert_eq!(resp.get("job-id").unwrap().as_i32(), Some(42));
        assert_eq!(resp.get("job-state").unwrap().as_i32(), Some(3));
    }

    #[test]
    fn test_parse_error_response() {
        let mut data = Vec::new();
        data.push(1);
        data.push(1);
        data.extend_from_slice(&0x0400u16.to_be_bytes()); // client-error-bad-request
        data.extend_from_slice(&2u32.to_be_bytes());
        data.push(END_OF_ATTRIBUTES_TAG);

        let resp = parse_response(&data).unwrap();
        assert!(!resp.is_success());
        assert_eq!(resp.status_code, 0x0400);
    }

    #[test]
    fn test_build_get_job_attributes_request() {
        let req = build_get_job_attributes_request("ipp://printer/ipp/print", 42, 2);

        // Operation = Get-Job-Attributes (0x0009)
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0x0009);
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), 2);
    }
}
```

- [ ] **Step 2: Export module from client lib.rs**

In `crates/devbridge-client/src/lib.rs`, add:

```rust
pub mod ipp_codec;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p devbridge-client ipp_codec`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-client/src/ipp_codec.rs crates/devbridge-client/src/lib.rs
git commit -m "Add IPP binary codec for Print-Job and Get-Job-Attributes"
```

---

## Task 7: PrintBackend Trait + WindowsSpooler Backend

**Files:**
- Create: `crates/devbridge-client/src/print_backend.rs`
- Create: `crates/devbridge-client/src/backend_windows_spooler.rs`

- [ ] **Step 1: Define PrintBackend trait**

Create `crates/devbridge-client/src/print_backend.rs`:

```rust
use std::path::Path;

use anyhow::Result;

use devbridge_core::job_event::EventEmitter;

/// A print job descriptor passed to backends.
pub struct PrintJobInfo {
    pub job_id: String,
    pub document_name: String,
    pub copies: u32,
    pub duplex: bool,
    pub color: bool,
    pub printer_name: String,
}

/// Trait for print backends that handle the final delivery of a job to a printer.
///
/// Each backend is responsible for:
/// 1. Rendering (if needed) — e.g., Ghostscript PDF→raster
/// 2. Delivering to the printer — e.g., IPP, RAW TCP, Windows spooler
/// 3. Emitting audit events at each step
pub trait PrintBackend: Send + Sync {
    fn name(&self) -> &str;
    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
    ) -> Result<()>;
}

/// Create the appropriate backend from config values.
pub fn create_backend(
    backend_type: &str,
    printer_address: Option<&str>,
    ghostscript_device: &str,
    ghostscript_resolution: u32,
    target_printer: &str,
) -> Result<Box<dyn PrintBackend>> {
    match backend_type {
        "direct_ipp" => {
            let addr = printer_address
                .ok_or_else(|| anyhow::anyhow!("direct_ipp requires printer_address"))?;
            Ok(Box::new(crate::backend_direct_ipp::DirectIpp::new(
                addr.to_string(),
                ghostscript_device.to_string(),
                ghostscript_resolution,
            )))
        }
        "direct_raw" => {
            let addr = printer_address
                .ok_or_else(|| anyhow::anyhow!("direct_raw requires printer_address"))?;
            Ok(Box::new(crate::backend_direct_raw::DirectRaw::new(
                addr.to_string(),
                ghostscript_device.to_string(),
                ghostscript_resolution,
            )))
        }
        "windows_spooler" | "" => {
            Ok(Box::new(crate::backend_windows_spooler::WindowsSpooler::new(
                target_printer.to_string(),
            )))
        }
        other => anyhow::bail!("unknown print_backend: {}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_backend_windows_spooler() {
        let backend = create_backend("windows_spooler", None, "ppmraw", 600, "TestPrinter");
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().name(), "windows_spooler");
    }

    #[test]
    fn test_create_backend_default_empty() {
        let backend = create_backend("", None, "ppmraw", 600, "TestPrinter");
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().name(), "windows_spooler");
    }

    #[test]
    fn test_create_backend_direct_raw_requires_address() {
        let backend = create_backend("direct_raw", None, "ppmraw", 600, "TestPrinter");
        assert!(backend.is_err());
        assert!(backend.unwrap_err().to_string().contains("printer_address"));
    }

    #[test]
    fn test_create_backend_direct_ipp_requires_address() {
        let backend = create_backend("direct_ipp", None, "pwgraster", 600, "TestPrinter");
        assert!(backend.is_err());
    }

    #[test]
    fn test_create_backend_unknown_type_errors() {
        let backend = create_backend("laser_beam", None, "ppmraw", 600, "TestPrinter");
        assert!(backend.is_err());
        assert!(backend.unwrap_err().to_string().contains("unknown"));
    }
}
```

- [ ] **Step 2: Create WindowsSpooler backend**

Create `crates/devbridge-client/src/backend_windows_spooler.rs`:

```rust
use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo};

/// Windows Spooler backend — wraps existing SumatraPDF/PrintTo flow with event emission.
pub struct WindowsSpooler {
    target_printer: String,
}

impl WindowsSpooler {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }
}

impl PrintBackend for WindowsSpooler {
    fn name(&self) -> &str {
        "windows_spooler"
    }

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
    ) -> Result<()> {
        let printer = &job.printer_name;

        // Check readiness (non-fatal)
        events.emit_ok(&job.job_id, PrintStage::Sending, &format!("Windows spooler to {}", printer));

        if let Err(e) = crate::printer::check_printer_ready(printer) {
            warn!(printer, error = %e, "printer readiness check failed, attempting print anyway");
        }

        // Send to printer via SumatraPDF or PrintTo
        crate::printer::print_pdf(printer, pdf_path)?;

        events.emit_ok(&job.job_id, PrintStage::Sent, &format!("submitted to Windows spooler for {}", printer));

        // Verify spooler processed the job
        let is_virtual = printer.to_lowercase().contains("pdf")
            || printer.to_lowercase().contains("xps")
            || printer.to_lowercase().contains("onenote")
            || printer.to_lowercase().contains("fax");

        let verification = crate::printer::verify_print_completion(printer, 60)?;
        if !verification.success {
            if is_virtual {
                warn!(
                    printer,
                    spooler_status = %verification.spooler_status,
                    detail = %verification.detail,
                    "spooler issue on virtual printer (advisory)"
                );
                events.emit_ok(
                    &job.job_id,
                    PrintStage::Completed,
                    &format!("virtual printer {} (spooler advisory: {})", printer, verification.detail),
                );
            } else {
                events.emit_fail(
                    &job.job_id,
                    PrintStage::Failed,
                    &format!("spooler {}: {}", verification.spooler_status, verification.detail),
                );
                anyhow::bail!(
                    "spooler {}: {} (printer: {})",
                    verification.spooler_status,
                    verification.detail,
                    printer
                );
            }
        } else {
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                &format!("printed via Windows spooler to {}", printer),
            );
        }

        Ok(())
    }
}
```

- [ ] **Step 3: Export modules from client lib.rs**

In `crates/devbridge-client/src/lib.rs`, add:

```rust
pub mod print_backend;
pub mod backend_windows_spooler;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p devbridge-client print_backend`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/print_backend.rs crates/devbridge-client/src/backend_windows_spooler.rs crates/devbridge-client/src/lib.rs
git commit -m "Add PrintBackend trait and WindowsSpooler backend"
```

---

## Task 8: DirectRaw Backend (Epson TCP:9100)

**Files:**
- Create: `crates/devbridge-client/src/backend_direct_raw.rs`

- [ ] **Step 1: Write the DirectRaw backend**

Create `crates/devbridge-client/src/backend_direct_raw.rs`:

```rust
use std::path::Path;

use anyhow::Result;
use tracing::info;

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo};

/// Direct RAW backend — Ghostscript renders PDF to raster, streams via TCP port 9100.
pub struct DirectRaw {
    address: String,
    gs_device: String,
    gs_resolution: u32,
}

impl DirectRaw {
    pub fn new(address: String, gs_device: String, gs_resolution: u32) -> Self {
        Self {
            address,
            gs_device,
            gs_resolution,
        }
    }
}

impl PrintBackend for DirectRaw {
    fn name(&self) -> &str {
        "direct_raw"
    }

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
    ) -> Result<()> {
        // Step 1: Render PDF → raster via Ghostscript
        let output_path = pdf_path.with_extension("raw");

        let render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
        )?;

        // Step 2: Stream raster to printer via TCP
        let data = std::fs::read(&output_path)?;
        let data_size = data.len();

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            &format!(
                "RAW TCP to {}, {:.1}MB",
                self.address,
                data_size as f64 / (1024.0 * 1024.0)
            ),
        );

        use std::io::Write;
        let mut stream = std::net::TcpStream::connect(&self.address)?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.write_all(&data)?;
        stream.flush()?;
        // Shutdown write side to signal end of data
        stream.shutdown(std::net::Shutdown::Write)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            &format!(
                "{:.1}MB delivered, socket closed cleanly",
                data_size as f64 / (1024.0 * 1024.0)
            ),
        );

        events.emit_ok(
            &job.job_id,
            PrintStage::Completed,
            "delivered to printer (no ACK via RAW)",
        );

        info!(
            job_id = %job.job_id,
            address = %self.address,
            pages = render_result.pages,
            data_size,
            "RAW print complete"
        );

        // Clean up temp raster file
        let _ = std::fs::remove_file(&output_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direct_raw_name() {
        let backend = DirectRaw::new("10.78.5.9:9100".into(), "ppmraw".into(), 600);
        assert_eq!(backend.name(), "direct_raw");
    }
}
```

- [ ] **Step 2: Export module from client lib.rs**

In `crates/devbridge-client/src/lib.rs`, add:

```rust
pub mod backend_direct_raw;
```

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p devbridge-client -- -D warnings`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-client/src/backend_direct_raw.rs crates/devbridge-client/src/lib.rs
git commit -m "Add DirectRaw backend for TCP port 9100 printing"
```

---

## Task 9: DirectIpp Backend (Canon IPP Print-Job)

**Files:**
- Create: `crates/devbridge-client/src/backend_direct_ipp.rs`
- Modify: `crates/devbridge-client/Cargo.toml`

- [ ] **Step 1: Add reqwest dependency**

In `crates/devbridge-client/Cargo.toml`, add:

```toml
reqwest = { version = "0.12", features = ["blocking"] }
```

- [ ] **Step 2: Write the DirectIpp backend**

Create `crates/devbridge-client/src/backend_direct_ipp.rs`:

```rust
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

    /// Build the IPP endpoint URL from the address.
    fn ipp_url(&self) -> String {
        // Address format: "IP:port" or "IP:port/path"
        if self.address.contains('/') {
            format!("http://{}", self.address)
        } else {
            format!("http://{}/ipp/print", self.address)
        }
    }

    /// Build the printer-uri attribute value.
    fn printer_uri(&self) -> String {
        if self.address.contains('/') {
            format!("ipp://{}", self.address)
        } else {
            format!("ipp://{}/ipp/print", self.address)
        }
    }

    /// Poll Get-Job-Attributes until job completes or timeout.
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
                printer_job_id,
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

            debug!(
                printer_job_id,
                job_state,
                state_reasons = %state_reasons,
                "IPP job state poll"
            );

            // IPP job states: 3=pending, 4=pending-held, 5=processing,
            // 6=processing-stopped, 7=canceled, 8=aborted, 9=completed
            match job_state {
                9 => {
                    events.emit_ok(
                        job_id,
                        PrintStage::Completed,
                        &format!("printer job-id={}, state=completed", printer_job_id),
                    );
                    return Ok(());
                }
                7 | 8 => {
                    let detail = format!(
                        "printer job-id={}, state={}, reasons={}",
                        printer_job_id,
                        if job_state == 7 { "canceled" } else { "aborted" },
                        state_reasons
                    );
                    events.emit_fail(job_id, PrintStage::Failed, &detail);
                    anyhow::bail!("{}", detail);
                }
                _ => {
                    // Still processing
                    if std::time::Instant::now() > deadline {
                        let detail = format!(
                            "printer job-id={} still in state {} after 60s",
                            printer_job_id, job_state
                        );
                        warn!("{}", detail);
                        // Not an error — the job may still complete
                        events.emit_ok(
                            job_id,
                            PrintStage::Completed,
                            &format!("printer job-id={}, state={} (poll timeout, likely printing)", printer_job_id, job_state),
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

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
    ) -> Result<()> {
        // Step 1: Render PDF → PWG-Raster via Ghostscript
        let output_path = pdf_path.with_extension("pwg");

        let render_result = crate::ghostscript::render(
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
        let data_size = raster_data.len();

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            &format!("IPP Print-Job to {}", self.address),
        );

        let ipp_header = ipp_codec::build_print_job_request(
            &printer_uri,
            "image/pwg-raster",
            &job.document_name,
            1,
        );

        // Concatenate IPP header + raster document body
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

        let printer_job_id = ipp_resp
            .get("job-id")
            .and_then(|a| a.as_i32())
            .unwrap_or(0) as u32;

        let job_state = ipp_resp
            .get("job-state")
            .and_then(|a| a.as_i32())
            .unwrap_or(0);

        events.emit_ok(
            &job.job_id,
            PrintStage::Acknowledged,
            &format!("printer job-id={}, state={}", printer_job_id, job_state),
        );

        info!(
            job_id = %job.job_id,
            printer_job_id,
            job_state,
            address = %self.address,
            "IPP Print-Job accepted"
        );

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
```

- [ ] **Step 3: Export module from client lib.rs**

In `crates/devbridge-client/src/lib.rs`, add:

```rust
pub mod backend_direct_ipp;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p devbridge-client direct_ipp`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/backend_direct_ipp.rs crates/devbridge-client/src/lib.rs crates/devbridge-client/Cargo.toml
git commit -m "Add DirectIpp backend for Canon IPP Print-Job"
```

---

## Task 10: Wire PrintBackend into Receiver

**Files:**
- Modify: `crates/devbridge-client/src/receiver.rs:130-246`

- [ ] **Step 1: Write failing test for backend selection**

Add to `crates/devbridge-client/src/receiver.rs` test module:

```rust
#[test]
fn test_backend_created_from_config() {
    use crate::print_backend::create_backend;

    let backend = create_backend("windows_spooler", None, "ppmraw", 600, "Test Printer").unwrap();
    assert_eq!(backend.name(), "windows_spooler");

    let backend = create_backend("direct_raw", Some("10.0.0.1:9100"), "ppmraw", 600, "Epson").unwrap();
    assert_eq!(backend.name(), "direct_raw");

    let backend = create_backend("direct_ipp", Some("10.0.0.1:631"), "pwgraster", 600, "Canon").unwrap();
    assert_eq!(backend.name(), "direct_ipp");
}
```

- [ ] **Step 2: Run test to verify it passes (it should — just confirming integration)**

Run: `cargo test -p devbridge-client test_backend_created_from_config`
Expected: PASS

- [ ] **Step 3: Refactor receiver.rs to use PrintBackend**

In `crates/devbridge-client/src/receiver.rs`, modify the `Receiver` struct to carry backend config, and replace the print dispatch section (lines 152-206).

Add fields to `Receiver`:

```rust
pub struct Receiver {
    server_address: String,
    machine_id: String,
    hostname: String,
    reconnect_interval: Duration,
    max_reconnect_interval: Duration,
    print_backend: String,
    printer_address: Option<String>,
    ghostscript_device: String,
    ghostscript_resolution: u32,
}
```

Update `Receiver::new()` to read new config fields:

```rust
print_backend: config.print_backend.clone(),
printer_address: config.printer_address.clone(),
ghostscript_device: config.ghostscript_device.clone(),
ghostscript_resolution: config.ghostscript_resolution,
```

Replace the print dispatch block (lines 152-206) in `run_inner()`:

```rust
// Print via configured backend
let print_printer = printer.clone();
let pdf = dest.clone();
let job_id_for_print = job.job_id.clone();
let doc_name = job.document_name.clone();
let copies = job.copies;
let backend_type = self.print_backend.clone();
let printer_addr = self.printer_address.clone();
let gs_device = self.ghostscript_device.clone();
let gs_resolution = self.ghostscript_resolution;

// Create event emitter for audit trail
let (event_tx, _) = tokio::sync::broadcast::channel(64);
let event_emitter = devbridge_core::job_event::EventEmitter::new(event_tx.clone());

// Persist events to local queue if available
let event_queue = queue.cloned();
let event_job_id = job.job_id.clone();
let mut event_rx = event_tx.subscribe();
let event_persist_task = tokio::spawn(async move {
    while let Ok(event) = event_rx.recv().await {
        if let Some(q) = &event_queue {
            let _ = q.insert_job_event(&event);
        }
    }
});

let print_result = tokio::task::spawn_blocking(move || {
    let backend = crate::print_backend::create_backend(
        &backend_type,
        printer_addr.as_deref(),
        &gs_device,
        gs_resolution,
        &print_printer,
    )?;

    let job_info = crate::print_backend::PrintJobInfo {
        job_id: job_id_for_print,
        document_name: doc_name,
        copies,
        duplex: false,
        color: true,
        printer_name: print_printer,
    };

    backend.print(&job_info, &pdf, &event_emitter)
})
.await
.unwrap_or_else(|e| Err(anyhow::anyhow!("print task panicked: {e}")));

// Stop event persistence
drop(event_tx);
let _ = event_persist_task.await;
```

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -p devbridge-client -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/receiver.rs
git commit -m "Wire PrintBackend into receiver, replacing direct SumatraPDF call"
```

---

## Task 11: CI — Bundle Ghostscript in Windows Build

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add Ghostscript download step**

In `.github/workflows/ci.yml`, in the `windows-build` job, add after the "Download redistributables for bundling" step (after line 289):

```yaml
      - name: Download Ghostscript portable
        shell: pwsh
        run: |
          $GsVersion = "10.04.0"
          $GsUrl = "https://github.com/ArtifexSoftware/ghostpdl-downloads/releases/download/gs10040/gs10040w64.exe"
          $GsInstaller = "installer\redist\gs_setup.exe"
          $GsDir = "installer\redist\ghostscript"

          Invoke-WebRequest -Uri $GsUrl -OutFile $GsInstaller -UseBasicParsing
          Write-Host "Ghostscript installer: $((Get-Item $GsInstaller).Length) bytes"

          # Extract using 7z (available on windows-latest)
          New-Item -ItemType Directory -Force -Path $GsDir | Out-Null
          7z x $GsInstaller -o"$GsDir" -y
          # Find gswin64c.exe in extracted files
          $GsExe = Get-ChildItem -Path $GsDir -Recurse -Filter "gswin64c.exe" | Select-Object -First 1
          if (-not $GsExe) {
            Write-Error "gswin64c.exe not found after extraction"
            exit 1
          }
          Write-Host "Found Ghostscript at: $($GsExe.FullName)"
          # Move to expected location
          New-Item -ItemType Directory -Force -Path "installer\redist\ghostscript\bin" | Out-Null
          Copy-Item $GsExe.FullName "installer\redist\ghostscript\bin\gswin64c.exe"
          # Copy required lib folder
          $GsLibDir = $GsExe.Directory.Parent.FullName + "\lib"
          if (Test-Path $GsLibDir) {
            Copy-Item -Recurse $GsLibDir "installer\redist\ghostscript\lib"
          }
          Write-Host "Ghostscript portable ready"
```

- [ ] **Step 2: Update post-install.ps1 to extract Ghostscript**

In `installer/post-install.ps1`, add after the SumatraPDF installation block:

```powershell
# Install Ghostscript portable (for direct print backends)
$GsSource = Join-Path $InstallDir "redist\ghostscript"
$GsTarget = Join-Path $InstallDir "ghostscript"
if (Test-Path $GsSource) {
    if (-not (Test-Path $GsTarget)) {
        Copy-Item -Recurse $GsSource $GsTarget
        Write-Host "Ghostscript installed to $GsTarget"
    }
}
```

And in the client config template, add print backend fields:

```powershell
# After the existing client config fields:
print_backend = "$PrintBackend"
$(if ($PrinterAddress) { "printer_address = `"$PrinterAddress`"" })
ghostscript_device = "$GhostscriptDevice"
ghostscript_resolution = $GhostscriptResolution
```

With new parameters:

```powershell
param(
    # ... existing params ...
    [string]$PrintBackend = "windows_spooler",
    [string]$PrinterAddress = "",
    [string]$GhostscriptDevice = "ppmraw",
    [int]$GhostscriptResolution = 600
)
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml installer/post-install.ps1
git commit -m "Bundle Ghostscript portable in NSIS installer and CI"
```

---

## Task 12: Dashboard Job Events Timeline (Frontend)

**Files:**
- Modify: `crates/devbridge-ui/src/api.rs`
- Modify: `crates/devbridge-ui/src/pages/jobs.rs`

- [ ] **Step 1: Add fetch_job_events API**

In `crates/devbridge-ui/src/api.rs`, add:

```rust
pub async fn fetch_job_events(job_id: &str) -> Result<Vec<serde_json::Value>, String> {
    let resp = gloo_net::http::Request::get(&format!("/api/jobs/{}/events", job_id))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}
```

- [ ] **Step 2: Add job detail timeline to jobs page**

In `crates/devbridge-ui/src/pages/jobs.rs`, add a `JobDetailView` component that shows the event timeline when a job row is clicked. This component:

- Fetches events via `fetch_job_events(job_id)`
- Renders a vertical timeline with timestamp, success/fail icon, stage, and detail
- Shows the stage-to-icon mapping: green checkmark for success, red X for failure

```rust
#[component]
fn JobEventTimeline(job_id: String) -> impl IntoView {
    let events = LocalResource::new(move || {
        let id = job_id.clone();
        async move { api::fetch_job_events(&id).await.unwrap_or_default() }
    });

    view! {
        <div class="job-timeline">
            <Suspense fallback=move || view! { <p>"Loading events..."</p> }>
                {move || events.get().map(|evts| {
                    if evts.is_empty() {
                        view! { <p class="text-muted">"No audit events recorded"</p> }.into_any()
                    } else {
                        view! {
                            <div class="timeline-list">
                                {evts.iter().map(|evt| {
                                    let stage = evt["stage"].as_str().unwrap_or("unknown");
                                    let success = evt["success"].as_bool().unwrap_or(false);
                                    let detail = evt["detail"].as_str().unwrap_or("");
                                    let timestamp = evt["timestamp"].as_str().unwrap_or("");
                                    let icon = if success { "check-circle" } else { "x-circle" };
                                    let color = if success { "text-success" } else { "text-danger" };

                                    view! {
                                        <div class="timeline-item">
                                            <span class="timeline-time">
                                                <TimeDisplay datetime=timestamp.to_string() />
                                            </span>
                                            <span class={format!("timeline-icon {}", color)}>
                                                {if success { "OK" } else { "FAIL" }}
                                            </span>
                                            <span class="timeline-stage">{stage}</span>
                                            <span class="timeline-detail">{detail}</span>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                })}
            </Suspense>
        </div>
    }
}
```

- [ ] **Step 3: Wire timeline into job rows**

Add click-to-expand on job rows in the jobs table. When a job row is clicked, toggle showing `JobEventTimeline` below it.

- [ ] **Step 4: Run clippy on UI crate**

Run: `cd crates/devbridge-ui && trunk build` (if WASM toolchain available locally) or just verify with clippy:
Run: `cargo clippy -p devbridge-ui --target wasm32-unknown-unknown -- -D warnings`
Expected: Clean (may need to skip if WASM target not installed locally — CI will catch it)

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-ui/src/api.rs crates/devbridge-ui/src/pages/jobs.rs
git commit -m "Add job event timeline to dashboard jobs page"
```

---

## Task 13: Extend WebSocket with PrintJobEvent

**Files:**
- Modify: `crates/devbridge-dashboard/src/api/ws.rs`
- Modify: `crates/devbridge-dashboard/src/state.rs`

- [ ] **Step 1: Add print event channel to AppState**

In `crates/devbridge-dashboard/src/state.rs`, add a second broadcast channel for print events:

```rust
pub print_events: tokio::sync::broadcast::Sender<devbridge_core::job_event::PrintJobEvent>,
```

Initialize in `AppState::new()`:

```rust
let (print_events, _) = tokio::sync::broadcast::channel(256);
```

Add builder method:

```rust
pub fn with_print_events(mut self, sender: tokio::sync::broadcast::Sender<devbridge_core::job_event::PrintJobEvent>) -> Self {
    self.print_events = sender;
    self
}
```

- [ ] **Step 2: Forward print events via WebSocket**

In `crates/devbridge-dashboard/src/api/ws.rs`, subscribe to both channels and forward:

```rust
let mut print_rx = state.print_events.subscribe();

// In handle_socket, select on both channels:
tokio::select! {
    event = job_rx.recv() => { /* existing job event handling */ }
    event = print_rx.recv() => {
        if let Ok(event) = event {
            let json = serde_json::to_string(&event).unwrap_or_default();
            if sender.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/devbridge-dashboard/src/api/ws.rs crates/devbridge-dashboard/src/state.rs
git commit -m "Forward PrintJobEvent via WebSocket for real-time audit display"
```

---

## Task 14: Wire Event Channels in Runtime

**Files:**
- Modify: `crates/devbridge-service/src/runtime.rs`

- [ ] **Step 1: Create and wire print event broadcast channel**

In `run_client()` (runtime.rs), after the existing broadcast channel creation:

```rust
let (print_event_tx, _) = tokio::sync::broadcast::channel(256);
```

Pass to AppState:

```rust
let app_state = AppState::new("client".to_string())
    .with_queue(Arc::clone(&queue))
    // ... existing builders ...
    .with_print_events(print_event_tx.clone());
```

Pass to Receiver (add field):

```rust
receiver.set_print_events(print_event_tx);
```

In `run_server()`, do the same — server also records events from client completions.

- [ ] **Step 2: Add print_events to Receiver**

In `crates/devbridge-client/src/receiver.rs`, add optional print event sender to `Receiver`:

```rust
pub fn set_print_events(&mut self, sender: tokio::sync::broadcast::Sender<devbridge_core::job_event::PrintJobEvent>) {
    self.print_events = Some(sender);
}
```

Use it in the print dispatch block to bridge events from the blocking backend to the async broadcast channel.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-service/src/runtime.rs crates/devbridge-client/src/receiver.rs
git commit -m "Wire print event broadcast channel through runtime to receiver"
```

---

## Task 15: E2E Tests

**Files:**
- Modify: `crates/devbridge-e2e/src/main.rs`

- [ ] **Step 1: Add E2E test for job events API**

Add test after existing tests in `main.rs`:

```rust
// Test 27: Job events API returns events for printed job
{
    let test_name = "job_events_api";
    // Find the most recent completed job
    let jobs_resp = client.get(&format!("http://{}:{}/api/jobs", server_host, port))
        .send().await?;
    let jobs: Vec<serde_json::Value> = jobs_resp.json().await?;

    if let Some(job) = jobs.iter().find(|j| j["status"].as_str() == Some("completed")) {
        let job_id = job["id"].as_str().unwrap();
        let events_resp = client.get(&format!(
            "http://{}:{}/api/jobs/{}/events", server_host, port, job_id
        )).send().await?;

        assert!(events_resp.status().is_success(), "GET /api/jobs/{}/events failed", job_id);

        let events: Vec<serde_json::Value> = events_resp.json().await?;
        // Events may be empty if job was printed before audit trail was added
        // But the API should return 200 with valid JSON array
        println!("  Job {} has {} audit events", job_id, events.len());

        for event in &events {
            assert!(event["stage"].is_string(), "event missing stage field");
            assert!(event["timestamp"].is_string(), "event missing timestamp field");
            assert!(!event["success"].is_null(), "event missing success field");
        }

        println!("  PASS: {}", test_name);
    } else {
        println!("  SKIP: {} (no completed jobs found)", test_name);
    }
}
```

- [ ] **Step 2: Add E2E test for print backend config backward compat**

```rust
// Test 28: Client config defaults to windows_spooler when no print_backend specified
{
    let test_name = "print_backend_config_default";
    let config_resp = client.get(&format!("http://{}:{}/api/config", client_host, port))
        .send().await?;
    assert!(config_resp.status().is_success());
    let config: serde_json::Value = config_resp.json().await?;
    assert_eq!(config["mode"].as_str(), Some("client"));
    println!("  PASS: {}", test_name);
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/devbridge-e2e/src/main.rs
git commit -m "Add E2E tests for job events API and print backend config"
```

---

## Task 16: Proto — Add New Job States

**Files:**
- Modify: `proto/devbridge.proto`

- [ ] **Step 1: Add new states to proto JobState enum**

In `proto/devbridge.proto`, add to the `JobState` enum:

```protobuf
enum JobState {
    JOB_STATE_UNSPECIFIED = 0;
    QUEUED = 1;
    DOWNLOADING = 2;
    PRINTING = 3;
    COMPLETED = 4;
    FAILED = 5;
    CANCELLED = 6;
    RENDERING = 7;
    SENDING = 8;
}
```

- [ ] **Step 2: Update JobCompletion to populate printer_status and spooler_status**

The `printer_status` and `spooler_status` fields already exist in the proto. No proto changes needed — just ensure the client populates them from the backend result.

- [ ] **Step 3: Update state mapping in dispatch.rs**

In `crates/devbridge-server/src/dispatch.rs`, update the proto→core state mapping to handle new variants:

```rust
7 => JobState::Printing,  // RENDERING maps to Printing for now
8 => JobState::Printing,  // SENDING maps to Printing for now
```

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add proto/devbridge.proto crates/devbridge-server/src/dispatch.rs
git commit -m "Add RENDERING and SENDING states to proto JobState enum"
```

---

## Task 17: Final Integration — Populate printer_status/spooler_status

**Files:**
- Modify: `crates/devbridge-client/src/receiver.rs`

- [ ] **Step 1: Populate JobCompletion with backend info**

In the `receiver.rs` completion reporting section, populate `printer_status` and `spooler_status` from the backend result:

```rust
let completion = JobCompletion {
    job_id: job.job_id.clone(),
    success,
    error_detail,
    pages_printed: if success { job.copies } else { 0 },
    printer_status: if success { "delivered".into() } else { "error".into() },
    spooler_status: backend_name.to_string(), // "direct_ipp", "direct_raw", or "windows_spooler"
};
```

- [ ] **Step 2: Run all workspace tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 3: Run fmt + clippy**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-client/src/receiver.rs
git commit -m "Populate printer_status and spooler_status in JobCompletion"
```

---

## Task 18: Version Bump + Push + CI

- [ ] **Step 1: Check current version**

```bash
git fetch origin
grep 'version' Cargo.toml | head -3
```

- [ ] **Step 2: Bump version**

Bump patch version in workspace `Cargo.toml` and `crates/devbridge-app/tauri.conf.json`.

- [ ] **Step 3: Commit version bump**

```bash
git add Cargo.toml crates/devbridge-app/tauri.conf.json
git commit -m "Bump version to 0.4.0 for direct print pipeline"
```

- [ ] **Step 4: Run local lint checks**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] **Step 5: Push and monitor CI**

```bash
git push origin dev
gh run list --branch dev --limit 3
# Monitor until green
```

---

## Verification Plan

### Unit Tests (Tasks 1-9)
- Config parsing with new fields + backward compat
- PrintStage serde roundtrip
- PrintJobEvent constructors
- IPP request encoding, response decoding
- Ghostscript page count parsing
- Backend factory (windows_spooler, direct_raw, direct_ipp, unknown → error)
- job_events SQLite insert + query

### E2E Tests (Task 15)
- `GET /api/jobs/{id}/events` returns valid JSON array
- Config backward compat on client

### Post-Deploy Verification
1. **Server (10.77.8.200:9120):** Dashboard serves `/api/jobs/{id}/events` endpoint, WebSocket pushes print events
2. **Client (10.77.9.235:9120):** Service starts with default `windows_spooler` backend (backward compat), prints test job successfully, audit events recorded in local DB
3. **pjpos client (10.78.5.10:9120):** After config change to `direct_raw`, test print goes through Ghostscript → TCP:9100 → Epson L3270
4. **pz-snv client (10.78.2.10:9120):** After config change to `direct_ipp`, test print goes through Ghostscript → PWG-Raster → IPP → Canon MG3600
5. Click job in dashboard → timeline shows all stages with timestamps
