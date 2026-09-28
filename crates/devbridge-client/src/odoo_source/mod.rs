//! Odoo as a second label source, pulled by the client itself (#90).
//!
//! Runs next to the gRPC receiver and needs NO pz-server: every
//! `poll_interval_secs` it
//!
//! 1. re-sends any ack Odoo has not accepted yet (from the durable
//!    [`ledger::Ledger`] — a network drop never loses an outcome);
//! 2. calls `/food/print/next` (oldest printing batch);
//! 3. never prints a line the ledger already knows while its result is not
//!    yet stored in Odoo (it is re-acked instead) — only a line whose result
//!    Odoo ACCEPTED and then offers again (a person re-queued it) starts a
//!    new print cycle; rejects unprintable / malformed lines with an ack error
//!    (`empty png`, `size`, `invalid line: …`), encodes the rest to TSPL
//!    ([`tspl`]) as ONE spooler document for the whole batch;
//! 4. records the lines `sending`, prints the document through the
//!    configured `windows_spooler_raw` backend under the client-wide
//!    [`crate::print_lock::PrintLock`] (EventID 307 byte-count verification,
//!    per-job timeout + cancellation, #51 in-flight guard), and
//! 5. acks every line (`printed_qty` or the error, ≤ 200 chars).
//!
//! A separate task sends `/food/print/heartbeat` every
//! `heartbeat_interval_secs` with the Windows queue state. The job also shows
//! in the client dashboard history (`odoo-<batch>-<id>`, its events carry the
//! EventID 307 evidence).

pub mod ledger;
pub mod printer_status;
pub mod rpc;
pub mod tspl;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use devbridge_core::config::OdooClientConfig;
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};
use devbridge_server::queue::JobQueue;

use crate::inflight::{InFlightJobs, PrintDispatch, run_print_task_with_timeout};
use crate::print_backend::{PrintBackend, PrintJobInfo};
use crate::print_lock::PrintLock;

use ledger::{AckState, Ledger, LineState};
use rpc::{Heartbeat, NextBatch, OdooRpc, RpcError};
use tspl::{LabelGeometry, MonoBitmap};

/// Longest wait between polls while Odoo is unreachable / refusing.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Delay before the next poll right after a batch was printed (drain the
/// Odoo queue promptly instead of waiting a full poll interval).
const AFTER_BATCH_DELAY: Duration = Duration::from_secs(1);

/// Everything the source shares with the rest of the client.
pub struct OdooSourceDeps {
    /// The `windows_spooler_raw` backend in production.
    pub backend: Arc<dyn PrintBackend>,
    /// Local printer name (shared with the dashboard, read per batch).
    pub target_printer: Arc<RwLock<String>>,
    pub print_lock: PrintLock,
    /// Client job history (dashboard); `None` in tests that do not need it.
    pub queue: Option<Arc<JobQueue>>,
    /// Where the batch TSPL file lives while it is being spooled.
    pub spool_dir: PathBuf,
    pub ledger: Arc<Ledger>,
    /// Per-batch hard timeout (`[jobs].print_timeout_secs`).
    pub print_timeout: Duration,
}

/// What one poll achieved — drives the next delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// Odoo has nothing to print.
    Idle,
    /// A batch was handled: new lines printed and/or rejected.
    Batch {
        batch_id: i64,
        printed: usize,
        rejected: usize,
        reacked: usize,
    },
    /// Odoo returned only lines this client already handled (their acks were
    /// re-sent). Nothing new — back off so a stuck batch is not hammered.
    OnlyKnownLines { batch_id: i64, reacked: usize },
    /// Nothing to print, but some acks could not be delivered (Odoo answered
    /// them with a JSON-RPC error). They stay owed; back off.
    AcksOwed { owed: usize },
}

/// Delay before the next poll.
pub fn next_delay(outcome: &Result<PollOutcome>, current: Duration, base: Duration) -> Duration {
    match outcome {
        Ok(PollOutcome::Idle) => base,
        Ok(PollOutcome::Batch { .. }) => AFTER_BATCH_DELAY.min(base),
        Ok(PollOutcome::OnlyKnownLines { .. } | PollOutcome::AcksOwed { .. }) | Err(_) => {
            backoff(current, base)
        }
    }
}

