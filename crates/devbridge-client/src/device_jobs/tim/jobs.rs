//! The card jobs of the TIM executor (#102 lanes D2 + D3): `card.purchase` /
//! `card.credit` (the held session, [`super::purchase`]), `card.commit` /
//! `card.rollback` (handed to the held payment, or answered from its
//! journal state), `card.cancel` (lane V: `ta_terminal_cancel` on a
//! transaction in its card wait), the restart reconcile, and the dispatch of
//! the one-session jobs of [`super::ops`] (reversal, shift, balance, receipt
//! copy).
//! Contract: `docs/device-jobs-card.md`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use tracing::{info, warn};

use super::card::{
    self, CODE_ALREADY_COMMITTED, CODE_ALREADY_ROLLED_BACK, CODE_BAD_CURRENCY, CODE_BAD_PAYLOAD,
    CODE_NO_TERMINAL, CODE_NOT_EXECUTED, CODE_NOT_RUNNING, CODE_ROLLED_BACK, CODE_SETTLE_MISMATCH,
    CODE_TERMINAL_ERROR, CODE_TERMINAL_UNREACHABLE, CODE_TIM_UNAVAILABLE, CODE_TOO_LATE,
    CODE_UNKNOWN_PURCHASE, CODE_WRONG_TERMINAL, CancelVerdict, Correlation, JOB_ACTIVATE,
    JOB_BALANCE, JOB_CANCEL, JOB_COMMIT, JOB_CREDIT, JOB_DEACTIVATE, JOB_PRINT_COPY, JOB_PURCHASE,
    JOB_REVERSAL, JOB_ROLLBACK, LastTransaction, TwoPhase, TxFacts,
};
use super::purchase::{
    ContactRefusal, PurchaseEnd, PurchaseHooks, PurchaseRequest, SettleOp, SettleReply,
    SettleRequest, run_purchase,
};
use super::{
    CardWait, SessionError, TerminalSpec, TerminalState, TimBackend, TimCanceller, TimExecutor,
    unsupported_text,
};
use crate::device_jobs::api::DeviceJob;
use crate::device_jobs::executor::{
    CODE_BAD_AMOUNT, CODE_UNSUPPORTED, Outcome, Reconciliation, Rejection,
};
use crate::device_jobs::journal::{JobState, Journal, JournalEntry};
use devbridge_core::config::TIM_DEFAULT_COMMIT_TIMEOUT_S;

/// `card.commit` / `card.rollback` — they run on the settle queue.
pub fn is_settle(job_type: &str) -> bool {
    job_type == JOB_COMMIT || job_type == JOB_ROLLBACK
}

fn reject(code: &'static str, text: String) -> Result<(), Rejection> {
    Err(Rejection { code, text })
}

/// A purchase refused for one of these left the terminal in trouble.
fn terminal_trouble(code: &str) -> bool {
    matches!(
        code,
        CODE_TERMINAL_UNREACHABLE | CODE_TERMINAL_ERROR | CODE_WRONG_TERMINAL
    )
}

/// The answer of a settle job, given the purchase's state.
pub fn settle_outcome(op: SettleOp, card_job_id: i64, reply: SettleReply) -> Outcome {
    match (op, reply) {
        (SettleOp::Commit, SettleReply::Committed) => {
            Outcome::Done(json!({"state": "committed", "card_job_id": card_job_id}))
        }
        (SettleOp::Commit, SettleReply::RolledBack(why)) => Outcome::failed(
            CODE_ALREADY_ROLLED_BACK,
            format!("card.purchase {card_job_id} is not charged ({why}) - nothing committed"),
        ),
        (SettleOp::Rollback, SettleReply::RolledBack(why)) => Outcome::Done(
            json!({"state": "rolled_back", "card_job_id": card_job_id, "detail": why}),
        ),
        (SettleOp::Rollback, SettleReply::Committed) => Outcome::failed(
            CODE_ALREADY_COMMITTED,
            format!(
                "card.purchase {card_job_id} is already committed - nothing rolled back (a storno is card.reversal)"
            ),
        ),
        (_, SettleReply::Unknown(why)) => Outcome::Indeterminate(format!(
            "the outcome of card.purchase {card_job_id} is not known yet: {why}"
        )),
    }
}

