//! Lane D3 card jobs of the TIM executor over the fake TIM (#102, CI — no
//! SDK): credit (two-phase), reversal (ONLY the TransSeq), activate /
//! deactivate, balance, print_copy, their failure codes, idempotency, the
//! restart reconcile. The balance + the 23:50 safety balance:
//! `ops_safety_tests.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::card::{self, LastTransaction, TxFacts};
use super::ops::COPY_SLIP_TITLE;
use super::reports::{Receipts, ShiftReport};
use super::tests::{FakeTim, executor, job};
use super::{SlipPrinter, TimExecutor};
use crate::device_jobs::api::{AckRequest, DeviceJob};
use crate::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use crate::device_jobs::journal::{JobState, Journal, SOURCE_SAFETY};

#[derive(Default)]
pub(super) struct Printer {
    pub(super) printed: Mutex<Vec<(i64, String, Vec<String>)>>,
    pub(super) fail: bool,
}

#[async_trait]
impl SlipPrinter for Printer {
    async fn print_slip(&self, kasa_id: i64, title: &str, lines: &[String]) -> Result<(), String> {
        self.printed
            .lock()
            .unwrap()
            .push((kasa_id, title.into(), lines.to_vec()));
        if self.fail {
            Err("printer off".into())
        } else {
            Ok(())
        }
    }
}

impl Printer {
    fn titles(&self) -> Vec<String> {
        self.printed
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.1.clone())
            .collect()
    }
}

pub(super) struct Rig {
    pub(super) fake: FakeTim,
    pub(super) journal: Arc<Journal>,
    pub(super) printer: Arc<Printer>,
    pub(super) tim: Arc<TimExecutor>,
    pub(super) dir: tempfile::TempDir,
}

pub(super) fn rig_with(printer: Printer, deadline: Duration) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let printer = Arc::new(printer);
    let tim = Arc::new(
        executor(&fake, dir.path())
            .with_journal(Arc::clone(&journal))
            .with_printer(Arc::clone(&printer) as Arc<dyn SlipPrinter>)
            .with_deadline(deadline)
            .with_release_within(Duration::from_secs(1)),
    );
    Rig {
        fake,
        journal,
        printer,
        tim,
        dir,
    }
}

pub(super) fn rig() -> Rig {
    rig_with(Printer::default(), Duration::from_secs(10))
}

/// The simulator's reversal of trans_seq 102 (4.11.2026): its own
/// trans_seq 103, the original amount, no card data.
fn reversal_facts(seq: i64, amount: i64) -> TxFacts {
    TxFacts {
        transaction_type: Some(card::TRANSACTION_TYPE_REVERSAL),
        amount_minor: Some(amount),
        currency_eur: Some(true),
        trans_seq: Some(seq),
        trans_ref: Some(203),
        trm_trans_ref: Some("0012345678103".into()),
        cvm: Some(0),
        cardholder_receipt: Some("Reversal\nRev. Seq-Cnt: 102".into()),
        merchant_receipt: Some("Reversal (merchant)".into()),
        ..TxFacts::default()
    }
}

fn credit_facts(seq: i64, amount: i64) -> TxFacts {
    TxFacts {
        transaction_type: Some(card::TRANSACTION_TYPE_CREDIT),
        amount_minor: Some(amount),
        currency_eur: Some(true),
        trans_seq: Some(seq),
        trans_ref: Some(204),
        auth_code: Some("302".into()),
        cvm: Some(0),
        cardholder_receipt: Some("Credit".into()),
        ..TxFacts::default()
    }
}

fn last_of(tx_type: i32, facts: TxFacts) -> LastTransaction {
    LastTransaction {
        transaction_type: tx_type,
        outcome: card::OUTCOME_SUCCESSFUL,
        facts: Some(facts),
    }
}

fn reversal_job(id: i64, payload: Value) -> DeviceJob {
    let mut j = job(card::JOB_REVERSAL, 3, payload);
    j.id = id;
    j
}

