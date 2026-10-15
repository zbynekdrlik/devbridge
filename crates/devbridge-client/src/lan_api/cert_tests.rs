//! Unit tests of the LAN API certificate handling (#101).

use super::*;
use rcgen::{BasicConstraints, IsCa, Issuer, date_time_ymd};

const HOST: &str = "pjsnvs.pos.example.sk";

/// 2030-01-01T00:00:00Z
const Y2030: i64 = 1_893_456_000;

/// A test CA (key + params).
fn test_ca() -> (CertificateParams, KeyPair) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "devbridge test CA");
    (params, key)
}

/// A leaf for `names` and `key`, signed by a fresh test CA, expiring at
/// 2030-01-01. Returns the PEM chain (leaf + CA).
fn leaf_chain(names: &[&str], key: &KeyPair) -> String {
    let (ca_params, ca_key) = test_ca();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let mut params =
        CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
    params.not_after = date_time_ymd(2030, 1, 1);
    let leaf = params.signed_by(key, &issuer).unwrap();
    format!("{}{}", leaf.pem(), ca_cert.pem())
}

#[test]
fn test_cert_paths() {
    let p = CertPaths::in_data_dir(Path::new("/data"));
    assert_eq!(p.dir, Path::new("/data/lan-api"));
    assert_eq!(p.cert, Path::new("/data/lan-api/cert.pem"));
    assert_eq!(p.key, Path::new("/data/lan-api/key.pem"));
}

#[test]
fn test_leaf_of_reads_the_first_certificate() {
    let key = KeyPair::generate().unwrap();
    let chain = leaf_chain(&[HOST, "alt.pos.example.sk"], &key);
    let leaf = leaf_of(chain.as_bytes()).unwrap();
    assert_eq!(leaf.not_after, Y2030);
    assert_eq!(
        leaf.dns_names,
        vec![HOST.to_string(), "alt.pos.example.sk".to_string()]
    );
    assert_eq!(leaf.public_key, key.public_key_raw());
    // a certificate without names
    let (ca_params, ca_key) = test_ca();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    assert!(leaf_of(ca.pem().as_bytes()).unwrap().dns_names.is_empty());
}

#[test]
fn test_leaf_of_errors() {
    assert!(leaf_of(b"not pem").is_err());
    let key = KeyPair::generate().unwrap();
    let err = leaf_of(key.serialize_pem().as_bytes())
        .unwrap_err()
        .to_string();
    assert!(err.contains("PRIVATE KEY (expected CERTIFICATE)"), "{err}");
    let broken = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    let err = leaf_of(broken.as_bytes()).unwrap_err().to_string();
    assert!(err.contains("unreadable certificate"), "{err}");
}

