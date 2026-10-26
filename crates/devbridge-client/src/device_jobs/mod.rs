//! Odoo device-job source for the store's pokladňa (#99): eKasa receipts and
//! card payments that Odoo queues for this bridge and the store's devices run
//! — EXACTLY ONCE (contract: odoo-erp `addons/company_pokladna_api/docs/
//! device_job_api.md`).
//!
//! Runs next to the gRPC receiver and the label source, needs no pz-server:
//!
//! - **one control loop** owns `pending` and `next` — they never run at the
//!   same time: `pending` at start, after any failed `next` (its response,
//!   and a claimed job with it, may be lost) and again after a `too_early`;
//!   then `next` as a long-poll (`wait_s`, HTTP timeout `wait_s + 10` s;
//!   a reply with `wait_s: 0` → sleep 1–2 s). A claimed job is written to the
//!   durable [`journal::Journal`] as `received` BEFORE any device contact;
//!   a job the journal does not know is acked `not_received` from `pending`
//!   (nothing ran), never while a `next` is in flight;
//! - jobs are refused without device contact when invalid (`card.purchase`
//!   amount not a finite number > 0 → `bad_amount`) or when no executor runs
//!   their type (`unsupported`);
//! - **one serial worker per device** ([`executor::DeviceQueue`]: device
//!   kind + the device's address, e.g. one Portos per till, #100) calls the
//!   [`executor::DeviceExecutor`] — a card PIN wait never blocks a receipt,
//!   a slow till never blocks another; a device never runs two jobs at once;
//!   a job that waited in its queue longer than `max_queue_wait` is refused
//!   `expired_local` (nothing ran);
//! - an unknown outcome (timeout, crash, restart mid-job) is acked
//!   `indeterminate` and **reconciled through the device** on an interval
//!   until it answers, then acked late — a journaled job is never executed
//!   again;
//! - acks are journaled and re-sent until Odoo accepts them (`duplicate` =
//!   accepted: the first final ack wins);
//! - a heartbeat (`bridge_version`, `device_status` = `portos=…;
//!   terminal=…`) every `heartbeat_interval_secs` and on a status change;
//!   the devices are probed for it every `status_refresh`.
//!
//! Logs name every job (id, type, till, client_uuid, sale) and its state
//! changes; of the payload only `amount` / `currency` ever appear, the API
//! key never.
//!
//! **LAN API (#101):** `[client.pokladna.lan_api]` adds a second entry: the
//! tills' local HTTPS API ([`crate::lan_api`]) hands `fiscal.*` jobs to
//! [`lan::LanJobs`], which journals them in `lan_jobs` and queues them on the
//! SAME device workers (negative job ids, [`journal::lan_job_id`]); the
//! reporter task tells Odoo about each one (`report_offline`) once Odoo
//! answers. The reporter runs whenever the source runs, so a job journaled
//! before the LAN API was switched off is still reported.

pub mod api;
pub mod executor;
pub mod journal;
pub mod lan;
pub mod portos;
pub mod report_offline;
mod tasks;
pub mod test_executor;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use devbridge_core::config::{PokladnaClientConfig, redact_secret};

use api::{
    AckRequest, BridgeApi, DeviceJob, NextReply, PendingJob, REFUSED_TOO_EARLY, next_timeout,
};
use executor::{DeviceKind, DeviceQueue, ExecutorRegistry, Outcome, validate};
use journal::{Inserted, Journal};
use portos::{PortosConfig, PortosExecutor};
use tasks::WorkItem;
use test_executor::{TestDevice, TestExecutor};

use crate::lan_api::LanApiDeps;
use crate::odoo_rpc::RpcError;

pub use tasks::{Backoff, heartbeat_due, queue_wait_exceeded};

/// Journal file in the client data dir.
pub const JOURNAL_FILE: &str = "device-journal.db";
/// The `test` executor's device log in the client data dir (E2E only).
pub const TEST_DEVICE_FILE: &str = "test-device.db";

