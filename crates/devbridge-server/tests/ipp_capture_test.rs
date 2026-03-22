//! Integration test for the IPP capture pipeline.
//!
//! Starts an IPP server on a random port, sends a raw IPP Print-Job request,
//! and verifies the job appears in the queue with correct metadata and spool file.

use std::sync::Arc;

use devbridge_core::config::ServerConfig;
use devbridge_core::config::TlsConfig;
use devbridge_core::job::JobState;
use devbridge_server::ipp_service::IppServer;
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

/// Build a minimal IPP Print-Job request (same as E2E binary).
fn build_ipp_print_job(pdf_data: &[u8]) -> Vec<u8> {
    // IPP header: version 1.1, operation Print-Job (0x0002)
    let mut buf = vec![1, 1, 0x00, 0x02];

    // Request ID
    buf.extend_from_slice(&1u32.to_be_bytes());

    // Operation attributes tag
    buf.push(0x01);

    // charset
    buf.push(0x47);
    let name = b"attributes-charset";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"utf-8";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // natural language
    buf.push(0x48);
    let name = b"attributes-natural-language";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"en-us";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // printer-uri
    buf.push(0x45);
    let name = b"printer-uri";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"ipp://localhost/ipp/print";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // document-format
    buf.push(0x49);
    let name = b"document-format";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = b"application/pdf";
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

    // End of attributes
    buf.push(0x03);

    // Document data
    buf.extend_from_slice(pdf_data);

    buf
}

#[tokio::test]
async fn test_ipp_capture_queues_job() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let storage = Storage::new(&db_path).unwrap();
    let queue = Arc::new(JobQueue::new(storage).unwrap());

    // Use port 0 to get a random available port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    // We can't use IppServer::run() directly (it binds its own socket),
    // so we configure it with the port we know and let it bind.
    // Drop the listener first so the port is free.
    drop(listener);

    let config = ServerConfig {
        ipp_port: port,
        grpc_port: 0,
        dashboard_port: 0,
        printer_name: "TestIPPPrinter".into(),
        spool_dir: spool_dir.to_string_lossy().to_string(),
        tls: TlsConfig {
            cert_file: String::new(),
            key_file: String::new(),
            ca_file: String::new(),
        },
    };

    let ipp_server = IppServer::new(config, Arc::clone(&queue));

    // Spawn IPP server in background
    tokio::spawn(async move {
        let _ = ipp_server.run().await;
    });

    // Wait for server to start accepting connections
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Create a test PDF payload (minimal but non-empty)
    let pdf_data = b"%PDF-1.4 test content for IPP capture verification";

    let ipp_payload = build_ipp_print_job(pdf_data);

    // Submit the IPP job
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/ipp/print"))
        .header("Content-Type", "application/ipp")
        .body(ipp_payload)
        .send()
        .await
        .expect("Failed to submit IPP job");

    assert!(
        resp.status().is_success(),
        "IPP submission failed: {}",
        resp.status()
    );

    // Give the server a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify job appears in the queue
    let jobs = queue.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1, "Expected exactly 1 job in queue");

    let job = &jobs[0];
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.payload_size, pdf_data.len() as u64);
    assert!(!job.payload_sha256.is_empty(), "SHA256 should be set");
    assert!(!job.job_id.is_empty(), "Job ID should be set");

    // Verify spool file was written
    let spool_file = spool_dir.join(format!("{}.pdf", job.job_id));
    assert!(spool_file.exists(), "Spool file should exist");
    let spool_contents = std::fs::read(&spool_file).unwrap();
    assert_eq!(spool_contents, pdf_data, "Spool file contents should match");

    // Verify SHA256
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pdf_data);
    let expected_sha = format!("{:x}", hasher.finalize());
    assert_eq!(job.payload_sha256, expected_sha);
}
