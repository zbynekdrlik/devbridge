//! The `test` executor (#99) — registered by `[client.pokladna]
//! test_executor = true`: the E2E, and a store brought up WITHOUT Portos
//! (installer `DEVBRIDGE_POKLADNA_TEST_EXECUTOR`, #101).
//!
//! It drives NO real device. It stands in for one (fiscal or card) with a
//! durable device log ([`TestDevice`], `test-device.db`) that plays the part
//! of Portos' receipt store / the terminal's last-transaction memory: every
//! run is recorded there, and [`DeviceExecutor::reconcile`] answers from it
//! — exactly what the real adapters will do with the real devices.
//!
//! A job runs only when its payload carries a script,
//! `"devbridge_test": {"outcome": …}`:
//!
//! | outcome | device log | answer |
//! |---|---|---|
//! | `done` (optional `delay_ms`) | run recorded | `Done` with the run count |
//! | `failed` | nothing | `Failed` `declined` (nothing ran) |
//! | `indeterminate` | run recorded | `Indeterminate` → reconcile finds the run |
//! | `crash` | run recorded | the PROCESS exits before the outcome is journaled |
//!
//! Any other job (e.g. a real receipt reaching a misconfigured store) is
//! refused as `test_executor` — nothing ran, which is the truth.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::api::DeviceJob;
use super::executor::{DeviceExecutor, DeviceKind, Outcome, Reconciliation};
use super::journal::JournalEntry;

/// Payload key of the test script.
pub const SCRIPT_KEY: &str = "devbridge_test";
/// Exit code of the simulated crash (`outcome: "crash"`).
pub const CRASH_EXIT_CODE: i32 = 75;
/// Status reported in the heartbeat.
pub const TEST_STATUS: &str = "test";

/// What the `crash` script does after recording the run. The default exits
/// the process; tests inject a hook that returns (the job then never
/// finishes, like a dead process).
pub type CrashHook = Arc<dyn Fn(i64) + Send + Sync>;

/// The simulated device's own durable memory of what it ran.
pub struct TestDevice {
    conn: Mutex<Connection>,
}

impl TestDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open test device log {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS test_device_runs (
                 job_id     INTEGER PRIMARY KEY,
                 job_type   TEXT NOT NULL,
                 executions INTEGER NOT NULL,
                 last_run   TEXT NOT NULL
             );",
        )
        .context("create test_device_runs table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record one run of `job_id`; returns how many times it ran in total.
    pub fn record_run(&self, job_id: i64, job_type: &str) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO test_device_runs (job_id, job_type, executions, last_run)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(job_id) DO UPDATE SET executions = executions + 1, last_run = ?3",
            params![job_id, job_type, Utc::now().to_rfc3339()],
        )
        .with_context(|| format!("test device: record run of job {job_id}"))?;
        conn.query_row(
            "SELECT executions FROM test_device_runs WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )
        .context("test device: read run count")
    }

    /// How many times the device ran `job_id` (0 = never).
    pub fn runs(&self, job_id: i64) -> Result<i64> {
        Ok(self
            .conn()
            .query_row(
                "SELECT executions FROM test_device_runs WHERE job_id = ?1",
                params![job_id],
                |r| r.get(0),
            )
            .optional()
            .context("test device: read run count")?
            .unwrap_or(0))
    }
}

/// The scripted outcome of a test job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    Done { delay: Duration },
    Failed,
    Indeterminate,
    Crash,
}

/// Read the `devbridge_test` script of a payload; `Err` = refuse the job.
pub fn script_of(payload: &Value) -> Result<Script, String> {
    let Some(script) = payload.get(SCRIPT_KEY) else {
        return Err(format!(
            "the test executor runs only jobs with a '{SCRIPT_KEY}' script - no device was contacted, nothing ran"
        ));
    };
    let outcome = script.get("outcome").and_then(Value::as_str).unwrap_or("");
    Ok(match outcome {
        "done" => Script::Done {
            delay: Duration::from_millis(
                script.get("delay_ms").and_then(Value::as_u64).unwrap_or(0),
            ),
        },
        "failed" => Script::Failed,
        "indeterminate" => Script::Indeterminate,
        "crash" => Script::Crash,
        other => {
            return Err(format!(
                "unknown {SCRIPT_KEY} outcome '{other}' - nothing ran"
            ));
        }
    })
}

