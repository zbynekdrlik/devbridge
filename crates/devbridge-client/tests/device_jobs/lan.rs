//! The tills' LAN API (#101) end to end: HTTPS with a test CA → the LAN API
//! → the SAME exactly-once path as Odoo jobs (journal, per-device queue,
//! externalId) → a fake Portos / the test device → `report_offline` to the
//! fake Odoo. Covers auth, refusals before any device contact, replay vs
//! conflict, a lost Portos answer reconciled after a restart (exactly one
//! POST), a LAN receipt later handed out by Odoo `next` (still one receipt),
//! one device queue shared by both channels, and the reporter's retries.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use devbridge_client::device_jobs::executor::{DeviceExecutor, DeviceKind, ExecutorRegistry};
use devbridge_client::device_jobs::journal::{JobState, Journal};
use devbridge_client::device_jobs::portos::payload::external_id;
use devbridge_client::device_jobs::portos::{PortosConfig, PortosExecutor, PortosTiming};
use devbridge_client::device_jobs::test_executor::{SCRIPT_KEY, TestDevice};
use devbridge_client::device_jobs::{DeviceJobSource, Timing};
use devbridge_client::lan_api::LanApiDeps;
use devbridge_client::lan_api::cert::CertPaths;
use devbridge_core::config::{LanApiConfig, PokladnaClientConfig, PokladnaKasaConfig};

use super::fake::*;
use super::fake_portos::*;

pub const HOST: &str = "pjsnvs.pos.example.sk";
pub const KEY3: &str = "lan-key-of-till-3-0123456789abcdefgh";
pub const KEY4: &str = "lan-key-of-till-4-0123456789abcdefgh";
const ORP: &str = "88812345678900001";

/// A test CA (params + key) and its PEM.
pub fn test_ca() -> (CertificateParams, KeyPair, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "devbridge LAN API test CA");
    let pem = params.self_signed(&key).unwrap().pem();
    (params, key, pem)
}

