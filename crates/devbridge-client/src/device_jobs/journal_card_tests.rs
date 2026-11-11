//! The `card_payments` table (#102 D2).

use std::time::Duration;

use serde_json::json;

use crate::device_jobs::api::{AckRequest, parse_job};
use crate::device_jobs::executor::Outcome;
use crate::device_jobs::journal::{JobState, Journal};

fn executing(j: &Journal, id: i64) {
    let job =
        parse_job(&json!({"id": id, "type": "card.purchase", "payload": {"amount": 2.5}})).unwrap();
    j.insert_received(&job).unwrap();
    assert!(j.start_execution(id).unwrap());
}

#[test]
fn test_contact_is_written_once_with_the_device_contact() {
    let j = Journal::open_in_memory().unwrap();
    // not executing: never contacted
    let job = parse_job(&json!({"id": 1, "type": "card.purchase"})).unwrap();
    j.insert_received(&job).unwrap();
    assert!(!j.card_contact(1, "10.78.2.8", 250, Some(100)).unwrap());
    assert!(j.card_payment(1).unwrap().is_none());
    assert!(
        !j.card_contact(99, "10.78.2.8", 250, None).unwrap(),
        "no such job"
    );
    executing(&j, 2);
    assert!(j.card_contact(2, "10.78.2.8", 250, Some(100)).unwrap());
    assert!(
        !j.card_contact(2, "10.78.2.8", 250, Some(100)).unwrap(),
        "a second contact is refused"
    );
    let row = j.card_payment(2).unwrap().unwrap();
    assert_eq!(
        (row.terminal_ip.as_str(), row.amount_minor, row.baseline_seq),
        ("10.78.2.8", 250, Some(100))
    );
    assert!(row.authorized_at.is_none() && row.ended_at.is_none() && row.person_reason.is_none());
    assert!(j.get(2).unwrap().unwrap().device_contact_at.is_some());
}

#[tokio::test]
async fn test_authorised_ack_owed_only_while_held() {
    let j = Journal::open_in_memory().unwrap();
    executing(&j, 3);
    j.card_contact(3, "10.78.2.8", 250, None).unwrap();
    assert!(j.owed_acks().unwrap().is_empty());
    assert_eq!(j.card_last_seq("10.78.2.8").unwrap(), None);
    // the acker is woken by the authorisation
    let woken = tokio::time::timeout(Duration::from_secs(5), j.card_ack_owed());
    assert!(
        j.card_authorized(3, 101, &json!({"state": "authorized"}))
            .unwrap()
    );
    woken.await.expect("acker woken");
    assert!(
        !j.card_authorized(3, 102, &json!({})).unwrap(),
        "the first authorisation is kept"
    );
    assert_eq!(j.card_last_seq("10.78.2.8").unwrap(), Some(101));
    assert_eq!(j.card_last_seq("10.78.2.9").unwrap(), None);
    assert_eq!(
        j.owed_acks().unwrap(),
        [AckRequest::Authorized {
            job_id: 3,
            result: json!({"state": "authorized"})
        }]
    );
    assert!(j.mark_card_auth_acked(3, "ok").unwrap());
    assert!(
        !j.mark_card_auth_acked(3, "ok").unwrap(),
        "only an owed one"
    );
    assert!(j.owed_acks().unwrap().is_empty());
    assert!(!j.card_payment(3).unwrap().unwrap().auth_refused);

    // Odoo refused it: the payment reads refused
    executing(&j, 4);
    j.card_contact(4, "10.78.2.8", 250, Some(101)).unwrap();
    j.card_authorized(4, 102, &json!({})).unwrap();
    assert!(j.mark_card_auth_acked(4, "refused: too_late").unwrap());
    assert!(j.card_payment(4).unwrap().unwrap().auth_refused);

    // decided before the ack went out: only the final ack is owed
    executing(&j, 5);
    j.card_contact(5, "10.78.2.8", 250, Some(102)).unwrap();
    j.card_authorized(5, 103, &json!({})).unwrap();
    j.record_outcome(5, &Outcome::failed("rolled_back", "deadline"))
        .unwrap();
    let owed = j.owed_acks().unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].kind(), "failed");
    // a restart while held: indeterminate, no stale authorisation
    executing(&j, 6);
    j.card_contact(6, "10.78.2.8", 250, Some(103)).unwrap();
    j.card_authorized(6, 104, &json!({})).unwrap();
    j.recover_after_restart().unwrap();
    assert_eq!(j.get(6).unwrap().unwrap().state, JobState::Indeterminate);
    assert!(j.owed_acks().unwrap().iter().all(|a| a.job_id() != 6));
}

