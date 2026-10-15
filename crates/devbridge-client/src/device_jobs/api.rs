//! The bridge side of Odoo's device-job contract (#99, odoo-erp
//! `addons/company_pokladna_api/docs/device_job_api.md`): `POST
//! /pokladna/bridge/next | ack | pending | heartbeat` over the shared
//! [`OdooTransport`]. Success is `result = {"ok": true, …}`, a business
//! refusal `{"ok": false, "error": {"code", "message"}}` →
//! [`RpcError::Refused`].

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::odoo_rpc::{OdooTransport, RpcError, require_ok};

pub const NEXT_PATH: &str = "/pokladna/bridge/next";
pub const ACK_PATH: &str = "/pokladna/bridge/ack";
pub const PENDING_PATH: &str = "/pokladna/bridge/pending";
pub const HEARTBEAT_PATH: &str = "/pokladna/bridge/heartbeat";

/// Contract: `error` is cut to 2000 characters by Odoo — never send more.
pub const MAX_ACK_ERROR_CHARS: usize = 2000;
/// Contract: `error_code` matches `[a-z0-9_.-]{1,64}`.
pub const MAX_ERROR_CODE_CHARS: usize = 64;

/// The `error_code` of an ack that tells Odoo the outcome is unknown yet.
pub const CODE_INDETERMINATE: &str = "indeterminate";
/// The `error_code` of an ack for a claimed job the bridge never received.
pub const CODE_NOT_RECEIVED: &str = "not_received";
/// Refusal: `not_received` inside the `next` response window — ask later.
pub const REFUSED_TOO_EARLY: &str = "too_early";
/// Refusal: `not_received` after this bridge's own `indeterminate` ack.
pub const REFUSED_AFTER_INDETERMINATE: &str = "after_indeterminate";

/// Text of a `not_received` ack.
pub const NOT_RECEIVED_TEXT: &str =
    "devbridge never received this job (the next response was lost) - nothing ran";

/// A claimed device job as `/pokladna/bridge/next` hands it out.
///
/// `Debug` is hand-written: it names the payload keys and the money summary
/// only, never the payload values (customer / card data stays out of logs).
#[derive(Clone, PartialEq)]
pub struct DeviceJob {
    pub id: i64,
    /// `fiscal.cash_register`, `card.purchase`, … (opaque to the channel).
    pub job_type: String,
    pub client_uuid: String,
    /// The till (`pos.config`) the job belongs to.
    pub kasa_id: Option<i64>,
    pub kasa_name: String,
    /// `{"id", "name", "pos_reference"}` or `null` for a job without a sale.
    pub pos_order: Value,
    /// Opaque per-type payload, handed to the executor unchanged.
    pub payload: Value,
    /// Odoo timestamps (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub created_at: String,
    pub claimed_at: String,
}

impl std::fmt::Debug for DeviceJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self
            .payload
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        f.debug_struct("DeviceJob")
            .field("id", &self.id)
            .field("job_type", &self.job_type)
            .field("client_uuid", &self.client_uuid)
            .field("kasa_id", &self.kasa_id)
            .field("pos_order", &pos_order_name(&self.pos_order))
            .field("payload_keys", &keys)
            .field("money", &money_summary(&self.payload))
            .finish()
    }
}

impl DeviceJob {
    /// The sale's name (`Bastion/0042`) or `""`.
    pub fn pos_order_name(&self) -> String {
        pos_order_name(&self.pos_order)
    }

    /// `amount` + `currency` of the payload, the ONLY payload values ever
    /// logged (`-` when the job carries no amount).
    pub fn money(&self) -> String {
        money_summary(&self.payload)
    }
}

