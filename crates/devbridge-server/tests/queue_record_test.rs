//! Tests for `JobQueue::record_job()` and `JobQueue::update_job_state()`.
//!
//! These methods are used by the client to track jobs received from the server
//! without invoking the routing logic that `push()` uses.

use std::sync::Arc;

use chrono::Utc;
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

fn make_test_meta(job_id: &str) -> JobMetadata {
    let now = Utc::now();
    JobMetadata {
        job_id: job_id.to_string(),
        document_name: "receipt.pdf".into(),
        target_printer: "EPSON L3270".into(),
        target_client_id: None,
        copies: 1,
        paper_size: "A4".into(),
        duplex: false,
        color: true,
        payload_size: 1024,
        payload_sha256: "abc123".into(),
        state: JobState::Downloading,
        retry_count: 0,
        error_detail: String::new(),
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn test_record_job_inserts_and_retrieves() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    let meta = make_test_meta("job-record-1");
    queue
        .record_job(&meta, "/tmp/spool/job-record-1.pdf")
        .unwrap();

    let jobs = queue.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, "job-record-1");
    assert_eq!(jobs[0].state, JobState::Downloading);
    assert_eq!(jobs[0].target_printer, "EPSON L3270");
    assert_eq!(jobs[0].document_name, "receipt.pdf");
    assert_eq!(jobs[0].payload_size, 1024);
}

#[test]
fn test_update_job_state_transitions() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    let meta = make_test_meta("job-state-1");
    queue
        .record_job(&meta, "/tmp/spool/job-state-1.pdf")
        .unwrap();

    // Downloading -> Printing
    queue
        .update_job_state("job-state-1", JobState::Printing)
        .unwrap();
    let job = queue.get_job("job-state-1").unwrap().unwrap();
    assert_eq!(job.state, JobState::Printing);

    // Printing -> Completed
    queue
        .update_job_state("job-state-1", JobState::Completed)
        .unwrap();
    let job = queue.get_job("job-state-1").unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);
}

#[test]
fn test_update_job_state_to_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    let meta = make_test_meta("job-fail-1");
    queue
        .record_job(&meta, "/tmp/spool/job-fail-1.pdf")
        .unwrap();

    queue
        .update_job_state("job-fail-1", JobState::Failed)
        .unwrap();
    let job = queue.get_job("job-fail-1").unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
}
