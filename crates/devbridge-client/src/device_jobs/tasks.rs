//! The background tasks of the device-job source (#99): one serial worker
//! per device queue (kind + device address, #100), the reconciler, the ack
//! sender, the device status refresher and the heartbeat. The `pending` /
//! `next` control loop lives in [`super::control_loop`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error, info, warn};

use super::Shared;
use super::api::{AckReply, AckRequest};
use super::executor::{CODE_EXPIRED_LOCAL, DeviceExecutor, DeviceQueue, Outcome, Reconciliation};
use super::journal::{JobState, Report};
use crate::odoo_rpc::RpcError;

/// Work for a device's serial queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkItem {
    /// Run a `received` job (at most once — the journal decides), queued at
    /// `queued_at` (it expires after `Timing::max_queue_wait`).
    Execute { job_id: i64, queued_at: Instant },
    /// Ask the device what happened to an `indeterminate` job.
    Reconcile(i64),
}

/// A job that waited in its device queue LONGER than `max` is not run any
/// more (exactly `max` still runs).
pub fn queue_wait_exceeded(queued_at: Instant, now: Instant, max: Duration) -> bool {
    now.saturating_duration_since(queued_at) > max
}

/// Doubling delay between `base` and `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            current: base,
        }
    }

    /// The delay to wait now; the next one doubles (capped at `max`).
    pub fn next_delay(&mut self) -> Duration {
        let now = self.current.min(self.max);
        self.current = (now * 2).min(self.max);
        now
    }

    pub fn reset(&mut self) {
        self.current = self.base;
    }
}

/// One device, one job at a time: jobs and reconciles of the same device are
/// strictly serial, so the device is never driven by two jobs at once.
pub(crate) async fn device_worker(
    shared: Arc<Shared>,
    queue: DeviceQueue,
    executor: Arc<dyn DeviceExecutor>,
    mut rx: UnboundedReceiver<WorkItem>,
) {
    while let Some(item) = rx.recv().await {
        let result = match item {
            WorkItem::Execute { job_id, queued_at } => {
                execute_job(&shared, &queue, &executor, job_id, queued_at).await
            }
            WorkItem::Reconcile(id) => {
                let r = reconcile_job(&shared, &queue, &executor, id).await;
                shared.reconcile_done(id);
                r
            }
        };
        if let Err(e) = result {
            error!(device = %queue, item = ?item, error = %format!("{e:#}"), "device worker: journal error");
        }
    }
}

fn outcome_of(joined: std::result::Result<Outcome, tokio::task::JoinError>) -> Outcome {
    joined.unwrap_or_else(|e| Outcome::Indeterminate(format!("executor failed mid-job: {e}")))
}