/// (CA PEM, leaf chain PEM, leaf key PEM) for `host`.
pub fn test_pki(host: &str) -> (String, String, String) {
    let (ca_params, ca_key, ca_pem) = test_ca();
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![host.to_string()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    (
        ca_pem.clone(),
        format!("{}{ca_pem}", leaf.pem()),
        key.serialize_pem(),
    )
}

pub fn lan_kasa(id: i64, key: Option<&str>) -> PokladnaKasaConfig {
    PokladnaKasaConfig {
        id,
        portos_url: None,
        terminal_ip: None,
        terminal_tid: None,
        lan_key: key.map(str::to_string),
    }
}

/// `[client.pokladna]` with the LAN API (a file certificate) and tills 3 + 4.
pub fn lan_config(url: &str) -> PokladnaClientConfig {
    PokladnaClientConfig {
        kasy: vec![lan_kasa(3, Some(KEY3)), lan_kasa(4, Some(KEY4))],
        lan_api: LanApiConfig {
            enabled: true,
            port: 0,
            hostname: HOST.into(),
            acme: false,
        },
        ..config(url, 1)
    }
}

/// A running source with its LAN API, and an HTTPS client that trusts the
/// CA `ca_pem` and resolves [`HOST`] to the listener.
pub struct Lan {
    pub run: Running,
    pub https: reqwest::Client,
    pub base: String,
}

/// An HTTPS client for `host` → `addr`, trusting only `ca_pem` (rustls on
/// every OS — the portos-live job runs these on Windows).
pub fn https_client(ca_pem: &str, host: &str, addr: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .resolve(host, addr)
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// Start the source with its LAN API on 127.0.0.1 (`dir` = the data dir).
pub fn start_lan(
    cfg: PokladnaClientConfig,
    journal: Arc<Journal>,
    registry: ExecutorRegistry,
    timing: Timing,
    dir: &Path,
    ca_pem: &str,
    server_address: &str,
) -> Lan {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let source = DeviceJobSource::new(cfg, journal, registry, timing)
        .unwrap()
        .with_lan_api(LanApiDeps {
            data_dir: dir.to_path_buf(),
            server_address: server_address.to_string(),
            client_id: "pjsnvs".into(),
            listener: Some(listener),
        });
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(source.run(shutdown.clone()));
    Lan {
        run: Running { shutdown, handle },
        https: https_client(ca_pem, HOST, addr),
        base: format!("https://{HOST}:{}", addr.port()),
    }
}

/// Write a test certificate for [`HOST`] into `dir`; returns the CA PEM.
pub fn install_test_cert(dir: &Path) -> String {
    let (ca, chain, key) = test_pki(HOST);
    let paths = CertPaths::in_data_dir(dir);
    std::fs::create_dir_all(&paths.dir).unwrap();
    std::fs::write(&paths.cert, chain).unwrap();
    std::fs::write(&paths.key, key).unwrap();
    ca
}

async fn answer(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

pub async fn post_raw(lan: &Lan, key: Option<&str>, body: String) -> (u16, Value) {
    let mut req = lan
        .https
        .post(format!("{}/v1/jobs", lan.base))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    answer(req.send().await.unwrap()).await
}

pub async fn post(lan: &Lan, key: Option<&str>, body: &Value) -> (u16, Value) {
    post_raw(lan, key, body.to_string()).await
}

pub async fn get(lan: &Lan, key: Option<&str>, path: &str) -> (u16, Value) {
    let mut req = lan.https.get(format!("{}{path}", lan.base));
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    answer(req.send().await.unwrap()).await
}

fn sale() -> Value {
    json!({
        "data": {
            "cashRegisterCode": ORP,
            "items": [{"type": "Positive", "name": "Chlieb", "quantity": {"amount": 2, "unit": "ks"},
                       "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0}],
            "payments": [{"name": "Hotovost", "amount": 2.6}],
            "roundingAmount": 0.02
        },
        "print": {"printerName": "pdf"}
    })
}

fn receipt(uuid: &str) -> Value {
    json!({"client_uuid": uuid, "kasa_id": 3, "job_type": "fiscal.cash_register",
        "payload": sale(), "pos_order_id": 812})
}

fn scripted(uuid: &str, outcome: &str) -> Value {
    json!({"client_uuid": uuid, "kasa_id": 3, "job_type": "fiscal.open_drawer",
        "payload": {SCRIPT_KEY: {"outcome": outcome}}})
}

fn portos_registry(journal: &Arc<Journal>, url: &str, register: Duration) -> ExecutorRegistry {
    let portos = PortosExecutor::new(
        PortosConfig {
            default_url: url.to_string(),
            kasy: Default::default(),
            timing: PortosTiming {
                lookup: Duration::from_secs(2),
                register,
                control: Duration::from_millis(300),
                probe: Duration::from_millis(300),
            },
        },
        Arc::clone(journal),
    )
    .unwrap();
    let mut r = ExecutorRegistry::default();
    r.register(Arc::new(portos)).unwrap();
    r
}

fn journal_in(dir: &Path) -> Arc<Journal> {
    Arc::new(Journal::open(&dir.join("device-journal.db")).unwrap())
}

fn lan_state(journal: &Journal, uuid: &str) -> Option<JobState> {
    journal.lan_job(3, uuid).unwrap().map(|j| j.state())
}

#[tokio::test]
async fn test_lan_receipt_over_https_runs_once_and_is_reported_once() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let registry = portos_registry(&journal, &purl, Duration::from_secs(2));
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let uuid = "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a";

    let (status, body) = post(&lan, Some(KEY3), &receipt(uuid)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "done", "{body}");
    assert_eq!(body["replay"], false);
    assert_eq!(body["client_uuid"], uuid);
    assert_eq!(body["kasa_id"], 3);
    assert_eq!(body["job_type"], "fiscal.cash_register");
    assert_eq!(body["result"]["device"], "portos", "{body}");
    assert_eq!(body["error"], Value::Null);
    {
        let p = portos.lock().unwrap();
        assert_eq!(p.post_count(), 1);
        assert_eq!(
            p.posts[0].1["request"]["externalId"],
            external_id(3, uuid),
            "the same externalId as the Odoo channel"
        );
        assert_eq!(p.lookups.len(), 1, "looked up before the POST");
    }

    // the report reaches Odoo once
    eventually("the LAN job is reported", || {
        journal
            .lan_job(3, uuid)
            .unwrap()
            .is_some_and(|j| j.reported)
    })
    .await;
    let stored = journal.lan_job(3, uuid).unwrap().unwrap();
    assert_eq!(stored.odoo_job_id, Some(70_001));
    assert_eq!(stored.report_note.as_deref(), Some("created"));
    {
        let f = fake.lock().unwrap();
        assert_eq!(f.reports.len(), 1);
        let r = &f.reports[0];
        assert_eq!(r["client_uuid"], uuid);
        assert_eq!(r["kasa_id"], 3);
        assert_eq!(r["job_type"], "fiscal.cash_register");
        assert_eq!(r["pos_order_id"], 812);
        assert_eq!(r["payload"], sale());
        assert_eq!(r["state"], "done");
        assert_eq!(r["result"], body["result"]);
        assert_eq!(r["error"], Value::Null);
        assert_eq!(r["origin"], "lan");
        assert!(r["executed_at"].as_str().unwrap().ends_with('Z'), "{r}");
        assert!(f.acks.is_empty(), "a LAN job is reported, never acked");
    }

    // a replay answers the stored row — Portos is not asked again
    let (status, again) = post(&lan, Some(KEY3), &receipt(uuid)).await;
    assert_eq!(status, 200);
    assert_eq!(again["replay"], true);
    assert_eq!(again["result"], body["result"]);
    assert_eq!(again["reported"], true);
    assert_eq!(again["odoo_job_id"], 70_001);
    // the poll
    let (status, polled) = get(&lan, Some(KEY3), &format!("/v1/jobs/3/{uuid}")).await;
    assert_eq!(status, 200);
    assert_eq!(polled["state"], "done");
    assert_eq!(polled["replay"], false);
    assert_eq!(polled["result"], body["result"]);
    // the same client_uuid naming another operation
    let mut other = receipt(uuid);
    other["payload"]["data"]["roundingAmount"] = json!(0.01);
    let (status, conflict) = post(&lan, Some(KEY3), &other).await;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["error"]["code"], "client_uuid_conflict");
    assert_eq!(conflict["job"]["state"], "done");
    // health
    let (status, health) = get(&lan, Some(KEY4), "/v1/health").await;
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["unreported"], 0);
    assert!(health["portos"].is_string(), "{health}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    lan.run.stop().await;
    assert_eq!(
        portos.lock().unwrap().post_count(),
        1,
        "exactly one receipt"
    );
    assert_eq!(
        fake.lock().unwrap().report_calls,
        1,
        "reported exactly once"
    );
}

#[tokio::test]
async fn test_lan_auth_and_refusals_before_any_device_contact() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let cfg = lan_config(&url);
    let bridge_key = cfg.api_key.clone();
    let lan = start_lan(
        cfg,
        Arc::clone(&journal),
        portos_registry(&journal, &purl, Duration::from_secs(2)),
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let job = receipt("refused-1");

    for key in [
        None,
        Some("wrong-key-0123456789abcdefghijklmnop"),
        Some(bridge_key.as_str()),
    ] {
        let (status, body) = post(&lan, key, &job).await;
        assert_eq!(status, 401, "{key:?}: {body}");
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    let (status, body) = post(&lan, Some(KEY4), &job).await;
    assert_eq!(status, 403, "till 4's key for till 3: {body}");
    assert_eq!(body["error"]["code"], "wrong_kasa");
    let (status, body) = post_raw(&lan, Some(KEY3), "{nope".into()).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "bad_request");
    let mut card = receipt("refused-2");
    card["job_type"] = json!("card.purchase");
    card["payload"] = json!({"amount": 12.4, "currency": "EUR"});
    let (status, body) = post(&lan, Some(KEY3), &card).await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "unsupported");
    let mut print = receipt("refused-print-text");
    print["job_type"] = json!("fiscal.print_text");
    print["payload"] = json!({"title": "X", "lines": ["a"], "cut": true});
    let (status, printed) = post(&lan, Some(KEY3), &print).await;
    assert_eq!(status, 422, "{printed}");
    assert_eq!(printed["error"]["code"], "unsupported");
    assert!(
        body["error"]["text"]
            .as_str()
            .unwrap()
            .contains("nothing ran"),
        "{body}"
    );
    let mut bad = receipt("refused-3");
    bad["payload"]["data"]["cashRegisterCode"] = Value::Null;
    let (status, body) = post(&lan, Some(KEY3), &bad).await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "bad_payload", "{body}");

    // GET: the same keys, the till in the path
    let (status, _) = get(&lan, None, "/v1/jobs/3/refused-3").await;
    assert_eq!(status, 401);
    let (status, _) = get(&lan, Some(KEY4), "/v1/jobs/3/refused-3").await;
    assert_eq!(status, 403);
    let (status, body) = get(&lan, Some(KEY3), "/v1/jobs/3/refused-3").await;
    assert_eq!(status, 404, "a refused job was never journaled: {body}");
    assert_eq!(body["error"]["code"], "not_found");
    let (status, _) = get(&lan, Some(KEY3), "/v1/jobs/x/refused-3").await;
    assert_eq!(status, 400);
    let (status, _) = get(&lan, Some(&bridge_key), "/v1/health").await;
    assert_eq!(status, 401, "the Odoo key is no LAN key");

    assert_eq!(portos.lock().unwrap().post_count(), 0);
    assert!(
        portos.lock().unwrap().lookups.is_empty(),
        "Portos was never asked"
    );
    for uuid in ["refused-1", "refused-2", "refused-3"] {
        assert_eq!(lan_state(&journal, uuid), None, "{uuid}");
    }
    // a refused client_uuid can be sent again, corrected
    let (status, body) = post(&lan, Some(KEY3), &receipt("refused-3")).await;
    assert_eq!(
        (status, body["state"].as_str()),
        (200, Some("done")),
        "{body}"
    );
    lan.run.stop().await;
}

#[tokio::test]
async fn test_lost_portos_answer_and_restart_reconciled_exactly_one_post() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().register = Register::StoreThenHang(60_000);
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let uuid = "lost-answer-1";

    // first process: the POST is stored by Portos, its answer never comes
    let journal = journal_in(dir.path());
    let first = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        portos_registry(&journal, &purl, Duration::from_secs(30)),
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let https = first.https.clone();
    let base = first.base.clone();
    let in_flight = tokio::spawn(async move {
        https
            .post(format!("{base}/v1/jobs"))
            .bearer_auth(KEY3)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(receipt(uuid).to_string())
            .send()
            .await
    });
    eventually("Portos stored the receipt", || {
        portos.lock().unwrap().post_count() == 1
    })
    .await;
    assert_eq!(lan_state(&journal, uuid), Some(JobState::Executing));
    // the process dies mid-job
    first.run.stop().await;
    in_flight.abort();
    drop(journal);

    // second process, same journal file: restart recovery, reconcile
    let journal = journal_in(dir.path());
    let second = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        portos_registry(&journal, &purl, Duration::from_secs(2)),
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    eventually("reconciled done", || {
        lan_state(&journal, uuid) == Some(JobState::Done)
    })
    .await;
    let (status, body) = get(&second, Some(KEY3), &format!("/v1/jobs/3/{uuid}")).await;
    assert_eq!(
        (status, body["state"].as_str()),
        (200, Some("done")),
        "{body}"
    );
    eventually("reported", || fake.lock().unwrap().reports.len() == 1).await;
    second.run.stop().await;
    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 1, "never POSTed twice");
    assert!(p.lookups.len() >= 2, "looked up again by the reconcile");
}

