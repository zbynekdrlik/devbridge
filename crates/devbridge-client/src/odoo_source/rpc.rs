//! Odoo print API client (#90 contract, stream david4 2026-09-28).
//!
//! All three endpoints are Odoo `type="json"` routes: `POST`, JSON-RPC 2.0
//! envelope `{"jsonrpc":"2.0","method":"call","params":{…}}` →
//! `{"jsonrpc":"2.0","result":{…}}`, `Authorization: Bearer <API key>`.
//! Errors come back with **HTTP 200** in two shapes, checked in this order:
//!
//! 1. a top-level JSON-RPC `error` object — authentication or an exception
//!    (never a 401/403);
//! 2. a business error inside the result: `result: {"error": "<text>"}`.
//!
//! The API key is only ever put into the `Authorization` header — it is not
//! part of any error text or log line produced here.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// Longest `error` text Odoo stores on a line (`error_text`, 200 chars).
pub const MAX_ACK_ERROR_CHARS: usize = 200;

/// Response body excerpt kept in a transport error.
const BODY_EXCERPT_CHARS: usize = 300;

/// What went wrong talking to Odoo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// Network / HTTP-level failure (connect, timeout, non-200 status).
    /// Retried with backoff.
    Transport(String),
    /// Top-level JSON-RPC `error` (auth or server exception). Retried with
    /// backoff — a bad key must not make the client give up for good.
    Rpc(String),
    /// `result.error` — Odoo understood the call and refused it. Final for
    /// this call; retrying the same request cannot change the answer.
    Business(String),
    /// The body is not the agreed envelope.
    Protocol(String),
}

impl RpcError {
    /// Business errors are Odoo's final answer; everything else may clear up.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Business(_))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "transport error: {m}"),
            Self::Rpc(m) => write!(f, "JSON-RPC error: {m}"),
            Self::Business(m) => write!(f, "Odoo refused: {m}"),
            Self::Protocol(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// Request body for an Odoo `type="json"` route.
pub fn envelope(params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "call", "params": params})
}

/// Parse a response body: top-level `error` first, then `result.error`.
pub fn parse_envelope(body: &str) -> Result<Value, RpcError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| RpcError::Protocol(format!("not JSON ({e}): {}", excerpt(body))))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::Rpc(rpc_error_text(err)));
    }
    let result = v
        .get("result")
        .cloned()
        .ok_or_else(|| RpcError::Protocol(format!("no \"result\": {}", excerpt(body))))?;
    if let Some(text) = result.get("error").and_then(odoo_text) {
        return Err(RpcError::Business(text));
    }
    Ok(result)
}