/// Run one `received` job on the device and journal what it did. A job that
/// waited in the queue longer than `Timing::max_queue_wait` is refused
/// `expired_local` instead — the device is not contacted.
pub(crate) async fn execute_job(
    shared: &Shared,
    queue: &DeviceQueue,
    executor: &Arc<dyn DeviceExecutor>,
    job_id: i64,
    queued_at: Instant,
) -> Result<()> {
    let Some(entry) = shared.journal.get(job_id)? else {
        error!(job_id, device = %queue, "device job vanished from the journal - NOT executed");
        return Ok(());
    };
    let max_wait = shared.timing.max_queue_wait;
    if queue_wait_exceeded(queued_at, Instant::now(), max_wait) {
        let waited_ms = queued_at.elapsed().as_millis() as u64;
        let text = format!(
            "the job waited {waited_ms} ms in the devbridge queue of {queue} (max {} ms) - the device was not contacted, nothing ran",
            max_wait.as_millis()
        );
        let expired = shared
            .journal
            .fail_unstarted(job_id, CODE_EXPIRED_LOCAL, &text)?;
        shared.ack_soon();
        warn!(job_id, job_type = %entry.job.job_type, device = %queue, waited_ms, expired, "device job expired in its queue - acked failed, nothing ran");
        return Ok(());
    }
    if !shared.journal.start_execution(job_id)? {
        warn!(job_id, device = %queue, state = entry.state.as_str(), "device job is not 'received' - NOT executed (at most once)");
        return Ok(());
    }
    let job = entry.job;
    info!(
        job_id,
        job_type = %job.job_type,
        kasa_id = ?job.kasa_id,
        client_uuid = %job.client_uuid,
        pos_order = %job.pos_order_name(),
        money = %job.money(),
        device = %queue,
        executor = executor.name(),
        "device job executing"
    );
    let started = Instant::now();
    let exec = Arc::clone(executor);
    let for_device = job.clone();
    let mut task = tokio::spawn(async move { exec.execute(&for_device).await });
    let guard = shared.timing.execute_guard;
    let outcome = match tokio::time::timeout(guard, &mut task).await {
        Ok(joined) => outcome_of(joined),
        Err(_) => {
            let reason = format!(
                "no answer from the {} device within {} s - outcome unknown, reconciled with the device",
                queue.kind,
                guard.as_secs()
            );
            shared
                .journal
                .record_outcome(job_id, &Outcome::Indeterminate(reason.clone()))?;
            shared.ack_soon();
            warn!(job_id, device = %queue, %reason, "device job reported indeterminate - the device queue waits for its real answer");
            // The device may still be running it: nothing else may use the
            // device until this call returns.
            outcome_of(task.await)
        }
    };
    let recorded = shared.journal.record_outcome(job_id, &outcome)?;
    shared.ack_soon();
    info!(
        job_id,
        job_type = %job.job_type,
        device = %queue,
        outcome = outcome.label(),
        detail = %outcome_detail(&outcome),
        recorded,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "device job finished"
    );
    Ok(())
}

/// Loggable part of an outcome: the error code/text, never a device result.
fn outcome_detail(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done(_) => String::new(),
        Outcome::Failed { code, text } => format!("{code}: {text}"),
        Outcome::Indeterminate(reason) => reason.clone(),
    }
}

/// Ask the device about one `indeterminate` job and journal the answer.
pub(crate) async fn reconcile_job(
    shared: &Shared,
    queue: &DeviceQueue,
    executor: &Arc<dyn DeviceExecutor>,
    job_id: i64,
) -> Result<()> {
    let Some(entry) = shared
        .journal
        .get(job_id)?
        .filter(|e| e.state == JobState::Indeterminate)
    else {
        return Ok(());
    };
    let exec = Arc::clone(executor);
    let asked = entry.clone();
    let answer = tokio::spawn(async move { exec.reconcile(&asked).await })
        .await
        .unwrap_or_else(|e| Reconciliation::StillUnknown(format!("reconcile failed: {e}")));
    let label = match &answer {
        Reconciliation::Done(_) => "done",
        Reconciliation::Failed { .. } => "failed",
        Reconciliation::StillUnknown(_) => "still unknown",
    };
    let detail = match &answer {
        Reconciliation::Done(_) => String::new(),
        Reconciliation::Failed { code, text } => format!("{code}: {text}"),
        Reconciliation::StillUnknown(reason) => reason.clone(),
    };
    match answer {
        Reconciliation::Done(result) => {
            shared
                .journal
                .record_outcome(job_id, &Outcome::Done(result))?;
        }
        Reconciliation::Failed { code, text } => {
            shared
                .journal
                .record_outcome(job_id, &Outcome::Failed { code, text })?;
        }
        Reconciliation::StillUnknown(reason) => {
            shared.journal.record_still_unknown(job_id, &reason)?;
        }
    }
    shared.ack_soon();
    info!(
        job_id,
        job_type = %entry.job.job_type,
        device = %queue,
        answer = label,
        %detail,
        attempts_before = entry.reconcile_attempts,
        "device job reconciled with the device"
    );
    Ok(())
}