/// Every delay of the source. [`Timing::default`] is the production / contract
/// set; tests shrink it.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Sleep after a `next` reply with `wait_s: 0` (contract: 1–2 s).
    pub short_poll_sleep: Duration,
    /// A `next` that came back empty faster than this did not really wait
    /// (a misbehaving server) — sleep like after `wait_s: 0`, never spin.
    pub min_long_poll: Duration,
    /// Added to `wait_s` for the HTTP timeout of `next` (contract: 10 s).
    pub next_timeout_margin: Duration,
    /// HTTP timeout of `ack` / `pending` / `heartbeat`.
    pub call_timeout: Duration,
    /// First retry delay after a failed call; doubles up to `backoff_max`.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Ask `pending` again this long after a `too_early`.
    pub pending_retry: Duration,
    /// How often `indeterminate` jobs are reconciled with their device.
    pub reconcile_interval: Duration,
    /// An executor call running longer is reported `indeterminate` early
    /// (below Odoo's 300 s claim timeout); its device stays blocked until the
    /// call returns.
    pub execute_guard: Duration,
    /// Owed acks are re-checked at least this often.
    pub ack_idle_tick: Duration,
    /// How often the device status is compared for an on-change heartbeat.
    pub status_check: Duration,
    /// How often the devices are probed for that status (#100).
    pub status_refresh: Duration,
    /// A job that waited longer than this in its device queue is not run
    /// any more: acked `failed` `expired_local`, nothing ran (#100).
    pub max_queue_wait: Duration,
    /// How long a LAN API call waits for its job before it answers 202 and
    /// the till polls (#101) — above Portos' register timeout (60 s) plus
    /// the lookup before it.
    pub lan_answer_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            short_poll_sleep: Duration::from_millis(1500),
            min_long_poll: Duration::from_secs(1),
            next_timeout_margin: Duration::from_secs(10),
            call_timeout: Duration::from_secs(30),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            pending_retry: Duration::from_secs(15),
            reconcile_interval: Duration::from_secs(30),
            execute_guard: Duration::from_secs(240),
            ack_idle_tick: Duration::from_secs(30),
            status_check: Duration::from_secs(1),
            status_refresh: Duration::from_secs(15),
            max_queue_wait: Duration::from_secs(120),
            lan_answer_wait: Duration::from_secs(90),
        }
    }
}

/// How long to wait before the next `next` call after a successful one.
pub fn delay_after_next(reply: &NextReply, elapsed: Duration, timing: &Timing) -> Duration {
    if reply.job.is_some() {
        Duration::ZERO
    } else if reply.wait_s == 0 || elapsed < timing.min_long_poll {
        timing.short_poll_sleep
    } else {
        Duration::ZERO
    }
}

/// When the next `pending` round is due after a successful one: only after
/// a `too_early`, `pending_retry` from `now`.
pub fn pending_due_after(retry_later: bool, now: Instant, timing: &Timing) -> Option<Instant> {
    retry_later.then(|| now + timing.pending_retry)
}

/// Whether a `pending` round is due at `now` (due exactly at its instant).
pub fn pending_round_due(pending_due: Option<Instant>, now: Instant) -> bool {
    pending_due.is_some_and(|due| now >= due)
}

/// State shared by the control loop and the background tasks.
pub(crate) struct Shared {
    api: BridgeApi,
    journal: Arc<Journal>,
    registry: ExecutorRegistry,
    timing: Timing,
    /// Wakes the ack sender.
    acks_due: Notify,
    /// Wakes the LAN job reporter (#101).
    reports_due: Notify,
    /// Changes with every recorded outcome: wakes the LAN API calls waiting
    /// for their job (#101).
    outcomes: watch::Sender<()>,
    /// Jobs with a reconcile queued or running.
    reconciling: Mutex<HashSet<i64>>,
    queues: HashMap<DeviceQueue, UnboundedSender<WorkItem>>,
}

impl Shared {
    /// Acks are owed (again): wake the ack sender.
    fn ack_soon(&self) {
        self.acks_due.notify_one();
    }

    /// A job's outcome was journaled: wake the ack sender, the LAN reporter
    /// and every LAN API call waiting for its job.
    fn outcome_recorded(&self) {
        self.ack_soon();
        self.reports_due.notify_one();
        self.outcomes.send_replace(());
    }

    /// The serial queue of the device `job` runs on.
    fn queue_for(&self, job: &api::DeviceJob) -> Option<&UnboundedSender<WorkItem>> {
        self.queues.get(&self.registry.queue_of(job)?)
    }

