//! What a Portos answer means for a job (#100) — pure functions, so every
//! row of the decision tables in `docs/device-jobs-fiscal.md` is unit
//! tested.
//!
//! The rule behind every row: `failed` ONLY when Portos proves nothing was
//! registered (a connection that never opened, a validation refusal before
//! anything is stored); everything else after the request may have reached
//! Portos is `indeterminate` and reconciled by the devbridge externalId.

use serde_json::{Value, json};

use super::http::Reply;
use super::payload::{FiscalOp, ReceiptKind};
use crate::device_jobs::executor::Outcome;

/// `device` of every Portos result.
pub const DEVICE: &str = "portos";

/// Portos could not be connected to — nothing was sent.
pub const CODE_UNREACHABLE: &str = "portos_unreachable";
/// Portos refused the request as invalid (HTTP 400, code -900).
pub const CODE_VALIDATION: &str = "portos_validation";
/// Portos wants credentials (HTTP 401).
pub const CODE_UNAUTHENTICATED: &str = "portos_unauthenticated";
/// This Portos has no such endpoint (HTTP 404 / 405 without a Portos code).
pub const CODE_ENDPOINT_MISSING: &str = "portos_endpoint_missing";
/// The lookup before a register failed — nothing was registered.
pub const CODE_LOOKUP_FAILED: &str = "portos_lookup_failed";
/// A receipt to copy does not exist (code -104).
pub const CODE_RECEIPT_NOT_FOUND: &str = "receipt_not_found";
/// Portos refused a drawer / copy request (another 4xx).
pub const CODE_REFUSED: &str = "portos_refused";
/// devbridge provably never sent the receipt (reconcile).
pub const CODE_NOT_EXECUTED: &str = "not_executed";
/// A drawer / copy / printed text whose outcome is unknown — harmless to repeat.
pub const CODE_RETRY_SAFE: &str = "outcome_unknown_retry_safe";

/// Portos' "receipt not found" code.
const PORTOS_RECEIPT_NOT_FOUND: i64 = -104;
/// Portos' validation error code.
const PORTOS_VALIDATION_ERROR: i64 = -900;
/// Longest Portos problem text kept in an ack.
const MAX_PROBLEM_CHARS: usize = 600;

/// Where a receipt result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The register POST answered it.
    Register,
    /// The lookup before the POST found it — nothing was posted.
    Lookup,
    /// The reconcile of an unknown outcome found it.
    Reconcile,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Lookup => "lookup",
            Self::Reconcile => "reconcile",
        }
    }
}

/// `online` (eKasa confirmed it, `uid` set), `offline` (stored in the CHDÚ,
/// sent to eKasa later — still a valid receipt) or `rejected` (eKasa refused
/// the stored receipt; a person must look at it).
pub fn receipt_state(result: &Value) -> &'static str {
    match result.get("isSuccessful") {
        Some(Value::Bool(true)) => "online",
        Some(Value::Bool(false)) => "rejected",
        _ => "offline",
    }
}

/// The job result of a Portos `RegisterReceiptResultDTO`. `None` when it is
/// not a receipt (no `request.data.okp` / `receiptNumber`) or belongs to
/// another externalId.
pub fn receipt_result(result: &Value, external_id: &str, via: Via) -> Option<Value> {
    let request = result.get("request")?;
    let data = request.get("data")?;
    let okp = data.get("okp").and_then(Value::as_str)?;
    let receipt_number = data.get("receiptNumber").filter(|n| n.is_number())?;
    if !external_id_matches(request.get("externalId"), external_id, via) {
        return None;
    }
    let state = receipt_state(result);
    let field = |v: Option<&Value>| v.cloned().unwrap_or(Value::Null);
    let mut receipt = json!({
        "device": DEVICE,
        "via": via.as_str(),
        "external_id": external_id,
        "state": state,
        "is_offline": state == "offline",
        "uid": field(result.pointer("/response/data/id")),
        "okp": okp,
        "receipt_number": receipt_number,
        "issue_date": field(data.get("issueDate")),
        "receipt_type": field(data.get("receiptType")),
        "amount": field(data.get("amount")),
        "rounding_amount": field(data.get("roundingAmount")),
        "cash_register_code": field(data.get("cashRegisterCode")),
        "portos_request_id": field(request.get("id")),
        "process_date": field(result.pointer("/response/processDate")),
        "ekasa_error": field(result.get("error")),
    });
    // A registered paragón (#115): Portos echoes its number in
    // `request.data.paragonNumber` (null on every other receipt).
    if let Some(number) = data.get("paragonNumber").filter(|n| n.is_number()) {
        receipt["paragon_number"] = number.clone();
    }
    Some(receipt)
}