#[test]
fn test_end_person_and_prune() {
    let j = Journal::open_in_memory().unwrap();
    executing(&j, 7);
    j.card_contact(7, "10.78.2.8", 250, None).unwrap();
    assert!(j.card_ended(7).unwrap());
    assert!(!j.card_ended(8).unwrap());
    assert!(j.card_for_person(7, "first").unwrap());
    assert!(
        !j.card_for_person(7, "second").unwrap(),
        "the first reason is kept"
    );
    let row = j.card_payment(7).unwrap().unwrap();
    assert!(row.ended_at.is_some());
    assert_eq!(row.person_reason.as_deref(), Some("first"));
    // the card row goes with its job row
    j.record_outcome(7, &Outcome::failed("rolled_back", "x"))
        .unwrap();
    j.mark_acked(7, "ok").unwrap();
    j.set_updated_at(7, "2000-01-01T00:00:00+00:00");
    assert_eq!(
        j.prune().unwrap(),
        1,
        "the job row, card rows are not counted"
    );
    assert!(j.card_payment(7).unwrap().is_none());
}

#[test]
fn test_later_payment_on_the_same_terminal() {
    let j = Journal::open_in_memory().unwrap();
    let set = |id: i64, at: &str| {
        j.conn()
            .execute(
                "UPDATE card_payments SET contact_at = ?2 WHERE job_id = ?1",
                rusqlite::params![id, at],
            )
            .unwrap();
    };
    for (id, ip) in [(1, "10.78.2.8"), (2, "10.78.2.8"), (3, "10.78.2.9")] {
        executing(&j, id);
        j.card_contact(id, ip, 250, None).unwrap();
    }
    set(1, "2026-11-04T10:00:00+00:00");
    set(2, "2026-11-04T10:00:01+00:00");
    set(3, "2026-11-04T11:00:00+00:00");
    assert!(j.card_later_on_terminal(1).unwrap(), "2 came after 1");
    assert!(
        !j.card_later_on_terminal(2).unwrap(),
        "nothing after 2 on its terminal"
    );
    assert!(!j.card_later_on_terminal(3).unwrap(), "another terminal");
    assert!(!j.card_later_on_terminal(99).unwrap(), "no such row");
    set(2, "2026-11-04T10:00:00+00:00");
    assert!(
        j.card_later_on_terminal(1).unwrap(),
        "the same moment counts as later"
    );
    set(2, "not a time");
    assert!(
        j.card_later_on_terminal(1).unwrap(),
        "unreadable counts as later"
    );
}

#[tokio::test]
async fn test_card_ack_wake_only_on_an_authorisation() {
    let j = Journal::open_in_memory().unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(50), j.card_ack_owed()).await;
    assert!(quiet.is_err(), "no authorisation, no wake-up");
}

