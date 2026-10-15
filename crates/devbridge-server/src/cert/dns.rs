//! DNS-01 records for the ACME orders (#101): a [`DnsProvider`] creates the
//! `_acme-challenge.<name>` TXT record and removes it afterwards. The zone's
//! DNS provider is Cloudflare ([`CloudflareDns`], API v4, a token with
//! `Zone.DNS: Edit`); the token is never logged.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Create the TXT record `name` = `value`; returns its id.
    async fn set_txt(&self, name: &str, value: &str) -> Result<String>;
    /// Remove the record `set_txt` returned.
    async fn remove_txt(&self, id: &str) -> Result<()>;
}

/// The DNS-01 record name of `hostname`.
pub fn txt_record_name(hostname: &str) -> String {
    format!("_acme-challenge.{hostname}")
}

/// TTL of the challenge records (Cloudflare's minimum).
pub const TXT_TTL_SECS: u64 = 60;

/// Cloudflare's answer envelope: `{"success": bool, "errors": [{code,
/// message}], "result": …}` → the `result`, or an error naming Cloudflare's
/// codes and messages (never the request).
pub fn parse_cloudflare(status: u16, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)
        .map_err(|_| anyhow!("Cloudflare HTTP {status}: not JSON ({} bytes)", body.len()))?;
    if (200..300).contains(&status) && v.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    let errors = v
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .map(|e| {
                    let code = e.get("code").map(Value::to_string).unwrap_or_default();
                    let message = e.get("message").and_then(Value::as_str).unwrap_or("");
                    format!("{code} {message}")
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    bail!("Cloudflare HTTP {status}: {errors}")
}

/// The TXT record Cloudflare is asked to create.
pub fn txt_record(name: &str, value: &str) -> Value {
    json!({"type": "TXT", "name": name, "content": value, "ttl": TXT_TTL_SECS})
}

/// Cloudflare API v4 for one zone.
pub struct CloudflareDns {
    http: reqwest::Client,
    api_base: String,
    zone_id: String,
    token: String,
}

impl std::fmt::Debug for CloudflareDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareDns")
            .field("api_base", &self.api_base)
            .field("zone_id", &self.zone_id)
            .field("token", &devbridge_core::config::redact_secret(&self.token))
            .finish()
    }
}

impl CloudflareDns {
    pub fn new(api_base: &str, zone_id: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("HTTP client")?;
        Ok(Self {
            http,
            api_base: api_base.trim().trim_end_matches('/').to_string(),
            zone_id: zone_id.trim().to_string(),
            token: token.to_string(),
        })
    }

    /// `<api_base>/zones/<zone_id>/dns_records`.
    pub fn records_url(&self) -> String {
        format!("{}/zones/{}/dns_records", self.api_base, self.zone_id)
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("Cloudflare: {what}"))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .with_context(|| format!("Cloudflare: {what}: reading the answer"))?;
        parse_cloudflare(status, &body).with_context(|| format!("Cloudflare: {what}"))
    }
}

