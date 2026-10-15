//! The Portos eKasa executor (#100) in the full device-job loop against the
//! fake Odoo and a fake Portos: a receipt is registered exactly once —
//! lookup before register, a lost answer / a crash between POST and answer
//! reconciled by the devbridge externalId, never POSTed twice; refusals that
//! prove nothing ran; per-Portos serial queues; the local queue expiry; the
//! `portos=` heartbeat status.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use devbridge_client::device_jobs::api::parse_job;
use devbridge_client::device_jobs::executor::{DeviceExecutor, ExecutorRegistry, Reconciliation};
use devbridge_client::device_jobs::journal::{JobState, Journal};
use devbridge_client::device_jobs::portos::payload::external_id;
use devbridge_client::device_jobs::portos::{PortosConfig, PortosExecutor, PortosTiming};

use super::fake::*;
use super::fake_portos::*;

const ORP: &str = "88812345678900001";

fn portos_timing(register: Duration) -> PortosTiming {
    PortosTiming {
        lookup: Duration::from_secs(2),
        register,
        control: Duration::from_millis(300),
        probe: Duration::from_millis(300),
    }
}

/// A Portos executor on `journal`: `default_url` for every till except `kasy`.
fn portos_at(
    journal: &Arc<Journal>,
    default_url: &str,
    kasy: &[(i64, &str)],
    register: Duration,
) -> PortosExecutor {
    PortosExecutor::new(
        PortosConfig {
            default_url: default_url.to_string(),
            kasy: kasy.iter().map(|(k, u)| (*k, u.to_string())).collect(),
            timing: portos_timing(register),
        },
        Arc::clone(journal),
    )
    .unwrap()
}

fn registry_of(portos: PortosExecutor) -> ExecutorRegistry {
    let mut r = ExecutorRegistry::default();
    r.register(Arc::new(portos)).unwrap();
    r
}

fn journal_in(dir: &Path) -> Arc<Journal> {
    Arc::new(Journal::open(&dir.join("device-journal.db")).unwrap())
}

