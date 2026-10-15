//! Unit tests of the journal's `lan_jobs` table and the shared state machine
//! on LAN API jobs (#101).

use super::*;
use serde_json::json;

fn lan(kasa_id: i64, uuid: &str) -> DeviceJob {
    DeviceJob {
        id: 0,
        job_type: "fiscal.cash_register".into(),
        client_uuid: uuid.into(),
        kasa_id: Some(kasa_id),
        kasa_name: String::new(),
        pos_order: json!({"id": 812}),
        payload: json!({"amount": 12.40, "currency": "EUR"}),
        created_at: String::new(),
        claimed_at: String::new(),
    }
}

fn odoo(id: i64) -> DeviceJob {
    DeviceJob {
        id,
        job_type: "fiscal.cash_register".into(),
        client_uuid: format!("uuid-{id}"),
        kasa_id: Some(3),
        kasa_name: "Pokladňa Bastion 1".into(),
        pos_order: json!({"id": 812, "name": "Bastion/0042"}),
        payload: json!({"amount": 1}),
        created_at: "2026-10-15 09:12:05".into(),
        claimed_at: "2026-10-15 09:12:06".into(),
    }
}

fn new_id(j: &Journal, job: &DeviceJob) -> i64 {
    match j.insert_lan_received(job).unwrap() {
        LanInserted::New(id) => id,
        other => panic!("expected a new row, got {other:?}"),
    }
}

#[test]
fn test_lan_job_ids_are_the_negated_row_ids() {
    assert_eq!(lan_job_id(1), -1);
    assert_eq!(lan_job_id(-7), 7, "and back");
    assert!(is_lan_job(-1));
    assert!(!is_lan_job(1));
    assert!(!is_lan_job(0), "0 is never a LAN id");
}

#[test]
fn test_insert_lan_received_round_trips_and_a_replay_is_known() {
    let j = Journal::open_in_memory().unwrap();
    let job = lan(3, "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a");
    let id = new_id(&j, &job);
    assert_eq!(id, -1, "the first row");
    let stored = j.lan_job_by_id(id).unwrap().unwrap();
    assert_eq!(stored.job_id(), -1);
    assert_eq!(stored.state(), JobState::Received);
    assert!(!stored.reported && stored.report_note.is_none() && stored.odoo_job_id.is_none());
    let e = &stored.entry;
    assert_eq!(e.job.job_type, "fiscal.cash_register");
    assert_eq!(e.job.client_uuid, job.client_uuid);
    assert_eq!(e.job.kasa_id, Some(3));
    assert_eq!(e.job.pos_order, json!({"id": 812}));
    assert_eq!(e.job.payload, job.payload);
    assert_eq!(
        e.job.created_at, e.received_at,
        "a LAN job's own receipt time"
    );
    assert!(chrono::DateTime::parse_from_rfc3339(&e.received_at).is_ok());
    assert_eq!(e.received_at, e.updated_at);
    assert_eq!(e.kind(), Some(DeviceKind::Fiscal));
    assert_eq!((e.report, e.reconcile_attempts), (Report::None, 0));
    assert!(e.result.is_none() && e.error_code.is_none() && e.device_contact_at.is_none());
    // the shared `get` routes the negative id to lan_jobs
    assert_eq!(j.get(id).unwrap().as_ref(), Some(e));
    assert_eq!(
        j.lan_job(3, &job.client_uuid).unwrap().as_ref(),
        Some(&stored)
    );
    assert!(j.lan_job(4, &job.client_uuid).unwrap().is_none());
    assert!(j.lan_job(3, "other").unwrap().is_none());

    // a replay (even of another operation) never creates a second row
    let mut changed = job.clone();
    changed.payload = json!({"amount": 99});
    match j.insert_lan_received(&changed).unwrap() {
        LanInserted::Known(known) => assert_eq!(*known, stored, "stored row, untouched"),
        other => panic!("{other:?}"),
    }
    // the same client_uuid on another till is another job
    assert_eq!(new_id(&j, &lan(4, &job.client_uuid)), -2);
    let err = j.insert_lan_received(&DeviceJob {
        kasa_id: None,
        ..lan(3, "x")
    });
    assert!(err.unwrap_err().to_string().contains("must name its till"));
}

