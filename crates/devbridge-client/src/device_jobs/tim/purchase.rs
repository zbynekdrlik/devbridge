//! One `card.purchase` — or `card.credit`, the same two-phase flow (#102
//! D3) — on its terminal (#102 lane D2): the HELD session.
//!
//! connect → login → terminal id → last transaction (the baseline
//! `trans_seq`) → journal the contact → `ta_terminal_transaction` (purchase,
//! AutoCommit off) → on the authorisation journal it + owe the
//! `phase: "authorized"` ack + start the slip print, then WAIT on the same
//! session for `card.commit` / `card.rollback` (a [`SettleRequest`] from the
//! settle queue), Odoo's refusal of the authorisation, or the rollback
//! deadline → commit / rollback → logout → disconnect.
//!
//! The session is never ended while the terminal waits for the commit: the
//! EftSimulator refuses a logout there (`request_pending`) and the
//! following disconnect hangs (measured 4.11.2026), so the connection that
//! authorised is the one that commits or rolls back.
//!
//! Everything blocks — the executor runs it on a blocking thread holding
//! the terminal's session lock.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use super::card::{
    CODE_BRIDGE_ERROR, CODE_TERMINAL_ERROR, CODE_TERMINAL_UNREACHABLE, CODE_WRONG_TERMINAL,
    LastTransaction, RC_COMMIT_TIMEOUT, TwoPhase, TxFacts, TxFailure, authorisation_problem,
    baseline, classify_transaction_error,
};
use super::{TerminalSpec, TimBackend, TimError, TimTerminal};

/// How often a held authorisation checks whether Odoo refused it.
pub const REFUSAL_POLL: Duration = Duration::from_millis(500);

/// Between two release rollbacks of a session whose outcome is unknown.
pub const RELEASE_RETRY: Duration = Duration::from_secs(2);

/// `ta_c_rc_sixml_wrong_state`: a rollback with nothing pending (measured on
/// the simulator 4.11.2026) — the terminal does not wait for a commit.
pub const RC_NOTHING_PENDING: i32 = 68;

/// What the purchase needs to know.
#[derive(Debug, Clone)]
pub struct PurchaseRequest {
    pub job_id: i64,
    /// Purchase or credit.
    pub kind: TwoPhase,
    pub amount_minor: i64,
    /// Rollback this long after the authorisation without a decision.
    pub deadline: Duration,
    /// The highest `trans_seq` devbridge stored for this terminal (the
    /// baseline when the terminal's last transaction carries none).
    pub stored_max_seq: Option<i64>,
    /// After an unknown end the session is released with rollbacks for at
    /// most this long (the terminal's CommitTimeout — it reverses an
    /// uncommitted authorisation itself by then) before the logout: a
    /// logout while it waits for a commit is refused and the disconnect
    /// hangs.
    pub release_within: Duration,
}

/// Why the contact could not be journaled — the transaction is NOT sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactRefusal {
    /// The terminal was already asked for this job (an earlier attempt may
    /// have charged): never a second transaction, the outcome is unknown.
    AlreadyAsked,
    /// The journal write failed: nothing was sent.
    Journal(String),
}

/// The journal / print side of a purchase (the executor's).
pub trait PurchaseHooks: Send {
    /// About to send the transaction: journal the baseline + the device
    /// contact. `Err` = do NOT transact.
    fn contact(&mut self, baseline: Option<i64>) -> Result<(), ContactRefusal>;
    /// The terminal authorised: journal it, owe the ack, print the slips.
    /// `Err` = it could not be journaled — the payment is rolled back.
    fn authorized(&mut self, facts: &TxFacts, tid: Option<&str>) -> Result<(), String>;
    /// Odoo refused the authorisation (`too_late`): roll back at once.
    fn authorization_refused(&mut self) -> bool;
    /// The payment is decided: no settle request can reach it any more
    /// (called before the late ones are answered).
    fn released(&mut self);
}

/// A decision for a held payment (from `card.commit` / `card.rollback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOp {
    Commit,
    Rollback,
}

/// The payment's state after a settle request, answered to the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleReply {
    Committed,
    RolledBack(String),
    Unknown(String),
}

pub struct SettleRequest {
    pub op: SettleOp,
    pub reply: tokio::sync::oneshot::Sender<SettleReply>,
}