/// Portos' `code` of a problem answer.
fn problem_code(body: &Value) -> Option<i64> {
    body.get("code").and_then(Value::as_i64)
}

/// A short text of a Portos problem answer: code, title, detail and the
/// field errors — Portos' own words (field paths and rules).
pub fn problem_text(status: u16, body: &Value) -> String {
    let mut parts = vec![format!("HTTP {status}")];
    if let Some(code) = problem_code(body) {
        parts.push(format!("code {code}"));
    }
    for key in ["title", "detail"] {
        if let Some(text) = body
            .get(key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            parts.push(text.to_string());
        }
    }
    if let Some(errors) = body.get("errors").and_then(Value::as_object) {
        for (field, messages) in errors {
            let messages: Vec<&str> = messages
                .as_array()
                .map(|m| m.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            parts.push(format!("{field}: {}", messages.join(" ")));
        }
    }
    parts.join(" | ").chars().take(MAX_PROBLEM_CHARS).collect()
}

/// A validation refusal (HTTP 400 / 422 with no code or -900): Portos
/// checks a request before it stores anything.
fn is_validation(status: u16, body: &Value) -> bool {
    matches!(status, 400 | 422)
        && problem_code(body).is_none_or(|code| code == PORTOS_VALIDATION_ERROR)
}

/// "This Portos has no such endpoint": 404 / 405 WITHOUT a Portos code.
fn is_missing_endpoint(status: u16, body: &Value) -> bool {
    matches!(status, 404 | 405) && problem_code(body).is_none()
}

/// The receipt lookup's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// A receipt with this externalId exists (the Portos result).
    Found(Value),
    /// Portos has no receipt with it.
    NotFound,
    /// Portos was not reachable — nothing was asked.
    Unreachable(String),
    /// Portos answered something else / not in time.
    Failed(String),
}

/// Whether a receipt answer belongs to `external_id`. A receipt found by a
/// lookup / reconcile MUST name it (a Portos that ignored the filter must
/// never hand another sale's receipt to this job); the answer to our own
/// register POST may leave it out.
fn external_id_matches(theirs: Option<&Value>, external_id: &str, via: Via) -> bool {
    match theirs.and_then(Value::as_str) {
        Some(theirs) => theirs == external_id,
        None => via == Via::Register,
    }
}

/// A Portos problem answer (`ProblemDetails`: an object with `status` or
/// `title`) — not the bare 404 of a route this Portos does not have.
fn is_problem(body: &Value) -> bool {
    body.get("status").is_some() || body.get("title").is_some()
}

/// `GET /api/v1/requests/receipts/receipt?CashRegisterCode=&ExternalId=`.
/// Not found = HTTP 204, a 4xx carrying code -104, or a 404 problem answer.
/// A bare 404 (no such route) or anything else is NOT "not found": the
/// receipt is then never posted (`portos_lookup_failed`).
pub fn classify_lookup(reply: Reply) -> Lookup {
    match reply {
        Reply::Unreachable(e) => Lookup::Unreachable(e),
        Reply::Lost(e) => Lookup::Failed(format!("no answer: {e}")),
        Reply::Answer { status: 200, body } if body.is_object() => Lookup::Found(body),
        Reply::Answer { status: 204, .. } => Lookup::NotFound,
        Reply::Answer { status: 404, body }
            if problem_code(&body).is_none() && is_problem(&body) =>
        {
            Lookup::NotFound
        }
        Reply::Answer { status, body }
            if (400..500).contains(&status)
                && problem_code(&body) == Some(PORTOS_RECEIPT_NOT_FOUND) =>
        {
            Lookup::NotFound
        }
        Reply::Answer { status, body } => Lookup::Failed(problem_text(status, &body)),
    }
}

