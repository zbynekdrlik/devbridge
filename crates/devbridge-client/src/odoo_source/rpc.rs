//! Odoo print API client (#90 contract, stream david4 2026-09-28).
//!
//! All three endpoints are Odoo `type="json"` routes spoken through the
//! shared [`crate::odoo_rpc::OdooTransport`] (JSON-RPC 2.0 envelope, Bearer
//! key, both HTTP-200 error kinds — top-level `error` first, then
//! `result.error` text; #99 extracted it from here so the device-job source
//! uses the same transport). The API key is only ever put into the
//! `Authorization` header.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::odoo_rpc::OdooTransport;
pub use crate::odoo_rpc::{RpcError, envelope, parse_envelope};

/// Longest `error` text Odoo stores on a line (`error_text`, 200 chars).
pub const MAX_ACK_ERROR_CHARS: usize = 200;

/// Whole-request timeout of every label call.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Cut an ack error text to Odoo's 200-character field (char boundary safe).
pub fn truncate_ack_error(text: &str) -> String {
    text.chars().take(MAX_ACK_ERROR_CHARS).collect()
}

/// One label line of `/food/print/next`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NextLine {
    pub line_id: i64,
    #[serde(default)]
    pub sequence: i64,
    #[serde(default, deserialize_with = "de_text")]
    pub product_name: String,
    #[serde(deserialize_with = "de_qty")]
    pub print_qty: i64,
    #[serde(default, deserialize_with = "de_text")]
    pub best_before: String,
    #[serde(default, deserialize_with = "de_text")]
    pub label_png_base64: String,
    #[serde(default, deserialize_with = "de_text")]
    pub line_type: String,
}

/// A `/next` line that does not match the contract (missing / wrong-typed
/// field). It is never printed; with a `line_id` it is acked as an error so
/// one bad line cannot block the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLine {
    pub line_id: Option<i64>,
    pub reason: String,
}

/// `/food/print/next` result: the oldest printing batch, or no lines.
#[derive(Debug, Clone, PartialEq)]
pub struct NextBatch {
    pub batch_id: Option<i64>,
    pub production_date: String,
    pub lines: Vec<NextLine>,
    pub invalid: Vec<InvalidLine>,
}

/// Wire shape of the `/next` result; lines are parsed one by one.
#[derive(Deserialize)]
struct RawBatch {
    #[serde(default, deserialize_with = "de_opt_id")]
    batch_id: Option<i64>,
    #[serde(default, deserialize_with = "de_text")]
    production_date: String,
    #[serde(default)]
    lines: Vec<Value>,
}

impl NextBatch {
    /// Lines in print order (`sequence`; Odoo already sorts, the stable sort
    /// keeps its order for equal sequences).
    pub fn lines_in_order(&self) -> Vec<&NextLine> {
        let mut v: Vec<&NextLine> = self.lines.iter().collect();
        v.sort_by_key(|l| l.sequence);
        v
    }
}

fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// An Odoo id: a number, or `false`/`null` (Odoo's "empty") as `None`.
fn de_opt_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Number(n) if n.is_i64() => Ok(n.as_i64()),
        other => Err(serde::de::Error::custom(format!(
            "expected an integer id or false, got {other}"
        ))),
    }
}

/// `print_qty` as an integer; Odoo may send `40` or `40.0`.
fn de_qty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    if let Some(i) = v.as_i64() {
        return Ok(i);
    }
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 => Ok(f as i64),
        _ => Err(serde::de::Error::custom(format!(
            "print_qty must be a whole number, got {v}"
        ))),
    }
}

/// Parse the `/next` result object.
pub fn parse_next(result: Value) -> Result<NextBatch, RpcError> {
    // serde would also read a JSON array positionally into the struct.
    if !result.is_object() {
        return Err(RpcError::Protocol(format!(
            "/food/print/next result is not an object: {result}"
        )));
    }
    let raw: RawBatch = serde_json::from_value(result)
        .map_err(|e| RpcError::Protocol(format!("/food/print/next result: {e}")))?;
    let mut lines = Vec::with_capacity(raw.lines.len());
    let mut invalid = Vec::new();
    for value in raw.lines {
        let line_id = value.get("line_id").and_then(Value::as_i64);
        match serde_json::from_value::<NextLine>(value) {
            Ok(line) => lines.push(line),
            Err(e) => invalid.push(InvalidLine {
                line_id,
                reason: format!("invalid line: {e}"),
            }),
        }
    }
    if (!lines.is_empty() || !invalid.is_empty()) && raw.batch_id.is_none() {
        return Err(RpcError::Protocol(
            "/food/print/next returned lines without batch_id".into(),
        ));
    }
    Ok(NextBatch {
        batch_id: raw.batch_id,
        production_date: raw.production_date,
        lines,
        invalid,
    })
}

