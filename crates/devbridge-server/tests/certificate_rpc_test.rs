//! `RequestCertificate` over real gRPC (#101): only an APPROVED client with
//! a live job stream from the same address gets its LAN API certificate; a
//! server without `[server.acme]` answers `acme_disabled`. The CA is a fake
//! here (the ACME order runs against Pebble in `acme_pebble.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;

use devbridge_core::client_registration::{ClientRegistration, PairingState};
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::print_bridge_server::PrintBridgeServer;
use devbridge_core::proto::{CertificateRequest, ClientIdentity, PrintJob};
use devbridge_server::cert::{CertificateAuthority, CertificateIssuer};
use devbridge_server::dispatch::DispatchService;
use devbridge_server::queue::JobQueue;
use devbridge_server::serial_bridge::SerialBridgeManager;
use devbridge_server::storage::Storage;

#[derive(Default)]
struct FakeCa {
    calls: AtomicUsize,
}

#[async_trait]
impl CertificateAuthority for FakeCa {
    async fn issue(&self, hostname: &str, _csr_der: &[u8]) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("CHAIN {hostname}"))
    }
}

fn register(queue: &JobQueue, id: &str, state: PairingState) {
    queue
        .upsert_client(&ClientRegistration {
            machine_id: id.into(),
            hostname: id.into(),
            printer_names: vec![],
            client_version: "0".into(),
            last_seen: Utc::now(),
            is_online: false,
            pairing_state: state,
            virtual_printer_name: None,
            virtual_printer_driver: None,
        })
        .unwrap();
}

async fn serve(dir: &std::path::Path, ca: Option<Arc<FakeCa>>) -> String {
    let queue = Arc::new(JobQueue::new(Storage::new(&dir.join("t.db")).unwrap()).unwrap());
    register(&queue, "pjsnvs", PairingState::Approved);
    register(&queue, "pending-store", PairingState::Pending);
    register(&queue, "rejected-store", PairingState::Rejected);
    let mut dispatch = DispatchService::new(
        Arc::clone(&queue),
        dir.join("spool"),
        Arc::new(AtomicU64::new(0)),
        3,
        0,
        Arc::new(SerialBridgeManager::new(vec![])),
    );
    if let Some(ca) = ca {
        let issuer = CertificateIssuer::new("pos.example.sk", ca as Arc<dyn CertificateAuthority>);
        dispatch = dispatch.with_certificates(Arc::new(issuer));
    }
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
    format!("http://{addr}")
}

fn csr(host: &str) -> Vec<u8> {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let request = params.serialize_request(&key).unwrap();
    let der: &[u8] = request.der();
    der.to_vec()
}

/// A live job stream of `machine_id`, as the store's receiver keeps it open.
async fn subscribe(url: &str, machine_id: &str) -> tonic::Streaming<PrintJob> {
    let mut client = PrintBridgeClient::connect(url.to_string()).await.unwrap();
    client
        .subscribe_jobs(ClientIdentity {
            machine_id: machine_id.into(),
            hostname: machine_id.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
}

fn request(client_id: &str) -> CertificateRequest {
    let host = format!("{client_id}.pos.example.sk");
    CertificateRequest {
        client_id: client_id.into(),
        csr_der: csr(&host),
        hostname: host,
    }
}

#[tokio::test]
async fn test_only_an_approved_client_gets_its_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(FakeCa::default());
    let url = serve(dir.path(), Some(Arc::clone(&ca))).await;
    let mut client = PrintBridgeClient::connect(url.clone()).await.unwrap();

    // approved, but no live job stream: refused, nothing ordered
    let offline = client
        .request_certificate(request("pjsnvs"))
        .await
        .unwrap()
        .into_inner();
    assert!(!offline.ok);
    assert_eq!(offline.error_code, "not_connected");
    assert_eq!(ca.calls.load(Ordering::SeqCst), 0);

    // the store's receiver is connected (from this address): issued
    let _stream = subscribe(&url, "pjsnvs").await;
    let ok = client
        .request_certificate(request("pjsnvs"))
        .await
        .unwrap()
        .into_inner();
    assert!(ok.ok, "{ok:?}");
    assert_eq!(ok.chain_pem, "CHAIN pjsnvs.pos.example.sk");
    assert!(ok.error_code.is_empty() && ok.error.is_empty());

    for id in ["pending-store", "rejected-store", "unknown-store"] {
        let refused = client
            .request_certificate(request(id))
            .await
            .unwrap()
            .into_inner();
        assert!(!refused.ok, "{id}");
        assert_eq!(refused.error_code, "not_approved", "{id}");
        assert!(refused.chain_pem.is_empty());
    }
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_without_acme_every_request_is_answered_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let url = serve(dir.path(), None).await;
    let mut client = PrintBridgeClient::connect(url).await.unwrap();
    let reply = client
        .request_certificate(request("pjsnvs"))
        .await
        .unwrap()
        .into_inner();
    assert!(!reply.ok);
    assert_eq!(reply.error_code, "acme_disabled");
    assert!(
        reply.error.contains("[server.acme] is not enabled"),
        "{}",
        reply.error
    );
}
