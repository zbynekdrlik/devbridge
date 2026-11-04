//! Card jobs of the TIM executor over the fake TIM (#102 lane D2, CI — no
//! SDK): approve + commit, decline, Stop, ECR cancel, deadline rollback,
//! card.rollback, crash before commit + reconcile, terminal_unreachable,
//! slips + a failing printer, idempotency.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::card::{self, LastTransaction, TxFacts};
use super::tests::{FakeTim, executor, job};
use super::{STATUS_OFFLINE, STATUS_OK, SlipPrinter, TimExecutor};
use crate::device_jobs::api::{AckRequest, DeviceJob};
use crate::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use crate::device_jobs::journal::{JobState, Journal};

const PURCHASE: i64 = 7;

/// The simulator's authorisation (eft-probe/probe.out, 4.11.2026).
fn facts(trans_seq: i64, amount_minor: i64, cvm: i32) -> TxFacts {
    TxFacts {
        transaction_type: Some(1),
        amount_minor: Some(amount_minor),
        currency_eur: Some(true),
        trans_seq: Some(trans_seq),
        trans_ref: Some(200),
        trm_trans_ref: Some("0012345678100".into()),
        auth_code: Some("300".into()),
        pos_entry_mode: Some(18),
        cvm: Some(cvm),
        brand_name: Some("VISA".into()),
        card_number_printable: Some("4193581767935240".into()),
        merchant_receipt: Some("*** Merchant ***\nSign here".into()),
        cardholder_receipt: Some("*** Cardholder receipt ***\nTotal-EFT EUR: 2.50".into()),
    }
}

fn last(trans_seq: i64, amount_minor: i64, outcome: i32) -> LastTransaction {
    LastTransaction {
        transaction_type: 1,
        outcome,
        facts: Some(facts(trans_seq, amount_minor, 1)),
    }
}

/// One printed slip: till, title, lines, when.
type Printed = (i64, String, Vec<String>, Instant);

/// A printer that records every slip (and fails when told).
#[derive(Default)]
struct FakePrinter {
    printed: Mutex<Vec<Printed>>,
    fail: bool,
}

#[async_trait]
impl SlipPrinter for FakePrinter {
    async fn print_slip(&self, kasa_id: i64, title: &str, lines: &[String]) -> Result<(), String> {
        self.printed
            .lock()
            .unwrap()
            .push((kasa_id, title.into(), lines.to_vec(), Instant::now()));
        if self.fail {
            Err("portos_unreachable: printer off".into())
        } else {
            Ok(())
        }
    }
}

impl FakePrinter {
    fn titles(&self) -> Vec<String> {
        self.printed
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.1.clone())
            .collect()
    }
}

struct Rig {
    fake: FakeTim,
    journal: Arc<Journal>,
    printer: Arc<FakePrinter>,
    tim: Arc<TimExecutor>,
    _dir: tempfile::TempDir,
}

fn rig_with(authorisation: Result<TxFacts, i32>, deadline: Duration, printer: FakePrinter) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    {
        let mut s = fake.0.lock().unwrap();
        s.purchase = Some(authorisation);
        s.last = last(100, 999, 1);
    }
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let printer = Arc::new(printer);
    let tim = Arc::new(
        executor(&fake, dir.path())
            .with_journal(Arc::clone(&journal))
            .with_printer(Arc::clone(&printer) as Arc<dyn SlipPrinter>)
            .with_deadline(deadline),
    );
    Rig {
        fake,
        journal,
        printer,
        tim,
        _dir: dir,
    }
}

fn rig(authorisation: Result<TxFacts, i32>) -> Rig {
    rig_with(
        authorisation,
        Duration::from_secs(10),
        FakePrinter::default(),
    )
}

fn purchase_job() -> DeviceJob {
    purchase_job_id(PURCHASE)
}

fn purchase_job_id(id: i64) -> DeviceJob {
    let mut j = job(
        card::JOB_PURCHASE,
        3,
        json!({"amount": 2.5, "currency": "EUR"}),
    );
    j.id = id;
    j
}

fn settle_job(id: i64, job_type: &str, payload: Value) -> DeviceJob {
    let mut j = job(job_type, 3, payload);
    j.id = id;
    j
}