#[test]
fn test_odoo_and_lan_ids_never_mix() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&odoo(1)).unwrap();
    let lan_id = new_id(&j, &lan(3, "uuid-1"));
    assert_eq!(lan_id, -1);
    assert_eq!(j.get(1).unwrap().unwrap().job.client_uuid, "uuid-1");
    assert_eq!(
        j.get(1).unwrap().unwrap().job.kasa_name,
        "Pokladňa Bastion 1"
    );
    assert_eq!(j.get(-1).unwrap().unwrap().job.kasa_name, "");
    assert!(j.start_execution(-1).unwrap());
    assert_eq!(
        j.get(1).unwrap().unwrap().state,
        JobState::Received,
        "Odoo row untouched"
    );
    assert!(j.start_execution(1).unwrap());
    assert!(
        j.lan_job_by_id(1).unwrap().is_none(),
        "a positive id is no LAN job"
    );
    assert!(j.get(-2).unwrap().is_none());
}

#[test]
fn test_lan_state_machine_runs_once_and_first_final_wins() {
    let j = Journal::open_in_memory().unwrap();
    let id = new_id(&j, &lan(3, "a"));
    assert!(j.mark_device_contact(id).unwrap());
    assert!(!j.mark_device_contact(id).unwrap(), "kept once");
    let contact = j.get(id).unwrap().unwrap().device_contact_at.unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(&contact).is_ok());
    assert!(j.start_execution(id).unwrap());
    assert!(!j.start_execution(id).unwrap(), "only once");
    assert_eq!(j.get(id).unwrap().unwrap().state, JobState::Executing);
    assert!(
        j.record_outcome(id, &Outcome::Done(json!({"uid": "U-1"})))
            .unwrap()
    );
    assert!(
        !j.record_outcome(id, &Outcome::failed("x", "y")).unwrap(),
        "the first final outcome wins"
    );
    let done = j.lan_job_by_id(id).unwrap().unwrap();
    assert_eq!(done.state(), JobState::Done);
    assert_eq!(done.entry.result, Some(json!({"uid": "U-1"})));
    assert!(done.report_owed());
    assert_eq!(j.unreported_lan_jobs().unwrap(), 1);

    // a refused-before-the-device job: fail_unstarted only from received
    let id2 = new_id(&j, &lan(3, "b"));
    assert!(
        j.fail_unstarted(id2, "expired_local", "waited - nothing ran")
            .unwrap()
    );
    assert!(!j.fail_unstarted(id2, "again", "x").unwrap());
    let failed = j.lan_job_by_id(id2).unwrap().unwrap();
    assert_eq!(failed.state(), JobState::Failed);
    assert_eq!(failed.entry.error_code.as_deref(), Some("expired_local"));
    assert_eq!(
        failed.entry.error_text.as_deref(),
        Some("waited - nothing ran")
    );
    assert!(failed.report_owed());
    assert!(
        !j.fail_unstarted(id, "x", "y").unwrap(),
        "a done job stays done"
    );
    assert_eq!(
        j.owed_lan_reports()
            .unwrap()
            .iter()
            .map(LanJob::job_id)
            .collect::<Vec<_>>(),
        vec![id, id2]
    );
    assert_eq!(j.unreported_lan_jobs().unwrap(), 2);
}