/// What the journal says about a purchase when no session holds it.
pub fn purchase_state(entry: &JournalEntry) -> SettleReply {
    match entry.state {
        JobState::Done => SettleReply::Committed,
        JobState::Failed => SettleReply::RolledBack(format!(
            "{}: {}",
            entry.error_code.as_deref().unwrap_or("failed"),
            entry.error_text.as_deref().unwrap_or("")
        )),
        other => SettleReply::Unknown(format!("card.purchase is {}", other.as_str())),
    }
}

/// The moments after which a held payment can no longer be undecided on
/// the terminal: contact, authorisation, end of the execute, executor start.
fn settle_moments(
    card: &crate::device_jobs::journal::CardPayment,
    started: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    [
        Some(&card.contact_at),
        card.authorized_at.as_ref(),
        card.ended_at.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|t| DateTime::parse_from_rfc3339(t).ok())
    .map(|t| t.with_timezone(&Utc))
    .chain([started])
    .collect()
}

/// The journal side of one purchase (the terminal prints its own slips).
struct JournalHooks {
    journal: Arc<Journal>,
    job_id: i64,
    tx_type: i32,
    ip: String,
    amount_minor: i64,
    holders: super::Holders,
    cancellers: super::Cancellers,
}

impl PurchaseHooks for JournalHooks {
    fn contact(&mut self, baseline: Option<i64>) -> Result<(), ContactRefusal> {
        match self.journal.card_contact_of(
            self.job_id,
            &self.ip,
            self.amount_minor,
            baseline,
            self.tx_type,
            None,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ContactRefusal::AlreadyAsked),
            Err(e) => Err(ContactRefusal::Journal(format!("{e:#}"))),
        }
    }

    fn authorized(&mut self, facts: &TxFacts, tid: Option<&str>) -> Result<(), String> {
        let result = card::card_result("authorized", tid, facts);
        let trans_seq = facts.trans_seq.unwrap_or_default();
        match self
            .journal
            .card_authorized(self.job_id, trans_seq, &result)
        {
            Ok(true) => {}
            Ok(false) => return Err("no card row for the job".into()),
            Err(e) => return Err(format!("{e:#}")),
        }
        // the slips were printed by the terminal itself (lane V print
        // options); their text is in the journaled result
        info!(
            job_id = self.job_id,
            cardholder_slip = facts.cardholder_receipt.is_some(),
            merchant_slip = facts.merchant_receipt.is_some(),
            "card slips printed by the terminal, text kept in the result"
        );
        Ok(())
    }

    fn released(&mut self) {
        self.holders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.job_id);
        self.cancellers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.job_id);
    }

    fn card_wait(&mut self, canceller: Arc<dyn TimCanceller>) {
        self.cancellers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(self.job_id, CardWait::Waiting(canceller));
    }

    fn card_wait_over(&mut self, authorised: bool) {
        let mut cancellers = self.cancellers.lock().unwrap_or_else(|p| p.into_inner());
        // authorised: `too_late` from now on, even before it is journaled
        let left = match authorised {
            true => cancellers.insert(self.job_id, CardWait::Authorised),
            false => cancellers.remove(&self.job_id),
        };
        info!(
            job_id = self.job_id,
            authorised,
            registered = left.is_some(),
            "card wait over - card.cancel no longer reaches the transaction"
        );
    }

    fn authorization_refused(&mut self) -> bool {
        self.journal
            .card_payment(self.job_id)
            .ok()
            .flatten()
            .is_some_and(|c| c.auth_refused)
    }
}

/// connect → login → terminal id → last transaction → logout → disconnect.
fn read_last(
    backend: &dyn TimBackend,
    spec: &TerminalSpec,
) -> Result<(Option<String>, LastTransaction), SessionError> {
    let mut terminal = super::open_terminal(backend, spec)?;
    terminal.connect()?;
    let mut logged_in = false;
    let result = (|| {
        terminal.login()?;
        logged_in = true;
        let tid = terminal.terminal_id()?;
        if let Some(expected) = &spec.expected_tid
            && tid.as_deref() != Some(expected.as_str())
        {
            return Err(SessionError::WrongTerminal {
                expected: expected.clone(),
                reported: tid,
            });
        }
        Ok((tid, terminal.last_transaction()?))
    })();
    if logged_in && let Err(e) = terminal.logout() {
        warn!(terminal = %spec.ip, error = %e, "TIM logout failed");
    }
    if let Err(e) = terminal.disconnect() {
        warn!(terminal = %spec.ip, error = %e, "TIM disconnect failed");
    }
    result
}