#[tokio::test]
async fn test_lan_receipt_later_handed_out_by_odoo_is_one_receipt() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        portos_registry(&journal, &purl, Duration::from_secs(2)),
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    // the fake Odoo's job 900 carries client_uuid "uuid-900"
    let (status, body) = post(&lan, Some(KEY3), &receipt("uuid-900")).await;
    assert_eq!(
        (status, body["state"].as_str()),
        (200, Some("done")),
        "{body}"
    );
    fake.lock()
        .unwrap()
        .queue(900, "fiscal.cash_register", sale());
    eventually("Odoo job 900 done", || final_state(&fake, 900) == St::Done).await;
    lan.run.stop().await;
    let p = portos.lock().unwrap();
    assert_eq!(
        p.post_count(),
        1,
        "the Odoo job found the LAN receipt by externalId"
    );
    assert_eq!(p.lookups.len(), 2);
    let ack = fake.lock().unwrap().acks_for(900);
    assert_eq!(ack.len(), 1);
    assert_eq!(ack[0]["result"]["device"], "portos");
}

/// A test-device registry (E2E's executor) and a journal in `dir`.
fn test_device_setup(dir: &Path) -> (Arc<Journal>, ExecutorRegistry, Arc<TestDevice>) {
    let (journal, device) = open_files(dir);
    let registry = test_registry(&device);
    (journal, registry, device)
}

