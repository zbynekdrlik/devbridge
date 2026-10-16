//! The ACME order of a store's LAN API certificate (#101): Let's Encrypt
//! (or any RFC 8555 CA — Pebble in CI) via `instant-acme`, DNS-01 through a
//! [`DnsProvider`]. The CSR comes from the store, so the private key never
//! exists here.
//!
//! One account per pz-server, kept in `<data_dir>/acme-account.json`
//! (created on the first order; it holds the ACCOUNT key, not a store key —
//! written restricted to SYSTEM + Administrators).
//! Every order: new order for the one name → for each pending
//! authorization the `_acme-challenge` TXT record → wait for propagation →
//! challenge ready → order ready → finalize with the store's CSR → the
//! chain. The TXT records are removed whatever happened — in the
//! background, so the chain is not held back by Cloudflare. The bounded
//! steps (record ≤ 30 s, propagation ≤ 60 s `MAX_PROPAGATION_SECS`,
//! validation [`READY_TIMEOUT`], download [`CERTIFICATE_TIMEOUT`]) leave
//! room for the CA's own requests inside `super::ANSWER_TIMEOUT` (270 s);
//! the issuer answers `busy` if an order is slower than that.
//!
//! The orchestration is exercised against a real ACME server (Pebble) by the
//! CI job `ACME (Pebble)` (`tests/acme_pebble.rs`); it is in
//! `.cargo/mutants.toml` `exclude_re`, the decisions around it (who gets
//! which name, the CSR, Cloudflare) are mutated in `mod.rs` / `dns.rs`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier,
    NewAccount, NewOrder, Order, OrderStatus, RetryPolicy,
};
use tokio::sync::Mutex;
use tracing::{info, warn};

use devbridge_core::file_acl::write_private;

use super::dns::{DnsProvider, txt_record_name};
use super::{AcmeSettings, CertificateAuthority};

/// How long the CA may take to validate the challenge.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the CA may take to issue after the CSR (seconds in practice).
const CERTIFICATE_TIMEOUT: Duration = Duration::from_secs(30);
/// The whole order (account, order, challenges, certificate). instant-acme
/// sets no HTTP timeouts and checks a RetryPolicy only between polls, so a
/// hung CA call is cut off here — the records made so far are still removed.
/// Below `super::ORDER_LIMIT` (checked at compile time): the issuer's own
/// cut-off would drop this cleanup.
const ORDER_LIMIT: Duration = Duration::from_secs(600);
const _: () = assert!(ORDER_LIMIT.as_secs() < super::ORDER_LIMIT.as_secs());

/// The production [`CertificateAuthority`].
pub struct AcmeCa {
    settings: AcmeSettings,
    dns: Arc<dyn DnsProvider>,
    account: Mutex<Option<Account>>,
}

impl AcmeCa {
    /// No network yet: the account is loaded / created on the first order.
    pub fn new(settings: AcmeSettings, dns: Arc<dyn DnsProvider>) -> Self {
        // instant-acme's HTTPS client uses the process-wide rustls provider;
        // name it, whatever else the build enables (`Err` = already set).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        info!(directory = %settings.directory_url, account = %settings.account_file.display(), "ACME certificate authority ready");
        Self {
            settings,
            dns,
            account: Mutex::new(None),
        }
    }

    fn builder(&self) -> Result<AccountBuilder> {
        match &self.settings.ca_file {
            Some(root) => Account::builder_with_root(root)
                .with_context(|| format!("trust the ACME CA root {}", root.display())),
            None => Account::builder().context("ACME HTTPS client"),
        }
    }

