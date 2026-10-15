//! The Portos executor against a REAL Portos eKasa API (#100) — InMemory
//! storage + Playground (integration) environment ONLY, installed fresh by the
//! CI job `portos-live` on a GitHub-hosted `windows-latest` runner. Never run
//! against a store's Portos: every test here registers receipts.
//!
//! Compiled only with `--features portos-live`:
//! `cargo test -p devbridge-client --features portos-live --test device_jobs portos_live -- --test-threads=1`
//! Env: `DEVBRIDGE_PORTOS_URL` (default `http://localhost:3010`) and
//! `DEVBRIDGE_PORTOS_CASH_REGISTER_CODE` (the InMemory identity's ORP code,
//! required — the CI job reads it from `GET /api/v1/identities`).
//!
//! A "crash between POST and answer" is a real one: a TCP proxy in front of
//! Portos forwards the register POST, lets Portos answer, and then drops (or
//! holds) that answer — Portos has registered the receipt, devbridge never
//! hears it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use devbridge_client::device_jobs::api::{DeviceJob, parse_job};
use devbridge_client::device_jobs::executor::{
    DeviceExecutor, ExecutorRegistry, Outcome, Reconciliation,
};
use devbridge_client::device_jobs::journal::Journal;
use devbridge_client::device_jobs::portos::http::{PortosHttp, Reply};
use devbridge_client::device_jobs::portos::{PortosConfig, PortosExecutor, PortosTiming};

use super::fake::*;

fn portos_url() -> String {
    std::env::var("DEVBRIDGE_PORTOS_URL").unwrap_or_else(|_| "http://localhost:3010".to_string())
}

fn orp() -> String {
    std::env::var("DEVBRIDGE_PORTOS_CASH_REGISTER_CODE").expect(
        "DEVBRIDGE_PORTOS_CASH_REGISTER_CODE must name the InMemory identity's cash register code",
    )
}

/// A client_uuid unique across runs of the same (InMemory) Portos.
fn unique(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4().simple())
}

fn executor(dir: &Path, base: &str, register: Duration) -> PortosExecutor {
    PortosExecutor::open(
        PortosConfig {
            default_url: base.to_string(),
            kasy: Default::default(),
            timing: PortosTiming {
                register,
                ..PortosTiming::default()
            },
        },
        &dir.join("portos-sent.db"),
    )
    .unwrap()
}

fn job(id: i64, job_type: &str, client_uuid: &str, payload: Value) -> DeviceJob {
    parse_job(
        &json!({"id": id, "type": job_type, "client_uuid": client_uuid,
        "kasa": {"id": 3, "name": "CI"}, "payload": payload}),
    )
    .unwrap()
}

fn sale(orp: &str) -> Value {
    json!({
        "data": {
            "cashRegisterCode": orp,
            "items": [
                {"type": "Positive", "name": "Chlieb kváskový 500 g", "quantity": {"amount": 2, "unit": "ks"},
                 "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0},
                {"type": "Positive", "name": "Rožok", "quantity": {"amount": 3},
                 "unitPrice": 0.12, "price": 0.36, "vatRate": 5.0}
            ],
            "payments": [{"name": "Hotovosť", "amount": 3.0}],
            "roundingAmount": 0.06,
            "headerText": "devbridge CI #100 - InMemory/Playground"
        },
        "print": {"printerName": "pdf"}
    })
}

/// The receipt number Portos will give the next receipt (counts receipts).
async fn next_number(orp: &str) -> i64 {
    let reply = PortosHttp::new()
        .unwrap()
        .get(
            &portos_url(),
            "/api/v1/requests/receipts/next_number",
            &[("cashRegisterCode".to_string(), orp.to_string())],
            Duration::from_secs(15),
        )
        .await;
    match reply {
        Reply::Answer { status: 200, body } => body["receiptNumber"]
            .as_i64()
            .unwrap_or_else(|| panic!("next_number: {body}")),
        other => panic!("next_number: {other:?}"),
    }
}

/// Wait up to `secs` for `cond` (a real Portos may take seconds per call).
async fn wait_until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(secs),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The loop timing for a real Portos: no early `indeterminate` report, no
/// status probe storm.
fn live_timing() -> devbridge_client::device_jobs::Timing {
    let mut t = timing();
    t.execute_guard = Duration::from_secs(120);
    t.status_refresh = Duration::from_secs(5);
    t
}

fn done(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Done(result) => result,
        other => panic!("not done: {other:?}"),
    }
}

