# Auto-Register Windows Shared Printer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make devbridge-service the single owner of Windows-printer registration: spawn the existing `register-virtual-printers.ps1` once at startup and again on every virtual-printer DB insert/update, never delete existing printers, retire the `DevBridgeReconcilePrinters` scheduled task and the printer-registration block in `post-install.ps1`.

**Architecture:** A new Rust module `printer_reconciler` lives in `devbridge-server`. It owns a Tokio task that drains a debounced `mpsc::Receiver<()>`, serializes the current `Vec<VirtualPrinter>` to a temp JSON file under `data_dir`, and spawns `powershell.exe -File <data_dir>/register-virtual-printers.ps1 -InputJson <path>`. The receiver is signalled (`try_send`) from `JobQueue::insert_virtual_printer` and `JobQueue::update_virtual_printer` whenever a sender has been wired. The runtime layer constructs the reconciler during server-mode startup and runs it concurrently with IPP/gRPC/dashboard listeners.

**Tech Stack:** Rust 2024, Tokio, `tokio::sync::mpsc`, `serde_json`, PowerShell 5.1+, Windows Print Spooler (rundll32 printui.dll).

**Spec:** [`docs/superpowers/specs/2026-04-22-auto-register-windows-printer-design.md`](../specs/2026-04-22-auto-register-windows-printer-design.md)

---

## File Map

### New
| File | Responsibility |
|------|----------------|
| `crates/devbridge-server/src/printer_reconciler.rs` | Reconciler struct, debounced loop, spawn-PowerShell helper, mockable invoker trait, unit tests |

### Modified
| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | `version = "0.8.19"` → `version = "0.8.20"` |
| `crates/devbridge-app/tauri.conf.json` | `"version": "0.8.19"` → `"version": "0.8.20"` |
| `crates/devbridge-server/src/storage.rs` | Bump hardcoded `client_version: "0.8.19"` test fixture to `"0.8.20"` |
| `crates/devbridge-server/src/lib.rs` | Add `pub mod printer_reconciler;` |
| `crates/devbridge-server/src/queue.rs` | Add `reconciler_signal: Option<mpsc::Sender<()>>` field + `set_reconciler_signal` setter; signal `try_send(())` from `insert_virtual_printer` and `update_virtual_printer` after the SQL write succeeds |
| `crates/devbridge-service/src/runtime.rs` | In `run_server`: build `Reconciler`, call `queue.set_reconciler_signal(tx)`, `tokio::spawn(reconciler.run(...))` |
| `deploy/register-virtual-printers.ps1` | Add optional `-InputJson <path>` parameter; when supplied, read printer list from that JSON file and skip dashboard fetch |
| `installer/post-install.ps1` | Strip Step 3-7 (printer-registration via API) and Step 8 (scheduled-task registration); add an upgrade-cleanup step that `Unregister-ScheduledTask -TaskName DevBridgeReconcilePrinters` idempotently; KEEP the copy of `register-virtual-printers.ps1` into `$DataDir` (the service reads it from there) |

### Deleted
| File | Reason |
|------|--------|
| `installer/register-printer.ps1` | Legacy single-printer script. No callers (verified via grep before removal) |

---

## Task 1: Version bump (FIRST commit on dev)

**Files:**
- Modify: `Cargo.toml` (workspace.package version, line 15)
- Modify: `crates/devbridge-app/tauri.conf.json` (`"version"` field, line 4)
- Modify: `crates/devbridge-server/src/storage.rs` (line ~1069 — test fixture)

- [ ] **Step 1: Bump workspace version to 0.8.20**

Edit `Cargo.toml` line 15:

```toml
[workspace.package]
version = "0.8.20"
```

- [ ] **Step 2: Bump tauri.conf.json**

Edit `crates/devbridge-app/tauri.conf.json` line 4:

```json
  "version": "0.8.20",
```

- [ ] **Step 3: Bump test fixture in storage.rs**

Run `grep -n '"0.8.19"' crates/devbridge-server/src/storage.rs` to confirm location, then change `client_version: "0.8.19".into(),` to `client_version: "0.8.20".into(),`.

- [ ] **Step 4: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0, no diff.

- [ ] **Step 5: Commit version bump**

```bash
git add Cargo.toml crates/devbridge-app/tauri.conf.json crates/devbridge-server/src/storage.rs
git commit -m "$(cat <<'EOF'
0.8.20: auto-register Windows printers on server (#44)

Bump version before any code changes per airuleset version-bumping rule.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Do NOT push yet — continue.

---

## Task 2: Reconciler module skeleton with non-Windows stub

**Files:**
- Create: `crates/devbridge-server/src/printer_reconciler.rs`
- Modify: `crates/devbridge-server/src/lib.rs`

- [ ] **Step 1: Add module to crate**

Edit `crates/devbridge-server/src/lib.rs`:

```rust
pub mod dispatch;
pub mod ipp_service;
pub mod printer_reconciler;
pub mod queue;
pub mod serial_bridge;
pub mod storage;
```

- [ ] **Step 2: Write the failing test for non-Windows stub**

Create `crates/devbridge-server/src/printer_reconciler.rs`:

```rust
//! Windows-printer reconciler for devbridge-server.
//!
//! Spawns `register-virtual-printers.ps1` (1) once at service startup and
//! (2) on every virtual-printer DB insert/update, debounced so a burst of
//! events coalesces into one PowerShell invocation. On non-Windows the
//! reconciler is a no-op that logs a skip message — keeps the Linux CI
//! build green and lets the orchestration logic be unit-tested.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use devbridge_core::virtual_printer::VirtualPrinter;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::queue::JobQueue;

