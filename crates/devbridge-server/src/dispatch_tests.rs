//! Unit tests of the gRPC dispatch service.

use super::*;
use crate::queue::JobQueue;
use crate::storage::Storage;
use devbridge_core::job::JobMetadata;

fn test_job(id: &str) -> JobMetadata {
    JobMetadata {
        job_id: id.to_string(),
        document_name: "test.pdf".into(),
        target_printer: "Test Printer".into(),
        target_client_id: None,
        copies: 1,
        paper_size: "A4".into(),
        duplex: false,
        color: true,
        payload_size: 1024,
        payload_sha256: "abc123".into(),
        state: JobState::Queued,
        retry_count: 0,
        error_detail: String::new(),
        requesting_user: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

/// A retry requeues a job that FAILED (or a stale in-flight one) — never a
/// still-queued one. Fail `id`, then requeue it (retry_count + 1).
fn fail_and_requeue(queue: &JobQueue, id: &str, error: &str) {
    queue.update_state(id, JobState::Failed).unwrap();
    queue.requeue_job(id, error).unwrap();
}

fn temp_dispatch(max_retries: u32) -> (tempfile::TempDir, Arc<JobQueue>, DispatchService) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());
    let spool_dir = dir.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();
    let serial_bridge = Arc::new(crate::serial_bridge::SerialBridgeManager::new(vec![]));
    let service = DispatchService::new(
        Arc::clone(&queue),
        spool_dir,
        Arc::new(AtomicU64::new(0)),
        max_retries,
        0, // retry_delay_secs=0 in tests so retry happens immediately
        serial_bridge,
    );
    (dir, queue, service)
}

#[test]
fn test_chunk_size_is_64kb() {
    // Kill mutant: replace * with + in 64 * 1024
    // 64 * 1024 = 65536, but 64 + 1024 = 1088
    assert_eq!(CHUNK_SIZE, 65536);
}

#[test]
fn test_proto_state_to_core_mapping() {
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Unspecified),
        JobState::Queued
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Queued),
        JobState::Queued
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Downloading),
        JobState::Downloading
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Printing),
        JobState::Printing
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Completed),
        JobState::Completed
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Failed),
        JobState::Failed
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Cancelled),
        JobState::Cancelled
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Rendering),
        JobState::Printing
    );
    assert_eq!(
        proto_state_to_core(devbridge_core::proto::JobState::Sending),
        JobState::Printing
    );
}

#[tokio::test]
async fn test_complete_job_success_sets_completed() {
    let (_dir, queue, service) = temp_dispatch(3);

    // Insert a job
    queue
        .push(test_job("job-ok"), "/tmp/ok.pdf".into())
        .unwrap();

    let completion = JobCompletion {
        job_id: "job-ok".into(),
        success: true,
        pages_printed: 1,
        error_detail: String::new(),
        printer_status: String::new(),
        spooler_status: String::new(),
        verification_method: String::new(),
        verification_evidence: String::new(),
        client_id: String::new(),
        unconfirmed: false,
    };
    let resp = service
        .complete_job(Request::new(completion))
        .await
        .unwrap();
    assert_eq!(resp.into_inner(), CompletionAck {});

    let job = queue.get_job("job-ok").unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);
}

/// Issue #52: the PrintJob the server streams to the client MUST carry
/// the real retry_count so the client mirrors it (server is source of
/// truth). Push a job, requeue it twice (retry_count = 2), then send it
/// and assert the emitted PrintJob.retry_count is 2 — kills the mutant
/// that would emit 0 / Default instead of meta.retry_count.
#[tokio::test]
async fn test_send_job_emits_server_retry_count() {
    let (_dir, queue, _service) = temp_dispatch(5);

    queue
        .push(test_job("job-rc"), "/tmp/rc.pdf".into())
        .unwrap();
    fail_and_requeue(&queue, "job-rc", "err1"); // retry_count = 1
    fail_and_requeue(&queue, "job-rc", "err2"); // retry_count = 2
    assert_eq!(queue.get_job("job-rc").unwrap().unwrap().retry_count, 2);

    let (tx, mut rx) = mpsc::channel::<Result<PrintJob, Status>>(1);
    send_job(&tx, &queue, "job-rc").await.unwrap();
    drop(tx);

    let emitted = rx
        .recv()
        .await
        .expect("send_job must emit a PrintJob")
        .expect("emitted PrintJob must be Ok");
    assert_eq!(
        emitted.retry_count, 2,
        "server must stream the real retry_count (2) in PrintJob (#52)"
    );
    assert_eq!(emitted.job_id, "job-rc");
}