fn sale() -> Value {
    json!({
        "data": {
            "cashRegisterCode": ORP,
            "items": [{"type": "Positive", "name": "Chlieb kváskový", "quantity": {"amount": 2, "unit": "ks"},
                       "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0}],
            "payments": [{"name": "Hotovosť", "amount": 2.6}],
            "roundingAmount": 0.02
        },
        "print": {"printerName": "pdf"}
    })
}

/// A query string as the fake Portos records it.
fn query(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The devbridge externalId of fake-Odoo job `id` on till 3.
fn ext(id: i64) -> String {
    external_id(3, &format!("uuid-{id}"))
}

/// The single ack of `id` (panics when there is not exactly one).
fn only_ack(fake: &Shared, id: i64) -> Value {
    let acks = fake.lock().unwrap().acks_for(id);
    assert_eq!(acks.len(), 1, "{id}: {acks:?}");
    acks[0].clone()
}

#[tokio::test]
async fn test_cash_register_is_looked_up_then_registered_once_and_acked_done() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock()
        .unwrap()
        .queue(501, "fiscal.cash_register", sale());

    eventually("receipt 501 done in Odoo", || {
        final_state(&fake, 501) == St::Done
    })
    .await;
    eventually("an ok heartbeat", || {
        fake.lock()
            .unwrap()
            .heartbeats
            .iter()
            .any(|h| h["device_status"] == "portos=ok; terminal=none")
    })
    .await;
    run.stop().await;

    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 1);
    let (kind, body) = &p.posts[0];
    assert_eq!(kind, "cash_register");
    assert_eq!(body["request"]["externalId"], ext(501));
    assert_eq!(body["request"]["data"], sale()["data"]);
    assert_eq!(body["print"], json!({"printerName": "pdf"}));
    assert_eq!(p.lookups.len(), 1, "one lookup before the POST");
    assert_eq!(p.lookups[0].get("ExternalId"), Some(&ext(501)));
    assert_eq!(
        p.lookups[0].get("CashRegisterCode").map(String::as_str),
        Some(ORP)
    );
    drop(p);
    let ack = only_ack(&fake, 501);
    let result = &ack["result"];
    assert_eq!(result["device"], "portos");
    assert_eq!(result["via"], "register");
    assert_eq!(result["external_id"], ext(501));
    assert_eq!(result["state"], "online");
    assert_eq!(result["is_offline"], false);
    assert_eq!(result["uid"], "O-1");
    assert_eq!(result["okp"], "okp-1");
    assert_eq!(result["receipt_number"], 1);
    assert_eq!(result["cash_register_code"], ORP);
    assert!(
        result.get("items").is_none(),
        "no receipt items in the result"
    );
    assert_eq!(journal.get(501).unwrap().unwrap().state, JobState::Done);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_every_fiscal_type_against_portos() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().register = Register::Offline;
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        let mut invalid = sale();
        invalid["data"]["receiptType"] = json!("Invalid");
        s.queue(511, "fiscal.invalid", invalid);
        s.queue(
            512,
            "fiscal.deposit",
            json!({"data": {"cashRegisterCode": ORP, "amount": 100.0}}),
        );
        s.queue(
            513,
            "fiscal.withdraw",
            json!({"data": {"cashRegisterCode": ORP, "amount": -40.5}}),
        );
        s.queue(
            514,
            "fiscal.print_copy",
            json!({"cashRegisterCode": ORP, "externalId": ext(512)}),
        );
        s.queue(515, "fiscal.open_drawer", json!({"pin": "Pin5"}));
    }
    eventually("all five done", || {
        (511..=515).all(|id| final_state(&fake, id) == St::Done)
    })
    .await;
    run.stop().await;

    let p = portos.lock().unwrap();
    let kinds: Vec<&str> = p.posts.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, vec!["invalid", "deposit", "withdraw"]);
    assert_eq!(
        p.posts[1].1["request"]["data"],
        json!({"cashRegisterCode": ORP, "amount": 100.0})
    );
    assert!(
        p.posts[1].1.get("print").is_none(),
        "no print context given, none sent"
    );
    assert_eq!(
        p.copy_calls,
        vec![query(&[
            ("CashRegisterCode", ORP),
            ("ExternalId", ext(512).as_str())
        ])]
    );
    assert_eq!(p.drawer_calls, vec![query(&[("pin", "Pin5")])]);
    drop(p);
    for id in [511, 512, 513] {
        let result = only_ack(&fake, id)["result"].clone();
        assert_eq!(result["state"], "offline", "{id}");
        assert_eq!(result["is_offline"], true, "{id}");
        assert_eq!(result["uid"], Value::Null, "{id}");
        assert_eq!(result["external_id"], ext(id), "{id}");
    }
    assert_eq!(only_ack(&fake, 512)["result"]["receipt_type"], "Deposit");
    assert_eq!(
        only_ack(&fake, 514)["result"],
        json!({"device": "portos", "printed": true, "printer_ready": true})
    );
    assert_eq!(
        only_ack(&fake, 515)["result"],
        json!({"device": "portos", "opened": true, "printer_ready": true})
    );
    assert_clean(&fake);
}