/// The register POST's answer → the job outcome.
pub fn register_outcome(reply: Reply, kind: ReceiptKind, external_id: &str) -> Outcome {
    let reconciled = format!("reconciled by externalId {external_id}, never registered again");
    match reply {
        Reply::Unreachable(e) => Outcome::failed(
            CODE_UNREACHABLE,
            format!(
                "devbridge could not connect to Portos ({e}) - the receipt was not sent, nothing ran"
            ),
        ),
        Reply::Lost(e) => Outcome::Indeterminate(format!(
            "no answer from Portos after the receipt was sent ({e}) - it may be registered; {reconciled}"
        )),
        Reply::Answer { status, body } if (200..300).contains(&status) => {
            match receipt_result(&body, external_id, Via::Register) {
                Some(result) => Outcome::Done(result),
                None => Outcome::Indeterminate(format!(
                    "Portos answered HTTP {status} but not with the receipt of externalId {external_id} - {reconciled}"
                )),
            }
        }
        Reply::Answer { status, body } if is_validation(status, &body) => Outcome::failed(
            CODE_VALIDATION,
            format!(
                "Portos refused the {} receipt as invalid ({}) - nothing was registered",
                kind.receipt_type(),
                problem_text(status, &body)
            ),
        ),
        Reply::Answer { status: 401, body } => Outcome::failed(
            CODE_UNAUTHENTICATED,
            format!(
                "Portos requires authentication ({}) - nothing was registered",
                problem_text(401, &body)
            ),
        ),
        Reply::Answer { status, body } if is_missing_endpoint(status, &body) => Outcome::failed(
            CODE_ENDPOINT_MISSING,
            format!(
                "this Portos has no {} ({}) - it cannot register {} receipts; nothing was registered",
                kind.path(),
                problem_text(status, &body),
                kind.receipt_type()
            ),
        ),
        Reply::Answer { status, body } => Outcome::Indeterminate(format!(
            "Portos answered {} - the receipt may be registered (a print error comes after registration); {reconciled}",
            problem_text(status, &body)
        )),
    }
}

/// A `print_copy` / `open_drawer` / `print_text` answer → the job outcome.
/// These leave no fiscal record; an unknown outcome is reconciled to `failed`
/// `outcome_unknown_retry_safe` (see [`retry_safe_text`]).
pub fn control_outcome(reply: Reply, op: FiscalOp) -> Outcome {
    let (what, flag) = match op {
        FiscalOp::OpenDrawer => ("open the drawer", "opened"),
        FiscalOp::PrintText => ("print the text", "printed"),
        FiscalOp::PrintCopy | FiscalOp::Register(_) => ("print the receipt copy", "printed"),
    };
    match reply {
        // Odoo reads a `done` X as "printed": a text Portos did not print
        // ends here, with Portos' own words (nothing is ever re-sent).
        Reply::Answer { status, body }
            if op == FiscalOp::PrintText
                && (200..300).contains(&status)
                && body.get(flag) != Some(&Value::Bool(true)) =>
        {
            Outcome::failed(
                CODE_RETRY_SAFE,
                format!(
                    "Portos accepted the text but did not report it printed (HTTP {status}; 202 = the printer was not ready, some models print later); no fiscal record, the till may simply retry"
                ),
            )
        }
        Reply::Answer { status, body } if op == FiscalOp::PrintText && status >= 500 => {
            Outcome::failed(
                CODE_RETRY_SAFE,
                format!(
                    "Portos answered {} when asked to print the text; no fiscal record, the till may simply retry",
                    problem_text(status, &body)
                ),
            )
        }
        Reply::Unreachable(e) => Outcome::failed(
            CODE_UNREACHABLE,
            format!("devbridge could not connect to Portos ({e}) - nothing ran"),
        ),
        Reply::Lost(e) => Outcome::Indeterminate(format!(
            "no answer from Portos after asking it to {what} ({e})"
        )),
        Reply::Answer { status, body } if (200..300).contains(&status) => {
            let mut result = serde_json::Map::new();
            result.insert("device".into(), json!(DEVICE));
            result.insert(flag.into(), body.get(flag).cloned().unwrap_or(Value::Null));
            result.insert("printer_ready".into(), json!(status == 200));
            Outcome::Done(Value::Object(result))
        }
        Reply::Answer { status, body } if problem_code(&body) == Some(PORTOS_RECEIPT_NOT_FOUND) => {
            Outcome::failed(
                CODE_RECEIPT_NOT_FOUND,
                format!(
                    "Portos has no such receipt ({}) - nothing was printed",
                    problem_text(status, &body)
                ),
            )
        }
        Reply::Answer { status, body } if is_validation(status, &body) => Outcome::failed(
            CODE_VALIDATION,
            format!(
                "Portos refused the request as invalid ({}) - nothing ran",
                problem_text(status, &body)
            ),
        ),
        Reply::Answer { status: 401, body } => Outcome::failed(
            CODE_UNAUTHENTICATED,
            format!(
                "Portos requires authentication ({}) - nothing ran",
                problem_text(401, &body)
            ),
        ),
        Reply::Answer { status, body } if is_missing_endpoint(status, &body) => Outcome::failed(
            CODE_ENDPOINT_MISSING,
            format!(
                "this Portos cannot {what} ({}) - nothing ran",
                problem_text(status, &body)
            ),
        ),
        Reply::Answer { status, body } if (400..500).contains(&status) => Outcome::failed(
            CODE_REFUSED,
            format!(
                "Portos refused to {what} ({}) - nothing ran",
                problem_text(status, &body)
            ),
        ),
        Reply::Answer { status, body } => Outcome::Indeterminate(format!(
            "Portos answered {} when asked to {what}",
            problem_text(status, &body)
        )),
    }
}