#[tokio::test]
async fn test_complete_job_failure_requeues_under_max_retries() {
    let (_dir, queue, service) = temp_dispatch(3);

    // Insert a job with retry_count = 0
    queue
        .push(test_job("job-retry"), "/tmp/retry.pdf".into())
        .unwrap();

    let completion = JobCompletion {
        job_id: "job-retry".into(),
        success: false,
        pages_printed: 0,
        error_detail: "printer jam".into(),
        printer_status: String::new(),
        spooler_status: String::new(),
        verification_method: String::new(),
        verification_evidence: String::new(),
        client_id: String::new(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();
    // Requeue runs in a spawned task (for exponential backoff); wait
    // long enough for retry_delay_secs=0 to fire.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let job = queue.get_job("job-retry").unwrap().unwrap();
    // Should be requeued (state = Queued, retry_count = 1)
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.retry_count, 1);
}

#[tokio::test]
async fn test_complete_job_failure_at_max_retries_fails_permanently() {
    let (_dir, queue, service) = temp_dispatch(2);

    // Insert a job and requeue it twice to reach retry_count=2
    queue
        .push(test_job("job-maxretry"), "/tmp/maxretry.pdf".into())
        .unwrap();
    fail_and_requeue(&queue, "job-maxretry", "err1"); // retry_count = 1
    fail_and_requeue(&queue, "job-maxretry", "err2"); // retry_count = 2

    let completion = JobCompletion {
        job_id: "job-maxretry".into(),
        success: false,
        pages_printed: 0,
        error_detail: "permanent error".into(),
        printer_status: String::new(),
        spooler_status: String::new(),
        verification_method: String::new(),
        verification_evidence: String::new(),
        client_id: String::new(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();

    let loaded = queue.get_job("job-maxretry").unwrap().unwrap();
    assert_eq!(loaded.state, JobState::Failed);
}

#[tokio::test]
async fn test_complete_job_failure_one_below_max_retries_requeues() {
    let (_dir, queue, service) = temp_dispatch(3);

    // Job with retry_count = 2, max_retries = 3 → should requeue (2 < 3)
    queue
        .push(test_job("job-below-max"), "/tmp/below.pdf".into())
        .unwrap();
    fail_and_requeue(&queue, "job-below-max", "err1"); // retry_count = 1
    fail_and_requeue(&queue, "job-below-max", "err2"); // retry_count = 2

    let completion = JobCompletion {
        job_id: "job-below-max".into(),
        success: false,
        pages_printed: 0,
        error_detail: "transient".into(),
        printer_status: String::new(),
        spooler_status: String::new(),
        verification_method: String::new(),
        verification_evidence: String::new(),
        client_id: String::new(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let loaded = queue.get_job("job-below-max").unwrap().unwrap();
    assert_eq!(loaded.state, JobState::Queued);
    assert_eq!(loaded.retry_count, 3);
}

#[tokio::test]
async fn test_complete_job_stores_verification_evidence() {
    let (_dir, queue, service) = temp_dispatch(3);
    queue
        .push(test_job("job-verified"), "/tmp/verified.pdf".into())
        .unwrap();

    let completion = JobCompletion {
        job_id: "job-verified".into(),
        success: true,
        pages_printed: 1,
        error_detail: String::new(),
        printer_status: "delivered".into(),
        spooler_status: "windows_spooler".into(),
        verification_method: "eventid_307".into(),
        verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
        client_id: "holla-client".into(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();

    let job = queue.get_job("job-verified").unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);

    let events = queue.get_job_events("job-verified").unwrap();
    let completed_event = events
        .iter()
        .find(|e| e.stage == devbridge_core::job_event::PrintStage::Completed);
    assert!(completed_event.is_some(), "should have a Completed event");
    let ev = completed_event.unwrap();
    assert_eq!(ev.verification_method, "eventid_307");
    assert!(ev.verification_evidence.contains("EventID 307"));
}

#[tokio::test]
async fn test_complete_job_client_mismatch_emits_warning() {
    let (_dir, queue, service) = temp_dispatch(3);

    let mut job = test_job("job-mismatch");
    job.target_client_id = Some("holla-client".into());
    queue.push(job, "/tmp/mismatch.pdf".into()).unwrap();

    let completion = JobCompletion {
        job_id: "job-mismatch".into(),
        success: true,
        pages_printed: 1,
        error_detail: String::new(),
        printer_status: "delivered".into(),
        spooler_status: "direct_ipp".into(),
        verification_method: "ipp_job_state".into(),
        verification_evidence: "IPP job-state=9 (completed)".into(),
        client_id: "pjpos-client".into(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();

    let events = queue.get_job_events("job-mismatch").unwrap();
    let mismatch_event = events
        .iter()
        .find(|e| e.verification_method == "client_mismatch");
    assert!(
        mismatch_event.is_some(),
        "should have a client_mismatch warning"
    );
    let ev = mismatch_event.unwrap();
    assert!(ev.detail.contains("pjpos-client"));
    assert!(ev.detail.contains("holla-client"));
}

#[tokio::test]
async fn test_complete_job_unpaired_no_mismatch_warning() {
    let (_dir, queue, service) = temp_dispatch(3);
    queue
        .push(test_job("job-unpaired"), "/tmp/unpaired.pdf".into())
        .unwrap();

    let completion = JobCompletion {
        job_id: "job-unpaired".into(),
        success: true,
        pages_printed: 1,
        error_detail: String::new(),
        printer_status: "delivered".into(),
        spooler_status: "direct_ipp".into(),
        verification_method: "ipp_job_state".into(),
        verification_evidence: "IPP job-state=9 (completed)".into(),
        client_id: "any-client".into(),
        unconfirmed: false,
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();

    let events = queue.get_job_events("job-unpaired").unwrap();
    let mismatch = events
        .iter()
        .any(|e| e.verification_method == "client_mismatch");
    assert!(!mismatch, "unpaired jobs should not get mismatch warnings");
}

#[test]
fn test_stream_peers_follow_the_current_connection() {
    let peers = StreamPeers::default();
    let a: Option<IpAddr> = Some("10.78.2.10".parse().unwrap());
    let b: Option<IpAddr> = Some("10.78.9.10".parse().unwrap());
    assert_eq!(peers.peer_of("pjsnvs"), None, "never connected");
    peers.connected("pjsnvs", "c1", a);
    assert_eq!(peers.peer_of("pjsnvs"), a);
    assert_eq!(peers.peer_of("pjzav"), None, "per client");
    // a reconnect replaces the old stream
    peers.connected("pjsnvs", "c2", b);
    assert_eq!(peers.peer_of("pjsnvs"), b);
    // the OLD connection's late cleanup keeps the newer one
    peers.disconnected("pjsnvs", "c1");
    assert_eq!(peers.peer_of("pjsnvs"), b);
    peers.disconnected("pjsnvs", "c2");
    assert_eq!(peers.peer_of("pjsnvs"), None);
    // an unknown peer address is no address
    peers.connected("pjzav", "c3", None);
    assert_eq!(peers.peer_of("pjzav"), None);
}

/// #114 (owner ruling 20.10.2026: neposielať znova). The printer ACCEPTED the
/// job but its end was never confirmed (pjkes 15.10.: `accepted job-id=45` →
/// polling timeout → resent → risk of a second page). The server must NOT
/// retry it: terminal `unconfirmed`, retry_count 0, its event says why, and
/// neither the stale reaper nor the retry query ever picks it up again.
#[tokio::test]
async fn test_unconfirmed_completion_is_terminal_and_never_retried() {
    let (_dir, queue, service) = temp_dispatch(3);
    queue
        .push(test_job("job-unconfirmed"), "/tmp/unconfirmed.pdf".into())
        .unwrap();
    queue
        .update_state("job-unconfirmed", JobState::Printing)
        .unwrap();
    let detail = "printer accepted job-id=45, completion not confirmed \
                  (no final job-state within 60s, last state=6) — not resent";
    let completion = JobCompletion {
        job_id: "job-unconfirmed".into(),
        success: false,
        unconfirmed: true,
        error_detail: detail.into(),
        verification_method: "ipp_job_state".into(),
        verification_evidence: detail.into(),
        client_id: "pjkes-client".into(),
        ..Default::default()
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();
    // retry_delay_secs = 0 here: a requeue would already have happened
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let job = queue.get_job("job-unconfirmed").unwrap().unwrap();
    assert_eq!(job.state, JobState::Unconfirmed);
    assert_eq!(job.retry_count, 0, "never resent");
    let events = queue.get_job_events("job-unconfirmed").unwrap();
    let last = events.last().expect("the completion event");
    assert_eq!(
        last.stage,
        devbridge_core::job_event::PrintStage::Unconfirmed
    );
    assert!(!last.success);
    assert_eq!(last.detail, detail);
    assert_eq!(last.verification_method, "ipp_job_state");

    // the stale reaper (downloading / printing) never takes it back
    queue
        .push(test_job("job-in-flight"), "/tmp/in-flight.pdf".into())
        .unwrap();
    queue
        .update_state("job-in-flight", JobState::Downloading)
        .unwrap();
    let stale: Vec<String> = queue
        .get_stale_jobs(0)
        .unwrap()
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(stale, vec!["job-in-flight".to_string()]);
}

/// A completion of an older client (no `unconfirmed` field = false) is
/// retried exactly as before #114.
#[tokio::test]
async fn test_failure_without_unconfirmed_is_still_retried() {
    let (_dir, queue, service) = temp_dispatch(3);
    queue
        .push(test_job("job-old-client"), "/tmp/old-client.pdf".into())
        .unwrap();
    let completion = JobCompletion {
        job_id: "job-old-client".into(),
        success: false,
        error_detail: "IPP job-state polling timeout after 60s, job-id=45, last state=6".into(),
        ..Default::default()
    };
    service
        .complete_job(Request::new(completion))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let job = queue.get_job("job-old-client").unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.retry_count, 1);
}

/// #114 (deep review of the v0.8.52 PR, finding 1): a failure the client
/// reports for a job that is already FINISHED — e.g. the abandoned backend
/// task's late result after the job ended `unconfirmed` — must neither mark
/// it failed nor schedule a retry: the delayed requeue would resend a job the
/// printer may have printed. The failure itself stays on record. (A
/// `completed` job is the case below: it becomes `unconfirmed`.)
#[tokio::test]
async fn test_a_late_failure_never_reopens_a_terminal_job() {
    let (_dir, queue, service) = temp_dispatch(3);
    let terminal = [JobState::Unconfirmed, JobState::Cancelled];
    for state in terminal {
        let id = format!("job-late-{state:?}");
        queue.push(test_job(&id), "/tmp/late.pdf".into()).unwrap();
        queue.update_state(&id, state).unwrap();
        let completion = JobCompletion {
            job_id: id.clone(),
            success: false,
            error_detail: "print cancelled by outer timeout (at: between IPP page sends)".into(),
            ..Default::default()
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();
    }
    // retry_delay_secs = 0 here: a requeue would already have happened
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    for state in terminal {
        let id = format!("job-late-{state:?}");
        let job = queue.get_job(&id).unwrap().unwrap();
        assert_eq!(job.state, state, "{id} stays finished");
        assert_eq!(job.retry_count, 0, "{id} is never resent");
        let events = queue.get_job_events(&id).unwrap();
        let last = events.last().expect("the failure event");
        assert_eq!(last.stage, PrintStage::Failed, "{id}: kept for the audit");
        assert!(last.detail.contains("between IPP page sends"), "{id}");
    }
}

/// #114 (review of the follow-up, finding 1): an older client (v0.8.51 /
/// v0.8.52) sends `completed` for EVERY page of a per-page job (pjpos' Epson
/// `jpeg`), so the server finishes the job after page 1; when page 2 then
/// fails, its plain failure arrives on a `completed` job. Partly printed:
/// the job becomes `unconfirmed` („možno vytlačené", Reprint kept) — never
/// left a green "completed", never resent.
#[tokio::test]
async fn test_a_failure_after_completed_ends_unconfirmed() {
    let (_dir, queue, service) = temp_dispatch(3);
    queue
        .push(test_job("job-partly"), "/tmp/partly.pdf".into())
        .unwrap();
    queue
        .update_state("job-partly", JobState::Completed)
        .unwrap();

    service
        .complete_job(Request::new(JobCompletion {
            job_id: "job-partly".into(),
            success: false,
            error_detail: "IPP job-state=8 (aborted), job-id=7".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    // retry_delay_secs = 0 here: a requeue would already have happened
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let job = queue.get_job("job-partly").unwrap().unwrap();
    assert_eq!(job.state, JobState::Unconfirmed, "partly printed");
    assert_eq!(job.retry_count, 0, "never resent");
    let events = queue.get_job_events("job-partly").unwrap();
    let failed = events
        .iter()
        .find(|e| e.stage == PrintStage::Failed)
        .expect("the failure stays on record");
    assert!(failed.detail.contains("job-state=8"), "{}", failed.detail);
    let last = events.last().expect("events");
    assert_eq!(last.stage, PrintStage::Unconfirmed);
    assert!(
        last.detail.contains("after it was reported completed"),
        "{}",
        last.detail
    );
}

/// #114 (review of the follow-up, finding 2): a job id waiting in a channel
/// is sent only while the job is still `queued` — one that finished (or was
/// already sent) meanwhile is never streamed to the client again.
#[tokio::test]
async fn test_send_job_sends_only_a_queued_job() {
    let (_dir, queue, _service) = temp_dispatch(3);
    for state in [
        JobState::Unconfirmed,
        JobState::Completed,
        JobState::Downloading,
    ] {
        let id = format!("job-not-queued-{state:?}");
        queue.push(test_job(&id), "/tmp/nq.pdf".into()).unwrap();
        queue.update_state(&id, state).unwrap();

        let (tx, mut rx) = mpsc::channel::<Result<PrintJob, Status>>(1);
        send_job(&tx, &queue, &id).await.unwrap();
        drop(tx);
        assert!(rx.recv().await.is_none(), "{state:?}: not sent");
    }
}

/// A client's status update for `job_id` as `report_status` receives it:
/// the wire state plus the audit event (`stage`, detail "late update").
fn status_update(
    job_id: &str,
    wire: devbridge_core::proto::JobState,
    stage: PrintStage,
) -> JobStatusUpdate {
    let event = PrintJobEvent::fail(job_id, stage, "late update");
    JobStatusUpdate {
        job_id: job_id.into(),
        state: wire as i32,
        message: serde_json::to_string(&event).unwrap(),
        timestamp: None,
    }
}

/// #114 (deep review of the v0.8.52 PR, findings 4 + 6): the status stream
/// races CompleteJob. A late `failed` / `printing` update must never move a
/// FINISHED job (completed, cancelled, unconfirmed) back — `printing` hands
/// it to the stale reaper, `failed` to a retry — and its audit event is
/// still stored.
#[test]
fn test_a_late_status_update_never_moves_a_terminal_job() {
    use devbridge_core::proto::JobState as Wire;
    let (_dir, queue, _service) = temp_dispatch(3);
    for terminal in [
        JobState::Unconfirmed,
        JobState::Completed,
        JobState::Cancelled,
    ] {
        for (wire, stage) in [
            (Wire::Failed, PrintStage::Failed),
            (Wire::Printing, PrintStage::Sending),
        ] {
            let id = format!("job-{terminal:?}-{wire:?}");
            queue.push(test_job(&id), "/tmp/late.pdf".into()).unwrap();
            queue.update_state(&id, terminal).unwrap();

            apply_status_update(&queue, &status_update(&id, wire, stage)).unwrap();

            let job = queue.get_job(&id).unwrap().unwrap();
            assert_eq!(job.state, terminal, "{id}");
            let events = queue.get_job_events(&id).unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| e.stage == stage && e.detail == "late update"),
                "{id}: the audit event is stored: {events:?}"
            );
        }
    }
}

/// A job still in progress takes the client's updates as before — `failed`
/// too (it waits for its retry, it is not finished) — and an `unconfirmed`
/// event (FAILED on the wire) is stored `unconfirmed` at once.
#[test]
fn test_a_status_update_moves_a_job_in_progress() {
    use devbridge_core::proto::JobState as Wire;
    let (_dir, queue, _service) = temp_dispatch(3);
    for current in [
        JobState::Queued,
        JobState::Downloading,
        JobState::Printing,
        JobState::Failed,
    ] {
        let id = format!("job-moving-{current:?}");
        queue.push(test_job(&id), "/tmp/moving.pdf".into()).unwrap();
        queue.update_state(&id, current).unwrap();

        apply_status_update(
            &queue,
            &status_update(&id, Wire::Printing, PrintStage::Sending),
        )
        .unwrap();
        assert_eq!(
            queue.get_job(&id).unwrap().unwrap().state,
            JobState::Printing,
            "{current:?}"
        );

        apply_status_update(
            &queue,
            &status_update(&id, Wire::Failed, PrintStage::Unconfirmed),
        )
        .unwrap();
        assert_eq!(
            queue.get_job(&id).unwrap().unwrap().state,
            JobState::Unconfirmed,
            "{current:?}"
        );
    }
}

/// #114: the client's `unconfirmed` event arrives as FAILED on the wire; this
/// server stores `unconfirmed` straight away (no transient `failed` with its
/// "Print Failed" notification). Every other event keeps the wire state.
#[test]
fn test_an_unconfirmed_event_is_stored_unconfirmed_at_once() {
    use devbridge_core::job_event::PrintStage;
    assert_eq!(
        status_update_state(JobState::Failed, Some(PrintStage::Unconfirmed)),
        JobState::Unconfirmed
    );
    assert_eq!(
        status_update_state(JobState::Failed, Some(PrintStage::Failed)),
        JobState::Failed
    );
    assert_eq!(
        status_update_state(JobState::Printing, Some(PrintStage::Acknowledged)),
        JobState::Printing
    );
    assert_eq!(
        status_update_state(JobState::Failed, None),
        JobState::Failed
    );
}
