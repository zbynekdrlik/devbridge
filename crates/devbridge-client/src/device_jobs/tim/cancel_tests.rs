//! `card.cancel` over the fake TIM (#102 lane V, Worldline docx case I5 —
//! CI, no SDK): a purchase / credit in its card wait is cancelled through
//! `ta_terminal_cancel` on its own session's terminal object and ends
//! `failed cancelled`; the next purchase runs; `too_late` after the
//! authorisation; `not_running` for anything not in its card wait; the
//! check, the routing to `<ip>#cancel` and the restart reconcile.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::card::{self, CancelVerdict, LastTransaction, TxFacts};
use super::tests::{FakeTim, executor, job};
use super::{TimExecutor, cancel_endpoint, settle_endpoint};
use crate::device_jobs::api::DeviceJob;
use crate::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use crate::device_jobs::journal::Journal;

const PURCHASE: i64 = 7;
const CANCEL: i64 = 8;

fn facts(trans_seq: i64, amount_minor: i64) -> TxFacts {
    TxFacts {
        transaction_type: Some(1),
        amount_minor: Some(amount_minor),
        currency_eur: Some(true),
        trans_seq: Some(trans_seq),
        trans_ref: Some(200),
        trm_trans_ref: Some("0012345678100".into()),
        auth_code: Some("300".into()),
        pos_entry_mode: Some(18),
        cvm: Some(1),
        brand_name: Some("VISA".into()),
        card_number_printable: Some("4193581767935240".into()),
        merchant_receipt: None,
        cardholder_receipt: None,
    }
}

struct Rig {
    fake: FakeTim,
    journal: Arc<Journal>,
    tim: Arc<TimExecutor>,
    _dir: tempfile::TempDir,
}

/// The cardholder never comes: the transaction waits until a cancel.
fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    {
        let mut s = fake.0.lock().unwrap();
        s.wait_for_cancel = true;
        s.purchase = Some(Ok(facts(101, 250)));
        s.last = LastTransaction {
            transaction_type: 1,
            outcome: 1,
            facts: Some(facts(100, 999)),
        };
    }
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let tim = Arc::new(
        executor(&fake, dir.path())
            .with_journal(Arc::clone(&journal))
            .with_deadline(Duration::from_secs(10))
            .with_release_within(Duration::from_secs(1)),
    );
    Rig {
        fake,
        journal,
        tim,
        _dir: dir,
    }
}

fn card_job(id: i64, job_type: &str, kasa_id: i64, payload: Value) -> DeviceJob {
    let mut j = job(job_type, kasa_id, payload);
    j.id = id;
    j
}

fn purchase_job(id: i64, job_type: &str, kasa_id: i64) -> DeviceJob {
    card_job(
        id,
        job_type,
        kasa_id,
        json!({"amount": 2.5, "currency": "EUR"}),
    )
}

fn cancel_job(id: i64, target: i64) -> DeviceJob {
    card_job(id, card::JOB_CANCEL, 3, json!({"card_job_id": target}))
}

