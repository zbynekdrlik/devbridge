//! Unit tests of the device journal (#99, #100).

use super::*;
use serde_json::json;

fn job(id: i64, job_type: &str) -> DeviceJob {
    DeviceJob {
        id,
        job_type: job_type.into(),
        client_uuid: format!("uuid-{id}"),
        kasa_id: Some(3),
        kasa_name: "Pokladňa Bastion 1".into(),
        pos_order: json!({"id": 812, "name": "Bastion/0042"}),
        payload: json!({"amount": 12.40, "currency": "EUR"}),
        created_at: "2026-10-14 09:12:05".into(),
        claimed_at: "2026-10-14 09:12:06".into(),
    }
}

#[test]
fn test_fail_unstarted_only_touches_received_jobs() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "fiscal.cash_register")).unwrap();
    assert!(
        j.fail_unstarted(1, "expired_local", "waited too long - nothing ran")
            .unwrap()
    );
    let e = j.get(1).unwrap().unwrap();
    assert_eq!(e.state, JobState::Failed);
    assert_eq!(e.error_code.as_deref(), Some("expired_local"));
    assert_eq!(
        e.error_text.as_deref(),
        Some("waited too long - nothing ran")
    );
    assert!(!e.acked);
    assert!(chrono::DateTime::parse_from_rfc3339(&e.updated_at).is_ok());
    assert_eq!(
        e.owed_ack(),
        Some(AckRequest::Failed {
            job_id: 1,
            code: "expired_local".into(),
            text: "waited too long - nothing ran".into()
        })
    );
    assert!(
        !j.fail_unstarted(1, "again", "x").unwrap(),
        "already failed"
    );
    assert_eq!(
        j.get(1).unwrap().unwrap().error_code.as_deref(),
        Some("expired_local")
    );

    // a job its worker already started is never failed this way
    j.insert_received(&job(2, "fiscal.cash_register")).unwrap();
    assert!(j.start_execution(2).unwrap());
    assert!(!j.fail_unstarted(2, "expired_local", "x").unwrap());
    assert_eq!(j.get(2).unwrap().unwrap().state, JobState::Executing);
    assert!(
        !j.fail_unstarted(99, "expired_local", "x").unwrap(),
        "unknown job"
    );
}

#[test]
fn test_insert_received_round_trips_and_known_is_never_new() {
    let j = Journal::open_in_memory().unwrap();
    let original = job(4711, "card.purchase");
    assert_eq!(j.insert_received(&original).unwrap(), Inserted::New);
    let e = j.get(4711).unwrap().unwrap();
    assert_eq!(e.job, original);
    assert_eq!(e.state, JobState::Received);
    assert_eq!(
        (e.acked, e.report, e.reconcile_attempts),
        (false, Report::None, 0)
    );
    assert_eq!(e.kind(), Some(DeviceKind::Card));
    assert!(e.result.is_none() && e.error_code.is_none());

    // Odoo handing the same job out again: kept as it is.
    j.start_execution(4711).unwrap();
    let mut again = original.clone();
    again.payload = json!({"amount": 99});
    match j.insert_received(&again).unwrap() {
        Inserted::Known(k) => {
            assert_eq!(k.state, JobState::Executing);
            assert_eq!(k.job.payload, original.payload);
        }
        other => panic!("{other:?}"),
    }
    assert!(j.get(1).unwrap().is_none());
}

#[test]
fn test_start_execution_happens_only_once_and_only_from_received() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "fiscal.cash_register")).unwrap();
    assert!(j.start_execution(1).unwrap());
    assert!(
        !j.start_execution(1).unwrap(),
        "second start must be refused"
    );
    assert!(!j.start_execution(2).unwrap(), "unknown job");
    j.insert_received(&job(3, "fiscal.cash_register")).unwrap();
    j.record_outcome(3, &Outcome::failed("bad_amount", "x"))
        .unwrap();
    assert!(!j.start_execution(3).unwrap(), "a refused job never starts");
    assert_eq!(j.get(1).unwrap().unwrap().state, JobState::Executing);
}

