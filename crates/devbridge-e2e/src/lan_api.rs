//! E2E coverage of the tills' LAN API (issue #101) on the RAW E2E client.
//!
//! `deploy/e2e-setup-client-local.ps1` writes the RAW client's
//! `[client.pokladna.lan_api]` (port 9243, hostname [`LAN_API_HOST`]) and its
//! two tills with their LAN keys through the REAL installer functions, plus
//! the E2E-only `acme = false` (the certificate is not from pz-server here).
//! `e2e-deploy-client` then runs `devbridge-e2e lan-test-cert` on pz-snv: a
//! throwaway test CA + a certificate for [`LAN_API_HOST`] in the RAW data
//! dir's `lan-api\` ([`write_test_cert`]); only the CA's PUBLIC certificate
//! reaches this runner (CI artifact `lan-api-test-ca`, env `E2E_LAN_API_CA`).
//!
//! The step is the ticket's acceptance, with the RAW client's test executor as
//! the device: while Odoo is unreachable (step 36's fake is gone) a till's
//! receipt goes over HTTPS to the LAN API and runs once; its replay answers
//! the stored result; a wrong key / another till's key / a card payment are
//! refused; the job is NOT reported yet. Then a fake Odoo comes up and
//! receives exactly ONE `report_offline` for it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use serde_json::{Value, json};

use crate::device_jobs::{E2E_POKLADNA_KEY, FAKE_POKLADNA_PORT};

/// Must match `deploy/e2e-setup-client-local.ps1`.
pub const LAN_API_PORT: u16 = 9243;
pub const LAN_API_HOST: &str = "e2e-raw-client.lan-api.test";
pub const LAN_KEY_TILL_3: &str = "e2e-lan-key-till-3-devbridge-101-0123456789";
pub const LAN_KEY_TILL_4: &str = "e2e-lan-key-till-4-devbridge-101-0123456789";

/// The LAN API listens once its certificate is usable (polled every 10 s).
const LAN_UP_WAIT: Duration = Duration::from_secs(120);
/// The reporter backs off up to 30 s while Odoo is unreachable.
const REPORT_WAIT: Duration = Duration::from_secs(150);

/// `devbridge-e2e lan-test-cert <dir> <hostname> <ca-out>`: a throwaway test
/// CA, a certificate for `hostname` signed by it (`<dir>/cert.pem` = leaf +
/// CA, `<dir>/key.pem`), and the CA's certificate in `ca_out`. The CA's key
/// is never written anywhere.
pub fn write_test_cert(dir: &Path, hostname: &str, ca_out: &Path) -> Result<()> {
    let ca_key = KeyPair::generate().context("CA key")?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "devbridge E2E LAN API test CA");
    let ca_pem = ca_params
        .self_signed(&ca_key)
        .context("CA certificate")?
        .pem();
    let key = KeyPair::generate().context("leaf key")?;
    let leaf = CertificateParams::new(vec![hostname.to_string()])
        .context("leaf params")?
        .signed_by(&key, &Issuer::from_params(&ca_params, &ca_key))
        .context("leaf certificate")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("cert.pem"), format!("{}{ca_pem}", leaf.pem()))
        .context("write cert.pem")?;
    std::fs::write(dir.join("key.pem"), key.serialize_pem()).context("write key.pem")?;
    std::fs::write(ca_out, &ca_pem).with_context(|| format!("write {}", ca_out.display()))?;
    Ok(())
}

/// A LAN API answer: `(status, body)`.
type Answer = (u16, Value);

/// A done job ran once on the test device (`via` execute), named
/// `client_uuid`, `replay` as expected.
pub fn check_lan_done(answer: &Answer, client_uuid: &str, replay: bool) -> Result<()> {
    let (status, body) = answer;
    ensure!(*status == 200, "expected 200, got {status}: {body}");
    ensure!(body["state"] == "done", "state not done: {body}");
    ensure!(body["client_uuid"] == client_uuid, "client_uuid: {body}");
    ensure!(body["kasa_id"] == 3, "kasa_id: {body}");
    ensure!(body["replay"] == replay, "replay must be {replay}: {body}");
    let result = &body["result"];
    ensure!(
        result["device"] == "test" && result["executions"] == 1 && result["via"] == "execute",
        "the test device must have run it exactly once: {result}"
    );
    Ok(())
}

