//! `card.balance` and the 23:50 safety balance over the fake TIM (#102 D3,
//! CI — no SDK): the result and its journal row, once a day per terminal,
//! a restart after 23:50, a refusal retried, an unknown answer kept, the
//! in-session re-check and the midnight cut-off.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use serde_json::json;

use super::ops::{SafetyEnd, safety_session, sim_balance};
use super::ops_tests::{count, failed_code, rig, run, typed};
use super::tests::{executor, spec};
use super::{TimBackend, TimExecutor, card};
use crate::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use crate::device_jobs::journal::{Journal, SOURCE_JOB, SOURCE_SAFETY};

#[tokio::test]
async fn test_balance_job_result_and_journal() {
    let r = rig();
    r.fake.0.lock().unwrap().balance = sim_balance();
    let j = typed(60, card::JOB_BALANCE, json!({}));
    match run(&r, &j).await {
        Outcome::Done(result) => {
            assert_eq!(result["state"], "closed");
            assert_eq!(result["period"], 3);
            assert_eq!(result["brands"][0]["brand_name"], "VISA");
            assert_eq!(result["brands"][0]["count"], 1);
            assert_eq!(
                result["total"],
                json!({"currency": "EUR", "count": 1, "amount": 1.0})
            );
            assert_eq!(result["receipts"]["merchant"], "*** Daily closing ***");
        }
        other => panic!("{other:?}"),
    }
    let row = r.journal.card_balance_of_job(60).unwrap().unwrap();
    assert_eq!(row.source, SOURCE_JOB);
    assert_eq!(row.outcome.as_deref(), Some("done"));
    // the store's local date at the call (a run across midnight sees either)
    let days =
        [Local::now() - chrono::Duration::minutes(1), Local::now()].map(super::ops::local_date);
    assert!(days.contains(&row.local_date), "{}", row.local_date);
    assert!(
        r.journal
            .card_balance_ran("10.78.2.8", &row.local_date)
            .unwrap()
    );
    // an interrupted balance cannot be proven: a person, the terminal untouched
    r.fake.0.lock().unwrap().op_rc = Some(10);
    let lost = typed(61, card::JOB_BALANCE, json!({}));
    assert!(matches!(run(&r, &lost).await, Outcome::Indeterminate(_)));
    assert_eq!(
        r.journal.card_balance_of_job(61).unwrap().unwrap().outcome,
        None
    );
    let entry = r.journal.get(61).unwrap().unwrap();
    let before = count(&r.fake.calls(), "balance");
    match r.tim.reconcile(&entry).await {
        Reconciliation::StillUnknown(why) => assert!(why.contains("person"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&r.fake.calls(), "balance"), before);
}

fn at(h: u32, m: u32) -> chrono::DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 4, h, m, 0).unwrap()
}

#[tokio::test]
async fn test_safety_balance_once_at_2350_when_none_ran() {
    let r = rig();
    r.fake.0.lock().unwrap().balance = sim_balance();
    assert_eq!(
        r.tim.safety_tick(at(23, 49)).await,
        (0, 0),
        "not before 23:50"
    );
    assert_eq!(count(&r.fake.calls(), "balance"), 0);
    // both terminals (tills 3 and 4) get one at 23:50
    assert_eq!(r.tim.safety_tick(at(23, 50)).await, (2, 2));
    assert_eq!(count(&r.fake.calls(), "balance"), 2);
    let rows = r.journal.card_balances("10.78.2.8").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, SOURCE_SAFETY);
    assert_eq!(rows[0].local_date, "2026-11-04");
    assert_eq!(rows[0].outcome.as_deref(), Some("done"));
    assert_eq!(rows[0].result.as_ref().unwrap()["total"]["amount"], 1.0);
    // later the same evening: none again
    assert_eq!(r.tim.safety_tick(at(23, 59)).await, (0, 0));
    // a restart after 23:50 (a new executor over the same journal): none
    let restarted = executor(&r.fake, r.dir.path()).with_journal(Arc::clone(&r.journal));
    assert_eq!(restarted.safety_tick(at(23, 55)).await, (0, 0));
    assert_eq!(count(&r.fake.calls(), "balance"), 2);
    // the next day is its own
    let next = Local.with_ymd_and_hms(2026, 11, 5, 23, 50, 0).unwrap();
    assert_eq!(restarted.safety_tick(next).await, (2, 2));
}