/// Double the delay, never below `base`, never above [`MAX_BACKOFF`].
pub fn backoff(current: Duration, base: Duration) -> Duration {
    (current * 2).clamp(base, MAX_BACKOFF.max(base))
}

/// Ack text for a batch whose print did not confirm in time: the abandoned
/// print task may still reach the printer, so a person must look before
/// reprinting (never "failed" — that invites a double print).
pub fn timed_out_ack_text(secs: u64) -> String {
    format!(
        "outcome unknown: print not confirmed within {secs}s - check the printer before reprinting"
    )
}

/// Last batch result, reported in the heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceStatus {
    pub last_job_id: Option<i64>,
    pub last_result: String,
}

/// A line of the current batch that will be printed.
struct PrintableLine {
    line_id: i64,
    qty: i64,
    bitmap: MonoBitmap,
}

pub struct OdooSource {
    cfg: OdooClientConfig,
    rpc: OdooRpc,
    deps: OdooSourceDeps,
    geometry: LabelGeometry,
    status: Mutex<SourceStatus>,
    inflight: InFlightJobs,
}

impl OdooSource {
    pub fn new(cfg: OdooClientConfig, deps: OdooSourceDeps) -> Result<Self> {
        let rpc = OdooRpc::new(&cfg.url, &cfg.api_key).context("Odoo HTTP client")?;
        let geometry = LabelGeometry {
            width_mm: cfg.label_width_mm,
            height_mm: cfg.label_height_mm,
            dpi: cfg.dpi,
        };
        Ok(Self {
            cfg,
            rpc,
            deps,
            geometry,
            status: Mutex::new(SourceStatus::default()),
            inflight: InFlightJobs::new(),
        })
    }

