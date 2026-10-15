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
//! start); a failed request is retried with backoff (5 min → 6 h), a
//! `not_connected` refusal (pz-server does not see this client's job stream
//! yet — at service start the request can be faster than the receiver)
//! after [`NOT_CONNECTED_RETRY`] (30 s). A certificate pz-server DID issue
//! but this PC could not use or install is not re-ordered before
//! [`REJECTED_RETRY`] (48 h) — every order counts against Let's Encrypt's 5
//! certificates per name per week. The certificate stays valid offline
//! until it expires (90 days).
//!
//! The pair is replaced key first, then certificate ([`store`]); a crash or
//! a failed rename in between is rolled forward ([`finish_pending`]) at
//! start and before every check, so the store never needs pz-server (the
//! internet) to get its listener back.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use tokio::sync::Notify;
use tonic::transport::Channel;
use tracing::{error, info, warn};
use x509_parser::prelude::{GeneralName, parse_x509_pem};

use devbridge_core::file_acl::{write_durable, write_private};
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
/// The next order after a certificate that was issued but could not be used
/// or installed here (Let's Encrypt allows 5 per name per week).
pub const REJECTED_RETRY: Duration = Duration::from_secs(48 * 3600);
/// The next request after `not_connected` (no job stream seen yet).
pub const NOT_CONNECTED_RETRY: Duration = Duration::from_secs(30);
/// pz-server's refusal: the request did not come from the address of this
/// client's current job stream (`devbridge_server::cert::CODE_NOT_CONNECTED`).
pub const CODE_NOT_CONNECTED: &str = "not_connected";
/// The whole `RequestCertificate` call: above pz-server's own answer bound
/// (it answers within 270 s, an order still running then is refused).
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

/// pz-server refused because it sees no job stream of this client from
/// this address (yet).
pub fn is_not_connected(reply: &CertificateReply) -> bool {
    !reply.ok && reply.error_code == CODE_NOT_CONNECTED
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

/// The `*.pem.new` file next to `path` (a replacement being installed).
pub fn pending(path: &Path) -> PathBuf {
    path.with_extension("pem.new")
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

fn install(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).with_context(|| format!("install {}", to.display()))
}

/// Replace the pair: both are written next to their place (the key
/// restricted before a byte of it is written), then renamed in — key first.
/// A failure before the key's rename leaves the old pair and no `*.new`
/// file; a failure between the renames leaves the new key + `cert.pem.new`,
/// which [`finish_pending`] installs.
pub fn store(paths: &CertPaths, key_pem: &str, chain_pem: &str) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create {}", paths.dir.display()))?;
    let key_new = pending(&paths.key);
    let cert_new = pending(&paths.cert);
    let staged = write_private(&key_new, key_pem.as_bytes())
        .with_context(|| format!("write {}", key_new.display()))
        .and_then(|()| {
            write_durable(&cert_new, chain_pem.as_bytes())
                .with_context(|| format!("write {}", cert_new.display()))
        })
        .and_then(|()| install(&key_new, &paths.key));
    if let Err(e) = staged {
        // the old pair is untouched; leave nothing half-written behind
        let _ = remove_if_present(&key_new);
        let _ = remove_if_present(&cert_new);
        return Err(e);
    }
    install(&cert_new, &paths.cert)
}

/// Roll an interrupted [`store`] forward: a `cert.pem.new` that belongs to
/// the new key (still `key.pem.new`, or already `key.pem`) is installed;
/// anything else pending is stale and removed (the old pair stays). `true`
/// when a pair was installed.
pub fn finish_pending(paths: &CertPaths) -> Result<bool> {
    let key_new = pending(&paths.key);
    let cert_new = pending(&paths.cert);
    let Ok(chain) = std::fs::read(&cert_new) else {
        // no pending certificate: the store stopped before the key was
        // installed, the old pair is intact
        remove_if_present(&key_new)?;
        return Ok(false);
    };
    if let Ok(new_key) = std::fs::read(&key_new) {
        if super::tls_config(&chain, &new_key).is_ok() {
            install(&key_new, &paths.key)?;
            install(&cert_new, &paths.cert)?;
            return Ok(true);
        }
    } else if let Ok(key) = std::fs::read(&paths.key)
        && super::tls_config(&chain, &key).is_ok()
    {
        install(&cert_new, &paths.cert)?;
        return Ok(true);
    }
    remove_if_present(&key_new)?;
    remove_if_present(&cert_new)?;
    Ok(false)
}