#[test]
fn test_outcomes_and_owed_acks() {
    let j = Journal::open_in_memory().unwrap();
    for id in 1..=4 {
        j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
        j.start_execution(id).unwrap();
    }
    assert!(
        j.record_outcome(1, &Outcome::Done(json!({"uid": "O-1"})))
            .unwrap()
    );
    assert!(
        j.record_outcome(2, &Outcome::failed("declined", "zamietnuté"))
            .unwrap()
    );
    assert!(
        j.record_outcome(3, &Outcome::Indeterminate("timeout".into()))
            .unwrap()
    );
    // 4 stays executing: nothing owed for it
    assert_eq!(
        j.owed_acks().unwrap(),
        vec![
            AckRequest::Done {
                job_id: 1,
                result: json!({"uid": "O-1"})
            },
            AckRequest::Failed {
                job_id: 2,
                code: "declined".into(),
                text: "zamietnuté".into()
            },
            AckRequest::Indeterminate {
                job_id: 3,
                reason: "timeout".into()
            },
        ]
    );
    assert!(j.mark_acked(1, "ok").unwrap());
    assert!(j.mark_acked(2, "duplicate").unwrap());
    assert!(j.mark_reported(3).unwrap());
    assert!(j.owed_acks().unwrap().is_empty());
    let e1 = j.get(1).unwrap().unwrap();
    assert!(e1.acked);
    assert_eq!(e1.ack_note.as_deref(), Some("ok"));
    assert_eq!(e1.result, Some(json!({"uid": "O-1"})));
    assert_eq!(j.get(3).unwrap().unwrap().report, Report::Sent);
    // mark_acked / mark_reported only touch rows in the matching state
    assert!(!j.mark_acked(3, "ok").unwrap());
    assert!(!j.mark_reported(1).unwrap());
    assert!(!j.mark_acked(4, "ok").unwrap());
}

/// A row in a given state, read back from the journal (owed_ack is
/// checked on the entry itself, not through the owed_acks SQL filter).
fn entry_in(j: &Journal, id: i64, outcome: Option<Outcome>) -> JournalEntry {
    j.insert_received(&job(id, "card.purchase")).unwrap();
    j.start_execution(id).unwrap();
    if let Some(o) = outcome {
        j.record_outcome(id, &o).unwrap();
    }
    j.get(id).unwrap().unwrap()
}

#[test]
fn test_owed_ack_guards_on_the_entry_itself() {
    let j = Journal::open_in_memory().unwrap();
    // done: owed until acked, never after
    let done = entry_in(&j, 1, Some(Outcome::Done(json!({"auth": "A1"}))));
    assert_eq!(
        done.owed_ack(),
        Some(AckRequest::Done {
            job_id: 1,
            result: json!({"auth": "A1"})
        })
    );
    let acked_done = JournalEntry {
        acked: true,
        ..done.clone()
    };
    assert_eq!(
        acked_done.owed_ack(),
        None,
        "an accepted done ack is not owed again"
    );
    // failed: the same
    let failed = entry_in(&j, 2, Some(Outcome::failed("declined", "zamietnutá")));
    assert_eq!(
        failed.owed_ack(),
        Some(AckRequest::Failed {
            job_id: 2,
            code: "declined".into(),
            text: "zamietnutá".into()
        })
    );
    let acked_failed = JournalEntry {
        acked: true,
        ..failed
    };
    assert_eq!(
        acked_failed.owed_ack(),
        None,
        "an accepted failed ack is not owed again"
    );
    // indeterminate: owed ONLY while the report is owed
    let unknown = entry_in(&j, 3, Some(Outcome::Indeterminate("no answer".into())));
    assert_eq!(unknown.report, Report::Owed);
    assert_eq!(
        unknown.owed_ack(),
        Some(AckRequest::Indeterminate {
            job_id: 3,
            reason: "no answer".into()
        })
    );
    for report in [Report::None, Report::Sent] {
        let quiet = JournalEntry {
            report,
            ..unknown.clone()
        };
        assert_eq!(quiet.owed_ack(), None, "{report:?}");
    }
    // in flight: nothing owed, whatever the flags say
    let running = entry_in(&j, 4, None);
    assert_eq!(running.state, JobState::Executing);
    for (acked, report) in [
        (false, Report::Owed),
        (true, Report::Owed),
        (false, Report::None),
    ] {
        let e = JournalEntry {
            acked,
            report,
            ..running.clone()
        };
        assert_eq!(e.owed_ack(), None);
    }
    // a received row never owes an ack
    let odd = JournalEntry {
        state: JobState::Received,
        ..done
    };
    assert_eq!(odd.owed_ack(), None);
}