/// `/food/print/ack` params: `printed_qty` + `error: null` for a printed
/// line, `printed_qty = 0` + the reason otherwise.
pub fn ack_params(line_id: i64, printed_qty: i64, error: Option<&str>) -> Value {
    json!({
        "line_id": line_id,
        "printed_qty": printed_qty,
        "error": error.map(truncate_ack_error),
    })
}

/// Accepted ack; `duplicate` = Odoo already had this exact final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckAccepted {
    pub duplicate: bool,
}

pub fn parse_ack(result: &Value) -> Result<AckAccepted, RpcError> {
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(RpcError::Protocol(format!(
            "/food/print/ack result without ok:true: {result}"
        )));
    }
    Ok(AckAccepted {
        duplicate: result.get("duplicate").and_then(Value::as_bool) == Some(true),
    })
}

/// `/food/print/heartbeat` params (the last three are optional on the Odoo
/// side and ignored until odoo-erp #6665 part B stores them).
#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub printer_name: String,
    pub paper_status: String,
    pub error_status: String,
    pub client_version: String,
    pub last_job_id: Option<i64>,
    pub last_result: String,
}

impl Heartbeat {
    pub fn params(&self) -> Value {
        json!({
            "printer_name": self.printer_name,
            "paper_status": self.paper_status,
            "error_status": self.error_status,
            "client_version": self.client_version,
            "last_job_id": self.last_job_id,
            "last_result": self.last_result,
        })
    }
}

/// Async HTTP client for the three endpoints.
pub struct OdooRpc {
    transport: OdooTransport,
}

impl OdooRpc {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, RpcError> {
        Ok(Self {
            transport: OdooTransport::new(base_url, api_key, CALL_TIMEOUT)?,
        })
    }

    /// Full URL of an endpoint path (`/food/print/next`).
    pub fn endpoint(&self, path: &str) -> String {
        self.transport.endpoint(path)
    }

    async fn call(&self, path: &str, params: Value) -> Result<Value, RpcError> {
        self.transport.call(path, params).await
    }

    pub async fn next(&self) -> Result<NextBatch, RpcError> {
        parse_next(self.call("/food/print/next", json!({})).await?)
    }

    pub async fn ack(
        &self,
        line_id: i64,
        printed_qty: i64,
        error: Option<&str>,
    ) -> Result<AckAccepted, RpcError> {
        let result = self
            .call("/food/print/ack", ack_params(line_id, printed_qty, error))
            .await?;
        parse_ack(&result)
    }