/// Where the certificate comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertSource {
    /// pz-server's gRPC address (`[client] server_address`).
    pub server_address: String,
    pub client_id: String,
    pub hostname: String,
}

async fn ask_for_certificate(source: &CertSource, csr_der: &[u8]) -> Result<CertificateReply> {
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
    Ok(reply)
}

async fn request_chain(source: &CertSource, csr_der: &[u8]) -> Result<String, RenewFailure> {
    let reply = ask_for_certificate(source, csr_der)
        .await
        .map_err(RenewFailure::NotIssued)?;
    let not_connected = is_not_connected(&reply);
    reply_chain(reply).map_err(|e| {
        if not_connected {
            RenewFailure::NotConnected(e)
        } else {
            RenewFailure::NotIssued(e)
        }
    })
}

/// Why a renewal failed.
#[derive(Debug)]
pub enum RenewFailure {
    /// No certificate came back (pz-server unreachable, refused, the CA
    /// failed) — retried with backoff.
    NotIssued(anyhow::Error),
    /// pz-server sees no job stream of this client from this address yet
    /// (`not_connected`) — asked again soon, outside the backoff.
    NotConnected(anyhow::Error),
    /// pz-server issued one, but it was not usable here or could not be
    /// installed — re-ordering at once would only burn the CA's limits.
    Rejected(anyhow::Error),
}

impl RenewFailure {
    /// When to try again.
    pub fn retry_after(&self, backoff: &mut Backoff) -> Duration {
        match self {
            Self::NotIssued(_) => backoff.next_delay(),
            Self::NotConnected(_) => NOT_CONNECTED_RETRY,
            Self::Rejected(_) => REJECTED_RETRY,
        }
    }
}

impl std::fmt::Display for RenewFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotIssued(e) => write!(f, "not issued: {e:#}"),
            Self::NotConnected(e) => write!(f, "no job stream seen by pz-server yet: {e:#}"),
            Self::Rejected(e) => write!(f, "issued but not usable here: {e:#}"),
        }
    }
}

/// One renewal: new key + CSR, pz-server issues, the answer is checked and
/// installed.
pub async fn renew(source: &CertSource, paths: &CertPaths) -> Result<Leaf, RenewFailure> {
    let fresh = new_key_and_csr(&source.hostname).map_err(RenewFailure::NotIssued)?;
    let chain = request_chain(source, &fresh.csr_der).await?;
    let leaf = check_issued(&chain, &source.hostname, &fresh.public_key, unix_now())
        .map_err(RenewFailure::Rejected)?;
    store(paths, &fresh.key_pem, &chain).map_err(RenewFailure::Rejected)?;
    Ok(leaf)
}

/// Keep the pair valid for as long as the source runs; `reload` is notified
/// after every new (or rolled-forward) certificate.
pub async fn maintain(source: CertSource, paths: CertPaths, reload: &Notify) {
    let mut retry = Backoff::new(RETRY_BASE, RETRY_MAX);
    loop {
        match finish_pending(&paths) {
            Ok(installed) => {
                if installed {
                    reload.notify_one();
                }
                info!(hostname = %source.hostname, installed, "LAN API certificate: interrupted replacement checked");
            }
            Err(e) => {
                warn!(hostname = %source.hostname, error = %format!("{e:#}"), "LAN API certificate: an interrupted replacement could not be finished");
            }
        }
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
                Err(failure) => {
                    let mut delay = failure.retry_after(&mut retry);
                    match &failure {
                        RenewFailure::NotIssued(_) | RenewFailure::NotConnected(_) => {
                            warn!(hostname = %source.hostname, error = %failure, retry_in_s = delay.as_secs(), "LAN API certificate request failed - the listener keeps the old one (if any)");
                        }
                        RenewFailure::Rejected(_) => {
                            // a failed final rename leaves the new pair
                            // pending: finish it now, not in 48 h
                            let rolled = finish_pending(&paths).unwrap_or(false);
                            if rolled {
                                reload.notify_one();
                                delay = CHECK_INTERVAL;
                            }
                            error!(hostname = %source.hostname, error = %failure, rolled, retry_in_s = delay.as_secs(), "LAN API certificate was issued but could NOT be used - not re-ordered before the retry (CA rate limits)");
                        }
                    }
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
