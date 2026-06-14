//! In-flight job guard — defense-in-depth against double-dispatch.
//!
//! Issue #51: when the outer `tokio::time::timeout` fires on a hung backend,
//! the receiver reports failure → the server requeues → the same `job_id` is
//! re-streamed to the client while the *abandoned* `spawn_blocking` thread from
//! the prior attempt may still be running (the cancellation token tells it to
//! stop, but a wedged IPP/Ghostscript call can take a moment to actually
//! unwind). If a second print task for the same `job_id` started concurrently,
//! two `Print-Job` streams would race the same physical printer — exactly the
//! partial-duplicate pattern PR #50 fought.
//!
//! `InFlightJobs` tracks which `job_id`s currently have a live print task. The
//! receiver calls [`InFlightJobs::try_begin`] before spawning the blocking
//! print; if the id is already in flight, the second dispatch is refused and
//! reported as a suppressed duplicate. The returned [`InFlightGuard`] removes
//! the id on drop, so the slot is released the moment the (possibly abandoned)
//! task finishes unwinding — no leaked entries even on cancellation.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

/// Shared registry of `job_id`s that currently have a live print task.
///
/// Cheap to clone (`Arc` inside); clone it into the receiver loop and hand a
/// clone to each spawned print task.
#[derive(Clone, Default)]
pub struct InFlightJobs {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl InFlightJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to mark `job_id` as in flight.
    ///
    /// Returns `Some(guard)` if the job was NOT already in flight (caller may
    /// proceed to print). Returns `None` if a print task for this `job_id` is
    /// already running — the caller MUST refuse to start a second print.
    ///
    /// The guard removes the id from the set on drop.
    pub fn try_begin(&self, job_id: &str) -> Option<InFlightGuard> {
        let mut set = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if set.contains(job_id) {
            warn!(
                job_id,
                in_flight = set.len(),
                "duplicate print dispatch suppressed — a print task for this \
                 job_id is already in flight (issue #51 in-flight guard)"
            );
            return None;
        }
        set.insert(job_id.to_string());
        info!(
            job_id,
            in_flight = set.len(),
            "in-flight guard acquired for print task"
        );
        Some(InFlightGuard {
            jobs: self.clone(),
            job_id: job_id.to_string(),
        })
    }

    /// Number of jobs currently in flight (test/diagnostic helper).
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True if a print task for `job_id` is currently registered.
    pub fn contains(&self, job_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(job_id)
    }
}

/// RAII guard that releases a `job_id`'s in-flight slot when dropped.
///
/// Held by the spawned print task (including an abandoned/cancelled one) so the
/// slot is freed exactly when the task stops touching the printer.
pub struct InFlightGuard {
    jobs: InFlightJobs,
    job_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut set = self
            .jobs
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set.remove(&self.job_id);
        info!(
            job_id = %self.job_id,
            in_flight = set.len(),
            "in-flight guard released for print task"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_begin_succeeds_second_is_refused() {
        let jobs = InFlightJobs::new();
        let g1 = jobs.try_begin("job-a");
        assert!(g1.is_some(), "first dispatch for a job_id must be allowed");
        // Second concurrent begin for the SAME job_id must be refused — this is
        // the double-dispatch the receiver must never start.
        let g2 = jobs.try_begin("job-a");
        assert!(
            g2.is_none(),
            "a second print for a job_id already in flight MUST be suppressed"
        );
        assert_eq!(jobs.len(), 1, "only one slot held while first guard alive");
        assert!(jobs.contains("job-a"));
    }

    #[test]
    fn test_distinct_job_ids_run_concurrently() {
        let jobs = InFlightJobs::new();
        let _a = jobs.try_begin("job-a").expect("job-a allowed");
        let b = jobs.try_begin("job-b");
        assert!(
            b.is_some(),
            "distinct job_ids must not block each other — only same-id is guarded"
        );
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn test_slot_released_on_guard_drop_allows_relaunch() {
        let jobs = InFlightJobs::new();
        {
            let _g = jobs.try_begin("job-a").expect("first allowed");
            assert!(jobs.contains("job-a"));
        } // guard dropped here — task finished unwinding
        assert!(
            jobs.is_empty(),
            "dropping the guard must release the in-flight slot"
        );
        // After the prior task fully exits, a legitimate retry may run.
        let g = jobs.try_begin("job-a");
        assert!(
            g.is_some(),
            "once the in-flight slot is released, a retry of the same job is allowed"
        );
    }
}
