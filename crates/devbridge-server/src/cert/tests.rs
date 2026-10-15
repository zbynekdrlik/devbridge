//! Unit tests of the certificate checks (#101) with a fake CA.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use devbridge_core::config::CloudflareConfig;

/// A CA that records what it was asked and answers a fixed chain (or fails).
struct FakeCa {
    calls: AtomicUsize,
    fail: bool,
    /// Holds the order open until the test releases it.
    gate: tokio::sync::Semaphore,
}

impl FakeCa {
    fn new(fail: bool, gated: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail,
            gate: tokio::sync::Semaphore::new(if gated { 0 } else { 1_000 }),
        })
    }
}

#[async_trait]
impl CertificateAuthority for FakeCa {
    async fn issue(&self, hostname: &str, csr_der: &[u8]) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        assert!(!csr_der.is_empty());
        if self.fail {
            anyhow::bail!("CA said no for {hostname}");
        }
        Ok(format!("CHAIN FOR {hostname}"))
    }
}

fn csr_for(names: &[&str]) -> Vec<u8> {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params =
        rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
            .unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key).unwrap();
    let der: &[u8] = csr.der();
    der.to_vec()
}

fn request(client_id: &str, hostname: &str, csr_der: Vec<u8>) -> CertificateRequest {
    CertificateRequest {
        client_id: client_id.into(),
        hostname: hostname.into(),
        csr_der,
    }
}

const ZONE: &str = "pos.example.sk";

fn issuer(ca: &Arc<FakeCa>) -> CertificateIssuer {
    CertificateIssuer::new(ZONE, Arc::clone(ca) as Arc<dyn CertificateAuthority>)
}

#[test]
fn test_name_for_is_the_client_id_in_the_zone() {
    let i = issuer(&FakeCa::new(false, false));
    assert_eq!(i.zone(), ZONE);
    assert_eq!(
        i.name_for("pjsnvs").as_deref(),
        Some("pjsnvs.pos.example.sk")
    );
    assert_eq!(
        i.name_for("E2E-Raw-Client").as_deref(),
        Some("e2e-raw-client.pos.example.sk"),
        "lower-cased"
    );
    for bad in ["", "a.b", "-x", "x_y", "*"] {
        assert_eq!(i.name_for(bad), None, "{bad}");
    }
}

#[test]
fn test_csr_dns_names() {
    assert_eq!(
        csr_dns_names(&csr_for(&["a.pos.example.sk"])).unwrap(),
        vec!["a.pos.example.sk".to_string()]
    );
    assert_eq!(
        csr_dns_names(&csr_for(&["a.x", "b.x"])).unwrap(),
        vec!["a.x".to_string(), "b.x".to_string()]
    );
    assert!(csr_dns_names(&csr_for(&[])).unwrap().is_empty());
    let ip = csr_dns_names(&csr_for(&["10.78.2.10"]))
        .unwrap_err()
        .to_string();
    assert!(ip.contains("non-DNS name"), "{ip}");
    let garbage = csr_dns_names(b"not a csr").unwrap_err().to_string();
    assert!(garbage.contains("unreadable CSR"), "{garbage}");
}

#[test]
fn test_slot_free() {
    let t0 = Instant::now();
    let hour = Duration::from_secs(3600);
    assert_eq!(slot_free(None, t0, hour), Ok(()));
    assert_eq!(slot_free(Some(Slot::Busy), t0, hour), Err(CODE_BUSY));
    assert_eq!(
        slot_free(Some(Slot::Issued(t0)), t0 + Duration::from_secs(10), hour),
        Err(CODE_RATE_LIMITED)
    );
    assert_eq!(
        slot_free(Some(Slot::Issued(t0)), t0 + hour, hour),
        Ok(()),
        "due exactly at the interval"
    );
    assert_eq!(
        slot_free(Some(Slot::Issued(t0 + hour)), t0, hour),
        Err(CODE_RATE_LIMITED),
        "a clock running backwards never frees it early"
    );
    assert_eq!(MIN_REISSUE_INTERVAL, hour);
}

#[tokio::test]
async fn test_an_approved_client_gets_its_own_name() {
    let ca = FakeCa::new(false, false);
    let i = issuer(&ca);
    let name = "pjsnvs.pos.example.sk";
    let reply = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), true)
        .await;
    assert_eq!(
        reply,
        CertificateReply {
            ok: true,
            chain_pem: format!("CHAIN FOR {name}"),
            error_code: String::new(),
            error: String::new(),
        }
    );
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
    // the next order for that name within the hour is refused, the CA is not asked
    let again = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), true)
        .await;
    assert!(!again.ok);
    assert_eq!(again.error_code, CODE_RATE_LIMITED);
    assert!(
        again.error.contains("less than 3600 s ago"),
        "{}",
        again.error
    );
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
    // another store is independent
    let other = "pjzav.pos.example.sk";
    assert!(
        i.answer(&request("pjzav", other, csr_for(&[other])), true)
            .await
            .ok
    );
    // with no interval a renewal goes through
    let fast = issuer(&ca).with_min_interval(Duration::ZERO);
    assert!(
        fast.answer(&request("pjsnvs", name, csr_for(&[name])), true)
            .await
            .ok
    );
    assert!(
        fast.answer(&request("pjsnvs", name, csr_for(&[name])), true)
            .await
            .ok
    );
}