    pub async fn heartbeat(&self, hb: &Heartbeat) -> Result<Value, RpcError> {
        self.call("/food/print/heartbeat", hb.params()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_next_contract_example_and_empty_queue() {
        let result = json!({"batch_id": 12, "production_date": "2026-09-29",
        "lines": [
            {"line_id": 346, "sequence": 20, "product_name": false, "print_qty": 1,
             "best_before": false, "label_png_base64": "", "line_type": "separator"},
            {"line_id": 345, "sequence": 10, "product_name": "Chlieb spišský 450g rezaný",
             "print_qty": 40.0, "best_before": "03.10.2026",
             "label_png_base64": "iVBOR", "line_type": "product"}
        ]});
        let b = parse_next(result).unwrap();
        assert_eq!(b.batch_id, Some(12));
        assert_eq!(b.production_date, "2026-09-29");
        let ordered: Vec<i64> = b.lines_in_order().iter().map(|l| l.line_id).collect();
        assert_eq!(ordered, vec![345, 346]);
        let first = b.lines_in_order()[0].clone();
        assert_eq!(first.print_qty, 40);
        assert_eq!(first.product_name, "Chlieb spišský 450g rezaný");
        assert_eq!(first.label_png_base64, "iVBOR");
        let sep = &b.lines_in_order()[1];
        assert_eq!(sep.product_name, "");
        assert_eq!(sep.best_before, "");
        assert_eq!(sep.line_type, "separator");

        let empty = parse_next(json!({"lines": []})).unwrap();
        assert!(empty.lines.is_empty());
        assert_eq!(empty.batch_id, None);
    }

    #[test]
    fn test_parse_next_rejects_bad_shapes() {
        assert!(matches!(
            parse_next(json!({"lines": [{"line_id": 1, "print_qty": 1}]})),
            Err(RpcError::Protocol(_))
        ));
        // only an invalid line, still needs its batch
        assert!(matches!(
            parse_next(json!({"lines": [{"line_id": 1}]})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            parse_next(json!({"batch_id": "x", "lines": []})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(parse_next(json!([1])), Err(RpcError::Protocol(_))));
        assert!(matches!(
            parse_next(json!({"batch_id": 1.5, "lines": []})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            parse_next(json!({"batch_id": true, "lines": []})),
            Err(RpcError::Protocol(_))
        ));
    }

    #[test]
    fn test_parse_next_isolates_invalid_lines() {
        let b = parse_next(json!({"batch_id": 7, "lines": [
            {"line_id": 1, "print_qty": 1.5},
            {"print_qty": 1},
            {"line_id": 3, "print_qty": 2, "label_png_base64": "x"}
        ]}))
        .unwrap();
        assert_eq!(b.lines.len(), 1);
        assert_eq!(b.lines[0].line_id, 3);
        assert_eq!(b.invalid.len(), 2);
        assert_eq!(b.invalid[0].line_id, Some(1));
        assert!(
            b.invalid[0].reason.starts_with("invalid line: "),
            "{:?}",
            b.invalid[0]
        );
        assert!(
            b.invalid[0].reason.contains("whole number"),
            "{:?}",
            b.invalid[0]
        );
        assert_eq!(b.invalid[1].line_id, None);
    }

    #[test]
    fn test_batch_id_false_or_null_is_empty() {
        for v in [json!(false), json!(null)] {
            let b = parse_next(json!({"batch_id": v, "lines": []})).unwrap();
            assert_eq!(b.batch_id, None);
            assert!(b.lines.is_empty() && b.invalid.is_empty());
        }
    }

    #[test]
    fn test_ack_params_contract_mapping() {
        assert_eq!(
            ack_params(345, 40, None),
            json!({"line_id": 345, "printed_qty": 40, "error": null})
        );
        assert_eq!(
            ack_params(346, 0, Some("empty png")),
            json!({"line_id": 346, "printed_qty": 0, "error": "empty png"})
        );
        let long = "č".repeat(250);
        let p = ack_params(1, 0, Some(&long));
        assert_eq!(p["error"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn test_parse_ack() {
        assert_eq!(
            parse_ack(&json!({"ok": true})).unwrap(),
            AckAccepted { duplicate: false }
        );
        assert_eq!(
            parse_ack(&json!({"ok": true, "duplicate": true})).unwrap(),
            AckAccepted { duplicate: true }
        );
        assert!(matches!(
            parse_ack(&json!({"ok": false})),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(parse_ack(&json!({})), Err(RpcError::Protocol(_))));
    }

    #[test]
    fn test_heartbeat_params() {
        let hb = Heartbeat {
            printer_name: "TSC ML241P Spišská".into(),
            paper_status: "ok".into(),
            error_status: "offline".into(),
            client_version: "0.8.41".into(),
            last_job_id: Some(12),
            last_result: "printed 3 labels".into(),
        };
        assert_eq!(
            hb.params(),
            json!({"printer_name": "TSC ML241P Spišská", "paper_status": "ok",
                   "error_status": "offline", "client_version": "0.8.41",
                   "last_job_id": 12, "last_result": "printed 3 labels"})
        );
    }

    #[test]
    fn test_endpoint_trims_trailing_slash() {
        let rpc = OdooRpc::new("https://erp.example.test/", "k-123").unwrap();
        assert_eq!(
            rpc.endpoint("/food/print/next"),
            "https://erp.example.test/food/print/next"
        );
    }

    #[test]
    fn test_numeric_text_fields_are_kept() {
        let b = parse_next(json!({"batch_id": 3, "production_date": 20260929,
            "lines": [{"line_id": 1, "print_qty": 1, "product_name": 42}]}))
        .unwrap();
        assert_eq!(b.production_date, "20260929");
        assert_eq!(b.lines[0].product_name, "42");
        assert_eq!(b.lines[0].sequence, 0);
    }

    #[test]
    fn test_truncate_ack_error() {
        assert_eq!(truncate_ack_error("short"), "short");
        assert_eq!(truncate_ack_error(&"a".repeat(201)).len(), 200);
    }
}