/// Time the reconciler waits after receiving a signal before invoking the
/// spawner, to coalesce a burst of registrations from a multi-printer
/// rollout into a single PS1 run.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// Hard upper bound on a single PS1 invocation. The script runs through
/// 6 printers in <2 s on pz-server; 60 s leaves headroom for spooler
/// stalls without letting a hung process pin the runtime forever.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);

/// Channel capacity for incoming reconcile signals. Larger than any
/// reasonable burst of registrations; `try_send` drops on full so a
/// flood cannot back up storage callers.
const SIGNAL_CHANNEL_CAPACITY: usize = 32;

/// Anything that can perform one reconcile pass given the current set
/// of virtual printers. Production impl spawns PowerShell; tests use a
/// counting double.
#[async_trait]
pub trait ReconcilerInvoker: Send + Sync {
    async fn invoke(&self, printers: &[VirtualPrinter]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingInvoker {
        count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ReconcilerInvoker for CountingInvoker {
        async fn invoke(&self, _printers: &[VirtualPrinter]) -> Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn placeholder_compiles() {
        // Ensures module compiles + trait wiring. Real tests added in Task 4.
        let count = Arc::new(AtomicUsize::new(0));
        let inv: Box<dyn ReconcilerInvoker> = Box::new(CountingInvoker {
            count: Arc::clone(&count),
        });
        inv.invoke(&[]).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
```

- [ ] **Step 3: Add `async-trait` to server Cargo.toml**

Run `grep -n 'async-trait\|^\[dependencies\]' crates/devbridge-server/Cargo.toml`. If `async-trait` is missing, add under `[dependencies]`:

```toml
async-trait = "0.1"
```

- [ ] **Step 4: Verify compilation + test passes locally via formatting alone**

Local CLAUDE.md forbids running cargo build/test/check locally. Run only:

```bash
cargo fmt --all --check
```

Expected: exit 0. Compilation will be verified by CI on push.

- [ ] **Step 5: Commit skeleton**

```bash
git add crates/devbridge-server/src/lib.rs crates/devbridge-server/src/printer_reconciler.rs crates/devbridge-server/Cargo.toml
git commit -m "Add printer_reconciler module skeleton + ReconcilerInvoker trait

Module compiles on all platforms; production spawn impl + debounce loop
land in subsequent commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: JobQueue signal wiring (TDD)

**Files:**
- Modify: `crates/devbridge-server/src/queue.rs` (struct + 2 methods + tests)

- [ ] **Step 1: Write the failing test for signal-on-insert**

Append to the existing `#[cfg(test)] mod tests` block at the bottom of `crates/devbridge-server/src/queue.rs`:

```rust
    #[tokio::test]
    async fn test_insert_virtual_printer_signals_reconciler() {
        use tokio::sync::mpsc;
        let storage = Storage::new(&temp_db_path()).unwrap();
        let mut queue = JobQueue::new(storage).unwrap();
        let (tx, mut rx) = mpsc::channel::<()>(8);
        queue.set_reconciler_signal(tx);

        let now = chrono::Utc::now();
        let vp = VirtualPrinter {
            id: "vp-signal-1".into(),
            display_name: "Signal Test".into(),
            ipp_name: "signal-test".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();

        assert!(
            rx.try_recv().is_ok(),
            "expected one signal after insert_virtual_printer"
        );
    }

    #[tokio::test]
    async fn test_update_virtual_printer_signals_reconciler() {
        use tokio::sync::mpsc;
        let storage = Storage::new(&temp_db_path()).unwrap();
        let mut queue = JobQueue::new(storage).unwrap();
        let (tx, mut rx) = mpsc::channel::<()>(8);
        queue.set_reconciler_signal(tx);

        let now = chrono::Utc::now();
        let mut vp = VirtualPrinter {
            id: "vp-signal-2".into(),
            display_name: "Signal Test 2".into(),
            ipp_name: "signal-test-2".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();
        // drain the insert signal
        let _ = rx.try_recv();

        vp.display_name = "Renamed".into();
        queue.update_virtual_printer(&vp).unwrap();
        assert!(
            rx.try_recv().is_ok(),
            "expected one signal after update_virtual_printer"
        );
    }

    #[tokio::test]
    async fn test_insert_virtual_printer_without_signal_does_not_panic() {
        let storage = Storage::new(&temp_db_path()).unwrap();
        let queue = JobQueue::new(storage).unwrap();

        let now = chrono::Utc::now();
        let vp = VirtualPrinter {
            id: "vp-nosignal".into(),
            display_name: "NoSignal".into(),
            ipp_name: "no-signal".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();
        // No reconciler wired — must not panic.
    }
```

If `temp_db_path()` doesn't already exist in queue.rs's test module, add it:

```rust
    fn temp_db_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("devbridge-test-{}.db", uuid::Uuid::new_v4()));
        p
    }
```

(Check with `grep -n "fn temp_db_path\|fn make_temp_db\|TempDir::new" crates/devbridge-server/src/queue.rs` first to follow whatever the existing tests already use.)

- [ ] **Step 2: Add the field, setter, and signalling code**

Modify `crates/devbridge-server/src/queue.rs`:

In the imports near the top:

```rust
use tokio::sync::mpsc;
```

In the `JobQueue` struct (around line 25-32), add the field:

```rust
pub struct JobQueue {
    storage: Mutex<Storage>,
    client_channels: Mutex<HashMap<String, mpsc::UnboundedSender<JobBundle>>>,
    default_pending: Arc<Mutex<VecDeque<String>>>,
    default_notify: Arc<Notify>,
    job_events: Option<broadcast::Sender<JobEvent>>,
    pairing_notify: Arc<Notify>,
    reconciler_signal: Option<mpsc::Sender<()>>,
}
```

In `JobQueue::new` (line 37), initialise the field:

```rust
        Ok(Self {
            storage: Mutex::new(storage),
            client_channels: Mutex::new(HashMap::new()),
            default_pending: Arc::new(Mutex::new(deque)),
            default_notify: Arc::new(Notify::new()),
            job_events: None,
            pairing_notify: Arc::new(Notify::new()),
            reconciler_signal: None,
        })
```

Add the setter immediately after `set_job_events` (around line 65):

```rust
    /// Wire the reconciler signal sender. After this is set,
    /// `insert_virtual_printer` and `update_virtual_printer` will fire a
    /// `try_send(())` to notify the reconciler that the virtual-printer
    /// list changed. Best-effort: a full channel drops the signal (the
    /// reconciler is already busy processing a previous burst).
    pub fn set_reconciler_signal(&mut self, tx: mpsc::Sender<()>) {
        self.reconciler_signal = Some(tx);
    }
```

Modify `insert_virtual_printer` (line 355) and `update_virtual_printer` (line 378):

```rust
    pub fn insert_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.insert_virtual_printer(vp)?;
        if let Some(tx) = &self.reconciler_signal {
            let _ = tx.try_send(());
        }
        Ok(())
    }

    pub fn update_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.update_virtual_printer(vp)?;
        if let Some(tx) = &self.reconciler_signal {
            let _ = tx.try_send(());
        }
        Ok(())
    }
```

- [ ] **Step 3: Verify formatting locally**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 4: Commit signal wiring**

```bash
git add crates/devbridge-server/src/queue.rs
git commit -m "JobQueue: signal reconciler on virtual printer insert/update

Adds optional mpsc::Sender<()> field; set via set_reconciler_signal.
Existing callers without a wired sender continue to work unchanged
(additive). Tests cover signal-fired, no-signal-no-panic, both insert
and update paths.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Reconciler debounce loop (TDD)

**Files:**
- Modify: `crates/devbridge-server/src/printer_reconciler.rs`

- [ ] **Step 1: Replace placeholder test with the real Reconciler API tests**

Replace the existing `#[cfg(test)] mod tests` block in `crates/devbridge-server/src/printer_reconciler.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::time::{Duration, sleep};

    use crate::storage::Storage;

    struct CountingInvoker {
        count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ReconcilerInvoker for CountingInvoker {
        async fn invoke(&self, _printers: &[VirtualPrinter]) -> Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn make_queue() -> Arc<JobQueue> {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("test.db");
        let storage = Storage::new(&db).unwrap();
        // Leak the TempDir so the file outlives the test (cheap; tests are short).
        std::mem::forget(dir);
        Arc::new(JobQueue::new(storage).unwrap())
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn startup_invokes_once_with_no_events() {
        let count = Arc::new(AtomicUsize::new(0));
        let invoker: Arc<dyn ReconcilerInvoker> = Arc::new(CountingInvoker {
            count: Arc::clone(&count),
        });
        let queue = make_queue();
        let (_tx, rx) = mpsc::channel::<()>(8);

        let handle = tokio::spawn(reconciler_loop(rx, queue, invoker));
        // Give the loop a chance to do its startup invoke (paused clock means we
        // need to advance time even for the immediate startup invoke).
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;

        handle.abort();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn burst_of_signals_coalesces_into_one_invoke() {
        let count = Arc::new(AtomicUsize::new(0));
        let invoker: Arc<dyn ReconcilerInvoker> = Arc::new(CountingInvoker {
            count: Arc::clone(&count),
        });
        let queue = make_queue();
        let (tx, rx) = mpsc::channel::<()>(32);

        let handle = tokio::spawn(reconciler_loop(rx, queue, invoker));
        // Let startup invoke happen
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        let after_startup = count.load(Ordering::SeqCst);
        assert_eq!(after_startup, 1);

        // Burst: 10 signals within 100ms (well under DEBOUNCE_DURATION).
        for _ in 0..10 {
            tx.try_send(()).unwrap();
        }
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        // Still inside debounce window — no extra invoke yet.
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Cross the debounce boundary.
        tokio::time::advance(DEBOUNCE_DURATION + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        handle.abort();
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "10-signal burst should yield exactly 1 additional invoke"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn closed_channel_exits_loop_cleanly() {
        let count = Arc::new(AtomicUsize::new(0));
        let invoker: Arc<dyn ReconcilerInvoker> = Arc::new(CountingInvoker {
            count: Arc::clone(&count),
        });
        let queue = make_queue();
        let (tx, rx) = mpsc::channel::<()>(8);

        let handle = tokio::spawn(reconciler_loop(rx, queue, invoker));
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;

        drop(tx);
        // After sender drops, the loop's recv() returns None; loop exits.
        let res = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(res.is_ok(), "loop should exit when channel closes");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn powershell_invoker_is_noop_on_non_windows() {
        let invoker = PowerShellInvoker {
            script_path: PathBuf::from("/nonexistent.ps1"),
            data_dir: PathBuf::from("/tmp"),
        };
        // Should return Ok and log skip.
        invoker.invoke(&[]).await.unwrap();
    }
}
```

- [ ] **Step 2: Implement `reconciler_loop` and `PowerShellInvoker`**

Append to `crates/devbridge-server/src/printer_reconciler.rs`:

```rust
/// The Tokio task body. Performs one immediate "startup" invoke, then
/// loops:
///   - Wait for a signal (or exit if the channel closes).
///   - Sleep DEBOUNCE_DURATION while draining any signals that arrive
///     during the sleep — collapses a burst of registrations into one
///     PS1 spawn.
///   - Reload printers from the queue, invoke the spawner, log result.
///
/// Failures from `invoker.invoke` are logged at warn and never propagate;
/// the reconciler must never tear down the service.
pub async fn reconciler_loop(
    mut rx: mpsc::Receiver<()>,
    queue: Arc<JobQueue>,
    invoker: Arc<dyn ReconcilerInvoker>,
) {
    info!("printer reconciler started");

    // Startup invoke — catches reboots, upgrades, drift.
    do_one_invoke(&queue, invoker.as_ref()).await;

    while let Some(()) = rx.recv().await {
        // Debounce: sleep, then drain anything that piled up.
        tokio::time::sleep(DEBOUNCE_DURATION).await;
        while rx.try_recv().is_ok() {
            // Coalesce additional signals received during the sleep.
        }
        do_one_invoke(&queue, invoker.as_ref()).await;
    }

    info!("printer reconciler exited (signal channel closed)");
}

async fn do_one_invoke(queue: &Arc<JobQueue>, invoker: &dyn ReconcilerInvoker) {
    let printers = match queue.list_virtual_printers() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "reconciler: failed to load virtual printers, skipping invoke");
            return;
        }
    };
    info!(count = printers.len(), "reconciler: invoking PS1");
    if let Err(e) = invoker.invoke(&printers).await {
        warn!(error = %e, "reconciler: PS1 invoke failed (continuing)");
    }
}

/// Production invoker: serializes printers to a JSON file under data_dir,
/// spawns powershell.exe with the script path, waits up to SPAWN_TIMEOUT,
/// logs stdout/stderr.
pub struct PowerShellInvoker {
    pub script_path: PathBuf,
    pub data_dir: PathBuf,
}

#[async_trait]
impl ReconcilerInvoker for PowerShellInvoker {
    #[cfg(target_os = "windows")]
    async fn invoke(&self, printers: &[VirtualPrinter]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        if !self.script_path.exists() {
            warn!(
                path = %self.script_path.display(),
                "reconciler: register-virtual-printers.ps1 not found, skipping"
            );
            return Ok(());
        }

        // Write JSON to <data_dir>/reconcile-input.json (atomic via .tmp + rename).
        let json_path = self.data_dir.join("reconcile-input.json");
        let tmp_path = self.data_dir.join("reconcile-input.json.tmp");
        let json_body = serde_json::to_vec_pretty(printers)?;
        let mut f = tokio::fs::File::create(&tmp_path).await?;
        f.write_all(&json_body).await?;
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&tmp_path, &json_path).await?;

        let mut cmd = Command::new("powershell.exe");
        cmd.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&self.script_path)
            .arg("-InputJson")
            .arg(&json_path);

        let child = cmd.spawn()?;
        match tokio::time::timeout(SPAWN_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if out.status.success() {
                    info!(stdout = %stdout.trim(), "reconciler: PS1 ok");
                } else {
                    warn!(
                        code = out.status.code().unwrap_or(-1),
                        stdout = %stdout.trim(),
                        stderr = %stderr.trim(),
                        "reconciler: PS1 non-zero exit"
                    );
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, "reconciler: PS1 wait failed");
            }
            Err(_) => {
                warn!(timeout_secs = SPAWN_TIMEOUT.as_secs(), "reconciler: PS1 timed out");
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    async fn invoke(&self, _printers: &[VirtualPrinter]) -> Result<()> {
        info!("printer reconciler skipped (not Windows)");
        Ok(())
    }
}

/// Build a default invoker pair: the production `PowerShellInvoker` and a
/// fresh `(Sender, Receiver)` for the signal channel.
pub fn build_default(
    data_dir: PathBuf,
) -> (Arc<dyn ReconcilerInvoker>, mpsc::Sender<()>, mpsc::Receiver<()>) {
    let invoker: Arc<dyn ReconcilerInvoker> = Arc::new(PowerShellInvoker {
        script_path: data_dir.join("register-virtual-printers.ps1"),
        data_dir: data_dir.clone(),
    });
    let (tx, rx) = mpsc::channel::<()>(SIGNAL_CHANNEL_CAPACITY);
    (invoker, tx, rx)
}
```

- [ ] **Step 3: Add `tempfile` to dev-dependencies**

Run `grep -n 'tempfile' crates/devbridge-server/Cargo.toml`. If absent, add under `[dev-dependencies]`:

```toml
tempfile = "3"
```

- [ ] **Step 4: Verify formatting**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 5: Commit reconciler core**

```bash
git add crates/devbridge-server/src/printer_reconciler.rs crates/devbridge-server/Cargo.toml
git commit -m "Reconciler: debounced loop + PowerShellInvoker

reconciler_loop: startup invoke + recv-debounce-drain pattern coalesces
event bursts into a single PS1 spawn. PowerShellInvoker serializes
printers to <data_dir>/reconcile-input.json, spawns powershell.exe with
60s timeout, logs stdout/stderr. Non-Windows stub returns Ok.

Tests cover startup-invokes-once, burst-coalescing, channel-closed-exit,
and the non-Windows no-op path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: PS1 -InputJson contract

**Files:**
- Modify: `deploy/register-virtual-printers.ps1`

- [ ] **Step 1: Add the parameter and JSON-input branch**

Edit `deploy/register-virtual-printers.ps1`:

Replace the `param(...)` block (lines 22-28) with:

```powershell
param(
    [int]$DashboardPort = 9120,
    [int]$IppPort = 631,
    [int]$DashboardWaitSecs = 60,
    [int]$IppWaitSecs = 15,
    [string]$LogPath = "C:\ProgramData\DevBridge\logs\register-virtual-printers.log",
    # When set, read the virtual-printer list from this JSON file instead of
    # querying the dashboard API. The DevBridge service passes this when it
    # spawns the script, eliminating the dashboard-startup race.
    [string]$InputJson = ""
)
```

Replace Step 1 (dashboard wait) and Step 2 (HTTP fetch) with this branched version. Find the block from `# Step 1: Wait for DevBridge dashboard` through `Write-Log "Found $($vps.Count) virtual printer(s) to reconcile."` and substitute:

```powershell
# Step 1+2: Source the virtual-printer list -- either from -InputJson (called
# by the service) or by polling the dashboard API (legacy path for any
# manual / scheduled-task invocation).
$vps = $null
if ($InputJson -ne "") {
    if (-not (Test-Path $InputJson)) {
        Write-Log "ERROR: -InputJson path '$InputJson' does not exist"
        exit 3
    }
    try {
        $vps = Get-Content -Raw -Path $InputJson | ConvertFrom-Json
        Write-Log "Loaded $($vps.Count) virtual printer(s) from -InputJson"
    } catch {
        Write-Log "ERROR: Failed to parse -InputJson '$InputJson': $_"
        exit 3
    }
} else {
    # Legacy: wait for dashboard, then fetch /api/virtual-printers.
    $dashReady = $false
    for ($i = 1; $i -le $DashboardWaitSecs; $i++) {
        try {
            $status = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/status" -TimeoutSec 3 -ErrorAction Stop
            if ($status.status -eq "running" -and $status.mode -eq "server") {
                $dashReady = $true
                Write-Log "Dashboard ready after ${i}s (version=$($status.version))"
                break
            }
        } catch {}
        Start-Sleep 1
    }
    if (-not $dashReady) {
        Write-Log "ERROR: Dashboard not ready after ${DashboardWaitSecs}s, aborting."
        exit 1
    }
    try {
        $vps = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/virtual-printers" -TimeoutSec 5 -ErrorAction Stop
    } catch {
        Write-Log "ERROR: Failed to fetch virtual printers: $_"
        exit 2
    }
}

if (-not $vps -or $vps.Count -eq 0) {
    Write-Log "No virtual printers configured -- nothing to reconcile."
    exit 0
}

Write-Log "Found $($vps.Count) virtual printer(s) to reconcile."
```

- [ ] **Step 2: Verify the rest of the script (Step 0 driver repair, Step 3 reconcile loop) is unchanged**

Open the file in your editor and confirm Step 0 (driver repair) at the top and Step 3 (reconcile loop with `foreach ($vp in $vps)`) at the bottom remain intact. The `$vp.display_name` / `$vp.ipp_name` field references match both JSON sources because they're shaped the same.

- [ ] **Step 3: Commit PS1 contract**

```bash
git add deploy/register-virtual-printers.ps1
git commit -m "register-virtual-printers.ps1: accept -InputJson <path>

When the service spawns this script it passes the printer list via JSON
file, eliminating the dashboard-startup race. The HTTP fallback remains
for manual diagnostic invocation. Reconcile loop and driver-repair logic
(Steps 0 and 3) are unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Runtime wiring

**Files:**
- Modify: `crates/devbridge-service/src/runtime.rs`

- [ ] **Step 1: Add the import**

In `crates/devbridge-service/src/runtime.rs`, add to the imports (alphabetised within the existing `use devbridge_server::...` block near the top of the file):

```rust
use devbridge_server::printer_reconciler::{build_default, reconciler_loop};
```

- [ ] **Step 2: Wire the reconciler into `run_server`**

Find this block in `run_server` (currently around lines 82-88 — verify with `grep -n 'set_job_events\|Arc::new(queue)' crates/devbridge-service/src/runtime.rs`):

```rust
    let storage = Storage::new(&db_path).context("Failed to open storage")?;
    let mut queue = JobQueue::new(storage).context("Failed to initialise job queue")?;

    // Job event broadcast channel (consumed by WebSocket clients)
    let (job_events_tx, _) = broadcast::channel::<JobEvent>(256);
    queue.set_job_events(job_events_tx.clone());
    let queue = Arc::new(queue);
```

Replace with:

```rust
    let storage = Storage::new(&db_path).context("Failed to open storage")?;
    let mut queue = JobQueue::new(storage).context("Failed to initialise job queue")?;

    // Job event broadcast channel (consumed by WebSocket clients)
    let (job_events_tx, _) = broadcast::channel::<JobEvent>(256);
    queue.set_job_events(job_events_tx.clone());

    // Wire the printer reconciler. The service is the single owner of
    // Windows-printer registration on the server: one PS1 spawn at startup
    // (catches reboots/upgrades/drift) plus one debounced spawn per
    // virtual-printer DB change (catches new client registrations).
    // set_reconciler_signal takes &mut so it MUST run before Arc-wrap.
    // Reconciler failures are logged and swallowed; they never crash the service.
    let (reconciler_invoker, reconciler_tx, reconciler_rx) = build_default(data_dir.clone());
    queue.set_reconciler_signal(reconciler_tx);

    let queue = Arc::new(queue);

    // Spawn the reconciler loop concurrently with the rest of startup.
    // The first (startup) invoke does not block dashboard/IPP/gRPC binding.
    {
        let queue_for_reconciler = Arc::clone(&queue);
        tokio::spawn(reconciler_loop(
            reconciler_rx,
            queue_for_reconciler,
            reconciler_invoker,
        ));
    }
```

- [ ] **Step 3: Verify formatting**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 4: Commit runtime wiring**

```bash
git add crates/devbridge-service/src/runtime.rs
git commit -m "runtime: spawn printer reconciler in server-mode startup

Constructs the production PowerShellInvoker, wires the signal channel
into the JobQueue (must happen before Arc-wrap), spawns the reconciler
loop as a background Tokio task. Reconciler runs concurrently with the
listener bindings; startup invoke does not block service boot.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Installer cleanup

**Files:**
- Modify: `installer/post-install.ps1`

- [ ] **Step 1: Remove printer-registration block (Steps 3-7) and scheduled-task registration (Step 8 inner content)**

Open `installer/post-install.ps1`. Find the block that starts with:

```
    # -- Step 3: Wait for dashboard API readiness -------------------------
```

and ends with:

```
        Write-Host "  WARNING: register-virtual-printers.ps1 not found in installer payload" -ForegroundColor Yellow
    }
  } catch {
    Write-Host "  Printer registration skipped (insufficient permissions: $_)" -ForegroundColor Yellow
  }
}
```

Replace the entire range (Steps 3-8 inclusive, but keep the wrapping `try { ... } catch { ... }` and Steps 1-2 driver-repair logic) with:

```powershell
    # -- Step 3: Service owns printer registration ------------------------
    # Previous installer versions (<= 0.8.19) registered Windows printers
    # here (querying /api/virtual-printers and shelling out to printui.dll)
    # AND set up a DevBridgeReconcilePrinters scheduled task that re-ran
    # the same logic at boot. That created a startup race: if the service
    # wasn't ready when this block fired, post-install fell back to a
    # legacy "single DevBridge printer" mode and silently broke the
    # multi-store setup.
    #
    # In 0.8.20 the devbridge-service process is the sole owner of
    # Windows-printer registration: it spawns register-virtual-printers.ps1
    # once at startup AND on every virtual-printer DB insert/update.
    # post-install just stages the script; the service runs it.

    # Stage the reconciler script into ProgramData so the service can find it.
    $reconcilerSrc = Join-Path $InstallDir "_up_\_up_\deploy\register-virtual-printers.ps1"
    if (-not (Test-Path $reconcilerSrc)) {
        $reconcilerSrc = Join-Path $InstallDir "register-virtual-printers.ps1"
    }
    $reconcilerDst = Join-Path $DataDir "register-virtual-printers.ps1"
    if (Test-Path $reconcilerSrc) {
        Copy-Item $reconcilerSrc $reconcilerDst -Force
        Write-Host "  Staged reconciler at $reconcilerDst" -ForegroundColor Cyan
    } else {
        Write-Host "  WARNING: register-virtual-printers.ps1 not found in installer payload" -ForegroundColor Yellow
    }

    # -- Step 4: Upgrade cleanup -- unregister stale scheduled task ------
    # 0.8.19 and earlier registered DevBridgeReconcilePrinters AtStartup.
    # 0.8.20+ no longer needs it (service does the same work). Idempotent
    # on fresh installs (no-op if the task doesn't exist).
    Unregister-ScheduledTask -TaskName "DevBridgeReconcilePrinters" `
        -Confirm:$false -ErrorAction SilentlyContinue
  } catch {
    Write-Host "  Printer registration setup skipped (insufficient permissions: $_)" -ForegroundColor Yellow
  }
}
```

- [ ] **Step 2: Verify the rest of post-install.ps1 is intact**

Run:
```bash
grep -nE "Step [0-9]+|DevBridgeReconcilePrinters|register-virtual-printers" installer/post-install.ps1
```

Expected output: Steps 1-4 only, one mention of DevBridgeReconcilePrinters (the Unregister line), one staging of the reconciler script, no rundll32 / Add-Printer / Get-Printer calls.

- [ ] **Step 3: Commit installer changes**

```bash
git add installer/post-install.ps1
git commit -m "installer: service owns printer registration; remove scheduled task

Removes Steps 3-8 of the printer-registration block (API query, fallback
to legacy single printer, rundll32 printui.dll loop, and the
DevBridgeReconcilePrinters scheduled-task registration). Adds idempotent
upgrade-cleanup that unregisters the stale task on existing installs.
The reconciler script is still staged into ProgramData so the
devbridge-service process can spawn it on startup and on virtual-printer
DB events.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Delete legacy register-printer.ps1

**Files:**
- Delete: `installer/register-printer.ps1`

- [ ] **Step 1: Verify no callers remain**

```bash
grep -rn "register-printer.ps1" --include="*.ps1" --include="*.rs" --include="*.toml" --include="*.json" --include="*.yml" --include="*.md" .
```

Expected: only matches inside `docs/superpowers/specs/` and `docs/superpowers/plans/` (this plan + the spec). If anything references it from `installer/`, `deploy/`, `crates/`, or `.github/`, STOP and investigate before deleting.

- [ ] **Step 2: Delete the file**

```bash
git rm installer/register-printer.ps1
```

- [ ] **Step 3: Commit deletion**

```bash
git commit -m "installer: remove legacy register-printer.ps1

Single-printer registration script from pre-multi-vp era. Not referenced
by any installer or workflow as of 0.8.20 (verified via grep). Service-
owned reconciliation (deploy/register-virtual-printers.ps1) is the
canonical path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Push, monitor CI, fix any failures

- [ ] **Step 1: Local format check**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 2: Push**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI to terminal state**

```bash
gh run list --branch dev --limit 3
```

Identify the latest run id. Then:

```bash
sleep 300 && gh run view <run-id> --json status,conclusion,jobs
```

Background that command. When it returns, evaluate: if any job failed, run `gh run view <run-id> --log-failed`, fix the root cause, batch all fixes into ONE commit, push, monitor again.

Common failure patterns to watch for:

- **Mutation testing** flags surviving mutants in `printer_reconciler.rs` or the new queue.rs paths. Fix by adding stronger assertions to the existing tests, not by adding `#[mutants::skip]`.
- **Clippy** warnings on the new module. Resolve at root cause (e.g., unused imports on non-Windows from the cfg-gated body).
- **Windows Build** fails on the new spawn code. Most likely path: `tokio::process::Command` requires the `process` feature on tokio — verify `tokio = { features = [..., "process"] }` is set in workspace Cargo.toml.

- [ ] **Step 4: Verify all jobs pass**

After CI is green, confirm:

```bash
gh run view <run-id> --json status,conclusion,jobs --jq '.jobs[] | {name: .name, conclusion: .conclusion}'
```

Every job must show `conclusion: success` (Dev Release skipped is OK).

- [ ] **Step 5: Wait for E2E deploy to pz-server (still on dev branch)**

The dev-branch CI does NOT deploy to pz-server (only main does). Skip to Task 10 to open the PR. Post-deploy verification on pz-server happens after merge.

---

## Task 10: PR + post-deploy verification

- [ ] **Step 1: Create the PR**

```bash
gh pr create --title "0.8.20: auto-register Windows printers on server (#44)" --body "$(cat <<'EOF'
## Summary
- devbridge-service now owns Windows-printer registration. New module `printer_reconciler` spawns `register-virtual-printers.ps1` (1) once at server-mode startup and (2) on every virtual-printer DB insert/update, debounced 500 ms.
- `register-virtual-printers.ps1` accepts `-InputJson <path>` so the service can pass the printer list directly (no dashboard-startup race).
- `installer/post-install.ps1` no longer registers printers and no longer creates the `DevBridgeReconcilePrinters` scheduled task. Adds idempotent upgrade-cleanup that unregisters the stale task on existing installs.
- `installer/register-printer.ps1` (legacy single-printer script, no callers) deleted.
- Never-delete invariant codified in PS1: zero `Remove-Printer` calls on any path. Orphaned Windows printers (virtual printer removed from DB) are left alone for manual cleanup.

Resolves #44.

## Test plan
- [ ] All Tier 1 jobs green (Format, Clippy, Test, Mutation, Build, Playwright, TDD Policy, Audit, Cargo Deny)
- [ ] Windows Build green
- [ ] After merge, main-branch E2E Deploy + E2E Test green on pz-server / pz-snv
- [ ] Manual verification on pz-server post-merge: `Get-Printer | ? PortName -like "*127.0.0.1:631/printers/*"` shows all 6 production virtual printers
- [ ] Manual verification: trigger new client registration → Windows printer appears on pz-server within ~5s without manual `Add-Printer`
- [ ] Manual verification: delete a virtual printer via dashboard → corresponding Windows printer still exists (never-delete invariant)
- [ ] Manual verification: `Get-ScheduledTask DevBridgeReconcilePrinters` returns "Not Found" after upgrade

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Verify PR is mergeable**

```bash
PR_NUM=$(gh pr view --json number --jq .number)
gh api repos/zbynekdrlik/devbridge/pulls/$PR_NUM --jq '{mergeable, mergeable_state}'
```

Expected: `mergeable: true`, `mergeable_state: "clean"`. If "behind", `git fetch origin && git merge origin/main && git push`. If "blocked" or "dirty", investigate.

- [ ] **Step 3: Wait for PR CI to pass**

```bash
gh pr checks $PR_NUM
```

Wait until all required checks are green.

- [ ] **Step 4: Provide PR URL and stop**

Output the PR URL for the user. **Do NOT merge** — wait for explicit user instruction (per `pr-merge-policy.md`).

After the user says "merge it":

- [ ] **Step 5: Merge the PR**

```bash
gh pr merge $PR_NUM --merge
```

- [ ] **Step 6: Monitor main-branch CI to deploy completion**

```bash
sleep 300 && gh run list --branch main --limit 1
sleep 600 && gh run view <main-run-id> --json status,conclusion,jobs
```

Wait until E2E Deploy Client + E2E Test reach `success`.

- [ ] **Step 7: Post-deploy verification on pz-server (functional, not just liveness)**

Use `mcp__win-pz-server__Shell` to run:

```powershell
# 1) Service is on 0.8.20
Invoke-RestMethod http://127.0.0.1:9120/api/status | Select-Object version

# 2) All 6 virtual printers from DB are registered as Windows printers
Get-Printer | Where-Object { $_.PortName -like "*127.0.0.1:631/printers/*" } |
    Select-Object Name, PortName, DriverName

# 3) Scheduled task is gone (upgrade-cleanup ran)
Get-ScheduledTask -TaskName DevBridgeReconcilePrinters -ErrorAction SilentlyContinue
```

Expected: version `0.8.20`, six printers each with port `http://127.0.0.1:631/printers/<ipp_name>` and driver `Microsoft IPP Class Driver`, the Get-ScheduledTask returns nothing.

- [ ] **Step 8: Functional verification — new-client trigger**

Read the latest `register-virtual-printers.log`:

```powershell
Get-Content C:\ProgramData\DevBridge\logs\register-virtual-printers.log -Tail 40
```

Expected: a recent `=== register-virtual-printers start ===` entry from the post-deploy service restart, ending in `done (all OK)` or with explicit failures listed.

Then via `mcp__win-pz-server__Shell` simulate a fresh registration by inserting a test virtual printer through the dashboard API:

```powershell
$body = @{ display_name = "test-auto-reg-44"; ipp_name = "test-auto-reg-44" } | ConvertTo-Json
Invoke-RestMethod -Uri http://127.0.0.1:9120/api/virtual-printers `
    -Method POST -ContentType application/json -Body $body
Start-Sleep 6
Get-Printer -Name "test-auto-reg-44" -ErrorAction SilentlyContinue
# Cleanup the test entry
$id = (Invoke-RestMethod http://127.0.0.1:9120/api/virtual-printers |
    Where-Object { $_.ipp_name -eq "test-auto-reg-44" }).id
Invoke-RestMethod -Uri "http://127.0.0.1:9120/api/virtual-printers/$id" -Method DELETE
# Verify never-delete: the Windows printer should still exist after DB delete
Start-Sleep 3
Get-Printer -Name "test-auto-reg-44" -ErrorAction SilentlyContinue
# Manual cleanup of the orphaned Windows printer
Remove-Printer -Name "test-auto-reg-44" -ErrorAction SilentlyContinue
```

Expected results:
- After POST + 6 s sleep: `Get-Printer` returns the new printer with the right port URL.
- After DELETE + 3 s sleep: `Get-Printer` STILL returns the printer (never-delete invariant).
- After explicit `Remove-Printer`: it's gone.

- [ ] **Step 9: Send completion report**

Per `completion-report.md`, the final message must be the structured report with the E2E test coverage table and verification evidence from Step 7-8.

---

## Verification checklist (mirrors spec acceptance criteria)

- [ ] New client install with `DEVBRIDGE_VIRTUAL_PRINTER_NAME` → Windows printer appears on server automatically within 5 s, no manual step.
- [ ] Server upgrade via `irm install.ps1 | iex` → all existing Windows printers remain.
- [ ] Service restart → missing Windows printers re-registered from DB.
- [ ] `DevBridgeReconcilePrinters` scheduled task is unregistered on upgrade.
- [ ] `installer/register-printer.ps1` is removed from the repo.
- [ ] post-install.ps1 no longer references `/api/virtual-printers` or registers Windows printers.
- [ ] Unit tests for reconciler module pass (debounce, startup, channel-closed).
- [ ] Integration tests for queue signal wiring pass (insert + update fire signals; missing sender no-op).
- [ ] E2E verification on pz-server passes Steps 7-8.
