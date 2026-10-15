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
    queue.requeue_job("job-rc", "err1").unwrap(); // retry_count = 1
    queue.requeue_job("job-rc", "err2").unwrap(); // retry_count = 2
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
    queue.requeue_job("job-maxretry", "err1").unwrap(); // retry_count = 1
    queue.requeue_job("job-maxretry", "err2").unwrap(); // retry_count = 2

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
    queue.requeue_job("job-below-max", "err1").unwrap(); // retry_count = 1
    queue.requeue_job("job-below-max", "err2").unwrap(); // retry_count = 2

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
