//! Exactly once: journal before the device, restart recovery through the
//! device, lost `next` responses, jobs handed out again.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use devbridge_client::device_jobs::executor::{DeviceKind, ExecutorRegistry, Outcome};
use devbridge_client::device_jobs::journal::{JobState, Journal, Report};
use devbridge_client::device_jobs::test_executor::TestExecutor;

use super::fake::*;

#[tokio::test]
async fn test_job_runs_once_and_is_acked_done_with_heartbeat() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(101, "fiscal.cash_register", script("done"));

    eventually("job 101 done in Odoo", || {
        final_state(&fake, 101) == St::Done
    })
    .await;
    eventually("journal acked", || {
        journal.get(101).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    eventually("a heartbeat", || {
        !fake.lock().unwrap().heartbeats.is_empty()
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.acks_for(101),
        vec![json!({"job_id": 101, "result":
            {"device": "test", "job_id": 101, "executions": 1, "via": "execute"}})]
    );
    assert_eq!(device.runs(101).unwrap(), 1);
    assert_eq!(
        s.jobs[&101].result,
        Some(json!({"device": "test", "job_id": 101, "executions": 1, "via": "execute"}))
    );
    let e = journal.get(101).unwrap().unwrap();
    assert_eq!(e.state, JobState::Done);
    assert_eq!(e.ack_note.as_deref(), Some("ok"));
    assert_eq!(e.job.client_uuid, "uuid-101");
    assert_eq!(e.job.kasa_id, Some(3));
    assert_eq!(
        s.heartbeats[0],
        json!({"bridge_version": env!("CARGO_PKG_VERSION"),
               "device_status": "portos=test; terminal=test"})
    );
    assert!(s.next_waits.iter().all(|w| *w == 1), "{:?}", s.next_waits);
    assert!(s.pending_calls >= 1, "pending runs at start");
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_invalid_and_unsupported_jobs_are_refused_without_device_contact() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    // only a card executor: fiscal jobs have none
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(TestExecutor::new(
            DeviceKind::Card,
            Arc::clone(&device),
        )))
        .unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        let mut zero = script("done");
        zero["amount"] = json!(0);
        s.queue(201, "card.purchase", zero);
        let mut text = script("done");
        text["amount"] = json!("12.40");
        s.queue(202, "card.purchase", text);
        s.queue(203, "display.text", script("done"));
        s.queue(204, "fiscal.cash_register", script("done"));
    }
    eventually("all four refused", || {
        (201..=204).all(|id| final_state(&fake, id) == St::Failed)
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    for (id, code) in [
        (201, "bad_amount"),
        (202, "bad_amount"),
        (203, "unsupported"),
        (204, "unsupported"),
    ] {
        let acks = s.acks_for(id);
        assert_eq!(acks.len(), 1, "{id}: {acks:?}");
        assert_eq!(acks[0]["error_code"], code, "{id}");
        assert_eq!(s.jobs[&id].error_code.as_deref(), Some(code), "{id}");
        assert!(
            acks[0]["error"].as_str().unwrap().contains("nothing ran"),
            "{acks:?}"
        );
        assert_eq!(device.runs(id).unwrap(), 0, "{id} reached a device");
        assert_eq!(journal.get(id).unwrap().unwrap().state, JobState::Failed);
    }
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_crash_after_received_restart_reconciles_through_the_device_never_executes() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    // The previous process journaled the claimed job and died before the
    // device was touched.
    fake.lock()
        .unwrap()
        .claimed(301, "fiscal.cash_register", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&301],
        true,
    ))
    .unwrap();
    journal.insert_received(&job).unwrap();
    drop(journal);

    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("job 301 resolved", || final_state(&fake, 301) == St::Failed).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(301);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "not_executed");
    assert_eq!(
        device.runs(301).unwrap(),
        0,
        "never executed after the restart"
    );
    assert_eq!(journal.get(301).unwrap().unwrap().state, JobState::Failed);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_crash_mid_job_restart_reconciles_without_a_second_execution() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let crashed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&crashed);
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(
            TestExecutor::new(DeviceKind::Fiscal, Arc::clone(&device))
                .with_crash_hook(Arc::new(move |id: i64| seen.lock().unwrap().push(id))),
        ))
        .unwrap();
    let first = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock()
        .unwrap()
        .queue(401, "fiscal.cash_register", script("crash"));
    eventually("the device ran job 401 and the process 'died'", || {
        crashed.lock().unwrap().contains(&401)
    })
    .await;
    assert_eq!(
        journal.get(401).unwrap().unwrap().state,
        JobState::Executing
    );
    first.stop().await;
    drop(journal);
    drop(device);
    // the dead process's long-poll stays open on the fake until its 1 s wait
    // ends - let it, so the restart's pending is not counted as an overlap
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(fake.lock().unwrap().acks_for(401).is_empty());

    // The restarted process: fresh handles on the same files.
    let (journal, device) = open_files(dir.path());
    let second = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("job 401 reconciled done", || {
        final_state(&fake, 401) == St::Done
    })
    .await;
    eventually("journal acked", || {
        journal.get(401).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    second.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.acks_for(401),
        vec![json!({"job_id": 401, "result":
            {"device": "test", "job_id": 401, "executions": 1, "via": "reconcile"}})]
    );
    assert_eq!(device.runs(401).unwrap(), 1, "exactly one execution");
    assert!(!s.jobs[&401].ran_after_not_received);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_lost_next_response_is_acked_not_received_after_too_early() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        let mut s = fake.lock().unwrap();
        // two responses lost: both jobs are too_early in the same pending
        // round, both must be asked again
        s.lose_next_responses = 2;
        s.too_early_window = Duration::from_millis(400);
        s.queue(501, "card.purchase", script("done"));
        s.queue(503, "fiscal.cash_register", script("done"));
    }
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("jobs 501 + 503 failed as not_received", || {
        let s = fake.lock().unwrap();
        s.jobs[&501].not_received && s.jobs[&503].not_received
    })
    .await;
    // keeps working afterwards
    fake.lock()
        .unwrap()
        .queue(502, "card.purchase", script("done"));
    eventually("job 502 done", || final_state(&fake, 502) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(501);
    assert!(
        acks.len() >= 2,
        "first not_received must be too_early: {acks:?}"
    );
    assert!(
        acks.iter().all(|a| a["error_code"] == "not_received"),
        "{acks:?}"
    );
    for id in [501, 503] {
        assert_eq!(device.runs(id).unwrap(), 0, "a lost job never runs");
        assert!(
            journal.get(id).unwrap().is_none(),
            "never journaled = never received"
        );
    }
    assert!(s.acks_for(503).len() >= 2, "{:?}", s.acks_for(503));
    assert_eq!(device.runs(502).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_journaled_job_handed_out_again_is_never_executed_again() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock()
        .unwrap()
        .queue(1101, "fiscal.cash_register", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1101],
        true,
    ))
    .unwrap();
    // journaled + executed + acked by this bridge before (Odoo bug re-queues it)
    journal.insert_received(&job).unwrap();
    journal.start_execution(1101).unwrap();
    journal
        .record_outcome(1101, &Outcome::Done(json!({"receipt": "R-1"})))
        .unwrap();
    journal.mark_acked(1101, "ok").unwrap();
    // and one whose final ack we think Odoo stored, but `pending` still lists it
    fake.lock()
        .unwrap()
        .claimed(1102, "card.purchase", script("done"));
    let other = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1102],
        true,
    ))
    .unwrap();
    journal.insert_received(&other).unwrap();
    journal
        .record_outcome(1102, &Outcome::failed("declined", "zamietnutá"))
        .unwrap();
    journal.mark_acked(1102, "ok").unwrap();
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("the stored outcomes are re-sent", || {
        final_state(&fake, 1101) == St::Done && final_state(&fake, 1102) == St::Failed
    })
    .await;
    run.stop().await;
    assert_eq!(device.runs(1101).unwrap(), 0, "NOT executed again");
    assert_eq!(device.runs(1102).unwrap(), 0, "NOT executed again");
    assert_eq!(
        fake.lock().unwrap().acks_for(1101),
        vec![json!({"job_id": 1101, "result": {"receipt": "R-1"}})]
    );
    assert_eq!(
        fake.lock().unwrap().acks_for(1102),
        vec![json!({"job_id": 1102, "error": "zamietnutá", "error_code": "declined"})]
    );
    assert_clean(&fake);
}

