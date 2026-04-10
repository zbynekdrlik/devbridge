# Tray App Print Notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real-time print job notifications, status tracking, and job history to the DevBridge tray app so store employees and terminal server users see when their documents print or fail.

**Architecture:** Capture `originating_user_name` from IPP attributes and store in the jobs database. The tray app connects to the existing `/api/ws` WebSocket for instant events, shows balloon notifications via `tauri-plugin-notification`, displays the last 5 jobs in the tray menu, and switches between 4 icon states (green/yellow/red/gray). On the terminal server, each per-user tray instance filters jobs by Windows username.

**Tech Stack:** Rust, Tauri v2, tokio-tungstenite, tauri-plugin-notification, SQLite

**Spec:** `docs/superpowers/specs/2026-04-09-tray-notifications-design.md`

**Known limitation:** The `ippper` crate (v0.4) does not expose the IPP `job-name` attribute to `SimpleIppDocument` — it is consumed internally. Document names remain `job-{uuid}` until ippper is patched upstream. The `originating_user_name` field IS exposed and is the key requirement for per-user filtering.

---

## File Structure

### New Files
| File | Purpose |
|------|---------|
| `crates/devbridge-app/src/ws_client.rs` | WebSocket client — connects to `/api/ws`, parses events, sends via mpsc channel |
| `crates/devbridge-app/src/job_tracker.rs` | Job state tracker — maintains last 5 jobs, icon state machine, triggers notifications |
| `assets/icons/tray-icon-green.png` | Tray icon: idle/OK state |
| `assets/icons/tray-icon-yellow.png` | Tray icon: printing in progress |
| `assets/icons/tray-icon-red.png` | Tray icon: last job failed |
| `assets/icons/tray-icon-gray.png` | Tray icon: service offline |

### Modified Files
| File | Changes |
|------|---------|
| `crates/devbridge-core/src/job.rs` | Add `requesting_user: Option<String>` to `JobMetadata`, add `requesting_user` to `JobEvent::Created` |
| `crates/devbridge-core/src/job_event.rs` | Add `requesting_user: Option<String>` to `PrintJobEvent` |
| `crates/devbridge-server/src/storage.rs` | Migration: `ALTER TABLE jobs ADD COLUMN requesting_user TEXT`; update insert/select queries |
| `crates/devbridge-server/src/ipp_service.rs` | Capture `originating_user_name` from `SimpleIppJobAttributes` |
| `crates/devbridge-dashboard/src/api/jobs.rs` | Expose `requesting_user` in API; add `?requesting_user=` filter |
| `crates/devbridge-app/src/main.rs` | Initialize WebSocket client and job tracker |
| `crates/devbridge-app/src/tray.rs` | Dynamic menu with job history, icon state switching, balloon notifications |
| `crates/devbridge-app/Cargo.toml` | Add `tokio-tungstenite`, `tauri-plugin-notification`, `futures-util` |
| `crates/devbridge-app/capabilities/default.json` | Add notification permission |

---

## Task 1: Capture IPP User in Backend

**Files:**
- Modify: `crates/devbridge-core/src/job.rs:29-46`
- Modify: `crates/devbridge-core/src/job_event.rs:25-37`
- Modify: `crates/devbridge-server/src/storage.rs:23-57,111-127,710-733`
- Modify: `crates/devbridge-server/src/ipp_service.rs:248-270`
- Modify: `crates/devbridge-dashboard/src/api/jobs.rs:82-122`

- [ ] **Step 1: Write failing test for `requesting_user` in JobMetadata**