/// `message` + `data.message` of a JSON-RPC error object (Odoo puts the
/// exception text in `data.message`, e.g. "Access Denied").
fn rpc_error_text(err: &Value) -> String {
    let message = err.get("message").and_then(Value::as_str).unwrap_or("");
    let data = err
        .get("data")
        .and_then(|d| d.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let code = err.get("code").map(Value::to_string).unwrap_or_default();
    match (message.is_empty(), data.is_empty()) {
        (false, false) => format!("{message}: {data} (code {code})"),
        (false, true) => format!("{message} (code {code})"),
        (true, false) => format!("{data} (code {code})"),
        (true, true) => format!("{err}"),
    }
}

/// Odoo renders an empty Char field as `false`; treat `false`/`null`/`""`
/// as absent and any other scalar as text.
fn odoo_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn excerpt(body: &str) -> String {
    let mut s: String = body.chars().take(BODY_EXCERPT_CHARS).collect();
    if body.chars().count() > BODY_EXCERPT_CHARS {
        s.push('…');
    }
    s
}

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
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OdooRpc {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| RpcError::Transport(format!("HTTP client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        })
    }

    /// Full URL of an endpoint path (`/food/print/next`).
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn call(&self, path: &str, params: Value) -> Result<Value, RpcError> {
        let url = self.endpoint(path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(envelope(params).to_string())
            .send()
            .await
            .map_err(|e| RpcError::Transport(format!("POST {url}: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| RpcError::Transport(format!("POST {url}: reading body: {e}")))?;
        if !status.is_success() {
            return Err(RpcError::Transport(format!(
                "POST {url}: HTTP {status}: {}",
                excerpt(&body)
            )));
        }
        parse_envelope(&body)
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
    fn test_envelope_shape() {
        assert_eq!(
            envelope(json!({"line_id": 5})),
            json!({"jsonrpc": "2.0", "method": "call", "params": {"line_id": 5}})
        );
    }

    #[test]
    fn test_parse_envelope_ok_result() {
        let r = parse_envelope(r#"{"jsonrpc":"2.0","result":{"ok":true}}"#).unwrap();
        assert_eq!(r, json!({"ok": true}));
        // `error: false/null/""` inside result is not an error
        for body in [
            r#"{"jsonrpc":"2.0","result":{"ok":true,"error":false}}"#,
            r#"{"jsonrpc":"2.0","result":{"ok":true,"error":null}}"#,
            r#"{"jsonrpc":"2.0","result":{"ok":true,"error":""}}"#,
            r#"{"jsonrpc":"2.0","error":null,"result":{"ok":true}}"#,
        ] {
            assert!(parse_envelope(body).is_ok(), "{body}");
        }
    }

    #[test]
    fn test_parse_envelope_top_level_error_is_rpc_error_checked_first() {
        let body = r#"{"jsonrpc":"2.0","error":{"code":100,"message":"Odoo Session Expired","data":{"message":"Access Denied"}},"result":{"error":"x"}}"#;
        let e = parse_envelope(body).unwrap_err();
        assert_eq!(
            e,
            RpcError::Rpc("Odoo Session Expired: Access Denied (code 100)".into())
        );
        assert!(e.is_retryable());
        assert_eq!(
            parse_envelope(r#"{"error":{"message":"Boom","code":200}}"#).unwrap_err(),
            RpcError::Rpc("Boom (code 200)".into())
        );
        assert_eq!(
            parse_envelope(r#"{"error":{"data":{"message":"Only data"},"code":1}}"#).unwrap_err(),
            RpcError::Rpc("Only data (code 1)".into())
        );
        assert_eq!(
            parse_envelope(r#"{"error":"flat"}"#).unwrap_err(),
            RpcError::Rpc("\"flat\"".into())
        );
    }

    #[test]
    fn test_parse_envelope_business_error() {
        let e =
            parse_envelope(r#"{"jsonrpc":"2.0","result":{"error":"Dávka nie je v stave Tlač."}}"#)
                .unwrap_err();
        assert_eq!(e, RpcError::Business("Dávka nie je v stave Tlač.".into()));
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_parse_envelope_protocol_errors() {
        assert!(matches!(
            parse_envelope("<html>502</html>"),
            Err(RpcError::Protocol(_))
        ));
        assert!(matches!(
            parse_envelope(r#"{"jsonrpc":"2.0"}"#),
            Err(RpcError::Protocol(_))
        ));
        let long = "x".repeat(1000);
        match parse_envelope(&long).unwrap_err() {
            RpcError::Protocol(m) => {
                assert!(m.chars().count() < 400, "excerpt too long: {}", m.len())
            }
            other => panic!("{other:?}"),
        }
    }

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
    fn test_rpc_error_display() {
        assert_eq!(
            RpcError::Business("x".into()).to_string(),
            "Odoo refused: x"
        );
        assert_eq!(
            RpcError::Transport("t".into()).to_string(),
            "transport error: t"
        );
        assert_eq!(RpcError::Rpc("r".into()).to_string(), "JSON-RPC error: r");
        assert_eq!(
            RpcError::Protocol("p".into()).to_string(),
            "unexpected response: p"
        );
        assert!(RpcError::Transport("t".into()).is_retryable());
        assert!(RpcError::Protocol("p".into()).is_retryable());
    }

    #[test]
    fn test_excerpt_boundary() {
        let exact = "a".repeat(BODY_EXCERPT_CHARS);
        assert_eq!(excerpt(&exact), exact);
        let over = "b".repeat(BODY_EXCERPT_CHARS + 1);
        let e = excerpt(&over);
        assert!(e.ends_with('…'), "{e}");
        assert_eq!(e.chars().count(), BODY_EXCERPT_CHARS + 1);
    }

    #[test]
    fn test_business_error_text_forms() {
        assert_eq!(
            parse_envelope(r#"{"result":{"error":42}}"#).unwrap_err(),
            RpcError::Business("42".into())
        );
        assert!(parse_envelope(r#"{"result":{"error":"   "}}"#).is_ok());
        assert!(parse_envelope(r#"{"result":[1,2]}"#).is_ok());
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