fn pos_order_name(pos_order: &Value) -> String {
    pos_order
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `"12.4 EUR"`, `"12.4"` (no currency) or `"-"` (no numeric amount).
pub fn money_summary(payload: &Value) -> String {
    let Some(amount) = payload.get("amount").filter(|a| a.is_number()) else {
        return "-".to_string();
    };
    match payload.get("currency").and_then(Value::as_str) {
        Some(cur) if !cur.is_empty() => {
            let cur: String = cur.chars().take(8).collect();
            format!("{amount} {cur}")
        }
        _ => amount.to_string(),
    }
}

/// A till this bridge serves (`kasy` of `next` / `heartbeat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kasa {
    pub id: i64,
    pub name: String,
}

/// `/pokladna/bridge/next` result.
#[derive(Debug, Clone, PartialEq)]
pub struct NextReply {
    /// The claimed job, or `None` when nothing arrived within the wait.
    pub job: Option<DeviceJob>,
    /// How long the call was allowed to wait; `0` = no waiting slot was free
    /// (or pure short polling) → sleep 1–2 s before the next call.
    pub wait_s: u64,
    pub kasy: Vec<Kasa>,
}

/// One `/pokladna/bridge/pending` entry — for reconciliation only, it carries
/// no payload and is never run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingJob {
    pub id: i64,
    pub job_type: String,
    /// `claimed` or `unknown`.
    pub state: String,
    pub client_uuid: String,
    pub claimed_at: String,
}

/// An accepted ack: `duplicate` = the job already had a final ack (the first
/// one wins — a retry is safe), `state` = Odoo's job state after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckReply {
    pub duplicate: bool,
    pub state: String,
}

/// What one `/pokladna/bridge/ack` call reports.
#[derive(Debug, Clone, PartialEq)]
pub enum AckRequest {
    /// The device ran the job; `result` is its answer.
    Done { job_id: i64, result: Value },
    /// The device confirmed nothing ran (or the bridge refused the job
    /// before any device contact).
    Failed {
        job_id: i64,
        code: String,
        text: String,
    },
    /// The bridge cannot tell whether the device ran it (yet).
    Indeterminate { job_id: i64, reason: String },
    /// A pending job the journal has no entry for — the `next` response was
    /// lost before it reached the bridge, nothing ran.
    NotReceived { job_id: i64 },
}

impl AckRequest {
    pub fn job_id(&self) -> i64 {
        match self {
            Self::Done { job_id, .. }
            | Self::Failed { job_id, .. }
            | Self::Indeterminate { job_id, .. }
            | Self::NotReceived { job_id } => *job_id,
        }
    }

    /// Short name for logs: `done` / `failed` / `indeterminate` / `not_received`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
            Self::Indeterminate { .. } => CODE_INDETERMINATE,
            Self::NotReceived { .. } => CODE_NOT_RECEIVED,
        }
    }

    /// The ack params per the contract.
    pub fn params(&self) -> Value {
        match self {
            Self::Done { job_id, result } => {
                json!({"job_id": job_id, "result": result_object(result)})
            }
            Self::Failed { job_id, code, text } => json!({
                "job_id": job_id,
                "error": truncate_error(text),
                "error_code": sanitize_error_code(code),
            }),
            Self::Indeterminate { job_id, reason } => json!({
                "job_id": job_id,
                "error": truncate_error(reason),
                "error_code": CODE_INDETERMINATE,
            }),
            Self::NotReceived { job_id } => json!({
                "job_id": job_id,
                "error": NOT_RECEIVED_TEXT,
                "error_code": CODE_NOT_RECEIVED,
            }),
        }
    }
}

/// The contract wants `result` as a JSON object; anything else is wrapped.
fn result_object(result: &Value) -> Value {
    if result.is_object() {
        result.clone()
    } else {
        json!({"value": result})
    }
}

/// Cut an ack `error` to the contract's 2000 characters (char-boundary safe).
pub fn truncate_error(text: &str) -> String {
    text.chars().take(MAX_ACK_ERROR_CHARS).collect()
}