/// A journal entry of `job` as it stands after a lost answer.
fn entry_of(job: &DeviceJob) -> devbridge_client::device_jobs::journal::JournalEntry {
    let journal = Journal::open_in_memory().unwrap();
    journal.insert_received(job).unwrap();
    journal.get(job.id).unwrap().unwrap()
}

// ── the lossy proxy ────────────────────────────────────────────────────────

struct LossyProxy {
    url: String,
    /// Register answers Portos produced and the proxy withheld.
    withheld: Arc<AtomicUsize>,
}

/// Read one HTTP request (head + content-length body).
async fn read_request(sock: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buf = vec![0u8; 16384];
    loop {
        let n = sock.read(&mut buf).await.unwrap();
        request.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        if let Some(head_end) = text.find("\r\n\r\n") {
            let len = text[..head_end]
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= head_end + 4 + len {
                return request;
            }
        }
        assert!(n > 0, "client closed mid-request");
    }
}

/// Forward every request to Portos (with `connection: close`, so its answer
/// ends at EOF). Answers of register POSTs are withheld: dropped, or held
/// open forever when `hold`.
async fn lossy_proxy(hold: bool) -> LossyProxy {
    let target = portos_url()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let withheld = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&withheld);
    tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let target = target.clone();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let request = read_request(&mut client).await;
                let head = String::from_utf8_lossy(&request).to_string();
                let first_line = head.lines().next().unwrap_or("").to_string();
                let line_end = head.find("\r\n").unwrap();
                let mut forwarded = request[..line_end + 2].to_vec();
                forwarded.extend_from_slice(b"connection: close\r\n");
                forwarded.extend_from_slice(&request[line_end + 2..]);
                let mut upstream = TcpStream::connect(&target).await.unwrap();
                upstream.write_all(&forwarded).await.unwrap();
                let mut answer = Vec::new();
                upstream.read_to_end(&mut answer).await.unwrap();
                let register = first_line.starts_with("POST /api/v1/requests/receipts/")
                    && !first_line.starts_with("POST /api/v1/requests/receipts/print_copy");
                if register {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if hold {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                    return; // the answer is lost
                }
                client.write_all(&answer).await.unwrap();
                client.shutdown().await.ok();
            });
        }
    });
    LossyProxy { url, withheld }
}

// ── the tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn live_portos_answers_and_reports_its_status() {
    let reply = PortosHttp::new()
        .unwrap()
        .get(
            &portos_url(),
            "/api/v1/product/info",
            &[],
            Duration::from_secs(15),
        )
        .await;
    println!("product/info: {reply:?}");
    assert!(
        matches!(reply, Reply::Answer { status: 200, .. }),
        "{reply:?}"
    );
    let dir = tempfile::tempdir().unwrap();
    let e = executor(dir.path(), &portos_url(), Duration::from_secs(60));
    assert_eq!(e.status(), "unknown");
    e.refresh_status().await;
    let status = e.status();
    println!("heartbeat status: portos={status}");
    assert!(
        status == "ok" || status.starts_with("degraded("),
        "{status}"
    );
}

