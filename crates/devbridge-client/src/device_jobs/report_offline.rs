//! `POST /pokladna/bridge/report_offline` (#101) — the bridge reports a job
//! it ran from the LAN API (Odoo did not create it) once Odoo answers.
//!
//! This is the contract PROPOSED to odoo-erp in the #101 design (the Odoo
//! side does not exist yet). Its whole shape lives in this file, so a change
//! of the contract is a change here:
//!
//! ```json
//! {"client_uuid": "…", "kasa_id": 3, "job_type": "fiscal.cash_register",
//!  "pos_order_id": 812, "payload": {…}, "state": "done", "result": {…},
//!  "error": null, "executed_at": "2026-10-15T10:00:00Z", "origin": "lan"}
//! ```
//!
//! - `state` is `done` or `failed`; `error` is `{code, text}` (`null` when
//!   done), `result` the device's answer (`null` when failed);
//! - reply `{"job_id": 123, "created": true|false}` (the #101 design) or,
//!   in the bridge routes' convention, the same with `"ok": true` — both
//!   are accepted; without `ok` a positive `job_id` is required.
//!   Idempotent by (`kasa_id`, `client_uuid`): the same report returns the
//!   same job id with `created: false`;
//! - a `client_uuid` naming another operation is refused
//!   `client_uuid_conflict` (`{"ok": false, "error": {…}}`).

use serde_json::{Value, json};

use super::api::{result_object, sanitize_error_code, truncate_error};
use super::journal::{JobState, LanJob};
use crate::odoo_rpc::{RpcError, require_ok, value_shape};

pub const REPORT_OFFLINE_PATH: &str = "/pokladna/bridge/report_offline";
/// `origin` of a job the till sent to the LAN API.
pub const ORIGIN_LAN: &str = "lan";

/// An accepted report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportReply {
    /// Odoo's job id (`None` when the reply named no positive id).
    pub job_id: Option<i64>,
    /// Odoo created the job now (`false`: it already had it).
    pub created: bool,
}

/// `2026-10-15T10:00:00Z` (UTC, whole seconds) of a journal timestamp
/// (RFC 3339); the text unchanged when it does not parse.
pub fn executed_at(journal_ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(journal_ts).map_or_else(
        |_| journal_ts.to_string(),
        |t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        },
    )
}

/// The `report_offline` params of a FINAL LAN job (`None` for any other).
/// `executed_at` is the time its outcome was journaled.
pub fn report_params(job: &LanJob) -> Option<Value> {
    let e = &job.entry;
    let (state, result, error) = match e.state {
        JobState::Done => (
            "done",
            result_object(e.result.as_ref().unwrap_or(&Value::Null)),
            Value::Null,
        ),
        JobState::Failed => (
            "failed",
            Value::Null,
            json!({
                "code": sanitize_error_code(e.error_code.as_deref().unwrap_or_default()),
                "text": truncate_error(e.error_text.as_deref().unwrap_or_default()),
            }),
        ),
        _ => return None,
    };
    Some(json!({
        "client_uuid": e.job.client_uuid,
        "kasa_id": e.job.kasa_id,
        "job_type": e.job.job_type,
        "pos_order_id": e.job.pos_order.get("id").and_then(Value::as_i64),
        "payload": e.job.payload,
        "state": state,
        "result": result,
        "error": error,
        "executed_at": executed_at(&e.updated_at),
        "origin": ORIGIN_LAN,
    }))
}