#[tokio::test]
async fn test_no_safety_balance_after_a_balance_job_that_day() {
    let r = rig();
    r.fake.0.lock().unwrap().balance = sim_balance();
    let id = r
        .journal
        .card_balance_contact("10.78.2.8", "2026-11-04", SOURCE_JOB, Some(70))
        .unwrap()
        .unwrap();
    r.journal
        .card_balance_outcome(id, "done", &json!({}))
        .unwrap();
    // only the other terminal (till 4) had none
    assert_eq!(r.tim.safety_tick(at(23, 50)).await, (1, 1));
    assert_eq!(r.journal.card_balances("10.78.2.8").unwrap().len(), 1);
    assert_eq!(r.journal.card_balances("10.78.2.18").unwrap().len(), 1);
}

#[tokio::test]
async fn test_unreachable_safety_balance_is_retried_that_evening() {
    let r = rig();
    r.fake.0.lock().unwrap().fail = vec!["connect"];
    assert_eq!(
        r.tim.safety_tick(at(23, 50)).await,
        (2, 0),
        "tried, not reached"
    );
    assert!(
        r.journal.card_balances("10.78.2.8").unwrap().is_empty(),
        "nothing ran - not counted for the day"
    );
    r.fake.0.lock().unwrap().fail.clear();
    r.fake.0.lock().unwrap().balance = sim_balance();
    assert_eq!(r.tim.safety_tick(at(23, 51)).await, (2, 2));
    assert_eq!(count(&r.fake.calls(), "balance"), 2);
    // without the library nothing is even journaled
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let none = TimExecutor::with_backend(
        vec![super::tests::spec(3, "10.78.2.8", None)],
        Err("missing".into()),
        dir.path(),
    )
    .with_journal(Arc::clone(&journal));
    assert_eq!(none.safety_tick(at(23, 50)).await, (0, 0));
    assert!(journal.card_balances("10.78.2.8").unwrap().is_empty());
}

#[test]
fn test_safety_due() {
    use super::safety::{safety_due, safety_time};
    let t = |h, m, s| chrono::NaiveTime::from_hms_opt(h, m, s).unwrap();
    assert_eq!(safety_time(), t(23, 50, 0));
    assert!(!safety_due(t(0, 0, 0)));
    assert!(!safety_due(t(23, 49, 59)));
    assert!(safety_due(t(23, 50, 0)));
    assert!(safety_due(t(23, 59, 59)));
}

#[tokio::test]
async fn test_the_safety_balance_runs_as_the_executors_background_task() {
    let r = rig();
    // it loops for as long as the source runs: never returns on its own
    let ran = tokio::time::timeout(Duration::from_millis(200), r.tim.background()).await;
    assert!(ran.is_err(), "the background task returned");
}

#[tokio::test]
async fn test_a_refused_balance_ran_nothing() {
    // a balance job the terminal refuses as busy: failed, nothing ran
    let r = rig();
    r.fake.0.lock().unwrap().op_rc = Some(69);
    let j = typed(62, card::JOB_BALANCE, json!({}));
    assert_eq!(failed_code(&run(&r, &j).await), card::CODE_TERMINAL_BUSY);
    let row = r.journal.card_balance_of_job(62).unwrap().unwrap();
    assert_eq!(row.outcome.as_deref(), Some("failed"));
    assert!(
        !r.journal
            .card_balance_ran("10.78.2.8", &row.local_date)
            .unwrap()
    );
    // the safety balance refused: no row kept, retried the next minute
    let r = rig();
    r.fake.0.lock().unwrap().op_rc = Some(69);
    assert_eq!(
        r.tim.safety_tick(at(23, 50)).await,
        (2, 0),
        "refused: not reached"
    );
    assert!(r.journal.card_balances("10.78.2.8").unwrap().is_empty());
    r.fake.0.lock().unwrap().op_rc = None;
    r.fake.0.lock().unwrap().balance = sim_balance();
    assert_eq!(r.tim.safety_tick(at(23, 51)).await, (2, 2));
    assert_eq!(count(&r.fake.calls(), "balance"), 4);
    assert_eq!(r.tim.safety_tick(at(23, 52)).await, (0, 0));
    // an unknown answer keeps the row: no second balance that day
    let r = rig();
    r.fake.0.lock().unwrap().op_rc = Some(10);
    assert_eq!(
        r.tim.safety_tick(at(23, 50)).await,
        (2, 2),
        "sent, answer unknown"
    );
    let rows = r.journal.card_balances("10.78.2.8").unwrap();
    assert_eq!((rows.len(), rows[0].outcome.clone()), (1, None));
    assert_eq!(r.tim.safety_tick(at(23, 51)).await, (0, 0));
}