/// The ONE exception to "failed only when nothing ran" (design (e)): a
/// drawer / copy whose answer was lost may or may not have happened, and
/// repeating it is harmless (no fiscal record) — so it ends `failed` with a
/// code that tells the till it may simply retry.
pub fn retry_safe_text(op: FiscalOp) -> String {
    let what = match op {
        FiscalOp::OpenDrawer => "the drawer opened",
        FiscalOp::PrintText => "the text was printed",
        FiscalOp::PrintCopy | FiscalOp::Register(_) => "the receipt copy was printed",
    };
    format!(
        "devbridge cannot tell whether {what} (Portos never answered); it leaves no fiscal record, so the till may simply retry"
    )
}

/// The unprocessed-requests list's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Unprocessed {
    /// Portos lists a request with this externalId as not yet processed.
    Listed,
    NotListed,
    Failed(String),
}

/// `GET /api/v1/requests/unprocessed?ExternalId=`: 204 = empty; 200 = a
/// list, of which only entries with THIS externalId count.
pub fn classify_unprocessed(reply: Reply, external_id: &str) -> Unprocessed {
    match reply {
        Reply::Answer { status: 204, .. } => Unprocessed::NotListed,
        Reply::Answer {
            status: 200,
            body: Value::Array(list),
        } => {
            let ours = list.iter().any(|r| {
                r.pointer("/request/externalId").and_then(Value::as_str) == Some(external_id)
            });
            if ours {
                Unprocessed::Listed
            } else {
                Unprocessed::NotListed
            }
        }
        Reply::Answer { status, body } => Unprocessed::Failed(problem_text(status, &body)),
        Reply::Unreachable(e) | Reply::Lost(e) => Unprocessed::Failed(e),
    }
}

/// Why a receipt devbridge DID send stays unknown after Portos has no
/// receipt with its externalId (the job stays `indeterminate`).
pub fn sent_but_not_found_text(unprocessed: Unprocessed, external_id: &str) -> String {
    match unprocessed {
        Unprocessed::Listed => format!(
            "Portos lists the request with externalId {external_id} as not processed yet - waiting for Portos"
        ),
        Unprocessed::NotListed => format!(
            "devbridge sent the receipt but Portos has neither a receipt nor an unprocessed request with externalId \
             {external_id}; Portos' documentation does not prove that a request whose answer was lost was never \
             stored (it may still be inside Portos, or its index table may be incomplete) - a person must check \
             Portos WebAdmin for externalId {external_id}; never register this sale again under another id"
        ),
        Unprocessed::Failed(e) => format!(
            "Portos has no receipt with externalId {external_id}; its unprocessed list could not be read ({e}) - asked again later"
        ),
    }
}

/// `receipt 42 (online)` for the log of a registered receipt, else `""`.
pub fn receipt_summary(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done(result) => match result.get("receipt_number").filter(|n| n.is_number()) {
            Some(number) => format!(
                "receipt {number} ({})",
                result.get("state").and_then(Value::as_str).unwrap_or("?")
            ),
            None => String::new(),
        },
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod tests;
