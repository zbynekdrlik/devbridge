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
//! 2. the name is exactly `<client_id>.<zone>` — a store gets its own name
//!    only (`bad_name`);
//! 3. the CSR names exactly that one DNS name (`bad_csr`);
//! 4. no order for that name is running (`busy`) and none was issued in the
//!    last [`MIN_REISSUE_INTERVAL`] (`rate_limited` — protects the CA's
//!    rate limits from a client stuck in a loop).

pub mod acme;
pub mod dns;

use std::collections::HashMap;
use std::path::Path;
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

/// Reply codes.
pub const CODE_ACME_DISABLED: &str = "acme_disabled";
pub const CODE_NOT_APPROVED: &str = "not_approved";
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

/// Answers `RequestCertificate` (see the module docs).
pub struct CertificateIssuer {
    zone: String,
    ca: Arc<dyn CertificateAuthority>,
    min_interval: Duration,
    slots: Mutex<HashMap<String, Slot>>,
}

impl CertificateIssuer {
    pub fn new(zone: &str, ca: Arc<dyn CertificateAuthority>) -> Self {
        Self {
            zone: zone.to_string(),
            ca,
            min_interval: MIN_REISSUE_INTERVAL,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Shorten the re-issue interval (tests).
    pub fn with_min_interval(mut self, min_interval: Duration) -> Self {
        self.min_interval = min_interval;
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

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Check `request` (from a client that is `approved` here) and, when it
    /// passes, issue the certificate.
    pub async fn answer(&self, request: &CertificateRequest, approved: bool) -> CertificateReply {
        let client_id = request.client_id.as_str();
        let hostname = request.hostname.as_str();
        if !approved {
            warn!(
                client_id,
                hostname, "certificate refused: the client is not approved here"
            );
            return refusal(
                CODE_NOT_APPROVED,
                format!("client '{client_id}' is not approved on this pz-server"),
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
            let mut slots = self.slots();
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
        let issued = self.ca.issue(&own_name, &request.csr_der).await;
        let mut slots = self.slots();
        match issued {
            Ok(chain_pem) => {
                slots.insert(own_name.clone(), Slot::Issued(Instant::now()));
                info!(client_id, hostname, "LAN API certificate issued");
                CertificateReply {
                    ok: true,
                    chain_pem,
                    error_code: String::new(),
                    error: String::new(),
                }
            }
            Err(e) => {
                slots.remove(&own_name);
                let text = format!("{e:#}");
                error!(client_id, hostname, error = %text, "LAN API certificate could not be issued");
                refusal(CODE_ACME_FAILED, text)
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
    let ca = acme::AcmeCa::new(
        acme::AcmeSettings::from_config(cfg, data_dir),
        Arc::new(dns),
    );
    Ok(Some(Arc::new(CertificateIssuer::new(
        &cfg.zone,
        Arc::new(ca),
    ))))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
