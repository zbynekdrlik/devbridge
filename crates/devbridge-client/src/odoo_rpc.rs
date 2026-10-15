//! Shared JSON-RPC 2.0 transport to Odoo `type="json"` routes — used by the
//! label source (`/food/print/*`, #90) and the device-job source
//! (`/pokladna/bridge/*`, #99).
//!
//! Every call is `POST`, envelope `{"jsonrpc":"2.0","method":"call",
//! "params":{…}}` → `{"jsonrpc":"2.0","result":{…}}`, `Authorization: Bearer
//! <API key>`. Errors come back with **HTTP 200**, checked in this order:
//!
//! 1. a top-level JSON-RPC `error` object — authentication or an exception
//!    (never a 401/403) → [`RpcError::Rpc`];
//! 2. a business refusal inside the result, in one of the two contract shapes:
//!    `result: {"error": "<text>"}` (labels) → [`RpcError::Business`], or
//!    `result: {"ok": false, "error": {"code", "message"}}` (device jobs,
//!    checked by [`require_ok`]) → [`RpcError::Refused`].
//!
//! The API key is only ever put into the `Authorization` header — it is not
//! part of any error text, log line or `Debug` output produced here.

use std::time::Duration;

use serde_json::{Value, json};

/// Response body excerpt kept in a transport / protocol error.
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
    /// `result.error` text (label contract) — Odoo understood the call and
    /// refused it. Final for this call.
    Business(String),
    /// `result.ok == false` with `error.code` / `error.message` (device-job
    /// contract) — Odoo understood the call and refused it. Final for this
    /// call; `code` is the machine token (`too_early`, `not_found`, …).
    Refused { code: String, message: String },
    /// The body is not the agreed envelope.
    Protocol(String),
}

impl RpcError {
    /// Refusals are Odoo's final answer; everything else may clear up.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Business(_) | Self::Refused { .. })
    }

    /// The machine code of a device-job refusal (`None` for anything else).
    pub fn refusal_code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "transport error: {m}"),
            Self::Rpc(m) => write!(f, "JSON-RPC error: {m}"),
            Self::Business(m) => write!(f, "Odoo refused: {m}"),
            Self::Refused { code, message } => write!(f, "Odoo refused ({code}): {message}"),
            Self::Protocol(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// How much of a response body an error message may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyExcerpt {
    /// Its first 300 characters (label printing: no personal / payment data).
    Text,
    /// Only its size: device-job responses carry card and customer data.
    SizeOnly,
}

fn body_excerpt(body: &str, mode: BodyExcerpt) -> String {
    match mode {
        BodyExcerpt::Text => excerpt(body),
        BodyExcerpt::SizeOnly => format!("<{} bytes, not logged>", body.len()),
    }
}

/// The SHAPE of a JSON value for an error message, never its values:
/// an object's keys, an array's length, or the type name.
pub fn value_shape(v: &Value) -> String {
    match v {
        Value::Object(o) => format!(
            "object {{{}}}",
            o.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
        ),
        Value::Array(a) => format!("array of {}", a.len()),
        Value::String(_) => "string".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Null => "null".to_string(),
    }
}

/// Request body for an Odoo `type="json"` route.
pub fn envelope(params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "call", "params": params})
}

/// Parse a response body: top-level `error` first, then `result.error` text.
pub fn parse_envelope(body: &str) -> Result<Value, RpcError> {
    parse_envelope_with(body, BodyExcerpt::Text)
}

/// [`parse_envelope`] whose protocol errors quote the body as `mode` allows.
pub fn parse_envelope_with(body: &str, mode: BodyExcerpt) -> Result<Value, RpcError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| RpcError::Protocol(format!("not JSON ({e}): {}", body_excerpt(body, mode))))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::Rpc(rpc_error_text(err)));
    }
    let result = v.get("result").cloned().ok_or_else(|| {
        RpcError::Protocol(format!("no \"result\": {}", body_excerpt(body, mode)))
    })?;
    if let Some(text) = result.get("error").and_then(odoo_text) {
        return Err(RpcError::Business(text));
    }
    Ok(result)
}

/// Device-job contract: `{"ok": true, …}` is success, `{"ok": false,
/// "error": {"code", "message"}}` a business refusal; anything else is not
/// the agreed shape.
pub fn require_ok(result: Value) -> Result<Value, RpcError> {
    match result.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(result),
        Some(false) => {
            let err = result.get("error");
            let field = |name: &str| {
                err.and_then(|e| e.get(name))
                    .and_then(odoo_text)
                    .unwrap_or_default()
            };
            let code = field("code");
            Err(RpcError::Refused {
                code: if code.is_empty() {
                    "unknown".to_string()
                } else {
                    code
                },
                message: field("message"),
            })
        }
        None => Err(RpcError::Protocol(format!(
            "result without a boolean \"ok\": {}",
            value_shape(&result)
        ))),
    }
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

/// Async HTTP client for Odoo `type="json"` routes of one base URL + key.
///
/// `Debug` is hand-written so the API key can never reach a log line.
pub struct OdooTransport {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    default_timeout: Duration,
    body_excerpt: BodyExcerpt,
}

impl std::fmt::Debug for OdooTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OdooTransport")
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &devbridge_core::config::redact_secret(&self.api_key),
            )
            .field("default_timeout", &self.default_timeout)
            .field("body_excerpt", &self.body_excerpt)
            .finish()
    }
}

