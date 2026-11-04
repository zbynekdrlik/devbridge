//! Card payment decisions of the TIM executor (#102 lane D2) — pure, no
//! library and no terminal: what a transaction result code means, the job
//! result Odoo maps (design odoo-erp issue 7598 §6, Odoo's
//! `device_job_card.md`), the slip lines, and the restart correlation.
//! Contract: `docs/device-jobs-card.md`.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};

use crate::device_jobs::api::DeviceJob;

/// `card.purchase` — authorise, then commit / roll back (two-phase).
pub const JOB_PURCHASE: &str = "card.purchase";
/// `card.commit {card_job_id, trans_seq?, amount?}` — composed by Odoo.
pub const JOB_COMMIT: &str = "card.commit";
/// `card.rollback {card_job_id, trans_seq?, amount?}`.
pub const JOB_ROLLBACK: &str = "card.rollback";

// Failure codes of a card job (acked `failed`: no money was taken).
pub const CODE_TERMINAL_UNREACHABLE: &str = "terminal_unreachable";
pub const CODE_TERMINAL_ERROR: &str = "terminal_error";
pub const CODE_WRONG_TERMINAL: &str = "wrong_terminal";
pub const CODE_DECLINED: &str = "declined";
pub const CODE_CANCELLED: &str = "cancelled";
pub const CODE_CARD_ERROR: &str = "card_error";
pub const CODE_TERMINAL_BUSY: &str = "terminal_busy";
pub const CODE_ROLLED_BACK: &str = "rolled_back";
pub const CODE_NOT_EXECUTED: &str = "not_executed";
pub const CODE_NO_TERMINAL: &str = "no_terminal";
pub const CODE_BAD_CURRENCY: &str = "bad_currency";
pub const CODE_BAD_PAYLOAD: &str = "bad_payload";
pub const CODE_TIM_UNAVAILABLE: &str = "tim_unavailable";
pub const CODE_BRIDGE_ERROR: &str = "bridge_error";
pub const CODE_ALREADY_ROLLED_BACK: &str = "already_rolled_back";
pub const CODE_ALREADY_COMMITTED: &str = "already_committed";
pub const CODE_UNKNOWN_PURCHASE: &str = "unknown_purchase";
pub const CODE_SETTLE_MISMATCH: &str = "settle_mismatch";

/// `ta_c_rc_trx_commit_timeout`: the commit came after the terminal's own
/// CommitTimeout — it has reversed the authorisation itself.
pub const RC_COMMIT_TIMEOUT: i32 = 89;

/// `ta_c_tt_purchase`.
pub const TRANSACTION_TYPE_PURCHASE: i32 = 1;
/// `ta_c_o_successful` / `ta_c_o_failed` (`ta_e_outcome_t`).
pub const OUTCOME_SUCCESSFUL: i32 = 1;
pub const OUTCOME_FAILED: i32 = 2;

/// Widest slip line (the Portos `printers/print` path, 40 columns).
pub const SLIP_WIDTH: usize = 40;
/// Most lines of one slip print (the `fiscal.print_text` limit).
pub const SLIP_MAX_LINES: usize = 80;
/// Title of the cardholder / merchant slip print.
pub const CARDHOLDER_SLIP_TITLE: &str = "Platba kartou - doklad zakaznika";
pub const MERCHANT_SLIP_TITLE: &str = "Platba kartou - doklad obchodnika";

/// Added to the terminal's CommitTimeout before a restart reconcile asks
/// the terminal: by then an uncommitted authorisation is decided there.
pub const SETTLE_MARGIN_S: i64 = 10;

