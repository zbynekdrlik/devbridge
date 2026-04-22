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
/// Only referenced inside the Windows spawn body; non-Windows builds
/// would flag it as dead code.
#[cfg(target_os = "windows")]
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
        // Fixed tmp filename is safe because `reconciler_loop` awaits each
        // `invoker.invoke` fully before the next one starts, and there is only
        // ever one reconciler task. If that architecture changes, switch to a
        // unique suffix (pid/timestamp) or `tempfile::NamedTempFile`.
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
                warn!(
                    timeout_secs = SPAWN_TIMEOUT.as_secs(),
                    "reconciler: PS1 timed out"
                );
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
) -> (
    Arc<dyn ReconcilerInvoker>,
    mpsc::Sender<()>,
    mpsc::Receiver<()>,
) {
    let invoker: Arc<dyn ReconcilerInvoker> = Arc::new(PowerShellInvoker {
        script_path: data_dir.join("register-virtual-printers.ps1"),
        data_dir: data_dir.clone(),
    });
    let (tx, rx) = mpsc::channel::<()>(SIGNAL_CHANNEL_CAPACITY);
    (invoker, tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("test.db");
        let storage = Storage::new(&db).unwrap();
        // Leak the TempDir so the SQLite file outlives the test; tests are short
        // and the OS reclaims temp files.
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
        // Let the loop's startup invoke complete; paused-clock requires advance.
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
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Burst: 10 signals (all fit inside SIGNAL_CHANNEL_CAPACITY=32).
        for _ in 0..10 {
            tx.try_send(()).unwrap();
        }

        // Inside the debounce window — no additional invoke yet.
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Cross the debounce boundary; sleep completes, drain runs, invoke fires.
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
        // Only the startup invoke should have fired — no event-driven invokes.
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn powershell_invoker_is_noop_on_non_windows() {
        let invoker = PowerShellInvoker {
            script_path: PathBuf::from("/nonexistent.ps1"),
            data_dir: PathBuf::from("/tmp"),
        };
        // Should return Ok and log a skip message.
        invoker.invoke(&[]).await.unwrap();
    }
}