#[test]
fn test_card_rows_carry_their_transaction_type() {
    let j = Journal::open_in_memory().unwrap();
    executing(&j, 11);
    executing(&j, 12);
    assert!(j.card_contact(11, "10.78.2.8", 250, Some(100)).unwrap());
    assert!(
        j.card_contact_of(12, "10.78.2.8", 250, Some(101), 3, Some(100))
            .unwrap()
    );
    assert_eq!(j.card_payment(11).unwrap().unwrap().tx_type, 1, "purchase");
    let reversal = j.card_payment(12).unwrap().unwrap();
    assert_eq!(reversal.tx_type, 3);
    assert_eq!(reversal.trans_seq, None);
    assert_eq!(reversal.target_seq, Some(100));
    assert_eq!(j.card_payment(11).unwrap().unwrap().target_seq, None);
    // a reversal's own trans_seq: stored once, no authorised ack owed
    assert!(j.card_transacted(12, 102).unwrap());
    assert!(
        !j.card_transacted(12, 999).unwrap(),
        "the first one is kept"
    );
    assert!(!j.card_transacted(77, 1).unwrap(), "no row");
    assert_eq!(j.card_payment(12).unwrap().unwrap().trans_seq, Some(102));
    assert_eq!(j.card_last_seq("10.78.2.8").unwrap(), Some(102));
    assert!(j.owed_acks().unwrap().is_empty());
}

#[test]
fn test_a_d2_card_table_gets_the_type_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE card_payments (job_id INTEGER PRIMARY KEY, terminal_ip TEXT NOT NULL,
                 amount_minor INTEGER NOT NULL, baseline_seq INTEGER, contact_at TEXT NOT NULL,
                 authorized_at TEXT, trans_seq INTEGER, auth_result TEXT,
                 auth_ack INTEGER NOT NULL DEFAULT 0, auth_ack_note TEXT, ended_at TEXT,
                 person_reason TEXT);
             INSERT INTO card_payments (job_id, terminal_ip, amount_minor, contact_at)
                 VALUES (5, '10.78.2.8', 250, '2026-11-04T10:00:00+00:00');",
        )
        .unwrap();
    }
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.card_payment(5).unwrap().unwrap().tx_type, 1);
    drop(j);
    // opening again keeps it
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.card_payment(5).unwrap().unwrap().tx_type, 1);
}

#[test]
fn test_card_job_by_uuid_is_the_tills_newest_card_job() {
    let j = Journal::open_in_memory().unwrap();
    let job = |id: i64, ty: &str, kasa: i64, uuid: &str| {
        parse_job(&json!({"id": id, "type": ty, "client_uuid": uuid,
                          "kasa": {"id": kasa}, "payload": {}}))
        .unwrap()
    };
    assert!(j.card_job_by_uuid(3, "u1").unwrap().is_none());
    j.insert_received(&job(1, "card.purchase", 3, "u1"))
        .unwrap();
    j.insert_received(&job(2, "card.purchase", 3, "u1"))
        .unwrap();
    j.insert_received(&job(3, "fiscal.cash_register", 3, "u2"))
        .unwrap();
    j.insert_received(&job(4, "card.purchase", 4, "u3"))
        .unwrap();
    let (entry, card) = j.card_job_by_uuid(3, "u1").unwrap().unwrap();
    assert_eq!(entry.job.id, 2, "the newest");
    assert!(card.is_none());
    assert!(
        j.card_job_by_uuid(3, "u2").unwrap().is_none(),
        "not a card job"
    );
    assert!(
        j.card_job_by_uuid(3, "u3").unwrap().is_none(),
        "another till's"
    );
    assert!(j.card_job_by_uuid(4, "u3").unwrap().is_some());
    // an Odoo-composed settle / shift job never answers for the payment
    for (id, ty) in [
        (5, "card.rollback"),
        (6, "card.commit"),
        (7, "card.activate"),
        (8, "card.deactivate"),
    ] {
        j.insert_received(&job(id, ty, 3, "u1")).unwrap();
    }
    assert_eq!(j.card_job_by_uuid(3, "u1").unwrap().unwrap().0.job.id, 2);
    j.insert_received(&job(9, "card.reversal", 3, "u4"))
        .unwrap();
    assert_eq!(j.card_job_by_uuid(3, "u4").unwrap().unwrap().0.job.id, 9);
}