#[test]
fn test_timestamps_are_rfc3339_now_and_move_with_each_transition() {
    let j = Journal::open_in_memory().unwrap();
    let before = Utc::now() - chrono::Duration::seconds(5);
    j.insert_received(&job(9, "fiscal.cash_register")).unwrap();
    let e = j.get(9).unwrap().unwrap();
    let received =
        chrono::DateTime::parse_from_rfc3339(&e.received_at).expect("received_at is RFC 3339");
    let updated =
        chrono::DateTime::parse_from_rfc3339(&e.updated_at).expect("updated_at is RFC 3339");
    assert!(received >= before && received <= Utc::now(), "{received}");
    assert_eq!(received, updated);
    std::thread::sleep(std::time::Duration::from_millis(5));
    j.start_execution(9).unwrap();
    let moved = j.get(9).unwrap().unwrap();
    let moved_at = chrono::DateTime::parse_from_rfc3339(&moved.updated_at).unwrap();
    assert!(moved_at > updated, "updated_at follows the transition");
    assert_eq!(moved.received_at, e.received_at, "received_at stays");
    // the test device log and the prune cutoff compare these as text:
    // RFC 3339 UTC strings sort like the instants they name
    assert!(e.updated_at < moved.updated_at);
}

#[test]
fn test_first_final_outcome_wins() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "card.purchase")).unwrap();
    j.start_execution(1).unwrap();
    j.record_outcome(1, &Outcome::Done(json!({"auth": "A1"})))
        .unwrap();
    assert!(!j.record_outcome(1, &Outcome::failed("x", "y")).unwrap());
    assert!(
        !j.record_outcome(1, &Outcome::Indeterminate("late".into()))
            .unwrap()
    );
    assert!(!j.record_outcome(1, &Outcome::Done(json!({}))).unwrap());
    assert!(!j.record_still_unknown(1, "late").unwrap());
    let e = j.get(1).unwrap().unwrap();
    assert_eq!(e.state, JobState::Done);
    assert_eq!(e.result, Some(json!({"auth": "A1"})));
    assert!(!j.record_outcome(99, &Outcome::Done(json!({}))).unwrap());
}

#[test]
fn test_indeterminate_resolves_late_and_report_is_owed_once() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "card.purchase")).unwrap();
    j.start_execution(1).unwrap();
    j.record_outcome(1, &Outcome::Indeterminate("no answer".into()))
        .unwrap();
    assert_eq!(j.get(1).unwrap().unwrap().report, Report::Owed);
    j.mark_reported(1).unwrap();
    // a still-unknown reconcile keeps the sent report, counts the attempt
    assert!(j.record_still_unknown(1, "terminal offline").unwrap());
    let e = j.get(1).unwrap().unwrap();
    assert_eq!(e.report, Report::Sent);
    assert_eq!(e.reconcile_attempts, 1);
    assert_eq!(e.error_text.as_deref(), Some("terminal offline"));
    assert!(j.owed_acks().unwrap().is_empty());
    // another indeterminate outcome does not re-owe a sent report
    j.record_outcome(1, &Outcome::Indeterminate("again".into()))
        .unwrap();
    assert_eq!(j.get(1).unwrap().unwrap().report, Report::Sent);
    assert_eq!(j.indeterminate().unwrap().len(), 1);
    // the late outcome
    assert!(
        j.record_outcome(1, &Outcome::Done(json!({"auth": "A9"})))
            .unwrap()
    );
    assert_eq!(
        j.owed_acks().unwrap(),
        vec![AckRequest::Done {
            job_id: 1,
            result: json!({"auth": "A9"})
        }]
    );
    assert!(j.indeterminate().unwrap().is_empty());
    assert!(!j.record_still_unknown(2, "x").unwrap());
}

