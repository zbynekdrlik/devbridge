# Tray RDP Filter Leak Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix issue #42 — replace ambiguous `Option<String>` filter state in the tray app with a typed `FilterState` enum that fails closed on detection failure, and replace the one-shot `detect_filter_user` call with a dedicated retry loop.

**Architecture:** `FilterState::{Pending, Disabled, User(String)}` replaces `Option<String>`. `JobTracker` initializes to `Pending` and drops all events while in that state. A separate `detect_filter_loop` task polls `/api/status` with exponential backoff (1s → 30s cap) until it can transition the tracker to `Disabled` (client mode) or `User(USERNAME)` (server mode). `fetch_initial_jobs` waits on a `tokio::sync::watch` channel that the detector publishes to, eliminating the secondary leak through the Recent Jobs menu.

**Tech Stack:** Rust 2024, tokio (with test-util for paused-time backoff tests), reqwest, tauri.

**Spec:** `docs/superpowers/specs/2026-04-27-tray-rdp-filter-leak-design.md`

---

## File Structure

### Modified Files

| File | Responsibility |
|---|---|
| `Cargo.toml` | Workspace version bump 0.8.20 → 0.8.21 |
| `crates/devbridge-app/Cargo.toml` | Add `[dev-dependencies]` with tokio `test-util` feature |
| `crates/devbridge-app/src/job_tracker.rs` | Define `FilterState` enum; replace `filter_user: Option<String>` with `filter_tx: watch::Sender<FilterState>`; expose `filter_subscribe()` for waiters; rewrite `should_process` against the new state; migrate all existing tests; add Pending-state regression tests |
| `crates/devbridge-app/src/tray.rs` | Define `StatusFetcher` closure type and `HttpStatusFetcher`; replace `detect_filter_user` with `detect_filter_loop`; spawn the detector in `setup_tray`; gate `fetch_initial_jobs` on `watch::Receiver::wait_for` |

No new files. No changes to `ws_client.rs`, `ipc_client.rs`, dashboard API, or wire protocol.

---

## Task 1: Bump version + enable test-util in devbridge-app dev-deps

**Files:**
- Modify: `Cargo.toml:15`
- Modify: `crates/devbridge-app/Cargo.toml` (append)

- [ ] **Step 1: Bump workspace version**

Edit `Cargo.toml:15` from:
```toml
version = "0.8.20"
```
to:
```toml
version = "0.8.21"
```

- [ ] **Step 2: Add tokio test-util to devbridge-app dev-dependencies**

Append to `crates/devbridge-app/Cargo.toml`:
```toml

[dev-dependencies]
# test-util provides tokio::time::pause / advance for deterministic
# backoff tests in detect_filter_loop.
tokio = { version = "1", features = ["full", "test-util"] }
```

- [ ] **Step 3: Format check**

Run: `cargo fmt --all --check`
Expected: passes silently.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/devbridge-app/Cargo.toml
git commit -m "0.8.21: bump version + enable tokio test-util for tray tests (#42)"
```

---

## Task 2: Define `FilterState` enum + Pending regression tests

**Files:**
- Modify: `crates/devbridge-app/src/job_tracker.rs`

- [ ] **Step 1: Write the failing tests**

Replace the `user_filter_*` and `no_filter_passes_all` tests in the `tests` module of `crates/devbridge-app/src/job_tracker.rs` with the new test suite. Add this block at the end of the `mod tests` block (other existing tests will be migrated in Task 4):

```rust
    // -- FilterState tests (issue #42) ---------------------------------

    #[test]
    fn pending_drops_all_events_with_user() {
        let tracker = JobTracker::new();
        // Default state is Pending — must drop everything until detection completes.
        assert!(!tracker.should_process(&Some("anyone".into())));
    }

    #[test]
    fn pending_drops_all_events_without_user() {
        let tracker = JobTracker::new();
        assert!(!tracker.should_process(&None));
    }

    #[test]
    fn disabled_passes_all_events() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::Disabled);
        assert!(tracker.should_process(&Some("alice".into())));
        assert!(tracker.should_process(&None));
    }

    #[test]
    fn user_state_matches_case_insensitive() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::User("Admin".into()));
        assert!(tracker.should_process(&Some("admin".into())));
        assert!(tracker.should_process(&Some("ADMIN".into())));
        assert!(tracker.should_process(&Some("Admin".into())));
        assert!(!tracker.should_process(&Some("other_user".into())));
    }

    #[test]
    fn user_state_drops_none_requesting_user() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::User("admin".into()));
        assert!(!tracker.should_process(&None));
    }

    #[test]
    fn filter_subscribe_yields_current_state() {
        let mut tracker = JobTracker::new();
        let mut rx = tracker.filter_subscribe();
        assert!(matches!(*rx.borrow(), FilterState::Pending));
        tracker.set_filter_state(FilterState::Disabled);
        // Watch channel borrow_and_update returns the latest sent value.
        rx.borrow_and_update();
        assert!(matches!(*rx.borrow(), FilterState::Disabled));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-app job_tracker::tests::pending_drops_all_events_with_user --no-run`
Expected: FAIL with "cannot find type `FilterState`" / "no method named `set_filter_state`".

- [ ] **Step 3: Implement `FilterState` enum + new JobTracker fields**

Replace the top of `crates/devbridge-app/src/job_tracker.rs` (lines 1-46, through the end of `JobTracker::new`) with:

```rust
use devbridge_core::job::JobState;
use devbridge_core::job_event::PrintStage;
use std::collections::VecDeque;
use tokio::sync::watch;

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
    pub target_printer: String,
    pub status: JobDisplayStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobDisplayStatus {
    InProgress,
    Completed,
    Failed,
}

/// Per-event filter for the tray. The Pending default is critical:
/// on a server-mode tray, we MUST drop all events until detection
/// proves who this RDP session belongs to. Returning the wrong answer
/// here leaks other users' print notifications (issue #42).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterState {
    /// Detection has not yet succeeded. Drop ALL events. Default at startup.
    Pending,
    /// Client mode confirmed — no filtering, pass all events.
    Disabled,
    /// Server mode confirmed — pass only events from this user.
    User(String),
}