#[tokio::test]
async fn test_reporter_retries_until_odoo_answers_and_stays_idempotent() {
    let fake = new_fake();
    {
        let mut f = fake.lock().unwrap();
        f.fail_reports = 3;
        f.lose_report_responses = 1;
    }
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let (journal, registry, device) = test_device_setup(dir.path());
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let (status, body) = post(&lan, Some(KEY3), &scripted("rep-1", "done")).await;
    assert_eq!(
        (status, body["state"].as_str()),
        (200, Some("done")),
        "{body}"
    );
    assert_eq!(body["result"]["device"], "test");
    eventually("reported", || {
        journal
            .lan_job(3, "rep-1")
            .unwrap()
            .is_some_and(|j| j.reported)
    })
    .await;
    let row = journal.lan_job(3, "rep-1").unwrap().unwrap();
    assert_eq!(
        row.report_note.as_deref(),
        Some("existing"),
        "the lost answer's retry"
    );
    assert_eq!(row.odoo_job_id, Some(70_001));
    tokio::time::sleep(Duration::from_millis(300)).await;
    lan.run.stop().await;
    let f = fake.lock().unwrap();
    assert_eq!(f.reports.len(), 1, "stored once");
    assert_eq!(f.report_calls, 5, "3 x 502, 1 lost answer, 1 accepted");
    assert_eq!(
        device.runs(row.job_id()).unwrap(),
        1,
        "the device ran it once"
    );
}