/// Force an executor's code into `[a-z0-9_.-]{1,64}` (lower-cased, any other
/// character → `_`); empty → `bridge_error` (Odoo's own default).
pub fn sanitize_error_code(code: &str) -> String {
    let cleaned: String = code
        .trim()
        .chars()
        .take(MAX_ERROR_CODE_CHARS)
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "bridge_error".to_string()
    } else {
        cleaned
    }
}

/// A positive integer id (never a string / bool / float).
fn positive_id(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|id| *id > 0)
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse one job object of `next`. Only a missing / invalid `id` is fatal —
/// a job with an id but odd fields is still journaled and answered (an
/// unknown type is refused as `unsupported`, nothing runs).
pub fn parse_job(v: &Value) -> Result<DeviceJob, RpcError> {
    let obj = v
        .as_object()
        .ok_or_else(|| RpcError::Protocol(format!("next: job is not an object: {v}")))?;
    let id = positive_id(obj.get("id"))
        .ok_or_else(|| RpcError::Protocol(format!("next: job without a positive id: {v}")))?;
    let kasa = obj.get("kasa").filter(|k| k.is_object());
    Ok(DeviceJob {
        id,
        job_type: text(obj.get("type")),
        client_uuid: text(obj.get("client_uuid")),
        kasa_id: positive_id(kasa.and_then(|k| k.get("id"))),
        kasa_name: text(kasa.and_then(|k| k.get("name"))),
        pos_order: obj
            .get("pos_order")
            .filter(|p| p.is_object())
            .cloned()
            .unwrap_or(Value::Null),
        payload: obj
            .get("payload")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
        created_at: text(obj.get("created_at")),
        claimed_at: text(obj.get("claimed_at")),
    })
}