pub struct JobTracker {
    pub recent_jobs: VecDeque<RecentJob>,
    pub icon_state: IconState,
    filter_tx: watch::Sender<FilterState>,
}

impl JobTracker {
    /// Create a new tracker. Starts with Gray icon, empty job list,
    /// and `FilterState::Pending` (drops all events until set otherwise).
    pub fn new() -> Self {
        let (filter_tx, _rx) = watch::channel(FilterState::Pending);
        Self {
            recent_jobs: VecDeque::new(),
            icon_state: IconState::Gray,
            filter_tx,
        }
    }

    /// Subscribe to filter state changes. Used by `fetch_initial_jobs`
    /// to wait until detection completes before querying the API.
    pub fn filter_subscribe(&self) -> watch::Receiver<FilterState> {
        self.filter_tx.subscribe()
    }

    /// Read the current filter state (cheap clone).
    pub fn filter_state(&self) -> FilterState {
        self.filter_tx.borrow().clone()
    }

    /// Set the filter state from the detector loop. Notifies all subscribers.
    pub fn set_filter_state(&mut self, state: FilterState) {
        // send() returns Err only if all receivers dropped — harmless here.
        let _ = self.filter_tx.send(state);
    }
```

Then locate the existing `should_process` method (around line 61 in the old file) and REPLACE it with:

```rust
    /// Returns true if the event passes the filter.
    /// `Pending` drops everything (fail-closed). `Disabled` passes everything.
    /// `User(u)` passes only events from `u` (case-insensitive).
    pub fn should_process(&self, requesting_user: &Option<String>) -> bool {
        match &*self.filter_tx.borrow() {
            FilterState::Pending => false,
            FilterState::Disabled => true,
            FilterState::User(filter) => match requesting_user {
                None => false,
                Some(user) => user.eq_ignore_ascii_case(filter),
            },
        }
    }
```

DELETE the now-obsolete methods:
- `pub fn filter_user(&self) -> Option<&String>` (line ~49)
- `pub fn set_filter_user(&mut self, user: Option<String>)` (line ~54)

- [ ] **Step 4: Run the new tests**

Run: `cargo test -p devbridge-app job_tracker::tests::pending_drops_all_events_with_user job_tracker::tests::pending_drops_all_events_without_user job_tracker::tests::disabled_passes_all_events job_tracker::tests::user_state_matches_case_insensitive job_tracker::tests::user_state_drops_none_requesting_user job_tracker::tests::filter_subscribe_yields_current_state`
Expected: 6 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-app/src/job_tracker.rs
git commit -m "JobTracker: introduce FilterState enum (Pending/Disabled/User) (#42)"
```

---

## Task 3: Migrate existing JobTracker tests + callers to FilterState

**Files:**
- Modify: `crates/devbridge-app/src/job_tracker.rs` (existing tests in `mod tests`)

- [ ] **Step 1: Migrate every test that called `JobTracker::new(None)` or `JobTracker::new(Some(_))`**

For each existing test in `crates/devbridge-app/src/job_tracker.rs`'s `mod tests` block, change:

```rust
let tracker = JobTracker::new(None);
let mut tracker = JobTracker::new(None);
```
to:
```rust
let tracker = JobTracker::new();
let mut tracker = JobTracker::new();
```

For tests that previously created with a filter user:
```rust
let tracker = JobTracker::new(Some("Admin".into()));
```
becomes:
```rust
let mut tracker = JobTracker::new();
tracker.set_filter_state(FilterState::User("Admin".into()));
```

The complete list of tests requiring migration (by name in the existing file): `new_tracker_starts_gray`, `job_created_sets_yellow`, `state_changed_to_completed_marks_done`, `state_changed_to_failed_marks_failed`, `state_changed_intermediate_returns_none`, `full_lifecycle_created_printing_completed`, `full_lifecycle_created_failed`, `job_completed_sets_green`, `job_failed_sets_red`, `max_5_recent_jobs`, `set_online_preserves_red`, `set_online_transitions`. The old `user_filter_matches_case_insensitive` and `no_filter_passes_all` tests are REPLACED by the new tests added in Task 2 — delete them.

- [ ] **Step 2: Update `tray.rs` callers of the removed methods**

In `crates/devbridge-app/src/tray.rs`, line ~31:
```rust
let tracker = Arc::new(Mutex::new(JobTracker::new(None)));
```
becomes:
```rust
let tracker = Arc::new(Mutex::new(JobTracker::new()));
```

Line ~34:
```rust
let initial_tracker = JobTracker::new(None);
```
becomes:
```rust
let initial_tracker = JobTracker::new();
```

Lines ~70-75 (the `detect_filter_user` call site) — leave the structure for now; Task 5 rewrites this whole block. Just change line ~74 from:
```rust
t.set_filter_user(filter_user);
```
to:
```rust
t.set_filter_state(filter_user);
```
(`filter_user` will become `FilterState` in Task 5, but for now temporarily change `detect_filter_user` to return `FilterState`. To keep the build green, replace the body of `detect_filter_user` with this stub — it will be deleted in Task 5):

```rust
async fn detect_filter_user(_dashboard_url: &str) -> FilterState {
    // Placeholder — Task 5 replaces this with detect_filter_loop.
    FilterState::Pending
}
```

Add `use crate::job_tracker::{IconState, JobDisplayStatus, JobTracker, FilterState};` at the top of `tray.rs` (extend the existing import line).

Lines ~218-221 (the `filter_user` lookup in `fetch_initial_jobs`):
```rust
let filter_user = {
    let t = tracker.lock().await;
    t.filter_user().cloned()
};
let url = match &filter_user {
    Some(u) => format!("{dashboard_url}/api/jobs?limit=5&requesting_user={u}"),
    None => format!("{dashboard_url}/api/jobs?limit=5"),
};
```
becomes (temporary — Task 6 rewrites this):
```rust
let filter_state = {
    let t = tracker.lock().await;
    t.filter_state()
};
let url = match &filter_state {
    FilterState::User(u) => {
        format!("{dashboard_url}/api/jobs?limit=5&requesting_user={u}")
    }
    FilterState::Disabled | FilterState::Pending => {
        format!("{dashboard_url}/api/jobs?limit=5")
    }
};
```

- [ ] **Step 3: Run all tests in devbridge-app**

Run: `cargo test -p devbridge-app`
Expected: all tests pass (existing ones migrated, new ones from Task 2 still passing).

- [ ] **Step 4: Format check**

Run: `cargo fmt --all --check`
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-app/src/job_tracker.rs crates/devbridge-app/src/tray.rs
git commit -m "JobTracker: migrate existing tests + tray callers to FilterState (#42)"
```

---

## Task 4: Define `StatusFetcher` abstraction + extract HTTP impl

**Files:**
- Modify: `crates/devbridge-app/src/tray.rs`

- [ ] **Step 1: Add `StatusFetcher` trait + `HttpStatusFetcher` near the bottom of `tray.rs`**

Insert this block immediately AFTER the `detect_filter_user` stub (or wherever convenient — before the `format_age` function works):

```rust
/// Outcome of one /api/status fetch. Variants distinguish:
/// - Ok(mode): service responded; `mode` is the `mode` field of the JSON
/// - Err(_): HTTP error, JSON parse error, or missing `mode` field
#[derive(Debug)]
pub enum FetchError {
    Http(String),
    InvalidJson,
    MissingMode,
}

/// Trait used so tests can substitute a queue-of-responses mock for the
/// real HTTP fetcher (avoids spinning up a real server in tests).
#[async_trait::async_trait]
pub trait StatusFetcher: Send + Sync {
    async fn fetch_status(&self) -> Result<String, FetchError>;
}

pub struct HttpStatusFetcher {
    url: String,
    client: reqwest::Client,
}

impl HttpStatusFetcher {
    pub fn new(dashboard_url: &str) -> Self {
        let url = format!("{dashboard_url}/api/status");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build");
        Self { url, client }
    }
}

#[async_trait::async_trait]
impl StatusFetcher for HttpStatusFetcher {
    async fn fetch_status(&self) -> Result<String, FetchError> {
        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| FetchError::Http(e.to_string()))?;
        let json: serde_json::Value =
            resp.json().await.map_err(|_| FetchError::InvalidJson)?;
        json["mode"]
            .as_str()
            .map(String::from)
            .ok_or(FetchError::MissingMode)
    }
}
```

- [ ] **Step 2: Add `async-trait` to devbridge-app `[dependencies]`**

Append to the `[dependencies]` block of `crates/devbridge-app/Cargo.toml`:

```toml
async-trait = "0.1"
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p devbridge-app`
Expected: clean build (only the unused-`StatusFetcher`-trait dead-code warning if any — fine, used in next task).

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-app/Cargo.toml crates/devbridge-app/src/tray.rs
git commit -m "tray: add StatusFetcher trait + HttpStatusFetcher impl (#42)"
```

