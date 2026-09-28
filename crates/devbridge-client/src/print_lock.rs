//! Client-wide print lock shared by every job source (#90).
//!
//! The client has two independent sources feeding the same local printer:
//! the gRPC `SubscribeJobs` receiver (pz-server, e.g. BarTender labels) and
//! the Odoo pull source ([`crate::odoo_source`]). The Windows spooler already
//! keeps each RAW document atomic, so bytes of two jobs never interleave; this
//! lock additionally makes the ORDER of submissions and their log lines
//! deterministic — one source talks to the printer at a time.
//!
//! It is a `std` mutex on purpose: it is taken INSIDE the blocking print task
//! (`spawn_blocking`) and held for that task's whole life, so a print whose
//! outer timeout fired (and is still unwinding after cancellation) keeps the
//! printer until it really stops touching it — the same reasoning as the #51
//! in-flight guard.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tracing::info;

/// Cheap-to-clone handle to the one client-wide print mutex.
#[derive(Clone, Default)]
pub struct PrintLock {
    inner: Arc<Mutex<()>>,
}

impl PrintLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Block until this source owns the printer. `source` ("grpc" / "odoo")
    /// and `job_id` are only used for the log lines. A poisoned lock (a print
    /// task panicked while holding it) is recovered: the unit value carries
    /// no state that could be inconsistent.
    pub fn hold(&self, source: &str, job_id: &str) -> MutexGuard<'_, ()> {
        let started = Instant::now();
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let waited_ms = started.elapsed().as_millis() as u64;
        info!(source, job_id, waited_ms, "print lock acquired");
        guard
    }

    /// True while some source holds the printer (diagnostics/tests).
    pub fn is_held(&self) -> bool {
        self.inner.try_lock().is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn test_hold_marks_the_lock_held_until_the_guard_drops() {
        let lock = PrintLock::new();
        assert!(!lock.is_held());
        let guard = lock.hold("grpc", "job-1");
        assert!(lock.is_held());
        drop(guard);
        assert!(!lock.is_held());
    }

    #[test]
    fn test_clones_share_one_mutex_so_sources_serialize() {
        let grpc = PrintLock::new();
        let odoo = grpc.clone();
        let guard = grpc.hold("grpc", "job-1");
        assert!(odoo.is_held(), "a clone must see the same lock");

        let entered = Arc::new(AtomicBool::new(false));
        let entered_t = Arc::clone(&entered);
        let t = std::thread::spawn(move || {
            let _g = odoo.hold("odoo", "odoo-batch-1");
            entered_t.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !entered.load(Ordering::SeqCst),
            "second source must wait while the first holds the printer"
        );
        drop(guard);
        t.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn test_poisoned_lock_is_recovered() {
        let lock = PrintLock::new();
        let l2 = lock.clone();
        let _ = std::thread::spawn(move || {
            let _g = l2.hold("grpc", "job-panics");
            panic!("print task panicked while holding the printer");
        })
        .join();
        let _g = lock.hold("odoo", "odoo-batch-2");
        assert!(lock.is_held());
    }
}