/// What a transaction response says — only the fields devbridge keeps.
/// The full PAN (`card_number`) and `card_ref` are never read from the
/// library, so they can be neither stored nor logged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxFacts {
    pub transaction_type: Option<i32>,
    pub amount_minor: Option<i64>,
    /// `Some(true)` = the amount is in EUR.
    pub currency_eur: Option<bool>,
    pub trans_seq: Option<i64>,
    pub trans_ref: Option<i64>,
    pub trm_trans_ref: Option<String>,
    pub auth_code: Option<String>,
    pub pos_entry_mode: Option<i32>,
    pub cvm: Option<i32>,
    pub brand_name: Option<String>,
    pub card_number_printable: Option<String>,
    pub merchant_receipt: Option<String>,
    pub cardholder_receipt: Option<String>,
}

/// `ta_terminal_transaction_info_request`: the terminal's LATEST transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastTransaction {
    /// `ta_e_transaction_type_t`, 0 = none yet.
    pub transaction_type: i32,
    /// `ta_e_outcome_t`.
    pub outcome: i32,
    /// `None` for a failed one (the simulator answers a decline without a
    /// response) and before the first transaction.
    pub facts: Option<TxFacts>,
}

/// What a failed `ta_terminal_transaction` means for the money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxFailure {
    /// The terminal answered that the payment did not happen.
    NothingCharged(&'static str),
    /// Anything else: the outcome is unknown (reconciled, never retried).
    Unknown,
}

/// Classify the result code of a failed transaction. Only codes in which
/// the TERMINAL says the payment failed are "nothing charged"; a lost
/// connection, a timeout, `host_timeout_outcome_unknown` (151) and every
/// code not listed are unknown.
pub fn classify_transaction_error(rc: i32) -> TxFailure {
    match rc {
        // ECR cancel (incl. the library's own card-wait timeout), Stop on
        // the terminal, cardholder timeout, card removed
        2 | 53..=55 => TxFailure::NothingCharged(CODE_CANCELLED),
        32..=48 | 74 | 76 => TxFailure::NothingCharged(CODE_CARD_ERROR),
        75 | 77..=88 | 91..=93 | 97..=118 | 123..=138 | 152 | 154..=156 | 158..=161 => {
            TxFailure::NothingCharged(CODE_DECLINED)
        }
        69 | 70 | 162 => TxFailure::NothingCharged(CODE_TERMINAL_BUSY),
        _ => TxFailure::Unknown,
    }
}

/// The amount of a `card.purchase` in cents: a whole number of cents > 0.
pub fn amount_minor(job: &DeviceJob) -> Option<i64> {
    let amount = job.payload.get("amount")?.as_f64()?;
    let cents = (amount * 100.0).round();
    let whole = (amount * 100.0 - cents).abs() < 1e-6;
    (amount.is_finite() && whole && (1.0..1e13).contains(&cents)).then_some(cents as i64)
}

/// A purchase's currency: absent / null or `EUR` (the terminal's currency).
pub fn currency_ok(job: &DeviceJob) -> bool {
    match job.payload.get("currency") {
        None | Some(Value::Null) => true,
        Some(Value::String(c)) => c == "EUR",
        Some(_) => false,
    }
}

/// `card_job_id` of a `card.commit` / `card.rollback` (a positive integer).
pub fn settle_target(job: &DeviceJob) -> Option<i64> {
    job.payload
        .get("card_job_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
}

/// A `trans_seq` / `amount` a settle job names that contradicts the stored
/// authorisation (`None` = consistent, or not named).
pub fn settle_mismatch(job: &DeviceJob, authorised: &Value) -> Option<String> {
    let tx = authorised.get("transaction")?;
    if let Some(seq) = job.payload.get("trans_seq").filter(|v| !v.is_null())
        && seq.as_i64() != tx.get("trans_seq").and_then(Value::as_i64)
    {
        return Some(format!(
            "trans_seq {seq} is not the authorised {}",
            tx.get("trans_seq").unwrap_or(&Value::Null)
        ));
    }
    if let Some(amount) = job.payload.get("amount").filter(|v| !v.is_null()) {
        let theirs = amount.as_f64().map(|a| (a * 100.0).round() as i64);
        let ours = tx
            .get("amount")
            .and_then(Value::as_f64)
            .map(|a| (a * 100.0).round() as i64);
        if theirs != ours {
            return Some(format!(
                "amount {amount} is not the authorised {}",
                tx.get("amount").unwrap_or(&Value::Null)
            ));
        }
    }
    None
}

/// A printable card number masked by devbridge to its last four digits
/// (`XXXXXXXXXXXX5240`); every other digit becomes `X`.
pub fn mask_card_number(printable: &str) -> String {
    let digits = printable.chars().filter(char::is_ascii_digit).count();
    let mut seen = 0;
    printable
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                seen += 1;
                if seen + 4 > digits { c } else { 'X' }
            } else {
                c
            }
        })
        .collect()
}