impl TimExecutor {
    pub(super) fn check_card(&self, job: &DeviceJob) -> Result<(), Rejection> {
        let unavailable = self.backend.is_err() || self.journal.is_none();
        let terminal = job.kasa_id.and_then(|k| self.spec_of(k)).is_some();
        let kind = job.job_type.as_str();
        let nothing = if TwoPhase::of_job(kind).is_some() || kind == JOB_REVERSAL {
            "nothing charged"
        } else {
            "nothing ran"
        };
        match kind {
            JOB_COMMIT | JOB_ROLLBACK | JOB_PRINT_COPY | JOB_CANCEL
                if card::settle_target(job).is_none() =>
            {
                reject(
                    CODE_BAD_PAYLOAD,
                    format!("{kind} needs card_job_id (a positive integer) - nothing ran"),
                )
            }
            JOB_COMMIT | JOB_ROLLBACK if self.journal.is_none() => reject(
                CODE_TIM_UNAVAILABLE,
                "the TIM executor has no journal - nothing ran".into(),
            ),
            JOB_COMMIT | JOB_ROLLBACK => Ok(()),
            JOB_CANCEL if unavailable => reject(
                CODE_TIM_UNAVAILABLE,
                "the TIM library is not loaded - the terminal was not contacted, nothing ran".into(),
            ),
            JOB_CANCEL if !terminal => reject(
                CODE_NO_TERMINAL,
                format!(
                    "till {:?} has no card terminal (terminal_ip) - nothing ran",
                    job.kasa_id
                ),
            ),
            JOB_CANCEL => Ok(()),
            JOB_PURCHASE | JOB_CREDIT | JOB_REVERSAL | JOB_ACTIVATE | JOB_DEACTIVATE
            | JOB_BALANCE | JOB_PRINT_COPY
                if unavailable =>
            {
                reject(
                    CODE_TIM_UNAVAILABLE,
                    format!("the TIM library is not loaded - the terminal was not contacted, {nothing}"),
                )
            }
            JOB_PURCHASE | JOB_CREDIT | JOB_REVERSAL | JOB_ACTIVATE | JOB_DEACTIVATE
            | JOB_BALANCE | JOB_PRINT_COPY
                if !terminal =>
            {
                reject(
                    CODE_NO_TERMINAL,
                    format!(
                        "till {:?} has no card terminal (terminal_ip) - {nothing}",
                        job.kasa_id
                    ),
                )
            }
            JOB_PURCHASE | JOB_CREDIT if card::amount_minor(job).is_none() => reject(
                CODE_BAD_AMOUNT,
                format!("{kind} amount must be a whole number of cents > 0 - nothing charged"),
            ),
            JOB_PURCHASE | JOB_CREDIT if !card::currency_ok(job) => reject(
                CODE_BAD_CURRENCY,
                "the terminal takes EUR only - nothing charged".into(),
            ),
            JOB_REVERSAL if card::reversal_target(job).is_none() => reject(
                CODE_BAD_PAYLOAD,
                "card.reversal needs trans_seq (an integer 0 ..= 2^31 - 1) and amount (whole cents > 0) - nothing charged".into(),
            ),
            JOB_PURCHASE | JOB_CREDIT | JOB_REVERSAL | JOB_ACTIVATE | JOB_DEACTIVATE
            | JOB_BALANCE | JOB_PRINT_COPY => Ok(()),
            other => reject(CODE_UNSUPPORTED, unsupported_text(other)),
        }
    }