    /// `true` = the caller queues the reconcile (none queued for it yet).
    fn start_reconcile(&self, job_id: i64) -> bool {
        self.reconciling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job_id)
    }

    fn reconcile_done(&self, job_id: i64) {
        self.reconciling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&job_id);
    }
}

/// Build the executors `[client.pokladna]` asks for: the `test` executor
/// for both kinds (`test_executor = true`: the E2E, or a store without
/// Portos, #101), else
/// the Portos eKasa executor for `fiscal.*` (#100) — each till's
/// `portos_url`, default `http://localhost:3010` — which journals its device
/// contacts in the source's own `journal`. Card jobs stay `unsupported`
/// until the TIM executor (#102).
pub fn registry_from_config(
    cfg: &PokladnaClientConfig,
    data_dir: &Path,
    journal: &Arc<Journal>,
) -> Result<ExecutorRegistry> {
    let mut registry = ExecutorRegistry::default();
    if cfg.test_executor {
        let device = Arc::new(
            TestDevice::open(&data_dir.join(TEST_DEVICE_FILE))
                .context("test executor device log")?,
        );
        for kind in DeviceKind::ALL {
            registry.register(Arc::new(TestExecutor::new(kind, Arc::clone(&device))))?;
        }
    } else {
        let portos = PortosExecutor::new(PortosConfig::from_pokladna(cfg), Arc::clone(journal))
            .context("Portos executor")?;
        registry.register(Arc::new(portos))?;
    }
    Ok(registry)
}

pub struct DeviceJobSource {
    cfg: PokladnaClientConfig,
    api: BridgeApi,
    journal: Arc<Journal>,
    registry: ExecutorRegistry,
    timing: Timing,
    /// Set by [`Self::with_lan_api`]: run the tills' LAN API (#101).
    lan_api: Option<LanApiDeps>,
}

impl DeviceJobSource {
    pub fn new(
        cfg: PokladnaClientConfig,
        journal: Arc<Journal>,
        registry: ExecutorRegistry,
        timing: Timing,
    ) -> Result<Self> {
        let api = BridgeApi::new(&cfg.url, &cfg.api_key, timing.call_timeout)
            .context("Odoo device-job HTTP client")?;
        Ok(Self {
            cfg,
            api,
            journal,
            registry,
            timing,
            lan_api: None,
        })
    }

    /// Also run the tills' LAN API (`[client.pokladna.lan_api]`, #101) — the
    /// caller decides (`lan_api.enabled`); the reporter runs either way.
    pub fn with_lan_api(mut self, deps: LanApiDeps) -> Self {
        self.lan_api = Some(deps);
        self
    }

    /// Run until `shutdown` fires: restart recovery, the workers, the ack
    /// sender, the reconciler, the heartbeat and the `pending`/`next` loop.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        info!(
            url = %self.cfg.url,
            api_key = redact_secret(&self.cfg.api_key),
            wait_s = self.cfg.wait_s,
            heartbeat_secs = self.cfg.heartbeat_interval_secs,
            executors = ?self.registry,
            test_executor = self.cfg.test_executor,
            "Odoo device-job source starting (pokladna: eKasa + card, exactly once)"
        );
        for (job_id, previous) in self.journal.recover_after_restart()? {
            warn!(
                job_id,
                previous_state = previous.as_str(),
                "device job was in flight when devbridge stopped - NOT executed again, reconciling it with the device"
            );
        }
        match self.journal.prune() {
            Ok(pruned) => info!(pruned, "device journal: old acked jobs pruned"),
            Err(e) => warn!(error = %format!("{e:#}"), "device journal prune failed"),
        }

        let mut queues = HashMap::new();
        let mut workers = Vec::new();
        for (queue, executor) in self.registry.queues() {
            let (tx, rx) = unbounded_channel();
            info!(device = %queue, executor = executor.name(), "device queue ready");
            queues.insert(queue.clone(), tx);
            workers.push((queue, executor, rx));
        }
        let heartbeat_every = Duration::from_secs(self.cfg.heartbeat_interval_secs.max(1));
        let wait_s = self.cfg.wait_s;
        let shared = Arc::new(Shared {
            api: self.api,
            journal: self.journal,
            registry: self.registry,
            timing: self.timing,
            acks_due: Notify::new(),
            reports_due: Notify::new(),
            outcomes: watch::channel(()).0,
            reconciling: Mutex::new(HashSet::new()),
            queues,
        });
        let mut handles = Vec::new();
        for (queue, executor, rx) in workers {
            handles.push(tokio::spawn(tasks::device_worker(
                Arc::clone(&shared),
                queue,
                executor,
                rx,
            )));
        }
        handles.push(tokio::spawn(tasks::acker(Arc::clone(&shared))));
        handles.push(tokio::spawn(tasks::reporter(Arc::clone(&shared))));
        if let Some(deps) = self.lan_api {
            let cfg = self.cfg.clone();
            let jobs = lan::LanJobs::new(Arc::clone(&shared));
            handles.push(tokio::spawn(async move {
                if let Err(e) = crate::lan_api::run(&cfg, jobs, deps).await {
                    error!(error = %format!("{e:#}"), "LAN API stopped with an error - the Odoo channel keeps running");
                }
            }));
        }
        handles.push(tokio::spawn(tasks::reconciler(Arc::clone(&shared))));
        handles.push(tokio::spawn(tasks::status_refresher(Arc::clone(&shared))));
        handles.push(tokio::spawn(tasks::heartbeat(
            Arc::clone(&shared),
            heartbeat_every,
        )));

        control_loop(&shared, wait_s, &shutdown).await;

        for handle in handles {
            handle.abort();
        }
        info!("Odoo device-job source stopped");
        Ok(())
    }
}

