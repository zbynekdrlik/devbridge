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