#[tokio::test]
async fn test_lost_answer_is_indeterminate_then_reconciled_never_posted_twice() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().register = Register::StoreThenHang(5000);
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_millis(300)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock()
        .unwrap()
        .queue(521, "fiscal.cash_register", sale());

    eventually("receipt 521 done by its late ack", || {
        final_state(&fake, 521) == St::Done
    })
    .await;
    run.stop().await;

    assert_eq!(
        portos.lock().unwrap().post_count(),
        1,
        "POSTed exactly once"
    );
    let acks = fake.lock().unwrap().acks_for(521);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    let reason = acks[0]["error"].as_str().unwrap();
    assert!(
        reason.contains("no answer from Portos after the receipt was sent"),
        "{reason}"
    );
    assert!(
        reason.contains(&format!("reconciled by externalId {}", ext(521))),
        "{reason}"
    );
    assert_eq!(acks[1]["result"]["via"], "reconcile");
    assert_eq!(acks[1]["result"]["external_id"], ext(521));
    assert_eq!(acks[1]["result"]["receipt_number"], 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_crash_between_post_and_answer_restart_reconciles_by_external_id() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    // Portos stores the receipt, then the answer never comes back
    portos.lock().unwrap().register = Register::StoreThenHang(60_000);
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let first_registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(30)));
    let first = start(
        config(&url, 1),
        Arc::clone(&journal),
        first_registry,
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(531, "fiscal.cash_register", sale());
    eventually("Portos stored receipt 531", || {
        portos.lock().unwrap().receipts.contains_key(&ext(531))
    })
    .await;
    let in_flight = journal.get(531).unwrap().unwrap();
    assert_eq!(in_flight.state, JobState::Executing);
    assert!(
        in_flight.device_contact_at.is_some(),
        "the device contact is journaled before the POST"
    );
    // the process dies while its POST waits for the answer
    first.stop().await;
    drop(journal);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(fake.lock().unwrap().acks_for(531).is_empty());

    // restarted: a fresh handle on the same journal (it holds the device contact)
    let journal = journal_in(dir.path());
    let second_registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(30)));
    let second = start(
        config(&url, 1),
        Arc::clone(&journal),
        second_registry,
        timing(),
    );
    eventually("receipt 531 reconciled done", || {
        final_state(&fake, 531) == St::Done
    })
    .await;
    eventually("journal acked", || {
        journal.get(531).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    second.stop().await;

    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 1, "never POSTed again after the restart");
    assert_eq!(p.receipts.len(), 1);
    drop(p);
    let ack = only_ack(&fake, 531);
    assert_eq!(ack["result"]["via"], "reconcile");
    assert_eq!(ack["result"]["external_id"], ext(531));
    assert_eq!(ack["result"]["okp"], "okp-1");
    assert_clean(&fake);
}

