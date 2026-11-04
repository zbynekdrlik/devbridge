//! The LAN API's wire format (#101): request validation and the JSON of a
//! job. Pure functions — the HTTP layer in `mod.rs` only maps them.
//!
//! `POST /v1/jobs`:
//!
//! ```json
//! {"client_uuid": "6f1c2a0e-…", "kasa_id": 3, "job_type": "fiscal.cash_register",
//!  "payload": {…}, "pos_order_id": 812}
//! ```
//!
//! `pos_order_id` is optional (an offline sale has no Odoo id yet); unknown
//! keys are ignored. A job answer:
//!
//! ```json
//! {"client_uuid": "…", "kasa_id": 3, "job_type": "fiscal.cash_register",
//!  "state": "done", "result": {…}, "error": null, "replay": false,
//!  "reported": false, "odoo_job_id": null,
//!  "received_at": "…", "updated_at": "…"}
//! ```
//!
//! `state` is `received` / `executing` (HTTP 202 — poll
//! `GET /v1/jobs/{kasa_id}/{client_uuid}`), `done` / `failed` (200; failed =
//! nothing ran) or `indeterminate` (202 — the outcome is reconciled with
//! Portos, poll again). Errors: `{"error": {"code", "text"}}`.

use serde_json::{Value, json};

use crate::device_jobs::journal::{CardPayment, JobState, JournalEntry, LanJob};
use crate::device_jobs::lan::LanRequest;
use crate::device_jobs::portos::payload::client_uuid_ok;

/// A positive integer (never a string / float / bool).
fn positive_int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|n| *n > 0)
}

/// Parse a `POST /v1/jobs` body. `Err` = HTTP 400 with this text (it names
/// fields, never their values).
pub fn parse_request(body: &[u8]) -> Result<LanRequest, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "the body is not JSON".to_string())?;
    let obj = v
        .as_object()
        .ok_or_else(|| "the body must be a JSON object".to_string())?;
    let client_uuid = obj
        .get("client_uuid")
        .and_then(Value::as_str)
        .filter(|u| client_uuid_ok(u))
        .ok_or_else(|| "client_uuid must be a string of [A-Za-z0-9_-]{1,64}".to_string())?;
    let kasa_id = positive_int(obj.get("kasa_id")).ok_or_else(|| {
        "kasa_id must be a positive integer (the till's pos.config id)".to_string()
    })?;
    let job_type = obj
        .get("job_type")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| "job_type must be a non-empty string".to_string())?;
    let payload = obj
        .get("payload")
        .filter(|p| p.is_object())
        .ok_or_else(|| "payload must be a JSON object".to_string())?;
    let pos_order_id = match obj.get("pos_order_id") {
        None | Some(Value::Null) => None,
        some => Some(
            positive_int(some)
                .ok_or_else(|| "pos_order_id must be a positive integer or null".to_string())?,
        ),
    };
    Ok(LanRequest {
        kasa_id,
        client_uuid: client_uuid.to_string(),
        job_type: job_type.to_string(),
        payload: payload.clone(),
        pos_order_id,
    })
}

/// A till's job id in a path: `GET /v1/jobs/{kasa_id}/{client_uuid}`.
pub fn parse_path(kasa_id: &str, client_uuid: &str) -> Option<i64> {
    let id = kasa_id.parse::<i64>().ok().filter(|n| *n > 0)?;
    client_uuid_ok(client_uuid).then_some(id)
}

/// `{"code", "text"}` of a job that failed or whose outcome is unknown.
fn job_error(job: &LanJob) -> Value {
    let e = &job.entry;
    match e.state {
        JobState::Failed => json!({
            "code": e.error_code.as_deref().unwrap_or("bridge_error"),
            "text": e.error_text.as_deref().unwrap_or_default(),
        }),
        JobState::Indeterminate => json!({
            "code": "indeterminate",
            "text": e.error_text.as_deref().unwrap_or_default(),
        }),
        _ => Value::Null,
    }
}