/// How a purchase ended.
#[derive(Debug, Clone, PartialEq)]
pub enum PurchaseEnd {
    /// No money was taken; nothing was authorised.
    Refused { code: &'static str, text: String },
    /// Authorised and committed.
    Committed {
        facts: Box<TxFacts>,
        tid: Option<String>,
    },
    /// Authorised, then rolled back: no money taken.
    RolledBack(String),
    /// Unknown whether money was taken — reconciled, never retried.
    Unknown(String),
}

impl PurchaseEnd {
    /// The answer to a settle request that arrives after the decision.
    fn settle_reply(&self) -> SettleReply {
        match self {
            Self::Committed { .. } => SettleReply::Committed,
            Self::RolledBack(why) => SettleReply::RolledBack(why.clone()),
            Self::Refused { code, text } => SettleReply::RolledBack(format!("{code}: {text}")),
            Self::Unknown(why) => SettleReply::Unknown(why.clone()),
        }
    }
}

fn refused(code: &'static str, what: &str, e: &TimError) -> PurchaseEnd {
    PurchaseEnd::Refused {
        code,
        text: format!("{what}: {e} - the transaction was not sent, nothing charged"),
    }
}

/// Run one purchase on `spec`'s terminal. After a connect, logout (when
/// logged in) and disconnect always run — but only once the terminal no
/// longer waits for a commit.
pub fn run_purchase(
    backend: &dyn TimBackend,
    spec: &TerminalSpec,
    req: &PurchaseRequest,
    hooks: &mut dyn PurchaseHooks,
    settle: &Receiver<SettleRequest>,
) -> PurchaseEnd {
    let mut terminal = match super::open_terminal(backend, spec) {
        Ok(t) => t,
        Err(e) => return refused(CODE_TERMINAL_ERROR, "TIM terminal object", &e),
    };
    if let Err(e) = terminal.connect() {
        return refused(
            CODE_TERMINAL_UNREACHABLE,
            "the terminal could not be reached",
            &e,
        );
    }
    let end = match terminal.login() {
        Ok(()) => {
            let end = logged_in(terminal.as_mut(), spec, req, hooks, settle);
            if matches!(end, PurchaseEnd::Unknown(_)) {
                release(terminal.as_mut(), req);
            }
            if let Err(e) = terminal.logout() {
                warn!(job_id = req.job_id, terminal = %spec.ip, error = %e, "TIM logout failed");
            }
            end
        }
        Err(e) => refused(CODE_TERMINAL_ERROR, "TIM login", &e),
    };
    if let Err(e) = terminal.disconnect() {
        warn!(job_id = req.job_id, terminal = %spec.ip, error = %e, "TIM disconnect failed");
    }
    // a settle request that came after the decision gets the decision
    hooks.released();
    while let Ok(late) = settle.try_recv() {
        let _ = late.reply.send(end.settle_reply());
    }
    end
}

fn logged_in(
    terminal: &mut dyn TimTerminal,
    spec: &TerminalSpec,
    req: &PurchaseRequest,
    hooks: &mut dyn PurchaseHooks,
    settle: &Receiver<SettleRequest>,
) -> PurchaseEnd {
    let tid = match terminal.terminal_id() {
        Ok(tid) => tid,
        Err(e) => return refused(CODE_TERMINAL_ERROR, "TIM terminal id", &e),
    };
    if let Some(expected) = &spec.expected_tid
        && tid.as_deref() != Some(expected.as_str())
    {
        return PurchaseEnd::Refused {
            code: CODE_WRONG_TERMINAL,
            text: format!(
                "the terminal at {} reported id {} but the till expects {expected} - nothing charged",
                spec.ip,
                tid.as_deref().unwrap_or("(none)")
            ),
        };
    }
    let last: Option<LastTransaction> = terminal
        .last_transaction()
        .map_err(|e| warn!(job_id = req.job_id, terminal = %spec.ip, error = %e, "TIM last transaction not readable - the baseline falls back to the journal"))
        .ok();
    let base = baseline(last.as_ref(), req.stored_max_seq);
    match hooks.contact(base) {
        Ok(()) => {}
        Err(ContactRefusal::AlreadyAsked) => {
            error!(
                job_id = req.job_id,
                "the terminal was already asked for this job - NO second transaction, the outcome is reconciled"
            );
            return PurchaseEnd::Unknown(
                "the terminal was already asked for this job - no second transaction, reconciled with the terminal".into(),
            );
        }
        Err(ContactRefusal::Journal(why)) => {
            return PurchaseEnd::Refused {
                code: CODE_BRIDGE_ERROR,
                text: format!(
                    "the journal did not record the terminal contact ({why}) - the transaction was not sent, nothing charged"
                ),
            };
        }
    }
    info!(job_id = req.job_id, terminal = %spec.ip, kind = ?req.kind, amount_minor = req.amount_minor, baseline_seq = ?base, "TIM transaction sent to the terminal");
    let facts = match terminal.transaction(req.kind, req.amount_minor) {
        Ok(facts) => facts,
        Err(e) => return failed_transaction(req, &e),
    };
    let authorised_at = Instant::now();
    info!(job_id = req.job_id, terminal = %spec.ip, trans_seq = ?facts.trans_seq, amount_minor = ?facts.amount_minor, "TIM transaction response: authorised");
    if let Some(problem) = authorisation_problem(&facts, req.amount_minor) {
        error!(job_id = req.job_id, %problem, "TIM authorisation cannot pay this job - rolling it back");
        return roll_back(terminal, req, &format!("rolled back at once: {problem}"));
    }
    if let Err(why) = hooks.authorized(&facts, tid.as_deref()) {
        error!(job_id = req.job_id, %why, "TIM authorisation could not be journaled - rolling it back");
        return roll_back(
            terminal,
            req,
            &format!("rolled back at once: the journal failed ({why})"),
        );
    }
    hold(terminal, req, hooks, settle, authorised_at, facts, tid)
}

/// A transaction that did not authorise: the terminal's "no" is
/// `Refused`; anything else is unknown (the session is released with a
/// rollback first, its answer changes nothing).
fn failed_transaction(req: &PurchaseRequest, e: &TimError) -> PurchaseEnd {
    match classify_transaction_error(e.code) {
        TxFailure::NothingCharged(code) => {
            info!(
                job_id = req.job_id,
                rc = e.code,
                code,
                "TIM transaction refused by the terminal - nothing charged"
            );
            PurchaseEnd::Refused {
                code,
                text: format!("the terminal refused the payment: {e} - nothing charged"),
            }
        }
        TxFailure::Unknown => {
            warn!(
                job_id = req.job_id,
                rc = e.code,
                "TIM transaction outcome unknown - released, then reconciled with the terminal"
            );
            PurchaseEnd::Unknown(format!(
                "the transaction failed with an unknown outcome: {e} - reconciled with the terminal"
            ))
        }
    }
}

/// The outcome is unknown and the terminal may still wait for a commit:
/// roll back until it answers (done, or nothing pending) or
/// `release_within` passed — never a logout while it waits.
fn release(terminal: &mut dyn TimTerminal, req: &PurchaseRequest) {
    let until = Instant::now() + req.release_within;
    loop {
        let answer = terminal.rollback();
        let released = match &answer {
            Ok(()) => true,
            Err(e) => e.code == RC_NOTHING_PENDING,
        };
        info!(
            job_id = req.job_id,
            ?answer,
            released,
            "TIM release rollback after an unknown outcome"
        );
        if released {
            return;
        }
        // its own check: the window ends the loop whatever `released` says
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(RELEASE_RETRY));
    }
}

