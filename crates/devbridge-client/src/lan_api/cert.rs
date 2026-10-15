//! The LAN API's certificate on the store PC (#101).
//!
//! The listener serves `<data_dir>/lan-api/cert.pem` (chain, leaf first) and
//! `key.pem`. With `[client.pokladna.lan_api] acme = true` this client keeps
//! them valid itself: it makes a NEW private key and a CSR for its
//! `hostname` HERE (the key never leaves the PC), sends the CSR to pz-server
//! over the existing gRPC channel (`RequestCertificate`), and pz-server runs
//! the Let's Encrypt DNS-01 order with the DNS token only it holds. The
//! answer is checked (the name, THIS key, not expired) before it replaces
//! the pair; the listener then reloads it.
//!
//! Renewal starts when 30 days are left (checked every 12 hours, and at
//! start); a failed request is retried with backoff (5 min → 6 h). The
//! certificate stays valid offline until it expires (90 days).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use tokio::sync::Notify;
use tonic::transport::Channel;
use tracing::{info, warn};
use x509_parser::prelude::{GeneralName, parse_x509_pem};

use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::{CertificateReply, CertificateRequest};

use crate::device_jobs::Backoff;

/// Renew when the certificate has this long left (or less).
pub const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);
/// How often the certificate is checked.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 3600);
/// First retry after a failed request; doubles up to [`RETRY_MAX`].
pub const RETRY_BASE: Duration = Duration::from_secs(5 * 60);
pub const RETRY_MAX: Duration = Duration::from_secs(6 * 3600);
/// The whole `RequestCertificate` call (pz-server waits for DNS propagation
/// and the CA's validation).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Where the listener's certificate and key live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertPaths {
    pub dir: PathBuf,
    /// PEM chain, leaf first.
    pub cert: PathBuf,
    /// PEM private key (SYSTEM + Administrators only on Windows).
    pub key: PathBuf,
}

impl CertPaths {
    /// `<data_dir>/lan-api/{cert.pem,key.pem}`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let dir = data_dir.join("lan-api");
        Self {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
            dir,
        }
    }
}

/// What the leaf certificate of a chain says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// Expiry, Unix seconds.
    pub not_after: i64,
    pub dns_names: Vec<String>,
    /// The raw public key (the subjectPublicKey bits).
    pub public_key: Vec<u8>,
}

/// Read the FIRST certificate of a PEM chain.
pub fn leaf_of(chain_pem: &[u8]) -> Result<Leaf> {
    let (_, pem) = parse_x509_pem(chain_pem).map_err(|e| anyhow!("no PEM certificate ({e:?})"))?;
    ensure!(
        pem.label == "CERTIFICATE",
        "the first PEM block is a {} (expected CERTIFICATE)",
        pem.label
    );
    let cert = pem
        .parse_x509()
        .map_err(|e| anyhow!("unreadable certificate ({e:?})"))?;
    let dns_names = match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|name| match name {
                GeneralName::DNSName(dns) => Some(dns.to_string()),
                _ => None,
            })
            .collect(),
        Ok(None) => Vec::new(),
        Err(e) => bail!("unreadable subjectAltName ({e})"),
    };
    Ok(Leaf {
        not_after: cert.validity().not_after.timestamp(),
        dns_names,
        public_key: cert.public_key().subject_public_key.data.to_vec(),
    })
}

/// The state of the pair on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertStatus {
    /// No certificate or no key yet.
    Missing,
    /// The pair cannot be served (unreadable, key and certificate differ …).
    Unusable(String),
    /// The certificate does not name the configured hostname.
    WrongName,
    /// Usable; expires at `not_after` (Unix seconds).
    Valid { not_after: i64 },
}

impl std::fmt::Display for CertStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Unusable(why) => write!(f, "unusable: {why}"),
            Self::WrongName => f.write_str("names another host"),
            Self::Valid { not_after } => write!(f, "valid until {not_after} (unix)"),
        }
    }
}

/// Judge a certificate chain + key (either absent = [`CertStatus::Missing`]).
pub fn status_of_pems(chain: Option<&[u8]>, key: Option<&[u8]>, hostname: &str) -> CertStatus {
    let (Some(chain), Some(key)) = (chain, key) else {
        return CertStatus::Missing;
    };
    let leaf = match leaf_of(chain) {
        Ok(leaf) => leaf,
        Err(e) => return CertStatus::Unusable(format!("{e:#}")),
    };
    if let Err(e) = super::tls_config(chain, key) {
        return CertStatus::Unusable(format!("{e:#}"));
    }
    if !leaf.dns_names.iter().any(|n| n == hostname) {
        return CertStatus::WrongName;
    }
    CertStatus::Valid {
        not_after: leaf.not_after,
    }
}

/// [`status_of_pems`] of the files.
pub fn status_of(paths: &CertPaths, hostname: &str) -> CertStatus {
    status_of_pems(
        std::fs::read(&paths.cert).ok().as_deref(),
        std::fs::read(&paths.key).ok().as_deref(),
        hostname,
    )
}