#[test]
fn test_restart_recovery_never_reexecutes_and_reconciles_first() {
    let j = Journal::open_in_memory().unwrap();
    for id in 1..=5 {
        j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
    }
    j.start_execution(2).unwrap();
    j.start_execution(3).unwrap();
    j.record_outcome(3, &Outcome::Done(json!({}))).unwrap();
    j.start_execution(4).unwrap();
    j.record_outcome(4, &Outcome::Indeterminate("x".into()))
        .unwrap();
    j.record_outcome(5, &Outcome::failed("unsupported", "no executor"))
        .unwrap();

    let recovered = j.recover_after_restart().unwrap();
    assert_eq!(
        recovered,
        vec![(1, JobState::Received), (2, JobState::Executing)]
    );
    for id in [1, 2] {
        let e = j.get(id).unwrap().unwrap();
        assert_eq!(e.state, JobState::Indeterminate);
        // reconciled through the device FIRST: no indeterminate ack yet
        assert_eq!(e.report, Report::None);
        assert!(!j.start_execution(id).unwrap(), "never executed again");
    }
    assert_eq!(
        j.get(1).unwrap().unwrap().error_text.as_deref(),
        Some("devbridge restarted while the job was received")
    );
    assert_eq!(
        j.get(2).unwrap().unwrap().error_text.as_deref(),
        Some("devbridge restarted while the job was executing")
    );
    assert_eq!(j.get(3).unwrap().unwrap().state, JobState::Done);
    assert_eq!(j.get(5).unwrap().unwrap().state, JobState::Failed);
    assert_eq!(j.indeterminate().unwrap().len(), 3);
    assert!(j.recover_after_restart().unwrap().is_empty());
}

#[test]
fn test_reopen_ack_only_for_acked_final_rows() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "card.purchase")).unwrap();
    assert!(!j.reopen_ack(1).unwrap());
    j.record_outcome(1, &Outcome::failed("bad_amount", "x"))
        .unwrap();
    assert!(!j.reopen_ack(1).unwrap(), "not acked yet");
    j.mark_acked(1, "ok").unwrap();
    assert!(j.reopen_ack(1).unwrap());
    assert_eq!(j.owed_acks().unwrap().len(), 1);
    assert!(!j.get(1).unwrap().unwrap().acked);
}

#[test]
fn test_insert_refused_is_final_in_one_write_and_never_starts() {
    let j = Journal::open_in_memory().unwrap();
    let refused = job(5, "card.purchase");
    assert_eq!(
        j.insert_refused(&refused, "bad_amount", "nothing ran")
            .unwrap(),
        Inserted::New
    );
    let e = j.get(5).unwrap().unwrap();
    assert_eq!(e.state, JobState::Failed);
    assert_eq!(e.error_code.as_deref(), Some("bad_amount"));
    assert_eq!(e.error_text.as_deref(), Some("nothing ran"));
    assert!(!e.acked);
    assert!(!j.start_execution(5).unwrap());
    assert!(
        j.recover_after_restart().unwrap().is_empty(),
        "not in flight"
    );
    assert_eq!(
        j.owed_acks().unwrap(),
        vec![AckRequest::Failed {
            job_id: 5,
            code: "bad_amount".into(),
            text: "nothing ran".into()
        }]
    );
    // an id already journaled stays as it is
    j.insert_received(&job(6, "fiscal.cash_register")).unwrap();
    assert!(matches!(
        j.insert_refused(&job(6, "fiscal.cash_register"), "unsupported", "x")
            .unwrap(),
        Inserted::Known(_)
    ));
    assert_eq!(j.get(6).unwrap().unwrap().state, JobState::Received);
    let fresh = j.get(6).unwrap().unwrap();
    assert!(fresh.error_code.is_none() && fresh.error_text.is_none());
}

