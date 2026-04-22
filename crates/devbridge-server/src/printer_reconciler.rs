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
pub const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// Hard upper bound on a single PS1 invocation. The script runs through
/// 6 printers in <2 s on pz-server; 60 s leaves headroom for spooler
/// stalls without letting a hung process pin the runtime forever.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);

/// Channel capacity for incoming reconcile signals. Larger than any
/// reasonable burst of registrations; `try_send` drops on full so a
/// flood cannot back up storage callers.
pub const SIGNAL_CHANNEL_CAPACITY: usize = 32;

/// Anything that can perform one reconcile pass given the current set
/// of virtual printers. Production impl spawns PowerShell; tests use a
/// counting double.
#[async_trait]
pub trait ReconcilerInvoker: Send + Sync {
    async fn invoke(&self, printers: &[VirtualPrinter]) -> Result<()>;
}

// The full reconciler_loop + PowerShellInvoker impl land in Task 4.
// This skeleton exists so Tasks 3 (signal wiring) and 4 can both compile
// independently.
#[allow(dead_code)]
pub(crate) fn _keep_unused_warning_silent() -> (PathBuf, Arc<JobQueue>, mpsc::Sender<()>) {
    unimplemented!("populated in Task 4")
}

#[allow(dead_code)]
fn _silence_unused(x: impl Into<String>) -> String {
    // Use all the imported items to prevent dead-code / unused-import lints
    // until Task 4 fleshes out the module.
    let _ = DEBOUNCE_DURATION;
    let _ = SPAWN_TIMEOUT;
    let _ = SIGNAL_CHANNEL_CAPACITY;
    info!("skeleton");
    warn!("skeleton");
    x.into()
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
