//! The one-session card jobs of the TIM executor (#102 lane D3):
//! `card.reversal`, `card.activate` / `card.deactivate`, `card.balance` and
//! `card.print_copy`, plus the 23:50 safety balance's terminal call. Each is
//! ONE per-use session (connect → login → terminal id → the call → logout →
//! disconnect) on the terminal's own queue, so it never runs beside a held
//! payment. Contract: `docs/device-jobs-card.md` §8-§11.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tracing::{error, info, warn};

use super::card::{
    self, CODE_BRIDGE_ERROR, CODE_NOT_EXECUTED, CODE_NOT_LAST_TRANSACTION, CODE_PRINT_UNKNOWN,
    CODE_TERMINAL_ERROR, CODE_TERMINAL_UNREACHABLE, CODE_TIM_UNAVAILABLE, CODE_UNKNOWN_PURCHASE,
    CODE_WRONG_TERMINAL, JOB_ACTIVATE, JOB_BALANCE, JOB_DEACTIVATE, JOB_PRINT_COPY,
    TRANSACTION_TYPE_REVERSAL, TwoPhase, TxFacts, TxFailure,
};
use super::reports::{self, Receipts};
use super::{
    SessionError, TerminalSpec, TerminalState, TimBackend, TimError, TimExecutor, TimTerminal,
};
use crate::device_jobs::api::DeviceJob;
use crate::device_jobs::executor::{Outcome, Reconciliation};
use crate::device_jobs::journal::{JobState, Journal, JournalEntry, SOURCE_JOB, SOURCE_SAFETY};

/// Why a session did not reach its call — nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFail {
    /// The connect failed.
    Unreachable(TimError),
    /// Login / terminal id / the terminal object / the wrong terminal.
    Refused(SessionError),
}

impl SessionFail {
    /// The job's failure code + text ("nothing ran").
    pub fn outcome(&self, nothing: &str) -> Outcome {
        match self {
            Self::Unreachable(e) => Outcome::failed(
                CODE_TERMINAL_UNREACHABLE,
                format!("the terminal could not be reached: {e} - {nothing}"),
            ),
            Self::Refused(e @ SessionError::WrongTerminal { .. }) => {
                Outcome::failed(CODE_WRONG_TERMINAL, format!("{e} - {nothing}"))
            }
            Self::Refused(e) => Outcome::failed(CODE_TERMINAL_ERROR, format!("{e} - {nothing}")),
        }
    }
}

/// connect → login → terminal id (checked against `terminal_tid`) →
/// `work(terminal, tid)` → logout → disconnect. After a connect, logout
/// (when logged in) and disconnect always run.
pub fn with_session<T>(
    backend: &dyn TimBackend,
    spec: &TerminalSpec,
    work: impl FnOnce(&mut dyn TimTerminal, Option<String>) -> T,
) -> Result<T, SessionFail> {
    let mut terminal = super::open_terminal(backend, spec)
        .map_err(|e| SessionFail::Refused(SessionError::Tim(e)))?;
    terminal.connect().map_err(SessionFail::Unreachable)?;
    let mut logged_in = false;
    let result = (|| {
        terminal
            .login()
            .map_err(|e| SessionFail::Refused(SessionError::Tim(e)))?;
        logged_in = true;
        let tid = terminal
            .terminal_id()
            .map_err(|e| SessionFail::Refused(SessionError::Tim(e)))?;
        if let Some(expected) = &spec.expected_tid
            && tid.as_deref() != Some(expected.as_str())
        {
            return Err(SessionFail::Refused(SessionError::WrongTerminal {
                expected: expected.clone(),
                reported: tid,
            }));
        }
        Ok(work(terminal.as_mut(), tid))
    })();
    if logged_in && let Err(e) = terminal.logout() {
        warn!(terminal = %spec.ip, error = %e, "TIM logout failed");
    }
    if let Err(e) = terminal.disconnect() {
        warn!(terminal = %spec.ip, error = %e, "TIM disconnect failed");
    }
    result
}

