//! RAW passthrough (issue #88): a vendor-driver virtual printer (e.g. the
//! pz-spisska TSC ML241P label printer) receives the driver's native bytes as
//! an IPP Print-Job with `document-format = application/octet-stream`. Those
//! bytes must reach the client BYTE-IDENTICAL — IPP capture → spool → gRPC
//! DownloadPayload — with no PDF conversion or sniffing anywhere, while the
//! PDF path stays exactly as it was.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

use devbridge_core::proto::PayloadRequest;
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::print_bridge_server::PrintBridgeServer;
use devbridge_core::virtual_printer::VirtualPrinter;
use devbridge_server::dispatch::DispatchService;
use devbridge_server::ipp_service::IppServer;
use devbridge_server::queue::JobQueue;
use devbridge_server::storage::Storage;

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn push_attr(buf: &mut Vec<u8>, tag: u8, name: &[u8], value: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
}

/// Minimal IPP/1.1 Print-Job request for `ipp_name` with `document_format`.
fn build_print_job(ipp_name: &str, document_format: &str, document: &[u8]) -> Vec<u8> {
    let mut buf = vec![1, 1, 0x00, 0x02];
    buf.extend_from_slice(&7u32.to_be_bytes());
    buf.push(0x01); // operation-attributes-tag
    push_attr(&mut buf, 0x47, b"attributes-charset", b"utf-8");
    push_attr(&mut buf, 0x48, b"attributes-natural-language", b"en-us");
    let uri = format!("ipp://localhost/printers/{ipp_name}");
    push_attr(&mut buf, 0x45, b"printer-uri", uri.as_bytes());
    push_attr(&mut buf, 0x42, b"document-name", b"label.btw");
    push_attr(
        &mut buf,
        0x49,
        b"document-format",
        document_format.as_bytes(),
    );
    buf.push(0x03); // end-of-attributes-tag
    buf.extend_from_slice(document);
    buf
}

/// A TSPL label job the way the TSC driver spools it, plus every byte value
/// 0x00..=0xFF (incl. IPP tag bytes 0x01/0x03 and a fake `%PDF` header in
/// the middle) — anything that "sniffs" or re-encodes would change the hash.
fn tspl_payload() -> Vec<u8> {
    let mut p = b"SIZE 50 mm,30 mm\r\nGAP 2 mm,0 mm\r\nDENSITY 8\r\nSPEED 4\r\nCLS\r\n".to_vec();
    p.extend(0u8..=255);
    p.extend_from_slice(b"%PDF-1.4 not a pdf\r\n");
    p.extend_from_slice(b"TEXT 10,10,\"3\",0,1,1,\"DevBridge #88\"\r\nPRINT 1,1\r\n");
    p
}

fn vp(ipp_name: &str, client: &str, driver: Option<&str>) -> VirtualPrinter {
    let now = chrono::Utc::now();
    VirtualPrinter {
        id: format!("vp-{ipp_name}"),
        display_name: ipp_name.replace('-', " "),
        ipp_name: ipp_name.into(),
        paired_client_id: Some(client.into()),
        driver: driver.map(String::from),
        created_at: now,
        updated_at: now,
    }
}

async fn start_ipp(
    queue: &Arc<JobQueue>,
    spool_dir: &std::path::Path,
    vps: &[VirtualPrinter],
) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server = IppServer::new(port, Arc::clone(queue), spool_dir.to_path_buf());
    for v in vps {
        queue.insert_virtual_printer(v).unwrap();
        server.add_printer(v).await.unwrap();
    }
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    port
}