    pub(super) async fn execute_card(&self, job: &DeviceJob) -> Outcome {
        match job.job_type.as_str() {
            JOB_PURCHASE => self.two_phase(job, TwoPhase::Purchase).await,
            JOB_CREDIT => self.two_phase(job, TwoPhase::Credit).await,
            JOB_COMMIT => self.settle(job, SettleOp::Commit).await,
            JOB_ROLLBACK => self.settle(job, SettleOp::Rollback).await,
            JOB_CANCEL => self.cancel(job).await,
            JOB_REVERSAL => self.reversal(job).await,
            JOB_ACTIVATE | JOB_DEACTIVATE | JOB_BALANCE => self.terminal_op(job).await,
            JOB_PRINT_COPY => self.print_copy(job).await,
            other => Outcome::failed(CODE_UNSUPPORTED, unsupported_text(other)),
        }
    }

    /// `card.purchase` / `card.credit`: one held session.
    async fn two_phase(&self, job: &DeviceJob, kind: TwoPhase) -> Outcome {
        let (Some(journal), Some(spec), Some(amount_minor)) = (
            self.journal.clone(),
            job.kasa_id.and_then(|k| self.spec_of(k)).cloned(),
            card::amount_minor(job),
        ) else {
            return Outcome::failed(
                CODE_TIM_UNAVAILABLE,
                "the purchase cannot reach a terminal - nothing charged",
            );
        };
        let stored_max_seq = journal.card_last_seq(&spec.ip).unwrap_or_else(|e| {
            warn!(job_id = job.id, error = %format!("{e:#}"), "journal: last trans_seq not readable");
            None
        });
        let (tx, rx) = std::sync::mpsc::channel::<SettleRequest>();
        self.holders_lock().insert(job.id, tx);
        let req = PurchaseRequest {
            job_id: job.id,
            kind,
            amount_minor,
            deadline: self.deadline,
            stored_max_seq,
            release_within: self
                .release_within
                .unwrap_or(Duration::from_secs(u64::from(spec.commit_timeout_s))),
        };
        let mut hooks = JournalHooks {
            journal: Arc::clone(&journal),
            job_id: job.id,
            tx_type: kind.tx_type(),
            ip: spec.ip.clone(),
            amount_minor,
            holders: Arc::clone(&self.holders),
            cancellers: Arc::clone(&self.cancellers),
        };
        let ip = spec.ip.clone();
        info!(job_id = job.id, kasa_id = spec.kasa_id, terminal = %ip, ?kind, amount_minor, deadline_s = self.deadline.as_secs(), "two-phase card transaction on the terminal");
        let ran = self
            .on_terminal(spec, move |backend, spec| {
                run_purchase(backend, spec, &req, &mut hooks, &rx)
            })
            .await;
        // the purchase itself left `holders` (`JournalHooks::released`)
        // before it answered the late settle requests
        let end = match ran {
            Some(Ok(end)) => end,
            Some(Err(e)) => PurchaseEnd::Unknown(format!("the purchase thread died: {e}")),
            None => PurchaseEnd::Refused {
                code: CODE_TIM_UNAVAILABLE,
                text: "the TIM library is not loaded - nothing charged".into(),
            },
        };
        let state = match &end {
            PurchaseEnd::Refused { code, text } if terminal_trouble(code) => {
                TerminalState::Failed(text.clone())
            }
            _ => TerminalState::Ready,
        };
        self.set_state(&ip, state);
        let outcome = match end {
            PurchaseEnd::Refused { code, text } => Outcome::failed(code, text),
            PurchaseEnd::Committed { facts, tid } => {
                Outcome::Done(card::card_result("committed", tid.as_deref(), &facts))
            }
            PurchaseEnd::RolledBack(why) => Outcome::failed(CODE_ROLLED_BACK, why),
            PurchaseEnd::Unknown(why) => {
                if let Err(e) = journal.card_ended(job.id) {
                    warn!(job_id = job.id, error = %format!("{e:#}"), "journal: card end not recorded");
                }
                Outcome::Indeterminate(why)
            }
        };
        info!(
            job_id = job.id,
            ?kind,
            outcome = outcome.label(),
            "two-phase card transaction finished"
        );
        outcome
    }