/// Queue a reconcile for every `indeterminate` job not queued yet. A job no
/// executor can reconcile (its device kind is not configured) is reported
/// `indeterminate` to Odoo once and left for a person.
pub(crate) fn schedule_reconciles(shared: &Shared) -> Result<()> {
    for entry in shared.journal.indeterminate()? {
        let job_id = entry.job.id;
        match shared.queue_for(&entry.job) {
            Some((_, queue)) => {
                if shared.start_reconcile(job_id) {
                    let _ = queue.send(WorkItem::Reconcile(job_id));
                }
            }
            None if entry.report == Report::None => {
                shared.journal.record_still_unknown(
                    job_id,
                    "no executor for this device is configured - reconcile it on the device by hand",
                )?;
                shared.ack_soon();
                error!(job_id, job_type = %entry.job.job_type, "device job outcome unknown and NO executor can ask the device - reported indeterminate, a person must reconcile it");
            }
            None => {}
        }
    }
    Ok(())
}

/// Probe the devices for the heartbeat's `device_status` on an interval.
pub(crate) async fn status_refresher(shared: Arc<Shared>) {
    loop {
        shared.registry.refresh_status().await;
        tokio::time::sleep(shared.timing.status_refresh).await;
    }
}

pub(crate) async fn reconciler(shared: Arc<Shared>) {
    loop {
        if let Err(e) = schedule_reconciles(&shared) {
            warn!(error = %format!("{e:#}"), "device-job reconciler: journal error");
        }
        tokio::time::sleep(shared.timing.reconcile_interval).await;
    }
}

/// Journal Odoo's acceptance of `ack`.
fn mark_accepted(shared: &Shared, ack: &AckRequest, note: &str) -> Result<()> {
    match ack {
        AckRequest::Indeterminate { job_id, .. } => {
            shared.journal.mark_reported(*job_id)?;
        }
        other => {
            shared.journal.mark_acked(other.job_id(), note)?;
        }
    }
    Ok(())
}

fn accepted_note(reply: &AckReply) -> &'static str {
    if reply.duplicate { "duplicate" } else { "ok" }
}