async fn start_grpc(queue: &Arc<JobQueue>, spool_dir: &std::path::Path) -> std::net::SocketAddr {
    use std::sync::atomic::AtomicU64;
    let dispatch = DispatchService::new(
        Arc::clone(queue),
        spool_dir.to_path_buf(),
        Arc::new(AtomicU64::new(0)),
        3,
        0,
        Arc::new(devbridge_server::serial_bridge::SerialBridgeManager::new(
            vec![],
        )),
    );
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

async fn submit(port: u16, ipp_name: &str, format: &str, document: &[u8]) {
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/printers/{ipp_name}"))
        .header("Content-Type", "application/ipp")
        .body(build_print_job(ipp_name, format, document))
        .send()
        .await
        .expect("IPP submit failed");
    assert!(resp.status().is_success(), "IPP status {}", resp.status());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

async fn download(addr: std::net::SocketAddr, job_id: &str) -> Vec<u8> {
    let mut client = PrintBridgeClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let mut stream = client
        .download_payload(PayloadRequest {
            job_id: job_id.into(),
            offset: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap().data);
    }
    out
}

#[tokio::test]
async fn test_octet_stream_job_reaches_raw_client_byte_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();
    let queue = Arc::new(JobQueue::new(Storage::new(&tmp.path().join("t.db")).unwrap()).unwrap());

    let label_vp = vp("spisska-stitky", "spisska-client", Some("TSC ML241P"));
    let port = start_ipp(&queue, &spool_dir, &[label_vp]).await;
    let grpc = start_grpc(&queue, &spool_dir).await;

    let payload = tspl_payload();
    submit(port, "spisska-stitky", "application/octet-stream", &payload).await;

    let jobs = queue.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.target_printer, "spisska-stitky");
    // Routed ONLY to the label client (paired VP), never the default queue.
    assert_eq!(job.target_client_id.as_deref(), Some("spisska-client"));
    assert_eq!(job.payload_size, payload.len() as u64);
    assert_eq!(job.payload_sha256, sha256_hex(&payload), "server hash");

    // Spool file holds the bytes verbatim …
    let spooled = std::fs::read(spool_dir.join(format!("{}.pdf", job.job_id))).unwrap();
    assert_eq!(spooled, payload, "spool file must be byte-identical");

    // … and so does what the client downloads over gRPC.
    let downloaded = download(grpc, &job.job_id).await;
    assert_eq!(downloaded.len(), payload.len());
    assert_eq!(sha256_hex(&downloaded), sha256_hex(&payload), "gRPC hash");
    assert_eq!(downloaded, payload);
}

#[tokio::test]
async fn test_pdf_job_path_unchanged_next_to_raw_printer() {
    // A normal store printer (default driver) keeps getting its PDF verbatim,
    // even when a RAW label printer exists on the same server.
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();
    let queue = Arc::new(JobQueue::new(Storage::new(&tmp.path().join("t.db")).unwrap()).unwrap());

    let store_vp = vp("pjsnvs-printer", "pjsnvs", None);
    let label_vp = vp("spisska-stitky", "spisska-client", Some("TSC ML241P"));
    let port = start_ipp(&queue, &spool_dir, &[store_vp, label_vp]).await;
    let grpc = start_grpc(&queue, &spool_dir).await;

    let pdf = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n".to_vec();
    submit(port, "pjsnvs-printer", "application/pdf", &pdf).await;

    let jobs = queue.get_all_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.target_printer, "pjsnvs-printer");
    assert_eq!(job.target_client_id.as_deref(), Some("pjsnvs"));
    assert_eq!(job.payload_sha256, sha256_hex(&pdf));
    assert_eq!(download(grpc, &job.job_id).await, pdf);

    // The store VP still resolves to the IPP Class Driver, the label VP to
    // its vendor driver.
    let vps = queue.list_virtual_printers().unwrap();
    let store = vps.iter().find(|v| v.ipp_name == "pjsnvs-printer").unwrap();
    let label = vps.iter().find(|v| v.ipp_name == "spisska-stitky").unwrap();
    assert_eq!(store.effective_driver(), "Microsoft IPP Class Driver");
    assert_eq!(label.effective_driver(), "TSC ML241P");
}