pub(super) fn typed(id: i64, job_type: &str, payload: Value) -> DeviceJob {
    let mut j = job(job_type, 3, payload);
    j.id = id;
    j
}

pub(super) async fn run(r: &Rig, j: &DeviceJob) -> Outcome {
    r.journal.insert_received(j).unwrap();
    assert!(r.journal.start_execution(j.id).unwrap());
    let outcome = r.tim.execute(j).await;
    r.journal.record_outcome(j.id, &outcome).unwrap();
    outcome
}

pub(super) fn failed_code(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed { code, .. } => code,
        other => panic!("expected failed, got {other:?}"),
    }
}

pub(super) fn count(calls: &[String], name: &str) -> usize {
    calls.iter().filter(|c| *c == name).count()
}

/// Wait (bounded) until `pred` holds.
async fn until(what: &str, pred: impl Fn() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while !pred() {
        assert!(std::time::Instant::now() < end, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

const REV: i64 = 20;

fn set_reversal(r: &Rig, answer: Result<TxFacts, i32>) {
    let mut s = r.fake.0.lock().unwrap();
    s.reversal = Some(answer);
    s.last = last_of(1, credit_facts(102, 250));
}

#[tokio::test]
async fn test_reversal_sends_only_the_trans_seq_and_needs_no_commit() {
    let r = rig();
    set_reversal(&r, Ok(reversal_facts(103, 250)));
    let j = reversal_job(
        REV,
        json!({"pos_payment_id": 4, "trans_seq": 102, "amount": 2.5, "tid": "12345678"}),
    );
    let outcome = run(&r, &j).await;
    let Outcome::Done(result) = &outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(result["state"], "reversed");
    assert_eq!(result["transaction"]["trans_seq"], 103);
    assert_eq!(result["transaction"]["amount"], 2.5);
    assert_eq!(result["transaction"]["type"], "reversal");
    assert_eq!(result["terminal"]["tid"], "12345678");
    let calls = r.fake.calls();
    // the reversal's ONLY argument is the TransSeq: no amount call, no commit
    let i = calls.iter().position(|c| c == "reversal").unwrap();
    assert_eq!(calls[i + 1], "trans_seq 102");
    assert!(!calls.iter().any(|c| c.starts_with("amount ")), "{calls:?}");
    assert_eq!(count(&calls, "commit"), 0);
    assert_eq!(count(&calls, "rollback"), 0);
    assert_eq!(count(&calls, "logout"), 1);
    assert_eq!(count(&calls, "disconnect"), 1);
    // contact journaled before the call, the reversal's own seq after it
    let row = r.journal.card_payment(REV).unwrap().unwrap();
    assert_eq!(
        (
            row.tx_type,
            row.baseline_seq,
            row.trans_seq,
            row.amount_minor
        ),
        (card::TRANSACTION_TYPE_REVERSAL, Some(102), Some(103), 250)
    );
    assert!(
        r.journal
            .get(REV)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_some()
    );
    // no authorised ack for a one-phase reversal
    assert!(
        !r.journal
            .owed_acks()
            .unwrap()
            .iter()
            .any(|a| matches!(a, AckRequest::Authorized { .. }))
    );
    until("the cardholder slip", || {
        r.printer.titles() == vec![card::CARDHOLDER_SLIP_TITLE.to_string()]
    })
    .await;
}

#[tokio::test]
async fn test_reversal_refusals_send_nothing_or_are_unknown() {
    // the payload names another terminal: nothing sent
    let r = rig();
    set_reversal(&r, Ok(reversal_facts(103, 250)));
    let other = reversal_job(
        REV,
        json!({"trans_seq": 102, "amount": 2.5, "tid": "99999999"}),
    );
    let outcome = run(&r, &other).await;
    assert_eq!(failed_code(&outcome), card::CODE_WRONG_TERMINAL);
    assert_eq!(count(&r.fake.calls(), "reversal"), 0);
    assert!(
        r.journal.card_payment(REV).unwrap().is_none(),
        "never contacted"
    );

    for (rc, expected) in [
        (67, Some(card::CODE_UNKNOWN_REFERENCE)),
        (82, Some(card::CODE_DECLINED)),
        (53, Some(card::CODE_CANCELLED)),
        (10, None),
        (151, None),
    ] {
        let r = rig();
        set_reversal(&r, Err(rc));
        let outcome = run(
            &r,
            &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
        )
        .await;
        match expected {
            Some(code) => assert_eq!(failed_code(&outcome), code, "rc {rc}"),
            None => {
                assert!(
                    matches!(outcome, Outcome::Indeterminate(_)),
                    "rc {rc}: {outcome:?}"
                );
                assert!(
                    r.journal
                        .card_payment(REV)
                        .unwrap()
                        .unwrap()
                        .ended_at
                        .is_some()
                );
            }
        }
        assert_eq!(count(&r.fake.calls(), "reversal"), 1, "rc {rc}");
        assert!(r.printer.titles().is_empty());
    }

    // the terminal cannot be reached: nothing reversed, the till is offline
    let r = rig();
    r.fake.0.lock().unwrap().fail = vec!["connect"];
    let outcome = run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(failed_code(&outcome), card::CODE_TERMINAL_UNREACHABLE);
    assert_eq!(r.tim.status(), super::STATUS_OFFLINE);
    let r = rig();
    r.fake.0.lock().unwrap().fail = vec!["login"];
    let outcome = run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(failed_code(&outcome), card::CODE_TERMINAL_ERROR);
}

#[tokio::test]
async fn test_a_contacted_reversal_is_never_sent_again_and_is_reconciled() {
    let r = rig();
    set_reversal(&r, Err(10));
    let j = reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5}));
    let first = run(&r, &j).await;
    assert!(matches!(first, Outcome::Indeterminate(_)));
    // the same key once more (a replay) never transacts twice
    r.journal.reopen_ack(REV).ok();
    let again = r.tim.execute(&j).await;
    assert!(matches!(again, Outcome::Indeterminate(_)), "{again:?}");
    assert_eq!(count(&r.fake.calls(), "reversal"), 1);

    // reconcile after the wait: the terminal's last is our reversal
    r.journal.card_backdate(REV, 3600);
    let tim = Arc::new(
        executor(&r.fake, r.dir.path())
            .with_journal(Arc::clone(&r.journal))
            .with_started_at(chrono::Utc::now() - chrono::Duration::hours(1)),
    );
    let entry = r.journal.get(REV).unwrap().unwrap();
    r.fake.0.lock().unwrap().last = last_of(3, reversal_facts(103, 250));
    match tim.reconcile(&entry).await {
        Reconciliation::Done(result) => {
            assert_eq!(result["state"], "reversed");
            assert_eq!(result["reconciled"], true);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        r.journal.card_payment(REV).unwrap().unwrap().trans_seq,
        Some(103)
    );
    assert_eq!(
        count(&r.fake.calls(), "reversal"),
        1,
        "reconcile only reads"
    );

    // a purchase as the last transaction, or trans_seq == baseline: a person
    for last in [
        last_of(1, reversal_facts(103, 250)),
        last_of(3, reversal_facts(102, 250)),
        last_of(3, reversal_facts(103, 300)),
    ] {
        let r2 = rig();
        set_reversal(&r2, Err(10));
        run(&r2, &j).await;
        r2.journal.card_backdate(REV, 3600);
        let tim = executor(&r2.fake, r2.dir.path())
            .with_journal(Arc::clone(&r2.journal))
            .with_started_at(chrono::Utc::now() - chrono::Duration::hours(1));
        r2.fake.0.lock().unwrap().last = last;
        let entry = r2.journal.get(REV).unwrap().unwrap();
        match tim.reconcile(&entry).await {
            Reconciliation::StillUnknown(why) => assert!(why.contains("person"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(
            r2.journal
                .card_payment(REV)
                .unwrap()
                .unwrap()
                .person_reason
                .is_some()
        );
    }
}

const CREDIT: i64 = 30;

#[tokio::test]
async fn test_credit_is_two_phase_like_a_purchase() {
    let r = rig();
    {
        let mut s = r.fake.0.lock().unwrap();
        s.purchase = Some(Ok(credit_facts(104, 150)));
        s.last = last_of(3, reversal_facts(103, 300));
    }
    let j = typed(
        CREDIT,
        card::JOB_CREDIT,
        json!({"pos_payment_id": 4, "amount": 1.5, "currency": "EUR"}),
    );
    r.journal.insert_received(&j).unwrap();
    assert!(r.journal.start_execution(CREDIT).unwrap());
    let tim = Arc::clone(&r.tim);
    let held = {
        let j = j.clone();
        tokio::spawn(async move { tim.execute(&j).await })
    };
    until("the credit is authorised", || {
        r.journal
            .card_payment(CREDIT)
            .unwrap()
            .and_then(|c| c.authorized_at)
            .is_some()
    })
    .await;
    // the authorised ack is owed for a credit too
    let owed = r.journal.owed_acks().unwrap();
    assert!(
        owed.iter()
            .any(|a| matches!(a, AckRequest::Authorized { job_id, .. } if *job_id == CREDIT))
    );
    let row = r.journal.card_payment(CREDIT).unwrap().unwrap();
    assert_eq!(row.tx_type, card::TRANSACTION_TYPE_CREDIT);
    assert_eq!(row.baseline_seq, Some(103));
    // Odoo's card.commit names the credit job
    let commit = typed(
        31,
        card::JOB_COMMIT,
        json!({"card_job_id": CREDIT, "amount": 1.5}),
    );
    assert_eq!(
        run(&r, &commit).await,
        Outcome::Done(json!({"state": "committed", "card_job_id": CREDIT}))
    );
    match held.await.unwrap() {
        Outcome::Done(result) => {
            assert_eq!(result["state"], "committed");
            assert_eq!(result["transaction"]["type"], "credit");
        }
        other => panic!("{other:?}"),
    }
    let calls = r.fake.calls();
    assert_eq!(count(&calls, "credit"), 1);
    assert_eq!(count(&calls, "purchase"), 0);
    assert!(calls.contains(&"amount 150".to_string()));
    assert_eq!(count(&calls, "commit"), 1);
}

#[tokio::test]
async fn test_credit_deadline_rolls_back_and_settles_answer_from_the_journal() {
    let r = rig_with(Printer::default(), Duration::from_millis(200));
    r.fake.0.lock().unwrap().purchase = Some(Ok(credit_facts(104, 150)));
    let j = typed(CREDIT, card::JOB_CREDIT, json!({"amount": 1.5}));
    let outcome = run(&r, &j).await;
    assert_eq!(failed_code(&outcome), card::CODE_ROLLED_BACK);
    let late = typed(31, card::JOB_COMMIT, json!({"card_job_id": CREDIT}));
    assert_eq!(
        failed_code(&run(&r, &late).await),
        card::CODE_ALREADY_ROLLED_BACK
    );
    // a settle job naming a reversal is not a two-phase payment
    let r2 = rig();
    set_reversal(&r2, Ok(reversal_facts(103, 250)));
    run(
        &r2,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    let bogus = typed(32, card::JOB_ROLLBACK, json!({"card_job_id": REV}));
    assert_eq!(
        failed_code(&run(&r2, &bogus).await),
        card::CODE_UNKNOWN_PURCHASE
    );
}

#[tokio::test]
async fn test_activate_and_deactivate_run_one_session() {
    let r = rig();
    r.fake.0.lock().unwrap().shift = ShiftReport {
        act_seq_counter: Some(3),
        counters: None,
        receipts: Receipts {
            merchant: Some("*** Terminal activation ***".into()),
            cardholder: None,
        },
    };
    let open = typed(40, card::JOB_ACTIVATE, json!({"session_id": 11}));
    match run(&r, &open).await {
        Outcome::Done(result) => {
            assert_eq!(result["state"], "activated");
            assert_eq!(result["act_seq_counter"], 3);
            assert_eq!(result["terminal"]["tid"], "12345678");
            assert_eq!(
                result["receipts"]["merchant"],
                "*** Terminal activation ***"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        r.journal
            .get(40)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_some()
    );
    let close = typed(41, card::JOB_DEACTIVATE, json!({"session_id": 11}));
    match run(&r, &close).await {
        Outcome::Done(result) => assert_eq!(result["state"], "deactivated"),
        other => panic!("{other:?}"),
    }
    let calls = r.fake.calls();
    assert_eq!(
        (count(&calls, "activate"), count(&calls, "deactivate")),
        (1, 1)
    );
    assert_eq!(count(&calls, "logout"), 2);
    // an error AFTER the terminal was asked is unknown, before it nothing ran
    r.fake.0.lock().unwrap().op_rc = Some(10);
    let failed = run(&r, &typed(42, card::JOB_ACTIVATE, json!({}))).await;
    assert!(matches!(failed, Outcome::Indeterminate(_)), "{failed:?}");
    r.fake.0.lock().unwrap().fail = vec!["connect"];
    let unreachable = run(&r, &typed(43, card::JOB_DEACTIVATE, json!({}))).await;
    assert_eq!(failed_code(&unreachable), card::CODE_TERMINAL_UNREACHABLE);
    assert!(
        r.journal
            .get(43)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_none()
    );
}

#[tokio::test]
async fn test_shift_reconcile_runs_it_again_unless_a_later_one_exists() {
    let r = rig();
    // never contacted: nothing ran
    let j = typed(50, card::JOB_DEACTIVATE, json!({}));
    r.journal.insert_received(&j).unwrap();
    let entry = r.journal.get(50).unwrap().unwrap();
    match r.tim.reconcile(&entry).await {
        Reconciliation::Failed { code, .. } => assert_eq!(code, card::CODE_NOT_EXECUTED),
        other => panic!("{other:?}"),
    }
    // contacted, no later shift job: run again (idempotent on the terminal)
    r.journal.mark_device_contact(50).unwrap();
    let entry = r.journal.get(50).unwrap().unwrap();
    match r.tim.reconcile(&entry).await {
        Reconciliation::Done(result) => {
            assert_eq!(result["state"], "deactivated");
            assert_eq!(result["reconciled"], true);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&r.fake.calls(), "deactivate"), 1);
    // a later activate of the till governs: a person, the terminal untouched
    r.journal
        .insert_received(&typed(51, card::JOB_ACTIVATE, json!({})))
        .unwrap();
    match r.tim.reconcile(&entry).await {
        Reconciliation::StillUnknown(why) => assert!(why.contains("later"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&r.fake.calls(), "deactivate"), 1);
}

/// A reversal done by this bridge, its trans_seq journaled.
async fn reversed(r: &Rig) {
    set_reversal(r, Ok(reversal_facts(103, 250)));
    run(
        r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    until("the reversal slip", || !r.printer.titles().is_empty()).await;
}

#[tokio::test]
async fn test_print_copy_of_the_latest_payment() {
    let r = rig();
    reversed(&r).await;
    {
        let mut s = r.fake.0.lock().unwrap();
        s.last = last_of(3, reversal_facts(103, 250));
        s.reprint = Receipts {
            merchant: Some("M".into()),
            cardholder: Some("Reversal\ncopy".into()),
        };
    }
    let copy = typed(80, card::JOB_PRINT_COPY, json!({"card_job_id": REV}));
    assert_eq!(
        run(&r, &copy).await,
        Outcome::Done(
            json!({"state": "printed", "card_job_id": REV, "trans_seq": 103,
                             "receipts": {"merchant": "M", "cardholder": "Reversal\ncopy"}})
        )
    );
    let printed = r.printer.printed.lock().unwrap().clone();
    let last = printed.last().unwrap();
    assert_eq!((last.0, last.1.as_str()), (3, COPY_SLIP_TITLE));
    assert_eq!(last.2, vec!["Reversal".to_string(), "copy".to_string()]);
    assert_eq!(count(&r.fake.calls(), "reprint"), 1);
    assert!(
        r.journal
            .get(80)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_some()
    );
}

#[tokio::test]
async fn test_print_copy_refusals() {
    let r = rig();
    reversed(&r).await;
    // another transaction is the terminal's latest: nothing printed
    r.fake.0.lock().unwrap().last = last_of(1, credit_facts(104, 150));
    let copy = typed(80, card::JOB_PRINT_COPY, json!({"card_job_id": REV}));
    assert_eq!(
        failed_code(&run(&r, &copy).await),
        card::CODE_NOT_LAST_TRANSACTION
    );
    assert_eq!(count(&r.fake.calls(), "reprint"), 0);
    // a job this bridge never ran / not a payment / failed
    for (id, target) in [(81, 999), (82, 80)] {
        let j = typed(id, card::JOB_PRINT_COPY, json!({"card_job_id": target}));
        assert_eq!(
            failed_code(&run(&r, &j).await),
            card::CODE_UNKNOWN_PURCHASE,
            "{target}"
        );
    }
    // the terminal's copy has no cardholder slip
    r.fake.0.lock().unwrap().last = last_of(3, reversal_facts(103, 250));
    let j = typed(83, card::JOB_PRINT_COPY, json!({"card_job_id": REV}));
    assert_eq!(failed_code(&run(&r, &j).await), card::CODE_PRINT_FAILED);
    // the printer refuses
    let failing = rig_with(
        Printer {
            fail: true,
            ..Printer::default()
        },
        Duration::from_secs(10),
    );
    reversed(&failing).await;
    {
        let mut s = failing.fake.0.lock().unwrap();
        s.last = last_of(3, reversal_facts(103, 250));
        s.reprint.cardholder = Some("x".into());
    }
    assert_eq!(
        failed_code(&run(&failing, &copy).await),
        card::CODE_PRINT_FAILED
    );
    // no printer at all
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let fake = FakeTim::failing(&[]);
    let bare = executor(&fake, dir.path()).with_journal(Arc::clone(&journal));
    fake.0.lock().unwrap().reversal = Some(Ok(reversal_facts(103, 250)));
    let rev = reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5}));
    journal.insert_received(&rev).unwrap();
    journal.start_execution(REV).unwrap();
    let o = bare.execute(&rev).await;
    journal.record_outcome(REV, &o).unwrap();
    journal.insert_received(&copy).unwrap();
    journal.start_execution(80).unwrap();
    assert_eq!(
        failed_code(&bare.execute(&copy).await),
        card::CODE_NO_PRINTER
    );
    // an interrupted copy reconciles as failed (no money, ask again)
    let entry = journal.get(80).unwrap().unwrap();
    assert!(entry.device_contact_at.is_none());
    journal.mark_device_contact(80).unwrap();
    let entry = journal.get(80).unwrap().unwrap();
    match bare.reconcile(&entry).await {
        Reconciliation::Failed { code, .. } => assert_eq!(code, card::CODE_PRINT_UNKNOWN),
        other => panic!("{other:?}"),
    }
    assert_eq!(entry.state, JobState::Executing);
}

#[tokio::test]
async fn test_d3_jobs_without_library_or_journal_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let no_journal = executor(&fake, dir.path());
    for (job_type, payload) in [
        (card::JOB_CREDIT, json!({"amount": 1.0})),
        (card::JOB_REVERSAL, json!({"trans_seq": 1, "amount": 1.0})),
        (card::JOB_ACTIVATE, json!({})),
        (card::JOB_DEACTIVATE, json!({})),
        (card::JOB_BALANCE, json!({})),
        (card::JOB_PRINT_COPY, json!({"card_job_id": 7})),
    ] {
        let rejection = no_journal.check(&job(job_type, 3, payload)).unwrap_err();
        assert_eq!(rejection.code, card::CODE_TIM_UNAVAILABLE, "{job_type}");
        let nothing = if job_type == card::JOB_CREDIT || job_type == card::JOB_REVERSAL {
            "nothing charged"
        } else {
            "nothing ran"
        };
        assert!(
            rejection.text.contains(nothing),
            "{job_type}: {}",
            rejection.text
        );
    }
    assert!(
        fake.calls().iter().all(|c| c == "settings"),
        "{:?}",
        fake.calls()
    );
}

#[tokio::test]
async fn test_rc_67_is_period_closed_only_after_a_balance_since_the_payment() {
    let r = rig();
    // the payment 102 this bridge charged, an hour ago
    {
        let mut s = r.fake.0.lock().unwrap();
        s.purchase = Some(Ok(credit_facts(102, 250)));
    }
    let pay = typed(10, card::JOB_PURCHASE, json!({"amount": 2.5}));
    r.journal.insert_received(&pay).unwrap();
    r.journal.start_execution(10).unwrap();
    assert!(
        r.journal
            .card_contact(10, "10.78.2.8", 250, Some(101))
            .unwrap()
    );
    r.journal.card_authorized(10, 102, &json!({})).unwrap();
    r.journal.card_backdate(10, 3600);
    set_reversal(&r, Err(67));
    let first = run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(
        failed_code(&first),
        card::CODE_UNKNOWN_REFERENCE,
        "no balance since"
    );
    // a balance ran after the payment: the period is proven closed
    let b = r
        .journal
        .card_balance_contact("10.78.2.8", "2026-11-04", SOURCE_SAFETY, None)
        .unwrap()
        .unwrap();
    r.journal
        .card_balance_outcome(b, "done", &json!({}))
        .unwrap();
    let second = run(
        &r,
        &reversal_job(REV + 1, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(failed_code(&second), card::CODE_PERIOD_CLOSED);
    assert_eq!(
        count(&r.fake.calls(), "reversal"),
        2,
        "both were sent: each failed"
    );
}

#[tokio::test]
async fn test_a_trans_seq_is_never_reversed_twice() {
    // the first reversal's answer is lost: unknown
    let r = rig();
    set_reversal(&r, Err(10));
    let first = run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert!(matches!(first, Outcome::Indeterminate(_)));
    // a second job for the same TransSeq (a retry) is never sent - and an
    // rc 67 the terminal would give is never read as a closed period
    set_reversal(&r, Err(67));
    let retry = run(
        &r,
        &reversal_job(REV + 1, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(failed_code(&retry), card::CODE_REVERSAL_UNRESOLVED);
    assert_eq!(count(&r.fake.calls(), "reversal"), 1);
    assert!(
        r.journal.card_payment(REV + 1).unwrap().is_none(),
        "never contacted"
    );
    // once the first is done, another is refused as already reversed
    let r = rig();
    set_reversal(&r, Ok(reversal_facts(103, 250)));
    run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    let again = run(
        &r,
        &reversal_job(REV + 1, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert_eq!(failed_code(&again), card::CODE_ALREADY_REVERSED);
    assert_eq!(count(&r.fake.calls(), "reversal"), 1);
    // after a FAILED one (nothing reversed) a new reversal is sent
    let r = rig();
    set_reversal(&r, Err(82));
    run(
        &r,
        &reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    set_reversal(&r, Ok(reversal_facts(103, 250)));
    let ok = run(
        &r,
        &reversal_job(REV + 1, json!({"trans_seq": 102, "amount": 2.5})),
    )
    .await;
    assert!(matches!(ok, Outcome::Done(_)), "{ok:?}");
    assert_eq!(count(&r.fake.calls(), "reversal"), 2);
}

#[tokio::test]
async fn test_a_reconciled_failed_reversal_is_not_reversed() {
    let r = rig();
    set_reversal(&r, Err(10));
    let j = reversal_job(REV, json!({"trans_seq": 102, "amount": 2.5}));
    run(&r, &j).await;
    r.journal.card_backdate(REV, 3600);
    let tim = executor(&r.fake, r.dir.path())
        .with_journal(Arc::clone(&r.journal))
        .with_started_at(chrono::Utc::now() - chrono::Duration::hours(1));
    r.fake.0.lock().unwrap().last = LastTransaction {
        transaction_type: card::TRANSACTION_TYPE_REVERSAL,
        outcome: card::OUTCOME_FAILED,
        facts: Some(reversal_facts(103, 250)),
    };
    let entry = r.journal.get(REV).unwrap().unwrap();
    match tim.reconcile(&entry).await {
        Reconciliation::Failed { code, text } => {
            assert_eq!(code, card::CODE_NOT_REVERSED);
            assert!(text.contains("NOT reversed"), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_print_copy_only_of_a_live_payment_of_the_same_till() {
    let r = rig();
    {
        let mut s = r.fake.0.lock().unwrap();
        s.last = last_of(1, credit_facts(103, 250));
        s.reprint.cardholder = Some("copy".into());
    }
    // a payment with that trans_seq, but of ANOTHER till
    let mut other = typed(90, card::JOB_PURCHASE, json!({"amount": 2.5}));
    other.kasa_id = Some(4);
    r.journal.insert_received(&other).unwrap();
    r.journal.start_execution(90).unwrap();
    r.journal
        .card_contact(90, "10.78.2.18", 250, Some(1))
        .unwrap();
    r.journal.card_authorized(90, 103, &json!({})).unwrap();
    r.journal
        .record_outcome(90, &Outcome::Done(json!({})))
        .unwrap();
    let copy = typed(91, card::JOB_PRINT_COPY, json!({"card_job_id": 90}));
    assert_eq!(
        failed_code(&run(&r, &copy).await),
        card::CODE_UNKNOWN_PURCHASE
    );
    // a payment of this till that was rolled back (failed): no receipt
    let rolled = typed(92, card::JOB_PURCHASE, json!({"amount": 2.5}));
    r.journal.insert_received(&rolled).unwrap();
    r.journal.start_execution(92).unwrap();
    r.journal
        .card_contact(92, "10.78.2.8", 250, Some(1))
        .unwrap();
    r.journal.card_authorized(92, 103, &json!({})).unwrap();
    r.journal
        .record_outcome(92, &Outcome::failed("rolled_back", "deadline"))
        .unwrap();
    let copy = typed(93, card::JOB_PRINT_COPY, json!({"card_job_id": 92}));
    assert_eq!(
        failed_code(&run(&r, &copy).await),
        card::CODE_UNKNOWN_PURCHASE
    );
    assert_eq!(count(&r.fake.calls(), "reprint"), 0);
    // the same payment while still held (executing) can be copied
    let held = typed(94, card::JOB_PURCHASE, json!({"amount": 2.5}));
    r.journal.insert_received(&held).unwrap();
    r.journal.start_execution(94).unwrap();
    r.journal
        .card_contact(94, "10.78.2.8", 250, Some(1))
        .unwrap();
    r.journal.card_authorized(94, 103, &json!({})).unwrap();
    let copy = typed(95, card::JOB_PRINT_COPY, json!({"card_job_id": 94}));
    assert!(matches!(run(&r, &copy).await, Outcome::Done(_)));
}