fn positive_job_id(result: &Value) -> Option<i64> {
    result
        .get("job_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
}

/// Parse an accepted report (after the envelope): `{"ok": …}` goes through
/// the bridge routes' rule (`false` = a refusal); without `ok` (the #101
/// design's shape) a positive `job_id` is what says "accepted".
pub fn parse_report_reply(result: Value) -> Result<ReportReply, RpcError> {
    let result = if result.get("ok").is_some() {
        require_ok(result)?
    } else if positive_job_id(&result).is_some() {
        result
    } else {
        return Err(RpcError::Protocol(format!(
            "report_offline result without \"ok\" and without a job_id: {}",
            value_shape(&result)
        )));
    };
    Ok(ReportReply {
        job_id: positive_job_id(&result),
        created: result.get("created").and_then(Value::as_bool) == Some(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_jobs::executor::Outcome;
    use crate::device_jobs::journal::{Journal, LanInserted};
    use crate::device_jobs::lan::LanRequest;

    fn journaled(outcome: Option<Outcome>, pos_order_id: Option<i64>) -> LanJob {
        let j = Journal::open_in_memory().unwrap();
        let job = LanRequest {
            kasa_id: 3,
            client_uuid: "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a".into(),
            job_type: "fiscal.cash_register".into(),
            payload: json!({"amount": 12.4, "currency": "EUR"}),
            pos_order_id,
        }
        .to_job();
        let LanInserted::New(id) = j.insert_lan_received(&job).unwrap() else {
            panic!("new")
        };
        if let Some(o) = outcome {
            j.record_outcome(id, &o).unwrap();
        }
        j.lan_job_by_id(id).unwrap().unwrap()
    }

    #[test]
    fn test_executed_at_is_utc_whole_seconds() {
        assert_eq!(
            executed_at("2026-10-15T12:00:00.123456+02:00"),
            "2026-10-15T10:00:00Z"
        );
        assert_eq!(
            executed_at("2026-10-15T10:00:00+00:00"),
            "2026-10-15T10:00:00Z"
        );
        assert_eq!(executed_at("garbage"), "garbage");
    }

    #[test]
    fn test_report_params_of_a_done_job() {
        let job = journaled(Some(Outcome::Done(json!({"uid": "U-1"}))), Some(812));
        let p = report_params(&job).unwrap();
        assert_eq!(p["client_uuid"], "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a");
        assert_eq!(p["kasa_id"], 3);
        assert_eq!(p["job_type"], "fiscal.cash_register");
        assert_eq!(p["pos_order_id"], 812);
        assert_eq!(p["payload"], json!({"amount": 12.4, "currency": "EUR"}));
        assert_eq!(p["state"], "done");
        assert_eq!(p["result"], json!({"uid": "U-1"}));
        assert_eq!(p["error"], Value::Null);
        assert_eq!(p["executed_at"], executed_at(&job.entry.updated_at));
        assert!(p["executed_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(p["origin"], "lan");
        assert_eq!(p.as_object().unwrap().len(), 10, "{p}");
        // a non-object device answer is wrapped like an ack's
        let scalar = journaled(Some(Outcome::Done(json!(5))), None);
        let p = report_params(&scalar).unwrap();
        assert_eq!(p["result"], json!({"value": 5}));
        assert_eq!(p["pos_order_id"], Value::Null, "an offline sale has none");
    }

    #[test]
    fn test_report_params_of_a_failed_job() {
        let job = journaled(
            Some(Outcome::failed(
                "Portos_Unreachable",
                "nothing ran\u{0}here",
            )),
            None,
        );
        let p = report_params(&job).unwrap();
        assert_eq!(p["state"], "failed");
        assert_eq!(p["result"], Value::Null);
        assert_eq!(
            p["error"],
            json!({"code": "portos_unreachable", "text": "nothing ranhere"})
        );
    }

    #[test]
    fn test_only_final_jobs_are_reported() {
        assert!(report_params(&journaled(None, None)).is_none());
        assert!(
            report_params(&journaled(Some(Outcome::Indeterminate("t/o".into())), None)).is_none()
        );
    }

    #[test]
    fn test_parse_report_reply() {
        assert_eq!(
            parse_report_reply(json!({"ok": true, "job_id": 123, "created": true})).unwrap(),
            ReportReply {
                job_id: Some(123),
                created: true
            }
        );
        assert_eq!(
            parse_report_reply(json!({"ok": true, "job_id": 7, "created": false})).unwrap(),
            ReportReply {
                job_id: Some(7),
                created: false
            }
        );
        assert_eq!(
            parse_report_reply(json!({"ok": true, "job_id": 0})).unwrap(),
            ReportReply {
                job_id: None,
                created: false
            }
        );
        assert_eq!(
            parse_report_reply(json!({"ok": true, "job_id": "123", "created": "yes"})).unwrap(),
            ReportReply {
                job_id: None,
                created: false
            }
        );
        let refused = parse_report_reply(json!({"ok": false, "error":
            {"code": "client_uuid_conflict", "message": "other operation"}}))
        .unwrap_err();
        assert_eq!(refused.refusal_code(), Some("client_uuid_conflict"));
        assert!(!refused.is_retryable());
        // the #101 design's shape: no "ok", the job_id says accepted
        assert_eq!(
            parse_report_reply(json!({"job_id": 1, "created": true})).unwrap(),
            ReportReply {
                job_id: Some(1),
                created: true
            }
        );
        for shape in [
            json!({}),
            json!({"created": true}),
            json!({"job_id": 0}),
            json!({"job_id": "1"}),
        ] {
            assert!(
                matches!(
                    parse_report_reply(shape.clone()),
                    Err(RpcError::Protocol(_))
                ),
                "{shape}"
            );
        }
        assert!(
            matches!(
                parse_report_reply(json!({"ok": "yes", "job_id": 1})),
                Err(RpcError::Protocol(_))
            ),
            "an ok that is not a boolean"
        );
        assert_eq!(REPORT_OFFLINE_PATH, "/pokladna/bridge/report_offline");
    }
}