fn parse_kasy(v: Option<&Value>) -> Vec<Kasa> {
    v.and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|k| {
                    positive_id(k.get("id")).map(|id| Kasa {
                        id,
                        name: text(k.get("name")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `next` result (after the envelope).
pub fn parse_next(result: Value) -> Result<NextReply, RpcError> {
    let result = require_ok(result)?;
    let job = match result.get("job") {
        None | Some(Value::Null) => None,
        Some(j) => Some(parse_job(j)?),
    };
    Ok(NextReply {
        job,
        wait_s: result.get("wait_s").and_then(Value::as_u64).unwrap_or(0),
        kasy: parse_kasy(result.get("kasy")),
    })
}

/// Parse the `pending` result. An entry without a positive id cannot be
/// answered and is skipped (returned in the second list for the log).
pub fn parse_pending(result: Value) -> Result<(Vec<PendingJob>, Vec<Value>), RpcError> {
    let result = require_ok(result)?;
    let list = result
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::Protocol(format!("pending: no \"jobs\" list: {result}")))?;
    let mut jobs = Vec::with_capacity(list.len());
    let mut skipped = Vec::new();
    for j in list {
        match positive_id(j.get("id")) {
            Some(id) => jobs.push(PendingJob {
                id,
                job_type: text(j.get("type")),
                state: text(j.get("state")),
                client_uuid: text(j.get("client_uuid")),
                claimed_at: text(j.get("claimed_at")),
            }),
            None => skipped.push(j.clone()),
        }
    }
    Ok((jobs, skipped))
}

/// Parse an accepted ack.
pub fn parse_ack_reply(result: Value) -> Result<AckReply, RpcError> {
    let result = require_ok(result)?;
    Ok(AckReply {
        duplicate: result.get("duplicate").and_then(Value::as_bool) == Some(true),
        state: text(result.get("job").and_then(|j| j.get("state"))),
    })
}

/// HTTP timeout of a `next` call: the asked `wait_s` plus the margin (the
/// contract wants exactly `wait_s + 10` s — never shorter, or a job claimed
/// at the end of the wait loses its response).
pub fn next_timeout(wait_s: u64, margin: Duration) -> Duration {
    Duration::from_secs(wait_s) + margin
}

/// Async client for the four bridge routes.
#[derive(Debug)]
pub struct BridgeApi {
    transport: OdooTransport,
}

impl BridgeApi {
    /// `call_timeout` bounds `ack` / `pending` / `heartbeat`; `next` uses
    /// [`next_timeout`].
    pub fn new(base_url: &str, api_key: &str, call_timeout: Duration) -> Result<Self, RpcError> {
        Ok(Self {
            transport: OdooTransport::new(base_url, api_key, call_timeout)?,
        })
    }

    /// Claim the oldest queued job of this bridge's tills (long-poll).
    pub async fn next(&self, wait_s: u64, timeout: Duration) -> Result<NextReply, RpcError> {
        let result = self
            .transport
            .call_with_timeout(NEXT_PATH, json!({"wait_s": wait_s}), timeout)
            .await?;
        parse_next(result)
    }

    /// Jobs this bridge claimed without a final ack.
    pub async fn pending(&self) -> Result<(Vec<PendingJob>, Vec<Value>), RpcError> {
        parse_pending(self.transport.call(PENDING_PATH, json!({})).await?)
    }

    pub async fn ack(&self, ack: &AckRequest) -> Result<AckReply, RpcError> {
        parse_ack_reply(self.transport.call(ACK_PATH, ack.params()).await?)
    }

    /// Returns the tills Odoo links to this bridge.
    pub async fn heartbeat(
        &self,
        bridge_version: &str,
        device_status: &str,
    ) -> Result<Vec<Kasa>, RpcError> {
        let result = require_ok(
            self.transport
                .call(
                    HEARTBEAT_PATH,
                    json!({"bridge_version": bridge_version, "device_status": device_status}),
                )
                .await?,
        )?;
        Ok(parse_kasy(result.get("kasy")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract_job() -> Value {
        json!({
            "id": 4711,
            "type": "card.purchase",
            "client_uuid": "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a",
            "kasa": {"id": 3, "name": "Pokladňa Bastion 1"},
            "pos_order": {"id": 812, "name": "Bastion/0042", "pos_reference": "Order 00003-001-0042"},
            "payload": {"amount": 12.40, "currency": "EUR", "card_last4": "4242"},
            "created_at": "2026-10-14 09:12:05",
            "claimed_at": "2026-10-14 09:12:06"
        })
    }

    #[test]
    fn test_parse_next_contract_example() {
        let r = parse_next(json!({"ok": true, "job": contract_job(), "wait_s": 25,
            "kasy": [{"id": 3, "name": "Pokladňa Bastion 1"}, {"id": "x"}]}))
        .unwrap();
        assert_eq!(r.wait_s, 25);
        assert_eq!(
            r.kasy,
            vec![Kasa {
                id: 3,
                name: "Pokladňa Bastion 1".into()
            }]
        );
        let job = r.job.unwrap();
        assert_eq!(job.id, 4711);
        assert_eq!(job.job_type, "card.purchase");
        assert_eq!(job.client_uuid, "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a");
        assert_eq!(job.kasa_id, Some(3));
        assert_eq!(job.kasa_name, "Pokladňa Bastion 1");
        assert_eq!(job.pos_order_name(), "Bastion/0042");
        assert_eq!(job.payload["amount"], json!(12.40));
        assert_eq!(job.created_at, "2026-10-14 09:12:05");
        assert_eq!(job.claimed_at, "2026-10-14 09:12:06");
        assert_eq!(job.money(), "12.4 EUR");
    }

    #[test]
    fn test_parse_next_empty_and_wait_zero() {
        let r = parse_next(json!({"ok": true, "job": null, "wait_s": 0, "kasy": []})).unwrap();
        assert_eq!(r.job, None);
        assert_eq!(r.wait_s, 0);
        // absent wait_s is read as 0 (= sleep before asking again, never spin)
        let r = parse_next(json!({"ok": true})).unwrap();
        assert_eq!((r.job, r.wait_s, r.kasy.len()), (None, 0, 0));
    }

    #[test]
    fn test_parse_next_refusal_and_bad_jobs() {
        assert_eq!(
            parse_next(json!({"ok": false, "error": {"code": "bad_request", "message": "wait_s"}}))
                .unwrap_err()
                .refusal_code(),
            Some("bad_request")
        );
        for bad in [
            json!({"ok": true, "job": {"type": "card.purchase"}}),
            json!({"ok": true, "job": {"id": "4711"}}),
            json!({"ok": true, "job": {"id": 0}}),
            json!({"ok": true, "job": {"id": -3}}),
            json!({"ok": true, "job": [4711]}),
            json!({"ok": true, "job": 4711}),
        ] {
            assert!(
                matches!(parse_next(bad.clone()), Err(RpcError::Protocol(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_parse_job_minimal_fields_default() {
        let job = parse_job(&json!({"id": 5, "kasa": false, "pos_order": false})).unwrap();
        assert_eq!(job.id, 5);
        assert_eq!(job.job_type, "");
        assert_eq!(job.kasa_id, None);
        assert_eq!(job.kasa_name, "");
        assert_eq!(job.pos_order, Value::Null);
        assert_eq!(job.payload, json!({}));
        assert_eq!(job.pos_order_name(), "");
        assert_eq!(job.money(), "-");
    }

    #[test]
    fn test_device_job_debug_shows_keys_and_money_never_other_payload_values() {
        let job = parse_job(&contract_job()).unwrap();
        let dbg = format!("{job:?}");
        assert!(!dbg.contains("4242"), "{dbg}");
        assert!(dbg.contains("card_last4"), "{dbg}");
        assert!(dbg.contains("12.4 EUR"), "{dbg}");
        assert!(dbg.contains("Bastion/0042"), "{dbg}");
        assert!(dbg.contains("4711"), "{dbg}");
    }

    #[test]
    fn test_money_summary_forms() {
        assert_eq!(
            money_summary(&json!({"amount": 5, "currency": "EUR"})),
            "5 EUR"
        );
        assert_eq!(money_summary(&json!({"amount": 0.5})), "0.5");
        assert_eq!(money_summary(&json!({"amount": 1, "currency": ""})), "1");
        assert_eq!(
            money_summary(&json!({"amount": "12.40", "currency": "EUR"})),
            "-"
        );
        assert_eq!(money_summary(&json!({"currency": "EUR"})), "-");
        assert_eq!(money_summary(&json!([1])), "-");
        assert_eq!(
            money_summary(&json!({"amount": 2, "currency": "ABCDEFGHIJ"})),
            "2 ABCDEFGH"
        );
    }

    #[test]
    fn test_parse_pending_contract_example_skips_unidentifiable() {
        let (jobs, skipped) = parse_pending(json!({"ok": true, "jobs": [
            {"id": 4711, "type": "card.purchase", "state": "unknown", "client_uuid": "6f1c",
             "kasa": {"id": 3, "name": "Pokladňa Bastion 1"}, "pos_order": null,
             "created_at": "2026-10-14 09:12:05", "claimed_at": "2026-10-14 09:12:06"},
            {"type": "fiscal.cash_register"}
        ]}))
        .unwrap();
        assert_eq!(
            jobs,
            vec![PendingJob {
                id: 4711,
                job_type: "card.purchase".into(),
                state: "unknown".into(),
                client_uuid: "6f1c".into(),
                claimed_at: "2026-10-14 09:12:06".into(),
            }]
        );
        assert_eq!(skipped, vec![json!({"type": "fiscal.cash_register"})]);
        assert!(matches!(
            parse_pending(json!({"ok": true})),
            Err(RpcError::Protocol(_))
        ));
        assert!(
            parse_pending(json!({"ok": true, "jobs": []}))
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn test_parse_ack_reply() {
        assert_eq!(
            parse_ack_reply(json!({"ok": true, "job": {"id": 4711, "state": "done"}})).unwrap(),
            AckReply {
                duplicate: false,
                state: "done".into()
            }
        );
        assert_eq!(
            parse_ack_reply(
                json!({"ok": true, "duplicate": true, "job": {"id": 4711, "state": "done"}})
            )
            .unwrap(),
            AckReply {
                duplicate: true,
                state: "done".into()
            }
        );
        let e =
            parse_ack_reply(json!({"ok": false, "error": {"code": "too_early", "message": "m"}}))
                .unwrap_err();
        assert_eq!(e.refusal_code(), Some(REFUSED_TOO_EARLY));
    }

    #[test]
    fn test_ack_params_per_contract() {
        assert_eq!(
            AckRequest::Done {
                job_id: 1,
                result: json!({"uid": "O-1"})
            }
            .params(),
            json!({"job_id": 1, "result": {"uid": "O-1"}})
        );
        assert_eq!(
            AckRequest::Done {
                job_id: 1,
                result: json!("plain")
            }
            .params(),
            json!({"job_id": 1, "result": {"value": "plain"}})
        );
        assert_eq!(
            AckRequest::Failed {
                job_id: 2,
                code: "Declined Card!".into(),
                text: "zamietnutá".into()
            }
            .params(),
            json!({"job_id": 2, "error": "zamietnutá", "error_code": "declined_card_"})
        );
        assert_eq!(
            AckRequest::Indeterminate {
                job_id: 3,
                reason: "timeout".into()
            }
            .params(),
            json!({"job_id": 3, "error": "timeout", "error_code": "indeterminate"})
        );
        assert_eq!(
            AckRequest::NotReceived { job_id: 4 }.params(),
            json!({"job_id": 4, "error": NOT_RECEIVED_TEXT, "error_code": "not_received"})
        );
        let long = "č".repeat(2500);
        let p = AckRequest::Failed {
            job_id: 5,
            code: "x".into(),
            text: long,
        }
        .params();
        assert_eq!(p["error"].as_str().unwrap().chars().count(), 2000);
    }

    #[test]
    fn test_ack_request_job_id_and_kind() {
        let all = [
            AckRequest::Done {
                job_id: 1,
                result: json!({}),
            },
            AckRequest::Failed {
                job_id: 2,
                code: "c".into(),
                text: "t".into(),
            },
            AckRequest::Indeterminate {
                job_id: 3,
                reason: "r".into(),
            },
            AckRequest::NotReceived { job_id: 4 },
        ];
        let ids: Vec<i64> = all.iter().map(AckRequest::job_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        let kinds: Vec<&str> = all.iter().map(AckRequest::kind).collect();
        assert_eq!(
            kinds,
            vec!["done", "failed", "indeterminate", "not_received"]
        );
    }

    #[test]
    fn test_sanitize_error_code() {
        assert_eq!(sanitize_error_code("bad_amount"), "bad_amount");
        assert_eq!(sanitize_error_code("portos.E-12"), "portos.e-12");
        assert_eq!(sanitize_error_code("  "), "bridge_error");
        assert_eq!(sanitize_error_code("a b/č"), "a_b__");
        assert_eq!(sanitize_error_code(&"x".repeat(80)).len(), 64);
    }

    #[test]
    fn test_next_timeout_is_wait_plus_margin() {
        let m = Duration::from_secs(10);
        assert_eq!(next_timeout(25, m), Duration::from_secs(35));
        assert_eq!(next_timeout(0, m), Duration::from_secs(10));
        assert_eq!(next_timeout(55, m), Duration::from_secs(65));
    }

    #[test]
    fn test_bridge_api_debug_hides_the_key() {
        let api = BridgeApi::new(
            "https://erp.example.test",
            "bridge-key-123",
            Duration::from_secs(30),
        )
        .unwrap();
        let dbg = format!("{api:?}");
        assert!(!dbg.contains("bridge-key-123"), "{dbg}");
    }
}