#[test]
fn test_prune_drops_only_old_acked_final_rows() {
    let j = Journal::open_in_memory().unwrap();
    let old = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS + 1)).to_rfc3339();
    for id in 1..=4 {
        j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
    }
    j.record_outcome(1, &Outcome::Done(json!({}))).unwrap();
    j.mark_acked(1, "ok").unwrap();
    j.record_outcome(2, &Outcome::Done(json!({}))).unwrap(); // ack still owed
    j.record_outcome(3, &Outcome::Indeterminate("x".into()))
        .unwrap();
    j.record_outcome(4, &Outcome::Done(json!({}))).unwrap();
    j.mark_acked(4, "ok").unwrap();
    j.insert_received(&job(5, "fiscal.cash_register")).unwrap();
    j.record_outcome(5, &Outcome::Done(json!({}))).unwrap();
    j.mark_acked(5, "refused: Odoo refused (not_found): x")
        .unwrap();
    for id in [1, 2, 3, 5] {
        j.set_updated_at(id, &old);
    }
    assert_eq!(j.prune().unwrap(), 1);
    assert!(j.get(1).unwrap().is_none());
    for id in 2..=5 {
        assert!(j.get(id).unwrap().is_some(), "{id} must stay");
    }
}

#[test]
fn test_journal_survives_reopen_wal_full() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    {
        let j = Journal::open(&path).unwrap();
        j.insert_received(&job(7, "card.purchase")).unwrap();
        j.start_execution(7).unwrap();
        let conn = j.conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous = FULL");
    }
    let j = Journal::open(&path).unwrap();
    assert_eq!(j.get(7).unwrap().unwrap().state, JobState::Executing);
    assert_eq!(
        j.recover_after_restart().unwrap(),
        vec![(7, JobState::Executing)]
    );
}

#[test]
fn test_state_and_report_codes() {
    for s in [
        JobState::Received,
        JobState::Executing,
        JobState::Done,
        JobState::Failed,
        JobState::Indeterminate,
    ] {
        assert_eq!(JobState::parse(s.as_str()), Some(s));
    }
    assert_eq!(JobState::parse("queued"), None);
    assert!(JobState::Done.is_final() && JobState::Failed.is_final());
    assert!(!JobState::Indeterminate.is_final() && !JobState::Executing.is_final());
    for r in [Report::None, Report::Owed, Report::Sent] {
        assert_eq!(Report::from_code(r.code()), r);
    }
    assert_eq!(Report::from_code(7), Report::None);
}

#[test]
fn test_corrupt_row_is_an_error_not_a_guess() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&job(1, "card.purchase")).unwrap();
    j.conn()
        .execute(
            "UPDATE device_jobs SET state = 'weird' WHERE job_id = 1",
            [],
        )
        .unwrap();
    assert!(j.get(1).is_err());
    j.conn()
        .execute(
            "UPDATE device_jobs SET state = 'done', result = '{oops' WHERE job_id = 1",
            [],
        )
        .unwrap();
    assert!(j.get(1).is_err());
    j.conn()
        .execute(
            "UPDATE device_jobs SET result = NULL, payload = 'not json' WHERE job_id = 1",
            [],
        )
        .unwrap();
    assert!(j.get(1).is_err());
}