/// How a reversal ended inside its session.
#[derive(Debug, Clone, PartialEq)]
enum ReversalEnd {
    Done(Box<TxFacts>, Option<String>),
    Refused(&'static str, String),
    Unknown(String),
}

/// What the reversal's session needs.
struct ReversalRun {
    journal: Arc<Journal>,
    job_id: i64,
    ip: String,
    trans_seq: i64,
    amount_minor: i64,
    payload_tid: Option<String>,
    stored_max_seq: Option<i64>,
}

impl ReversalRun {
    /// A balance of this terminal ended `done` after this bridge sent the
    /// payment `trans_seq` (unreadable = not proven).
    fn period_closed_proven(&self) -> bool {
        let proven = self
            .journal
            .card_balance_since_payment(&self.ip, self.trans_seq)
            .unwrap_or(false);
        info!(
            job_id = self.job_id,
            trans_seq = self.trans_seq,
            proven,
            "balance since the reversed payment"
        );
        proven
    }

    /// Inside the session: the payload's terminal id, the baseline, the
    /// contact (journaled BEFORE the call), then the reversal with ONLY the
    /// TransSeq.
    fn run(&self, terminal: &mut dyn TimTerminal, tid: Option<String>) -> ReversalEnd {
        let job_id = self.job_id;
        if let Some(wanted) = &self.payload_tid
            && tid.as_deref() != Some(wanted.as_str())
        {
            return ReversalEnd::Refused(
                CODE_WRONG_TERMINAL,
                format!(
                    "the payment was charged on terminal {wanted}, this till's terminal is {} - nothing reversed",
                    tid.as_deref().unwrap_or("(unknown)")
                ),
            );
        }
        let last = terminal
            .last_transaction()
            .map_err(|e| warn!(job_id, error = %e, "TIM last transaction not readable - the baseline falls back to the journal"))
            .ok();
        let base = card::baseline(last.as_ref(), self.stored_max_seq);
        // a TransSeq is reversed at most once: never again while another
        // reversal of it is done or unresolved (checked under the session
        // lock, so two reversal jobs never race)
        match self
            .journal
            .card_other_reversal(&self.ip, self.trans_seq, job_id)
        {
            Ok(None) => {}
            Ok(Some((other, JobState::Done))) => {
                return ReversalEnd::Refused(
                    card::CODE_ALREADY_REVERSED,
                    format!(
                        "trans_seq {} was already reversed by job {other} - nothing sent",
                        self.trans_seq
                    ),
                );
            }
            Ok(Some((other, state))) => {
                warn!(
                    job_id,
                    other,
                    state = state.as_str(),
                    "an earlier reversal of this trans_seq is unresolved - NOT sent again"
                );
                return ReversalEnd::Refused(
                    card::CODE_REVERSAL_UNRESOLVED,
                    format!(
                        "an earlier reversal of trans_seq {} (job {other}) is {} - nothing sent, a person decides",
                        self.trans_seq,
                        state.as_str()
                    ),
                );
            }
            Err(e) => {
                return ReversalEnd::Refused(
                    CODE_BRIDGE_ERROR,
                    format!("journal: {e:#} - the reversal was not sent, nothing reversed"),
                );
            }
        }
        match self.journal.card_contact_of(
            job_id,
            &self.ip,
            self.amount_minor,
            base,
            TRANSACTION_TYPE_REVERSAL,
            Some(self.trans_seq),
        ) {
            Ok(true) => {}
            Ok(false) => {
                error!(
                    job_id,
                    "the terminal was already asked for this reversal - NO second one, reconciled"
                );
                return ReversalEnd::Unknown(
                    "the terminal was already asked for this reversal - no second one, reconciled with the terminal".into(),
                );
            }
            Err(e) => {
                return ReversalEnd::Refused(
                    CODE_BRIDGE_ERROR,
                    format!(
                        "the journal did not record the terminal contact ({e:#}) - the reversal was not sent, nothing reversed"
                    ),
                );
            }
        }
        info!(job_id, terminal = %self.ip, trans_seq = self.trans_seq, baseline_seq = ?base, "TIM reversal sent to the terminal (TransSeq only)");
        match terminal.reversal(self.trans_seq) {
            Ok(facts) => {
                info!(job_id, reversal_seq = ?facts.trans_seq, amount_minor = ?facts.amount_minor, "TIM reversal response: reversed");
                ReversalEnd::Done(Box::new(facts), tid)
            }
            Err(e) => match card::classify_reversal_error(e.code, self.period_closed_proven()) {
                TxFailure::NothingCharged(code) => {
                    info!(
                        job_id,
                        rc = e.code,
                        code,
                        "TIM reversal refused by the terminal - nothing reversed"
                    );
                    ReversalEnd::Refused(
                        code,
                        format!("the terminal refused the reversal: {e} - nothing reversed"),
                    )
                }
                TxFailure::Unknown => {
                    warn!(
                        job_id,
                        rc = e.code,
                        "TIM reversal outcome unknown - reconciled with the terminal"
                    );
                    ReversalEnd::Unknown(format!(
                        "the reversal failed with an unknown outcome: {e} - reconciled with the terminal"
                    ))
                }
            },
        }
    }
}

/// The balance's answer journaled: `done` + the result, or `failed`.
fn journal_balance(journal: &Journal, id: Option<i64>, outcome: &Outcome) {
    let Some(id) = id else { return };
    let (state, result) = match outcome {
        Outcome::Done(result) => ("done", result.clone()),
        Outcome::Failed { code, text } => ("failed", json!({"code": code, "text": text})),
        Outcome::Indeterminate(why) => {
            return warn!(balance = id, %why, "balance outcome unknown - left NULL");
        }
    };
    if let Err(e) = journal.card_balance_outcome(id, state, &result) {
        warn!(balance = id, error = %format!("{e:#}"), "journal: balance outcome not recorded");
    }
}

/// The store's local date (`YYYY-MM-DD`) of a moment.
pub fn local_date(at: chrono::DateTime<chrono::Local>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// The terminal operation of a shift / balance job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOp {
    Activate,
    Deactivate,
    Balance,
}

impl TerminalOp {
    pub fn of_job(job_type: &str) -> Option<Self> {
        match job_type {
            JOB_ACTIVATE => Some(Self::Activate),
            JOB_DEACTIVATE => Some(Self::Deactivate),
            JOB_BALANCE => Some(Self::Balance),
            _ => None,
        }
    }