/// The device's answer for a recorded run.
fn run_result(job_id: i64, executions: i64, via: &str) -> Value {
    json!({"device": "test", "job_id": job_id, "executions": executions, "via": via})
}

fn default_crash_hook() -> CrashHook {
    Arc::new(|job_id: i64| {
        tracing::error!(
            job_id,
            exit_code = CRASH_EXIT_CODE,
            "TEST executor: simulated crash after the device ran the job - the process exits now"
        );
        std::process::exit(CRASH_EXIT_CODE);
    })
}

pub struct TestExecutor {
    kind: DeviceKind,
    device: Arc<TestDevice>,
    crash: CrashHook,
}

impl TestExecutor {
    pub fn new(kind: DeviceKind, device: Arc<TestDevice>) -> Self {
        Self {
            kind,
            device,
            crash: default_crash_hook(),
        }
    }

    /// Replace the process exit of the `crash` script (tests).
    pub fn with_crash_hook(mut self, hook: CrashHook) -> Self {
        self.crash = hook;
        self
    }

    fn record(&self, job: &DeviceJob) -> Result<i64, Outcome> {
        self.device
            .record_run(job.id, &job.job_type)
            .map_err(|e| Outcome::Indeterminate(format!("test device log: {e:#}")))
    }
}

#[async_trait]
impl DeviceExecutor for TestExecutor {
    fn kind(&self) -> DeviceKind {
        self.kind
    }

    fn name(&self) -> &str {
        "test"
    }