#[tokio::test]
async fn test_a_refused_report_is_final_and_a_failed_job_is_reported_failed() {
    let fake = new_fake();
    fake.lock().unwrap().refuse_reports = Some("client_uuid_conflict");
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let (journal, registry, _device) = test_device_setup(dir.path());
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let (status, body) = post(&lan, Some(KEY3), &scripted("ref-1", "failed")).await;
    assert_eq!(status, 200, "a failed job is a final answer: {body}");
    assert_eq!(body["state"], "failed");
    assert_eq!(body["error"]["code"], "declined");
    eventually("journaled as refused", || {
        journal
            .lan_job(3, "ref-1")
            .unwrap()
            .is_some_and(|j| j.reported)
    })
    .await;
    let note = journal
        .lan_job(3, "ref-1")
        .unwrap()
        .unwrap()
        .report_note
        .unwrap();
    assert!(note.starts_with("refused: "), "{note}");
    assert!(note.contains("client_uuid_conflict"), "{note}");
    {
        // the refused report carried the failure
        let f = fake.lock().unwrap();
        assert_eq!(f.refused_reports.len(), 1);
        let sent = &f.refused_reports[0];
        assert_eq!(sent["client_uuid"], "ref-1");
        assert_eq!(sent["state"], "failed");
        assert_eq!(sent["error"]["code"], "declined");
        assert_eq!(sent["result"], Value::Null);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // even once Odoo would accept it, a refused report is not sent again
    fake.lock().unwrap().refuse_reports = None;
    tokio::time::sleep(Duration::from_millis(200)).await;
    lan.run.stop().await;
    let f = fake.lock().unwrap();
    assert_eq!(f.report_calls, 1, "a refusal is never retried");
    assert!(f.reports.is_empty());
}

#[tokio::test]
async fn test_failed_job_report_carries_the_error() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let (journal, registry, _device) = test_device_setup(dir.path());
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let (status, _) = post(&lan, Some(KEY3), &scripted("fail-1", "failed")).await;
    assert_eq!(status, 200);
    eventually("reported", || fake.lock().unwrap().reports.len() == 1).await;
    lan.run.stop().await;
    let r = fake.lock().unwrap().reports[0].clone();
    assert_eq!(r["state"], "failed");
    assert_eq!(r["result"], Value::Null);
    assert_eq!(r["error"]["code"], "declined");
    assert!(
        r["error"]["text"].as_str().unwrap().contains("nothing ran"),
        "{r}"
    );
    assert_eq!(
        r["pos_order_id"],
        Value::Null,
        "an offline sale without an Odoo order"
    );
}