#[test]
fn test_card_job_by_uuid_never_answers_with_a_cancel() {
    // card.cancel (lane V) is composed by Odoo for a till's payment: the
    // till's GET of its payment must still read the payment
    let j = Journal::open_in_memory().unwrap();
    let job = |id: i64, ty: &str| {
        parse_job(&json!({"id": id, "type": ty, "client_uuid": "u1",
                          "kasa": {"id": 3}, "payload": {}}))
        .unwrap()
    };
    j.insert_received(&job(1, "card.purchase")).unwrap();
    j.insert_received(&job(2, "card.cancel")).unwrap();
    assert_eq!(j.card_job_by_uuid(3, "u1").unwrap().unwrap().0.job.id, 1);
}

#[test]
fn test_other_reversal_of_a_trans_seq() {
    let j = Journal::open_in_memory().unwrap();
    let ip = "10.78.2.8";
    for id in [21, 22, 23] {
        let job = parse_job(&json!({"id": id, "type": "card.reversal", "payload": {}})).unwrap();
        j.insert_received(&job).unwrap();
        assert!(j.start_execution(id).unwrap());
    }
    assert!(
        j.card_contact_of(21, ip, 250, Some(1), 3, Some(100))
            .unwrap()
    );
    // the job itself does not count
    assert_eq!(j.card_other_reversal(ip, 100, 21).unwrap(), None);
    let (other, state) = j.card_other_reversal(ip, 100, 22).unwrap().unwrap();
    assert_eq!((other, state), (21, JobState::Executing));
    // another TransSeq / terminal is its own
    assert_eq!(j.card_other_reversal(ip, 101, 22).unwrap(), None);
    assert_eq!(j.card_other_reversal("10.78.2.18", 100, 22).unwrap(), None);
    j.record_outcome(21, &Outcome::Done(json!({}))).unwrap();
    assert_eq!(
        j.card_other_reversal(ip, 100, 22).unwrap(),
        Some((21, JobState::Done))
    );
    // a failed reversal reversed nothing: it does not block another
    assert!(
        j.card_contact_of(23, ip, 250, Some(1), 3, Some(200))
            .unwrap()
    );
    j.record_outcome(23, &Outcome::failed("declined", "no"))
        .unwrap();
    assert_eq!(j.card_other_reversal(ip, 200, 22).unwrap(), None);
    // a purchase with that seq is no reversal
    executing(&j, 24);
    assert!(j.card_contact(24, ip, 250, Some(1)).unwrap());
    j.card_authorized(24, 300, &json!({})).unwrap();
    assert_eq!(j.card_other_reversal(ip, 300, 22).unwrap(), None);
}

#[test]
fn test_balance_since_a_payment() {
    let j = Journal::open_in_memory().unwrap();
    let ip = "10.78.2.8";
    executing(&j, 31);
    assert!(j.card_contact(31, ip, 250, Some(1)).unwrap());
    j.card_authorized(31, 102, &json!({})).unwrap();
    j.card_backdate(31, 3600);
    assert!(!j.card_balance_since_payment(ip, 102).unwrap());
    // a balance BEFORE the payment does not count
    let early = j
        .card_balance_contact(ip, "2026-11-03", "job", Some(1))
        .unwrap()
        .unwrap();
    j.card_balance_outcome(early, "done", &json!({})).unwrap();
    j.card_balances_backdate(1);
    assert!(!j.card_balance_since_payment(ip, 102).unwrap());
    // a balance after it counts only once it ended done
    let after = j
        .card_balance_contact(ip, "2026-11-04", "safety", None)
        .unwrap()
        .unwrap();
    assert!(
        !j.card_balance_since_payment(ip, 102).unwrap(),
        "outcome unknown"
    );
    j.card_balance_outcome(after, "done", &json!({})).unwrap();
    assert!(j.card_balance_since_payment(ip, 102).unwrap());
    assert!(
        !j.card_balance_since_payment(ip, 999).unwrap(),
        "an unknown payment"
    );
    assert!(!j.card_balance_since_payment("10.78.2.18", 102).unwrap());
}
