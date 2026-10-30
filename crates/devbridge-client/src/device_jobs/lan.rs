//! The LAN API's door into the device-job source (#101).
//!
//! A `fiscal.*` job a till sends to the store's LAN API takes the SAME
//! exactly-once path as an Odoo job: the same checks before any device
//! contact ([`validate`] → the executor's own `check`), the journal
//! (`lan_jobs`, `received` committed BEFORE the device is asked), the same
//! per-device serial queue (a LAN job and an Odoo job never drive one Portos
//! at once), the same externalId (a receipt registered here is found by the
//! lookup-before-register when the same `client_uuid` later comes through
//! Odoo `next`) — and it is reported to Odoo afterwards
//! (`report_offline`, the reporter task).
//!
//! A `client_uuid` already journaled on that till is NEVER run again: the
//! same operation (type + payload) gets the stored row back, another one is
//! a `client_uuid_conflict`. So the till's rule is simple: only a final
//! `failed` job or a refusal means "nothing ran"; after anything else (a
//! 5xx, a timeout, no answer) it sends the SAME `client_uuid` again — never
//! the sale under a new one.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use super::Shared;
use super::api::DeviceJob;
use super::executor::{DeviceKind, Outcome, validate};
use super::journal::{JobState, LanInserted, LanJob};
use super::tasks::WorkItem;

/// The one `fiscal.*` type the LAN API refuses: Odoo reports nothing back
/// for it (`report_offline` refuses the type).
const PRINT_TEXT_JOB_TYPE: &str = "fiscal.print_text";
/// Error code: a job type the LAN API does not run (anything but `fiscal.*`,
/// and `fiscal.print_text`).
pub const CODE_LAN_UNSUPPORTED: &str = "unsupported";
/// Error code: the `client_uuid` names another operation on this till.
pub const CODE_CLIENT_UUID_CONFLICT: &str = "client_uuid_conflict";
/// Error code: devbridge could not journal the job — this call started
/// nothing (send the same `client_uuid` again).
pub const CODE_BRIDGE_ERROR: &str = "bridge_error";
/// Error code: the job IS journaled (it runs or ran) but its state could not
/// be read — poll it / send the same `client_uuid` again.
pub const CODE_STATE_UNKNOWN: &str = "state_unknown";

/// One job as the till sends it.
#[derive(Debug, Clone, PartialEq)]
pub struct LanRequest {
    pub kasa_id: i64,
    pub client_uuid: String,
    pub job_type: String,
    pub payload: Value,
    /// Odoo's `pos.order` id when the till knows it (an offline sale has
    /// none yet).
    pub pos_order_id: Option<i64>,
}

impl LanRequest {
    /// The job as the executors see it (`id` 0 until it is journaled).
    pub fn to_job(&self) -> DeviceJob {
        DeviceJob {
            id: 0,
            job_type: self.job_type.clone(),
            client_uuid: self.client_uuid.clone(),
            kasa_id: Some(self.kasa_id),
            kasa_name: String::new(),
            pos_order: self
                .pos_order_id
                .map_or(Value::Null, |id| json!({ "id": id })),
            payload: self.payload.clone(),
            created_at: String::new(),
            claimed_at: String::new(),
        }
    }
}

/// What the LAN API answers a submitted job.
#[derive(Debug, Clone, PartialEq)]
pub enum LanAnswer {
    /// The job's journal row — new, or the stored one of a replay.
    Job { job: Box<LanJob>, replay: bool },
    /// Refused BEFORE journaling: no device was contacted, nothing ran (the
    /// till may correct it and send the same `client_uuid` again).
    Refused { code: String, text: String },
    /// The `client_uuid` already names ANOTHER operation on this till.
    Conflict { job: Box<LanJob> },
    /// devbridge could not journal it — this call started nothing.
    BridgeError(String),
    /// Journaled (it runs or ran), but its state could not be read.
    StateUnknown(String),
}

/// How far a submitted job got before it is answered.
enum Admission {
    /// Answered without (new) journaling: refused, or a conflict.
    Answered(LanAnswer),
    /// In the journal: wait for it.
    Journaled { job_id: i64, replay: bool },
}

/// Is a replayed request the stored operation? Same type and same payload
/// (the `pos.order` id is context, not the operation).
pub fn same_operation(stored: &DeviceJob, request: &LanRequest) -> bool {
    stored.job_type == request.job_type && stored.payload == request.payload
}

/// A LAN call answers at once when the job is final, or its outcome is
/// unknown (`indeterminate` — reconciled later, the till polls); otherwise
/// it waits for the job until `deadline` (inclusive).
pub fn answer_now(state: JobState, now: Instant, deadline: Instant) -> bool {
    state.is_final() || state == JobState::Indeterminate || now >= deadline
}