#[test]
fn test_mark_device_contact_is_durable_and_kept_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    let first;
    {
        let j = Journal::open(&path).unwrap();
        j.insert_received(&job(1, "fiscal.cash_register")).unwrap();
        j.insert_received(&job(2, "fiscal.cash_register")).unwrap();
        assert_eq!(j.get(1).unwrap().unwrap().device_contact_at, None);
        assert!(j.mark_device_contact(1).unwrap());
        first = j.get(1).unwrap().unwrap().device_contact_at.unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&first).is_ok(),
            "{first}"
        );
        assert!(!j.mark_device_contact(1).unwrap(), "the first time is kept");
        assert!(!j.mark_device_contact(99).unwrap(), "unknown job");
    }
    // a restart keeps the mark: reconcile can tell "sent" from "never sent"
    let j = Journal::open(&path).unwrap();
    j.recover_after_restart().unwrap();
    let one = j.get(1).unwrap().unwrap();
    assert_eq!(one.state, JobState::Indeterminate);
    assert_eq!(one.device_contact_at.as_deref(), Some(first.as_str()));
    assert_eq!(j.get(2).unwrap().unwrap().device_contact_at, None);
}

#[test]
fn test_a_99_journal_gets_the_device_contact_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    {
        // the #99 (v0.8.45/46) schema, one row in flight
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE device_jobs (
                 job_id INTEGER PRIMARY KEY, job_type TEXT NOT NULL, client_uuid TEXT NOT NULL,
                 kasa_id INTEGER, kasa_name TEXT NOT NULL, pos_order TEXT NOT NULL,
                 payload TEXT NOT NULL, odoo_created_at TEXT NOT NULL, odoo_claimed_at TEXT NOT NULL,
                 state TEXT NOT NULL, result TEXT, error_code TEXT, error_text TEXT,
                 acked INTEGER NOT NULL DEFAULT 0, ack_note TEXT,
                 report INTEGER NOT NULL DEFAULT 0, reconcile_attempts INTEGER NOT NULL DEFAULT 0,
                 received_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             INSERT INTO device_jobs (job_id, job_type, client_uuid, kasa_id, kasa_name, pos_order,
                 payload, odoo_created_at, odoo_claimed_at, state, received_at, updated_at)
             VALUES (5, 'fiscal.cash_register', 'u5', 3, 'K', 'null', '{}', 'c', 'c', 'executing',
                 '2026-10-15T10:00:00+00:00', '2026-10-15T10:00:00+00:00'),
                    (6, 'fiscal.cash_register', 'u6', 3, 'K', 'null', '{}', 'c', 'c', 'done',
                 '2026-10-15T10:00:00+00:00', '2026-10-15T10:00:00+00:00');",
        )
        .unwrap();
    }
    let j = Journal::open(&path).unwrap();
    let in_flight = j.get(5).unwrap().unwrap();
    assert_eq!(in_flight.state, JobState::Executing);
    assert_eq!(
        in_flight.device_contact_at.as_deref(),
        Some(CONTACT_UNKNOWN_BEFORE_MIGRATION),
        "an in-flight row never reads as 'never contacted'"
    );
    assert_eq!(
        j.get(6).unwrap().unwrap().device_contact_at,
        None,
        "final row"
    );
    assert!(!j.mark_device_contact(5).unwrap(), "already marked");
    j.insert_received(&job(7, "fiscal.cash_register")).unwrap();
    assert_eq!(j.get(7).unwrap().unwrap().device_contact_at, None);
    drop(j);
    // opening again neither adds the column twice nor re-marks rows
    let again = Journal::open(&path).unwrap();
    assert_eq!(again.get(7).unwrap().unwrap().device_contact_at, None);
    assert_eq!(
        again.get(5).unwrap().unwrap().device_contact_at.as_deref(),
        Some(CONTACT_UNKNOWN_BEFORE_MIGRATION)
    );
}