    /// Run it on `terminal`: the job result, or the TIM error.
    fn call(self, terminal: &mut dyn TimTerminal, tid: Option<&str>) -> Result<Value, TimError> {
        Ok(match self {
            Self::Activate => reports::shift_result("activated", tid, &terminal.activate()?),
            Self::Deactivate => reports::shift_result("deactivated", tid, &terminal.deactivate()?),
            Self::Balance => reports::balance_result(tid, &terminal.balance()?),
        })
    }
}

/// What a shift / balance call's error means: a code in which the terminal
/// says it did not run it (busy, cancelled, …: the purchase table) is
/// `failed`; any other is unknown.
pub(super) fn op_error(op: TerminalOp, e: &TimError) -> Outcome {
    match card::classify_transaction_error(e.code) {
        TxFailure::NothingCharged(code) => Outcome::failed(
            code,
            format!("the terminal refused the {op:?}: {e} - nothing ran"),
        ),
        TxFailure::Unknown => Outcome::Indeterminate(format!(
            "{op:?} failed after the terminal was asked: {e} - reconciled"
        )),
    }
}

/// The receipt copy's session: the latest transaction must be the named
/// one, then the reprint.
fn copy_session(
    terminal: &mut dyn TimTerminal,
    journal: &Journal,
    job_id: i64,
    trans_seq: i64,
) -> Result<Receipts, Outcome> {
    if let Err(e) = journal.mark_device_contact(job_id) {
        return Err(Outcome::failed(
            CODE_BRIDGE_ERROR,
            format!("the journal did not record the terminal contact ({e:#}) - nothing printed"),
        ));
    }
    let last = terminal.last_transaction().map_err(|e| {
        Outcome::failed(
            CODE_TERMINAL_ERROR,
            format!("the terminal's last transaction is not readable: {e} - nothing printed"),
        )
    })?;
    let last_seq = last.facts.as_ref().and_then(|f| f.trans_seq);
    if last_seq != Some(trans_seq) {
        return Err(Outcome::failed(
            CODE_NOT_LAST_TRANSACTION,
            format!(
                "the terminal reprints only its latest receipt (trans_seq {last_seq:?}), not trans_seq {trans_seq} - nothing printed"
            ),
        ));
    }
    info!(
        job_id,
        trans_seq, "TIM receipt copy requested (the terminal prints it)"
    );
    terminal.reprint().map_err(|e| {
        Outcome::failed(
            CODE_TERMINAL_ERROR,
            format!("the terminal gave no receipt copy: {e} - nothing printed"),
        )
    })
}

impl TimExecutor {
    /// Run `work` in ONE session on `spec`'s terminal (its queue lock), and
    /// keep the heartbeat state. `Err` = the outcome to answer.
    async fn one_session<T: Send + 'static>(
        &self,
        spec: TerminalSpec,
        nothing: &'static str,
        work: impl FnOnce(&mut dyn TimTerminal, Option<String>) -> T + Send + 'static,
    ) -> Result<T, Outcome> {
        let ip = spec.ip.clone();
        let ran = self
            .on_terminal(spec, move |backend, spec| with_session(backend, spec, work))
            .await;
        match ran {
            None => Err(Outcome::failed(
                CODE_TIM_UNAVAILABLE,
                format!("the TIM library is not loaded - {nothing}"),
            )),
            Some(Err(e)) => {
                self.set_state(&ip, TerminalState::Failed(e.clone()));
                Err(Outcome::Indeterminate(format!(
                    "the session thread died: {e}"
                )))
            }
            Some(Ok(Err(fail))) => {
                self.set_state(&ip, TerminalState::Failed(format!("{fail:?}")));
                Err(fail.outcome(nothing))
            }
            Some(Ok(Ok(value))) => {
                self.set_state(&ip, TerminalState::Ready);
                Ok(value)
            }
        }
    }