#[tokio::test]
async fn live_cash_register_is_registered_exactly_once() {
    let orp = orp();
    let dir = tempfile::tempdir().unwrap();
    let e = executor(dir.path(), &portos_url(), Duration::from_secs(60));
    let uuid = unique("sale");
    let before = next_number(&orp).await;
    let first = done(
        e.execute(&job(1, "fiscal.cash_register", &uuid, sale(&orp)))
            .await,
    );
    println!("registered: {first}");
    assert_eq!(first["via"], "register");
    assert_eq!(first["external_id"], format!("dbj-3-{uuid}"));
    assert!(
        first["state"] == "online" || first["state"] == "offline",
        "{first}"
    );
    assert!(
        first["okp"].as_str().is_some_and(|o| !o.is_empty()),
        "{first}"
    );
    assert!(first["receipt_number"].is_number(), "{first}");
    assert_eq!(first["cash_register_code"], orp.as_str());
    assert_eq!(first["receipt_type"], "CashRegister");

    // the same operation handed out again (a new job, same client_uuid):
    // found by its externalId, NOT registered again
    let again = done(
        e.execute(&job(2, "fiscal.cash_register", &uuid, sale(&orp)))
            .await,
    );
    assert_eq!(again["via"], "lookup");
    assert_eq!(again["receipt_number"], first["receipt_number"]);
    assert_eq!(again["okp"], first["okp"]);
    assert_eq!(next_number(&orp).await, before + 1, "exactly one receipt");

    // the longest client_uuid the Odoo contract allows fits Portos' externalId
    let unique_long = format!("{:L<64}", unique("x"));
    assert_eq!(unique_long.len(), 64);
    let r = done(
        e.execute(&job(3, "fiscal.cash_register", &unique_long, sale(&orp)))
            .await,
    );
    assert_eq!(r["external_id"], format!("dbj-3-{unique_long}"));
    assert_eq!(next_number(&orp).await, before + 2);
}

#[tokio::test]
async fn live_every_fiscal_type() {
    let orp = orp();
    let dir = tempfile::tempdir().unwrap();
    let e = executor(dir.path(), &portos_url(), Duration::from_secs(60));
    let mut invalid = sale(&orp);
    invalid["data"]["receiptType"] = json!("Invalid");
    for (id, job_type, payload, receipt_type) in [
        (11, "fiscal.invalid", invalid, "Invalid"),
        (
            12,
            "fiscal.deposit",
            json!({"data": {"cashRegisterCode": orp, "amount": 50.0}}),
            "Deposit",
        ),
        (
            13,
            "fiscal.withdraw",
            json!({"data": {"cashRegisterCode": orp, "amount": -20.5}}),
            "Withdraw",
        ),
    ] {
        let result = done(e.execute(&job(id, job_type, &unique("t"), payload)).await);
        println!("{job_type}: {result}");
        assert_eq!(result["receipt_type"], receipt_type, "{job_type}");
        assert_eq!(result["via"], "register");
        assert_eq!(
            result["external_id"]
                .as_str()
                .map(|x| x.starts_with("dbj-3-t-")),
            Some(true)
        );
    }

    // a copy of the cash register receipt, by its devbridge externalId
    let uuid = unique("copy");
    let original = done(
        e.execute(&job(14, "fiscal.cash_register", &uuid, sale(&orp)))
            .await,
    );
    let before = next_number(&orp).await;
    let copy = e
        .execute(&job(
            15,
            "fiscal.print_copy",
            &unique("c"),
            json!({"cashRegisterCode": orp, "externalId": original["external_id"]}),
        ))
        .await;
    println!("print_copy: {copy:?}");
    let copy = done(copy);
    assert!(copy["printed"].is_boolean(), "{copy}");
    let missing = e
        .execute(&job(
            16,
            "fiscal.print_copy",
            &unique("c"),
            json!({"cashRegisterCode": orp, "externalId": "dbj-3-never-registered"}),
        ))
        .await;
    println!("print_copy of an unknown receipt: {missing:?}");
    assert!(
        matches!(&missing, Outcome::Failed { code, .. } if code == "receipt_not_found"),
        "{missing:?}"
    );

    // the drawer: no fiscal record; whatever this Portos' printer answers
    // must follow the drawer table (never unreachable / bad payload)
    let drawer = job(17, "fiscal.open_drawer", &unique("d"), json!({}));
    let outcome = e.execute(&drawer).await;
    println!("open_drawer: {outcome:?}");
    match outcome {
        Outcome::Done(result) => assert!(result.get("opened").is_some(), "{result}"),
        Outcome::Indeterminate(_) => match e.reconcile(&entry_of(&drawer)).await {
            Reconciliation::Failed { code, .. } => assert_eq!(code, "outcome_unknown_retry_safe"),
            other => panic!("{other:?}"),
        },
        Outcome::Failed { code, text } => {
            assert!(
                [
                    "portos_refused",
                    "portos_endpoint_missing",
                    "portos_validation"
                ]
                .contains(&code.as_str()),
                "{code}: {text}"
            )
        }
    }
    assert_eq!(
        next_number(&orp).await,
        before,
        "copy and drawer register nothing"
    );
}

