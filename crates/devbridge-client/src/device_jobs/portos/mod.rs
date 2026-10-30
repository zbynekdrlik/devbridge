//! Portos eKasa API executor for `fiscal.*` device jobs (#100).
//!
//! Portos (NineDigit, `NineDigit.eKasa.API` service, HTTP on port 3010, no
//! authentication) registers eKasa receipts in the store's CHDÚ and prints
//! them. Each till's Portos is `[[client.pokladna.kasy]] portos_url`
//! (default `http://localhost:3010`); every Portos is its own serial device
//! queue. Contract and decision tables: `docs/device-jobs-fiscal.md`.
//!
//! A receipt is registered EXACTLY ONCE:
//!
//! 1. devbridge owns the key: `externalId = "dbj-"` + 32 hex of
//!    SHA-256(`"<kasa_id>:<client_uuid>"`) ([`payload::external_id`], 36
//!    characters — Portos accepts at most 40) — never taken from the payload;
//! 2. before any register POST it asks Portos for that externalId
//!    (`GET /api/v1/requests/receipts/receipt`) — found → `done`, nothing is
//!    posted (a lost answer, a restart, an offline-path receipt);
//! 3. it marks the job's device contact in the journal
//!    ([`Journal::mark_device_contact`], durable) and POSTs; a connection
//!    that never opened is `failed` ("nothing ran"), a validation refusal
//!    (HTTP 400, code -900) is `failed`; ANY other unclear end (timeout,
//!    reset, 5xx — a print error comes AFTER registration) is
//!    `indeterminate`;
//! 4. reconcile asks Portos again: found → `done`; not found and never
//!    sent → `failed not_executed`; not found but sent → still unknown, a
//!    person checks (Portos' docs do not prove a lookup miss means "never
//!    stored").
//!
//! `fiscal.open_drawer` / `fiscal.print_copy` / `fiscal.print_text` leave no
//! fiscal record (`print_text` reaches only `printers/print`): an
//! unknown outcome is reconciled to `failed outcome_unknown_retry_safe` —
//! the one documented exception to "failed only when nothing ran".
//!
//! Logs name the job, the externalId, the Portos address and the receipt
//! number / state — never items, prices or payments.

pub mod http;
pub mod outcome;
pub mod payload;
pub mod status;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{info, warn};

use devbridge_core::config::PokladnaClientConfig;

use self::http::{PortosHttp, Reply};
use self::outcome::{
    CODE_LOOKUP_FAILED, CODE_NOT_EXECUTED, CODE_RETRY_SAFE, CODE_UNREACHABLE, Lookup, Via,
    classify_lookup, classify_unprocessed, control_outcome, receipt_result, receipt_summary,
    register_outcome, retry_safe_text, sent_but_not_found_text,
};
use self::payload::{
    CODE_BAD_PAYLOAD, FiscalOp, ReceiptKind, cash_register_code, check_payload, client_uuid_ok,
    copy_query, drawer_query, external_id, print_text_body, register_body,
};
use self::status::{DeviceState, probe_state, worst};
use super::api::DeviceJob;
use super::executor::{
    CODE_UNSUPPORTED, DeviceExecutor, DeviceKind, Outcome, Reconciliation, Rejection,
};
use super::journal::{Journal, JournalEntry};

/// Portos' address when a till names none.
pub const DEFAULT_PORTOS_URL: &str = "http://localhost:3010";

/// Error code: a job that names no till (kasa).
pub const CODE_NO_KASA: &str = "no_kasa";
/// Error code: a receipt job whose `client_uuid` cannot be part of an
/// externalId.
pub const CODE_BAD_CLIENT_UUID: &str = "bad_client_uuid";
/// Error code: the device contact could not be journaled — nothing was sent.
pub const CODE_JOURNAL: &str = "bridge_error";

pub const RECEIPT_PATH: &str = "/api/v1/requests/receipts/receipt";
pub const UNPROCESSED_PATH: &str = "/api/v1/requests/unprocessed";
pub const PRINT_COPY_PATH: &str = "/api/v1/requests/receipts/print_copy";
pub const OPEN_DRAWER_PATH: &str = "/api/v1/printers/open_drawer";
pub const PRINT_TEXT_PATH: &str = "/api/v1/printers/print";
pub const PRODUCT_INFO_PATH: &str = "/api/v1/product/info";
pub const PRINTER_STATUS_PATH: &str = "/api/v1/printers/status";