#[async_trait]
impl DnsProvider for CloudflareDns {
    async fn set_txt(&self, name: &str, value: &str) -> Result<String> {
        let request = self
            .http
            .post(self.records_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(txt_record(name, value).to_string());
        let result = self.send(request, "create the TXT record").await?;
        result
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .context("Cloudflare: the created record has no id")
    }

    async fn remove_txt(&self, id: &str) -> Result<()> {
        let request = self.http.delete(format!("{}/{id}", self.records_url()));
        self.send(request, "remove the TXT record")
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{delete, post};

    const TOKEN: &str = "cf-test-token-0123456789";

    #[derive(Default)]
    struct FakeCf {
        created: Vec<Value>,
        deleted: Vec<String>,
    }

    type St = Arc<Mutex<FakeCf>>;

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {TOKEN}"))
    }

    fn denied() -> axum::response::Response {
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}], "result": null})),
        )
            .into_response()
    }

    async fn create(
        State(st): State<St>,
        Path(zone): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> axum::response::Response {
        if !authorized(&headers) {
            return denied();
        }
        assert_eq!(zone, "zone123");
        let v: Value = serde_json::from_str(&body).unwrap();
        st.lock().unwrap().created.push(v);
        axum::Json(json!({"success": true, "errors": [], "result": {"id": "rec-1", "type": "TXT"}}))
            .into_response()
    }

    async fn remove(
        State(st): State<St>,
        Path((zone, id)): Path<(String, String)>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        if !authorized(&headers) {
            return denied();
        }
        assert_eq!(zone, "zone123");
        st.lock().unwrap().deleted.push(id.clone());
        axum::Json(json!({"success": true, "errors": [], "result": {"id": id}})).into_response()
    }

    async fn fake() -> (String, St) {
        let st: St = Arc::default();
        let app = Router::new()
            .route("/client/v4/zones/{zone}/dns_records", post(create))
            .route("/client/v4/zones/{zone}/dns_records/{id}", delete(remove))
            .with_state(Arc::clone(&st));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/client/v4/"), st)
    }

    #[tokio::test]
    async fn test_cloudflare_creates_and_removes_the_txt_record() {
        let (base, st) = fake().await;
        let cf = CloudflareDns::new(&base, " zone123 ", TOKEN).unwrap();
        assert_eq!(cf.records_url(), format!("{base}zones/zone123/dns_records"));
        let id = cf
            .set_txt("_acme-challenge.pjsnvs.pos.example.sk", "dns-value")
            .await
            .unwrap();
        assert_eq!(id, "rec-1");
        assert_eq!(
            st.lock().unwrap().created,
            vec![
                json!({"type": "TXT", "name": "_acme-challenge.pjsnvs.pos.example.sk",
                "content": "dns-value", "ttl": 60})
            ]
        );
        cf.remove_txt(&id).await.unwrap();
        assert_eq!(st.lock().unwrap().deleted, vec!["rec-1".to_string()]);
    }

    #[tokio::test]
    async fn test_cloudflare_refusal_names_the_error_never_the_token() {
        let (base, st) = fake().await;
        let cf = CloudflareDns::new(&base, "zone123", "wrong-token").unwrap();
        let err = format!(
            "{:#}",
            cf.set_txt("_acme-challenge.x.y", "v").await.unwrap_err()
        );
        assert!(
            err.contains("Cloudflare HTTP 403: 10000 Authentication error"),
            "{err}"
        );
        assert!(err.contains("create the TXT record"), "{err}");
        assert!(!err.contains("wrong-token"), "{err}");
        let err = format!("{:#}", cf.remove_txt("rec-1").await.unwrap_err());
        assert!(err.contains("remove the TXT record"), "{err}");
        assert!(st.lock().unwrap().created.is_empty());
        assert!(st.lock().unwrap().deleted.is_empty());
        let dbg = format!("{cf:?}");
        assert!(
            !dbg.contains("wrong-token") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }

    #[tokio::test]
    async fn test_cloudflare_unreachable_is_an_error() {
        let cf = CloudflareDns::new("http://127.0.0.1:9/client/v4", "zone123", TOKEN).unwrap();
        let err = format!("{:#}", cf.set_txt("a", "b").await.unwrap_err());
        assert!(err.contains("Cloudflare: create the TXT record"), "{err}");
    }

    #[test]
    fn test_parse_cloudflare() {
        assert_eq!(
            parse_cloudflare(200, r#"{"success":true,"result":{"id":"x"}}"#).unwrap(),
            json!({"id": "x"})
        );
        assert_eq!(
            parse_cloudflare(200, r#"{"success":true}"#).unwrap(),
            Value::Null
        );
        let e = parse_cloudflare(200, r#"{"success":false,"errors":[{"code":81057,"message":"Record already exists."},{"code":1,"message":"b"}]}"#)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "Cloudflare HTTP 200: 81057 Record already exists.; 1 b");
        let e = parse_cloudflare(500, r#"{"success":true,"result":{}}"#)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "Cloudflare HTTP 500: ", "a 5xx is never success");
        let e = parse_cloudflare(502, "<html>").unwrap_err().to_string();
        assert_eq!(e, "Cloudflare HTTP 502: not JSON (6 bytes)");
        let e = parse_cloudflare(299, r#"{"success":true,"result":1}"#).unwrap();
        assert_eq!(e, json!(1), "any 2xx");
        assert!(parse_cloudflare(300, r#"{"success":true}"#).is_err());
        assert!(parse_cloudflare(199, r#"{"success":true}"#).is_err());
    }

    #[test]
    fn test_txt_record_and_name() {
        assert_eq!(
            txt_record_name("pjsnvs.pos.example.sk"),
            "_acme-challenge.pjsnvs.pos.example.sk"
        );
        assert_eq!(
            txt_record("n", "v"),
            json!({"type": "TXT", "name": "n", "content": "v", "ttl": 60})
        );
    }

    #[tokio::test]
    async fn test_a_created_record_without_an_id_is_an_error() {
        async fn no_id() -> axum::response::Response {
            axum::Json(json!({"success": true, "result": {"id": ""}})).into_response()
        }
        let app = Router::new().route("/zones/{zone}/dns_records", post(no_id));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cf = CloudflareDns::new(&format!("http://{addr}"), "z", TOKEN).unwrap();
        let err = format!("{:#}", cf.set_txt("a", "b").await.unwrap_err());
        assert!(err.contains("has no id"), "{err}");
    }
}
