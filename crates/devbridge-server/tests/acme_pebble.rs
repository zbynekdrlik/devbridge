//! A real ACME DNS-01 order (#101) against Pebble, Let's Encrypt's test CA,
//! with `pebble-challtestsrv` as the DNS the CA validates against — run by
//! the CI job `ACME (Pebble)` (feature `acme-pebble`), which starts both and
//! sets:
//!
//! - `DEVBRIDGE_PEBBLE_DIRECTORY` — e.g. `https://localhost:14000/dir`;
//! - `DEVBRIDGE_PEBBLE_ROOT` — Pebble's TLS root (`pebble.minica.pem`);
//! - `DEVBRIDGE_CHALLTESTSRV` — its management API, e.g.
//!   `http://127.0.0.1:8055`.
//!
//! The DNS provider here sets / clears the TXT records through
//! challtestsrv — the production one is Cloudflare (`cert::dns`, tested
//! against a fake API). Missing env = the test FAILS (never skipped).
#![cfg(feature = "acme-pebble")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use x509_parser::prelude::{GeneralName, parse_x509_pem};

use devbridge_core::proto::CertificateRequest;
use devbridge_server::cert::acme::AcmeCa;
use devbridge_server::cert::dns::DnsProvider;
use devbridge_server::cert::{AcmeSettings, Caller, CertificateAuthority, CertificateIssuer};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set - the ACME (Pebble) CI job starts Pebble and sets it")
    })
}

/// DNS-01 records in pebble-challtestsrv.
struct ChallTestSrv {
    http: reqwest::Client,
    base: String,
    set: Mutex<Vec<String>>,
    cleared: Mutex<Vec<String>>,
}

#[async_trait]
impl DnsProvider for ChallTestSrv {
    async fn set_txt(&self, name: &str, value: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .post(format!("{}/set-txt", self.base))
            .body(json!({"host": format!("{name}."), "value": value}).to_string())
            .send()
            .await?;
        anyhow::ensure!(resp.status().is_success(), "set-txt: {}", resp.status());
        self.set.lock().unwrap().push(name.to_string());
        Ok(name.to_string())
    }

    async fn remove_txt(&self, id: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!("{}/clear-txt", self.base))
            .body(json!({"host": format!("{id}.")}).to_string())
            .send()
            .await?;
        anyhow::ensure!(resp.status().is_success(), "clear-txt: {}", resp.status());
        self.cleared.lock().unwrap().push(id.to_string());
        Ok(())
    }
}

/// (key, CSR DER) of a store for `host`.
fn store_csr(host: &str) -> (rcgen::KeyPair, Vec<u8>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key).unwrap();
    let der: &[u8] = csr.der();
    (key, der.to_vec())
}

/// The leaf of a PEM chain: its DNS names and raw public key.
fn leaf(chain: &str) -> (Vec<String>, Vec<u8>) {
    let (_, pem) = parse_x509_pem(chain.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let names = cert
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .filter_map(|n| match n {
            GeneralName::DNSName(d) => Some(d.to_string()),
            _ => None,
        })
        .collect();
    (names, cert.public_key().subject_public_key.data.to_vec())
}

#[tokio::test]
async fn test_acme_dns01_order_against_pebble() {
    let dir = tempfile::tempdir().unwrap();
    let dns = Arc::new(ChallTestSrv {
        http: reqwest::Client::new(),
        base: env("DEVBRIDGE_CHALLTESTSRV")
            .trim_end_matches('/')
            .to_string(),
        set: Mutex::default(),
        cleared: Mutex::default(),
    });
    let settings = AcmeSettings {
        directory_url: env("DEVBRIDGE_PEBBLE_DIRECTORY"),
        contact: Some("mailto:ci@example.test".into()),
        ca_file: Some(env("DEVBRIDGE_PEBBLE_ROOT").into()),
        account_file: dir.path().join("acme-account.json"),
        propagation: Duration::ZERO,
    };
    let ca: Arc<dyn CertificateAuthority> = Arc::new(AcmeCa::new(
        settings.clone(),
        Arc::clone(&dns) as Arc<dyn DnsProvider>,
    ));
    let issuer = CertificateIssuer::new("pos.example.test", Arc::clone(&ca));

    let host = "pjsnvs.pos.example.test";
    let (key, csr_der) = store_csr(host);
    let reply = issuer
        .answer(
            &CertificateRequest {
                client_id: "pjsnvs".into(),
                hostname: host.into(),
                csr_der,
            },
            Caller {
                approved: true,
                on_its_stream: true,
            },
        )
        .await;
    assert!(reply.ok, "{reply:?}");
    let (names, public_key) = leaf(&reply.chain_pem);
    assert_eq!(names, vec![host.to_string()]);
    assert_eq!(
        public_key,
        key.public_key_raw(),
        "issued for the store's own key"
    );
    assert!(
        reply.chain_pem.matches("BEGIN CERTIFICATE").count() >= 2,
        "leaf + intermediate"
    );
    let record = format!("_acme-challenge.{host}");
    assert_eq!(*dns.set.lock().unwrap(), vec![record.clone()]);
    assert_eq!(
        *dns.cleared.lock().unwrap(),
        vec![record],
        "the TXT record is removed"
    );
    assert!(settings.account_file.exists(), "the ACME account is saved");

    // a second process restores the saved account and issues again
    let restored = AcmeCa::new(settings, Arc::clone(&dns) as Arc<dyn DnsProvider>);
    let other = "pjzav.pos.example.test";
    let (key2, csr2) = store_csr(other);
    let chain = restored.issue(other, &csr2).await.expect("second order");
    let (names, public_key) = leaf(&chain);
    assert_eq!(names, vec![other.to_string()]);
    assert_eq!(public_key, key2.public_key_raw());
}