/// Per-call timeouts. All stay well below the source's `execute_guard`.
#[derive(Debug, Clone, PartialEq)]
pub struct PortosTiming {
    /// Receipt lookup / unprocessed list.
    pub lookup: Duration,
    /// Register POST (Portos waits up to ~5 s for eKasa before it issues an
    /// offline receipt, then prints).
    pub register: Duration,
    /// Drawer / copy.
    pub control: Duration,
    /// Status probes (`product/info`, `printers/status`).
    pub probe: Duration,
}

impl Default for PortosTiming {
    fn default() -> Self {
        Self {
            lookup: Duration::from_secs(15),
            register: Duration::from_secs(60),
            control: Duration::from_secs(30),
            probe: Duration::from_secs(5),
        }
    }
}

/// Which Portos serves which till.
#[derive(Debug, Clone, PartialEq)]
pub struct PortosConfig {
    /// A till without its own `portos_url` (or not listed) uses this one.
    pub default_url: String,
    /// `kasa_id → portos_url` of `[[client.pokladna.kasy]]`.
    pub kasy: BTreeMap<i64, String>,
    pub timing: PortosTiming,
}

/// `http://host:3010/` → `http://host:3010`.
fn base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

impl PortosConfig {
    pub fn from_pokladna(cfg: &PokladnaClientConfig) -> Self {
        let kasy = cfg
            .kasy
            .iter()
            .filter_map(|k| {
                let url = base_url(k.portos_url.as_deref()?);
                (!url.is_empty()).then_some((k.id, url))
            })
            .collect();
        Self {
            default_url: DEFAULT_PORTOS_URL.to_string(),
            kasy,
            timing: PortosTiming::default(),
        }
    }

    /// The Portos of a till.
    pub fn url_for(&self, kasa_id: Option<i64>) -> &str {
        kasa_id
            .and_then(|id| self.kasy.get(&id))
            .map_or(self.default_url.as_str(), String::as_str)
    }

    /// Every Portos address, once each, in a stable order.
    pub fn endpoints(&self) -> Vec<String> {
        let mut all: BTreeSet<String> = self.kasy.values().cloned().collect();
        all.insert(self.default_url.clone());
        all.into_iter().collect()
    }
}

/// Path, query and body of a request that leaves no fiscal record — the
/// allowlist of what each of these operations may reach.
fn control_request(
    op: FiscalOp,
    payload: &serde_json::Value,
) -> (
    &'static str,
    Vec<(String, String)>,
    Option<serde_json::Value>,
) {
    match op {
        FiscalOp::OpenDrawer => (OPEN_DRAWER_PATH, drawer_query(payload), None),
        FiscalOp::PrintText => (PRINT_TEXT_PATH, Vec::new(), Some(print_text_body(payload))),
        FiscalOp::PrintCopy | FiscalOp::Register(_) => (PRINT_COPY_PATH, copy_query(payload), None),
    }
}

pub struct PortosExecutor {
    config: PortosConfig,
    http: PortosHttp,
    /// The source's own device journal: the device-contact mark of a job is
    /// written there before its register request leaves the PC.
    journal: Arc<Journal>,
    states: Mutex<BTreeMap<String, DeviceState>>,
}

impl PortosExecutor {
    pub fn new(config: PortosConfig, journal: Arc<Journal>) -> Result<Self> {
        let states = config
            .endpoints()
            .into_iter()
            .map(|e| (e, DeviceState::Unknown))
            .collect();
        info!(endpoints = ?config.endpoints(), "Portos executor ready");
        Ok(Self {
            http: PortosHttp::new().context("Portos HTTP client")?,
            config,
            journal,
            states: Mutex::new(states),
        })
    }