/// The last four digits of a printable card number.
pub fn last4(printable: &str) -> Option<String> {
    let digits: Vec<char> = printable.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 4).then(|| digits[digits.len() - 4..].iter().collect())
}

/// CVM name (`ta_e_cvm_t`).
pub fn cvm_name(cvm: Option<i32>) -> &'static str {
    match cvm {
        Some(1) => "pin",
        Some(2) => "pin_signature",
        Some(3) => "signature",
        Some(4) => "on_device",
        Some(5) => "no_cvm",
        Some(6) => "other",
        _ => "undefined",
    }
}

/// The merchant slip is printed only when the cardholder signs.
pub fn needs_signature(cvm: Option<i32>) -> bool {
    matches!(cvm, Some(2) | Some(3))
}

/// The job result of an authorised / committed payment (Odoo's O1 shape).
pub fn card_result(state: &str, tid: Option<&str>, facts: &TxFacts) -> Value {
    let printable = facts.card_number_printable.as_deref().map(mask_card_number);
    json!({
        "state": state,
        "terminal": {"tid": tid},
        "transaction": {
            "trans_seq": facts.trans_seq,
            "trm_trans_ref": facts.trm_trans_ref,
            "trans_ref": facts.trans_ref.map(|r| r.to_string()),
            "auth_code": facts.auth_code,
            "amount": facts.amount_minor.map(|m| m as f64 / 100.0),
            "currency": if facts.currency_eur == Some(true) { Some("EUR") } else { None },
            "pos_entry_mode": facts.pos_entry_mode.map(|m| m.to_string()),
            "cvm": cvm_name(facts.cvm),
        },
        "card": {
            "brand_name": facts.brand_name,
            "card_number_printable": printable,
            "last4": facts.card_number_printable.as_deref().and_then(last4),
        },
        "receipts": {
            "merchant": facts.merchant_receipt,
            "cardholder": facts.cardholder_receipt,
        },
    })
}

/// Why an authorisation cannot be kept: it must be rolled back at once
/// (`None` = it pays exactly the job).
pub fn authorisation_problem(facts: &TxFacts, amount_minor: i64) -> Option<String> {
    if facts.trans_seq.is_none() {
        return Some("the terminal answered without a trans_seq".to_string());
    }
    if facts.amount_minor != Some(amount_minor) {
        return Some(format!(
            "the terminal authorised {:?} cents, the job asked {amount_minor}",
            facts.amount_minor
        ));
    }
    if facts.currency_eur != Some(true) {
        return Some("the terminal authorised another currency than EUR".to_string());
    }
    None
}

/// The baseline of a purchase: the terminal's last `trans_seq` before it.
/// No transaction yet = 0; a last transaction without a response (a
/// failed one) = the highest seq devbridge stored for this terminal;
/// `None` = unknown (a crash then leaves the payment for a person).
pub fn baseline(last: Option<&LastTransaction>, stored_max: Option<i64>) -> Option<i64> {
    match last {
        Some(l) if l.transaction_type == 0 && l.facts.is_none() => Some(stored_max.unwrap_or(0)),
        Some(l) => l.facts.as_ref().and_then(|f| f.trans_seq).or(stored_max),
        None => stored_max,
    }
}