    fn job_spec(&self, job: &DeviceJob) -> Option<TerminalSpec> {
        job.kasa_id.and_then(|k| self.spec_of(k)).cloned()
    }

    /// `card.reversal`: ONE session, the contact journaled before the call,
    /// only the TransSeq sent, no commit.
    pub(super) async fn reversal(&self, job: &DeviceJob) -> Outcome {
        let (Some(journal), Some(spec), Some((trans_seq, amount_minor))) = (
            self.journal.clone(),
            self.job_spec(job),
            card::reversal_target(job),
        ) else {
            return Outcome::failed(
                CODE_TIM_UNAVAILABLE,
                "the reversal cannot reach a terminal - nothing reversed",
            );
        };
        let run = ReversalRun {
            journal: Arc::clone(&journal),
            job_id: job.id,
            ip: spec.ip.clone(),
            trans_seq,
            amount_minor,
            payload_tid: card::payload_tid(job).map(str::to_string),
            stored_max_seq: journal.card_last_seq(&spec.ip).unwrap_or_else(|e| {
                warn!(job_id = job.id, error = %format!("{e:#}"), "journal: last trans_seq not readable");
                None
            }),
        };
        info!(job_id = job.id, kasa_id = spec.kasa_id, terminal = %spec.ip, trans_seq, amount_minor, "card.reversal on the terminal");
        let end = match self
            .one_session(spec, "nothing reversed", move |t, tid| run.run(t, tid))
            .await
        {
            Ok(end) => end,
            Err(outcome) => return outcome,
        };
        let outcome = match end {
            ReversalEnd::Done(facts, tid) => {
                if let Some(seq) = facts.trans_seq
                    && let Err(e) = journal.card_transacted(job.id, seq)
                {
                    warn!(job_id = job.id, error = %format!("{e:#}"), "journal: reversal trans_seq not recorded");
                }
                // reported as the terminal answered (Odoo compares it with the
                // payment); both amounts are logged, never a log-only branch
                info!(job_id = job.id, reversed = ?facts.amount_minor, asked = amount_minor, "card.reversal amounts (the terminal's answer is the result)");
                Outcome::Done(card::card_result("reversed", tid.as_deref(), &facts))
            }
            ReversalEnd::Refused(code, text) => Outcome::failed(code, text),
            ReversalEnd::Unknown(why) => {
                if let Err(e) = journal.card_ended(job.id) {
                    warn!(job_id = job.id, error = %format!("{e:#}"), "journal: card end not recorded");
                }
                Outcome::Indeterminate(why)
            }
        };
        info!(
            job_id = job.id,
            outcome = outcome.label(),
            "card.reversal finished"
        );
        outcome
    }