/// The one `report_offline` of the job: done, its till, origin lan.
pub fn check_report(report: &Value, client_uuid: &str) -> Result<()> {
    ensure!(
        report["client_uuid"] == client_uuid,
        "client_uuid: {report}"
    );
    ensure!(report["kasa_id"] == 3, "kasa_id: {report}");
    ensure!(
        report["job_type"] == "fiscal.cash_register",
        "job_type: {report}"
    );
    ensure!(report["pos_order_id"] == 812, "pos_order_id: {report}");
    ensure!(report["state"] == "done", "state: {report}");
    ensure!(report["origin"] == "lan", "origin: {report}");
    ensure!(report["result"]["executions"] == 1, "result: {report}");
    ensure!(
        report["executed_at"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "executed_at: {report}"
    );
    Ok(())
}

// ── the fake Odoo that comes back online ──────────────────────────────────

#[derive(Default)]
struct Fake {
    reports: Vec<Value>,
    unauthorized: usize,
}

type Shared = Arc<Mutex<Fake>>;

fn result(v: Value) -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": v}))
}

fn authorized(st: &Shared, headers: &HeaderMap) -> bool {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {E2E_POKLADNA_KEY}"));
    if !ok {
        st.lock().unwrap().unauthorized += 1;
    }
    ok
}

/// No job for the bridge: wait like a long-poll (at most 10 s).
async fn next(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if authorized(&st, &headers) {
        let wait = body["params"]["wait_s"].as_u64().unwrap_or(0).min(10);
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
    result(
        json!({"ok": true, "job": null, "wait_s": 10, "kasy": [{"id": 3, "name": "E2E Pokladna"}]}),
    )
}

async fn empty_ok(State(st): State<Shared>, headers: HeaderMap) -> Json<Value> {
    authorized(&st, &headers);
    result(json!({"ok": true, "jobs": [], "kasy": [{"id": 3, "name": "E2E Pokladna"}]}))
}

async fn report_offline(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 100,
            "message": "Odoo Server Error", "data": {"message": "Access Denied"}}}));
    }
    let mut s = st.lock().unwrap();
    s.reports.push(body["params"].clone());
    let job_id = 80_000 + s.reports.len() as i64;
    result(json!({"ok": true, "job_id": job_id, "created": true}))
}

/// Bind the fake Odoo port (step 36's listener may take a moment to close).
async fn bind_fake() -> Result<tokio::net::TcpListener> {
    let started = Instant::now();
    loop {
        match tokio::net::TcpListener::bind(("0.0.0.0", FAKE_POKLADNA_PORT)).await {
            Ok(listener) => return Ok(listener),
            Err(e) if started.elapsed() < Duration::from_secs(30) => {
                println!("  fake Odoo port {FAKE_POKLADNA_PORT} busy ({e}) - retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => bail!("bind fake Odoo on port {FAKE_POKLADNA_PORT}: {e}"),
        }
    }
}

// ── the step ───────────────────────────────────────────────────────────────

async fn call(req: reqwest::RequestBuilder) -> Result<Answer> {
    let resp = req.send().await.context("LAN API request")?;
    let status = resp.status().as_u16();
    let text = resp.text().await.context("LAN API answer")?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    ))
}

struct LanApi {
    https: reqwest::Client,
    base: String,
}

impl LanApi {
    async fn post(&self, key: &str, body: &Value) -> Result<Answer> {
        call(
            self.https
                .post(format!("{}/v1/jobs", self.base))
                .bearer_auth(key)
                .json(body),
        )
        .await
    }

    async fn get(&self, key: &str, path: &str) -> Result<Answer> {
        call(
            self.https
                .get(format!("{}{path}", self.base))
                .bearer_auth(key),
        )
        .await
    }
}