#[tokio::test]
async fn test_unknown_outcome_without_an_executor_is_reported_once_and_never_run() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock()
        .unwrap()
        .claimed(1001, "card.purchase", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1001],
        true,
    ))
    .unwrap();
    journal.insert_received(&job).unwrap();
    journal.start_execution(1001).unwrap();
    // restart with ONLY a fiscal executor: nobody can ask the terminal
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(TestExecutor::new(
            DeviceKind::Fiscal,
            Arc::clone(&device),
        )))
        .unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    eventually("reported indeterminate", || {
        fake.lock().unwrap().jobs[&1001].state == St::Unknown
    })
    .await;
    // several reconcile intervals later: still exactly one report
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.stop().await;
    let e = journal.get(1001).unwrap().unwrap();
    assert_eq!(e.state, JobState::Indeterminate);
    assert_eq!(e.report, Report::Sent);
    assert_eq!(e.reconcile_attempts, 1);
    let acks = fake.lock().unwrap().acks_for(1001);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(device.runs(1001).unwrap(), 0);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_refused_not_received_is_not_retried() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        // claimed by this bridge, reported indeterminate, but missing from
        // the journal: Odoo refuses not_received (after_indeterminate)
        let mut s = fake.lock().unwrap();
        s.claimed(1601, "card.purchase", script("done"));
        let j = s.jobs.get_mut(&1601).unwrap();
        j.state = St::Unknown;
        j.indeterminate_reported = true;
    }
    // wait_s 0: a next round every 100 ms - a retry would show at once
    let run = start(
        config(&url, 0),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("not_received tried", || {
        !fake.lock().unwrap().acks_for(1601).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    run.stop().await;
    let s = fake.lock().unwrap();
    assert_eq!(s.acks_for(1601).len(), 1, "{:?}", s.acks_for(1601));
    assert_eq!(s.jobs[&1601].state, St::Unknown);
    assert!(s.next_calls >= 3, "next keeps running: {}", s.next_calls);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_transport_error_on_not_received_retries_the_pending_round() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        let mut s = fake.lock().unwrap();
        s.lose_next_responses = 1;
        // the first ack is the not_received of the lost job: 502, not stored
        s.fail_acks = 1;
        s.queue(1701, "card.purchase", script("done"));
    }
    // wait_s 0: a pending that is NOT retried would only come back much later
    let run = start(
        config(&url, 0),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("lost job failed as not_received", || {
        fake.lock().unwrap().jobs[&1701].not_received
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let acks = s.acks_for(1701);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert!(acks.iter().all(|a| a["error_code"] == "not_received"));
    // the failed round is repeated before another next
    assert!(s.pending_calls >= 3, "{}", s.pending_calls);
    assert_eq!(device.runs(1701).unwrap(), 0);
    drop(s);
    assert_clean(&fake);
}