#[tokio::test]
async fn test_an_unknown_outcome_answers_202_then_resolves() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let (journal, registry, _device) = test_device_setup(dir.path());
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    let (status, body) = post(&lan, Some(KEY3), &scripted("unk-1", "indeterminate")).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["state"], "indeterminate");
    assert_eq!(body["error"]["code"], "indeterminate");
    eventually("reconciled done", || {
        lan_state(&journal, "unk-1") == Some(JobState::Done)
    })
    .await;
    let (status, body) = get(&lan, Some(KEY3), "/v1/jobs/3/unk-1").await;
    assert_eq!(
        (status, body["state"].as_str()),
        (200, Some("done")),
        "{body}"
    );
    assert_eq!(body["result"]["via"], "reconcile");
    eventually("reported", || fake.lock().unwrap().reports.len() == 1).await;
    lan.run.stop().await;
    assert_eq!(fake.lock().unwrap().reports[0]["state"], "done");
}

#[tokio::test]
async fn test_concurrent_identical_posts_run_the_job_once() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let gate = GateDevice::new(DeviceKind::Fiscal);
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::clone(&gate) as Arc<dyn DeviceExecutor>)
        .unwrap();
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        timing(),
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    // a till retrying in a hurry: the same job four times at once, while
    // the device still holds the first
    let job = scripted("twin-1", "done");
    let release = async {
        eventually("the job holds the device", || {
            gate.started.lock().unwrap().len() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        gate.gate.add_permits(1);
    };
    let (a, b, c, d, ()) = tokio::join!(
        post(&lan, Some(KEY3), &job),
        post(&lan, Some(KEY3), &job),
        post(&lan, Some(KEY3), &job),
        post(&lan, Some(KEY3), &job),
        release,
    );
    let answers = [a, b, c, d];
    for (status, body) in &answers {
        assert_eq!(*status, 200, "{body}");
        assert_eq!(body["state"], "done", "{body}");
        assert_eq!(body["client_uuid"], "twin-1");
    }
    let journaled_by = answers
        .iter()
        .filter(|(_, body)| body["replay"] == false)
        .count();
    assert_eq!(journaled_by, 1, "one call journaled it, three are replays");
    tokio::time::sleep(Duration::from_millis(200)).await;
    lan.run.stop().await;
    let id = journal.lan_job(3, "twin-1").unwrap().unwrap().job_id();
    assert_eq!(
        *gate.started.lock().unwrap(),
        vec![id],
        "the device ran it once"
    );
}

#[tokio::test]
async fn test_lan_and_odoo_jobs_share_one_device_queue_and_a_slow_job_answers_202() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = install_test_cert(dir.path());
    let journal = journal_in(dir.path());
    let gate = GateDevice::new(DeviceKind::Fiscal);
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::clone(&gate) as Arc<dyn DeviceExecutor>)
        .unwrap();
    let short_wait = Timing {
        lan_answer_wait: Duration::from_millis(300),
        ..timing()
    };
    let lan = start_lan(
        lan_config(&url),
        Arc::clone(&journal),
        registry,
        short_wait,
        dir.path(),
        &ca,
        "127.0.0.1:1",
    );
    // the LAN job holds the device, the call answers 202 after the wait
    let (status, body) = post(&lan, Some(KEY3), &scripted("gate-1", "done")).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["state"], "executing");
    // an Odoo job for the same device waits behind it
    fake.lock()
        .unwrap()
        .queue(901, "fiscal.cash_register", json!({}));
    eventually("Odoo job 901 claimed", || {
        final_state(&fake, 901) == St::Claimed
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.started.lock().unwrap().len(), 1, "one job at a time");
    gate.gate.add_permits(2);
    eventually("both done", || {
        final_state(&fake, 901) == St::Done && lan_state(&journal, "gate-1") == Some(JobState::Done)
    })
    .await;
    lan.run.stop().await;
    let lan_id = journal.lan_job(3, "gate-1").unwrap().unwrap().job_id();
    assert_eq!(*gate.started.lock().unwrap(), vec![lan_id, 901]);
    assert_eq!(
        gate.max_running.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}
