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

    /// Wait (bounded: a mutant that never asks the CA must fail fast, not
    /// hang the mutation run) until the CA was asked `n` times.
    async fn wait_for_calls(&self, n: usize) {
        for _ in 0..400 {
            if self.calls.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the CA was asked {} times, expected {n}",
            self.calls.load(Ordering::SeqCst)
        );
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

/// An approved client asking over its own job stream's address.
const OK: Caller = Caller {
    approved: true,
    on_its_stream: true,
};
const NOT_APPROVED: Caller = Caller {
    approved: false,
    on_its_stream: true,
};

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
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
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
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
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
        i.answer(&request("pjzav", other, csr_for(&[other])), OK)
            .await
            .ok
    );
    // with no interval a renewal goes through
    let fast = issuer(&ca).with_min_interval(Duration::ZERO);
    assert!(
        fast.answer(&request("pjsnvs", name, csr_for(&[name])), OK)
            .await
            .ok
    );
    assert!(
        fast.answer(&request("pjsnvs", name, csr_for(&[name])), OK)
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
        .answer(&request("pjsnvs", name, csr_for(&[name])), NOT_APPROVED)
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
        let r = i.answer(&request(client, host, csr_for(&[host])), OK).await;
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
        let r = i.answer(&request("pjsnvs", name, csr), OK).await;
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
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
        .await;
    assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_ACME_FAILED));
    assert_eq!(r.error, format!("CA said no for {name}"));
    // not rate-limited: nothing was issued
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
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
            i.answer(&request("pjsnvs", name, csr_for(&[name])), OK)
                .await
        })
    };
    ca.wait_for_calls(1).await;
    let second = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
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
        !dir.path().join(ACCOUNT_FILE).exists(),
        "no network, no account yet"
    );
}

#[test]
fn test_acme_settings_from_config() {
    let dir = std::path::Path::new("/data");
    let s = AcmeSettings::from_config(&acme_config(), dir);
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
    let s = AcmeSettings::from_config(&cfg, dir);
    assert_eq!(s.contact, None);
    assert_eq!(
        s.ca_file,
        Some(std::path::PathBuf::from("/pebble/root.pem"))
    );
    assert_eq!(s.propagation, Duration::ZERO);
}

#[test]
fn test_same_peer() {
    let a: IpAddr = "10.78.2.10".parse().unwrap();
    let b: IpAddr = "10.78.9.10".parse().unwrap();
    assert!(same_peer(Some(a), Some(a)));
    assert!(!same_peer(Some(a), Some(b)), "another PC");
    assert!(!same_peer(None, Some(a)), "no live job stream");
    assert!(!same_peer(Some(a), None), "the request address is unknown");
    assert!(!same_peer(None, None));
}

#[tokio::test]
async fn test_a_request_off_the_clients_stream_is_refused() {
    let ca = FakeCa::new(false, false);
    let i = issuer(&ca);
    let name = "pjsnvs.pos.example.sk";
    let off_stream = Caller {
        approved: true,
        on_its_stream: false,
    };
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), off_stream)
        .await;
    assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_NOT_CONNECTED));
    assert!(
        r.error
            .contains("'pjsnvs' has no job stream from this address"),
        "{}",
        r.error
    );
    assert!(r.chain_pem.is_empty());
    assert_eq!(ca.calls.load(Ordering::SeqCst), 0, "nothing was ordered");
    assert_eq!(CODE_NOT_CONNECTED, "not_connected");
    // not approved is reported first
    let r = i
        .answer(
            &request("pjsnvs", name, csr_for(&[name])),
            Caller {
                approved: false,
                on_its_stream: false,
            },
        )
        .await;
    assert_eq!(r.error_code, CODE_NOT_APPROVED);
}

#[tokio::test]
async fn test_a_dropped_request_never_leaves_the_name_busy() {
    let ca = FakeCa::new(false, true);
    let i = Arc::new(issuer(&ca).with_min_interval(Duration::ZERO));
    let name = "pjsnvs.pos.example.sk";
    let first = {
        let i = Arc::clone(&i);
        tokio::spawn(async move {
            i.answer(&request("pjsnvs", name, csr_for(&[name])), OK)
                .await
        })
    };
    ca.wait_for_calls(1).await;
    // the store gives up (its timeout, a VPN drop): the RPC future is dropped
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    // the order itself goes on, so the name is busy until it ends ...
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
        .await;
    assert_eq!(r.error_code, CODE_BUSY);
    // ... and is settled when it does (one permit for it, one for the next)
    ca.gate.add_permits(2);
    let mut answered = None;
    for _ in 0..400 {
        let r = i
            .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
            .await;
        if r.error_code != CODE_BUSY {
            answered = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let r = answered.expect("the name was left busy");
    assert!(r.ok, "{r:?}");
    assert_eq!(ca.calls.load(Ordering::SeqCst), 2);
}

/// A CA whose order panics while `panics` is set (a bug in the ACME client).
struct PanickingCa {
    panics: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl CertificateAuthority for PanickingCa {
    async fn issue(&self, hostname: &str, _csr_der: &[u8]) -> Result<String> {
        if self.panics.load(Ordering::SeqCst) {
            panic!("ACME client bug for {hostname}");
        }
        Ok(format!("CHAIN FOR {hostname}"))
    }
}

#[tokio::test]
async fn test_a_panicking_order_frees_the_name() {
    let ca = Arc::new(PanickingCa {
        panics: std::sync::atomic::AtomicBool::new(true),
    });
    let i = CertificateIssuer::new(ZONE, Arc::clone(&ca) as Arc<dyn CertificateAuthority>);
    let name = "pjsnvs.pos.example.sk";
    for _ in 0..2 {
        let r = i
            .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
            .await;
        assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_ACME_FAILED));
        assert!(
            r.error.starts_with("the certificate order failed:"),
            "{}",
            r.error
        );
    }
    // the name was freed, not left busy: once the CA works it is issued
    ca.panics.store(false, Ordering::SeqCst);
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
        .await;
    assert!(r.ok, "{r:?}");
}

#[tokio::test]
async fn test_a_slow_order_is_answered_busy_in_time_and_goes_on() {
    let ca = FakeCa::new(false, true);
    let i = issuer(&ca)
        .with_min_interval(Duration::ZERO)
        .with_answer_timeout(Duration::from_millis(100));
    let name = "pjsnvs.pos.example.sk";
    let started = std::time::Instant::now();
    let r = i
        .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered in time"
    );
    assert_eq!((r.ok, r.error_code.as_str()), (false, CODE_BUSY));
    assert!(
        r.error.contains("is still running after 0 s"),
        "{}",
        r.error
    );
    assert_eq!(ca.calls.load(Ordering::SeqCst), 1);
    assert_eq!(ANSWER_TIMEOUT, Duration::from_secs(270));
    // the order goes on and settles the name when it ends
    ca.gate.add_permits(2);
    let mut answered = None;
    for _ in 0..400 {
        let r = i
            .answer(&request("pjsnvs", name, csr_for(&[name])), OK)
            .await;
        if r.error_code != CODE_BUSY {
            answered = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(answered.expect("the name was left busy").ok);
    assert_eq!(ca.calls.load(Ordering::SeqCst), 2);
}
