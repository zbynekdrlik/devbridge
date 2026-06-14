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
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
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

/// Outcome of [`run_print_task_with_timeout`].
#[derive(Debug)]
pub(crate) enum PrintDispatch {
    /// The print task ran to completion within the timeout. Carries the
    /// backend's own `Result`.
    Completed(Result<()>),
    /// The print task exceeded the configured per-job timeout. The
    /// cancellation token has been signalled so the (now abandoned) blocking
    /// task stops touching the printer; the receiver reports failure to the
    /// server.
    TimedOut,
    /// A print task for this `job_id` was already in flight. The second
    /// dispatch was suppressed (issue #51 in-flight guard) and NOTHING was
    /// sent to the printer — no double-stream.
    DuplicateSuppressed,
}

/// Run one print task under the per-job outer timeout, with cancellation and a
/// double-dispatch guard (issue #51).
///
/// Behaviour:
/// 1. Acquire the in-flight slot for `job_id`. If a print for this `job_id` is
///    already running, return [`PrintDispatch::DuplicateSuppressed`] WITHOUT
///    spawning anything — the receiver must never start a second concurrent
///    print for the same job (two racing IPP streams = partial duplicates).
/// 2. Spawn the blocking print work, passing it the `CancellationToken`.
/// 3. Wait up to `print_timeout`. If the task finishes first, return its
///    result. If the timeout fires, CANCEL the token so the abandoned blocking
///    thread observes cancellation and stops, and return
///    [`PrintDispatch::TimedOut`].
///
/// `make_print` builds and runs the blocking print; it receives the token so
/// the backend can poll it. The returned [`InFlightGuard`] is moved into the
/// spawned task so the slot is released exactly when the (possibly cancelled)
/// task finishes unwinding.
pub(crate) async fn run_print_task_with_timeout<F>(
    inflight: &InFlightJobs,
    job_id: &str,
    print_timeout: Duration,
    cancel: CancellationToken,
    make_print: F,
) -> PrintDispatch
where
    F: FnOnce(CancellationToken) -> Result<()> + Send + 'static,
{
    let guard = match inflight.try_begin(job_id) {
        Some(g) => g,
        None => {
            // A prior task for this job_id is still alive (e.g. an abandoned,
            // cancelled-but-not-yet-unwound task). Refuse the second dispatch
            // — defense-in-depth against the server-requeue double-print. The
            // try_begin call already logs the suppression with the job_id.
            return PrintDispatch::DuplicateSuppressed;
        }
    };

    let task_cancel = cancel.clone();
    let print_handle = tokio::task::spawn_blocking(move || {
        // Hold the guard for the whole life of the blocking task (incl. an
        // abandoned/cancelled one) so the in-flight slot is freed only when
        // this task actually stops touching the printer.
        let _guard = guard;
        make_print(task_cancel)
    });

    match tokio::time::timeout(print_timeout, print_handle).await {
        Ok(join_result) => {
            let result =
                join_result.unwrap_or_else(|e| Err(anyhow::anyhow!("print task panicked: {e}")));
            PrintDispatch::Completed(result)
        }
        Err(_) => {
            // Outer timeout fired. The JoinHandle is dropped, but the blocking
            // thread keeps running until it returns on its own — so SIGNAL the
            // token. Cancellation-aware backends (ghostscript child kill,
            // direct_ipp connection drop, spooler/IPP verify loops) observe it
            // and bail, releasing the printer and the in-flight slot promptly.
            warn!(
                job_id,
                timeout_secs = print_timeout.as_secs(),
                "print task timed out — cancelling in-flight task so it stops \
                 touching the printer (issue #51)"
            );
            cancel.cancel();
            PrintDispatch::TimedOut
        }
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

    // ----------------------------------------------------------------------
    // Issue #51 regression: a backend that genuinely hangs (printer offline,
    // ghostscript stuck on a malformed PDF, IPP TLS deadlock) must, when the
    // outer per-job timeout fires:
    //   (1) have its in-flight blocking task CANCELLED (the CancellationToken
    //       set so the abandoned task stops touching the printer), and
    //   (2) NOT allow a second concurrent print for the same job_id to start
    //       (the requeue double-dispatch the server would otherwise trigger).
    // Before the fix, the outer timeout dropped the JoinHandle but never
    // signalled the task, and there was no in-flight guard — so a requeued
    // retry raced the still-running first task on the same physical printer.
    // ----------------------------------------------------------------------

    /// RED→GREEN: outer-timeout on a hung backend must cancel the in-flight
    /// task. A fake backend blocks until the cancellation token fires
    /// (simulating an EXTERNAL hang — the printer/child never returning). The
    /// dispatch helper must (a) report `TimedOut`, (b) leave the token
    /// cancelled, and (c) let the blocking task observe the cancel and exit so
    /// the in-flight slot is released.
    #[tokio::test]
    async fn test_hung_backend_is_cancelled_on_outer_timeout() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let inflight = InFlightJobs::new();
        let cancel = CancellationToken::new();
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let observed_for_task = Arc::clone(&observed_cancel);

        // Fake backend that hangs until cancelled — the issue's exact failure
        // mode (a wedged backend that never returns on its own).
        let make_print = move |token: CancellationToken| -> Result<()> {
            // Spin-wait on the cancel flag with a hard ceiling so a regression
            // (token never set) fails the test by timeout rather than hanging.
            let start = std::time::Instant::now();
            while !token.is_cancelled() {
                if start.elapsed() > Duration::from_secs(5) {
                    return Ok(()); // token was never set → bug not reproduced
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            observed_for_task.store(true, Ordering::SeqCst);
            anyhow::bail!("print cancelled by outer timeout")
        };

        let dispatch = run_print_task_with_timeout(
            &inflight,
            "job-hung",
            Duration::from_millis(50),
            cancel.clone(),
            make_print,
        )
        .await;

        assert!(
            matches!(dispatch, PrintDispatch::TimedOut),
            "a hung backend exceeding the outer timeout must report TimedOut, got {dispatch:?}"
        );
        assert!(
            cancel.is_cancelled(),
            "outer timeout MUST cancel the token so the abandoned task stops \
             touching the printer (issue #51)"
        );

        // The blocking task must observe the cancel and exit, releasing the slot.
        for _ in 0..200 {
            if inflight.is_empty() && observed_cancel.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            observed_cancel.load(Ordering::SeqCst),
            "the cancelled backend must observe the token and stop"
        );
        assert!(
            inflight.is_empty(),
            "the in-flight slot must be released once the cancelled task exits"
        );
    }

    /// RED→GREEN: while a print for a job_id is still in flight, a second
    /// dispatch for the SAME job_id (the requeue double-print) must be
    /// suppressed without sending anything to the printer.
    #[tokio::test]
    async fn test_requeue_double_dispatch_is_suppressed_while_first_in_flight() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let inflight = InFlightJobs::new();
        let prints_started = Arc::new(AtomicUsize::new(0));

        // First task: hangs until cancelled, holding the in-flight slot.
        let started1 = Arc::clone(&prints_started);
        let first = run_print_task_with_timeout(
            &inflight,
            "job-dup",
            Duration::from_millis(40),
            CancellationToken::new(),
            move |token: CancellationToken| -> Result<()> {
                started1.fetch_add(1, Ordering::SeqCst);
                let start = std::time::Instant::now();
                while !token.is_cancelled() {
                    if start.elapsed() > Duration::from_secs(2) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                anyhow::bail!("first task cancelled")
            },
        );

        // Concurrently, simulate the server requeue re-dispatching the SAME
        // job_id while the first task is still alive. It must be refused.
        let started2 = Arc::clone(&prints_started);
        let inflight2 = inflight.clone();
        let second = async move {
            // Give the first task a beat to register its in-flight slot.
            tokio::time::sleep(Duration::from_millis(5)).await;
            run_print_task_with_timeout(
                &inflight2,
                "job-dup",
                Duration::from_secs(5),
                CancellationToken::new(),
                move |_token: CancellationToken| -> Result<()> {
                    started2.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
        };

        let (first_outcome, second_outcome) = tokio::join!(first, second);

        assert!(
            matches!(first_outcome, PrintDispatch::TimedOut),
            "first (hung) task should time out, got {first_outcome:?}"
        );
        assert!(
            matches!(second_outcome, PrintDispatch::DuplicateSuppressed),
            "a second dispatch for a job_id already in flight MUST be suppressed, \
             got {second_outcome:?}"
        );
        assert_eq!(
            prints_started.load(Ordering::SeqCst),
            1,
            "only the FIRST print body may run — the duplicate must never reach \
             the printer (no double IPP stream)"
        );
    }

    /// Happy path: a print that finishes within the timeout returns
    /// `Completed` carrying the backend's own result, the token is NOT
    /// cancelled, and the in-flight slot is released afterward. Guards the
    /// non-timeout arm of the dispatch seam (and kills mutants that would
    /// always-cancel or mis-map the Completed variant).
    #[tokio::test]
    async fn test_successful_print_completes_without_cancellation() {
        let inflight = InFlightJobs::new();
        let cancel = CancellationToken::new();

        let dispatch = run_print_task_with_timeout(
            &inflight,
            "job-ok",
            Duration::from_secs(5),
            cancel.clone(),
            |_token: CancellationToken| -> Result<()> { Ok(()) },
        )
        .await;

        assert!(
            matches!(dispatch, PrintDispatch::Completed(Ok(()))),
            "a print that finishes in time must report Completed(Ok), got {dispatch:?}"
        );
        assert!(
            !cancel.is_cancelled(),
            "a successful print must NOT cancel the token"
        );
        assert!(
            inflight.is_empty(),
            "the in-flight slot must be released after a successful print"
        );
    }

    /// A backend error within the timeout is surfaced as `Completed(Err)` (the
    /// timeout did not fire) — not swallowed and not reported as TimedOut.
    #[tokio::test]
    async fn test_backend_error_within_timeout_is_surfaced_not_timed_out() {
        let inflight = InFlightJobs::new();
        let cancel = CancellationToken::new();

        let dispatch = run_print_task_with_timeout(
            &inflight,
            "job-err",
            Duration::from_secs(5),
            cancel.clone(),
            |_token: CancellationToken| -> Result<()> { anyhow::bail!("printer rejected job") },
        )
        .await;

        match dispatch {
            PrintDispatch::Completed(Err(e)) => {
                assert!(
                    e.to_string().contains("printer rejected job"),
                    "backend error detail must be preserved, got: {e}"
                );
            }
            other => {
                panic!("expected Completed(Err) for an in-time backend failure, got {other:?}")
            }
        }
        assert!(
            !cancel.is_cancelled(),
            "an in-time backend error must NOT cancel the token (the timeout did not fire)"
        );
        assert!(
            inflight.is_empty(),
            "slot released after a failed-but-in-time print"
        );
    }
}