    pub fn status(&self) -> SourceStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_status(&self, batch_id: i64, result: String) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        s.last_job_id = Some(batch_id);
        s.last_result = result;
    }

    /// Run until `shutdown` fires: startup ledger recovery, the heartbeat
    /// task and the poll loop.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let (w, h) = self.geometry.max_dots();
        info!(
            url = %self.cfg.url,
            api_key = devbridge_core::config::redact_secret(&self.cfg.api_key),
            printer_name = %self.cfg.printer_name,
            label_mm = %format!("{}x{}", self.cfg.label_width_mm, self.cfg.label_height_mm),
            label_dots = %format!("{w}x{h}"),
            dpi = self.cfg.dpi,
            poll_secs = self.cfg.poll_interval_secs,
            heartbeat_secs = self.cfg.heartbeat_interval_secs,
            "Odoo label source starting (no pz-server in this path)"
        );
        for line_id in self.deps.ledger.recover_interrupted()? {
            warn!(
                line_id,
                "Odoo line was mid-print when the client stopped — NOT reprinted, acked as interrupted"
            );
        }
        match self.deps.ledger.prune() {
            Ok(pruned) => info!(pruned, "Odoo ledger: old acked rows pruned"),
            Err(e) => warn!(error = %e, "Odoo ledger prune failed"),
        }

        let me = Arc::new(self);
        let heartbeat = tokio::spawn(heartbeat_loop(Arc::clone(&me), shutdown.clone()));

        let base = Duration::from_secs(me.cfg.poll_interval_secs.max(1));
        let mut delay = base;
        loop {
            let outcome = me.poll_once().await;
            delay = next_delay(&outcome, delay, base);
            match &outcome {
                Ok(PollOutcome::Idle) => debug!("Odoo: no labels waiting"),
                Ok(o) => {
                    info!(outcome = ?o, next_poll_ms = delay.as_millis() as u64, "Odoo poll handled a batch")
                }
                Err(e) => warn!(
                    error = %format!("{e:#}"),
                    retry_in_secs = delay.as_secs(),
                    "Odoo poll failed — retrying with backoff"
                ),
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        heartbeat.abort();
        info!("Odoo label source stopped");
        Ok(())
    }

    /// One poll cycle (see the module docs).
    pub async fn poll_once(&self) -> Result<PollOutcome> {
        self.flush_acks().await?;
        let batch = self.rpc.next().await?;
        let has_lines = !(batch.lines.is_empty() && batch.invalid.is_empty());
        let outcome = match batch.batch_id.filter(|_| has_lines) {
            Some(batch_id) => self.handle_batch(batch_id, &batch).await?,
            None => PollOutcome::Idle,
        };
        // Acks for everything decided above (printed, rejected, re-acked).
        let owed = self.flush_acks().await?;
        Ok(match outcome {
            PollOutcome::Idle if owed > 0 => PollOutcome::AcksOwed { owed },
            other => other,
        })
    }

    async fn handle_batch(&self, batch_id: i64, batch: &NextBatch) -> Result<PollOutcome> {
        info!(
            batch_id,
            lines = batch.lines.len(),
            production_date = %batch.production_date,
            "Odoo batch received"
        );
        let mut printable = Vec::new();
        let (mut rejected, mut reacked) = (0usize, 0usize);
        let mut seen = std::collections::HashSet::new();
        for bad in &batch.invalid {
            let Some(line_id) = bad.line_id else {
                error!(batch_id, reason = %bad.reason, "Odoo line without line_id — cannot print or ack it");
                continue;
            };
            if !seen.insert(line_id) {
                continue;
            }
            if self.handle_known_line(batch_id, line_id)? {
                reacked += 1;
                continue;
            }
            warn!(batch_id, line_id, reason = %bad.reason, "Odoo line rejected — malformed");
            let reason = rpc::truncate_ack_error(&bad.reason);
            self.deps
                .ledger
                .record_result(batch_id, line_id, 0, Some(reason.as_str()))?;
            rejected += 1;
        }
        for line in batch.lines_in_order() {
            if !seen.insert(line.line_id) {
                warn!(
                    batch_id,
                    line_id = line.line_id,
                    "Odoo sent the same line twice in one batch — ignoring the repeat"
                );
                continue;
            }
            if self.handle_known_line(batch_id, line.line_id)? {
                reacked += 1;
                continue;
            }
            if line.print_qty < 0 {
                let reason = format!("invalid print_qty {}", line.print_qty);
                warn!(batch_id, line_id = line.line_id, %reason, "Odoo line rejected");
                self.deps
                    .ledger
                    .record_result(batch_id, line.line_id, 0, Some(reason.as_str()))?;
                rejected += 1;
                continue;
            }
            if line.print_qty == 0 {
                info!(
                    batch_id,
                    line_id = line.line_id,
                    "Odoo line with print_qty 0 — nothing to print, acked done"
                );
                self.deps
                    .ledger
                    .record_result(batch_id, line.line_id, 0, None)?;
                rejected += 1;
                continue;
            }
            match tspl::decode_label(&line.label_png_base64, &self.geometry) {
                Ok(bitmap) => {
                    debug!(
                        batch_id,
                        line_id = line.line_id,
                        sequence = line.sequence,
                        line_type = %line.line_type,
                        product = %line.product_name,
                        best_before = %line.best_before,
                        qty = line.print_qty,
                        png = %format!("{}x{}", bitmap.width, bitmap.height),
                        "Odoo line encoded"
                    );
                    printable.push(PrintableLine {
                        line_id: line.line_id,
                        qty: line.print_qty,
                        bitmap,
                    });
                }
                Err(e) => {
                    warn!(
                        batch_id,
                        line_id = line.line_id,
                        line_type = %line.line_type,
                        product = %line.product_name,
                        detail = %e,
                        "Odoo line rejected — not printed"
                    );
                    self.deps.ledger.record_result(
                        batch_id,
                        line.line_id,
                        0,
                        Some(e.ack_text().as_str()),
                    )?;
                    rejected += 1;
                }
            }
        }

        if printable.is_empty() {
            if rejected == 0 {
                self.set_status(
                    batch_id,
                    format!("already handled ({reacked} lines re-acked)"),
                );
                return Ok(PollOutcome::OnlyKnownLines { batch_id, reacked });
            }
            self.set_status(
                batch_id,
                format!("nothing printable ({rejected} lines rejected)"),
            );
        } else {
            self.print_batch(batch_id, &printable).await?;
        }
        Ok(PollOutcome::Batch {
            batch_id,
            printed: printable.len(),
            rejected,
            reacked,
        })
    }

    /// Decide a line Odoo offers that the ledger may already know.
    /// Returns `true` when it must NOT be printed now (it is re-acked
    /// instead), `false` when it is new — including a line whose result Odoo
    /// ACCEPTED and now offers again: a person re-queued it in Odoo, so the
    /// old ledger row is dropped and it prints as a new cycle.
    fn handle_known_line(&self, batch_id: i64, line_id: i64) -> Result<bool> {
        let Some(known) = self.deps.ledger.get(line_id)? else {
            return Ok(false);
        };
        match (known.state, known.ack) {
            // Only a crashed previous process leaves `sending` (recovered at
            // startup); treat a straggler the same way — never reprint.
            (LineState::Sending, _) => {
                warn!(
                    batch_id,
                    line_id, "Odoo line still `sending` — closing as interrupted, not reprinting"
                );
                self.deps.ledger.record_result(
                    known.batch_id,
                    line_id,
                    0,
                    Some(ledger::INTERRUPTED_REASON),
                )?;
                Ok(true)
            }
            (LineState::Done, AckState::Accepted) => {
                info!(
                    batch_id,
                    line_id,
                    previous_printed_qty = known.printed_qty,
                    previous_error = ?known.error,
                    "Odoo re-queued a line whose result it had accepted — new print cycle"
                );
                self.deps.ledger.forget(line_id)?;
                Ok(false)
            }
            (LineState::Done, AckState::Owed | AckState::Refused) => {
                info!(
                    batch_id,
                    line_id,
                    printed_qty = known.printed_qty,
                    error = ?known.error,
                    ack = ?known.ack,
                    "Odoo offered a line whose result it has not stored — NOT reprinting, re-sending its ack"
                );
                self.deps.ledger.reopen_ack(line_id)?;
                Ok(true)
            }
        }
    }

    /// Spool the printable lines as ONE document and record each outcome.
    async fn print_batch(&self, batch_id: i64, lines: &[PrintableLine]) -> Result<()> {
        let labels: Vec<(&MonoBitmap, u32)> = lines
            .iter()
            .map(|l| (&l.bitmap, u32::try_from(l.qty).unwrap_or(u32::MAX)))
            .collect();
        let document = tspl::build_document(&self.geometry, &labels);
        let copies: i64 = lines.iter().map(|l| l.qty).sum();
        let ids: Vec<i64> = lines.iter().map(|l| l.line_id).collect();

        // Durable BEFORE the spooler sees a byte: a crash from here on can
        // never lead to a second print of these lines.
        self.deps.ledger.mark_sending(batch_id, &ids)?;

        let result = self
            .spool_document(batch_id, &document, lines.len(), copies)
            .await;
        let summary = match &result {
            Ok(evidence) => {
                for l in lines {
                    self.deps
                        .ledger
                        .record_result(batch_id, l.line_id, l.qty, None)?;
                }
                info!(batch_id, labels = lines.len(), copies, bytes = document.len(), %evidence, "Odoo batch printed");
                format!(
                    "printed {} labels ({copies} copies), {evidence}",
                    lines.len()
                )
            }
            Err(reason) => {
                let ack = rpc::truncate_ack_error(reason);
                for l in lines {
                    self.deps
                        .ledger
                        .record_result(batch_id, l.line_id, 0, Some(ack.as_str()))?;
                }
                error!(batch_id, labels = lines.len(), %reason, "Odoo batch print FAILED — lines acked as error");
                format!("error: {ack}")
            }
        };
        self.set_status(batch_id, summary);
        Ok(())
    }

    /// Write the TSPL to the spool dir and print it via the backend.
    /// `Ok(evidence)` = EventID 307 confirmed; `Err(reason)` otherwise.
    async fn spool_document(
        &self,
        batch_id: i64,
        document: &[u8],
        labels: usize,
        copies: i64,
    ) -> std::result::Result<String, String> {
        let short = uuid::Uuid::new_v4().simple().to_string();
        let job_id = format!("odoo-{batch_id}-{}", &short[..8]);
        let path = self.deps.spool_dir.join(format!("{job_id}.tspl"));
        let doc_name = format!("Odoo batch {batch_id} ({labels} labels, {copies} copies)");
        let printer = self.deps.target_printer.read().await.clone();

        if let Err(e) = tokio::fs::create_dir_all(&self.deps.spool_dir).await {
            return Err(format!("spool dir {}: {e}", self.deps.spool_dir.display()));
        }
        if let Err(e) = tokio::fs::write(&path, document).await {
            return Err(format!("write spool file {}: {e}", path.display()));
        }

        let now = Utc::now();
        if let Some(q) = &self.deps.queue {
            let meta = JobMetadata {
                job_id: job_id.clone(),
                document_name: doc_name.clone(),
                target_printer: printer.clone(),
                target_client_id: None,
                copies: 1,
                paper_size: format!("{}x{} mm", self.geometry.width_mm, self.geometry.height_mm),
                duplex: false,
                color: false,
                payload_size: document.len() as u64,
                payload_sha256: format!("{:x}", Sha256::digest(document)),
                state: JobState::Printing,
                retry_count: 0,
                error_detail: String::new(),
                requesting_user: None,
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = q.record_job(&meta, &path.to_string_lossy()) {
                warn!(job_id, error = %e, "failed to record Odoo job in client history");
            }
        }

        let (event_tx, _) = tokio::sync::broadcast::channel::<PrintJobEvent>(64);
        let events = EventEmitter::new(event_tx.clone());
        let persist = self.deps.queue.clone().map(|q| {
            let mut rx = event_tx.subscribe();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    let _ = q.insert_job_event(&ev);
                }
            })
        });
        events.emit_ok(
            &job_id,
            PrintStage::Received,
            format!("{doc_name} from Odoo ({} B TSPL)", document.len()),
        );

        let backend = Arc::clone(&self.deps.backend);
        let lock = self.deps.print_lock.clone();
        let print_events = events.clone();
        let job = PrintJobInfo {
            job_id: job_id.clone(),
            document_name: doc_name,
            copies: 1,
            duplex: false,
            color: false,
            printer_name: printer,
            printer_display_name: None,
        };
        let print_path = path.clone();
        let make_print = move |cancel: CancellationToken| -> Result<()> {
            let _printer = lock.hold("odoo", &job.job_id);
            backend.print(&job, &print_path, &print_events, &cancel)
        };
        let dispatch = run_print_task_with_timeout(
            &self.inflight,
            &job_id,
            self.deps.print_timeout,
            CancellationToken::new(),
            make_print,
        )
        .await;
        let timed_out = matches!(dispatch, PrintDispatch::TimedOut);
        let result = match dispatch {
            PrintDispatch::Completed(Ok(())) => Ok(events.last_verification().1),
            PrintDispatch::Completed(Err(e)) => Err(format!("{e:#}")),
            PrintDispatch::TimedOut => Err(timed_out_ack_text(self.deps.print_timeout.as_secs())),
            PrintDispatch::DuplicateSuppressed => {
                Err("duplicate print suppressed — batch document still in flight".to_string())
            }
        };

        drop(events);
        drop(event_tx);
        if let Some(task) = persist {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        if let Some(q) = &self.deps.queue {
            let state = if result.is_ok() {
                JobState::Completed
            } else {
                JobState::Failed
            };
            let _ = q.update_job_state(&job_id, state);
        }
        // A timed-out task may still be reading the file — leave it.
        if !timed_out {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                debug!(job_id, error = %e, "could not remove Odoo spool file");
            }
        }
        result
    }

    /// Send every owed ack; returns how many are still owed. A transport
    /// failure (Odoo unreachable) stops the flush and fails the poll; a
    /// JSON-RPC / protocol error on ONE line leaves that line owed and moves
    /// on (it must never block the other acks or `/next`); a business refusal
    /// is Odoo's final answer and is recorded as such.
    async fn flush_acks(&self) -> Result<usize> {
        let mut owed = 0usize;
        for entry in self.deps.ledger.pending_acks()? {
            let error = entry.error.as_deref();
            match self.rpc.ack(entry.line_id, entry.printed_qty, error).await {
                Ok(accepted) => {
                    let note = if accepted.duplicate {
                        ledger::ACK_NOTE_DUPLICATE
                    } else {
                        ledger::ACK_NOTE_OK
                    };
                    self.deps.ledger.mark_acked(entry.line_id, note)?;
                    info!(
                        batch_id = entry.batch_id,
                        line_id = entry.line_id,
                        printed_qty = entry.printed_qty,
                        error = ?entry.error,
                        duplicate = accepted.duplicate,
                        "Odoo ack accepted"
                    );
                }
                Err(RpcError::Business(text)) => {
                    error!(
                        batch_id = entry.batch_id,
                        line_id = entry.line_id,
                        refusal = %text,
                        "Odoo refused the ack — recorded, re-sent only if Odoo offers the line again"
                    );
                    self.deps.ledger.mark_acked(
                        entry.line_id,
                        &format!("{}{text}", ledger::ACK_NOTE_REFUSED_PREFIX),
                    )?;
                }
                Err(e @ RpcError::Transport(_)) => {
                    return Err(anyhow::Error::new(e).context(format!(
                        "ack of line {} (batch {}) — kept for retry",
                        entry.line_id, entry.batch_id
                    )));
                }
                Err(e) => {
                    owed += 1;
                    warn!(
                        batch_id = entry.batch_id,
                        line_id = entry.line_id,
                        error = %e,
                        "Odoo ack failed for this line — kept owed, continuing with the others"
                    );
                }
            }
        }
        Ok(owed)
    }

    /// Current heartbeat payload (blocking printer-status read inside).
    async fn heartbeat_payload(&self) -> Heartbeat {
        let printer = self.deps.target_printer.read().await.clone();
        let raw =
            tokio::task::spawn_blocking(move || printer_status::query_printer_status(&printer))
                .await
                .ok()
                .flatten();
        let (paper_status, error_status) = printer_status::map_printer_status(raw.as_deref());
        let status = self.status();
        Heartbeat {
            printer_name: self.cfg.printer_name.clone(),
            paper_status,
            error_status,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            last_job_id: status.last_job_id,
            last_result: status.last_result,
        }
    }
}