    fn states(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, DeviceState>> {
        self.states.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Store the probed state of `endpoint`; returns the previous state when
    /// it changed.
    pub fn record_state(&self, endpoint: &str, state: DeviceState) -> Option<DeviceState> {
        let previous = self.states().insert(endpoint.to_string(), state.clone());
        previous.filter(|p| *p != state)
    }

    /// `(Portos address, externalId, cash register code)` of a receipt job.
    fn receipt_key(&self, job: &DeviceJob, op: FiscalOp) -> Option<(String, String, String)> {
        let kasa_id = job.kasa_id?;
        let code = cash_register_code(op, &job.payload)?;
        Some((
            self.config.url_for(Some(kasa_id)).to_string(),
            external_id(kasa_id, &job.client_uuid),
            code.to_string(),
        ))
    }

    async fn lookup(&self, base: &str, code: &str, external_id: &str) -> Lookup {
        let query = [
            ("CashRegisterCode".to_string(), code.to_string()),
            ("ExternalId".to_string(), external_id.to_string()),
        ];
        classify_lookup(
            self.http
                .get(base, RECEIPT_PATH, &query, self.config.timing.lookup)
                .await,
        )
    }

    async fn register(&self, job: &DeviceJob, kind: ReceiptKind) -> Outcome {
        let Some((base, ext, code)) = self.receipt_key(job, FiscalOp::Register(kind)) else {
            return Outcome::failed(
                CODE_BAD_PAYLOAD,
                "the receipt names no till or cash register code - nothing ran",
            );
        };
        match self.lookup(&base, &code, &ext).await {
            Lookup::Found(found) => {
                let outcome = receipt_result(&found, &ext, Via::Lookup).map_or_else(
                    || Outcome::Indeterminate(format!(
                        "Portos found a receipt for externalId {ext} but its answer is unreadable - reconciled, never registered again"
                    )),
                    Outcome::Done,
                );
                info!(job_id = job.id, kasa_id = ?job.kasa_id, client_uuid = %job.client_uuid, external_id = %ext, portos = %base, receipt = %receipt_summary(&outcome), "Portos already has this receipt - NOT registered again");
                return outcome;
            }
            Lookup::NotFound => {}
            Lookup::Unreachable(e) => {
                return Outcome::failed(
                    CODE_UNREACHABLE,
                    format!(
                        "devbridge could not connect to Portos ({e}) - the receipt was not sent, nothing ran"
                    ),
                );
            }
            Lookup::Failed(e) => {
                return Outcome::failed(
                    CODE_LOOKUP_FAILED,
                    format!(
                        "Portos could not be asked whether externalId {ext} is registered ({e}) - the receipt was not sent, nothing ran"
                    ),
                );
            }
        }
        if let Err(e) = self.journal.mark_device_contact(job.id) {
            return Outcome::failed(
                CODE_JOURNAL,
                format!(
                    "the device contact could not be journaled ({e:#}) - the receipt was not sent, nothing ran"
                ),
            );
        }
        let body = register_body(&job.payload, &ext);
        let reply = self
            .http
            .post(
                &base,
                kind.path(),
                &[],
                Some(&body),
                self.config.timing.register,
            )
            .await;
        let outcome = register_outcome(reply, kind, &ext);
        info!(job_id = job.id, kasa_id = ?job.kasa_id, client_uuid = %job.client_uuid, external_id = %ext, portos = %base, outcome = outcome.label(), receipt = %receipt_summary(&outcome), "Portos register answered");
        outcome
    }

    async fn control(&self, job: &DeviceJob, op: FiscalOp) -> Outcome {
        let base = self.config.url_for(job.kasa_id);
        let (path, query, body) = control_request(op, &job.payload);
        let reply = self
            .http
            .post(
                base,
                path,
                &query,
                body.as_ref(),
                self.config.timing.control,
            )
            .await;
        let outcome = control_outcome(reply, op);
        info!(job_id = job.id, kasa_id = ?job.kasa_id, job_type = %job.job_type, portos = %base, outcome = outcome.label(), "Portos control request answered");
        outcome
    }

    async fn reconcile_receipt(&self, entry: &JournalEntry, kind: ReceiptKind) -> Reconciliation {
        let Some((base, ext, code)) = self.receipt_key(&entry.job, FiscalOp::Register(kind)) else {
            return Reconciliation::StillUnknown(
                "the receipt names no till or cash register code - a person must check it in Portos".into(),
            );
        };
        match self.lookup(&base, &code, &ext).await {
            Lookup::Found(found) => receipt_result(&found, &ext, Via::Reconcile).map_or_else(
                || Reconciliation::StillUnknown(format!(
                    "Portos found a receipt for externalId {ext} but its answer is unreadable - a person must check it in Portos"
                )),
                Reconciliation::Done,
            ),
            Lookup::Unreachable(e) | Lookup::Failed(e) => Reconciliation::StillUnknown(format!(
                "Portos could not be asked about externalId {ext} ({e}) - asked again later"
            )),
            Lookup::NotFound => {
                let contacted = entry.device_contact_at.is_some();
                self.reconcile_not_found(&base, &code, &ext, contacted).await
            }
        }
    }

    async fn reconcile_not_found(
        &self,
        base: &str,
        code: &str,
        ext: &str,
        contacted: bool,
    ) -> Reconciliation {
        if !contacted {
            return Reconciliation::Failed {
                code: CODE_NOT_EXECUTED.to_string(),
                text: format!(
                    "devbridge never sent this receipt to Portos (the journal has no device contact) and Portos has no receipt with externalId {ext} - nothing ran"
                ),
            };
        }
        let query = [
            ("CashRegisterCode".to_string(), code.to_string()),
            ("ExternalId".to_string(), ext.to_string()),
        ];
        let reply = self
            .http
            .get(base, UNPROCESSED_PATH, &query, self.config.timing.lookup)
            .await;
        Reconciliation::StillUnknown(sent_but_not_found_text(
            classify_unprocessed(reply, ext),
            ext,
        ))
    }

    async fn probe(&self, endpoint: &str) -> DeviceState {
        let timeout = self.config.timing.probe;
        let (info, printer): (Reply, Reply) = tokio::join!(
            self.http.get(endpoint, PRODUCT_INFO_PATH, &[], timeout),
            self.http.get(endpoint, PRINTER_STATUS_PATH, &[], timeout),
        );
        probe_state(&info, &printer)
    }
}

#[async_trait]
impl DeviceExecutor for PortosExecutor {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Fiscal
    }

    fn name(&self) -> &str {
        "portos"
    }

    fn check(&self, job: &DeviceJob) -> Result<(), Rejection> {
        let Some(op) = FiscalOp::for_job_type(&job.job_type) else {
            return Err(Rejection {
                code: CODE_UNSUPPORTED,
                text: format!(
                    "the Portos executor has no operation for job type '{}' - Portos was not contacted, nothing ran",
                    job.job_type
                ),
            });
        };
        if job.kasa_id.is_none() {
            return Err(Rejection {
                code: CODE_NO_KASA,
                text:
                    "the job names no till (kasa) - devbridge cannot choose its Portos, nothing ran"
                        .to_string(),
            });
        }
        if matches!(op, FiscalOp::Register(_)) && !client_uuid_ok(&job.client_uuid) {
            return Err(Rejection {
                code: CODE_BAD_CLIENT_UUID,
                text: "client_uuid must match [A-Za-z0-9_-]{1,64} (it is part of the receipt's externalId) - Portos was not contacted, nothing ran".to_string(),
            });
        }
        check_payload(op, &job.payload).map_err(|problems| Rejection {
            code: CODE_BAD_PAYLOAD,
            text: format!("{problems} - Portos was not contacted, nothing ran"),
        })
    }

    async fn execute(&self, job: &DeviceJob) -> Outcome {
        match FiscalOp::for_job_type(&job.job_type) {
            Some(FiscalOp::Register(kind)) => self.register(job, kind).await,
            Some(op) => self.control(job, op).await,
            None => Outcome::failed(
                CODE_UNSUPPORTED,
                format!(
                    "the Portos executor has no operation for job type '{}' - nothing ran",
                    job.job_type
                ),
            ),
        }
    }

    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        let job = &entry.job;
        match FiscalOp::for_job_type(&job.job_type) {
            Some(FiscalOp::Register(kind)) => self.reconcile_receipt(entry, kind).await,
            Some(op) => Reconciliation::Failed {
                code: CODE_RETRY_SAFE.to_string(),
                text: retry_safe_text(op),
            },
            None => Reconciliation::StillUnknown(format!(
                "the Portos executor has no operation for job type '{}' - a person must check it",
                job.job_type
            )),
        }
    }

    fn status(&self) -> String {
        worst(&self.states())
    }

    fn endpoints(&self) -> Vec<String> {
        self.config.endpoints()
    }

    fn endpoint(&self, job: &DeviceJob) -> String {
        self.config.url_for(job.kasa_id).to_string()
    }

    async fn refresh_status(&self) {
        for endpoint in self.config.endpoints() {
            let state = self.probe(&endpoint).await;
            if let Some(previous) = self.record_state(&endpoint, state.clone()) {
                warn!(portos = %endpoint, previous = %previous.label(), now = %state.label(), "Portos state changed");
            }
        }
    }
}

#[cfg(test)]
mod tests;