/// Health of the fiscal side for `GET /v1/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanHealth {
    /// The Portos state the heartbeat reports (`ok`, `offline`, …; `none`
    /// without a fiscal executor).
    pub portos: String,
    /// Final LAN jobs Odoo has not been told about yet.
    pub unreported: i64,
}

/// The LAN API's handle on the running device-job source.
#[derive(Clone)]
pub struct LanJobs {
    shared: Arc<Shared>,
}

impl LanJobs {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// Run `request` (at most once per till + `client_uuid`) and answer with
    /// its journal row once it is settled or `Timing::lan_answer_wait`
    /// passed (the till then polls [`Self::get`]).
    pub async fn submit(&self, request: LanRequest) -> LanAnswer {
        let (job_id, replay) = match self.admit(&request) {
            Ok(Admission::Answered(answer)) => return answer,
            Ok(Admission::Journaled { job_id, replay }) => (job_id, replay),
            Err(e) => {
                let text = format!("{e:#}");
                error!(kasa_id = request.kasa_id, client_uuid = %request.client_uuid, error = %text, "LAN device job could NOT be journaled - this call started nothing");
                return LanAnswer::BridgeError(text);
            }
        };
        match self.wait_settled(job_id).await {
            Ok(job) => LanAnswer::Job {
                job: Box::new(job),
                replay,
            },
            Err(e) => {
                let text = format!("{e:#}");
                error!(job_id, kasa_id = request.kasa_id, client_uuid = %request.client_uuid, error = %text, "LAN device job is journaled but its state could not be read - the till must poll");
                LanAnswer::StateUnknown(text)
            }
        }
    }

    /// Check, journal and queue `request` — or answer it at once. `Err` =
    /// nothing was journaled by this call.
    fn admit(&self, request: &LanRequest) -> Result<Admission> {
        let shared = &self.shared;
        if let Some(stored) = shared
            .journal
            .lan_job(request.kasa_id, &request.client_uuid)?
        {
            return Ok(self.replayed(stored, request));
        }
        let mut job = request.to_job();
        if DeviceKind::for_job_type(&job.job_type) != Some(DeviceKind::Fiscal) {
            warn!(kasa_id = request.kasa_id, client_uuid = %request.client_uuid, job_type = %job.job_type, "LAN device job refused: not a fiscal.* job");
            return Ok(Admission::Answered(LanAnswer::Refused {
                code: CODE_LAN_UNSUPPORTED.to_string(),
                text: format!(
                    "the LAN API runs fiscal.* jobs only ('{}' is not one; card payments need the bank online) - nothing ran",
                    job.job_type
                ),
            }));
        }
        if job.job_type == PRINT_TEXT_JOB_TYPE {
            warn!(kasa_id = request.kasa_id, client_uuid = %request.client_uuid, "LAN device job refused: a printed text comes from Odoo only");
            return Ok(Admission::Answered(LanAnswer::Refused {
                code: CODE_LAN_UNSUPPORTED.to_string(),
                text: format!(
                    "the LAN API does not run {PRINT_TEXT_JOB_TYPE} (Odoo composes that text and sends it over the Odoo channel) - nothing ran"
                ),
            }));
        }
        if let Err(rejection) = validate(&job, &shared.registry) {
            warn!(kasa_id = request.kasa_id, client_uuid = %request.client_uuid, job_type = %job.job_type, code = rejection.code, reason = %rejection.text, "LAN device job refused before any device contact");
            return Ok(Admission::Answered(LanAnswer::Refused {
                code: rejection.code.to_string(),
                text: rejection.text,
            }));
        }
        let job_id = match shared.journal.insert_lan_received(&job)? {
            LanInserted::New(id) => id,
            // the same client_uuid raced in through another call
            LanInserted::Known(stored) => return Ok(self.replayed(*stored, request)),
        };
        job.id = job_id;
        info!(
            job_id,
            job_type = %job.job_type,
            kasa_id = request.kasa_id,
            client_uuid = %job.client_uuid,
            pos_order_id = ?request.pos_order_id,
            money = %job.money(),
            "LAN device job received (journaled)"
        );
        let item = WorkItem::Execute {
            job_id,
            queued_at: Instant::now(),
        };
        let queued = shared
            .queue_for(&job)
            .is_some_and(|queue| queue.send(item).is_ok());
        if !queued {
            // journaled but never queued: failed "nothing ran"; if even that
            // write fails, the row stays `received` and the next start
            // reconciles it (never contacted → not executed)
            match shared.journal.record_outcome(
                job_id,
                &Outcome::failed(
                    CODE_BRIDGE_ERROR,
                    "the devbridge fiscal worker is not running - nothing ran",
                ),
            ) {
                Ok(recorded) => {
                    shared.outcome_recorded();
                    error!(
                        job_id,
                        recorded, "fiscal device worker gone - LAN job failed, nothing ran"
                    );
                }
                Err(e) => {
                    error!(job_id, error = %format!("{e:#}"), "fiscal device worker gone and the LAN job's failure could not be journaled");
                }
            }
        }
        Ok(Admission::Journaled {
            job_id,
            replay: false,
        })
    }