async fn heartbeat_loop(source: Arc<OdooSource>, shutdown: CancellationToken) {
    let every = Duration::from_secs(source.cfg.heartbeat_interval_secs.max(1));
    let mut ticker = tokio::time::interval(every);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let hb = source.heartbeat_payload().await;
        match source.rpc.heartbeat(&hb).await {
            Ok(result) => debug!(
                printer_name = %hb.printer_name,
                paper_status = %hb.paper_status,
                error_status = %hb.error_status,
                last_job_id = ?hb.last_job_id,
                printer_id = %result.get("printer_id").map(ToString::to_string).unwrap_or_default(),
                "Odoo heartbeat sent"
            ),
            Err(e) => warn!(
                error = %e,
                paper_status = %hb.paper_status,
                error_status = %hb.error_status,
                "Odoo heartbeat failed"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn test_backoff_doubles_and_is_capped() {
        assert_eq!(backoff(S(5), S(5)), S(10));
        assert_eq!(backoff(S(10), S(5)), S(20));
        assert_eq!(backoff(S(40), S(5)), MAX_BACKOFF);
        assert_eq!(backoff(MAX_BACKOFF, S(5)), MAX_BACKOFF);
        // never below the configured interval (e.g. after the 1 s post-batch delay)
        assert_eq!(backoff(S(1), S(5)), S(5));
        // an interval above the cap is respected, not shortened
        assert_eq!(backoff(S(90), S(90)), S(90));
    }

    #[test]
    fn test_timed_out_ack_text_says_outcome_unknown() {
        let t = timed_out_ack_text(1800);
        assert!(t.starts_with("outcome unknown"), "{t}");
        assert!(
            t.contains("1800s") && t.contains("before reprinting"),
            "{t}"
        );
        assert!(t.chars().count() <= rpc::MAX_ACK_ERROR_CHARS);
    }

    #[test]
    fn test_next_delay_per_outcome() {
        let base = S(5);
        assert_eq!(next_delay(&Ok(PollOutcome::Idle), S(40), base), base);
        let batch = PollOutcome::Batch {
            batch_id: 1,
            printed: 2,
            rejected: 0,
            reacked: 0,
        };
        assert_eq!(next_delay(&Ok(batch), S(40), base), S(1));
        let known = PollOutcome::OnlyKnownLines {
            batch_id: 1,
            reacked: 3,
        };
        assert_eq!(next_delay(&Ok(known), S(5), base), S(10));
        assert_eq!(
            next_delay(&Err(anyhow::anyhow!("down")), S(20), base),
            S(40)
        );
        assert_eq!(
            next_delay(&Ok(PollOutcome::AcksOwed { owed: 2 }), S(5), base),
            S(10)
        );
        // a 1 s poll interval keeps 1 s after a batch
        let batch = PollOutcome::Batch {
            batch_id: 1,
            printed: 1,
            rejected: 0,
            reacked: 0,
        };
        assert_eq!(next_delay(&Ok(batch), S(1), S(1)), S(1));
    }
}
