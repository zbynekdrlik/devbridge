//! Integration tests for the gRPC job transfer pipeline.
//!
//! These tests start a real tonic gRPC server in-process, connect a client,
//! and exercise the full subscribe -> download -> complete flow.

use std::sync::Arc;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::print_bridge_server::PrintBridgeServer;
use devbridge_core::proto::{ClientIdentity, JobCompletion, PayloadRequest};

use devbridge_server::dispatch::DispatchService;
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

/// Helper to create a test job with a known SHA256 for the given payload.
fn make_test_job(job_id: &str, payload: &[u8]) -> JobMetadata {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let sha = format!("{:x}", hasher.finalize());

    JobMetadata {
        job_id: job_id.to_string(),
        document_name: "test-document.pdf".into(),
        target_printer: "TestPrinter".into(),
        copies: 1,
        paper_size: "A4".into(),
        duplex: false,
        color: true,
        payload_size: payload.len() as u64,
        payload_sha256: sha,
        state: JobState::Queued,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Spin up a DispatchService on a random port and return the address.
async fn start_server(queue: Arc<JobQueue>, spool_dir: std::path::PathBuf) -> std::net::SocketAddr {
    let dispatch = DispatchService::new(Arc::clone(&queue), spool_dir);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(PrintBridgeServer::new(dispatch))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Brief yield to let the server task start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    addr
}

#[tokio::test]
async fn test_job_subscribe_and_download() {
    // -- Setup: temp dirs, storage, queue, spool file --------------------------
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let payload = b"Hello, this is a test print payload for gRPC transfer.";

    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    // Write spool file and push job into the queue.
    let job_meta = make_test_job("grpc-job-1", payload);
    let spool_path = spool_dir.join("grpc-job-1.bin");
    std::fs::write(&spool_path, payload).unwrap();
    queue
        .push(job_meta.clone(), spool_path.to_str().unwrap().to_string())
        .unwrap();

    // -- Start server and connect client ----------------------------------------
    let addr = start_server(Arc::clone(&queue), spool_dir.clone()).await;
    let mut client = PrintBridgeClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // -- SubscribeJobs: receive the job we just pushed --------------------------
    let identity = ClientIdentity {
        machine_id: "test-machine".into(),
        hostname: "localhost".into(),
        printer_names: vec!["TestPrinter".into()],
        client_version: "0.1.0".into(),
    };

    let mut stream = client.subscribe_jobs(identity).await.unwrap().into_inner();

    let received_job = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("timed out waiting for job")
        .expect("stream ended unexpectedly")
        .expect("gRPC error receiving job");

    assert_eq!(received_job.job_id, "grpc-job-1");
    assert_eq!(received_job.document_name, "test-document.pdf");
    assert_eq!(received_job.target_printer, "TestPrinter");
    assert_eq!(received_job.copies, 1);
    assert_eq!(received_job.paper_size, "A4");
    assert!(!received_job.duplex);
    assert!(received_job.color);
    assert_eq!(received_job.payload_size, payload.len() as u64);
    assert_eq!(received_job.payload_sha256, job_meta.payload_sha256);

    // -- DownloadPayload: receive all chunks and verify SHA256 ------------------
    let download_req = PayloadRequest {
        job_id: "grpc-job-1".into(),
        offset: 0,
    };

    let mut chunk_stream = client
        .download_payload(download_req)
        .await
        .unwrap()
        .into_inner();

    let mut downloaded = Vec::new();
    let mut saw_last_chunk = false;

    while let Some(chunk) = chunk_stream.next().await {
        let chunk = chunk.expect("gRPC error receiving chunk");
        assert_eq!(chunk.job_id, "grpc-job-1");
        downloaded.extend_from_slice(&chunk.data);
        if chunk.is_last {
            saw_last_chunk = true;
        }
    }

    assert!(saw_last_chunk, "never received a chunk with is_last = true");
    assert_eq!(downloaded, payload);

    let mut hasher = Sha256::new();
    hasher.update(&downloaded);
    let computed_sha = format!("{:x}", hasher.finalize());
    assert_eq!(computed_sha, job_meta.payload_sha256);

    // -- CompleteJob: mark as completed -----------------------------------------
    let completion = JobCompletion {
        job_id: "grpc-job-1".into(),
        success: true,
        error_detail: String::new(),
        pages_printed: 1,
    };

    let ack = client.complete_job(completion).await;
    assert!(ack.is_ok(), "complete_job should succeed");

    // Verify the job state in storage is now Completed.
    let stored = queue.get_job("grpc-job-1").unwrap().unwrap();
    assert_eq!(stored.state, JobState::Completed);
}

#[tokio::test]
async fn test_resumable_download() {
    // -- Setup ------------------------------------------------------------------
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    // Use a payload larger than one chunk marker to make offsets meaningful.
    let payload: Vec<u8> = (0..1024u32).flat_map(|i| i.to_le_bytes()).collect();

    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    let job_meta = make_test_job("resume-job-1", &payload);
    let spool_path = spool_dir.join("resume-job-1.bin");
    std::fs::write(&spool_path, &payload).unwrap();
    queue
        .push(job_meta, spool_path.to_str().unwrap().to_string())
        .unwrap();

    let addr = start_server(Arc::clone(&queue), spool_dir.clone()).await;
    let mut client = PrintBridgeClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // -- Full download from offset 0 -------------------------------------------
    let full_req = PayloadRequest {
        job_id: "resume-job-1".into(),
        offset: 0,
    };
    let mut full_stream = client
        .download_payload(full_req)
        .await
        .unwrap()
        .into_inner();

    let mut full_data = Vec::new();
    while let Some(chunk) = full_stream.next().await {
        full_data.extend_from_slice(&chunk.unwrap().data);
    }
    assert_eq!(full_data, payload, "full download should match payload");

    // -- Partial download from a non-zero offset --------------------------------
    let resume_offset = 512_u64;
    let partial_req = PayloadRequest {
        job_id: "resume-job-1".into(),
        offset: resume_offset,
    };
    let mut partial_stream = client
        .download_payload(partial_req)
        .await
        .unwrap()
        .into_inner();

    let mut partial_data = Vec::new();
    let mut first_chunk = true;
    while let Some(chunk) = partial_stream.next().await {
        let chunk = chunk.unwrap();
        if first_chunk {
            assert_eq!(
                chunk.offset, resume_offset,
                "first chunk offset should match requested offset"
            );
            first_chunk = false;
        }
        partial_data.extend_from_slice(&chunk.data);
    }

    assert_eq!(
        partial_data,
        &payload[resume_offset as usize..],
        "partial download should match payload from the given offset"
    );
}