#[test]
fn test_lan_indeterminate_is_reconciled_and_reported_only_when_resolved() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&odoo(5)).unwrap();
    j.record_outcome(5, &Outcome::Indeterminate("odoo t/o".into()))
        .unwrap();
    let id = new_id(&j, &lan(3, "c"));
    j.start_execution(id).unwrap();
    assert!(
        j.record_outcome(id, &Outcome::Indeterminate("no answer".into()))
            .unwrap()
    );
    let e = j.get(id).unwrap().unwrap();
    assert_eq!(e.state, JobState::Indeterminate);
    assert_eq!(
        e.report,
        Report::None,
        "a LAN job owes no indeterminate ack"
    );
    assert_eq!(e.error_text.as_deref(), Some("no answer"));
    assert!(j.owed_lan_reports().unwrap().is_empty(), "not resolved yet");
    assert_eq!(j.unreported_lan_jobs().unwrap(), 0);
    assert_eq!(
        j.owed_acks().unwrap().len(),
        1,
        "only the Odoo job owes an indeterminate ack"
    );
    let waiting: Vec<i64> = j
        .indeterminate()
        .unwrap()
        .iter()
        .map(|e| e.job.id)
        .collect();
    assert_eq!(waiting, vec![5, id], "Odoo first, then LAN");
    assert!(j.record_still_unknown(id, "Portos offline").unwrap());
    let e = j.get(id).unwrap().unwrap();
    assert_eq!(e.reconcile_attempts, 1);
    assert_eq!(e.error_text.as_deref(), Some("Portos offline"));
    assert_eq!(e.report, Report::None);
    assert!(
        j.record_outcome(id, &Outcome::Done(json!({"uid": "late"})))
            .unwrap()
    );
    assert_eq!(j.owed_lan_reports().unwrap().len(), 1, "now it is reported");
    assert!(
        !j.record_still_unknown(id, "x").unwrap(),
        "no longer indeterminate"
    );
}

#[test]
fn test_restart_recovery_covers_both_tables() {
    let j = Journal::open_in_memory().unwrap();
    j.insert_received(&odoo(1)).unwrap();
    let received = new_id(&j, &lan(3, "r"));
    let executing = new_id(&j, &lan(3, "e"));
    j.start_execution(executing).unwrap();
    let done = new_id(&j, &lan(3, "d"));
    j.start_execution(done).unwrap();
    j.record_outcome(done, &Outcome::Done(json!({}))).unwrap();
    let recovered = j.recover_after_restart().unwrap();
    assert_eq!(
        recovered,
        vec![
            (1, JobState::Received),
            (received, JobState::Received),
            (executing, JobState::Executing)
        ]
    );
    for id in [received, executing] {
        let e = j.get(id).unwrap().unwrap();
        assert_eq!(e.state, JobState::Indeterminate, "{id}");
        assert!(
            e.error_text
                .as_deref()
                .unwrap()
                .starts_with("devbridge restarted while the job was"),
            "{e:?}"
        );
        assert_eq!(e.report, Report::None);
    }
    assert_eq!(
        j.get(executing).unwrap().unwrap().error_text.as_deref(),
        Some("devbridge restarted while the job was executing")
    );
    assert_eq!(j.get(done).unwrap().unwrap().state, JobState::Done);
    assert!(j.recover_after_restart().unwrap().is_empty());
}

#[test]
fn test_mark_lan_reported_keeps_the_outcome_time() {
    let j = Journal::open_in_memory().unwrap();
    let id = new_id(&j, &lan(3, "m"));
    assert!(
        !j.mark_lan_reported(id, Some(1), "created").unwrap(),
        "only a final job is reported"
    );
    j.start_execution(id).unwrap();
    j.record_outcome(id, &Outcome::Done(json!({}))).unwrap();
    j.set_updated_at(id, "2026-10-15T10:00:00+00:00");
    assert!(j.mark_lan_reported(id, Some(123), "created").unwrap());
    let r = j.lan_job_by_id(id).unwrap().unwrap();
    assert!(r.reported && !r.report_owed());
    assert_eq!(r.report_note.as_deref(), Some("created"));
    assert_eq!(r.odoo_job_id, Some(123));
    assert!(r.entry.acked, "acked mirrors reported");
    assert_eq!(r.entry.ack_note.as_deref(), Some("created"));
    assert_eq!(
        r.entry.updated_at, "2026-10-15T10:00:00+00:00",
        "executed_at stays"
    );
    // a later answer without an id keeps the stored one
    assert!(j.mark_lan_reported(id, None, "existing").unwrap());
    assert_eq!(j.lan_job_by_id(id).unwrap().unwrap().odoo_job_id, Some(123));
    assert!(j.owed_lan_reports().unwrap().is_empty());
    assert_eq!(j.unreported_lan_jobs().unwrap(), 0);
    // a new outcome cannot happen on a final row, so it stays reported
    assert!(!j.record_outcome(id, &Outcome::failed("a", "b")).unwrap());
    assert!(j.lan_job_by_id(id).unwrap().unwrap().reported);
}