#[test]
fn test_status_of_pems() {
    let key = KeyPair::generate().unwrap();
    let chain = leaf_chain(&[HOST], &key);
    let key_pem = key.serialize_pem();
    assert_eq!(
        status_of_pems(Some(chain.as_bytes()), Some(key_pem.as_bytes()), HOST),
        CertStatus::Valid { not_after: Y2030 }
    );
    assert_eq!(
        status_of_pems(None, Some(key_pem.as_bytes()), HOST),
        CertStatus::Missing
    );
    assert_eq!(
        status_of_pems(Some(chain.as_bytes()), None, HOST),
        CertStatus::Missing
    );
    assert_eq!(
        status_of_pems(
            Some(chain.as_bytes()),
            Some(key_pem.as_bytes()),
            "other.pos.example.sk"
        ),
        CertStatus::WrongName
    );
    let other_key = KeyPair::generate().unwrap().serialize_pem();
    match status_of_pems(Some(chain.as_bytes()), Some(other_key.as_bytes()), HOST) {
        CertStatus::Unusable(why) => assert!(why.contains("do not belong together"), "{why}"),
        other => panic!("{other:?}"),
    }
    match status_of_pems(Some(&b"garbage"[..]), Some(key_pem.as_bytes()), HOST) {
        CertStatus::Unusable(why) => assert!(why.contains("no PEM certificate"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_renewal_due_at_30_days_left() {
    let month = RENEW_BEFORE.as_secs() as i64;
    assert_eq!(month, 30 * 24 * 3600);
    let valid = CertStatus::Valid { not_after: Y2030 };
    assert!(!renewal_due(&valid, Y2030 - month - 1));
    assert!(renewal_due(&valid, Y2030 - month), "due at exactly 30 days");
    assert!(renewal_due(&valid, Y2030 + 1), "expired");
    for status in [
        CertStatus::Missing,
        CertStatus::WrongName,
        CertStatus::Unusable("x".into()),
    ] {
        assert!(renewal_due(&status, 0), "{status:?}");
    }
    assert_eq!(CHECK_INTERVAL, Duration::from_secs(12 * 3600));
    assert_eq!(
        (RETRY_BASE, RETRY_MAX),
        (Duration::from_secs(300), Duration::from_secs(21_600))
    );
}

#[test]
fn test_cert_status_display() {
    assert_eq!(CertStatus::Missing.to_string(), "missing");
    assert_eq!(CertStatus::Unusable("x".into()).to_string(), "unusable: x");
    assert_eq!(CertStatus::WrongName.to_string(), "names another host");
    assert_eq!(
        CertStatus::Valid { not_after: 5 }.to_string(),
        "valid until 5 (unix)"
    );
}

#[test]
fn test_unix_now() {
    assert!(unix_now() > 1_760_000_000, "{}", unix_now());
}

#[test]
fn test_new_key_and_csr_names_exactly_the_host() {
    use x509_parser::prelude::{FromDer, ParsedExtension, X509CertificationRequest};
    let fresh = new_key_and_csr(HOST).unwrap();
    let (_, csr) = X509CertificationRequest::from_der(&fresh.csr_der).unwrap();
    let names: Vec<String> = csr
        .requested_extensions()
        .into_iter()
        .flatten()
        .filter_map(|ext| match ext {
            ParsedExtension::SubjectAlternativeName(san) => Some(
                san.general_names
                    .iter()
                    .map(|n| format!("{n:?}"))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(names, vec![format!("DNSName({HOST:?})")]);
    assert_eq!(
        csr.certification_request_info
            .subject_pki
            .subject_public_key
            .data
            .to_vec(),
        fresh.public_key
    );
    let key = KeyPair::from_pem(&fresh.key_pem).unwrap();
    assert_eq!(key.public_key_raw(), fresh.public_key.as_slice());
    assert!(
        new_key_and_csr("pj\u{161}nvs.pos.example.sk").is_err(),
        "not IA5"
    );
}

#[test]
fn test_check_issued() {
    let fresh = new_key_and_csr(HOST).unwrap();
    let key = KeyPair::from_pem(&fresh.key_pem).unwrap();
    let chain = leaf_chain(&[HOST], &key);
    let leaf = check_issued(&chain, HOST, &fresh.public_key, Y2030 - 1).unwrap();
    assert_eq!(leaf.not_after, Y2030);
    let err = check_issued(&chain, "other.pos.example.sk", &fresh.public_key, 0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not name other.pos.example.sk"), "{err}");
    let other = KeyPair::generate().unwrap();
    let err = check_issued(&chain, HOST, other.public_key_raw(), 0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not for this PC's new key"), "{err}");
    let err = check_issued(&chain, HOST, &fresh.public_key, Y2030)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already expired"), "{err}");
    assert!(check_issued("garbage", HOST, &fresh.public_key, 0).is_err());
}

#[test]
fn test_reply_chain() {
    let reply = |ok: bool, chain: &str| CertificateReply {
        ok,
        chain_pem: chain.into(),
        error_code: if ok {
            String::new()
        } else {
            "acme_disabled".into()
        },
        error: if ok {
            String::new()
        } else {
            "no acme here".into()
        },
    };
    assert_eq!(reply_chain(reply(true, "CHAIN")).unwrap(), "CHAIN");
    assert!(reply_chain(reply(true, " \n")).is_err(), "an empty chain");
    let err = reply_chain(reply(false, "")).unwrap_err().to_string();
    assert_eq!(
        err,
        "pz-server did not issue the certificate (acme_disabled): no acme here"
    );
}

#[test]
fn test_store_installs_a_usable_pair() {
    let dir = tempfile::tempdir().unwrap();
    let paths = CertPaths::in_data_dir(dir.path());
    assert_eq!(status_of(&paths, HOST), CertStatus::Missing);
    let fresh = new_key_and_csr(HOST).unwrap();
    let key = KeyPair::from_pem(&fresh.key_pem).unwrap();
    let chain = leaf_chain(&[HOST], &key);
    store(&paths, &fresh.key_pem, &chain).unwrap();
    assert_eq!(
        status_of(&paths, HOST),
        CertStatus::Valid { not_after: Y2030 }
    );
    assert_eq!(std::fs::read_to_string(&paths.cert).unwrap(), chain);
    assert_eq!(std::fs::read_to_string(&paths.key).unwrap(), fresh.key_pem);
    assert!(!paths.key.with_extension("pem.new").exists());
    assert!(!paths.cert.with_extension("pem.new").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&paths.key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the key is owner-only");
    }
    // a second store replaces the pair
    let second = new_key_and_csr(HOST).unwrap();
    let second_key = KeyPair::from_pem(&second.key_pem).unwrap();
    let second_chain = leaf_chain(&[HOST], &second_key);
    store(&paths, &second.key_pem, &second_chain).unwrap();
    assert_eq!(std::fs::read_to_string(&paths.key).unwrap(), second.key_pem);
    assert!(matches!(status_of(&paths, HOST), CertStatus::Valid { .. }));
    // a key installed without its certificate (a crash between the renames)
    std::fs::write(&paths.key, fresh.key_pem.as_bytes()).unwrap();
    assert!(matches!(status_of(&paths, HOST), CertStatus::Unusable(_)));
}

/// A new key and a chain issued for it.
fn issued_pair() -> (String, String) {
    let fresh = new_key_and_csr(HOST).unwrap();
    let key = KeyPair::from_pem(&fresh.key_pem).unwrap();
    let chain = leaf_chain(&[HOST], &key);
    (fresh.key_pem, chain)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn test_finish_pending_rolls_an_interrupted_store_forward() {
    let dir = tempfile::tempdir().unwrap();
    let paths = CertPaths::in_data_dir(dir.path());
    assert_eq!(
        pending(&paths.cert),
        dir.path().join("lan-api/cert.pem.new")
    );
    assert_eq!(pending(&paths.key), dir.path().join("lan-api/key.pem.new"));
    assert!(
        !finish_pending(&paths).unwrap(),
        "no directory, nothing pending"
    );
    let (old_key, old_chain) = issued_pair();
    store(&paths, &old_key, &old_chain).unwrap();
    assert!(!finish_pending(&paths).unwrap(), "nothing pending");
    assert_eq!(read(&paths.cert), old_chain);

    // stopped between the renames: the new key is in, its certificate pending
    let (new_key, new_chain) = issued_pair();
    std::fs::write(&paths.key, &new_key).unwrap();
    std::fs::write(pending(&paths.cert), &new_chain).unwrap();
    assert!(matches!(status_of(&paths, HOST), CertStatus::Unusable(_)));
    assert!(finish_pending(&paths).unwrap());
    assert_eq!(read(&paths.cert), new_chain);
    assert!(matches!(status_of(&paths, HOST), CertStatus::Valid { .. }));
    assert!(!pending(&paths.cert).exists());

    // stopped before the key's rename: both pending, both installed
    let (third_key, third_chain) = issued_pair();
    std::fs::write(pending(&paths.key), &third_key).unwrap();
    std::fs::write(pending(&paths.cert), &third_chain).unwrap();
    assert!(finish_pending(&paths).unwrap());
    assert_eq!(read(&paths.key), third_key);
    assert_eq!(read(&paths.cert), third_chain);
    assert!(!pending(&paths.key).exists() && !pending(&paths.cert).exists());

    // a pending certificate of another key is stale: removed, pair kept
    std::fs::write(pending(&paths.cert), &old_chain).unwrap();
    assert!(!finish_pending(&paths).unwrap());
    assert!(!pending(&paths.cert).exists());
    assert_eq!(read(&paths.cert), third_chain);

    // a lone pending key (stopped before the certificate was written)
    std::fs::write(pending(&paths.key), &old_key).unwrap();
    assert!(!finish_pending(&paths).unwrap());
    assert!(!pending(&paths.key).exists());
    assert_eq!(read(&paths.key), third_key);

    // a pending pair that does not belong together: both removed
    std::fs::write(pending(&paths.key), &old_key).unwrap();
    std::fs::write(pending(&paths.cert), &new_chain).unwrap();
    assert!(!finish_pending(&paths).unwrap());
    assert!(!pending(&paths.key).exists() && !pending(&paths.cert).exists());
    assert!(matches!(status_of(&paths, HOST), CertStatus::Valid { .. }));
}

#[test]
fn test_a_failed_store_keeps_the_old_pair_and_no_pending_key() {
    let dir = tempfile::tempdir().unwrap();
    let paths = CertPaths::in_data_dir(dir.path());
    let (old_key, old_chain) = issued_pair();
    store(&paths, &old_key, &old_chain).unwrap();
    // the certificate cannot be written (a directory is in its way)
    std::fs::create_dir(pending(&paths.cert)).unwrap();
    let (new_key, new_chain) = issued_pair();
    let err = format!("{:#}", store(&paths, &new_key, &new_chain).unwrap_err());
    assert!(err.contains("cert.pem.new"), "{err}");
    assert_eq!(read(&paths.key), old_key);
    assert_eq!(read(&paths.cert), old_chain);
    assert!(!pending(&paths.key).exists(), "no half-installed key left");
    assert!(matches!(status_of(&paths, HOST), CertStatus::Valid { .. }));
}

#[test]
fn test_retry_after_a_failed_renewal() {
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_MAX);
    let not_issued = RenewFailure::NotIssued(anyhow!("pz-server unreachable"));
    assert_eq!(not_issued.retry_after(&mut backoff), RETRY_BASE);
    assert_eq!(not_issued.retry_after(&mut backoff), RETRY_BASE * 2);
    let rejected = RenewFailure::Rejected(anyhow!("not for this key"));
    assert_eq!(
        rejected.retry_after(&mut backoff),
        REJECTED_RETRY,
        "an issued but unusable certificate is not re-ordered soon"
    );
    assert_eq!(REJECTED_RETRY, Duration::from_secs(48 * 3600));
    assert_eq!(not_issued.to_string(), "not issued: pz-server unreachable");
    assert_eq!(
        rejected.to_string(),
        "issued but not usable here: not for this key"
    );
}