#[tokio::test]
async fn test_receipt_already_in_portos_is_done_without_a_post() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().preregister(&ext(541));
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock()
        .unwrap()
        .queue(541, "fiscal.cash_register", sale());
    eventually("receipt 541 done", || final_state(&fake, 541) == St::Done).await;
    run.stop().await;

    assert_eq!(
        portos.lock().unwrap().post_count(),
        0,
        "found by externalId - not posted"
    );
    let ack = only_ack(&fake, 541);
    assert_eq!(ack["result"]["via"], "lookup");
    assert_eq!(ack["result"]["receipt_number"], 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_refusals_that_prove_nothing_ran_are_failed() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().register = Register::Refuse(
        400,
        json!({"code": -900, "title": "Validácia zlyhala", "status": 400,
               "errors": {"Request.Data.Items[0].VatRate": ["Sadzba DPH 20,00% nie je platná."]}}),
    );
    let purl = start_portos(Arc::clone(&portos)).await;
    // till 4's Portos does not run at all
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let down = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(
        &journal,
        &purl,
        &[(4, down.as_str())],
        Duration::from_secs(2),
    ));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        s.queue(551, "fiscal.cash_register", sale());
        let mut bad = sale();
        bad["data"]["items"][0]["price"] = json!(2.585);
        s.queue(552, "fiscal.cash_register", bad);
        s.queue_on(
            553,
            "fiscal.deposit",
            json!({"data": {"cashRegisterCode": ORP, "amount": 5}}),
            4,
        );
        s.queue_on(554, "fiscal.open_drawer", json!({}), 4);
    }
    eventually("all four failed", || {
        (551..=554).all(|id| final_state(&fake, id) == St::Failed)
    })
    .await;
    run.stop().await;

    let code = |id| {
        only_ack(&fake, id)["error_code"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let text = |id| only_ack(&fake, id)["error"].as_str().unwrap().to_string();
    assert_eq!(code(551), "portos_validation");
    assert!(
        text(551).contains("Sadzba DPH 20,00% nie je platná."),
        "{}",
        text(551)
    );
    assert!(
        text(551).ends_with("nothing was registered"),
        "{}",
        text(551)
    );
    assert_eq!(code(552), "bad_payload");
    assert_eq!(
        text(552),
        "payload.data.items[0].price must have at most 2 decimal places - Portos was not contacted, nothing ran"
    );
    assert_eq!(code(553), "portos_unreachable");
    assert!(
        text(553).ends_with("the receipt was not sent, nothing ran"),
        "{}",
        text(553)
    );
    assert_eq!(code(554), "portos_unreachable");
    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 1, "only 551 reached Portos");
    assert!(p.receipts.is_empty(), "nothing registered");
    assert_eq!(p.lookups.len(), 1, "the bad payload never reached Portos");
    drop(p);
    assert_eq!(journal.get(552).unwrap().unwrap().state, JobState::Failed);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_print_error_after_registration_and_lost_drawer_answer() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    {
        let mut p = portos.lock().unwrap();
        p.register = Register::StoreThenPrintError;
        p.drawer = Control::Ok(5000); // answers after the 300 ms control timeout
        p.copy = Control::Answer(403, json!({"code": -104, "title": "Doklad sa nenašiel"}));
    }
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        s.queue(561, "fiscal.cash_register", sale());
        s.queue(562, "fiscal.open_drawer", json!({}));
        s.queue(
            563,
            "fiscal.print_copy",
            json!({"cashRegisterCode": ORP, "year": 2026, "month": 10, "receiptNumber": 9}),
        );
    }
    eventually("561 done, 562 + 563 failed", || {
        final_state(&fake, 561) == St::Done
            && final_state(&fake, 562) == St::Failed
            && final_state(&fake, 563) == St::Failed
    })
    .await;
    run.stop().await;

    let acks = fake.lock().unwrap().acks_for(561);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert!(
        acks[0]["error"].as_str().unwrap().contains("code -502"),
        "{acks:?}"
    );
    assert_eq!(acks[1]["result"]["via"], "reconcile");
    let drawer = fake.lock().unwrap().acks_for(562);
    assert_eq!(drawer.len(), 2, "{drawer:?}");
    assert_eq!(drawer[0]["error_code"], "indeterminate");
    assert_eq!(drawer[1]["error_code"], "outcome_unknown_retry_safe");
    assert!(
        drawer[1]["error"]
            .as_str()
            .unwrap()
            .contains("the till may simply retry")
    );
    assert_eq!(only_ack(&fake, 563)["error_code"], "receipt_not_found");
    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 1);
    assert_eq!(
        p.drawer_calls.len(),
        1,
        "devbridge never repeats a drawer by itself"
    );
    assert_eq!(
        p.copy_calls[0],
        query(&[
            ("CashRegisterCode", ORP),
            ("Year", "2026"),
            ("Month", "10"),
            ("ReceiptNumber", "9")
        ])
    );
    drop(p);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_restart_before_the_post_reconciles_to_not_executed() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    // a previous process journaled job 571 and died before it reached Portos
    fake.lock()
        .unwrap()
        .claimed(571, "fiscal.cash_register", sale());
    let job = parse_job(&job_json(&fake.lock().unwrap().jobs[&571], true)).unwrap();
    journal.insert_received(&job).unwrap();
    journal.start_execution(571).unwrap();
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    eventually("job 571 failed", || final_state(&fake, 571) == St::Failed).await;
    run.stop().await;

    let ack = only_ack(&fake, 571);
    assert_eq!(ack["error_code"], "not_executed");
    assert!(
        ack["error"]
            .as_str()
            .unwrap()
            .contains("never sent this receipt to Portos")
    );
    let p = portos.lock().unwrap();
    assert_eq!(p.post_count(), 0);
    assert_eq!(p.lookups.len(), 1);
    assert!(
        p.unprocessed_calls.is_empty(),
        "never sent: no need to ask the queue"
    );
    drop(p);
    assert_clean(&fake);
}