    /// `card.activate` / `card.deactivate` / `card.balance`: ONE session,
    /// `device_contact_at` (and a balance row) journaled before the call.
    pub(super) async fn terminal_op(&self, job: &DeviceJob) -> Outcome {
        let (Some(journal), Some(spec), Some(op)) = (
            self.journal.clone(),
            self.job_spec(job),
            TerminalOp::of_job(&job.job_type),
        ) else {
            return Outcome::failed(CODE_TIM_UNAVAILABLE, "no terminal - nothing ran");
        };
        let job_id = job.id;
        let ip = spec.ip.clone();
        info!(job_id, kasa_id = spec.kasa_id, terminal = %ip, ?op, "card terminal operation");
        let date = local_date(chrono::Local::now());
        let journal_in = Arc::clone(&journal);
        let ran = self
            .one_session(spec, "nothing ran", move |terminal, tid| {
                match journal_in.mark_device_contact(job_id) {
                    Ok(true) => {}
                    Ok(false) => {
                        return (None, Outcome::Indeterminate(
                            "the terminal was already asked for this job - not asked again, reconciled".into(),
                        ));
                    }
                    Err(e) => {
                        return (None, Outcome::failed(
                            CODE_BRIDGE_ERROR,
                            format!("the journal did not record the terminal contact ({e:#}) - nothing ran"),
                        ));
                    }
                }
                let balance = match op {
                    TerminalOp::Balance => match journal_in.card_balance_contact(
                        &ip,
                        &date,
                        SOURCE_JOB,
                        Some(job_id),
                    ) {
                        Ok(id) => id,
                        Err(e) => {
                            return (None, Outcome::failed(
                                CODE_BRIDGE_ERROR,
                                format!("the journal did not record the balance ({e:#}) - nothing ran"),
                            ));
                        }
                    },
                    _ => None,
                };
                let outcome = match op.call(terminal, tid.as_deref()) {
                    Ok(result) => Outcome::Done(result),
                    Err(e) => op_error(op, &e),
                };
                (balance, outcome)
            })
            .await;
        let outcome = match ran {
            Ok((balance, outcome)) => {
                journal_balance(&journal, balance, &outcome);
                outcome
            }
            Err(outcome) => outcome,
        };
        info!(job_id, ?op, outcome = outcome.label(), result = ?outcome_total(&outcome), "card terminal operation finished");
        outcome
    }

    /// `card.print_copy {card_job_id}`: the terminal reprints its latest
    /// receipt on its own printer (lane V print options), when it is the
    /// named payment's; the copy's text is the result.
    pub(super) async fn print_copy(&self, job: &DeviceJob) -> Outcome {
        let (Some(journal), Some(spec), Some(target)) = (
            self.journal.clone(),
            self.job_spec(job),
            card::settle_target(job),
        ) else {
            return Outcome::failed(CODE_TIM_UNAVAILABLE, "no terminal - nothing printed");
        };
        let trans_seq = match copy_target(&journal, job, target) {
            Ok(seq) => seq,
            Err(refusal) => return refusal,
        };
        let job_id = job.id;
        let journal_in = Arc::clone(&journal);
        let receipts = match self
            .one_session(spec, "nothing printed", move |t, _| {
                copy_session(t, &journal_in, job_id, trans_seq)
            })
            .await
        {
            Ok(Ok(receipts)) => receipts,
            Ok(Err(outcome)) | Err(outcome) => return outcome,
        };
        info!(
            job_id,
            card_job_id = target,
            trans_seq,
            cardholder_slip = receipts.cardholder.is_some(),
            merchant_slip = receipts.merchant.is_some(),
            "card receipt copy printed by the terminal"
        );
        Outcome::Done(reports::copy_result(target, trans_seq, &receipts))
    }

