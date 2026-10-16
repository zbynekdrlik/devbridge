//! The LAN API certificate from pz-server (#101): the client makes its key
//! and CSR, asks the REAL gRPC `DispatchService` (`RequestCertificate`), whose
//! certificate authority here is a test CA instead of Let's Encrypt (the
//! ACME order itself runs against Pebble in the `ACME (Pebble)` CI job);
//! the listener starts with the issued certificate. pz-server issues only to
//! a request from the address of the client's live job stream (the receiver
//! keeps it open; the test holds one). An unapproved client gets nothing and
//! its listener never starts.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use rcgen::{CertificateParams, CertificateSigningRequestParams, Issuer, KeyPair};
use rustls::pki_types::CertificateSigningRequestDer;

use devbridge_client::lan_api::cert::{CertPaths, CertStatus, status_of};
use devbridge_core::client_registration::{ClientRegistration, PairingState};
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::print_bridge_server::PrintBridgeServer;
use devbridge_core::proto::{ClientIdentity, PrintJob};
use devbridge_server::cert::{CertificateAuthority, CertificateIssuer};
use devbridge_server::dispatch::DispatchService;
use devbridge_server::queue::JobQueue;
use devbridge_server::serial_bridge::SerialBridgeManager;
use devbridge_server::storage::Storage;

use super::fake::*;
use super::lan::*;

/// Signs every CSR with a test CA (what Let's Encrypt does in production).
struct TestCa {
    params: CertificateParams,
    key: KeyPair,
    pem: String,
    calls: AtomicUsize,
}

#[async_trait]
impl CertificateAuthority for TestCa {
    async fn issue(&self, _hostname: &str, csr_der: &[u8]) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let csr = CertificateSigningRequestParams::from_der(&CertificateSigningRequestDer::from(
            csr_der,
        ))?;
        let leaf = csr.signed_by(&Issuer::from_params(&self.params, &self.key))?;
        Ok(format!("{}{}", leaf.pem(), self.pem))
    }
}

fn client(id: &str, state: PairingState) -> ClientRegistration {
    ClientRegistration {
        machine_id: id.into(),
        hostname: id.into(),
        printer_names: vec![],
        client_version: "0".into(),
        last_seen: Utc::now(),
        is_online: false,
        pairing_state: state,
        virtual_printer_name: None,
        virtual_printer_driver: None,
    }
}

/// pz-server's gRPC service with `ca` behind `RequestCertificate`; returns
/// its address.
async fn start_pz_server(dir: &std::path::Path, ca: Arc<TestCa>) -> String {
    let queue = Arc::new(JobQueue::new(Storage::new(&dir.join("server.db")).unwrap()).unwrap());
    queue
        .upsert_client(&client("pjsnvs", PairingState::Approved))
        .unwrap();
    queue
        .upsert_client(&client("pjzav", PairingState::Pending))
        .unwrap();
    let issuer = CertificateIssuer::new("pos.example.sk", ca as Arc<dyn CertificateAuthority>);
    let dispatch = DispatchService::new(
        Arc::clone(&queue),
        dir.join("spool"),
        Arc::new(AtomicU64::new(0)),
        3,
        0,
        Arc::new(SerialBridgeManager::new(vec![])),
    )
    .with_certificates(Arc::new(issuer));
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
    addr.to_string()
}

/// The live job stream of `machine_id` (what the store's receiver keeps
/// open to `server`).
async fn subscribe(server: &str, machine_id: &str) -> tonic::Streaming<PrintJob> {
    let mut client = PrintBridgeClient::connect(format!("http://{server}"))
        .await
        .unwrap();
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

fn test_ca_issuer() -> Arc<TestCa> {
    let (params, key, pem) = test_ca();
    Arc::new(TestCa {
        params,
        key,
        pem,
        calls: AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn test_the_listener_starts_with_a_certificate_issued_by_pz_server() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = test_ca_issuer();
    let server = start_pz_server(dir.path(), Arc::clone(&ca)).await;
    let _stream = subscribe(&server, "pjsnvs").await;
    let (journal, device) = open_files(dir.path());
    let mut cfg = lan_config(&url);
    cfg.lan_api.acme = true;
    let lan = start_lan(
        cfg,
        journal,
        test_registry(&device),
        timing(),
        dir.path(),
        &ca.pem,
        &server,
    );
    let paths = CertPaths::in_data_dir(dir.path());
    eventually("certificate installed", || {
        matches!(status_of(&paths, HOST), CertStatus::Valid { .. })
    })
    .await;
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&paths.key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // HTTPS with the issued certificate, trusted through the test CA
    let mut answered = None;
    for _ in 0..100 {
        if let Ok(resp) = lan
            .https
            .get(format!("{}/v1/health", lan.base))
            .bearer_auth(KEY3)
            .send()
            .await
        {
            answered = Some(resp.status().as_u16());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(answered, Some(200));
    lan.run.stop().await;
    assert_eq!(
        ca.calls.load(Ordering::SeqCst),
        1,
        "a valid certificate is not renewed"
    );
}

#[tokio::test]
async fn test_an_unapproved_client_gets_no_certificate() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = test_ca_issuer();
    let server = start_pz_server(dir.path(), Arc::clone(&ca)).await;
    let _stream = subscribe(&server, "pjzav").await;
    let (journal, device) = open_files(dir.path());
    let mut cfg = lan_config(&url);
    cfg.lan_api.acme = true;
    cfg.lan_api.hostname = "pjzav.pos.example.sk".into();
    let source = devbridge_client::device_jobs::DeviceJobSource::new(
        cfg,
        journal,
        test_registry(&device),
        timing(),
    )
    .unwrap()
    .with_lan_api(devbridge_client::lan_api::LanApiDeps {
        data_dir: dir.path().to_path_buf(),
        server_address: server,
        client_id: "pjzav".into(),
        listener: None,
    });
    let shutdown = tokio_util::sync::CancellationToken::new();
    let run = Running {
        shutdown: shutdown.clone(),
        handle: tokio::spawn(source.run(shutdown)),
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    run.stop().await;
    let paths = CertPaths::in_data_dir(dir.path());
    assert_eq!(
        status_of(&paths, "pjzav.pos.example.sk"),
        CertStatus::Missing
    );
    assert_eq!(ca.calls.load(Ordering::SeqCst), 0, "the CA was never asked");
}