In `crates/devbridge-core/src/job.rs`, add at the end:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_metadata_has_requesting_user_field() {
        let meta = JobMetadata {
            job_id: "test-1".into(),
            document_name: "test.pdf".into(),
            target_printer: "printer".into(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: false,
            payload_size: 100,
            payload_sha256: "abc".into(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: Some("alice".into()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(meta.requesting_user, Some("alice".to_string()));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p devbridge-core job_metadata_has_requesting_user_field`
Expected: FAIL — `requesting_user` field does not exist on `JobMetadata`

- [ ] **Step 3: Add `requesting_user` to JobMetadata**

In `crates/devbridge-core/src/job.rs`, add the field to `JobMetadata` (after `error_detail`, before `created_at`):

```rust
    pub requesting_user: Option<String>,
```

And add `requesting_user` to `JobEvent::Created`:

```rust
pub enum JobEvent {
    Created {
        job_id: String,
        document_name: String,
        requesting_user: Option<String>,
    },
    StateChanged {
        job_id: String,
        new_state: JobState,
    },
}
```

- [ ] **Step 4: Add `requesting_user` to PrintJobEvent**

In `crates/devbridge-core/src/job_event.rs`, add the field to `PrintJobEvent` (after `job_id`, before `stage`):

```rust
pub struct PrintJobEvent {
    pub job_id: String,
    #[serde(default)]
    pub requesting_user: Option<String>,
    pub stage: PrintStage,
    pub success: bool,
    pub detail: String,
    #[serde(default)]
    pub verification_method: String,
    #[serde(default)]
    pub verification_evidence: String,
    pub timestamp: DateTime<Utc>,
}
```

Update the `new()`, `ok()`, `fail()`, and `verified()` constructors to set `requesting_user: None`.

- [ ] **Step 5: Fix all compilation errors from new fields**

The new fields will cause compilation errors everywhere `JobMetadata`, `JobEvent::Created`, and `PrintJobEvent` are constructed. Fix each callsite:

- `crates/devbridge-server/src/ipp_service.rs` — set `requesting_user: Some(document.job_attributes.originating_user_name.clone())` in the `JobMetadata` construction (around line 255)
- `crates/devbridge-server/src/queue.rs` — wherever `JobEvent::Created` is emitted, add `requesting_user: meta.requesting_user.clone()`
- Any other callsite constructing `JobMetadata` — set `requesting_user: None` for non-IPP paths (e.g., reprint)

- [ ] **Step 6: Add database migration for `requesting_user` column**

In `crates/devbridge-server/src/storage.rs`, after the last migration (virtual_printer_name, around line 127), add:

```rust
// Migration: add requesting_user column to jobs
if conn
    .prepare("SELECT requesting_user FROM jobs LIMIT 0")
    .is_err()
{
    let _ = conn.execute_batch("ALTER TABLE jobs ADD COLUMN requesting_user TEXT;");
}
```

- [ ] **Step 7: Update storage insert to include `requesting_user`**

Find the `INSERT INTO jobs` statement in `storage.rs` and add `requesting_user` to the column list and values. It will be stored as a nullable TEXT.

- [ ] **Step 8: Update `row_to_job` to read `requesting_user`**

In the `row_to_job` function (around line 710), add:

```rust
requesting_user: row.get::<_, Option<String>>("requesting_user").unwrap_or(None),
```

- [ ] **Step 9: Expose `requesting_user` in dashboard API**

In `crates/devbridge-dashboard/src/api/jobs.rs`, in the `get_jobs` handler (around line 100), add to the JSON object:

```rust
"requesting_user": j.requesting_user,
```

Add a `requesting_user` query parameter filter. After `let limit` parsing (around line 88), add:

```rust
let filter_user: Option<String> = params.get("requesting_user").cloned();
```

Then in the iterator chain, after `.take(limit)`, add:

```rust
.filter(|j| {
    filter_user.as_ref().map_or(true, |u| {
        j.requesting_user.as_ref().map_or(false, |ru| ru == u)
    })
})
```

Note: apply `.filter()` before `.take(limit)` so the limit applies to filtered results.

- [ ] **Step 10: Run test to verify it passes**

Run: `cargo test -p devbridge-core job_metadata_has_requesting_user_field`
Expected: PASS

- [ ] **Step 11: Write test for requesting_user storage round-trip**

In `crates/devbridge-server/src/storage.rs` tests (or add a test module), write a test that:
1. Creates a `Storage` with a temp database
2. Inserts a job with `requesting_user: Some("alice")`
3. Reads it back via `get_job()`
4. Asserts `requesting_user == Some("alice")`

- [ ] **Step 12: Run all tests**

Run: `cargo test --workspace`
Expected: All pass. Fix any remaining compilation errors from the new fields.

- [ ] **Step 13: Commit**

```bash
git add crates/devbridge-core/src/job.rs crates/devbridge-core/src/job_event.rs \
  crates/devbridge-server/src/storage.rs crates/devbridge-server/src/ipp_service.rs \
  crates/devbridge-server/src/queue.rs crates/devbridge-dashboard/src/api/jobs.rs
git commit -m "Capture IPP requesting_user and expose in API"
```

---

## Task 2: Add Tray Icon Variants

**Files:**
- Create: `assets/icons/tray-icon-green.png`
- Create: `assets/icons/tray-icon-yellow.png`
- Create: `assets/icons/tray-icon-red.png`
- Create: `assets/icons/tray-icon-gray.png`

- [ ] **Step 1: Generate 4 icon variants from the existing tray icon**

The existing icon is at `assets/icons/tray-icon.png`. Create 4 color-tinted variants:
- `tray-icon-green.png` — green tint (idle/OK)
- `tray-icon-yellow.png` — yellow/amber tint (printing)
- `tray-icon-red.png` — red tint (error)
- `tray-icon-gray.png` — gray/desaturated (offline)

Use ImageMagick to create them:

```bash
# Read existing icon dimensions first
identify assets/icons/tray-icon.png

# Create colored variants (32x32 solid color circles with transparency)
convert -size 32x32 xc:none -fill '#4ade80' -draw 'circle 16,16 16,2' assets/icons/tray-icon-green.png
convert -size 32x32 xc:none -fill '#facc15' -draw 'circle 16,16 16,2' assets/icons/tray-icon-yellow.png
convert -size 32x32 xc:none -fill '#f87171' -draw 'circle 16,16 16,2' assets/icons/tray-icon-red.png
convert -size 32x32 xc:none -fill '#9ca3af' -draw 'circle 16,16 16,2' assets/icons/tray-icon-gray.png
```

If ImageMagick is not available, create simple solid-color PNG icons programmatically or use the existing icon as-is for all states initially (color switching can be refined later).

- [ ] **Step 2: Commit**

```bash
git add assets/icons/tray-icon-green.png assets/icons/tray-icon-yellow.png \
  assets/icons/tray-icon-red.png assets/icons/tray-icon-gray.png
git commit -m "Add tray icon color variants for status states"
```

---

## Task 3: Add WebSocket Client Module

**Files:**
- Create: `crates/devbridge-app/src/ws_client.rs`
- Modify: `crates/devbridge-app/Cargo.toml`

- [ ] **Step 1: Add dependencies to Cargo.toml**

In `crates/devbridge-app/Cargo.toml`, add under `[dependencies]`:

```toml
tokio-tungstenite = "0.26"
futures-util = "0.3"
tauri-plugin-notification = "2"
```

- [ ] **Step 2: Write failing test for WebSocket event parsing**

In `crates/devbridge-app/src/ws_client.rs`:

```rust
use devbridge_core::job::JobEvent;
use devbridge_core::job_event::PrintJobEvent;
use serde::Deserialize;

/// A message received from the dashboard WebSocket.
#[derive(Debug, Clone)]
pub enum WsEvent {
    Job(JobEvent),
    Print(PrintJobEvent),
}

/// Try to parse a WebSocket text message as either a JobEvent or PrintJobEvent.
pub fn parse_ws_message(text: &str) -> Option<WsEvent> {
    // PrintJobEvent has a "stage" field; JobEvent has a "type" field
    if let Ok(evt) = serde_json::from_str::<PrintJobEvent>(text) {
        return Some(WsEvent::Print(evt));
    }
    if let Ok(evt) = serde_json::from_str::<JobEvent>(text) {
        return Some(WsEvent::Job(evt));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_job_created_event() {
        let json = r#"{"type":"created","job_id":"abc","document_name":"test.pdf","requesting_user":"alice"}"#;
        let evt = parse_ws_message(json).unwrap();
        match evt {
            WsEvent::Job(JobEvent::Created { job_id, document_name, requesting_user }) => {
                assert_eq!(job_id, "abc");
                assert_eq!(document_name, "test.pdf");
                assert_eq!(requesting_user, Some("alice".to_string()));
            }
            _ => panic!("Expected JobEvent::Created"),
        }
    }

    #[test]
    fn parse_print_event() {
        let json = r#"{"job_id":"abc","requesting_user":"bob","stage":"completed","success":true,"detail":"done","verification_method":"","verification_evidence":"","timestamp":"2026-04-09T10:00:00Z"}"#;
        let evt = parse_ws_message(json).unwrap();
        match evt {
            WsEvent::Print(evt) => {
                assert_eq!(evt.job_id, "abc");
                assert_eq!(evt.requesting_user, Some("bob".to_string()));
            }
            _ => panic!("Expected PrintJobEvent"),
        }
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p devbridge-app parse_`
Expected: PASS (the parsing code is already implemented above)

- [ ] **Step 4: Add the WebSocket connection loop**

Add to `ws_client.rs`:

```rust
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{info, warn};
use std::time::Duration;

/// Connect to the dashboard WebSocket and forward parsed events.
/// Reconnects with exponential backoff on disconnect.
pub async fn run_ws_client(
    dashboard_url: String,
    tx: mpsc::Sender<WsEvent>,
) {
    let ws_url = dashboard_url
        .replace("http://", "ws://")
        .replace("https://", "wss://")
        + "/api/ws";

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);

    loop {
        info!(url = %ws_url, "Connecting to dashboard WebSocket");
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                backoff = Duration::from_secs(1); // reset on successful connect
                info!("WebSocket connected");

                let (_, mut read) = ws_stream.split();
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Some(evt) = parse_ws_message(&text) {
                                if tx.send(evt).await.is_err() {
                                    return; // receiver dropped, shut down
                                }
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                        Err(e) => {
                            warn!(error = %e, "WebSocket error");
                            break;
                        }
                        _ => {} // ignore binary/ping/pong
                    }
                }
                info!("WebSocket disconnected");
            }
            Err(e) => {
                warn!(error = %e, backoff_secs = backoff.as_secs(), "WebSocket connection failed");
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
```

- [ ] **Step 5: Declare the module**

In `crates/devbridge-app/src/main.rs`, add:

```rust
mod ws_client;
```

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-app/src/ws_client.rs crates/devbridge-app/Cargo.toml \
  crates/devbridge-app/src/main.rs
git commit -m "Add WebSocket client module for real-time tray events"
```

---

## Task 4: Add Job Tracker Module

**Files:**
- Create: `crates/devbridge-app/src/job_tracker.rs`

- [ ] **Step 1: Write failing test for icon state machine**

Create `crates/devbridge-app/src/job_tracker.rs`:

```rust
use devbridge_core::job_event::PrintStage;
use std::collections::VecDeque;

const MAX_RECENT_JOBS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Green,  // idle, OK
    Yellow, // printing in progress
    Red,    // last job failed
    Gray,   // service offline
}

#[derive(Debug, Clone)]
pub struct RecentJob {
    pub job_id: String,
    pub document_name: String,
    pub status: JobDisplayStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobDisplayStatus {
    InProgress,
    Completed,
    Failed,
}

pub struct JobTracker {
    pub recent_jobs: VecDeque<RecentJob>,
    pub icon_state: IconState,
    filter_user: Option<String>,
}

impl JobTracker {
    pub fn new(filter_user: Option<String>) -> Self {
        Self {
            recent_jobs: VecDeque::new(),
            icon_state: IconState::Gray, // start offline until first status check
            filter_user,
        }
    }

    /// Returns true if this event should be processed (passes user filter).
    pub fn should_process(&self, requesting_user: &Option<String>) -> bool {
        match &self.filter_user {
            None => true, // client mode: show all
            Some(filter) => requesting_user
                .as_ref()
                .map_or(false, |u| u.eq_ignore_ascii_case(filter)),
        }
    }

    /// Update tracker when a new job is created.
    pub fn on_job_created(&mut self, job_id: String, document_name: String) {
        let job = RecentJob {
            job_id,
            document_name,
            status: JobDisplayStatus::InProgress,
            timestamp: chrono::Utc::now(),
        };
        self.recent_jobs.push_front(job);
        while self.recent_jobs.len() > MAX_RECENT_JOBS {
            self.recent_jobs.pop_back();
        }
        self.icon_state = IconState::Yellow;
    }

    /// Update tracker when a print stage event arrives.
    pub fn on_print_event(&mut self, job_id: &str, stage: PrintStage, success: bool) {
        // Update existing job status
        if let Some(job) = self.recent_jobs.iter_mut().find(|j| j.job_id == job_id) {
            match stage {
                PrintStage::Completed => {
                    job.status = JobDisplayStatus::Completed;
                    self.icon_state = IconState::Green;
                }
                PrintStage::Failed => {
                    job.status = JobDisplayStatus::Failed;
                    self.icon_state = IconState::Red;
                }
                _ if !success => {
                    job.status = JobDisplayStatus::Failed;
                    self.icon_state = IconState::Red;
                }
                _ => {} // intermediate stages keep Yellow
            }
        }
    }

    /// Set icon to green (connected) or gray (disconnected).
    pub fn set_online(&mut self, online: bool) {
        if !online {
            self.icon_state = IconState::Gray;
        } else if self.icon_state == IconState::Gray {
            self.icon_state = IconState::Green;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_starts_gray() {
        let tracker = JobTracker::new(None);
        assert_eq!(tracker.icon_state, IconState::Gray);
        assert!(tracker.recent_jobs.is_empty());
    }

    #[test]
    fn job_created_sets_yellow() {
        let mut tracker = JobTracker::new(None);
        tracker.on_job_created("j1".into(), "test.pdf".into());
        assert_eq!(tracker.icon_state, IconState::Yellow);
        assert_eq!(tracker.recent_jobs.len(), 1);
        assert_eq!(tracker.recent_jobs[0].document_name, "test.pdf");
    }

    #[test]
    fn job_completed_sets_green() {
        let mut tracker = JobTracker::new(None);
        tracker.on_job_created("j1".into(), "test.pdf".into());
        tracker.on_print_event("j1", PrintStage::Completed, true);
        assert_eq!(tracker.icon_state, IconState::Green);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Completed);
    }

    #[test]
    fn job_failed_sets_red() {
        let mut tracker = JobTracker::new(None);
        tracker.on_job_created("j1".into(), "test.pdf".into());
        tracker.on_print_event("j1", PrintStage::Failed, false);
        assert_eq!(tracker.icon_state, IconState::Red);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Failed);
    }

    #[test]
    fn max_5_recent_jobs() {
        let mut tracker = JobTracker::new(None);
        for i in 0..7 {
            tracker.on_job_created(format!("j{i}"), format!("doc{i}.pdf"));
        }
        assert_eq!(tracker.recent_jobs.len(), 5);
        assert_eq!(tracker.recent_jobs[0].job_id, "j6"); // most recent first
    }

    #[test]
    fn user_filter_matches_case_insensitive() {
        let tracker = JobTracker::new(Some("Alice".into()));
        assert!(tracker.should_process(&Some("alice".into())));
        assert!(tracker.should_process(&Some("ALICE".into())));
        assert!(!tracker.should_process(&Some("bob".into())));
        assert!(!tracker.should_process(&None));
    }

    #[test]
    fn no_filter_passes_all() {
        let tracker = JobTracker::new(None);
        assert!(tracker.should_process(&Some("anyone".into())));
        assert!(tracker.should_process(&None));
    }

    #[test]
    fn set_online_transitions() {
        let mut tracker = JobTracker::new(None);
        assert_eq!(tracker.icon_state, IconState::Gray);
        tracker.set_online(true);
        assert_eq!(tracker.icon_state, IconState::Green);
        tracker.set_online(false);
        assert_eq!(tracker.icon_state, IconState::Gray);
    }

    #[test]
    fn set_online_preserves_red() {
        let mut tracker = JobTracker::new(None);
        tracker.set_online(true);
        tracker.on_job_created("j1".into(), "test.pdf".into());
        tracker.on_print_event("j1", PrintStage::Failed, false);
        assert_eq!(tracker.icon_state, IconState::Red);
        tracker.set_online(true); // should NOT reset to green
        assert_eq!(tracker.icon_state, IconState::Red);
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p devbridge-app -- job_tracker`
Expected: All 8 tests PASS

- [ ] **Step 3: Declare the module**

In `crates/devbridge-app/src/main.rs`, add:

```rust
mod job_tracker;
```

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-app/src/job_tracker.rs crates/devbridge-app/src/main.rs
git commit -m "Add job tracker with icon state machine and user filtering"
```

---

## Task 5: Integrate Tray Menu with Job History and Notifications

**Files:**
- Modify: `crates/devbridge-app/src/tray.rs`
- Modify: `crates/devbridge-app/src/main.rs`
- Modify: `crates/devbridge-app/capabilities/default.json`

This is the integration task where everything comes together.

- [ ] **Step 1: Add notification permission to Tauri capabilities**

In `crates/devbridge-app/capabilities/default.json`, update:

```json
{
  "identifier": "default",
  "description": "Default capabilities for DevBridge tray app",
  "windows": ["*"],
  "permissions": ["notification:default"]
}
```

- [ ] **Step 2: Rewrite tray.rs with job history and notifications**

Replace the entire `crates/devbridge-app/src/tray.rs` with the new implementation. Key changes:

1. **Menu structure**: "Recent Jobs" section (5 items) + "Open Dashboard" + status line + Start/Stop/Quit
2. **`rebuild_menu()`**: Takes the `JobTracker` state and builds the menu dynamically
3. **`update_icon()`**: Switches tray icon based on `IconState`
4. **`show_notification()`**: Shows balloon via `tauri-plugin-notification`
5. **Event loop**: Receives `WsEvent` from mpsc channel, updates tracker, rebuilds menu, shows notification, updates icon

```rust
use crate::job_tracker::{IconState, JobDisplayStatus, JobTracker};
use crate::ws_client::{self, WsEvent};
use devbridge_core::job::JobEvent;
use devbridge_core::job_event::PrintStage;
use std::sync::Arc;
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

pub fn setup_tray(app: &tauri::App, dashboard_port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let dashboard_url = format!("http://127.0.0.1:{dashboard_port}");

    // Detect if server mode and get username for filtering
    let filter_user = detect_filter_user(&dashboard_url);

    let tracker = Arc::new(Mutex::new(JobTracker::new(filter_user)));

    // Build initial menu
    let menu = build_menu(app.handle(), &JobTracker::new(None))?;

    let _tray = TrayIconBuilder::new()
        .icon(Image::from_path("assets/icons/tray-icon-gray.png").unwrap_or_else(|_| {
            app.default_window_icon().cloned().expect("no icon")
        }))
        .menu(&menu)
        .tooltip("DevBridge")
        .on_menu_event({
            let dashboard_url = dashboard_url.clone();
            let app_handle = app.handle().clone();
            move |_tray, event| {
                handle_menu_event(&app_handle, &dashboard_url, event.id().as_ref());
            }
        })
        .build(app)?;

    // Spawn WebSocket client and event processing loop
    let app_handle = app.handle().clone();
    let tracker_clone = tracker.clone();
    let dashboard_url_clone = dashboard_url.clone();

    tauri::async_runtime::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<WsEvent>(64);

        // Spawn WS client in background
        let ws_url = dashboard_url_clone.clone();
        tauri::async_runtime::spawn(async move {
            ws_client::run_ws_client(ws_url, tx).await;
        });

        // Fetch initial jobs
        fetch_initial_jobs(&dashboard_url_clone, &tracker_clone).await;

        // Mark as online after initial fetch
        {
            let mut t = tracker_clone.lock().await;
            t.set_online(true);
        }
        update_tray(&app_handle, &tracker_clone).await;

        // Process events from WebSocket
        while let Some(event) = rx.recv().await {
            let mut t = tracker_clone.lock().await;
            match &event {
                WsEvent::Job(JobEvent::Created { job_id, document_name, requesting_user }) => {
                    if t.should_process(requesting_user) {
                        t.on_job_created(job_id.clone(), document_name.clone());
                        drop(t);
                        show_notification(&app_handle, "Print Job Received", document_name);
                        update_tray(&app_handle, &tracker_clone).await;
                    }
                }
                WsEvent::Job(JobEvent::StateChanged { .. }) => {
                    // State changes are also reflected via PrintJobEvent
                }
                WsEvent::Print(evt) => {
                    if t.should_process(&evt.requesting_user) {
                        t.on_print_event(&evt.job_id, evt.stage, evt.success);
                        let msg = match evt.stage {
                            PrintStage::Completed => format!("Printed: {} ✓", evt.detail),
                            PrintStage::Failed => format!("Failed: {}", evt.detail),
                            PrintStage::Sending => format!("Printing: {}", evt.detail),
                            _ => String::new(),
                        };
                        drop(t);
                        if !msg.is_empty() {
                            show_notification(&app_handle, "DevBridge", &msg);
                        }
                        update_tray(&app_handle, &tracker_clone).await;
                    }
                }
            }
        }
    });

    // Status polling (for connection health)
    let tracker_poll = tracker.clone();
    let app_poll = app.handle().clone();
    let poll_url = dashboard_url.clone();
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let online = client
                .get(format!("{poll_url}/api/status"))
                .send()
                .await
                .is_ok();
            let mut t = tracker_poll.lock().await;
            t.set_online(online);
            drop(t);
            update_tray(&app_poll, &tracker_poll).await;
        }
    });

    Ok(())
}

fn detect_filter_user(dashboard_url: &str) -> Option<String> {
    // Blocking check at startup — if server mode, filter by current user
    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?
        .get(format!("{dashboard_url}/api/status"))
        .send()
        .ok()?
        .json::<serde_json::Value>()
        .ok()?;

    if resp.get("mode")?.as_str()? == "server" {
        // On server (terminal server), filter by Windows username
        std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .ok()
    } else {
        None // Client mode: show all jobs
    }
}

async fn fetch_initial_jobs(
    dashboard_url: &str,
    tracker: &Arc<Mutex<JobTracker>>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let url = format!("{dashboard_url}/api/jobs?limit=5");
    if let Ok(resp) = client.get(&url).send().await {
        if let Ok(jobs) = resp.json::<Vec<serde_json::Value>>().await {
            let mut t = tracker.lock().await;
            // Jobs come newest first from API — add in reverse so oldest is pushed first
            for job in jobs.iter().rev() {
                let job_id = job["id"].as_str().unwrap_or("").to_string();
                let name = job["name"].as_str().unwrap_or("unknown").to_string();
                let status = job["status"].as_str().unwrap_or("queued");
                t.on_job_created(job_id, name);
                // Update status based on current state
                if let Some(recent) = t.recent_jobs.front_mut() {
                    recent.status = match status {
                        "completed" => JobDisplayStatus::Completed,
                        "failed" | "cancelled" => JobDisplayStatus::Failed,
                        _ => JobDisplayStatus::InProgress,
                    };
                }
            }
            // Set icon based on most recent job
            if let Some(front) = t.recent_jobs.front() {
                t.icon_state = match front.status {
                    JobDisplayStatus::Completed => IconState::Green,
                    JobDisplayStatus::Failed => IconState::Red,
                    JobDisplayStatus::InProgress => IconState::Yellow,
                };
            }
        }
    }
}

fn build_menu(
    app: &AppHandle,
    tracker: &JobTracker,
) -> Result<tauri::menu::Menu<tauri::Wry>, Box<dyn std::error::Error>> {
    let mut builder = MenuBuilder::new(app);

    // Recent jobs section
    if !tracker.recent_jobs.is_empty() {
        let header = MenuItemBuilder::with_id("header_jobs", "Recent Jobs")
            .enabled(false)
            .build(app)?;
        builder = builder.item(&header);

        for job in &tracker.recent_jobs {
            let icon = match job.status {
                JobDisplayStatus::Completed => "✓",
                JobDisplayStatus::Failed => "✗",
                JobDisplayStatus::InProgress => "⏳",
            };
            let age = format_age(job.timestamp);
            let label = format!("{icon} {}  {age}", job.document_name);
            let item = MenuItemBuilder::with_id(format!("job_{}", job.job_id), label)
                .enabled(false)
                .build(app)?;
            builder = builder.item(&item);
        }
        builder = builder.separator();
    }

    // Open Dashboard
    let open = MenuItemBuilder::with_id("open_dashboard", "Open Dashboard").build(app)?;
    builder = builder.item(&open);
    builder = builder.separator();

    // Status line
    let status_text = match tracker.icon_state {
        IconState::Green => "● Online",
        IconState::Yellow => "◐ Printing...",
        IconState::Red => "● Error",
        IconState::Gray => "○ Offline",
    };
    let status = MenuItemBuilder::with_id("status", status_text)
        .enabled(false)
        .build(app)?;
    builder = builder.item(&status);

    // Start/Stop
    let start = MenuItemBuilder::with_id("start_service", "Start Service").build(app)?;
    let stop = MenuItemBuilder::with_id("stop_service", "Stop Service").build(app)?;
    builder = builder.item(&start).item(&stop);
    builder = builder.separator();

    // Quit
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    builder = builder.item(&quit);

    Ok(builder.build()?)
}

async fn update_tray(app: &AppHandle, tracker: &Arc<Mutex<JobTracker>>) {
    let t = tracker.lock().await;

    // Update icon
    let icon_path = match t.icon_state {
        IconState::Green => "assets/icons/tray-icon-green.png",
        IconState::Yellow => "assets/icons/tray-icon-yellow.png",
        IconState::Red => "assets/icons/tray-icon-red.png",
        IconState::Gray => "assets/icons/tray-icon-gray.png",
    };
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(icon) = Image::from_path(icon_path) {
            let _ = tray.set_icon(Some(icon));
        }
        // Rebuild menu
        if let Ok(menu) = build_menu(app, &t) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn show_notification(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

fn handle_menu_event(app: &AppHandle, dashboard_url: &str, event_id: &str) {
    match event_id {
        "open_dashboard" => {
            let _ = open::that(dashboard_url);
        }
        "start_service" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::ipc_client::send_request(
                    &devbridge_core::ipc::IpcRequest::StartService,
                ).await {
                    warn!(error = %e, "Failed to start service");
                }
            });
        }
        "stop_service" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::ipc_client::send_request(
                    &devbridge_core::ipc::IpcRequest::StopService,
                ).await {
                    warn!(error = %e, "Failed to stop service");
                }
            });
        }
        "quit" => {
            std::process::exit(0);
        }
        _ => {}
    }
}