    /// A shift / balance / copy job whose outcome is unknown.
    pub(super) async fn reconcile_terminal_op(&self, entry: &JournalEntry) -> Reconciliation {
        let job = &entry.job;
        if entry.device_contact_at.is_none() {
            return Reconciliation::Failed {
                code: CODE_NOT_EXECUTED.into(),
                text: "the terminal was never asked (no device contact journaled) - nothing ran"
                    .into(),
            };
        }
        if job.job_type == JOB_PRINT_COPY {
            return Reconciliation::Failed {
                code: CODE_PRINT_UNKNOWN.into(),
                text: "the copy was interrupted - whether it printed is unknown, no money is involved; ask for a new copy".into(),
            };
        }
        let Some(op) = TerminalOp::of_job(&job.job_type) else {
            return Reconciliation::StillUnknown("not a terminal operation".into());
        };
        if op == TerminalOp::Balance {
            return Reconciliation::StillUnknown(
                "left for a person: the terminal does not tell whether the interrupted balance ran - the next balance (or the 23:50 safety balance) closes the period anyway".into(),
            );
        }
        let later = self
            .journal
            .as_ref()
            .map(|j| j.card_later_shift_job(job.id, job.kasa_id).unwrap_or(true))
            .unwrap_or(true);
        if later {
            return Reconciliation::StillUnknown(
                "left for a person: a later card.activate / card.deactivate of this till governs the terminal's shift".into(),
            );
        }
        // the SDK: an open (closed) shift answers ok to another activate
        // (deactivate) — running it again is the reconcile
        let Some(spec) = self.job_spec(job) else {
            return Reconciliation::StillUnknown("the till has no terminal any more".into());
        };
        info!(
            job_id = job.id,
            ?op,
            "card shift job reconciled by running it again (idempotent on the terminal)"
        );
        match self
            .one_session(spec, "nothing ran", move |t, tid| {
                op.call(t, tid.as_deref())
            })
            .await
        {
            Ok(Ok(mut result)) => {
                result["reconciled"] = json!(true);
                Reconciliation::Done(result)
            }
            Ok(Err(e)) => Reconciliation::StillUnknown(format!("the terminal answered {e}")),
            Err(outcome) => Reconciliation::StillUnknown(format!(
                "the terminal could not be asked: {}",
                outcome.label()
            )),
        }
    }

    /// One safety balance on `spec`'s terminal for `local_date` — its row
    /// journaled right BEFORE the call (inside the session, so a wait behind a
    /// held payment that dies journals nothing), never twice for one terminal
    /// and day, never after `until` (the day's midnight: a wait past it files
    /// nothing under the old day). `None` = it already ran (or a balance job
    /// of that day ended `done`, or the day is over); `Some((reached,
    /// outcome))` — `reached` = the balance was sent to the terminal.
    pub(super) async fn safety_balance(
        &self,
        spec: TerminalSpec,
        local_date: &str,
        until: Instant,
    ) -> Option<(bool, Outcome)> {
        let journal = self.journal.clone()?;
        // cheap check first: no session at all once the day's balance ran
        match journal.card_balance_ran(&spec.ip, local_date) {
            Ok(false) => {}
            Ok(true) => return None,
            Err(e) => {
                warn!(terminal = %spec.ip, error = %format!("{e:#}"), "journal: balance of the day not readable - no safety balance");
                return None;
            }
        }
        let ip = spec.ip.clone();
        info!(terminal = %ip, local_date, "TIM safety balance (no balance ran today)");
        let date = local_date.to_string();
        let journal_in = Arc::clone(&journal);
        let ip_in = ip.clone();
        let ended = match self
            .one_session(spec, "nothing ran", move |t, tid| {
                safety_session(t, tid, &journal_in, &ip_in, &date, until)
            })
            .await
        {
            Ok(ended) => ended,
            Err(outcome @ Outcome::Failed { .. }) => SafetyEnd::NotSent(outcome),
            // the session thread died: a row may exist (then no retry)
            Err(outcome) => {
                error!(terminal = %ip, local_date, outcome = outcome.label(), "TIM safety balance session died - its row, if written, keeps the day");
                return Some((false, outcome));
            }
        };
        match ended {
            SafetyEnd::AlreadyRan => None,
            SafetyEnd::NotSent(outcome) => {
                warn!(terminal = %ip, local_date, outcome = outcome.label(), "TIM safety balance did not run - nothing journaled, retried next minute");
                Some((false, outcome))
            }
            SafetyEnd::Sent(id, outcome) => {
                journal_balance(&journal, Some(id), &outcome);
                info!(terminal = %ip, local_date, balance = id, outcome = outcome.label(), result = ?outcome_total(&outcome), "TIM safety balance finished - journaled (Odoo has no channel for it yet)");
                Some((true, outcome))
            }
        }
    }
}

/// How a safety balance's session ended.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SafetyEnd {
    /// A balance of the day ran meanwhile (or its row exists), or the day
    /// ended while it waited: nothing sent.
    AlreadyRan,
    /// Nothing ran (journal error, the terminal refused it): no row kept.
    NotSent(Outcome),
    /// Sent with balance row `id`.
    Sent(i64, Outcome),
}