---

## Task 5: Implement `detect_filter_loop` with paused-time tests

**Files:**
- Modify: `crates/devbridge-app/src/tray.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/devbridge-app/src/tray.rs` at the bottom (after any existing tests, or create a `#[cfg(test)] mod tests` block if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_tracker::{FilterState, JobTracker};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::collections::VecDeque;
    use tokio::sync::Mutex as TokioMutex;

    /// Mock fetcher that returns queued responses one at a time.
    struct MockFetcher {
        responses: StdMutex<VecDeque<Result<String, FetchError>>>,
    }

    impl MockFetcher {
        fn new(responses: Vec<Result<String, FetchError>>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl StatusFetcher for MockFetcher {
        async fn fetch_status(&self) -> Result<String, FetchError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(FetchError::Http("no more responses".into())))
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn detect_filter_loop_handles_client_mode() {
        let tracker = Arc::new(TokioMutex::new(JobTracker::new()));
        let fetcher = MockFetcher::new(vec![Ok("client".to_string())]);
        detect_filter_loop(tracker.clone(), Arc::new(fetcher)).await;
        let state = tracker.lock().await.filter_state();
        assert!(matches!(state, FilterState::Disabled));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn detect_filter_loop_handles_server_mode_with_username() {
        // SAFETY: tests in this module run single-threaded (current_thread)
        // so set_var won't race with other threads.
        unsafe { std::env::set_var("USERNAME", "alice") };
        let tracker = Arc::new(TokioMutex::new(JobTracker::new()));
        let fetcher = MockFetcher::new(vec![Ok("server".to_string())]);
        detect_filter_loop(tracker.clone(), Arc::new(fetcher)).await;
        let state = tracker.lock().await.filter_state();
        assert!(
            matches!(state, FilterState::User(ref u) if u == "alice"),
            "got {state:?}"
        );
        unsafe { std::env::remove_var("USERNAME") };
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn detect_filter_loop_retries_after_http_error() {
        unsafe { std::env::set_var("USERNAME", "bob") };
        let tracker = Arc::new(TokioMutex::new(JobTracker::new()));
        // Three errors, then success — verifies retry loop.
        let fetcher = MockFetcher::new(vec![
            Err(FetchError::Http("connection refused".into())),
            Err(FetchError::Http("connection refused".into())),
            Err(FetchError::Http("connection refused".into())),
            Ok("server".to_string()),
        ]);
        detect_filter_loop(tracker.clone(), Arc::new(fetcher)).await;
        let state = tracker.lock().await.filter_state();
        assert!(
            matches!(state, FilterState::User(ref u) if u == "bob"),
            "got {state:?}"
        );
        unsafe { std::env::remove_var("USERNAME") };
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn detect_filter_loop_keeps_pending_when_username_missing_then_set() {
        // Simulate: server mode but USERNAME unset on first call,
        // then USERNAME appears on second call.
        unsafe { std::env::remove_var("USERNAME") };
        unsafe { std::env::remove_var("USER") };
        let tracker = Arc::new(TokioMutex::new(JobTracker::new()));
        let fetcher = MockFetcher::new(vec![
            Ok("server".to_string()), // first call: server mode but env unset → stay pending
            Ok("server".to_string()), // second call: env now set → transition to User
        ]);
        // Spawn the detector. Need to set USERNAME between iterations.
        // The first call will set USERNAME=missing path; we need to set
        // USERNAME after the first sleep starts so the second call sees it.
        // Trick: use a tokio::spawn + advance time.
        let tracker_clone = tracker.clone();
        let fetcher = Arc::new(fetcher);
        let handle = tokio::spawn(async move {
            detect_filter_loop(tracker_clone, fetcher).await;
        });
        // Yield once so the loop reaches its first sleep.
        tokio::task::yield_now().await;
        // Set USERNAME for the next iteration.
        unsafe { std::env::set_var("USERNAME", "carol") };
        // Advance enough to pass the first backoff (1s).
        tokio::time::advance(Duration::from_secs(2)).await;
        handle.await.unwrap();
        let state = tracker.lock().await.filter_state();
        assert!(
            matches!(state, FilterState::User(ref u) if u == "carol"),
            "got {state:?}"
        );
        unsafe { std::env::remove_var("USERNAME") };
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p devbridge-app tray::tests::detect_filter_loop_handles_client_mode --no-run`
Expected: FAIL with "cannot find function `detect_filter_loop`".

- [ ] **Step 3: Implement `detect_filter_loop`**

In `crates/devbridge-app/src/tray.rs`, REPLACE the entire `detect_filter_user` stub (the placeholder added in Task 3) with:

```rust
/// Initial backoff delay between failed `/api/status` fetches.
const DETECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Cap on the exponential backoff between retries.
const DETECT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Retry loop that polls `/api/status` until it can definitively transition
/// the tracker out of `FilterState::Pending`. Exits as soon as the tracker
/// reaches `Disabled` (client mode) or `User(_)` (server mode + USERNAME).
///
/// USERNAME and service mode are session-stable — once known, they don't
/// change for the lifetime of either the user's RDP session or the service
/// config. So we do NOT re-detect on WS reconnect.
pub async fn detect_filter_loop(
    tracker: Arc<Mutex<JobTracker>>,
    fetcher: Arc<dyn StatusFetcher>,
) {
    let mut backoff = DETECT_INITIAL_BACKOFF;
    loop {
        match fetcher.fetch_status().await {
            Ok(mode) if mode == "client" => {
                tracker
                    .lock()
                    .await
                    .set_filter_state(FilterState::Disabled);
                tracing::info!("Tray filter: client mode (no filtering)");
                return;
            }
            Ok(mode) if mode == "server" => {
                match std::env::var("USERNAME").or_else(|_| std::env::var("USER")) {
                    Ok(user) => {
                        tracker
                            .lock()
                            .await
                            .set_filter_state(FilterState::User(user.clone()));
                        tracing::info!("Tray filter: server mode, user={user}");
                        return;
                    }
                    Err(_) => {
                        tracing::error!(
                            "Server mode but USERNAME/USER env unset; staying Pending"
                        );
                    }
                }
            }
            Ok(mode) => {
                tracing::warn!("Unknown service mode: {mode}; staying Pending");
            }
            Err(e) => {
                tracing::warn!("Status fetch failed: {e:?}; retrying after {backoff:?}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(DETECT_MAX_BACKOFF);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p devbridge-app tray::tests`
Expected: 4 tests pass (`detect_filter_loop_handles_client_mode`, `detect_filter_loop_handles_server_mode_with_username`, `detect_filter_loop_retries_after_http_error`, `detect_filter_loop_keeps_pending_when_username_missing_then_set`).

If `detect_filter_loop_keeps_pending_when_username_missing_then_set` is flaky on the env-var race, simplify it to two SEPARATE tests:

```rust
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn detect_filter_loop_logs_when_username_missing() {
    unsafe { std::env::remove_var("USERNAME") };
    unsafe { std::env::remove_var("USER") };
    let tracker = Arc::new(TokioMutex::new(JobTracker::new()));
    // One server response then the loop will sleep + retry. To prevent infinite hang,
    // use a queue that returns one Ok then drives "no more responses" Err — combined
    // with a 100ms timeout that we await separately.
    let fetcher = MockFetcher::new(vec![Ok("server".to_string())]);
    let handle = tokio::spawn(detect_filter_loop(tracker.clone(), Arc::new(fetcher)));
    // Give the loop one tick to call fetch_status, fail USERNAME lookup, then start sleeping.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    // Verify still Pending.
    assert!(matches!(tracker.lock().await.filter_state(), FilterState::Pending));
    handle.abort();
}
```

(Drop the multi-iteration test — covered by `detect_filter_loop_retries_after_http_error` for the retry semantics.)

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-app/src/tray.rs
git commit -m "tray: detect_filter_loop with exponential backoff + retry tests (#42)"
```

---

## Task 6: Wire `detect_filter_loop` into `setup_tray` + gate `fetch_initial_jobs`

**Files:**
- Modify: `crates/devbridge-app/src/tray.rs`

- [ ] **Step 1: Replace the one-shot detection in `run_event_loop`**

Locate the top of `run_event_loop` in `crates/devbridge-app/src/tray.rs` (around lines 67-75). The block currently looks like:

```rust
async fn run_event_loop(app: AppHandle, dashboard_url: String, tracker: Arc<Mutex<JobTracker>>) {
    // Detect server mode and set filter_user before fetching initial jobs.
    // Done async here so it doesn't block the Tauri main thread in setup_tray.
    let filter_user = detect_filter_user(&dashboard_url).await;
    tracing::info!("Tray filter_user: {:?}", filter_user);
    {
        let mut t = tracker.lock().await;
        t.set_filter_state(filter_user);
    }
```

REPLACE that block with:

```rust
async fn run_event_loop(app: AppHandle, dashboard_url: String, tracker: Arc<Mutex<JobTracker>>) {
    // Spawn detect_filter_loop in the background; it retries until the
    // tracker leaves Pending. fetch_initial_jobs below waits on the same
    // signal so we don't query the API with the wrong (or no) filter.
    let fetcher: Arc<dyn StatusFetcher> = Arc::new(HttpStatusFetcher::new(&dashboard_url));
    let tracker_for_detect = tracker.clone();
    tauri::async_runtime::spawn(async move {
        detect_filter_loop(tracker_for_detect, fetcher).await;
    });
```

- [ ] **Step 2: Delete the obsolete `detect_filter_user` function**

Remove the stub function from `tray.rs` (added in Task 3, ~5 lines). The new `detect_filter_loop` is its replacement.

- [ ] **Step 3: Gate `fetch_initial_jobs` on filter known**

REPLACE the body of `fetch_initial_jobs` in `crates/devbridge-app/src/tray.rs` with:

```rust
async fn fetch_initial_jobs(dashboard_url: &str, tracker: &Arc<Mutex<JobTracker>>) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build");

    // Wait for the detector loop to leave Pending — otherwise we'd query
    // /api/jobs with no filter on a server-mode tray and leak other users'
    // jobs into the Recent Jobs menu (issue #42).
    let mut filter_rx = {
        let t = tracker.lock().await;
        t.filter_subscribe()
    };
    if let Err(e) = filter_rx
        .wait_for(|state| !matches!(state, FilterState::Pending))
        .await
    {
        tracing::warn!("Filter watch sender dropped before detection completed: {e}");
        return;
    }
    let filter_state = filter_rx.borrow().clone();

    let url = match &filter_state {
        FilterState::User(u) => {
            format!("{dashboard_url}/api/jobs?limit=5&requesting_user={u}")
        }
        FilterState::Disabled => format!("{dashboard_url}/api/jobs?limit=5"),
        FilterState::Pending => unreachable!("wait_for guarded against Pending"),
    };

    match http.get(&url).send().await {
        Ok(resp) => match resp.json::<Vec<serde_json::Value>>().await {
            Ok(jobs) => {
                let mut t = tracker.lock().await;
                // Jobs come newest-first from API; add in reverse so push_front gives correct order
                for job in jobs.iter().rev() {
                    let job_id = job["id"].as_str().unwrap_or("").to_string();
                    let target_printer = job["printer"]
                        .as_str()
                        .unwrap_or("unknown printer")
                        .to_string();
                    let status_str = job["status"].as_str().unwrap_or("queued");
                    let created_at_str = job["created_at"].as_str().unwrap_or("");

                    if job_id.is_empty() {
                        continue;
                    }

                    let timestamp = chrono::DateTime::parse_from_rfc3339(created_at_str)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());

                    let display_status = match status_str {
                        "completed" => JobDisplayStatus::Completed,
                        "failed" | "cancelled" => JobDisplayStatus::Failed,
                        _ => JobDisplayStatus::InProgress,
                    };

                    t.add_job(job_id, target_printer, timestamp, display_status);
                }
            }
            Err(e) => tracing::warn!("Failed to parse initial jobs: {e}"),
        },
        Err(e) => tracing::warn!("Failed to fetch initial jobs: {e}"),
    }
}
```

- [ ] **Step 4: Verify build + all tests**

Run: `cargo test -p devbridge-app`
Expected: all tests pass (12+ from Task 2/3 + 3-4 new ones from Task 5).

Run: `cargo fmt --all --check`
Expected: passes.

Run: `cargo clippy -p devbridge-app -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-app/src/tray.rs
git commit -m "tray: wire detect_filter_loop + gate fetch_initial_jobs on filter known (#42)"
```

---

## Task 7: Push and monitor CI

- [ ] **Step 1: Local format check**

```bash
cargo fmt --all --check
```
Expected: passes.

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Identify the CI run + monitor in background**

```bash
RUN_ID=$(gh run list --branch dev --limit 1 --json databaseId --jq '.[0].databaseId')
echo "Monitoring run $RUN_ID"
```

Then in a background Bash:

```bash
sleep 600 && gh run view $RUN_ID --json status,conclusion,jobs
```

When it completes, check `conclusion`. If `failure`, run `gh run view $RUN_ID --log-failed` and fix.

- [ ] **Step 4: If mutation testing surfaces survivors in `detect_filter_loop`**

`cargo mutants --in-diff` will mutate the new code. Survivors are likely around:
- The `>=` vs `>` boundary in the backoff cap (`(backoff * 2).min(DETECT_MAX_BACKOFF)`)
- The match arm that picks `mode == "client"` vs `mode == "server"`

For each survivor: write a test that detects the mutation. Example for the mode match — add a test that returns `Ok("server")` and asserts `User`, plus another that returns `Ok("client")` and asserts `Disabled` (already in Task 5). For backoff cap, the test would need to assert specific sleep durations — that may be a low-value mutation and acceptable to leave.

If a survivor is genuinely untestable (e.g., the `tracing::info!` message text), add it to `mutants.toml` exclusions with a justifying comment.

- [ ] **Step 5: Verify all Tier 1 + Tier 1.5 + Tier 2 jobs pass**

Run: `gh run view $RUN_ID --json jobs --jq '.jobs[] | {name, conclusion}'`
Expected: every job has `"conclusion": "success"`.

The dev CI deploys to `pz-server` and `pz-snv` automatically. The new tray binary will be packaged into the NSIS installer and deployed. After CI succeeds, both clients should be running 0.8.21.

---

## Task 8: Real-world verification on pz-server (the actual bug repro)

- [ ] **Step 1: Confirm 0.8.21 is deployed on pz-server**

```bash
curl -s http://10.88.1.100:9120/api/status | jq .version
```
Expected: `"0.8.21"`.

- [ ] **Step 2: Service-already-running test (regression coverage)**

Use Windows MCP tools.

```
mcp__win-pz-server__Shell command: "Get-Service DevBridge"
```
Expected: `Status Running`.

Verify both pjsnvs and pjpos tray apps are running:
```
mcp__win-pz-server__Shell command: "Get-Process devbridge-app -ErrorAction SilentlyContinue | Select-Object SessionId, UserName"
```

If both are not running, manually start them via the user's session (cannot be done remotely without SSH-as-user; if needed, ask the user to log in as both accounts and confirm trays are visible).

Trigger a print job from pjpos session by submitting a small print job to the pjpos virtual printer:
```
mcp__win-pz-server__Shell command: "Add-Type -AssemblyName System.Drawing; $pd = New-Object System.Drawing.Printing.PrintDocument; $pd.PrinterSettings.PrinterName = 'pjpos printer'; $pd.add_PrintPage({ param($s,$e) $e.Graphics.DrawString('test', (New-Object System.Drawing.Font 'Arial',24), [System.Drawing.Brushes]::Black, 100, 100); $e.HasMorePages = $false }); $pd.Print()"
```

(Note: this PrintDocument call runs as the pz-server admin user, which is neither pjsnvs nor pjpos. The `requesting_user` recorded by the IPP service will reflect whichever user the print spooler associates with the job — typically the pz-server admin in this case. **For the actual user-isolation test, the print job must originate from a real pjpos RDP session.** If the MCP-driven print can't simulate this, fall back to: ask the user to print from a real pjpos RDP session.)

- [ ] **Step 3: Read the tray log on the pjsnvs session**

After the print job, fetch the latest tray log from pjsnvs's session:
```
mcp__win-pz-server__FileRead path: "C:\\Users\\pjsnvs\\AppData\\Local\\DevBridge\\tray.log"
```
(Adjust path to wherever the tracing subscriber writes — check `crates/devbridge-app/src/main.rs` for the log file location.)

**Expected:** The log should contain `should_process = false` (or equivalent "dropped event for user X" line) for any job whose `requesting_user` is not pjsnvs. Crucially: **no `Print Job Received` notification entries** for pjpos's job in pjsnvs's log.

- [ ] **Step 4: Service-cold-start test (the original bug)**

This requires user approval per `no-destructive-remote-actions.md` — restarting the service causes brief downtime. Before proceeding, ask the user:

> "I need to stop+start the DevBridge service on pz-server (~5 seconds downtime) to reproduce the cold-start bug from #42. The fix should hold even when the tray launches before the service is ready. Should I proceed?"

If approved:
```
mcp__win-pz-server__ServiceStop name: "DevBridge"
mcp__win-pz-server__Wait seconds: 2
mcp__win-pz-server__ServiceStart name: "DevBridge"
```

Then trigger another pjpos print job and re-verify per Step 3.

- [ ] **Step 5: Document verification evidence in PR**

Capture:
- pjsnvs tray.log excerpts showing dropped events with `should_process = false`
- Absence of "Print Job Received" entries on pjsnvs's tray
- Both before and after service restart

---

## Task 9: Open PR + audits

- [ ] **Step 1: Create the PR from dev to main**

```bash
gh pr create --title "Tray: fix RDP filter leak with FilterState enum (fixes #42)" --body "$(cat <<'EOF'
## Summary
- Replace ambiguous `Option<String>` filter with typed `FilterState::{Pending, Disabled, User(String)}` enum that fails closed on detection failure
- Replace one-shot `detect_filter_user` with `detect_filter_loop` that retries with exponential backoff (1s → 30s cap) until detection succeeds
- Gate `fetch_initial_jobs` on filter being known (via `tokio::sync::watch`) — fixes secondary leak through the Recent Jobs menu
- Bump version 0.8.20 → 0.8.21

Fixes #42 (Tray leaks other users' print notifications on shared RDP server).

## Test plan
- [x] Unit: `JobTracker::should_process` drops events when `FilterState::Pending`
- [x] Unit: `detect_filter_loop` retries on HTTP errors, transitions on client/server mode
- [x] Real verification on pz-server: pjsnvs's tray does not see pjpos's notifications or Recent Jobs entries (both warm and cold-start cases)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Wait for PR CI to pass**

The PR will trigger another full CI run (push event already triggered one; pull_request event triggers another). Both must pass.

```bash
PR_NUMBER=$(gh pr list --head dev --json number --jq '.[0].number')
RUN_ID=$(gh run list --event pull_request --branch dev --limit 1 --json databaseId --jq '.[0].databaseId')
sleep 600 && gh run view $RUN_ID --json status,conclusion
```

- [ ] **Step 3: Verify PR is mergeable + clean**

```bash
gh api repos/zbynekdrlik/devbridge/pulls/$PR_NUMBER --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```
Expected: `{"mergeable": true, "mergeable_state": "clean"}`.

- [ ] **Step 4: Run `/plan-check` audit**

Invoke the plan-check skill against this plan. Verify every step is `[x]`. If any are `[ ]`, complete them before reporting.

- [ ] **Step 5: Run `/review` audit**

Invoke `/review $PR_NUMBER`. Address any 🔴 critical and 🟡 warning findings. 🔵 suggestions can be deferred.

- [ ] **Step 6: Send completion report and wait for explicit "merge it"**

Per `pr-merge-policy.md`: do NOT merge. Report the green PR URL and the verification evidence. Wait for explicit user approval.

---

## Verification

After PR merges to main and main CI completes (which re-deploys to pz-server and pz-snv):

1. **Service-already-running case:** real users pjsnvs and pjpos both RDP into pz-server. pjpos prints. pjsnvs sees no notification, no menu entry.
2. **Service-cold-start case:** stop service, immediately log in as both users, restart service. pjpos prints. pjsnvs still sees nothing.
3. **Client mode unaffected:** any client tray (pjpos-client, pjkeb-client, etc.) continues to show ALL jobs from its single user (no regression on client-mode trays).