fn run_uuid() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("e2e-lan-{ms}")
}

/// Step body — see the module docs.
pub async fn test_lan_api(client_host: &str, ca_path: &str) -> Result<()> {
    let ca_pem = std::fs::read(ca_path)
        .with_context(|| format!("read the LAN API test CA {ca_path} (E2E_LAN_API_CA)"))?;
    let addr: SocketAddr = format!("{client_host}:{LAN_API_PORT}")
        .parse()
        .context("LAN API address")?;
    let lan = LanApi {
        https: reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).context("test CA PEM")?)
            .resolve(LAN_API_HOST, addr)
            .timeout(Duration::from_secs(120))
            .build()?,
        base: format!("https://{LAN_API_HOST}:{LAN_API_PORT}"),
    };

    // the LAN API is up (HTTPS, the test certificate, trusted via the test CA)
    let started = Instant::now();
    let health = loop {
        match lan.get(LAN_KEY_TILL_3, "/v1/health").await {
            Ok(answer) if answer.0 == 200 => break answer.1,
            other if started.elapsed() > LAN_UP_WAIT => {
                bail!(
                    "LAN API not up within {}s: {other:?}",
                    LAN_UP_WAIT.as_secs()
                )
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    };
    ensure!(
        health["status"] == "ok" && health["portos"] == "test",
        "health: {health}"
    );
    println!("  LAN API up over HTTPS: {health}");

    // 1. Odoo is down (step 36's fake is gone): a receipt runs exactly once
    let client_uuid = run_uuid();
    let receipt = json!({"client_uuid": client_uuid, "kasa_id": 3,
        "job_type": "fiscal.cash_register", "pos_order_id": 812,
        "payload": {"devbridge_test": {"outcome": "done"}, "amount": 2.5, "currency": "EUR"}});
    let first = lan.post(LAN_KEY_TILL_3, &receipt).await?;
    check_lan_done(&first, &client_uuid, false)?;
    ensure!(
        first.1["reported"] == false,
        "reported while Odoo is down? {}",
        first.1
    );
    // 2. the replay answers the stored result, nothing runs again
    let again = lan.post(LAN_KEY_TILL_3, &receipt).await?;
    check_lan_done(&again, &client_uuid, true)?;
    // 3. refusals
    let (status, body) = lan
        .post("e2e-wrong-lan-key-0123456789abcdefghijk", &receipt)
        .await?;
    ensure!(status == 401, "a wrong key must be 401: {status} {body}");
    let (status, body) = lan.post(E2E_POKLADNA_KEY, &receipt).await?;
    ensure!(
        status == 401,
        "the Odoo bridge key must be 401: {status} {body}"
    );
    let (status, body) = lan.post(LAN_KEY_TILL_4, &receipt).await?;
    ensure!(
        status == 403 && body["error"]["code"] == "wrong_kasa",
        "till 4's key for till 3 must be 403: {status} {body}"
    );
    let card = json!({"client_uuid": format!("{client_uuid}-card"), "kasa_id": 3,
        "job_type": "card.purchase", "payload": {"amount": 2.5, "currency": "EUR"}});
    let (status, body) = lan.post(LAN_KEY_TILL_3, &card).await?;
    ensure!(
        status == 422 && body["error"]["code"] == "unsupported",
        "a card payment must be 422 unsupported: {status} {body}"
    );
    println!(
        "  receipt done once over the LAN, replay answered, wrong / bridge / other-till key + card refused"
    );

    // 4. Odoo is back: exactly one report_offline
    let state: Shared = Arc::default();
    let app = Router::new()
        .route("/pokladna/bridge/next", post(next))
        .route("/pokladna/bridge/pending", post(empty_ok))
        .route("/pokladna/bridge/heartbeat", post(empty_ok))
        .route("/pokladna/bridge/ack", post(empty_ok))
        .route("/pokladna/bridge/report_offline", post(report_offline))
        .with_state(Arc::clone(&state));
    let listener = bind_fake().await?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let outcome = verify_report(&lan, &state, &client_uuid).await;
    server.abort();
    outcome?;
    println!("PASS");
    Ok(())
}

async fn verify_report(lan: &LanApi, state: &Shared, client_uuid: &str) -> Result<()> {
    let started = Instant::now();
    let ours = |s: &Fake| {
        s.reports
            .iter()
            .filter(|r| r["client_uuid"] == client_uuid)
            .cloned()
            .collect::<Vec<_>>()
    };
    while ours(&state.lock().unwrap()).is_empty() {
        ensure!(
            started.elapsed() < REPORT_WAIT,
            "no report_offline within {}s",
            REPORT_WAIT.as_secs()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // a duplicate would come within the reporter's next rounds
    tokio::time::sleep(Duration::from_secs(5)).await;
    let reports = ours(&state.lock().unwrap());
    ensure!(
        reports.len() == 1,
        "exactly one report expected: {reports:?}"
    );
    check_report(&reports[0], client_uuid)?;
    let (status, body) = lan
        .get(LAN_KEY_TILL_3, &format!("/v1/jobs/3/{client_uuid}"))
        .await?;
    ensure!(
        status == 200 && body["reported"] == true && body["odoo_job_id"].as_i64().is_some(),
        "the job must be reported now: {status} {body}"
    );
    ensure!(
        state.lock().unwrap().unauthorized == 0,
        "a request without the bridge key"
    );
    println!(
        "  Odoo back: exactly one report_offline (odoo job {}), the LAN job shows it",
        body["odoo_job_id"]
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(uuid: &str, replay: bool) -> Answer {
        (
            200,
            json!({"state": "done", "client_uuid": uuid, "kasa_id": 3, "replay": replay,
                "result": {"device": "test", "executions": 1, "via": "execute"}}),
        )
    }

    #[test]
    fn test_check_lan_done() {
        assert!(check_lan_done(&done("u", false), "u", false).is_ok());
        assert!(check_lan_done(&done("u", true), "u", false).is_err());
        assert!(check_lan_done(&done("u", false), "v", false).is_err());
        let mut twice = done("u", false);
        twice.1["result"]["executions"] = json!(2);
        assert!(check_lan_done(&twice, "u", false).is_err());
        let mut status = done("u", false);
        status.0 = 202;
        assert!(check_lan_done(&status, "u", false).is_err());
    }

    #[test]
    fn test_check_report() {
        let good = json!({"client_uuid": "u", "kasa_id": 3, "job_type": "fiscal.cash_register",
            "pos_order_id": 812, "state": "done", "origin": "lan", "result": {"executions": 1},
            "executed_at": "2026-10-15T10:00:00Z"});
        assert!(check_report(&good, "u").is_ok());
        for (key, bad) in [
            ("origin", json!("odoo")),
            ("state", json!("failed")),
            ("executed_at", json!("2026-10-15T10:00:00+00:00")),
            ("pos_order_id", Value::Null),
        ] {
            let mut r = good.clone();
            r[key] = bad;
            assert!(check_report(&r, "u").is_err(), "{key}");
        }
    }

    #[test]
    fn test_write_test_cert() {
        let dir = std::env::temp_dir().join(format!("dbe2e-lan-{}", run_uuid()));
        let ca = dir.join("ca.pem");
        write_test_cert(&dir.join("lan-api"), LAN_API_HOST, &ca).unwrap();
        let chain = std::fs::read_to_string(dir.join("lan-api/cert.pem")).unwrap();
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2, "leaf + CA");
        let ca_pem = std::fs::read_to_string(&ca).unwrap();
        assert!(chain.ends_with(&ca_pem), "the chain ends with the CA");
        let key = std::fs::read_to_string(dir.join("lan-api/key.pem")).unwrap();
        assert!(key.contains("BEGIN PRIVATE KEY"));
        assert!(
            !ca_pem.contains("PRIVATE KEY"),
            "the CA key is never written"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