/// Journal + start + execute + record, as the device worker does.
async fn run(r: &Rig, j: &DeviceJob) -> Outcome {
    r.journal.insert_received(j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let outcome = r.tim.execute(j).await;
    r.journal.record_outcome(j.id, &outcome).unwrap();
    outcome
}

/// Start `j` in the background and wait until it sits in its card wait.
async fn in_card_wait(r: &Rig, j: DeviceJob) -> tokio::task::JoinHandle<Outcome> {
    r.journal.insert_received(&j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let tim = Arc::clone(&r.tim);
    let running = tokio::spawn(async move { tim.execute(&j).await });
    let until = Instant::now() + Duration::from_secs(5);
    while !r.fake.0.lock().unwrap().in_card_wait {
        assert!(
            Instant::now() < until,
            "never in its card wait; calls {:?}",
            r.fake.calls()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    running
}

async fn finish(r: &Rig, id: i64, running: tokio::task::JoinHandle<Outcome>) -> Outcome {
    let outcome = running.await.unwrap();
    r.journal.record_outcome(id, &outcome).unwrap();
    outcome
}

fn failed(outcome: &Outcome) -> (&str, &str) {
    match outcome {
        Outcome::Failed { code, text } => (code, text),
        other => panic!("expected failed, got {other:?}"),
    }
}

fn cancels(r: &Rig) -> usize {
    r.fake.calls().iter().filter(|c| *c == "cancel").count()
}

#[test]
fn test_cancel_verdict() {
    assert_eq!(card::cancel_verdict(true, false), CancelVerdict::Send);
    assert_eq!(card::cancel_verdict(true, true), CancelVerdict::Send);
    assert_eq!(card::cancel_verdict(false, true), CancelVerdict::TooLate);
    assert_eq!(
        card::cancel_verdict(false, false),
        CancelVerdict::NotRunning
    );
}

#[tokio::test]
async fn test_cancel_stops_a_purchase_in_its_card_wait_and_the_next_one_runs() {
    let r = rig();
    let running = in_card_wait(&r, purchase_job(PURCHASE, card::JOB_PURCHASE, 3)).await;

    let outcome = run(&r, &cancel_job(CANCEL, PURCHASE)).await;
    let Outcome::Done(result) = &outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(
        result,
        &json!({"cancel_sent": true, "card_job_id": PURCHASE})
    );

    // the purchase's own ack is the authority: cancelled, nothing charged
    let purchase = finish(&r, PURCHASE, running).await;
    let (code, text) = failed(&purchase);
    assert_eq!(code, card::CODE_CANCELLED);
    assert!(text.contains("nothing charged"), "{text}");
    assert_eq!(cancels(&r), 1);
    assert!(r.tim.cancellers.lock().unwrap().is_empty(), "handle gone");
    let calls = r.fake.calls();
    let tail: Vec<&str> = calls
        .iter()
        .map(String::as_str)
        .skip_while(|c| *c != "purchase")
        .collect();
    assert_eq!(
        tail,
        [
            "purchase",
            "amount 250",
            "cancel",
            "logout",
            "disconnect",
            "dispose"
        ],
        "no rollback / no second transaction after a cancel"
    );

    // the same cancel again finds nothing in its card wait: harmless
    let again = run(&r, &cancel_job(CANCEL + 1, PURCHASE)).await;
    assert_eq!(failed(&again).0, card::CODE_NOT_RUNNING);
    assert_eq!(cancels(&r), 1, "nothing sent");

    // another transaction can be performed afterwards (docx I5)
    r.fake.0.lock().unwrap().wait_for_cancel = false;
    let next = purchase_job(20, card::JOB_PURCHASE, 3);
    r.journal.insert_received(&next).unwrap();
    assert!(r.journal.start_execution(next.id).unwrap());
    let tim = Arc::clone(&r.tim);
    let next_running = tokio::spawn(async move { tim.execute(&next).await });
    let until = Instant::now() + Duration::from_secs(5);
    while r
        .journal
        .card_payment(20)
        .unwrap()
        .and_then(|c| c.authorized_at)
        .is_none()
    {
        assert!(
            Instant::now() < until,
            "never authorised: {:?}",
            r.fake.calls()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // authorised and held: a cancel is too late now
    let late = run(&r, &cancel_job(21, 20)).await;
    let (code, text) = failed(&late);
    assert_eq!(code, card::CODE_TOO_LATE);
    assert!(
        text.contains("card.rollback") && text.contains("nothing ran"),
        "{text}"
    );
    assert_eq!(cancels(&r), 1, "nothing sent");
    let commit = run(
        &r,
        &card_job(22, card::JOB_COMMIT, 3, json!({"card_job_id": 20})),
    )
    .await;
    assert!(matches!(commit, Outcome::Done(_)), "{commit:?}");
    let committed = finish(&r, 20, next_running).await;
    let Outcome::Done(result) = &committed else {
        panic!("{committed:?}")
    };
    assert_eq!(result["state"], "committed");
    // committed: still too late (a storno is card.reversal)
    let after = run(&r, &cancel_job(23, 20)).await;
    assert_eq!(failed(&after).0, card::CODE_TOO_LATE);
}

#[tokio::test]
async fn test_cancel_stops_a_credit_in_its_card_wait() {
    let r = rig();
    let running = in_card_wait(&r, purchase_job(PURCHASE, card::JOB_CREDIT, 3)).await;
    let outcome = run(&r, &cancel_job(CANCEL, PURCHASE)).await;
    assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
    let credit = finish(&r, PURCHASE, running).await;
    assert_eq!(failed(&credit).0, card::CODE_CANCELLED);
}

#[tokio::test]
async fn test_cancel_not_running_sends_nothing() {
    let r = rig();
    // unknown job id
    let unknown = run(&r, &cancel_job(CANCEL, 999)).await;
    let (code, text) = failed(&unknown);
    assert_eq!(code, card::CODE_NOT_RUNNING);
    assert!(text.contains("nothing ran"), "{text}");
    // a journaled purchase that never reached its card wait
    let waiting = purchase_job(30, card::JOB_PURCHASE, 3);
    r.journal.insert_received(&waiting).unwrap();
    let queued = run(&r, &cancel_job(31, 30)).await;
    assert_eq!(failed(&queued).0, card::CODE_NOT_RUNNING);
    // a card job that is no purchase / credit
    let balance = card_job(32, card::JOB_BALANCE, 3, json!({}));
    r.journal.insert_received(&balance).unwrap();
    let other = run(&r, &cancel_job(33, 32)).await;
    assert_eq!(failed(&other).0, card::CODE_NOT_RUNNING);
    assert_eq!(cancels(&r), 0);
    assert!(
        !r.fake.calls().contains(&"connect".to_string()),
        "no terminal contact"
    );
}

#[tokio::test]
async fn test_cancel_of_another_tills_purchase_is_not_running() {
    let r = rig();
    // till 4's purchase is in its card wait; till 3's cancel names it
    let running = in_card_wait(&r, purchase_job(PURCHASE, card::JOB_PURCHASE, 4)).await;
    let foreign = run(&r, &cancel_job(CANCEL, PURCHASE)).await;
    assert_eq!(failed(&foreign).0, card::CODE_NOT_RUNNING);
    assert_eq!(cancels(&r), 0, "never another till's terminal");
    // till 4's own cancel stops it
    let own = card_job(
        CANCEL + 1,
        card::JOB_CANCEL,
        4,
        json!({"card_job_id": PURCHASE}),
    );
    assert!(matches!(run(&r, &own).await, Outcome::Done(_)));
    assert_eq!(
        failed(&finish(&r, PURCHASE, running).await).0,
        card::CODE_CANCELLED
    );
}

#[tokio::test]
async fn test_cancel_that_is_not_sent_is_not_running_and_a_retry_works() {
    let r = rig();
    r.fake.0.lock().unwrap().fail = vec!["cancel"];
    let running = in_card_wait(&r, purchase_job(PURCHASE, card::JOB_PURCHASE, 3)).await;
    let refused = run(&r, &cancel_job(CANCEL, PURCHASE)).await;
    let (code, text) = failed(&refused);
    assert_eq!(code, card::CODE_NOT_RUNNING);
    assert!(
        text.contains("not sent") && text.contains("nothing ran"),
        "{text}"
    );
    assert!(
        r.fake.0.lock().unwrap().in_card_wait,
        "still waiting for the card"
    );
    r.fake.0.lock().unwrap().fail.clear();
    let retry = run(&r, &cancel_job(CANCEL + 1, PURCHASE)).await;
    assert!(matches!(retry, Outcome::Done(_)), "{retry:?}");
    assert_eq!(
        failed(&finish(&r, PURCHASE, running).await).0,
        card::CODE_CANCELLED
    );
}

#[tokio::test]
async fn test_purchase_without_a_cancel_handle_still_runs() {
    let r = rig();
    {
        let mut s = r.fake.0.lock().unwrap();
        s.fail = vec!["canceller"];
        s.wait_for_cancel = false;
    }
    let j = purchase_job(PURCHASE, card::JOB_PURCHASE, 3);
    r.journal.insert_received(&j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let tim = Arc::clone(&r.tim);
    let running = tokio::spawn(async move { tim.execute(&j).await });
    let until = Instant::now() + Duration::from_secs(5);
    while r
        .journal
        .card_payment(PURCHASE)
        .unwrap()
        .and_then(|c| c.authorized_at)
        .is_none()
    {
        assert!(Instant::now() < until, "never authorised");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(r.tim.cancellers.lock().unwrap().is_empty());
    let rollback = run(
        &r,
        &card_job(9, card::JOB_ROLLBACK, 3, json!({"card_job_id": PURCHASE})),
    )
    .await;
    assert!(matches!(rollback, Outcome::Done(_)), "{rollback:?}");
    assert_eq!(
        failed(&finish(&r, PURCHASE, running).await).0,
        card::CODE_ROLLED_BACK
    );
}

#[test]
fn test_cancel_check_and_routing() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let no_journal = executor(&fake, dir.path());
    let with_journal =
        executor(&fake, dir.path()).with_journal(Arc::new(Journal::open_in_memory().unwrap()));
    let code = |tim: &TimExecutor, j: &DeviceJob| tim.check(j).err().map(|e| e.code);

    let ok = cancel_job(CANCEL, PURCHASE);
    assert_eq!(code(&with_journal, &ok), None);
    assert_eq!(
        code(
            &with_journal,
            &card_job(CANCEL, card::JOB_CANCEL, 3, json!({}))
        ),
        Some(card::CODE_BAD_PAYLOAD)
    );
    assert_eq!(
        code(
            &with_journal,
            &card_job(CANCEL, card::JOB_CANCEL, 3, json!({"card_job_id": 0}))
        ),
        Some(card::CODE_BAD_PAYLOAD)
    );
    assert_eq!(
        code(
            &with_journal,
            &card_job(CANCEL, card::JOB_CANCEL, 9, json!({"card_job_id": 7}))
        ),
        Some(card::CODE_NO_TERMINAL)
    );
    assert_eq!(code(&no_journal, &ok), Some(card::CODE_TIM_UNAVAILABLE));
    let no_library = TimExecutor::with_backend(
        vec![super::tests::spec(3, "10.78.2.8", None)],
        Err("not loaded".into()),
        dir.path(),
    )
    .with_journal(Arc::new(Journal::open_in_memory().unwrap()));
    assert_eq!(code(&no_library, &ok), Some(card::CODE_TIM_UNAVAILABLE));

    // its own queue: neither the busy terminal queue nor the settle queue
    assert_eq!(with_journal.endpoint(&ok), cancel_endpoint("10.78.2.8"));
    assert_eq!(cancel_endpoint("10.78.2.8"), "10.78.2.8#cancel");
    let endpoints = with_journal.endpoints();
    for ip in ["10.78.2.8", "10.78.2.18"] {
        assert!(endpoints.contains(&ip.to_string()), "{endpoints:?}");
        assert!(endpoints.contains(&settle_endpoint(ip)), "{endpoints:?}");
        assert!(endpoints.contains(&cancel_endpoint(ip)), "{endpoints:?}");
    }
    assert_eq!(endpoints.len(), 6);
    assert_eq!(
        with_journal.endpoint(&card_job(1, card::JOB_CANCEL, 9, json!({"card_job_id": 7}))),
        "",
        "no terminal, no queue name"
    );
}

#[tokio::test]
async fn test_cancel_reconciles_as_not_running_after_a_restart() {
    let r = rig();
    let j = cancel_job(CANCEL, PURCHASE);
    r.journal.insert_received(&j).unwrap();
    let entry = r.journal.get(CANCEL).unwrap().unwrap();
    match r.tim.reconcile(&entry).await {
        Reconciliation::Failed { code, text } => {
            assert_eq!(code, card::CODE_NOT_RUNNING);
            assert!(text.contains("nothing to cancel"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        r.fake
            .calls()
            .iter()
            .all(|c| c != "connect" && c != "cancel")
    );
}

#[tokio::test]
async fn test_cancel_of_another_tills_authorised_payment_is_not_running() {
    let r = rig();
    r.fake.0.lock().unwrap().wait_for_cancel = false;
    let j = purchase_job(PURCHASE, card::JOB_PURCHASE, 4);
    r.journal.insert_received(&j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let tim = Arc::clone(&r.tim);
    let running = tokio::spawn(async move { tim.execute(&j).await });
    let until = Instant::now() + Duration::from_secs(5);
    while r
        .journal
        .card_payment(PURCHASE)
        .unwrap()
        .and_then(|c| c.authorized_at)
        .is_none()
    {
        assert!(Instant::now() < until, "never authorised");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // till 3 names till 4's held payment: not ours, so not even too_late
    let foreign = run(&r, &cancel_job(CANCEL, PURCHASE)).await;
    assert_eq!(failed(&foreign).0, card::CODE_NOT_RUNNING);
    let own = card_job(
        CANCEL + 1,
        card::JOB_CANCEL,
        4,
        json!({"card_job_id": PURCHASE}),
    );
    assert_eq!(failed(&run(&r, &own).await).0, card::CODE_TOO_LATE);
    let rollback = card_job(10, card::JOB_ROLLBACK, 4, json!({"card_job_id": PURCHASE}));
    assert!(matches!(run(&r, &rollback).await, Outcome::Done(_)));
    assert_eq!(
        failed(&finish(&r, PURCHASE, running).await).0,
        card::CODE_ROLLED_BACK
    );
    assert_eq!(cancels(&r), 0);
}