/// Journal + start the job as the device worker does, then execute it.
async fn run(r: &Rig, j: &DeviceJob) -> Outcome {
    r.journal.insert_received(j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let outcome = r.tim.execute(j).await;
    r.journal.record_outcome(j.id, &outcome).unwrap();
    outcome
}

/// Start the purchase in the background and wait until it is authorised.
async fn authorised(r: &Rig) -> tokio::task::JoinHandle<Outcome> {
    authorised_id(r, PURCHASE).await
}

async fn authorised_id(r: &Rig, id: i64) -> tokio::task::JoinHandle<Outcome> {
    let j = purchase_job_id(id);
    r.journal.insert_received(&j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let tim = Arc::clone(&r.tim);
    let running = tokio::spawn(async move { tim.execute(&j).await });
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let card = r.journal.card_payment(id).unwrap();
        if card.and_then(|c| c.authorized_at).is_some() {
            return running;
        }
        assert!(
            Instant::now() < until,
            "never authorised; calls {:?}",
            r.fake.calls()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn finish(r: &Rig, running: tokio::task::JoinHandle<Outcome>) -> Outcome {
    finish_id(r, running, PURCHASE).await
}

async fn finish_id(r: &Rig, running: tokio::task::JoinHandle<Outcome>, id: i64) -> Outcome {
    let outcome = running.await.unwrap();
    r.journal.record_outcome(id, &outcome).unwrap();
    outcome
}

fn calls_after_create(r: &Rig) -> Vec<String> {
    r.fake
        .calls()
        .into_iter()
        .filter(|c| c != "settings")
        .collect()
}

fn failed_code(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed { code, .. } => code,
        other => panic!("expected failed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_approve_then_commit() {
    let r = rig(Ok(facts(101, 250, 1)));
    let running = authorised(&r).await;
    // the authorisation is owed to Odoo at once, masked, before the commit
    let owed = r.journal.owed_acks().unwrap();
    let AckRequest::Authorized { job_id, result } = &owed[0] else {
        panic!("{owed:?}")
    };
    assert_eq!(*job_id, PURCHASE);
    assert_eq!(result["state"], "authorized");
    assert_eq!(result["terminal"]["tid"], "12345678");
    assert_eq!(result["transaction"]["trans_seq"], 101);
    assert_eq!(result["transaction"]["amount"], 2.5);
    assert_eq!(result["card"]["card_number_printable"], "XXXXXXXXXXXX5240");
    assert!(!result.to_string().contains("4193581767935240"), "no PAN");
    assert_eq!(r.tim.status(), "busy", "the session is held");
    let card_row = r.journal.card_payment(PURCHASE).unwrap().unwrap();
    assert_eq!(
        (
            card_row.baseline_seq,
            card_row.amount_minor,
            card_row.trans_seq
        ),
        (Some(100), 250, Some(101))
    );
    assert!(
        r.journal
            .get(PURCHASE)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_some()
    );

    let commit = settle_job(
        8,
        card::JOB_COMMIT,
        json!({"card_job_id": PURCHASE, "trans_seq": 101, "amount": 2.5}),
    );
    let committed = run(&r, &commit).await;
    assert_eq!(
        committed,
        Outcome::Done(json!({"state": "committed", "card_job_id": PURCHASE}))
    );
    match finish(&r, running).await {
        Outcome::Done(result) => {
            assert_eq!(result["state"], "committed");
            assert_eq!(
                result["receipts"]["merchant"],
                "*** Merchant ***\nSign here"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        calls_after_create(&r),
        [
            "create",
            "pos_id 3",
            "connect",
            "login",
            "terminal_id",
            "last_transaction",
            "purchase",
            "amount 250",
            "commit",
            "logout",
            "disconnect",
            "dispose"
        ]
    );
    assert!(
        r.journal
            .owed_acks()
            .unwrap()
            .iter()
            .all(|a| !matches!(a, AckRequest::Authorized { .. })),
        "no auth ack once decided"
    );
    assert_eq!(r.tim.status(), STATUS_OK);
    // the cardholder slip only (PIN: no signature, no merchant slip)
    assert_eq!(r.printer.titles(), [card::CARDHOLDER_SLIP_TITLE]);
    let printed = r.printer.printed.lock().unwrap();
    assert_eq!(printed[0].0, 3);
    assert_eq!(
        printed[0].2,
        ["*** Cardholder receipt ***", "Total-EFT EUR: 2.50"]
    );
}

#[tokio::test]
async fn test_signature_slip_and_a_failing_printer_never_change_the_payment() {
    let r = rig_with(
        Ok(facts(101, 250, 3)),
        Duration::from_secs(10),
        FakePrinter {
            fail: true,
            ..Default::default()
        },
    );
    let running = authorised(&r).await;
    let commit = settle_job(8, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(run(&r, &commit).await.label(), "done");
    assert_eq!(finish(&r, running).await.label(), "done");
    let until = Instant::now() + Duration::from_secs(5);
    while r.printer.titles().len() < 2 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        r.printer.titles(),
        [card::CARDHOLDER_SLIP_TITLE, card::MERCHANT_SLIP_TITLE]
    );
}

#[tokio::test]
async fn test_refusals_of_the_terminal_charge_nothing() {
    for (rc, code) in [
        (82, card::CODE_DECLINED),
        (53, card::CODE_CANCELLED),
        (2, card::CODE_CANCELLED),
        (69, card::CODE_TERMINAL_BUSY),
    ] {
        let r = rig(Err(rc));
        let outcome = run(&r, &purchase_job()).await;
        assert_eq!(failed_code(&outcome), code, "rc {rc}");
        let calls = calls_after_create(&r);
        assert!(
            !calls.iter().any(|c| c == "commit" || c == "rollback"),
            "rc {rc}: {calls:?}"
        );
        assert!(
            calls.ends_with(&["logout".into(), "disconnect".into(), "dispose".into()]),
            "{calls:?}"
        );
        // the contact was journaled before the transaction
        assert!(
            r.journal
                .get(PURCHASE)
                .unwrap()
                .unwrap()
                .device_contact_at
                .is_some()
        );
        assert!(r.printer.titles().is_empty());
        assert_eq!(r.tim.status(), STATUS_OK, "the terminal answered: it works");
    }
}

#[tokio::test]
async fn test_unknown_transaction_outcome_is_indeterminate() {
    let r = rig(Err(10));
    let outcome = run(&r, &purchase_job()).await;
    assert!(
        matches!(&outcome, Outcome::Indeterminate(why) if why.contains("rc 10")),
        "{outcome:?}"
    );
    assert!(
        calls_after_create(&r).contains(&"rollback".to_string()),
        "released with a rollback"
    );
    assert!(
        r.journal
            .card_payment(PURCHASE)
            .unwrap()
            .unwrap()
            .ended_at
            .is_some()
    );
}

#[tokio::test]
async fn test_terminal_unreachable_sends_nothing() {
    let r = rig(Ok(facts(101, 250, 1)));
    r.fake.0.lock().unwrap().fail = vec!["connect"];
    let outcome = run(&r, &purchase_job()).await;
    assert_eq!(failed_code(&outcome), card::CODE_TERMINAL_UNREACHABLE);
    let entry = r.journal.get(PURCHASE).unwrap().unwrap();
    assert!(entry.device_contact_at.is_none(), "provably never sent");
    assert!(r.journal.card_payment(PURCHASE).unwrap().is_none());
    assert_eq!(r.tim.status(), STATUS_OFFLINE);
    // a login failure: nothing sent either
    let r = rig(Ok(facts(101, 250, 1)));
    r.fake.0.lock().unwrap().fail = vec!["login"];
    assert_eq!(
        failed_code(&run(&r, &purchase_job()).await),
        card::CODE_TERMINAL_ERROR
    );
    // the wrong terminal at that IP
    let r = rig(Ok(facts(101, 250, 1)));
    r.fake.0.lock().unwrap().terminal_id = Some("99".into());
    assert_eq!(
        failed_code(&run(&r, &purchase_job()).await),
        card::CODE_WRONG_TERMINAL
    );
    assert!(!calls_after_create(&r).contains(&"purchase".to_string()));
}

#[tokio::test]
async fn test_deadline_rolls_the_payment_back() {
    let r = rig_with(
        Ok(facts(101, 250, 1)),
        Duration::from_millis(200),
        FakePrinter::default(),
    );
    let started = Instant::now();
    let running = authorised(&r).await;
    let outcome = finish(&r, running).await;
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(failed_code(&outcome), card::CODE_ROLLED_BACK);
    assert!(matches!(&outcome, Outcome::Failed { text, .. } if text.contains("deadline")));
    assert!(calls_after_create(&r).contains(&"rollback".to_string()));
    // a commit after the deadline: the journal says it is not charged
    let commit = settle_job(8, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(
        failed_code(&run(&r, &commit).await),
        card::CODE_ALREADY_ROLLED_BACK
    );
    let commits = calls_after_create(&r)
        .iter()
        .filter(|c| *c == "commit")
        .count();
    assert_eq!(commits, 0, "never committed");
}

#[tokio::test]
async fn test_card_rollback_and_late_settles() {
    let r = rig(Ok(facts(101, 250, 1)));
    let running = authorised(&r).await;
    let rollback = settle_job(8, card::JOB_ROLLBACK, json!({"card_job_id": PURCHASE}));
    match run(&r, &rollback).await {
        Outcome::Done(result) => assert_eq!(result["state"], "rolled_back"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        failed_code(&finish(&r, running).await),
        card::CODE_ROLLED_BACK
    );
    // the same purchase settled again from the journal
    let again = settle_job(9, card::JOB_ROLLBACK, json!({"card_job_id": PURCHASE}));
    assert_eq!(run(&r, &again).await.label(), "done");
    let commit = settle_job(10, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(
        failed_code(&run(&r, &commit).await),
        card::CODE_ALREADY_ROLLED_BACK
    );
}

#[tokio::test]
async fn test_commit_timeout_on_the_terminal_is_rolled_back() {
    let r = rig(Ok(facts(101, 250, 1)));
    r.fake.0.lock().unwrap().commit_rc = Some(card::RC_COMMIT_TIMEOUT);
    let running = authorised(&r).await;
    let commit = settle_job(8, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(
        failed_code(&run(&r, &commit).await),
        card::CODE_ALREADY_ROLLED_BACK
    );
    assert_eq!(
        failed_code(&finish(&r, running).await),
        card::CODE_ROLLED_BACK
    );
    // any other commit error: unknown on both
    let r = rig(Ok(facts(101, 250, 1)));
    r.fake.0.lock().unwrap().commit_rc = Some(10);
    let running = authorised(&r).await;
    let commit = settle_job(8, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(run(&r, &commit).await.label(), "indeterminate");
    assert_eq!(finish(&r, running).await.label(), "indeterminate");
}

#[tokio::test]
async fn test_settle_refusals_send_nothing() {
    let r = rig(Ok(facts(101, 250, 1)));
    let running = authorised(&r).await;
    let wrong_seq = settle_job(
        8,
        card::JOB_COMMIT,
        json!({"card_job_id": PURCHASE, "trans_seq": 999}),
    );
    assert_eq!(
        failed_code(&run(&r, &wrong_seq).await),
        card::CODE_SETTLE_MISMATCH
    );
    let wrong_amount = settle_job(
        9,
        card::JOB_COMMIT,
        json!({"card_job_id": PURCHASE, "amount": 3.0}),
    );
    assert_eq!(
        failed_code(&run(&r, &wrong_amount).await),
        card::CODE_SETTLE_MISMATCH
    );
    let unknown = settle_job(10, card::JOB_COMMIT, json!({"card_job_id": 4711}));
    assert_eq!(
        failed_code(&run(&r, &unknown).await),
        card::CODE_UNKNOWN_PURCHASE
    );
    let mut other_till = settle_job(11, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    other_till.kasa_id = Some(4);
    assert_eq!(
        failed_code(&run(&r, &other_till).await),
        card::CODE_UNKNOWN_PURCHASE
    );
    let not_purchase = settle_job(12, card::JOB_ROLLBACK, json!({"card_job_id": 8}));
    assert_eq!(
        failed_code(&run(&r, &not_purchase).await),
        card::CODE_UNKNOWN_PURCHASE
    );
    assert!(
        !calls_after_create(&r)
            .iter()
            .any(|c| c == "commit" || c == "rollback"),
        "still held"
    );
    // Odoo refused the authorisation (too_late): rolled back at once
    r.journal
        .mark_card_auth_acked(PURCHASE, "refused: too_late")
        .unwrap();
    let outcome = finish(&r, running).await;
    assert!(
        matches!(&outcome, Outcome::Failed { code, text } if code == "rolled_back" && text.contains("Odoo refused")),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_an_authorisation_that_cannot_pay_the_job_is_rolled_back() {
    let r = rig(Ok(facts(101, 200, 1)));
    let outcome = run(&r, &purchase_job()).await;
    assert_eq!(failed_code(&outcome), card::CODE_ROLLED_BACK);
    assert!(
        r.journal
            .card_payment(PURCHASE)
            .unwrap()
            .unwrap()
            .authorized_at
            .is_none(),
        "never offered to Odoo"
    );
    assert!(calls_after_create(&r).contains(&"rollback".to_string()));
}

#[tokio::test]
async fn test_a_contacted_key_is_never_transacted_again() {
    let r = rig(Ok(facts(101, 250, 1)));
    let j = purchase_job();
    r.journal.insert_received(&j).unwrap();
    r.journal.start_execution(PURCHASE).unwrap();
    // an earlier attempt reached the terminal
    assert!(
        r.journal
            .card_contact(PURCHASE, "10.78.2.8", 250, Some(99))
            .unwrap()
    );
    let outcome = r.tim.execute(&j).await;
    assert_eq!(
        outcome.label(),
        "indeterminate",
        "may have charged: never failed"
    );
    assert!(
        !calls_after_create(&r).contains(&"purchase".to_string()),
        "no second transaction"
    );
    // Odoo handing the job out again is never executed (the journal)
    assert!(matches!(
        r.journal.insert_received(&j).unwrap(),
        crate::device_jobs::journal::Inserted::Known(_)
    ));
}

/// A purchase whose bridge died after the authorisation, before a commit.
fn crashed(r: &Rig, backdate_s: i64) -> crate::device_jobs::journal::JournalEntry {
    let j = purchase_job();
    r.journal.insert_received(&j).unwrap();
    r.journal.start_execution(PURCHASE).unwrap();
    r.journal
        .card_contact(PURCHASE, "10.78.2.8", 250, Some(100))
        .unwrap();
    let result = card::card_result("authorized", Some("12345678"), &facts(101, 250, 1));
    r.journal.card_authorized(PURCHASE, 101, &result).unwrap();
    r.journal.recover_after_restart().unwrap();
    r.journal.card_backdate(PURCHASE, backdate_s);
    r.journal.get(PURCHASE).unwrap().unwrap()
}

/// The same terminal + journal under an executor that started 10 min ago.
fn restarted(r: Rig) -> Rig {
    let tim = Arc::new(
        executor(&r.fake, r._dir.path())
            .with_journal(Arc::clone(&r.journal))
            .with_started_at(chrono::Utc::now() - chrono::Duration::seconds(600)),
    );
    Rig { tim, ..r }
}

#[tokio::test]
async fn test_reconcile_after_a_crash_before_commit() {
    // match: a purchase above the baseline with our amount, successful
    let r = restarted(rig(Ok(facts(101, 250, 1))));
    let entry = crashed(&r, 600);
    assert_eq!(entry.state, JobState::Indeterminate);
    assert!(
        r.journal.owed_acks().unwrap().is_empty(),
        "no stale auth ack after the restart"
    );
    r.fake.0.lock().unwrap().last = last(101, 250, 1);
    match r.tim.reconcile(&entry).await {
        Reconciliation::Done(result) => {
            assert_eq!(result["state"], "successful");
            assert_eq!(result["reconciled"], true);
            assert_eq!(result["transaction"]["trans_seq"], 101);
        }
        other => panic!("{other:?}"),
    }
    assert!(!calls_after_create(&r).contains(&"purchase".to_string()));

    // the terminal reversed it: not charged
    let r = restarted(rig(Ok(facts(101, 250, 1))));
    let entry = crashed(&r, 600);
    r.fake.0.lock().unwrap().last = last(101, 250, 2);
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::Failed { code, .. } if code == "rolled_back")
    );

    // no match: a person decides, the terminal is not asked again
    let r = restarted(rig(Ok(facts(101, 250, 1))));
    let entry = crashed(&r, 600);
    r.fake.0.lock().unwrap().last = last(100, 999, 1);
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.starts_with("left for a person"))
    );
    let asked = r.fake.calls().len();
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.starts_with("left for a person"))
    );
    assert_eq!(r.fake.calls().len(), asked, "no second terminal contact");
    assert!(
        r.journal
            .card_payment(PURCHASE)
            .unwrap()
            .unwrap()
            .person_reason
            .is_some()
    );

    // too early: the terminal's commit timeout has not passed
    let r = restarted(rig(Ok(facts(101, 250, 1))));
    let entry = crashed(&r, 30);
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.starts_with("waiting until"))
    );
    assert!(!calls_after_create(&r).contains(&"connect".to_string()));
    // nor after a fresh start of the executor (a crash just before it)
    let fresh = rig(Ok(facts(101, 250, 1)));
    let entry = crashed(&fresh, 600);
    assert!(
        matches!(fresh.tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.starts_with("waiting until"))
    );

    // the terminal could not be asked: try again later
    let r = restarted(rig(Ok(facts(101, 250, 1))));
    let entry = crashed(&r, 600);
    r.fake.0.lock().unwrap().fail = vec!["connect"];
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.contains("could not be asked"))
    );
    assert!(
        r.journal
            .card_payment(PURCHASE)
            .unwrap()
            .unwrap()
            .person_reason
            .is_none()
    );
}

#[tokio::test]
async fn test_reconcile_without_contact_and_of_settle_jobs() {
    let r = rig(Ok(facts(101, 250, 1)));
    let j = purchase_job();
    r.journal.insert_received(&j).unwrap();
    r.journal.recover_after_restart().unwrap();
    let entry = r.journal.get(PURCHASE).unwrap().unwrap();
    assert!(
        matches!(r.tim.reconcile(&entry).await, Reconciliation::Failed { code, .. } if code == card::CODE_NOT_EXECUTED)
    );

    // a commit whose purchase is unresolved follows it
    let r = rig(Ok(facts(101, 250, 1)));
    let _purchase = crashed(&r, 0);
    let commit = settle_job(8, card::JOB_COMMIT, json!({"card_job_id": PURCHASE}));
    assert_eq!(run(&r, &commit).await.label(), "indeterminate");
    let commit_entry = r.journal.get(8).unwrap().unwrap();
    assert!(matches!(
        r.tim.reconcile(&commit_entry).await,
        Reconciliation::StillUnknown(_)
    ));
    r.journal
        .record_outcome(PURCHASE, &Outcome::Done(json!({})))
        .unwrap();
    assert!(matches!(
        r.tim.reconcile(&commit_entry).await,
        Reconciliation::Done(_)
    ));
    let rollback = settle_job(9, card::JOB_ROLLBACK, json!({"card_job_id": PURCHASE}));
    r.journal.insert_received(&rollback).unwrap();
    let rb_entry = r.journal.get(9).unwrap().unwrap();
    assert!(
        matches!(r.tim.reconcile(&rb_entry).await, Reconciliation::Failed { code, .. } if code == card::CODE_ALREADY_COMMITTED)
    );
}

/// Every log line of this test binary (one global subscriber).
fn captured() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || Sink(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global test subscriber");
        buffer
    })
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The timestamp of the first captured line holding all of `needles`.
fn logged_at(needles: &[&str]) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let text = String::from_utf8(captured().lock().unwrap().clone()).unwrap();
    text.lines()
        .find(|l| needles.iter().all(|n| l.contains(n)))
        .and_then(|l| chrono::DateTime::parse_from_rfc3339(l.split_whitespace().next()?).ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_slip_print_request_within_3_s_of_the_response_by_the_logs() {
    captured();
    // an id no other test uses: the subscriber is global
    const LOGGED: i64 = 9_701;
    let r = rig(Ok(facts(101, 250, 1)));
    let running = authorised_id(&r, LOGGED).await;
    let until = Instant::now() + Duration::from_secs(5);
    while r.printer.titles().is_empty() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let commit = settle_job(9_702, card::JOB_COMMIT, json!({"card_job_id": LOGGED}));
    assert_eq!(run(&r, &commit).await.label(), "done");
    assert_eq!(finish_id(&r, running, LOGGED).await.label(), "done");
    let id = format!("job_id={LOGGED}");
    let response =
        logged_at(&["TIM transaction response: authorised", &id]).expect("response logged");
    let print = logged_at(&["card slip print request sent", &id]).expect("print request logged");
    let elapsed = print - response;
    assert!(elapsed >= chrono::Duration::zero(), "{elapsed}");
    assert!(
        elapsed <= chrono::Duration::seconds(3),
        "response -> print request took {elapsed}"
    );
}
