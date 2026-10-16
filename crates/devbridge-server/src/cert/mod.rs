//! The stores' LAN API certificates (#101) — the `RequestCertificate` RPC.
//!
//! A store PC sends a CSR for its own name; pz-server checks WHO asks and
//! WHAT for, then a [`CertificateAuthority`] issues it — in production
//! [`acme::AcmeCa`]: Let's Encrypt, ACME DNS-01 through Cloudflare
//! ([`dns`]). The store's private key never leaves the store; the DNS token
//! never leaves pz-server.
//!
//! The checks, in order (the first failure answers):
//! 1. the client is APPROVED on this server (`not_approved`);
//! 2. the request comes from the address of the client's current job
//!    stream (`not_connected`) — the gRPC identity is the client_id the
//!    caller names, so this binds the request to the connection that
//!    receives that store's print jobs. A binding, NOT authentication: a
//!    peer that takes over the stream (cutting the store's printing) passes;
//!    real client authentication is a separate change across every RPC;
//! 3. the name is exactly `<client_id>.<zone>` — a store gets its own name
//!    only (`bad_name`);
//! 4. the CSR names exactly that one DNS name (`bad_csr`);
//! 5. no order for that name is running (`busy`) and none was issued in the
//!    last [`MIN_REISSUE_INTERVAL`] (`rate_limited` — protects the CA's
//!    rate limits from a client stuck in a loop).
//!
//! The order runs in its own task: a request dropped on the way (the
//! store's timeout, a VPN drop) never leaves the name `busy` — the order
//! goes on, removes its DNS records and settles the name, and is cut off
//! after [`ORDER_LIMIT`] (instant-acme sets no HTTP timeouts: a hung CA
//! call would otherwise keep the name busy until a restart). The request
//! is answered within [`ANSWER_TIMEOUT`] (the store waits 300 s): an order
//! still running then is answered `busy` and goes on.

pub mod acme;
pub mod dns;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use tracing::{error, info, warn};
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509CertificationRequest};

use devbridge_core::config::{AcmeServerConfig, valid_dns_label};
use devbridge_core::proto::{CertificateReply, CertificateRequest};

/// A name gets at most one certificate per this interval.
pub const MIN_REISSUE_INTERVAL: Duration = Duration::from_secs(3600);
/// A certificate request is answered within this — below the store's own
/// 300 s `RequestCertificate` timeout.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(270);
/// An order still running after this fails and frees its name (the ACME
/// order cuts itself off earlier, at `acme::ORDER_LIMIT`, so it can still
/// remove its records).
pub const ORDER_LIMIT: Duration = Duration::from_secs(660);
/// The ACME account file in the server's data dir (the ACCOUNT key —
/// restricted to SYSTEM + Administrators).
pub const ACCOUNT_FILE: &str = "acme-account.json";

/// Reply codes.
pub const CODE_ACME_DISABLED: &str = "acme_disabled";
pub const CODE_NOT_APPROVED: &str = "not_approved";
pub const CODE_NOT_CONNECTED: &str = "not_connected";
pub const CODE_BAD_NAME: &str = "bad_name";
pub const CODE_BAD_CSR: &str = "bad_csr";
pub const CODE_BUSY: &str = "busy";
pub const CODE_RATE_LIMITED: &str = "rate_limited";
pub const CODE_ACME_FAILED: &str = "acme_failed";

/// Turns a checked CSR into a certificate chain.
#[async_trait]
pub trait CertificateAuthority: Send + Sync {
    /// Issue a certificate for `hostname` (already checked against the CSR);
    /// returns the PEM chain, leaf first.
    async fn issue(&self, hostname: &str, csr_der: &[u8]) -> Result<String>;
}

/// What an [`acme::AcmeCa`] needs from `[server.acme]`.
#[derive(Debug, Clone, PartialEq)]
pub struct AcmeSettings {
    pub directory_url: String,
    /// `mailto:` contact of the account (none when empty).
    pub contact: Option<String>,
    /// A private / test CA root to trust (Pebble).
    pub ca_file: Option<PathBuf>,
    pub account_file: PathBuf,
    pub propagation: Duration,
}

impl AcmeSettings {
    /// Trimmed values; the account lives in `<data_dir>/`[`ACCOUNT_FILE`].
    pub fn from_config(cfg: &AcmeServerConfig, data_dir: &Path) -> Self {
        let email = cfg.contact_email.trim();
        let ca_file = cfg.ca_file.trim();
        Self {
            directory_url: cfg.directory_url.trim().to_string(),
            contact: (!email.is_empty()).then(|| format!("mailto:{email}")),
            ca_file: (!ca_file.is_empty()).then(|| PathBuf::from(ca_file)),
            account_file: data_dir.join(ACCOUNT_FILE),
            propagation: Duration::from_secs(cfg.propagation_secs),
        }
    }
}

/// Who asks for a certificate, as the gRPC service sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// The client is approved on this server.
    pub approved: bool,
    /// The request comes from the address of the client's own live job
    /// stream ([`same_peer`]).
    pub on_its_stream: bool,
}

