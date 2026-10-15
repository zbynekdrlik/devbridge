//! Acks, reconciliation, the control loop's timing, serial devices and the
//! heartbeat.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use devbridge_client::device_jobs::api::DeviceJob;
use devbridge_client::device_jobs::executor::{
    DeviceExecutor, DeviceKind, ExecutorRegistry, Outcome, Reconciliation,
};
use devbridge_client::device_jobs::journal::{Journal, JournalEntry};
use devbridge_client::device_jobs::test_executor::SCRIPT_KEY;

use super::fake::*;

#[tokio::test]
async fn test_executor_past_the_guard_is_reported_indeterminate_then_acked_late() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let mut t = timing();
    t.execute_guard = Duration::from_millis(150);
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        t,
    );
    fake.lock().unwrap().queue(
        601,
        "card.purchase",
        json!({SCRIPT_KEY: {"outcome": "done", "delay_ms": 700}, "amount": 5, "currency": "EUR"}),
    );
    eventually("reported indeterminate", || {
        fake.lock().unwrap().jobs[&601].state == St::Unknown
    })
    .await;
    eventually("late done", || final_state(&fake, 601) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(601);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(
        acks[1],
        json!({"job_id": 601, "result":
            {"device": "test", "job_id": 601, "executions": 1, "via": "execute"}})
    );
    assert_eq!(device.runs(601).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_indeterminate_outcome_is_reconciled_through_the_device_and_acked_late() {
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
        .queue(701, "card.purchase", script("indeterminate"));
    eventually("late done", || final_state(&fake, 701) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(701);
    // the indeterminate report goes out at once; the reconcile (next tick)
    // only rarely beats it, then the late ack alone is owed
    assert!(acks.len() <= 2, "{acks:?}");
    if acks.len() == 2 {
        assert_eq!(acks[0]["error_code"], "indeterminate", "{acks:?}");
    }
    assert_eq!(
        acks.last().unwrap(),
        &json!({"job_id": 701, "result":
            {"device": "test", "job_id": 701, "executions": 1, "via": "reconcile"}})
    );
    assert_eq!(device.runs(701).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_ack_transport_errors_are_retried_and_a_duplicate_is_success() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        let mut s = fake.lock().unwrap();
        // 1st ack: 502, not stored; 2nd: stored but its response lost (502);
        // 3rd: Odoo answers duplicate = accepted.
        s.fail_acks = 1;
        s.lose_ack_responses = 1;
    }
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(801, "fiscal.cash_register", script("done"));
    eventually("ack accepted after retries", || {
        journal.get(801).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let acks = s.acks_for(801);
    assert_eq!(acks.len(), 3, "{acks:?}");
    assert!(
        acks.windows(2).all(|w| w[0] == w[1]),
        "the same ack is retried: {acks:?}"
    );
    assert_eq!(s.jobs[&801].state, St::Done);
    assert_eq!(
        journal.get(801).unwrap().unwrap().ack_note.as_deref(),
        Some("duplicate")
    );
    assert_eq!(device.runs(801).unwrap(), 1, "re-acking never re-executes");
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_wait_zero_reply_sleeps_between_next_calls() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().force_wait_zero = true;
    let run = start(
        config(&url, 25),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    run.stop().await;
    let s = fake.lock().unwrap();
    // 100 ms short-poll sleep: ~8 calls in 800 ms, never a tight loop
    assert!(
        (3..=12).contains(&s.next_calls),
        "next calls in 800 ms: {}",
        s.next_calls
    );
    assert!(s.next_waits.iter().all(|w| *w == 25), "asks its wait_s");
    let gaps: Vec<Duration> = s.next_started.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= Duration::from_millis(90)),
        "{gaps:?}"
    );
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_next_http_timeout_is_wait_plus_margin_then_pending_before_next() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().hang_next = true;
    let mut t = timing();
    t.next_timeout_margin = Duration::from_millis(300);
    let run = start(
        config(&url, 0),
        Arc::clone(&journal),
        test_registry(&device),
        t,
    );
    eventually("a second pending after the timed-out next", || {
        fake.lock().unwrap().pending_calls >= 2
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let next_at = s.next_started[0];
    let pending_after = s.pending_at[1];
    let waited = pending_after - next_at;
    // wait_s 0 + margin 300 ms: never shorter (a late job would lose its
    // response), and the pending only after the call ended
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    assert!(s.pending_at[0] <= next_at, "pending runs first at start");
}

#[tokio::test]
async fn test_card_wait_never_blocks_a_receipt_and_a_device_runs_one_job_at_a_time() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let card = GateDevice::new(DeviceKind::Card);
    let fiscal = GateDevice::new(DeviceKind::Fiscal);
    let mut registry = ExecutorRegistry::default();
    registry.register(card.clone()).unwrap();
    registry.register(fiscal.clone()).unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        s.queue(901, "card.purchase", json!({"amount": 10}));
        s.queue(902, "card.purchase", json!({"amount": 20}));
        s.queue(903, "fiscal.cash_register", json!({}));
    }
    eventually("card 901 and receipt 903 started", || {
        card.started.lock().unwrap().contains(&901) && fiscal.started.lock().unwrap().contains(&903)
    })
    .await;
    fiscal.gate.add_permits(1);
    eventually("receipt done while the card waits", || {
        final_state(&fake, 903) == St::Done
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        *card.started.lock().unwrap(),
        vec![901],
        "card 902 waits for 901"
    );
    assert_eq!(final_state(&fake, 902), St::Claimed);
    card.gate.add_permits(1);
    eventually("card 902 started", || {
        card.started.lock().unwrap().contains(&902)
    })
    .await;
    card.gate.add_permits(1);
    eventually("both cards done", || {
        final_state(&fake, 901) == St::Done && final_state(&fake, 902) == St::Done
    })
    .await;
    run.stop().await;
    assert_eq!(*card.finished.lock().unwrap(), vec![901, 902]);
    assert_eq!(card.max_running.load(Ordering::SeqCst), 1);
    assert_eq!(fiscal.max_running.load(Ordering::SeqCst), 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_heartbeat_at_start_and_on_device_status_change() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let card = GateDevice::new(DeviceKind::Card);
    let mut registry = ExecutorRegistry::default();
    registry.register(card.clone()).unwrap();
    let run = start(config(&url, 1), journal, registry, timing());
    eventually("first heartbeat", || {
        !fake.lock().unwrap().heartbeats.is_empty()
    })
    .await;
    *card.status.lock().unwrap() = "offline".into();
    eventually("heartbeat on change", || {
        fake.lock().unwrap().heartbeats.len() >= 2
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    assert_eq!(s.heartbeats[0]["device_status"], "portos=none; terminal=ok");
    assert_eq!(
        s.heartbeats[1]["device_status"],
        "portos=none; terminal=offline"
    );
    // interval is 30 s: no third one in this short run
    assert_eq!(s.heartbeats.len(), 2);
}

#[tokio::test]
async fn test_no_next_before_pending_succeeds() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().pending_rpc_error = true;
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("pending retried with backoff", || {
        fake.lock().unwrap().pending_calls >= 3
    })
    .await;
    assert_eq!(fake.lock().unwrap().next_calls, 0, "next before pending");
    fake.lock().unwrap().pending_rpc_error = false;
    eventually("next once pending succeeded", || {
        fake.lock().unwrap().next_calls >= 1
    })
    .await;
    run.stop().await;
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_refused_ack_is_journaled_and_not_retried() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().refuse_acks = Some("not_claimed_by_you");
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(1301, "fiscal.cash_register", script("done"));
    eventually("refusal journaled", || {
        journal.get(1301).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.stop().await;
    let e = journal.get(1301).unwrap().unwrap();
    assert_eq!(
        e.ack_note.as_deref(),
        Some("refused: Odoo refused (not_claimed_by_you): refused: not_claimed_by_you")
    );
    assert_eq!(
        fake.lock().unwrap().acks_for(1301).len(),
        1,
        "a refusal is final"
    );
    assert_eq!(device.runs(1301).unwrap(), 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_failing_ack_stays_owed_without_blocking_the_others() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().rpc_error_acks_for = vec![1401];
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    {
        let mut s = fake.lock().unwrap();
        s.queue(1401, "fiscal.cash_register", script("done"));
        s.queue(1402, "fiscal.cash_register", script("done"));
    }
    eventually("1402 accepted while 1401 fails", || {
        journal.get(1402).unwrap().is_some_and(|e| e.acked)
            && fake.lock().unwrap().acks_for(1401).len() >= 2
    })
    .await;
    assert!(
        !journal.get(1401).unwrap().unwrap().acked,
        "1401 stays owed"
    );
    fake.lock().unwrap().rpc_error_acks_for.clear();
    eventually("1401 accepted once Odoo recovers", || {
        journal.get(1401).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    run.stop().await;
    assert_eq!(final_state(&fake, 1401), St::Done);
    assert_eq!(
        device.runs(1401).unwrap(),
        1,
        "a failing ack never re-executes"
    );
    assert_clean(&fake);
}

/// A device that cannot tell for its first `unknown_answers` reconciles.
struct SlowToAnswer {
    reconciles: AtomicUsize,
    unknown_answers: usize,
}

#[async_trait]
impl DeviceExecutor for SlowToAnswer {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Card
    }
    fn name(&self) -> &str {
        "slow"
    }
    async fn execute(&self, _job: &DeviceJob) -> Outcome {
        Outcome::Indeterminate("terminal connection lost mid-payment".into())
    }
    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        let n = self.reconciles.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.unknown_answers {
            Reconciliation::StillUnknown(format!("terminal offline ({n})"))
        } else {
            Reconciliation::Done(json!({"auth": format!("A-{}", entry.job.id)}))
        }
    }
    fn status(&self) -> String {
        "ok".into()
    }
}

#[tokio::test]
async fn test_unknown_device_answer_is_asked_again_until_it_answers() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let device = Arc::new(SlowToAnswer {
        reconciles: AtomicUsize::new(0),
        unknown_answers: 2,
    });
    let mut registry = ExecutorRegistry::default();
    registry.register(device.clone()).unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock().unwrap().queue(
        1501,
        "card.purchase",
        json!({"amount": 7, "currency": "EUR"}),
    );
    eventually("late done", || final_state(&fake, 1501) == St::Done).await;
    run.stop().await;
    assert_eq!(device.reconciles.load(Ordering::SeqCst), 3);
    let e = journal.get(1501).unwrap().unwrap();
    assert_eq!(e.reconcile_attempts, 2);
    assert_eq!(e.result, Some(json!({"auth": "A-1501"})));
    let acks = fake.lock().unwrap().acks_for(1501);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(
        acks[1],
        json!({"job_id": 1501, "result": {"auth": "A-1501"}})
    );
    assert_clean(&fake);
}