/// A new certificate is needed: none, unusable, another name, or at most
/// [`RENEW_BEFORE`] left at `now` (Unix seconds).
pub fn renewal_due(status: &CertStatus, now: i64) -> bool {
    match status {
        CertStatus::Valid { not_after } => not_after - now <= RENEW_BEFORE.as_secs() as i64,
        _ => true,
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A fresh private key and the CSR for `hostname`.
pub struct KeyAndCsr {
    pub key_pem: String,
    pub csr_der: Vec<u8>,
    /// The raw public key, to recognise the issued certificate.
    pub public_key: Vec<u8>,
}

/// Generate a new ECDSA P-256 key and its CSR naming exactly `hostname`.
pub fn new_key_and_csr(hostname: &str) -> Result<KeyAndCsr> {
    let key = KeyPair::generate().context("generate the LAN API key")?;
    let mut params =
        CertificateParams::new(vec![hostname.to_string()]).context("certificate name")?;
    params.distinguished_name = DistinguishedName::new();
    let csr = params
        .serialize_request(&key)
        .context("build the certificate request")?;
    let csr_der: &[u8] = csr.der();
    Ok(KeyAndCsr {
        key_pem: key.serialize_pem(),
        csr_der: csr_der.to_vec(),
        public_key: key.public_key_raw().to_vec(),
    })
}

/// Accept an issued chain only when its leaf names `hostname`, is for
/// `public_key` (this PC's new key) and is not expired at `now`.
pub fn check_issued(chain_pem: &str, hostname: &str, public_key: &[u8], now: i64) -> Result<Leaf> {
    let leaf = leaf_of(chain_pem.as_bytes())?;
    ensure!(
        leaf.dns_names.iter().any(|n| n == hostname),
        "the issued certificate does not name {hostname} (it names {:?})",
        leaf.dns_names
    );
    ensure!(
        leaf.public_key == public_key,
        "the issued certificate is not for this PC's new key"
    );
    ensure!(
        leaf.not_after > now,
        "the issued certificate is already expired"
    );
    Ok(leaf)
}

/// pz-server's answer: the chain, or why it refused.
pub fn reply_chain(reply: CertificateReply) -> Result<String> {
    if reply.ok && !reply.chain_pem.trim().is_empty() {
        return Ok(reply.chain_pem);
    }
    bail!(
        "pz-server did not issue the certificate ({}): {}",
        reply.error_code,
        reply.error
    )
}

/// Restrict the private key file (SYSTEM + Administrators on Windows,
/// owner-only elsewhere).
#[cfg(windows)]
#[path = "key_acl_windows.rs"]
mod key_acl;

#[cfg(not(windows))]
mod key_acl {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use anyhow::{Context, Result};

    pub fn restrict(path: &Path) -> Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {}", path.display()))
    }
}

/// Replace the pair: both are written next to their place, the key
/// restricted, then renamed in (key first). A crash in between leaves a
/// pair that does not match — [`status_of`] calls it unusable and a new one
/// is requested.
pub fn store(paths: &CertPaths, key_pem: &str, chain_pem: &str) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create {}", paths.dir.display()))?;
    let key_new = paths.key.with_extension("pem.new");
    let cert_new = paths.cert.with_extension("pem.new");
    std::fs::write(&key_new, key_pem).with_context(|| format!("write {}", key_new.display()))?;
    key_acl::restrict(&key_new)?;
    std::fs::write(&cert_new, chain_pem)
        .with_context(|| format!("write {}", cert_new.display()))?;
    std::fs::rename(&key_new, &paths.key)
        .with_context(|| format!("install {}", paths.key.display()))?;
    std::fs::rename(&cert_new, &paths.cert)
        .with_context(|| format!("install {}", paths.cert.display()))?;
    Ok(())
}

/// Where the certificate comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertSource {
    /// pz-server's gRPC address (`[client] server_address`).
    pub server_address: String,
    pub client_id: String,
    pub hostname: String,
}

async fn request_chain(source: &CertSource, csr_der: &[u8]) -> Result<String> {
    let endpoint = Channel::from_shared(format!("http://{}", source.server_address))
        .context("pz-server address")?
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT);
    let mut client = PrintBridgeClient::connect(endpoint)
        .await
        .with_context(|| format!("connect to pz-server {}", source.server_address))?;
    let reply = client
        .request_certificate(CertificateRequest {
            client_id: source.client_id.clone(),
            hostname: source.hostname.clone(),
            csr_der: csr_der.to_vec(),
        })
        .await
        .context("RequestCertificate")?
        .into_inner();
    reply_chain(reply)
}

/// One renewal: new key + CSR, pz-server issues, the answer is checked and
/// installed.
pub async fn renew(source: &CertSource, paths: &CertPaths) -> Result<Leaf> {
    let fresh = new_key_and_csr(&source.hostname)?;
    let chain = request_chain(source, &fresh.csr_der).await?;
    let leaf = check_issued(&chain, &source.hostname, &fresh.public_key, unix_now())?;
    store(paths, &fresh.key_pem, &chain)?;
    Ok(leaf)
}

/// Keep the pair valid for as long as the source runs; `reload` is notified
/// after every new certificate.
pub async fn maintain(source: CertSource, paths: CertPaths, reload: &Notify) {
    let mut retry = Backoff::new(RETRY_BASE, RETRY_MAX);
    loop {
        let status = status_of(&paths, &source.hostname);
        let due = renewal_due(&status, unix_now());
        info!(hostname = %source.hostname, status = %status, due, "LAN API certificate checked");
        let delay = if due {
            match renew(&source, &paths).await {
                Ok(leaf) => {
                    retry.reset();
                    reload.notify_one();
                    info!(hostname = %source.hostname, not_after = leaf.not_after, "LAN API certificate issued by pz-server and installed");
                    CHECK_INTERVAL
                }
                Err(e) => {
                    let delay = retry.next_delay();
                    warn!(hostname = %source.hostname, error = %format!("{e:#}"), retry_in_s = delay.as_secs(), "LAN API certificate request failed - the listener keeps the old one (if any)");
                    delay
                }
            }
        } else {
            CHECK_INTERVAL
        };
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
#[path = "cert_tests.rs"]
mod tests;