#[tokio::test]
async fn test_a_balance_job_while_the_safety_balance_waits_is_not_doubled() {
    let r = rig();
    r.fake.0.lock().unwrap().balance = sim_balance();
    // a balance job ended done before the safety balance got the terminal
    let id = r
        .journal
        .card_balance_contact("10.78.2.8", "2026-11-04", SOURCE_JOB, Some(70))
        .unwrap()
        .unwrap();
    r.journal
        .card_balance_outcome(id, "done", &json!({}))
        .unwrap();
    let spec = super::tests::spec(3, "10.78.2.8", Some("12345678"));
    let until = Instant::now() + Duration::from_secs(60);
    assert_eq!(r.tim.safety_balance(spec, "2026-11-04", until).await, None);
    assert_eq!(count(&r.fake.calls(), "connect"), 0, "not even a session");
}

/// One session's terminal of the fake (connected, as `with_session` hands it).
fn fake_terminal(r: &super::ops_tests::Rig) -> Box<dyn super::TimTerminal> {
    r.fake.terminal(&spec(3, "10.78.2.8", None)).unwrap()
}

#[tokio::test]
async fn test_the_safety_session_rechecks_the_day_under_the_lock() {
    let r = rig();
    r.fake.0.lock().unwrap().balance = sim_balance();
    let later = Instant::now() + Duration::from_secs(60);
    // a balance job ended done while the safety balance waited for the lock
    let id = r
        .journal
        .card_balance_contact("10.78.2.8", "2026-11-04", SOURCE_JOB, Some(71))
        .unwrap()
        .unwrap();
    r.journal
        .card_balance_outcome(id, "done", &json!({}))
        .unwrap();
    let mut t = fake_terminal(&r);
    let ended = safety_session(
        t.as_mut(),
        None,
        &r.journal,
        "10.78.2.8",
        "2026-11-04",
        later,
    );
    assert_eq!(ended, SafetyEnd::AlreadyRan);
    assert_eq!(count(&r.fake.calls(), "balance"), 0);
    // none ran: the row is journaled, then the balance
    let ended = safety_session(
        t.as_mut(),
        Some("1".into()),
        &r.journal,
        "10.78.2.18",
        "2026-11-04",
        later,
    );
    let SafetyEnd::Sent(row, Outcome::Done(result)) = ended else {
        panic!("{ended:?}")
    };
    assert_eq!(result["state"], "closed");
    assert_eq!(r.journal.card_balances("10.78.2.18").unwrap()[0].id, row);
    // the day is over while it waited: nothing sent, nothing journaled
    let past = Instant::now();
    let ended = safety_session(
        t.as_mut(),
        None,
        &r.journal,
        "10.78.2.99",
        "2026-11-04",
        past,
    );
    assert_eq!(ended, SafetyEnd::AlreadyRan);
    assert!(r.journal.card_balances("10.78.2.99").unwrap().is_empty());
    assert_eq!(count(&r.fake.calls(), "balance"), 1);
}

#[test]
fn test_until_midnight() {
    use super::safety::until_midnight;
    assert_eq!(until_midnight(at(23, 50)), Duration::from_secs(600));
    assert_eq!(until_midnight(at(0, 0)), Duration::from_secs(86_400));
    assert_eq!(
        until_midnight(Local.with_ymd_and_hms(2026, 11, 4, 23, 59, 59).unwrap()),
        Duration::from_secs(1)
    );
}

#[test]
fn test_outcome_total_for_the_log_line() {
    use super::ops::outcome_total;
    let done = Outcome::Done(json!({"total": {"amount": 1.0}}));
    assert_eq!(outcome_total(&done), Some(&json!({"amount": 1.0})));
    assert_eq!(outcome_total(&Outcome::Done(json!({}))), None);
    assert_eq!(outcome_total(&Outcome::failed("x", "y")), None);
    assert_eq!(outcome_total(&Outcome::Indeterminate("z".into())), None);
}