    async fn execute(&self, job: &DeviceJob) -> Outcome {
        let script = match script_of(&job.payload) {
            Ok(s) => s,
            Err(text) => return Outcome::failed("test_executor", text),
        };
        match script {
            Script::Failed => {
                Outcome::failed("declined", "test device declined the job - nothing ran")
            }
            Script::Done { delay } => {
                tokio::time::sleep(delay).await;
                match self.record(job) {
                    Ok(n) => Outcome::Done(run_result(job.id, n, "execute")),
                    Err(o) => o,
                }
            }
            Script::Indeterminate => match self.record(job) {
                Ok(_) => Outcome::Indeterminate(
                    "test device: no answer after the job was sent".to_string(),
                ),
                Err(o) => o,
            },
            Script::Crash => {
                if let Err(o) = self.record(job) {
                    return o;
                }
                (self.crash)(job.id);
                // A returning hook = a test: behave like the dead process,
                // never deliver an outcome.
                std::future::pending::<Outcome>().await
            }
        }
    }

    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        match self.device.runs(entry.job.id) {
            Ok(0) => Reconciliation::Failed {
                code: "not_executed".to_string(),
                text: "test device has no record of this job - nothing ran".to_string(),
            },
            Ok(n) => Reconciliation::Done(run_result(entry.job.id, n, "reconcile")),
            Err(e) => Reconciliation::StillUnknown(format!("test device log: {e:#}")),
        }
    }

    fn status(&self) -> String {
        TEST_STATUS.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_jobs::journal::{JobState, Journal};
    use std::sync::atomic::{AtomicI64, Ordering};

    fn job(id: i64, payload: Value) -> DeviceJob {
        DeviceJob {
            id,
            job_type: "fiscal.cash_register".into(),
            client_uuid: "u".into(),
            kasa_id: Some(3),
            kasa_name: String::new(),
            pos_order: Value::Null,
            payload,
            created_at: String::new(),
            claimed_at: String::new(),
        }
    }

    fn entry_for(j: &DeviceJob) -> JournalEntry {
        let journal = Journal::open_in_memory().unwrap();
        journal.insert_received(j).unwrap();
        journal.get(j.id).unwrap().unwrap()
    }

    fn exec() -> (TestExecutor, Arc<TestDevice>) {
        let device = Arc::new(TestDevice::open_in_memory().unwrap());
        (
            TestExecutor::new(DeviceKind::Fiscal, Arc::clone(&device)),
            device,
        )
    }

    #[test]
    fn test_script_parsing() {
        assert_eq!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "done"}})),
            Ok(Script::Done {
                delay: Duration::ZERO
            })
        );
        assert_eq!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "done", "delay_ms": 250}})),
            Ok(Script::Done {
                delay: Duration::from_millis(250)
            })
        );
        assert_eq!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "failed"}})),
            Ok(Script::Failed)
        );
        assert_eq!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "indeterminate"}})),
            Ok(Script::Indeterminate)
        );
        assert_eq!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "crash"}})),
            Ok(Script::Crash)
        );
        assert!(
            script_of(&json!({"amount": 5}))
                .unwrap_err()
                .contains("nothing ran")
        );
        assert!(
            script_of(&json!({SCRIPT_KEY: {"outcome": "boom"}}))
                .unwrap_err()
                .contains("'boom'")
        );
        assert!(script_of(&json!({SCRIPT_KEY: {}})).is_err());
    }

    #[tokio::test]
    async fn test_job_without_script_is_refused_and_never_recorded() {
        let (e, device) = exec();
        let out = e
            .execute(&job(1, json!({"amount": 12.4, "currency": "EUR"})))
            .await;
        match out {
            Outcome::Failed { code, text } => {
                assert_eq!(code, "test_executor");
                assert!(text.contains("nothing ran"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(device.runs(1).unwrap(), 0);
    }

    #[tokio::test]
    async fn test_done_failed_indeterminate_scripts() {
        let (e, device) = exec();
        let done = e
            .execute(&job(1, json!({SCRIPT_KEY: {"outcome": "done"}})))
            .await;
        assert_eq!(
            done,
            Outcome::Done(
                json!({"device": "test", "job_id": 1, "executions": 1, "via": "execute"})
            )
        );
        assert_eq!(device.runs(1).unwrap(), 1);

        let failed = e
            .execute(&job(2, json!({SCRIPT_KEY: {"outcome": "failed"}})))
            .await;
        assert_eq!(failed.label(), "failed");
        assert_eq!(device.runs(2).unwrap(), 0);

        let unknown = e
            .execute(&job(3, json!({SCRIPT_KEY: {"outcome": "indeterminate"}})))
            .await;
        assert_eq!(unknown.label(), "indeterminate");
        assert_eq!(device.runs(3).unwrap(), 1);

        let refused = e
            .execute(&job(4, json!({SCRIPT_KEY: {"outcome": "nope"}})))
            .await;
        assert_eq!(refused.label(), "failed");
        assert_eq!(e.kind(), DeviceKind::Fiscal);
        assert_eq!(e.name(), "test");
        assert_eq!(e.status(), "test");
    }

    #[tokio::test]
    async fn test_reconcile_reads_the_device_log() {
        let (e, device) = exec();
        let ran = job(5, json!({SCRIPT_KEY: {"outcome": "indeterminate"}}));
        e.execute(&ran).await;
        assert_eq!(
            e.reconcile(&entry_for(&ran)).await,
            Reconciliation::Done(
                json!({"device": "test", "job_id": 5, "executions": 1, "via": "reconcile"})
            )
        );
        let never = job(6, json!({SCRIPT_KEY: {"outcome": "done"}}));
        match e.reconcile(&entry_for(&never)).await {
            Reconciliation::Failed { code, .. } => assert_eq!(code, "not_executed"),
            other => panic!("{other:?}"),
        }
        assert_eq!(device.runs(6).unwrap(), 0);
        assert_eq!(entry_for(&never).state, JobState::Received);
    }

    #[tokio::test]
    async fn test_crash_records_the_run_then_calls_the_hook_and_never_answers() {
        let crashed = Arc::new(AtomicI64::new(0));
        let seen = Arc::clone(&crashed);
        let device = Arc::new(TestDevice::open_in_memory().unwrap());
        let e = TestExecutor::new(DeviceKind::Card, Arc::clone(&device))
            .with_crash_hook(Arc::new(move |id: i64| seen.store(id, Ordering::SeqCst)));
        let j = job(9, json!({SCRIPT_KEY: {"outcome": "crash"}}));
        let r = tokio::time::timeout(Duration::from_millis(100), e.execute(&j)).await;
        assert!(r.is_err(), "a crashed job never delivers an outcome");
        assert_eq!(crashed.load(Ordering::SeqCst), 9);
        assert_eq!(device.runs(9).unwrap(), 1);
    }

    #[test]
    fn test_device_log_counts_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-device.db");
        {
            let d = TestDevice::open(&path).unwrap();
            assert_eq!(d.record_run(1, "card.purchase").unwrap(), 1);
            assert_eq!(d.record_run(1, "card.purchase").unwrap(), 2);
        }
        let d = TestDevice::open(&path).unwrap();
        assert_eq!(d.runs(1).unwrap(), 2);
        assert_eq!(d.runs(2).unwrap(), 0);
    }
}