/// Both addresses are known and equal.
pub fn same_peer(stream: Option<IpAddr>, request: Option<IpAddr>) -> bool {
    matches!((stream, request), (Some(s), Some(r)) if s == r)
}

/// A refusal of `RequestCertificate`.
pub fn refusal(code: &str, text: impl Into<String>) -> CertificateReply {
    CertificateReply {
        ok: false,
        chain_pem: String::new(),
        error_code: code.to_string(),
        error: text.into(),
    }
}

/// The answer of a server without `[server.acme]`.
pub fn disabled_reply() -> CertificateReply {
    refusal(
        CODE_ACME_DISABLED,
        "this pz-server issues no LAN API certificates ([server.acme] is not enabled)",
    )
}

/// The DNS names a CSR asks for. `Err` for an unreadable CSR or any
/// subjectAltName that is not a DNS name.
pub fn csr_dns_names(csr_der: &[u8]) -> Result<Vec<String>> {
    let (_, csr) = X509CertificationRequest::from_der(csr_der)
        .map_err(|e| anyhow!("unreadable CSR ({e:?})"))?;
    let mut names = Vec::new();
    for extension in csr.requested_extensions().into_iter().flatten() {
        if let ParsedExtension::SubjectAlternativeName(san) = extension {
            for name in &san.general_names {
                match name {
                    GeneralName::DNSName(dns) => names.push(dns.to_string()),
                    other => return Err(anyhow!("the CSR asks for a non-DNS name ({other:?})")),
                }
            }
        }
    }
    Ok(names)
}

/// Where an issuance of one name stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// An order is running.
    Busy,
    /// The last certificate was issued at this instant.
    Issued(Instant),
}

/// Whether a new order may start for a name in `slot` at `now`
/// (`Err(code)` otherwise). Due exactly at `min_interval`.
fn slot_free(slot: Option<Slot>, now: Instant, min_interval: Duration) -> Result<(), &'static str> {
    match slot {
        None => Ok(()),
        Some(Slot::Busy) => Err(CODE_BUSY),
        Some(Slot::Issued(at)) if now.saturating_duration_since(at) < min_interval => {
            Err(CODE_RATE_LIMITED)
        }
        Some(Slot::Issued(_)) => Ok(()),
    }
}

type Slots = Arc<Mutex<HashMap<String, Slot>>>;

fn lock(slots: &Slots) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
    slots.lock().unwrap_or_else(|p| p.into_inner())
}

/// Settles a name's slot when its order ends — however it ends (an error, a
/// panic): issued → [`Slot::Issued`], else free.
struct SlotGuard {
    slots: Slots,
    name: String,
    issued: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = lock(&self.slots);
        if self.issued {
            slots.insert(self.name.clone(), Slot::Issued(Instant::now()));
        } else {
            slots.remove(&self.name);
        }
    }
}

/// Answers `RequestCertificate` (see the module docs).
pub struct CertificateIssuer {
    zone: String,
    ca: Arc<dyn CertificateAuthority>,
    min_interval: Duration,
    answer_timeout: Duration,
    order_limit: Duration,
    slots: Slots,
}