/// The JSON of a job answer (`replay`: the client_uuid was journaled
/// before this call).
pub fn job_json(job: &LanJob, replay: bool) -> Value {
    let e = &job.entry;
    let result = match e.state {
        JobState::Done => e.result.clone().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    json!({
        "client_uuid": e.job.client_uuid,
        "kasa_id": e.job.kasa_id,
        "job_type": e.job.job_type,
        "state": e.state.as_str(),
        "result": result,
        "error": job_error(job),
        "replay": replay,
        "reported": job.reported,
        "odoo_job_id": job.odoo_job_id,
        "received_at": e.received_at,
        "updated_at": e.updated_at,
    })
}

/// The answer of `GET /v1/card-jobs/{kasa_id}/{client_uuid}` (#102 D3): the
/// Odoo card job as this bridge knows it. While a payment is held after its
/// authorisation, `phase` is `authorized` and `result` the authorisation;
/// once final, `state` + `result` / `error` as the ack Odoo gets.
pub fn card_job_json(entry: &JournalEntry, card: Option<&CardPayment>) -> Value {
    let authorized = card.and_then(|c| c.auth_result.clone());
    let held = entry.state == JobState::Executing && authorized.is_some();
    let result = match entry.state {
        JobState::Done => entry.result.clone().unwrap_or(Value::Null),
        _ if held => authorized.unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let error = match entry.state {
        JobState::Failed => json!({
            "code": entry.error_code.as_deref().unwrap_or("bridge_error"),
            "text": entry.error_text.as_deref().unwrap_or_default(),
        }),
        JobState::Indeterminate => json!({
            "code": "indeterminate",
            "text": entry.error_text.as_deref().unwrap_or_default(),
        }),
        _ => Value::Null,
    };
    json!({
        "job_id": entry.job.id,
        "client_uuid": entry.job.client_uuid,
        "kasa_id": entry.job.kasa_id,
        "job_type": entry.job.job_type,
        "state": entry.state.as_str(),
        "phase": if held { Value::from("authorized") } else { Value::Null },
        "result": result,
        "error": error,
        "received_at": entry.received_at,
        "updated_at": entry.updated_at,
    })
}

/// HTTP status of a job answer: 200 once final, 202 while it runs or its
/// outcome is still unknown (the till polls).
pub fn job_status(state: JobState) -> u16 {
    if state.is_final() { 200 } else { 202 }
}

/// `{"error": {"code", "text"}}`.
pub fn error_json(code: &str, text: &str) -> Value {
    json!({"error": {"code": code, "text": text}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_jobs::executor::Outcome;
    use crate::device_jobs::journal::{Journal, LanInserted};

    const UUID: &str = "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a";

    fn body(v: Value) -> Vec<u8> {
        v.to_string().into_bytes()
    }

    #[test]
    fn test_parse_request_happy_path() {
        let r = parse_request(&body(json!({
            "client_uuid": UUID, "kasa_id": 3, "job_type": "fiscal.cash_register",
            "payload": {"amount": 1}, "pos_order_id": 812, "extra": "ignored"
        })))
        .unwrap();
        assert_eq!(
            r,
            LanRequest {
                kasa_id: 3,
                client_uuid: UUID.into(),
                job_type: "fiscal.cash_register".into(),
                payload: json!({"amount": 1}),
                pos_order_id: Some(812),
            }
        );
        let null = parse_request(&body(json!({
            "client_uuid": "u", "kasa_id": 1, "job_type": "fiscal.open_drawer",
            "payload": {}, "pos_order_id": null
        })))
        .unwrap();
        assert_eq!(null.pos_order_id, None);
        let absent = parse_request(&body(json!({
            "client_uuid": "u", "kasa_id": 1, "job_type": "t", "payload": {}
        })))
        .unwrap();
        assert_eq!(absent.pos_order_id, None);
    }

    #[test]
    fn test_parse_request_names_the_bad_field() {
        let ok = json!({"client_uuid": UUID, "kasa_id": 3, "job_type": "fiscal.x", "payload": {}});
        let with = |key: &str, v: Value| {
            let mut b = ok.clone();
            b[key] = v;
            parse_request(&body(b)).unwrap_err()
        };
        let without = |key: &str| {
            let mut b = ok.clone();
            b.as_object_mut().unwrap().remove(key);
            parse_request(&body(b)).unwrap_err()
        };
        assert_eq!(
            parse_request(b"not json").unwrap_err(),
            "the body is not JSON"
        );
        assert_eq!(
            parse_request(b"[1]").unwrap_err(),
            "the body must be a JSON object"
        );
        for bad in [json!(""), json!("a b"), json!("x".repeat(65)), json!(7)] {
            assert!(with("client_uuid", bad).starts_with("client_uuid"));
        }
        assert!(without("client_uuid").starts_with("client_uuid"));
        for bad in [json!(0), json!(-3), json!("3"), json!(3.5), json!(null)] {
            assert!(with("kasa_id", bad).starts_with("kasa_id"));
        }
        assert!(without("kasa_id").starts_with("kasa_id"));
        for bad in [json!(""), json!("  "), json!(1)] {
            assert!(with("job_type", bad).starts_with("job_type"));
        }
        assert!(without("job_type").starts_with("job_type"));
        for bad in [json!([]), json!("x"), json!(null)] {
            assert!(with("payload", bad).starts_with("payload"));
        }
        assert!(without("payload").starts_with("payload"));
        for bad in [json!(0), json!("812"), json!(-1)] {
            assert!(with("pos_order_id", bad).starts_with("pos_order_id"));
        }
    }

    #[test]
    fn test_parse_path() {
        assert_eq!(parse_path("3", UUID), Some(3));
        assert_eq!(parse_path("0", UUID), None);
        assert_eq!(parse_path("-3", UUID), None);
        assert_eq!(parse_path("x", UUID), None);
        assert_eq!(parse_path("3", "a b"), None);
        assert_eq!(parse_path("3", ""), None);
    }

    fn lan_job(outcome: Option<Outcome>) -> LanJob {
        let j = Journal::open_in_memory().unwrap();
        let req = LanRequest {
            kasa_id: 3,
            client_uuid: UUID.into(),
            job_type: "fiscal.cash_register".into(),
            payload: json!({}),
            pos_order_id: None,
        };
        let LanInserted::New(id) = j.insert_lan_received(&req.to_job()).unwrap() else {
            panic!("new")
        };
        if let Some(o) = outcome {
            j.record_outcome(id, &o).unwrap();
        }
        j.lan_job_by_id(id).unwrap().unwrap()
    }

    /// An Odoo card.purchase journaled and executing, with its outcome.
    fn card_entry(outcome: Option<Outcome>, authorised: bool) -> Value {
        let j = Journal::open_in_memory().unwrap();
        let job = crate::device_jobs::api::parse_job(&json!({
            "id": 9, "type": "card.purchase", "client_uuid": UUID,
            "kasa": {"id": 3}, "payload": {"amount": 2.5}}))
        .unwrap();
        j.insert_received(&job).unwrap();
        assert!(j.start_execution(9).unwrap());
        assert!(j.card_contact(9, "10.78.2.8", 250, Some(1)).unwrap());
        if authorised {
            j.card_authorized(9, 2, &json!({"state": "authorized"}))
                .unwrap();
        }
        if let Some(o) = outcome {
            j.record_outcome(9, &o).unwrap();
        }
        let (entry, card) = j.card_job_by_uuid(3, UUID).unwrap().unwrap();
        card_job_json(&entry, card.as_ref())
    }

    #[test]
    fn test_card_job_json() {
        let held = card_entry(None, true);
        assert_eq!(held["job_id"], 9);
        assert_eq!(held["client_uuid"], UUID);
        assert_eq!(held["kasa_id"], 3);
        assert_eq!(held["state"], "executing");
        assert_eq!(held["phase"], "authorized");
        assert_eq!(held["result"], json!({"state": "authorized"}));
        assert_eq!(held["error"], Value::Null);
        assert!(held["received_at"].is_string());
        // not authorised yet: no phase, no result
        let asked = card_entry(None, false);
        assert_eq!(
            (asked["phase"].clone(), asked["result"].clone()),
            (Value::Null, Value::Null)
        );
        let done = card_entry(Some(Outcome::Done(json!({"state": "committed"}))), true);
        assert_eq!(done["result"], json!({"state": "committed"}));
        assert_eq!(done["phase"], Value::Null, "final: no phase");
        let failed = card_entry(Some(Outcome::failed("rolled_back", "deadline")), true);
        assert_eq!(failed["result"], Value::Null);
        assert_eq!(
            failed["error"],
            json!({"code": "rolled_back", "text": "deadline"})
        );
        let unknown = card_entry(Some(Outcome::Indeterminate("lost".into())), true);
        assert_eq!(unknown["state"], "indeterminate");
        assert_eq!(unknown["phase"], Value::Null);
        assert_eq!(
            unknown["error"],
            json!({"code": "indeterminate", "text": "lost"})
        );
    }

    #[test]
    fn test_job_json_and_status() {
        let done = lan_job(Some(Outcome::Done(json!({"uid": "U"}))));
        let v = job_json(&done, true);
        assert_eq!(v["client_uuid"], UUID);
        assert_eq!(v["kasa_id"], 3);
        assert_eq!(v["job_type"], "fiscal.cash_register");
        assert_eq!(v["state"], "done");
        assert_eq!(v["result"], json!({"uid": "U"}));
        assert_eq!(v["error"], Value::Null);
        assert_eq!(v["replay"], true);
        assert_eq!(v["reported"], false);
        assert_eq!(v["odoo_job_id"], Value::Null);
        assert_eq!(v["received_at"], done.entry.received_at);
        assert_eq!(v["updated_at"], done.entry.updated_at);
        assert_eq!(v.as_object().unwrap().len(), 11);
        assert_eq!(job_status(JobState::Done), 200);

        let failed = lan_job(Some(Outcome::failed("portos_unreachable", "nothing ran")));
        let v = job_json(&failed, false);
        assert_eq!(v["state"], "failed");
        assert_eq!(v["replay"], false);
        assert_eq!(v["result"], Value::Null);
        assert_eq!(
            v["error"],
            json!({"code": "portos_unreachable", "text": "nothing ran"})
        );
        assert_eq!(job_status(JobState::Failed), 200);

        let unknown = lan_job(Some(Outcome::Indeterminate("no answer".into())));
        let v = job_json(&unknown, false);
        assert_eq!(v["state"], "indeterminate");
        assert_eq!(
            v["error"],
            json!({"code": "indeterminate", "text": "no answer"})
        );
        assert_eq!(job_status(JobState::Indeterminate), 202);

        let running = lan_job(None);
        let v = job_json(&running, false);
        assert_eq!(v["state"], "received");
        assert_eq!(v["error"], Value::Null);
        assert_eq!(v["result"], Value::Null);
        assert_eq!(job_status(JobState::Received), 202);
        assert_eq!(job_status(JobState::Executing), 202);

        // a failed row without a code still names one
        let mut no_code = failed.clone();
        no_code.entry.error_code = None;
        no_code.entry.error_text = None;
        assert_eq!(
            job_json(&no_code, false)["error"],
            json!({"code": "bridge_error", "text": ""})
        );
        // a result is only shown for a done job
        let mut odd = running;
        odd.entry.result = Some(json!({"x": 1}));
        assert_eq!(job_json(&odd, false)["result"], Value::Null);
    }

    #[test]
    fn test_error_json() {
        assert_eq!(
            error_json("unauthorized", "no key"),
            json!({"error": {"code": "unauthorized", "text": "no key"}})
        );
    }
}