/// Inside the session (the terminal's lock held): re-check the day, journal
/// the row, then the balance. A refusal in which the terminal says it did
/// not run drops the row again, so the next tick retries.
pub(super) fn safety_session(
    terminal: &mut dyn TimTerminal,
    tid: Option<String>,
    journal: &Journal,
    ip: &str,
    date: &str,
    until: Instant,
) -> SafetyEnd {
    if Instant::now() >= until {
        info!(terminal = %ip, date, "the safety balance waited past midnight - not sent for the old day");
        return SafetyEnd::AlreadyRan;
    }
    let journal_fail = |e: anyhow::Error| {
        SafetyEnd::NotSent(Outcome::failed(
            CODE_BRIDGE_ERROR,
            format!("journal: {e:#} - nothing ran"),
        ))
    };
    match journal.card_balance_ran(ip, date) {
        Ok(false) => {}
        Ok(true) => return SafetyEnd::AlreadyRan,
        Err(e) => return journal_fail(e),
    }
    let id = match journal.card_balance_contact(ip, date, SOURCE_SAFETY, None) {
        Ok(Some(id)) => id,
        Ok(None) => return SafetyEnd::AlreadyRan,
        Err(e) => return journal_fail(e),
    };
    match terminal.balance() {
        Ok(report) => SafetyEnd::Sent(
            id,
            Outcome::Done(reports::balance_result(tid.as_deref(), &report)),
        ),
        Err(e) => match op_error(TerminalOp::Balance, &e) {
            refused @ Outcome::Failed { .. } => {
                let forgotten = journal.card_balance_forget(id).unwrap_or(false);
                info!(
                    balance = id,
                    forgotten,
                    rc = e.code,
                    "safety balance refused by the terminal - nothing ran"
                );
                SafetyEnd::NotSent(refused)
            }
            unknown => SafetyEnd::Sent(id, unknown),
        },
    }
}

/// The EUR total of a balance / shift result, for the log line.
pub(super) fn outcome_total(outcome: &Outcome) -> Option<&Value> {
    match outcome {
        Outcome::Done(result) => result.get("total"),
        _ => None,
    }
}

/// The `trans_seq` of the payment a `card.print_copy` names — a
/// `card.purchase` / `card.credit` / `card.reversal` of the same till this
/// bridge ran, with its terminal answer journaled.
fn copy_target(journal: &Journal, job: &DeviceJob, target: i64) -> Result<i64, Outcome> {
    let refuse = |why: String| {
        Outcome::failed(
            CODE_UNKNOWN_PURCHASE,
            format!("card_job_id {target}: {why} - nothing printed"),
        )
    };
    let entry = match journal.get(target) {
        Ok(Some(entry)) => entry,
        Ok(None) => return Err(refuse("this bridge never received that card job".into())),
        Err(e) => {
            return Err(Outcome::failed(
                CODE_BRIDGE_ERROR,
                format!("journal: {e:#} - nothing printed"),
            ));
        }
    };
    let payment =
        TwoPhase::of_job(&entry.job.job_type).is_some() || entry.job.job_type == card::JOB_REVERSAL;
    if !payment || entry.job.kasa_id != job.kasa_id {
        return Err(refuse("not a card payment / reversal of this till".into()));
    }
    let seq = journal
        .card_payment(target)
        .ok()
        .flatten()
        .and_then(|c| c.trans_seq);
    match (seq, entry.state) {
        (Some(seq), state) if state != JobState::Failed => Ok(seq),
        _ => Err(refuse(format!(
            "no terminal receipt for it (state {})",
            entry.state.as_str()
        ))),
    }
}

#[cfg(test)]
pub(super) fn sim_balance() -> reports::BalanceReport {
    reports::BalanceReport {
        counters: Some(reports::Counters {
            seq_counter: Some(3),
            brands: vec![reports::BrandCounter {
                brand_name: Some("VISA".into()),
                acq_id: Some(2),
                count: Some(1),
                totals: vec![reports::Total {
                    currency_eur: Some(true),
                    count: Some(1),
                    amount_minor: Some(100),
                    details: vec![],
                }],
            }],
        }),
        receipts: Receipts {
            merchant: Some("*** Daily closing ***".into()),
            cardholder: None,
        },
    }
}