fn roll_back(terminal: &mut dyn TimTerminal, req: &PurchaseRequest, why: &str) -> PurchaseEnd {
    match terminal.rollback() {
        Ok(()) => {
            info!(job_id = req.job_id, %why, "TIM authorisation rolled back - no money taken");
            PurchaseEnd::RolledBack(format!(
                "{why} - the authorisation was rolled back, no money taken"
            ))
        }
        Err(e) => {
            error!(job_id = req.job_id, %why, error = %e, "TIM rollback failed - outcome unknown");
            PurchaseEnd::Unknown(format!("{why}, but the rollback failed: {e}"))
        }
    }
}

fn commit(
    terminal: &mut dyn TimTerminal,
    req: &PurchaseRequest,
    facts: TxFacts,
    tid: Option<String>,
) -> PurchaseEnd {
    match terminal.commit() {
        Ok(()) => {
            info!(job_id = req.job_id, trans_seq = ?facts.trans_seq, "TIM payment committed");
            PurchaseEnd::Committed {
                facts: Box::new(facts),
                tid,
            }
        }
        Err(e) if e.code == RC_COMMIT_TIMEOUT => {
            warn!(job_id = req.job_id, error = %e, "TIM commit too late - the terminal reversed the authorisation itself");
            PurchaseEnd::RolledBack(format!(
                "the commit came after the terminal's commit timeout ({e}) - the terminal reversed the authorisation, no money taken"
            ))
        }
        Err(e) => {
            error!(job_id = req.job_id, error = %e, "TIM commit failed - outcome unknown");
            PurchaseEnd::Unknown(format!("the commit failed: {e}"))
        }
    }
}

/// Wait for the decision on a held authorisation.
fn hold(
    terminal: &mut dyn TimTerminal,
    req: &PurchaseRequest,
    hooks: &mut dyn PurchaseHooks,
    settle: &Receiver<SettleRequest>,
    authorised_at: Instant,
    facts: TxFacts,
    tid: Option<String>,
) -> PurchaseEnd {
    let deadline = authorised_at + req.deadline;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let why = format!(
                "no card.commit within {} s of the authorisation (devbridge deadline)",
                req.deadline.as_secs()
            );
            return roll_back(terminal, req, &why);
        }
        match settle.recv_timeout(remaining.min(REFUSAL_POLL)) {
            Ok(SettleRequest { op, reply }) => {
                info!(
                    job_id = req.job_id,
                    ?op,
                    waited_ms = authorised_at.elapsed().as_millis() as u64,
                    "TIM settle request for the held payment"
                );
                let end = match op {
                    SettleOp::Commit => commit(terminal, req, facts, tid),
                    SettleOp::Rollback => roll_back(terminal, req, "card.rollback"),
                };
                let _ = reply.send(end.settle_reply());
                return end;
            }
            Err(RecvTimeoutError::Disconnected) => {
                // no settle queue can reach this payment any more
                return roll_back(terminal, req, "no settle channel for the held payment");
            }
            Err(RecvTimeoutError::Timeout) => {
                if hooks.authorization_refused() {
                    return roll_back(terminal, req, "Odoo refused the authorisation (too late)");
                }
            }
        }
    }
}