#[tokio::test]
async fn test_refusals_never_reach_the_ca() {
    let ca = FakeCa::new(false, false);
    let i = issuer(&ca);
    let name = "pjsnvs.pos.example.sk";
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), false)
        .await;
    assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_NOT_APPROVED));
    assert!(r.chain_pem.is_empty());
    assert!(r.error.contains("'pjsnvs' is not approved"), "{}", r.error);

    for (client, host) in [
        ("pjsnvs", "pjzav.pos.example.sk"),
        ("pjsnvs", "pjsnvs.other.sk"),
        ("pjsnvs", "PJSNVS.pos.example.sk"),
        ("a.b", "a.b.pos.example.sk"),
    ] {
        let r = i
            .answer(&request(client, host, csr_for(&[host])), true)
            .await;
        assert_eq!(
            (r.ok, r.error_code.as_str()),
            (false, CODE_BAD_NAME),
            "{client} {host}"
        );
        assert!(
            r.error.contains("<client_id>.pos.example.sk"),
            "{}",
            r.error
        );
    }
    for csr in [
        csr_for(&["pjzav.pos.example.sk"]),
        csr_for(&[name, "pjzav.pos.example.sk"]),
        csr_for(&[]),
        csr_for(&["10.78.2.10"]),
        b"garbage".to_vec(),
    ] {
        let r = i.answer(&request("pjsnvs", name, csr), true).await;
        assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_BAD_CSR));
    }
    assert_eq!(ca.calls.load(Ordering::SeqCst), 0, "nothing was ordered");
}

#[tokio::test]
async fn test_a_failed_order_frees_the_name() {
    let ca = FakeCa::new(true, false);
    let i = issuer(&ca);
    let name = "pjsnvs.pos.example.sk";
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), true)
        .await;
    assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_ACME_FAILED));
    assert_eq!(r.error, format!("CA said no for {name}"));
    // not rate-limited: nothing was issued
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), true)
        .await;
    assert_eq!(r.error_code, CODE_ACME_FAILED);
    assert_eq!(ca.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_one_order_per_name_at_a_time() {
    let ca = FakeCa::new(false, true);
    let i = Arc::new(issuer(&ca));
    let name = "pjsnvs.pos.example.sk";
    let first = {
        let i = Arc::clone(&i);
        tokio::spawn(async move {
            i.answer(&request("pjsnvs", name, csr_for(&[name])), true)
                .await
        })
    };
    while ca.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), true)
        .await;
    assert_eq!((second.ok, second.error_code.as_str()), (false, CODE_BUSY));
    ca.gate.add_permits(1);
    assert!(first.await.unwrap().ok);
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_disabled_reply_and_refusal() {
    let d = disabled_reply();
    assert!(!d.ok && d.chain_pem.is_empty());
    assert_eq!(d.error_code, CODE_ACME_DISABLED);
    assert!(
        d.error.contains("[server.acme] is not enabled"),
        "{}",
        d.error
    );
    let r = refusal("x", "y");
    assert_eq!(
        r,
        CertificateReply {
            ok: false,
            chain_pem: String::new(),
            error_code: "x".into(),
            error: "y".into()
        }
    );
}

fn acme_config() -> AcmeServerConfig {
    AcmeServerConfig {
        enabled: true,
        zone: ZONE.into(),
        contact_email: " ops@example.sk ".into(),
        cloudflare: CloudflareConfig {
            zone_id: "zone123".into(),
            api_token: "cf-token".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn test_issuer_from_config() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        issuer_from_config(&AcmeServerConfig::default(), dir.path(), Some("t"))
            .unwrap()
            .is_none(),
        "disabled"
    );
    let issuer = issuer_from_config(&acme_config(), dir.path(), None)
        .unwrap()
        .expect("enabled");
    assert_eq!(issuer.zone(), ZONE);
    let mut no_token = acme_config();
    no_token.cloudflare.api_token = String::new();
    let err = issuer_from_config(&no_token, dir.path(), None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("[server.acme] is enabled but invalid"),
        "{err}"
    );
    assert!(err.contains("no Cloudflare API token"), "{err}");
    assert!(
        issuer_from_config(&no_token, dir.path(), Some("env-token"))
            .unwrap()
            .is_some(),
        "the env token is enough"
    );
    assert!(
        !dir.path().join(acme::ACCOUNT_FILE).exists(),
        "no network, no account yet"
    );
}

#[test]
fn test_acme_settings_from_config() {
    let dir = std::path::Path::new("/data");
    let s = acme::AcmeSettings::from_config(&acme_config(), dir);
    assert_eq!(
        s.directory_url,
        devbridge_core::config::LETS_ENCRYPT_DIRECTORY
    );
    assert_eq!(s.contact.as_deref(), Some("mailto:ops@example.sk"));
    assert_eq!(s.ca_file, None);
    assert_eq!(s.account_file, dir.join("acme-account.json"));
    assert_eq!(s.propagation, Duration::from_secs(30));
    let mut cfg = acme_config();
    cfg.contact_email = " ".into();
    cfg.ca_file = "/pebble/root.pem".into();
    cfg.propagation_secs = 0;
    let s = acme::AcmeSettings::from_config(&cfg, dir);
    assert_eq!(s.contact, None);
    assert_eq!(
        s.ca_file,
        Some(std::path::PathBuf::from("/pebble/root.pem"))
    );
    assert_eq!(s.propagation, Duration::ZERO);
}