/// A journaled receipt job whose outcome is unknown; `contacted` = its
/// register request had left devbridge.
fn unknown_entry(id: i64, contacted: bool) -> devbridge_client::device_jobs::journal::JournalEntry {
    let journal = Journal::open_in_memory().unwrap();
    let job = parse_job(&json!({"id": id, "type": "fiscal.deposit",
        "client_uuid": format!("uuid-{id}"), "kasa": {"id": 3, "name": "K"},
        "payload": {"data": {"cashRegisterCode": ORP, "amount": 5}}}))
    .unwrap();
    journal.insert_received(&job).unwrap();
    if contacted {
        assert!(journal.mark_device_contact(id).unwrap());
    }
    journal.get(id).unwrap().unwrap()
}

#[tokio::test]
async fn test_reconcile_decision_table() {
    let portos = new_portos();
    let purl = start_portos(Arc::clone(&portos)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = portos_at(&journal, &purl, &[], Duration::from_secs(2));

    // found → done
    portos.lock().unwrap().preregister(&ext(580));
    match e.reconcile(&unknown_entry(580, true)).await {
        Reconciliation::Done(result) => {
            assert_eq!(result["via"], "reconcile");
            assert_eq!(result["external_id"], ext(580));
        }
        other => panic!("{other:?}"),
    }
    // not found, never contacted → failed, nothing ran
    match e.reconcile(&unknown_entry(581, false)).await {
        Reconciliation::Failed { code, text } => {
            assert_eq!(code, "not_executed");
            assert!(text.ends_with("nothing ran"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    // not found, contacted, listed as unprocessed → still unknown, waiting
    portos.lock().unwrap().unprocessed = vec![ext(582)];
    match e.reconcile(&unknown_entry(582, true)).await {
        Reconciliation::StillUnknown(reason) => {
            assert!(reason.contains("as not processed yet"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    // not found, contacted, not listed → still unknown: a person checks
    match e.reconcile(&unknown_entry(583, true)).await {
        Reconciliation::StillUnknown(reason) => {
            assert!(
                reason.contains("a person must check Portos WebAdmin"),
                "{reason}"
            )
        }
        other => panic!("{other:?}"),
    }
    {
        let p = portos.lock().unwrap();
        assert_eq!(
            p.unprocessed_calls.len(),
            2,
            "asked only for the two sent receipts"
        );
        assert_eq!(p.unprocessed_calls[1].get("ExternalId"), Some(&ext(583)));
        assert_eq!(
            p.unprocessed_calls[1]
                .get("CashRegisterCode")
                .map(String::as_str),
            Some(ORP)
        );
        assert_eq!(p.post_count(), 0, "reconcile never registers");
    }
    // Portos answers the lookup with an error → still unknown
    portos.lock().unwrap().lookup_answer =
        Some((500, json!({"code": -400, "title": "Chyba úložiska"})));
    match e.reconcile(&unknown_entry(584, true)).await {
        Reconciliation::StillUnknown(reason) => {
            assert!(
                reason.contains("could not be asked about externalId")
                    && reason.contains("code -400"),
                "{reason}"
            )
        }
        other => panic!("{other:?}"),
    }
    // a lookup answer that is not this receipt → still unknown
    let foreign = portos
        .lock()
        .unwrap()
        .receipt("dbj-3-someone-else", "Deposit", &json!({}), true);
    portos.lock().unwrap().lookup_answer = Some((200, foreign));
    match e.reconcile(&unknown_entry(585, true)).await {
        Reconciliation::StillUnknown(reason) => assert!(reason.contains("unreadable"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // Portos not running → still unknown
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let down = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let offline = portos_at(&journal, &down, &[], Duration::from_secs(2));
    match offline.reconcile(&unknown_entry(586, true)).await {
        Reconciliation::StillUnknown(reason) => {
            assert!(reason.contains("could not be asked"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_each_portos_has_its_own_queue_and_stale_jobs_expire() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let slow = new_portos();
    slow.lock().unwrap().drawer = Control::Gate;
    let slow_url = start_portos(Arc::clone(&slow)).await;
    let fast = new_portos();
    let fast_url = start_portos(Arc::clone(&fast)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let portos = PortosExecutor::new(
        PortosConfig {
            default_url: slow_url.clone(),
            kasy: [(4, fast_url.clone())].into_iter().collect(),
            timing: PortosTiming {
                control: Duration::from_secs(5),
                ..portos_timing(Duration::from_secs(2))
            },
        },
        Arc::clone(&journal),
    )
    .unwrap();
    let mut t = timing();
    t.max_queue_wait = Duration::from_millis(300);
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        registry_of(portos),
        t,
    );
    {
        let mut s = fake.lock().unwrap();
        s.queue_on(591, "fiscal.open_drawer", json!({}), 3); // slow Portos, held by the gate
        s.queue_on(592, "fiscal.cash_register", sale(), 3); // behind it on the SAME Portos
        s.queue_on(593, "fiscal.cash_register", sale(), 4); // another Portos
    }
    eventually(
        "till 4's receipt done while till 3's Portos is busy",
        || final_state(&fake, 593) == St::Done,
    )
    .await;
    assert_ne!(
        final_state(&fake, 591),
        St::Done,
        "the drawer on till 3 is still running"
    );
    // 592 now waits behind the held drawer longer than max_queue_wait
    tokio::time::sleep(Duration::from_millis(500)).await;
    let gate = Arc::clone(&slow.lock().unwrap().gate);
    gate.add_permits(1);
    eventually("591 done, 592 expired", || {
        final_state(&fake, 591) == St::Done && final_state(&fake, 592) == St::Failed
    })
    .await;
    run.stop().await;

    let ack = only_ack(&fake, 592);
    assert_eq!(ack["error_code"], "expired_local");
    assert!(
        ack["error"]
            .as_str()
            .unwrap()
            .ends_with("the device was not contacted, nothing ran"),
        "{ack}"
    );
    assert!(ack["error"].as_str().unwrap().contains(&slow_url), "{ack}");
    assert_eq!(
        slow.lock().unwrap().post_count(),
        0,
        "the expired receipt never reached Portos"
    );
    assert!(slow.lock().unwrap().lookups.is_empty());
    assert_eq!(fast.lock().unwrap().post_count(), 1);
    assert_eq!(
        fast.lock().unwrap().posts[0].1["request"]["externalId"],
        external_id(4, "uuid-593")
    );
    assert_clean(&fake);
}

#[tokio::test]
async fn test_heartbeat_reports_the_portos_state() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let portos = new_portos();
    portos.lock().unwrap().printer = json!({"state": "Error", "paperState": "Empty"});
    let purl = start_portos(Arc::clone(&portos)).await;
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_in(dir.path());
    let registry = registry_of(portos_at(&journal, &purl, &[], Duration::from_secs(2)));
    let run = start(config(&url, 1), journal, registry, timing());
    let sent = |status: &str| {
        let fake = Arc::clone(&fake);
        let status = status.to_string();
        move || {
            fake.lock()
                .unwrap()
                .heartbeats
                .iter()
                .any(|h| h["device_status"] == status.as_str())
        }
    };
    eventually(
        "degraded",
        sent("portos=degraded(printer=Error,paper=Empty); terminal=none"),
    )
    .await;
    portos.lock().unwrap().info_ok = false;
    eventually("offline", sent("portos=offline; terminal=none")).await;
    {
        let mut p = portos.lock().unwrap();
        p.info_ok = true;
        p.printer = json!({"state": "Ready", "paperState": "NearEnd"});
    }
    eventually("ok again", sent("portos=ok; terminal=none")).await;
    run.stop().await;
    let first = fake.lock().unwrap().heartbeats[0]["device_status"].clone();
    assert!(
        first == "portos=unknown; terminal=none"
            || first == "portos=degraded(printer=Error,paper=Empty); terminal=none",
        "{first}"
    );
    assert_clean(&fake);
}