impl CertificateIssuer {
    pub fn new(zone: &str, ca: Arc<dyn CertificateAuthority>) -> Self {
        Self {
            zone: zone.to_string(),
            ca,
            min_interval: MIN_REISSUE_INTERVAL,
            answer_timeout: ANSWER_TIMEOUT,
            order_limit: ORDER_LIMIT,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Shorten the re-issue interval (tests).
    pub fn with_min_interval(mut self, min_interval: Duration) -> Self {
        self.min_interval = min_interval;
        self
    }

    /// Shorten the answer bound (tests).
    pub fn with_answer_timeout(mut self, answer_timeout: Duration) -> Self {
        self.answer_timeout = answer_timeout;
        self
    }

    /// Shorten the order limit (tests).
    pub fn with_order_limit(mut self, order_limit: Duration) -> Self {
        self.order_limit = order_limit;
        self
    }

    pub fn zone(&self) -> &str {
        &self.zone
    }

    /// The one name `client_id` may get: `<client_id>.<zone>` (lower-case);
    /// `None` when the client id is not a DNS label.
    pub fn name_for(&self, client_id: &str) -> Option<String> {
        let label = client_id.to_ascii_lowercase();
        valid_dns_label(&label).then(|| format!("{label}.{}", self.zone))
    }

    /// Check `request` from `caller` and, when it passes, issue the
    /// certificate.
    pub async fn answer(&self, request: &CertificateRequest, caller: Caller) -> CertificateReply {
        let client_id = request.client_id.as_str();
        let hostname = request.hostname.as_str();
        if !caller.approved {
            warn!(
                client_id,
                hostname, "certificate refused: the client is not approved here"
            );
            return refusal(
                CODE_NOT_APPROVED,
                format!("client '{client_id}' is not approved on this pz-server"),
            );
        }
        if !caller.on_its_stream {
            warn!(
                client_id,
                hostname,
                "certificate refused: not from the address of the client's live job stream"
            );
            return refusal(
                CODE_NOT_CONNECTED,
                format!(
                    "client '{client_id}' has no job stream from this address - a certificate is issued only to the connected client"
                ),
            );
        }
        let Some(own_name) = self.name_for(client_id).filter(|name| name == hostname) else {
            warn!(client_id, hostname, zone = %self.zone, "certificate refused: not the client's own name");
            return refusal(
                CODE_BAD_NAME,
                format!(
                    "client '{client_id}' may get only '<client_id>.{}', not '{hostname}'",
                    self.zone
                ),
            );
        };
        match csr_dns_names(&request.csr_der) {
            Ok(names) if names == [own_name.clone()] => {}
            Ok(names) => {
                warn!(client_id, hostname, csr_names = ?names, "certificate refused: the CSR names other hosts");
                return refusal(
                    CODE_BAD_CSR,
                    format!("the CSR must name exactly {own_name} (it names {names:?})"),
                );
            }
            Err(e) => {
                warn!(client_id, hostname, error = %format!("{e:#}"), "certificate refused: bad CSR");
                return refusal(CODE_BAD_CSR, format!("{e:#}"));
            }
        }
        {
            let mut slots = lock(&self.slots);
            let slot = slots.get(&own_name).copied();
            if let Err(code) = slot_free(slot, Instant::now(), self.min_interval) {
                warn!(
                    client_id,
                    hostname,
                    code,
                    "certificate refused: an order for this name is running or was just issued"
                );
                return refusal(
                    code,
                    format!(
                        "a certificate for {own_name} is being issued or was issued less than {} s ago",
                        self.min_interval.as_secs()
                    ),
                );
            }
            slots.insert(own_name.clone(), Slot::Busy);
        }
        info!(client_id, hostname, "issuing a LAN API certificate");
        let mut guard = SlotGuard {
            slots: Arc::clone(&self.slots),
            name: own_name,
            issued: false,
        };
        let ca = Arc::clone(&self.ca);
        let csr_der = request.csr_der.clone();
        let client = client_id.to_string();
        let order_limit = self.order_limit;
        // Its own task: the order ends (records removed, slot settled) even
        // when this request is dropped, and at the latest after the limit.
        let order = tokio::spawn(async move {
            let issued = tokio::time::timeout(order_limit, ca.issue(&guard.name, &csr_der))
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow!(
                        "the certificate order took longer than {} s",
                        order_limit.as_secs()
                    ))
                });
            guard.issued = issued.is_ok();
            match &issued {
                Ok(_) => {
                    info!(client_id = %client, hostname = %guard.name, "LAN API certificate issued");
                }
                Err(e) => {
                    error!(client_id = %client, hostname = %guard.name, error = %format!("{e:#}"), "LAN API certificate could not be issued");
                }
            }
            issued
        });
        match tokio::time::timeout(self.answer_timeout, order).await {
            Ok(Ok(Ok(chain_pem))) => CertificateReply {
                ok: true,
                chain_pem,
                error_code: String::new(),
                error: String::new(),
            },
            Ok(Ok(Err(e))) => refusal(CODE_ACME_FAILED, format!("{e:#}")),
            Ok(Err(e)) => {
                error!(client_id, hostname, error = %e, "the certificate order task failed");
                refusal(
                    CODE_ACME_FAILED,
                    format!("the certificate order failed: {e}"),
                )
            }
            Err(_) => {
                warn!(
                    client_id,
                    hostname,
                    wait_s = self.answer_timeout.as_secs(),
                    "LAN API certificate order still running - answered busy, it goes on"
                );
                refusal(
                    CODE_BUSY,
                    format!(
                        "the certificate order for {hostname} is still running after {} s - ask again later",
                        self.answer_timeout.as_secs()
                    ),
                )
            }
        }
    }
}

/// The issuer `[server.acme]` asks for: `None` when disabled, `Err` when
/// enabled but invalid. `env_token` is `DEVBRIDGE_CLOUDFLARE_API_TOKEN`. The
/// ACME account is kept in `<data_dir>/acme-account.json`.
pub fn issuer_from_config(
    cfg: &AcmeServerConfig,
    data_dir: &Path,
    env_token: Option<&str>,
) -> Result<Option<Arc<CertificateIssuer>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    let token = cfg.api_token(env_token);
    let problems = cfg.problems(&token);
    ensure!(
        problems.is_empty(),
        "[server.acme] is enabled but invalid: {}",
        problems.join("; ")
    );
    let dns = dns::CloudflareDns::new(&cfg.cloudflare.api_base, &cfg.cloudflare.zone_id, &token)
        .context("Cloudflare client")?;
    let ca = acme::AcmeCa::new(AcmeSettings::from_config(cfg, data_dir), Arc::new(dns));
    Ok(Some(Arc::new(CertificateIssuer::new(
        &cfg.zone,
        Arc::new(ca),
    ))))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
