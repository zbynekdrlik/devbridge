//! The device seam (#99): a [`DeviceExecutor`] drives ONE kind of store
//! device — the fiscal printer (Portos eKasa, D1) or the card terminal (TIM,
//! D4). #99 ships the trait, the [`ExecutorRegistry`] and the config-gated
//! [`super::test_executor::TestExecutor`]; the real adapters plug in later.
//!
//! Exactly-once contract for implementors:
//! - [`DeviceExecutor::execute`] is called AT MOST ONCE per job (the journal
//!   guarantees it). Return [`Outcome::Failed`] ONLY when the device confirms
//!   nothing ran (refused before the operation, declined card); a timeout, a
//!   lost connection or any error in the middle is [`Outcome::Indeterminate`].
//! - [`DeviceExecutor::reconcile`] asks the device what happened to a job
//!   whose outcome the bridge does not know (Portos receipt search, TIM
//!   previous-transaction result). It must NEVER run the operation itself.
//! - Bound every device call with the adapter's own timeout.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::api::DeviceJob;
use super::journal::JournalEntry;

/// The store device an executor drives; each kind has its own serial queue,
/// so a card payment waiting for a PIN never blocks a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceKind {
    /// eKasa receipts and the cash drawer — `fiscal.*` jobs (Portos).
    Fiscal,
    /// Card payments on the terminal — `card.*` jobs (TIM API).
    Card,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 2] = [DeviceKind::Fiscal, DeviceKind::Card];

    /// The kind a job type needs (`None` = no device kind handles it).
    pub fn for_job_type(job_type: &str) -> Option<Self> {
        if job_type.starts_with("fiscal.") {
            Some(Self::Fiscal)
        } else if job_type.starts_with("card.") {
            Some(Self::Card)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fiscal => "fiscal",
            Self::Card => "card",
        }
    }

    /// Key in the heartbeat's `device_status` (`portos=…; terminal=…`).
    pub fn status_key(self) -> &'static str {
        match self {
            Self::Fiscal => "portos",
            Self::Card => "terminal",
        }
    }
}

impl std::fmt::Display for DeviceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the device did with a job.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It ran; `result` is the device's answer (UID/OKP, authorisation …).
    Done(Value),
    /// The device confirmed that NOTHING ran.
    Failed { code: String, text: String },
    /// Unknown whether it ran — reconciled later, never retried.
    Indeterminate(String),
}

impl Outcome {
    pub fn failed(code: &str, text: impl Into<String>) -> Self {
        Self::Failed {
            code: code.to_string(),
            text: text.into(),
        }
    }

    /// `done` / `failed` / `indeterminate` — the journal state it leads to.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Done(_) => "done",
            Self::Failed { .. } => "failed",
            Self::Indeterminate(_) => "indeterminate",
        }
    }
}

/// The device's answer to "what happened to this job?".
#[derive(Debug, Clone, PartialEq)]
pub enum Reconciliation {
    /// It ran — with this result.
    Done(Value),
    /// It did not run.
    Failed { code: String, text: String },
    /// The device still cannot tell (offline, busy …) — ask again later.
    StillUnknown(String),
}

#[async_trait]
pub trait DeviceExecutor: Send + Sync {
    /// The device this executor drives (one executor per kind).
    fn kind(&self) -> DeviceKind;

    /// Short adapter name for logs (`test`, `portos`, `tim`).
    fn name(&self) -> &str;

    /// Run the job on the device — called at most once per job.
    async fn execute(&self, job: &DeviceJob) -> Outcome;

    /// Read the real outcome of a journaled job from the device.
    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation;

    /// Short device state for the heartbeat (`ok`, `offline`, …).
    fn status(&self) -> String;
}

/// Executors by device kind.
#[derive(Default, Clone)]
pub struct ExecutorRegistry {
    executors: BTreeMap<DeviceKind, Arc<dyn DeviceExecutor>>,
}

impl std::fmt::Debug for ExecutorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: BTreeMap<&str, &str> = self
            .executors
            .iter()
            .map(|(k, e)| (k.as_str(), e.name()))
            .collect();
        f.debug_tuple("ExecutorRegistry").field(&names).finish()
    }
}

impl ExecutorRegistry {
    /// Register `executor` for its kind; a second one for the same kind is a
    /// configuration error.
    pub fn register(&mut self, executor: Arc<dyn DeviceExecutor>) -> anyhow::Result<()> {
        let kind = executor.kind();
        if let Some(existing) = self.executors.get(&kind) {
            anyhow::bail!(
                "two executors for the {kind} device: {} and {}",
                existing.name(),
                executor.name()
            );
        }
        self.executors.insert(kind, executor);
        Ok(())
    }