/// Sleep `delay`; `true` = shutdown fired.
async fn sleep_or_shutdown(delay: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// The ONE loop that talks `pending` and `next` — never both at once.
async fn control_loop(shared: &Shared, wait_s: u64, shutdown: &CancellationToken) {
    let timing = &shared.timing;
    let mut backoff = Backoff::new(timing.backoff_base, timing.backoff_max);
    // `pending` first: a previous process may have claimed jobs it never acked.
    let mut pending_due: Option<Instant> = Some(Instant::now());
    loop {
        if pending_round_due(pending_due, Instant::now()) {
            let pass = tokio::select! {
                _ = shutdown.cancelled() => return,
                pass = pending_pass(shared) => pass,
            };
            match pass {
                Ok(retry_later) => {
                    backoff.reset();
                    pending_due = pending_due_after(retry_later, Instant::now(), timing);
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    warn!(error = %format!("{e:#}"), retry_in_ms = delay.as_millis() as u64, "device jobs: pending failed - no next before it succeeds");
                    if sleep_or_shutdown(delay, shutdown).await {
                        return;
                    }
                    continue;
                }
            }
        }

        let started = Instant::now();
        let timeout = next_timeout(wait_s, timing.next_timeout_margin);
        let reply = tokio::select! {
            _ = shutdown.cancelled() => return,
            reply = shared.api.next(wait_s, timeout) => reply,
        };
        let delay = match reply {
            Ok(reply) => {
                backoff.reset();
                let accepted = reply.job.clone().map(|job| accept_job(shared, job));
                if let Some(Err(e)) = accepted {
                    error!(error = %format!("{e:#}"), "device job could NOT be journaled - not executed; pending will answer it");
                    pending_due = Some(Instant::now());
                }
                delay_after_next(&reply, started.elapsed(), timing)
            }
            Err(e) => {
                // This call's response — and a job claimed with it — may be
                // lost: reconcile through `pending` before the next `next`.
                pending_due = Some(Instant::now());
                let delay = backoff.next_delay();
                warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "device jobs: next failed - pending runs before the next call");
                delay
            }
        };
        if sleep_or_shutdown(delay, shutdown).await {
            return;
        }
    }
}