/// Send every owed ack once. A transport failure stops the round (Odoo is
/// unreachable); a JSON-RPC / protocol error keeps only that ack owed; a
/// refusal is Odoo's final answer and is journaled as such.
pub(crate) async fn flush_acks(shared: &Shared) -> Result<()> {
    let mut first_error: Option<RpcError> = None;
    for ack in shared.journal.owed_acks()? {
        let job_id = ack.job_id();
        match shared.api.ack(&ack).await {
            Ok(reply) => {
                mark_accepted(shared, &ack, accepted_note(&reply))?;
                info!(job_id, ack = ack.kind(), duplicate = reply.duplicate, odoo_state = %reply.state, "device job ack accepted");
            }
            Err(e) if !e.is_retryable() => {
                mark_accepted(shared, &ack, &format!("refused: {e}"))?;
                error!(job_id, ack = ack.kind(), refusal = %e, "Odoo refused the device job ack - journaled, a person must look at this job");
            }
            Err(e @ RpcError::Transport(_)) => return Err(e.into()),
            Err(e) => {
                warn!(job_id, ack = ack.kind(), error = %e, "device job ack failed - kept owed, continuing with the others");
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    match first_error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Deliver owed acks: on every outcome (notified), on a safety tick, and
/// with backoff while Odoo is unreachable.
pub(crate) async fn acker(shared: Arc<Shared>) {
    let timing = &shared.timing;
    let mut backoff = Backoff::new(timing.backoff_base, timing.backoff_max);
    loop {
        match flush_acks(&shared).await {
            Ok(()) => {
                backoff.reset();
                tokio::select! {
                    _ = shared.acks_due.notified() => {}
                    _ = tokio::time::sleep(timing.ack_idle_tick) => {}
                }
            }
            Err(e) => {
                let delay = backoff.next_delay();
                warn!(error = %format!("{e:#}"), retry_in_ms = delay.as_millis() as u64, "device job acks not delivered - retrying");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// A heartbeat is due on the first round, when the device status changed,
/// or when `interval` passed since the last one.
pub fn heartbeat_due(
    last: Option<&(String, Instant)>,
    status: &str,
    interval: Duration,
    now: Instant,
) -> bool {
    match last {
        None => true,
        Some((sent, at)) => sent != status || now.duration_since(*at) >= interval,
    }
}

pub(crate) async fn heartbeat(shared: Arc<Shared>, interval: Duration) {
    let version = env!("CARGO_PKG_VERSION");
    let mut last: Option<(String, Instant)> = None;
    loop {
        let status = shared.registry.device_status();
        if heartbeat_due(last.as_ref(), &status, interval, Instant::now()) {
            match shared.api.heartbeat(version, &status).await {
                Ok(kasy) => {
                    debug!(bridge_version = version, device_status = %status, kasy = ?kasy, "device-job heartbeat sent")
                }
                Err(e) => {
                    warn!(error = %e, device_status = %status, "device-job heartbeat failed")
                }
            }
            last = Some((status, Instant::now()));
        }
        tokio::time::sleep(shared.timing.status_check).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn test_backoff_doubles_caps_and_resets() {
        let mut b = Backoff::new(MS(100), MS(350));
        assert_eq!(b.next_delay(), MS(100));
        assert_eq!(b.next_delay(), MS(200));
        assert_eq!(b.next_delay(), MS(350));
        assert_eq!(b.next_delay(), MS(350));
        b.reset();
        assert_eq!(b.next_delay(), MS(100));
        // a base above the cap never exceeds the cap
        let mut odd = Backoff::new(MS(500), MS(300));
        assert_eq!(odd.next_delay(), MS(300));
        assert_eq!(odd.next_delay(), MS(300));
    }

    #[test]
    fn test_queue_wait_exceeded_only_after_the_max() {
        let t0 = Instant::now();
        let max = Duration::from_secs(120);
        assert!(!queue_wait_exceeded(t0, t0, max));
        assert!(
            !queue_wait_exceeded(t0, t0 + max, max),
            "exactly max still runs"
        );
        assert!(queue_wait_exceeded(t0, t0 + max + MS(1), max));
        // a clock that seems to run backwards never expires a job
        assert!(!queue_wait_exceeded(t0 + max + MS(1), t0, max));
    }

    #[test]
    fn test_heartbeat_due_first_on_change_and_on_interval() {
        let t0 = Instant::now();
        let interval = Duration::from_secs(30);
        assert!(heartbeat_due(None, "portos=ok", interval, t0));
        let last = ("portos=ok".to_string(), t0);
        assert!(!heartbeat_due(
            Some(&last),
            "portos=ok",
            interval,
            t0 + Duration::from_secs(29)
        ));
        assert!(heartbeat_due(
            Some(&last),
            "portos=ok",
            interval,
            t0 + interval
        ));
        assert!(heartbeat_due(
            Some(&last),
            "portos=offline",
            interval,
            t0 + Duration::from_secs(1)
        ));
    }

    #[test]
    fn test_outcome_detail_never_carries_the_device_result() {
        assert_eq!(outcome_detail(&Outcome::Done(json!({"auth": "A1"}))), "");
        assert_eq!(
            outcome_detail(&Outcome::failed("declined", "zamietnutá")),
            "declined: zamietnutá"
        );
        assert_eq!(
            outcome_detail(&Outcome::Indeterminate("timeout".into())),
            "timeout"
        );
    }

    #[test]
    fn test_accepted_note() {
        assert_eq!(
            accepted_note(&AckReply {
                duplicate: true,
                state: "done".into()
            }),
            "duplicate"
        );
        assert_eq!(
            accepted_note(&AckReply {
                duplicate: false,
                state: "done".into()
            }),
            "ok"
        );
    }

    async fn panicking_executor() -> Outcome {
        panic!("executor bug")
    }

    #[tokio::test]
    async fn test_outcome_of_a_panicked_executor_is_indeterminate() {
        let joined = tokio::spawn(panicking_executor()).await;
        match outcome_of(joined) {
            Outcome::Indeterminate(reason) => {
                assert!(reason.starts_with("executor failed mid-job"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        let ok = tokio::spawn(async { Outcome::failed("a", "b") }).await;
        assert_eq!(outcome_of(ok), Outcome::failed("a", "b"));
    }
}