/// What the terminal's last transaction says about OUR purchase.
#[derive(Debug, Clone, PartialEq)]
pub enum Correlation {
    /// Ours, and the money was taken.
    Charged(TxFacts),
    /// Ours, and it failed (rolled back / reversed): no money.
    NotCharged,
    /// Cannot be proven either way — left for a person.
    Undecidable(String),
}

/// What the journal knows about the payment being reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ours {
    /// The terminal's last `trans_seq` before it.
    pub baseline: Option<i64>,
    pub amount_minor: i64,
    /// Its own `trans_seq`, when the authorisation was journaled.
    pub trans_seq: Option<i64>,
    /// Another payment was sent to the same terminal after it: the
    /// terminal's LATEST transaction can no longer be proven ours.
    pub later_payment: bool,
}

/// Ours = a purchase with a `trans_seq` above the stored baseline (exactly
/// the journaled one when the authorisation was stored) AND the job's
/// amount, and no later payment on that terminal. Anything else is never
/// guessed.
pub fn correlate(ours: Ours, last: &LastTransaction) -> Correlation {
    let Ours {
        baseline,
        amount_minor,
        trans_seq,
        later_payment,
    } = ours;
    if later_payment {
        return Correlation::Undecidable(
            "another payment ran on this terminal after it - its last transaction cannot be proven to be this one".to_string(),
        );
    }
    let Some(baseline) = baseline else {
        return Correlation::Undecidable(
            "the terminal's trans_seq before this payment is unknown".to_string(),
        );
    };
    let Some(facts) = &last.facts else {
        return Correlation::Undecidable(format!(
            "the terminal's last transaction (type {}, outcome {}) carries no trans_seq",
            last.transaction_type, last.outcome
        ));
    };
    let seq_ok = match trans_seq {
        Some(expected) => facts.trans_seq == Some(expected),
        None => facts.trans_seq.is_some_and(|s| s > baseline),
    };
    let matches = last.transaction_type == TRANSACTION_TYPE_PURCHASE
        && seq_ok
        && facts.amount_minor == Some(amount_minor);
    if !matches {
        return Correlation::Undecidable(format!(
            "the terminal's last transaction (type {}, trans_seq {:?}, {:?} cents) is not this payment (after trans_seq {baseline}, authorised as {trans_seq:?}, {amount_minor} cents)",
            last.transaction_type, facts.trans_seq, facts.amount_minor
        ));
    }
    match last.outcome {
        OUTCOME_SUCCESSFUL => Correlation::Charged(facts.clone()),
        OUTCOME_FAILED => Correlation::NotCharged,
        other => Correlation::Undecidable(format!(
            "the terminal reports outcome {other} for this payment"
        )),
    }
}

/// When a restart reconcile may ask the terminal: the latest of the given
/// moments + the terminal's CommitTimeout + [`SETTLE_MARGIN_S`].
pub fn settle_after(moments: &[DateTime<Utc>], commit_timeout_s: u32) -> Option<DateTime<Utc>> {
    let latest = moments.iter().max()?;
    Some(*latest + ChronoDuration::seconds(i64::from(commit_timeout_s) + SETTLE_MARGIN_S))
}

/// A terminal slip as printable lines: control / format characters become
/// spaces, lines wider than [`SLIP_WIDTH`] wrap, trailing blank lines go,
/// at most [`SLIP_MAX_LINES`].
pub fn slip_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let clean: Vec<char> = raw
            .trim_end()
            .chars()
            .map(|c| {
                if c.is_control() || is_format(c) {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if clean.is_empty() {
            lines.push(String::new());
        }
        for chunk in clean.chunks(SLIP_WIDTH) {
            lines.push(chunk.iter().collect::<String>().trim_end().to_string());
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.truncate(SLIP_MAX_LINES);
    lines
}

/// Unicode Cf characters the Portos print checker refuses.
fn is_format(c: char) -> bool {
    matches!(u32::from(c), 0xAD | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