#[tokio::test]
async fn live_validation_refusal_registers_nothing() {
    let orp = orp();
    let dir = tempfile::tempdir().unwrap();
    let e = executor(dir.path(), &portos_url(), Duration::from_secs(60));
    let before = next_number(&orp).await;
    let mut wrong = sale(&orp);
    // price != unitPrice * quantity: shape is fine, Portos refuses it
    wrong["data"]["items"][0]["price"] = json!(9.99);
    let outcome = e
        .execute(&job(21, "fiscal.cash_register", &unique("bad"), wrong))
        .await;
    println!("refused: {outcome:?}");
    match &outcome {
        Outcome::Failed { code, text } => {
            assert_eq!(code, "portos_validation", "{text}");
            assert!(text.ends_with("nothing was registered"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(next_number(&orp).await, before);
}

#[tokio::test]
async fn live_lost_answer_is_reconciled_by_external_id_never_registered_twice() {
    let orp = orp();
    let dir = tempfile::tempdir().unwrap();
    let proxy = lossy_proxy(false).await;
    let e = executor(dir.path(), &proxy.url, Duration::from_secs(60));
    let before = next_number(&orp).await;
    let receipt = job(31, "fiscal.cash_register", &unique("lost"), sale(&orp));
    let outcome = e.execute(&receipt).await;
    println!("lost answer: {outcome:?}");
    assert!(matches!(outcome, Outcome::Indeterminate(_)), "{outcome:?}");
    assert_eq!(
        proxy.withheld.load(Ordering::SeqCst),
        1,
        "Portos answered, devbridge never heard it"
    );
    assert_eq!(next_number(&orp).await, before + 1, "Portos registered it");

    match e.reconcile(&entry_of(&receipt)).await {
        Reconciliation::Done(result) => {
            println!("reconciled: {result}");
            assert_eq!(result["via"], "reconcile");
            assert_eq!(
                result["external_id"],
                format!("dbj-3-{}", receipt.client_uuid)
            );
            assert!(
                result["okp"].as_str().is_some_and(|o| !o.is_empty()),
                "{result}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        next_number(&orp).await,
        before + 1,
        "never registered twice"
    );

    // a receipt devbridge never sent: Portos has none → provably nothing ran
    let never = job(32, "fiscal.cash_register", &unique("never"), sale(&orp));
    match e.reconcile(&entry_of(&never)).await {
        Reconciliation::Failed { code, .. } => assert_eq!(code, "not_executed"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn live_crash_between_post_and_answer_through_the_device_job_loop() {
    let orp = orp();
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let proxy = lossy_proxy(true).await;
    let before = next_number(&orp).await;
    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(executor(
            dir.path(),
            &proxy.url,
            Duration::from_secs(60),
        )))
        .unwrap();
    let first = start(
        config(&url, 1),
        Arc::clone(&journal),
        registry,
        live_timing(),
    );
    fake.lock()
        .unwrap()
        .queue(41, "fiscal.cash_register", sale(&orp));
    wait_until("Portos registered receipt 41", 90, || {
        proxy.withheld.load(Ordering::SeqCst) == 1
    })
    .await;
    // the process dies while its POST waits for the answer
    first.stop().await;
    drop(journal);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(fake.lock().unwrap().acks_for(41).is_empty());

    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(executor(
            dir.path(),
            &portos_url(),
            Duration::from_secs(60),
        )))
        .unwrap();
    let second = start(
        config(&url, 1),
        Arc::clone(&journal),
        registry,
        live_timing(),
    );
    wait_until("receipt 41 reconciled done", 90, || {
        final_state(&fake, 41) == St::Done
    })
    .await;
    second.stop().await;

    let acks = fake.lock().unwrap().acks_for(41);
    println!("acks: {acks:?}");
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["result"]["via"], "reconcile");
    assert_eq!(acks[0]["result"]["external_id"], "dbj-3-uuid-41");
    assert_eq!(next_number(&orp).await, before + 1, "exactly one receipt");
    assert_clean(&fake);
}