    pub(super) fn holders_lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::HashMap<i64, std::sync::mpsc::Sender<SettleRequest>>,
    > {
        self.holders.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The payment a settle job names, if it is a `card.purchase` /
    /// `card.credit` of the same till; else the refusal.
    fn settle_purchase(&self, job: &DeviceJob, target: i64) -> Result<JournalEntry, Outcome> {
        let unknown = |why: &str| {
            Outcome::failed(
                CODE_UNKNOWN_PURCHASE,
                format!("card_job_id {target}: {why} - nothing ran"),
            )
        };
        let Some(journal) = &self.journal else {
            return Err(unknown("no journal"));
        };
        let entry = match journal.get(target) {
            Ok(Some(entry)) => entry,
            Ok(None) => return Err(unknown("this bridge never received that card.purchase")),
            Err(e) => return Err(Outcome::Indeterminate(format!("journal: {e:#}"))),
        };
        if TwoPhase::of_job(&entry.job.job_type).is_none() || entry.job.kasa_id != job.kasa_id {
            return Err(unknown("not a card.purchase / card.credit of this till"));
        }
        let card = journal.card_payment(target).ok().flatten();
        if let Some(problem) = card
            .as_ref()
            .and_then(|c| c.auth_result.as_ref())
            .and_then(|auth| card::settle_mismatch(job, auth))
        {
            return Err(Outcome::failed(
                CODE_SETTLE_MISMATCH,
                format!("{problem} - nothing ran"),
            ));
        }
        Ok(entry)
    }

    async fn settle(&self, job: &DeviceJob, op: SettleOp) -> Outcome {
        let Some(target) = card::settle_target(job) else {
            return Outcome::failed(CODE_BAD_PAYLOAD, "no card_job_id - nothing ran");
        };
        if let Err(refusal) = self.settle_purchase(job, target) {
            return refusal;
        }
        let holder = self.holders_lock().get(&target).cloned();
        if let Some(tx) = holder {
            let (reply, answer) = tokio::sync::oneshot::channel();
            if tx.send(SettleRequest { op, reply }).is_ok()
                && let Ok(state) = answer.await
            {
                info!(
                    job_id = job.id,
                    card_job_id = target,
                    ?op,
                    ?state,
                    "settle request answered by the held payment"
                );
                return settle_outcome(op, target, state);
            }
        }
        // no session holds it: the purchase's own journal state decides
        let state = match self.settle_purchase(job, target) {
            Ok(entry) => purchase_state(&entry),
            Err(refusal) => return refusal,
        };
        info!(
            job_id = job.id,
            card_job_id = target,
            ?op,
            ?state,
            "settle request follows the journaled purchase"
        );
        settle_outcome(op, target, state)
    }

    /// `card.cancel {card_job_id}`: `ta_terminal_cancel` on the named
    /// purchase / credit of this till while it is in its card wait — never a
    /// new connection (the terminal takes one ECR connection, and a cancel
    /// acts only on the session's own pending request). `done` = the cancel
    /// was SENT; the payment's own ack says whether it stopped.
    async fn cancel(&self, job: &DeviceJob) -> Outcome {
        let Some(target) = card::settle_target(job) else {
            return Outcome::failed(CODE_BAD_PAYLOAD, "no card_job_id - nothing ran");
        };
        let Some(journal) = &self.journal else {
            return Outcome::failed(
                CODE_TIM_UNAVAILABLE,
                "the TIM executor has no journal - nothing ran",
            );
        };
        let entry = match journal.get(target) {
            Ok(entry) => entry,
            Err(e) => return Outcome::Indeterminate(format!("journal: {e:#}")),
        };
        let ours = entry.as_ref().is_some_and(|e| {
            TwoPhase::of_job(&e.job.job_type).is_some() && e.job.kasa_id == job.kasa_id
        });
        let journaled_authorised = match ours.then(|| journal.card_payment(target)) {
            None => false,
            Some(Ok(card)) => card.is_some_and(|c| c.authorized_at.is_some()),
            Some(Err(e)) => return Outcome::Indeterminate(format!("journal: {e:#}")),
        };
        let cancellers = Arc::clone(&self.cancellers);
        // the check and the send under ONE lock: the purchase's
        // `card_wait_over` can never pass between them; on a blocking
        // thread like every TIM call
        let decided = tokio::task::spawn_blocking(move || {
            let registry = cancellers.lock().unwrap_or_else(|p| p.into_inner());
            // a state is only registered by our own purchase / credit
            let wait = registry.get(&target).filter(|_| ours);
            let verdict = card::cancel_verdict(
                matches!(wait, Some(CardWait::Waiting(_))),
                journaled_authorised || matches!(wait, Some(CardWait::Authorised)),
            );
            let sent = match (verdict, wait) {
                (CancelVerdict::Send, Some(CardWait::Waiting(canceller))) => {
                    Some(canceller.cancel())
                }
                _ => None,
            };
            (verdict, sent)
        })
        .await;
        let (verdict, sent) = match decided {
            Ok(decided) => decided,
            Err(e) => {
                return Outcome::Indeterminate(format!(
                    "the cancel thread died ({e}) - the payment's own result decides"
                ));
            }
        };
        info!(job_id = job.id, card_job_id = target, known = ours, journaled_authorised, ?verdict, sent = ?sent, "card.cancel");
        match (verdict, sent) {
            (CancelVerdict::Send, Some(Ok(()))) => {
                Outcome::Done(json!({"cancel_sent": true, "card_job_id": target}))
            }
            (CancelVerdict::Send, Some(Err(e))) => Outcome::failed(
                CODE_NOT_RUNNING,
                format!("card_job_id {target}: the cancel was not sent ({e}) - nothing ran"),
            ),
            (CancelVerdict::TooLate, _) => Outcome::failed(
                CODE_TOO_LATE,
                format!(
                    "card_job_id {target} is already authorised - nothing ran; use card.rollback while it is held (card.reversal after the commit)"
                ),
            ),
            _ => Outcome::failed(
                CODE_NOT_RUNNING,
                format!(
                    "card_job_id {target} is not in its card wait on this till's terminal (not started, ended, or unknown) - nothing ran"
                ),
            ),
        }
    }

    pub(super) async fn reconcile_card(&self, entry: &JournalEntry) -> Reconciliation {
        match entry.job.job_type.as_str() {
            // the documented exception to "failed = nothing ran" (like
            // Portos' outcome_unknown_retry_safe): the cancel may have gone
            // out before the restart, but it moves no money and a restart
            // ended every card wait — the payment's own reconcile decides
            JOB_CANCEL => Reconciliation::Failed {
                code: card::CODE_CANCEL_UNKNOWN.into(),
                text: "the bridge restarted while card.cancel ran - it may have been sent; no transaction is in its card wait any more, the payment's own result decides (no money moved by a cancel)".into(),
            },
            JOB_PURCHASE | JOB_CREDIT | JOB_REVERSAL => self.reconcile_transaction(entry).await,
            JOB_ACTIVATE | JOB_DEACTIVATE | JOB_BALANCE | JOB_PRINT_COPY => {
                self.reconcile_terminal_op(entry).await
            }
            JOB_COMMIT | JOB_ROLLBACK => {
                let op = if entry.job.job_type == JOB_COMMIT {
                    SettleOp::Commit
                } else {
                    SettleOp::Rollback
                };
                let Some(target) = card::settle_target(&entry.job) else {
                    return Reconciliation::StillUnknown("no card_job_id".into());
                };
                let state = match self.settle_purchase(&entry.job, target) {
                    Ok(purchase) => purchase_state(&purchase),
                    Err(refusal) => return as_reconciliation(refusal),
                };
                as_reconciliation(settle_outcome(op, target, state))
            }
            other => Reconciliation::StillUnknown(format!(
                "the TIM executor does not run '{other}' - a person must reconcile it"
            )),
        }
    }

    /// A `card.purchase` / `card.credit` / `card.reversal` whose outcome
    /// is unknown: the terminal's last transaction decides, else a person.
    async fn reconcile_transaction(&self, entry: &JournalEntry) -> Reconciliation {
        let job_id = entry.job.id;
        let Some(journal) = &self.journal else {
            return Reconciliation::StillUnknown("the TIM executor has no journal".into());
        };
        if entry.device_contact_at.is_none() {
            return Reconciliation::Failed {
                code: CODE_NOT_EXECUTED.into(),
                text:
                    "the terminal was never asked (no device contact journaled) - nothing charged"
                        .into(),
            };
        }
        let card = match journal.card_payment(job_id) {
            Ok(Some(card)) => card,
            Ok(None) => {
                return Reconciliation::StillUnknown(
                    "no card row - a person must reconcile it".into(),
                );
            }
            Err(e) => return Reconciliation::StillUnknown(format!("journal: {e:#}")),
        };
        if let Some(reason) = &card.person_reason {
            return Reconciliation::StillUnknown(format!("left for a person: {reason}"));
        }
        let spec = entry.job.kasa_id.and_then(|k| self.spec_of(k)).cloned();
        let commit_timeout = spec
            .as_ref()
            .map_or(TIM_DEFAULT_COMMIT_TIMEOUT_S, |s| s.commit_timeout_s);
        let after = card::settle_after(&settle_moments(&card, self.started_at), commit_timeout);
        if let Some(after) = after.filter(|_| card::settle_pending(Utc::now(), after)) {
            return Reconciliation::StillUnknown(format!(
                "waiting until the terminal's commit timeout has passed ({after})"
            ));
        }
        let Some(spec) = spec else {
            return Reconciliation::StillUnknown(
                "the till has no terminal any more - a person must reconcile it".into(),
            );
        };
        let ip = spec.ip.clone();
        let read = match self.on_terminal(spec, read_last).await {
            Some(Ok(read)) => read,
            Some(Err(e)) => Err(SessionError::Aborted(e)),
            None => return Reconciliation::StillUnknown("the TIM library is not loaded".into()),
        };
        let (tid, last) = match read {
            Ok(read) => {
                self.set_state(&ip, TerminalState::Ready);
                read
            }
            Err(e) => {
                self.set_state(&ip, TerminalState::Failed(e.to_string()));
                return Reconciliation::StillUnknown(format!(
                    "the terminal could not be asked: {e}"
                ));
            }
        };
        let later_payment = journal.card_later_on_terminal(job_id).unwrap_or(true);
        let verdict = card::correlate(
            card::Ours {
                baseline: card.baseline_seq,
                amount_minor: card.amount_minor,
                trans_seq: card.trans_seq,
                tx_type: card.tx_type,
                later_payment,
            },
            &last,
        );
        info!(job_id, tx_type = card.tx_type, baseline_seq = ?card.baseline_seq, last_type = last.transaction_type, last_outcome = last.outcome, verdict = ?std::mem::discriminant(&verdict), "card transaction reconciled with the terminal's last transaction");
        match verdict {
            Correlation::Charged(facts) => {
                if let Some(seq) = facts.trans_seq
                    && let Err(e) = journal.card_transacted(job_id, seq)
                {
                    warn!(job_id, error = %format!("{e:#}"), "journal: reconciled trans_seq not recorded");
                }
                let state = if card.tx_type == card::TRANSACTION_TYPE_REVERSAL {
                    "reversed"
                } else {
                    "successful"
                };
                let mut result = card::card_result(state, tid.as_deref(), &facts);
                result["reconciled"] = json!(true);
                Reconciliation::Done(result)
            }
            Correlation::NotCharged if card.tx_type == card::TRANSACTION_TYPE_REVERSAL => {
                Reconciliation::Failed {
                    code: card::CODE_NOT_REVERSED.into(),
                    text: "the terminal reports this reversal failed - the payment is NOT reversed, no money moved".into(),
                }
            }
            Correlation::NotCharged => Reconciliation::Failed {
                code: CODE_ROLLED_BACK.into(),
                text: "the terminal reports this payment failed (rolled back / reversed) - no money taken".into(),
            },
            Correlation::Undecidable(why) => {
                if let Err(e) = journal.card_for_person(job_id, &why) {
                    warn!(job_id, error = %format!("{e:#}"), "journal: person flag not recorded");
                }
                warn!(job_id, %why, "card transaction outcome cannot be proven by the terminal - left for a person, never transacted again");
                Reconciliation::StillUnknown(format!("left for a person: {why}"))
            }
        }
    }
}

fn as_reconciliation(outcome: Outcome) -> Reconciliation {
    match outcome {
        Outcome::Done(result) => Reconciliation::Done(result),
        Outcome::Failed { code, text } => Reconciliation::Failed { code, text },
        Outcome::Indeterminate(why) => Reconciliation::StillUnknown(why),
    }
}