/// A job `next` handed out: journal it (BEFORE any device contact), refuse
/// it without device contact when invalid, else queue it on its device.
fn accept_job(shared: &Shared, job: DeviceJob) -> Result<()> {
    let job_id = job.id;
    let verdict = validate(&job, &shared.registry);
    let inserted = match &verdict {
        Ok(_) => shared.journal.insert_received(&job)?,
        Err(rejection) => shared
            .journal
            .insert_refused(&job, rejection.code, &rejection.text)?,
    };
    if let Inserted::Known(known) = inserted {
        error!(
            job_id,
            state = known.state.as_str(),
            "Odoo handed out an already journaled device job again - NOT executed again, its outcome is re-sent"
        );
        shared.journal.reopen_ack(job_id)?;
        shared.ack_soon();
        return Ok(());
    }
    info!(
        job_id,
        job_type = %job.job_type,
        kasa_id = ?job.kasa_id,
        kasa = %job.kasa_name,
        client_uuid = %job.client_uuid,
        pos_order = %job.pos_order_name(),
        money = %job.money(),
        claimed_at = %job.claimed_at,
        "device job received (journaled)"
    );
    match verdict {
        Ok(kind) => {
            let item = WorkItem::Execute {
                job_id,
                queued_at: Instant::now(),
            };
            let queued = shared
                .queue_for(&job)
                .is_some_and(|queue| queue.send(item).is_ok());
            if !queued {
                // Its worker is gone; it never reached the device.
                shared.journal.record_outcome(
                    job_id,
                    &Outcome::failed(
                        "bridge_error",
                        format!("the devbridge {kind} worker is not running - nothing ran"),
                    ),
                )?;
                shared.outcome_recorded();
                error!(job_id, device = %kind, "device worker gone - job acked failed, nothing ran");
            }
        }
        Err(rejection) => {
            shared.outcome_recorded();
            warn!(job_id, job_type = %job.job_type, code = rejection.code, reason = %rejection.text, "device job refused before any device contact");
        }
    }
    Ok(())
}

/// One `pending` round (control loop only). Returns `true` when Odoo said
/// `too_early` for a `not_received` — ask again later.
async fn pending_pass(shared: &Shared) -> Result<bool> {
    let (jobs, skipped) = shared.api.pending().await?;
    for entry in &skipped {
        warn!(entry = %entry, "pending entry without a job id - cannot answer it");
    }
    let mut retry_later = false;
    for p in &jobs {
        match shared.journal.get(p.id)? {
            Some(known) => {
                // Odoo has no final ack of it: owe the stored outcome again
                // (no-op unless it was marked accepted); unknown outcomes are
                // the reconciler's, running jobs their worker's.
                let reopened = shared.journal.reopen_ack(p.id)?;
                info!(job_id = p.id, odoo_state = %p.state, journal_state = known.state.as_str(), reopened, "pending job is journaled - its outcome is (re-)sent, never executed again");
            }
            None => retry_later |= ack_not_received(shared, p).await?,
        }
    }
    shared.ack_soon();
    info!(
        pending = jobs.len(),
        retry_later, "device jobs: pending reconciled"
    );
    Ok(retry_later)
}