    /// A `client_uuid` already journaled on this till: never run again.
    fn replayed(&self, stored: LanJob, request: &LanRequest) -> Admission {
        let job_id = stored.job_id();
        if !same_operation(&stored.entry.job, request) {
            warn!(job_id, kasa_id = request.kasa_id, client_uuid = %request.client_uuid, stored_type = %stored.entry.job.job_type, job_type = %request.job_type, "LAN device job refused: client_uuid_conflict (another operation under this client_uuid) - NOT executed");
            return Admission::Answered(LanAnswer::Conflict {
                job: Box::new(stored),
            });
        }
        info!(job_id, kasa_id = request.kasa_id, client_uuid = %request.client_uuid, state = stored.state().as_str(), "LAN device job replayed - NOT executed again, the stored outcome is answered");
        Admission::Journaled {
            job_id,
            replay: true,
        }
    }

    /// The journal row of `job_id` once [`answer_now`] says so.
    async fn wait_settled(&self, job_id: i64) -> Result<LanJob> {
        // subscribed BEFORE every read: an outcome between the read and the
        // wait still wakes it
        let mut outcomes = self.shared.outcomes.subscribe();
        let deadline = Instant::now() + self.shared.timing.lan_answer_wait;
        loop {
            let job = self
                .shared
                .journal
                .lan_job_by_id(job_id)?
                .with_context(|| format!("LAN job {job_id} vanished from the journal"))?;
            if answer_now(job.state(), Instant::now(), deadline) {
                return Ok(job);
            }
            // the sender lives in Shared, which this handle keeps alive
            let _ = tokio::time::timeout_at(deadline.into(), outcomes.changed()).await;
        }
    }

    /// The job of till `kasa_id` with `client_uuid` (`GET /v1/jobs/…`).
    pub fn get(&self, kasa_id: i64, client_uuid: &str) -> Result<Option<LanJob>> {
        self.shared.journal.lan_job(kasa_id, client_uuid)
    }

    /// `GET /v1/health`.
    pub fn health(&self) -> Result<LanHealth> {
        Ok(LanHealth {
            portos: self
                .shared
                .registry
                .get(DeviceKind::Fiscal)
                .map_or_else(|| "none".to_string(), |e| e.status()),
            unreported: self.shared.journal.unreported_lan_jobs()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn request(job_type: &str, payload: Value) -> LanRequest {
        LanRequest {
            kasa_id: 3,
            client_uuid: "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a".into(),
            job_type: job_type.into(),
            payload,
            pos_order_id: Some(812),
        }
    }

    #[test]
    fn test_to_job() {
        let r = request("fiscal.cash_register", json!({"amount": 1}));
        let job = r.to_job();
        assert_eq!(job.id, 0);
        assert_eq!(job.job_type, "fiscal.cash_register");
        assert_eq!(job.client_uuid, r.client_uuid);
        assert_eq!(job.kasa_id, Some(3));
        assert_eq!(job.pos_order, json!({"id": 812}));
        assert_eq!(job.payload, json!({"amount": 1}));
        assert!(job.kasa_name.is_empty() && job.created_at.is_empty() && job.claimed_at.is_empty());
        let offline = LanRequest {
            pos_order_id: None,
            ..r
        };
        assert_eq!(offline.to_job().pos_order, Value::Null);
    }

    #[test]
    fn test_same_operation_is_type_and_payload() {
        let r = request("fiscal.cash_register", json!({"amount": 1}));
        let stored = r.to_job();
        assert!(same_operation(&stored, &r));
        let other_order = LanRequest {
            pos_order_id: Some(9),
            ..r.clone()
        };
        assert!(
            same_operation(&stored, &other_order),
            "pos.order is context"
        );
        assert!(!same_operation(
            &stored,
            &request("fiscal.invalid", json!({"amount": 1}))
        ));
        assert!(!same_operation(
            &stored,
            &request("fiscal.cash_register", json!({"amount": 2}))
        ));
    }

    #[test]
    fn test_answer_now() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(90);
        for state in [JobState::Done, JobState::Failed, JobState::Indeterminate] {
            assert!(answer_now(state, t0, later), "{state:?}");
        }
        for state in [JobState::Received, JobState::Executing] {
            assert!(!answer_now(state, t0, later), "{state:?} waits");
            assert!(
                answer_now(state, later, later),
                "{state:?}: due at the deadline"
            );
            assert!(answer_now(state, later + Duration::from_millis(1), later));
        }
        assert_eq!(CODE_LAN_UNSUPPORTED, "unsupported");
        assert_eq!(CODE_CLIENT_UUID_CONFLICT, "client_uuid_conflict");
        assert_eq!(CODE_BRIDGE_ERROR, "bridge_error");
        assert_eq!(CODE_STATE_UNKNOWN, "state_unknown");
    }
}