    /// The account: cached, else from the file, else created (and saved).
    async fn account(&self) -> Result<Account> {
        let mut cached = self.account.lock().await;
        if let Some(account) = cached.as_ref() {
            return Ok(account.clone());
        }
        let file = &self.settings.account_file;
        let account = match std::fs::read_to_string(file) {
            Ok(text) => {
                let credentials: AccountCredentials = serde_json::from_str(&text)
                    .with_context(|| format!("read the ACME account {}", file.display()))?;
                self.builder()?
                    .from_credentials(credentials)
                    .await
                    .context("restore the ACME account")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let contact: Vec<&str> = self.settings.contact.iter().map(String::as_str).collect();
                let (account, credentials) = self
                    .builder()?
                    .create(
                        &NewAccount {
                            contact: &contact,
                            terms_of_service_agreed: true,
                            only_return_existing: false,
                        },
                        self.settings.directory_url.clone(),
                        None,
                    )
                    .await
                    .context("create the ACME account")?;
                let text = serde_json::to_string_pretty(&credentials)
                    .context("serialize the ACME account")?;
                write_private(file, text.as_bytes())
                    .with_context(|| format!("save the ACME account {}", file.display()))?;
                info!(account = %account.id(), file = %file.display(), "ACME account created");
                account
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read the ACME account {}", file.display()));
            }
        };
        *cached = Some(account.clone());
        Ok(account)
    }

    /// The account, a new order for `hostname`, then [`Self::complete`].
    async fn order(
        &self,
        hostname: &str,
        csr_der: &[u8],
        records: &mut Vec<String>,
    ) -> Result<String> {
        let account = self.account().await?;
        let identifiers = [Identifier::Dns(hostname.to_string())];
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .context("ACME new order")?;
        self.complete(&mut order, hostname, csr_der, records).await
    }

    /// Answer the challenges (records go to `records` as they are made),
    /// then finalize with the store's CSR.
    async fn complete(
        &self,
        order: &mut Order,
        hostname: &str,
        csr_der: &[u8],
        records: &mut Vec<String>,
    ) -> Result<String> {
        {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authz = authorization.context("ACME authorization")?;
                match authz.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    other => bail!("the ACME authorization of {hostname} is {other:?}"),
                }
                let mut challenge = authz
                    .challenge(ChallengeType::Dns01)
                    .context("the CA offers no dns-01 challenge")?;
                let name = txt_record_name(hostname);
                let id = self
                    .dns
                    .set_txt(&name, &challenge.key_authorization().dns_value())
                    .await?;
                records.push(id);
                info!(record = %name, wait_s = self.settings.propagation.as_secs(), "ACME DNS-01 record created");
                tokio::time::sleep(self.settings.propagation).await;
                challenge
                    .set_ready()
                    .await
                    .context("ACME challenge ready")?;
            }
        }
        let status = order
            .poll_ready(&RetryPolicy::new().timeout(READY_TIMEOUT))
            .await
            .context("ACME order validation")?;
        ensure!(
            status == OrderStatus::Ready,
            "the ACME order of {hostname} is {status:?} after validation"
        );
        order.finalize_csr(csr_der).await.context("ACME finalize")?;
        order
            .poll_certificate(&RetryPolicy::new().timeout(CERTIFICATE_TIMEOUT))
            .await
            .context("ACME certificate download")
    }
}

#[async_trait]
impl CertificateAuthority for AcmeCa {
    async fn issue(&self, hostname: &str, csr_der: &[u8]) -> Result<String> {
        let mut records = Vec::new();
        let issued = tokio::time::timeout(ORDER_LIMIT, self.order(hostname, csr_der, &mut records))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow!(
                    "the ACME order of {hostname} took longer than {} s",
                    ORDER_LIMIT.as_secs()
                ))
            });
        // the records are no longer needed: removed in the background
        let dns = Arc::clone(&self.dns);
        tokio::spawn(async move {
            for record in &records {
                match dns.remove_txt(record).await {
                    Ok(()) => info!(record = %record, "ACME DNS-01 record removed"),
                    Err(e) => {
                        warn!(record = %record, error = %format!("{e:#}"), "ACME DNS-01 record could not be removed - remove it by hand");
                    }
                }
            }
        });
        issued
    }
}