/// Ack `not_received` for a pending job the journal never saw. `Ok(true)` =
/// ask `pending` again later: `too_early` (the `next` response may still be
/// on its way) or a JSON-RPC / protocol error on this one ack. Only a
/// transport error fails the round (Odoo unreachable) — one bad answer must
/// never stop `next` for good.
async fn ack_not_received(shared: &Shared, p: &PendingJob) -> Result<bool> {
    match shared
        .api
        .ack(&AckRequest::NotReceived { job_id: p.id })
        .await
    {
        Ok(reply) => {
            warn!(
                job_id = p.id,
                job_type = %p.job_type,
                client_uuid = %p.client_uuid,
                claimed_at = %p.claimed_at,
                duplicate = reply.duplicate,
                odoo_state = %reply.state,
                "device job claimed but never received (lost next response) - acked not_received, nothing ran"
            );
            Ok(false)
        }
        Err(e) if e.refusal_code() == Some(REFUSED_TOO_EARLY) => {
            info!(
                job_id = p.id,
                "not_received too early - its next response may still arrive, asking pending again later"
            );
            Ok(true)
        }
        Err(e) if !e.is_retryable() => {
            error!(job_id = p.id, job_type = %p.job_type, refusal = %e, "Odoo refused not_received for a job missing from the journal - a person must reconcile it");
            Ok(false)
        }
        Err(e @ RpcError::Transport(_)) => Err(e.into()),
        Err(e) => {
            warn!(job_id = p.id, error = %e, "not_received ack failed - asking pending again later");
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(job: bool, wait_s: u64) -> NextReply {
        NextReply {
            job: job.then(|| {
                api::parse_job(&serde_json::json!({"id": 1, "type": "fiscal.cash_register"}))
                    .unwrap()
            }),
            wait_s,
            kasy: Vec::new(),
        }
    }

    #[test]
    fn test_default_timing_matches_the_contract() {
        let t = Timing::default();
        assert!((Duration::from_secs(1)..=Duration::from_secs(2)).contains(&t.short_poll_sleep));
        assert_eq!(t.next_timeout_margin, Duration::from_secs(10));
        assert_eq!(
            next_timeout(25, t.next_timeout_margin),
            Duration::from_secs(35)
        );
        assert!(
            t.execute_guard < Duration::from_secs(300),
            "below Odoo's claim timeout"
        );
        assert_eq!(t.min_long_poll, Duration::from_secs(1));
        assert_eq!(t.call_timeout, Duration::from_secs(30));
        assert_eq!(
            (t.backoff_base, t.backoff_max),
            (Duration::from_secs(1), Duration::from_secs(30))
        );
        assert_eq!(t.pending_retry, Duration::from_secs(15));
        assert_eq!(t.reconcile_interval, Duration::from_secs(30));
        assert_eq!(t.ack_idle_tick, Duration::from_secs(30));
        assert_eq!(t.status_check, Duration::from_secs(1));
        assert_eq!(t.status_refresh, Duration::from_secs(15));
        assert_eq!(t.max_queue_wait, Duration::from_secs(120));
        assert_eq!(t.lan_answer_wait, Duration::from_secs(90));
    }

    #[test]
    fn test_pending_round_due() {
        let now = Instant::now();
        assert!(!pending_round_due(None, now));
        assert!(pending_round_due(Some(now), now), "due at its own instant");
        assert!(pending_round_due(Some(now), now + Duration::from_millis(1)));
        assert!(!pending_round_due(
            Some(now + Duration::from_millis(1)),
            now
        ));
    }

    #[test]
    fn test_pending_due_after() {
        let t = Timing::default();
        let now = Instant::now();
        assert_eq!(pending_due_after(false, now, &t), None);
        assert_eq!(
            pending_due_after(true, now, &t),
            Some(now + Duration::from_secs(15))
        );
    }

    #[test]
    fn test_delay_after_next() {
        let t = Timing::default();
        let long = Duration::from_secs(25);
        // a job: ask again at once
        assert_eq!(delay_after_next(&reply(true, 25), long, &t), Duration::ZERO);
        assert_eq!(
            delay_after_next(&reply(true, 0), Duration::ZERO, &t),
            Duration::ZERO
        );
        // the long-poll waited: ask again at once
        assert_eq!(
            delay_after_next(&reply(false, 25), long, &t),
            Duration::ZERO
        );
        // no waiting slot: sleep 1-2 s
        assert_eq!(
            delay_after_next(&reply(false, 0), long, &t),
            t.short_poll_sleep
        );
        // came back empty at once although it may wait: never spin
        assert_eq!(
            delay_after_next(&reply(false, 25), Duration::from_millis(30), &t),
            t.short_poll_sleep
        );
        assert_eq!(
            delay_after_next(&reply(false, 25), t.min_long_poll, &t),
            Duration::ZERO
        );
    }

    /// A device that is never asked anything (queue routing tests).
    struct Idle(DeviceKind);

    #[async_trait::async_trait]
    impl executor::DeviceExecutor for Idle {
        fn kind(&self) -> DeviceKind {
            self.0
        }
        fn name(&self) -> &str {
            "idle"
        }
        async fn execute(&self, _job: &api::DeviceJob) -> Outcome {
            unreachable!("routing tests never execute")
        }
        async fn reconcile(&self, _entry: &journal::JournalEntry) -> executor::Reconciliation {
            unreachable!("routing tests never reconcile")
        }
        fn status(&self) -> String {
            "idle".into()
        }
    }

    fn shared_with(
        journal: Arc<Journal>,
        kinds: &[DeviceKind],
    ) -> (Shared, Vec<tokio::sync::mpsc::UnboundedReceiver<WorkItem>>) {
        let mut queues = HashMap::new();
        let mut receivers = Vec::new();
        let mut registry = ExecutorRegistry::default();
        for kind in kinds {
            let (tx, rx) = unbounded_channel();
            queues.insert(DeviceQueue::new(*kind, ""), tx);
            receivers.push(rx);
            registry.register(Arc::new(Idle(*kind))).unwrap();
        }
        let shared = Shared {
            api: BridgeApi::new("http://127.0.0.1:9", "k", Duration::from_secs(1)).unwrap(),
            journal,
            registry,
            timing: Timing::default(),
            acks_due: Notify::new(),
            reports_due: Notify::new(),
            outcomes: watch::channel(()).0,
            reconciling: Mutex::new(HashSet::new()),
            queues,
        };
        (shared, receivers)
    }

    fn indeterminate_row(journal: &Journal, id: i64, job_type: &str) {
        let job = api::parse_job(&serde_json::json!({"id": id, "type": job_type})).unwrap();
        journal.insert_received(&job).unwrap();
        journal
            .record_outcome(id, &Outcome::Indeterminate("timeout".into()))
            .unwrap();
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkItem>) -> Vec<WorkItem> {
        let mut items = Vec::new();
        while let Ok(item) = rx.try_recv() {
            items.push(item);
        }
        items
    }

    #[test]
    fn test_one_reconcile_per_job_at_a_time() {
        let journal = Arc::new(Journal::open_in_memory().unwrap());
        indeterminate_row(&journal, 7, "card.purchase");
        let (shared, mut rx) = shared_with(Arc::clone(&journal), &[DeviceKind::Card]);
        tasks::schedule_reconciles(&shared).unwrap();
        tasks::schedule_reconciles(&shared).unwrap();
        assert_eq!(
            drain(&mut rx[0]),
            vec![WorkItem::Reconcile(7)],
            "queued once"
        );
        shared.reconcile_done(7);
        tasks::schedule_reconciles(&shared).unwrap();
        assert_eq!(
            drain(&mut rx[0]),
            vec![WorkItem::Reconcile(7)],
            "queued again after it ran"
        );
        assert_eq!(journal.get(7).unwrap().unwrap().reconcile_attempts, 0);
    }

    #[test]
    fn test_unknown_outcome_without_executor_is_reported_once() {
        let journal = Arc::new(Journal::open_in_memory().unwrap());
        indeterminate_row(&journal, 8, "card.purchase");
        journal.mark_reported(8).unwrap();
        indeterminate_row(&journal, 9, "card.balance");
        // the restart case: unknown outcome, nothing reported yet
        journal
            .insert_received(
                &api::parse_job(&serde_json::json!({"id": 10, "type": "card.purchase"})).unwrap(),
            )
            .unwrap();
        journal.recover_after_restart().unwrap();
        let (shared, _rx) = shared_with(Arc::clone(&journal), &[DeviceKind::Fiscal]);
        tasks::schedule_reconciles(&shared).unwrap();
        tasks::schedule_reconciles(&shared).unwrap();
        let ten = journal.get(10).unwrap().unwrap();
        assert_eq!(ten.report, journal::Report::Owed);
        assert_eq!(ten.reconcile_attempts, 1, "reported once, not every round");
        // already owed / sent: left alone
        assert_eq!(journal.get(8).unwrap().unwrap().reconcile_attempts, 0);
        assert_eq!(journal.get(9).unwrap().unwrap().reconcile_attempts, 0);
        assert_eq!(
            journal.owed_acks().unwrap().len(),
            2,
            "9 and 10 owe the indeterminate ack"
        );
    }

    #[test]
    fn test_registry_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open_in_memory().unwrap());
        let store =
            registry_from_config(&PokladnaClientConfig::default(), dir.path(), &journal).unwrap();
        assert_eq!(
            store.kinds(),
            vec![DeviceKind::Fiscal],
            "Portos only, no TIM yet"
        );
        assert_eq!(store.get(DeviceKind::Fiscal).unwrap().name(), "portos");
        assert_eq!(store.device_status(), "portos=unknown; terminal=none");
        assert!(!dir.path().join(TEST_DEVICE_FILE).exists());

        let e2e_dir = tempfile::tempdir().unwrap();
        let cfg = PokladnaClientConfig {
            test_executor: true,
            ..Default::default()
        };
        let on = registry_from_config(&cfg, e2e_dir.path(), &journal).unwrap();
        assert_eq!(on.kinds(), vec![DeviceKind::Fiscal, DeviceKind::Card]);
        assert_eq!(on.device_status(), "portos=test; terminal=test");
        assert_eq!(on.get(DeviceKind::Fiscal).unwrap().name(), "test");
        assert!(e2e_dir.path().join(TEST_DEVICE_FILE).exists());
    }
}