#[test]
fn test_report_owed_guards_on_the_row_itself() {
    let j = Journal::open_in_memory().unwrap();
    let id = new_id(&j, &lan(3, "g"));
    let mut row = j.lan_job_by_id(id).unwrap().unwrap();
    for (state, reported, owed) in [
        (JobState::Done, false, true),
        (JobState::Failed, false, true),
        (JobState::Done, true, false),
        (JobState::Failed, true, false),
        (JobState::Indeterminate, false, false),
        (JobState::Received, false, false),
        (JobState::Executing, false, false),
    ] {
        row.entry.state = state;
        row.reported = reported;
        assert_eq!(row.report_owed(), owed, "{state:?} reported={reported}");
    }
}

#[test]
fn test_prune_drops_old_reported_lan_rows_only() {
    let j = Journal::open_in_memory().unwrap();
    let old = "2020-01-01T00:00:00+00:00";
    let mut ids = Vec::new();
    for (uuid, report) in [
        ("reported", Some("created")),
        ("refused", Some("refused: client_uuid_conflict")),
        ("unreported", None),
    ] {
        let id = new_id(&j, &lan(3, uuid));
        j.start_execution(id).unwrap();
        j.record_outcome(id, &Outcome::Done(json!({}))).unwrap();
        if let Some(note) = report {
            j.mark_lan_reported(id, Some(1), note).unwrap();
        }
        j.set_updated_at(id, old);
        ids.push(id);
    }
    let fresh = new_id(&j, &lan(3, "fresh"));
    j.start_execution(fresh).unwrap();
    j.record_outcome(fresh, &Outcome::Done(json!({}))).unwrap();
    j.mark_lan_reported(fresh, Some(2), "created").unwrap();
    assert_eq!(j.prune().unwrap(), 1);
    assert!(j.get(ids[0]).unwrap().is_none(), "old + reported: pruned");
    assert!(j.get(ids[1]).unwrap().is_some(), "a refused report is kept");
    assert!(j.get(ids[2]).unwrap().is_some(), "never reported: kept");
    assert!(j.get(fresh).unwrap().is_some(), "recent: kept");
    // a pruned row's id is never handed out again (AUTOINCREMENT)
    assert_eq!(new_id(&j, &lan(3, "next")), -5);
}

#[test]
fn test_corrupt_lan_row_is_an_error() {
    let j = Journal::open_in_memory().unwrap();
    let id = new_id(&j, &lan(3, "z"));
    j.conn()
        .execute("UPDATE lan_jobs SET state = 'weird' WHERE id = 1", [])
        .unwrap();
    assert!(j.lan_job_by_id(id).is_err());
    j.conn()
        .execute(
            "UPDATE lan_jobs SET state = 'done', result = 'not json' WHERE id = 1",
            [],
        )
        .unwrap();
    assert!(j.lan_job_by_id(id).is_err());
    j.conn()
        .execute("UPDATE lan_jobs SET result = '{\"ok\":1}' WHERE id = 1", [])
        .unwrap();
    assert_eq!(
        j.lan_job_by_id(id).unwrap().unwrap().entry.result,
        Some(json!({"ok": 1}))
    );
}

#[test]
fn test_lan_table_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    let id = {
        let j = Journal::open(&path).unwrap();
        new_id(&j, &lan(3, "durable"))
    };
    let j = Journal::open(&path).unwrap();
    assert_eq!(
        j.lan_job(3, "durable").unwrap().map(|l| l.job_id()),
        Some(id)
    );
}