impl OdooTransport {
    /// `default_timeout` is the whole-request timeout of [`Self::call`];
    /// [`Self::call_with_timeout`] overrides it per call (long-poll routes).
    pub fn new(base_url: &str, api_key: &str, default_timeout: Duration) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| RpcError::Transport(format!("HTTP client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            default_timeout,
            body_excerpt: BodyExcerpt::Text,
        })
    }

    /// How much of a response body its errors may quote (default: text).
    pub fn with_body_excerpt(mut self, mode: BodyExcerpt) -> Self {
        self.body_excerpt = mode;
        self
    }

    /// Full URL of an endpoint path (`/food/print/next`).
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The timeout [`Self::call`] uses.
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// One call with the default timeout; returns the parsed `result`.
    pub async fn call(&self, path: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(path, params, self.default_timeout)
            .await
    }

    /// One call; the whole request (connect, send, response body) must finish
    /// within `timeout`, else [`RpcError::Transport`].
    pub async fn call_with_timeout(
        &self,
        path: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let url = self.endpoint(path);
        let resp = self
            .http
            .post(&url)
            .timeout(timeout)
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
                body_excerpt(&body, self.body_excerpt)
            )));
        }
        parse_envelope_with(&body, self.body_excerpt)
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
    fn test_device_contract_refusal_object_passes_the_envelope_and_require_ok_reads_it() {
        // the device-job business shape: result.error is an OBJECT
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":false,"error":{"code":"too_early","message":"Úloha bola práve odovzdaná"}}}"#;
        let result =
            parse_envelope(body).expect("an error object is not label-style result.error text");
        let e = require_ok(result).unwrap_err();
        assert_eq!(
            e,
            RpcError::Refused {
                code: "too_early".into(),
                message: "Úloha bola práve odovzdaná".into()
            }
        );
        assert_eq!(e.refusal_code(), Some("too_early"));
        assert!(!e.is_retryable());
        assert_eq!(
            e.to_string(),
            "Odoo refused (too_early): Úloha bola práve odovzdaná"
        );
    }

    #[test]
    fn test_require_ok_shapes() {
        assert_eq!(
            require_ok(json!({"ok": true, "jobs": []})).unwrap(),
            json!({"ok": true, "jobs": []})
        );
        // a refusal without code/message still is a refusal
        assert_eq!(
            require_ok(json!({"ok": false})).unwrap_err(),
            RpcError::Refused {
                code: "unknown".into(),
                message: String::new()
            }
        );
        assert_eq!(
            require_ok(json!({"ok": false, "error": {"code": false, "message": "x"}})).unwrap_err(),
            RpcError::Refused {
                code: "unknown".into(),
                message: "x".into()
            }
        );
        for bad in [json!({}), json!({"ok": "true"}), json!([1]), json!(null)] {
            assert!(
                matches!(require_ok(bad.clone()), Err(RpcError::Protocol(_))),
                "{bad}"
            );
        }
        assert_eq!(RpcError::Transport("t".into()).refusal_code(), None);
    }

    #[test]
    fn test_size_only_excerpts_never_quote_the_body() {
        let truncated =
            r#"{"jsonrpc":"2.0","result":{"ok":true,"job":{"id":7,"payload":{"card_last4":"4242""#;
        match parse_envelope_with(truncated, BodyExcerpt::SizeOnly).unwrap_err() {
            RpcError::Protocol(m) => {
                assert!(!m.contains("4242"), "{m}");
                assert!(
                    m.contains(&format!("<{} bytes, not logged>", truncated.len())),
                    "{m}"
                );
            }
            other => panic!("{other:?}"),
        }
        match parse_envelope_with(r#"{"id":1,"card":"4242"}"#, BodyExcerpt::SizeOnly).unwrap_err() {
            RpcError::Protocol(m) => {
                assert!(!m.contains("4242") && m.starts_with("no \"result\""), "{m}")
            }
            other => panic!("{other:?}"),
        }
        // the label contract keeps its excerpt
        match parse_envelope_with("<html>bad gateway</html>", BodyExcerpt::Text).unwrap_err() {
            RpcError::Protocol(m) => assert!(m.contains("bad gateway"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(body_excerpt("abc", BodyExcerpt::Text), "abc");
        assert_eq!(
            body_excerpt("abc", BodyExcerpt::SizeOnly),
            "<3 bytes, not logged>"
        );
    }

    #[test]
    fn test_value_shape_names_keys_never_values() {
        assert_eq!(
            value_shape(&json!({"id": 7, "card_last4": "4242"})),
            "object {card_last4, id}"
        );
        assert_eq!(value_shape(&json!([1, 2, 3])), "array of 3");
        assert_eq!(value_shape(&json!("4242")), "string");
        assert_eq!(value_shape(&json!(4242)), "number");
        assert_eq!(value_shape(&json!(true)), "bool");
        assert_eq!(value_shape(&Value::Null), "null");
        let e = require_ok(json!({"job": {"card_last4": "4242"}})).unwrap_err();
        assert_eq!(
            e,
            RpcError::Protocol("result without a boolean \"ok\": object {job}".into())
        );
    }

    #[test]
    fn test_transport_endpoint_timeout_and_debug_never_show_the_key() {
        let t = OdooTransport::new(
            "https://erp.example.test/",
            "k-123-secret",
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            t.endpoint("/pokladna/bridge/next"),
            "https://erp.example.test/pokladna/bridge/next"
        );
        assert_eq!(t.default_timeout(), Duration::from_secs(60));
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("k-123-secret"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(dbg.contains("body_excerpt: Text"), "{dbg}");
        let quiet = t.with_body_excerpt(BodyExcerpt::SizeOnly);
        assert!(format!("{quiet:?}").contains("body_excerpt: SizeOnly"));
    }
}