fn format_age(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now() - timestamp;
    let secs = elapsed.num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
```

- [ ] **Step 3: Update main.rs to register notification plugin**

In `crates/devbridge-app/src/main.rs`, in the Tauri builder chain, add:

```rust
.plugin(tauri_plugin_notification::init())
```

Before `.setup(...)`.

- [ ] **Step 4: Run all workspace tests**

Run: `cargo test --workspace`
Expected: All pass. Fix any compilation errors.

- [ ] **Step 5: Run format check**

Run: `cargo fmt --all --check`
Fix any formatting issues.

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-app/src/tray.rs crates/devbridge-app/src/main.rs \
  crates/devbridge-app/capabilities/default.json
git commit -m "Integrate tray menu with job history, notifications, and icon states"
```

---

## Task 6: Dashboard API Test for requesting_user Filter

**Files:**
- Modify: `crates/devbridge-dashboard/` test files

- [ ] **Step 1: Write test for `/api/jobs?requesting_user=` filter**

In the dashboard tests (find existing test location or create `crates/devbridge-dashboard/tests/api_jobs_test.rs`):

```rust
#[tokio::test]
async fn test_jobs_filtered_by_requesting_user() {
    // Set up test AppState with a real Storage (temp db)
    // Insert two jobs: one with requesting_user="alice", one with "bob"
    // GET /api/jobs?requesting_user=alice
    // Assert only alice's job is returned
    // GET /api/jobs (no filter)
    // Assert both jobs returned
}
```

The exact implementation depends on existing test infrastructure. Follow the pattern of existing dashboard tests.

- [ ] **Step 2: Run tests**

Run: `cargo test -p devbridge-dashboard`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/devbridge-dashboard/
git commit -m "Add test for requesting_user job filter"
```

---

## Task 7: Playwright E2E Test for requesting_user in Dashboard

**Files:**
- Modify: `playwright/tests/jobs.spec.ts`

- [ ] **Step 1: Add test for requesting_user display**

In `playwright/tests/jobs.spec.ts`, add a test that verifies the dashboard UI shows `requesting_user` when available. This will require the dashboard UI (`devbridge-ui`) to display the field.

Note: If the WASM UI doesn't yet display `requesting_user`, this test documents the expected behavior. The UI update to display the field should be added to `crates/devbridge-ui/src/pages/jobs.rs` (or equivalent Leptos component).

```typescript
test('shows requesting user in job details', async ({ page }) => {
  const console = attachConsoleCollector(page);
  await page.goto('/jobs');

  // The API now returns requesting_user field
  // Verify the table or job details show this information
  // This test may need updating after the UI is modified to display the field

  assertCleanConsole(console);
});
```

- [ ] **Step 2: Commit**

```bash
git add playwright/tests/jobs.spec.ts
git commit -m "Add Playwright test for requesting_user in jobs page"
```

---

## Task 8: Push, Monitor CI, Create PR

- [ ] **Step 1: Run local format check**

```bash
cargo fmt --all --check
```

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI until all jobs complete**

```bash
gh run list --branch dev --limit 3
gh run view <run-id>
```

Wait for ALL jobs to reach terminal state. If any fail, investigate with `gh run view <run-id> --log-failed`, fix, and push again.

- [ ] **Step 4: If mutation testing fails — kill survivors**

Download mutation results and write tests to kill any surviving mutants in the new code:

```bash
gh run download <run-id> --name mutation-results --dir /tmp/mutation-results
cat /tmp/mutation-results/survived.txt
```

- [ ] **Step 5: Create PR**

```bash
gh pr create --title "Add tray app notifications and job tracking (#11)" --body "$(cat <<'EOF'
## Summary
- Capture IPP `requesting-user-name` and store in jobs database
- Tray app connects via WebSocket for instant notifications
- Balloon notifications for job received/completed/failed
- Last 5 jobs shown in tray menu with status icons
- Tray icon changes: green (OK), yellow (printing), red (error), gray (offline)
- Per-user filtering on terminal server (RDP users see only their own jobs)

## Test plan
- [ ] Unit tests for job tracker state machine (8 tests)
- [ ] Unit tests for WebSocket event parsing
- [ ] Integration test for requesting_user storage round-trip
- [ ] API test for requesting_user filter
- [ ] Playwright test for requesting_user in dashboard
- [ ] Mutation testing passes
- [ ] Manual verification: balloon notifications on client machines

Closes #11

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 6: Wait for PR CI, verify mergeable**

```bash
gh api repos/zbynekdrlik/devbridge/pulls/<PR_NUMBER> --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```

- [ ] **Step 7: Report PR URL and wait for merge approval**

---

## Verification

After merge and deploy:

1. **pz-server (terminal server):** Open RDP as a test user, print a document, verify tray balloon appears showing the user's job. Verify a different RDP user does NOT see the first user's notification.
2. **pz-snv:** Print from server to pz-snv virtual printer, verify balloon appears on pz-snv tray showing "Receiving: ..." → "Printed: ... ✓". Verify tray icon goes yellow then green.
3. **pz-holla:** Same verification as pz-snv.
4. **Dashboard API:** `GET /api/jobs` returns `requesting_user` field. `GET /api/jobs?requesting_user=alice` filters correctly.
5. **Error case:** Cause a print failure (e.g., turn off printer), verify red icon and failure balloon.