    pub fn get(&self, kind: DeviceKind) -> Option<Arc<dyn DeviceExecutor>> {
        self.executors.get(&kind).cloned()
    }

    /// Registered `(kind, executor)` pairs, in a stable order.
    pub fn entries(&self) -> Vec<(DeviceKind, Arc<dyn DeviceExecutor>)> {
        self.executors
            .iter()
            .map(|(kind, executor)| (*kind, Arc::clone(executor)))
            .collect()
    }

    /// Registered kinds, in a stable order.
    pub fn kinds(&self) -> Vec<DeviceKind> {
        self.executors.keys().copied().collect()
    }

    /// The heartbeat's `device_status`: `portos=<status>; terminal=<status>`,
    /// `none` for a kind without an executor.
    pub fn device_status(&self) -> String {
        DeviceKind::ALL
            .iter()
            .map(|kind| {
                let status = self
                    .executors
                    .get(kind)
                    .map(|e| e.status())
                    .unwrap_or_else(|| "none".to_string());
                format!("{}={status}", kind.status_key())
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Error code: a job type no registered executor handles.
pub const CODE_UNSUPPORTED: &str = "unsupported";
/// Error code: a `card.purchase` whose amount is not a finite number > 0.
pub const CODE_BAD_AMOUNT: &str = "bad_amount";

/// Why a job is refused before any device contact (acked `failed` — nothing
/// ran, so it is always safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub text: String,
}

/// Decide whether a claimed job may reach a device: the job's own shape
/// first (`bad_amount`), then whether an executor exists (`unsupported`).
pub fn validate(job: &DeviceJob, registry: &ExecutorRegistry) -> Result<DeviceKind, Rejection> {
    if job.job_type == "card.purchase" {
        let amount_ok = job
            .payload
            .get("amount")
            .and_then(Value::as_f64)
            .is_some_and(|a| a.is_finite() && a > 0.0);
        if !amount_ok {
            return Err(Rejection {
                code: CODE_BAD_AMOUNT,
                text: "card.purchase amount must be a finite number > 0 - the terminal was not contacted, nothing ran".to_string(),
            });
        }
    }
    match DeviceKind::for_job_type(&job.job_type).filter(|k| registry.get(*k).is_some()) {
        Some(kind) => Ok(kind),
        None => Err(Rejection {
            code: CODE_UNSUPPORTED,
            text: format!(
                "devbridge has no executor for job type '{}' - no device was contacted, nothing ran",
                job.job_type
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Stub(DeviceKind, &'static str);

    #[async_trait]
    impl DeviceExecutor for Stub {
        fn kind(&self) -> DeviceKind {
            self.0
        }
        fn name(&self) -> &str {
            self.1
        }
        async fn execute(&self, _job: &DeviceJob) -> Outcome {
            Outcome::Done(json!({}))
        }
        async fn reconcile(&self, _entry: &JournalEntry) -> Reconciliation {
            Reconciliation::StillUnknown("stub".into())
        }
        fn status(&self) -> String {
            format!("{}-ok", self.1)
        }
    }

    fn job(job_type: &str, payload: Value) -> DeviceJob {
        DeviceJob {
            id: 1,
            job_type: job_type.into(),
            client_uuid: "u".into(),
            kasa_id: Some(3),
            kasa_name: String::new(),
            pos_order: Value::Null,
            payload,
            created_at: String::new(),
            claimed_at: String::new(),
        }
    }

    fn both() -> ExecutorRegistry {
        let mut r = ExecutorRegistry::default();
        r.register(Arc::new(Stub(DeviceKind::Fiscal, "portos")))
            .unwrap();
        r.register(Arc::new(Stub(DeviceKind::Card, "tim"))).unwrap();
        r
    }

    #[test]
    fn test_kind_for_job_type() {
        for t in [
            "fiscal.cash_register",
            "fiscal.invalid",
            "fiscal.deposit",
            "fiscal.withdraw",
            "fiscal.print_copy",
            "fiscal.open_drawer",
        ] {
            assert_eq!(DeviceKind::for_job_type(t), Some(DeviceKind::Fiscal), "{t}");
        }
        for t in ["card.purchase", "card.reversal", "card.balance"] {
            assert_eq!(DeviceKind::for_job_type(t), Some(DeviceKind::Card), "{t}");
        }
        for t in ["", "fiscal", "card", "display.text", "Fiscal.cash_register"] {
            assert_eq!(DeviceKind::for_job_type(t), None, "{t}");
        }
        assert_eq!(DeviceKind::Fiscal.to_string(), "fiscal");
        assert_eq!(DeviceKind::Card.as_str(), "card");
        assert_eq!(DeviceKind::Fiscal.status_key(), "portos");
        assert_eq!(DeviceKind::Card.status_key(), "terminal");
    }

    #[test]
    fn test_registry_device_status_and_duplicates() {
        assert_eq!(
            ExecutorRegistry::default().device_status(),
            "portos=none; terminal=none"
        );
        let mut card_only = ExecutorRegistry::default();
        card_only
            .register(Arc::new(Stub(DeviceKind::Card, "tim")))
            .unwrap();
        assert_eq!(card_only.device_status(), "portos=none; terminal=tim-ok");
        assert_eq!(card_only.kinds(), vec![DeviceKind::Card]);
        let mut r = both();
        assert_eq!(r.device_status(), "portos=portos-ok; terminal=tim-ok");
        assert_eq!(r.kinds(), vec![DeviceKind::Fiscal, DeviceKind::Card]);
        let names: Vec<(DeviceKind, String)> = r
            .entries()
            .into_iter()
            .map(|(k, e)| (k, e.name().to_string()))
            .collect();
        assert_eq!(
            names,
            vec![
                (DeviceKind::Fiscal, "portos".to_string()),
                (DeviceKind::Card, "tim".to_string())
            ]
        );
        let err = r
            .register(Arc::new(Stub(DeviceKind::Card, "second")))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("two executors for the card device: tim and second"),
            "{err}"
        );
        assert_eq!(r.get(DeviceKind::Card).unwrap().name(), "tim");
        assert!(format!("{r:?}").contains("\"card\": \"tim\""));
    }

    #[test]
    fn test_validate_routes_supported_jobs() {
        let r = both();
        assert_eq!(
            validate(&job("fiscal.cash_register", json!({})), &r),
            Ok(DeviceKind::Fiscal)
        );
        assert_eq!(
            validate(
                &job("card.purchase", json!({"amount": 12.40, "currency": "EUR"})),
                &r
            ),
            Ok(DeviceKind::Card)
        );
        assert_eq!(
            validate(&job("card.purchase", json!({"amount": 0.01})), &r),
            Ok(DeviceKind::Card)
        );
        // only card.purchase is amount-checked by the channel
        assert_eq!(
            validate(&job("card.balance", json!({})), &r),
            Ok(DeviceKind::Card)
        );
    }

    #[test]
    fn test_validate_bad_amount_never_reaches_the_terminal() {
        let r = both();
        for payload in [
            json!({"amount": 0}),
            json!({"amount": -12.4}),
            json!({"amount": "12.40"}),
            json!({"amount": null}),
            json!({"currency": "EUR"}),
            json!([]),
        ] {
            let rej = validate(&job("card.purchase", payload.clone()), &r).unwrap_err();
            assert_eq!(rej.code, CODE_BAD_AMOUNT, "{payload}");
            assert!(rej.text.contains("nothing ran"), "{}", rej.text);
        }
        // bad amount is reported even when no card executor exists
        let rej = validate(
            &job("card.purchase", json!({"amount": -1})),
            &ExecutorRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(rej.code, CODE_BAD_AMOUNT);
    }

    #[test]
    fn test_validate_unsupported_types_and_missing_executors() {
        let r = both();
        let rej = validate(&job("display.text", json!({})), &r).unwrap_err();
        assert_eq!(rej.code, CODE_UNSUPPORTED);
        assert!(rej.text.contains("'display.text'"), "{}", rej.text);
        let empty = ExecutorRegistry::default();
        assert_eq!(
            validate(&job("fiscal.cash_register", json!({})), &empty)
                .unwrap_err()
                .code,
            CODE_UNSUPPORTED
        );
        assert_eq!(
            validate(&job("card.purchase", json!({"amount": 5})), &empty)
                .unwrap_err()
                .code,
            CODE_UNSUPPORTED
        );
    }

    #[test]
    fn test_outcome_helpers() {
        assert_eq!(
            Outcome::failed("declined", "no"),
            Outcome::Failed {
                code: "declined".into(),
                text: "no".into()
            }
        );
        assert_eq!(Outcome::Done(json!({})).label(), "done");
        assert_eq!(Outcome::failed("a", "b").label(), "failed");
        assert_eq!(Outcome::Indeterminate("x".into()).label(), "indeterminate");
    }
}
