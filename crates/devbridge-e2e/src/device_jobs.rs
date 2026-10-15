//! E2E coverage for the Odoo device-job source (issue #99) — eKasa receipts
//! and card payments, executed exactly once.
//!
//! The RAW E2E client (`e2e-raw-client`, `deploy/e2e-setup-client-local.ps1`)
//! runs with a `[client.pokladna]` block written by the REAL installer
//! functions (`Get-DevBridgePokladnaToml`, url = this fake Odoo on the server
//! runner `http://<server>:9231`, the E2E bridge key, `wait_s` 10) plus the
//! E2E-only `test_executor = true` (no real device: a durable test-device log
//! stands in for Portos / the terminal). Its task has a 1-minute keep-alive
//! trigger, so a process that dies is started again.
//!
//! The step hosts a fake Odoo that follows the device-job contract (JSON-RPC
//! 2.0, Bearer key, `next` long-poll claiming a job once, `pending`, `ack`
//! with first-final-wins / `duplicate`, `not_received` refused inside the
//! claim window, `heartbeat`) and asserts:
//! 1. a receipt job runs once on the (test) device and is acked `done`;
//! 2. a `card.purchase` with amount 0 is acked `failed` / `bad_amount` and an
//!    unknown type `failed` / `unsupported` — no device contact;
//! 3. a job this bridge claimed but never received (lost `next` response,
//!    only in `pending`) is acked `not_received`;
//! 4. FORCED RESTART MID-JOB: a `crash` job makes the device record the run
//!    and the client PROCESS exit before the outcome is journaled; the
//!    restarted process reconciles it through the device and acks `done` with
//!    `executions: 1` — never a second execution, never `not_received`;
//! 5. the heartbeat carries the client version and `portos=test; terminal=test`;
//! 6. every `next` asked `wait_s` 10 (the installer value reached the
//!    service), no `pending` ran while a `next` of the live process was in
//!    flight (checked before the crash), every request had the right Bearer
//!    key.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::odoo_source::RAW_DASHBOARD_PORT;

/// Must match `deploy/e2e-setup-client-local.ps1` + `deploy/e2e-setup-server.ps1`.
pub const FAKE_POKLADNA_PORT: u16 = 9231;
pub const E2E_POKLADNA_KEY: &str = "e2e-fake-pokladna-key-devbridge-99";
/// `DEVBRIDGE_POKLADNA_WAIT_S` of the E2E setup.
pub const E2E_POKLADNA_WAIT_S: u64 = 10;
/// The heartbeat status of the two test executors.
pub const TEST_DEVICE_STATUS: &str = "portos=test; terminal=test";

/// The client's device-job loop backs off up to 30 s while this (not yet
/// running) fake Odoo is unreachable — wait well past that.
const FIRST_CONTACT_WAIT: Duration = Duration::from_secs(240);
/// The crashed client comes back via the 1-minute keep-alive trigger.
const RESTART_WAIT: Duration = Duration::from_secs(300);
const CLAIM_WAIT: Duration = Duration::from_secs(90);
const HEARTBEAT_WAIT: Duration = Duration::from_secs(90);
/// Contract: `not_received` is refused until max(cap 25, asked wait) + 20 s.
const NOT_RECEIVED_WINDOW: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Queued,
    Claimed,
    Unknown,
    Done,
    Failed,
}

impl St {
    fn as_str(self) -> &'static str {
        match self {
            St::Queued => "queued",
            St::Claimed => "claimed",
            St::Unknown => "unknown",
            St::Done => "done",
            St::Failed => "failed",
        }
    }
}

struct Job {
    id: i64,
    job_type: String,
    payload: Value,
    state: St,
    /// `None` for a job claimed before this run (outside the window).
    claimed_at: Option<Instant>,
    indeterminate_reported: bool,
    not_received: bool,
}

#[derive(Default)]
struct Fake {
    jobs: BTreeMap<i64, Job>,
    acks: Vec<Value>,
    heartbeats: Vec<Value>,
    next_waits: Vec<u64>,
    pending_at: Vec<Instant>,
    next_in_flight: usize,
    overlap: usize,
    unauthorized: usize,
}

type Shared = Arc<Mutex<Fake>>;

impl Fake {
    fn queue(&mut self, id: i64, job_type: &str, payload: Value) {
        self.jobs.insert(
            id,
            Job {
                id,
                job_type: job_type.into(),
                payload,
                state: St::Queued,
                claimed_at: None,
                indeterminate_reported: false,
                not_received: false,
            },
        );
    }
}

fn job_json(j: &Job, with_payload: bool) -> Value {
    let mut v = json!({
        "id": j.id,
        "type": j.job_type,
        "client_uuid": format!("e2e-{}", j.id),
        "kasa": {"id": 3, "name": "E2E Pokladna"},
        "pos_order": null,
        "created_at": "2026-10-15 09:00:00",
        "claimed_at": "2026-10-15 09:00:01",
    });
    if with_payload {
        v["payload"] = j.payload.clone();
    } else {
        v["state"] = json!(j.state.as_str());
    }
    v
}

fn result(v: Value) -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": v}))
}

fn access_denied() -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 100,
        "message": "Odoo Server Error", "data": {"message": "Access Denied"}}}))
}

fn authorized(st: &Shared, headers: &HeaderMap) -> bool {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {E2E_POKLADNA_KEY}"));
    if !ok {
        st.lock().unwrap().unauthorized += 1;
    }
    ok
}

fn refused(code: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": code}})
}

fn ok_job(j: &Job, duplicate: bool) -> Value {
    json!({"ok": true, "duplicate": duplicate, "job": {"id": j.id, "state": j.state.as_str()}})
}

/// The contract's ack rules for the cases the bridge can produce.
fn apply_ack(s: &mut Fake, p: &Value) -> Value {
    let Some(j) = p["job_id"].as_i64().and_then(|id| s.jobs.get_mut(&id)) else {
        return refused("not_found");
    };
    if j.state == St::Queued {
        return refused("not_claimed");
    }
    let code = p["error_code"].as_str().unwrap_or("");
    let failed = !code.is_empty() || p["error"].as_str().is_some_and(|e| !e.trim().is_empty());
    let is_final = matches!(j.state, St::Done | St::Failed);
    match code {
        "not_received" if is_final => ok_job(j, true),
        "not_received" if j.indeterminate_reported => refused("after_indeterminate"),
        "not_received"
            if j.claimed_at
                .is_some_and(|at| at.elapsed() < NOT_RECEIVED_WINDOW) =>
        {
            refused("too_early")
        }
        "not_received" => {
            j.state = St::Failed;
            j.not_received = true;
            ok_job(j, false)
        }
        _ if is_final => ok_job(j, true),
        "indeterminate" => {
            j.state = St::Unknown;
            j.indeterminate_reported = true;
            ok_job(j, false)
        }
        _ => {
            j.state = if failed { St::Failed } else { St::Done };
            ok_job(j, false)
        }
    }
}

/// Decrements the in-flight `next` count also when the client gives up.
struct InFlight(Shared);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.lock().unwrap().next_in_flight -= 1;
    }
}

async fn next(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    let wait_s = body["params"]["wait_s"].as_u64().unwrap_or(u64::MAX);
    let _guard = {
        let mut s = st.lock().unwrap();
        s.next_waits.push(wait_s);
        if s.next_in_flight > 0 {
            s.overlap += 1;
        }
        s.next_in_flight += 1;
        InFlight(Arc::clone(&st))
    };
    let effective = wait_s.min(25);
    let deadline = Instant::now() + Duration::from_secs(effective);
    loop {
        {
            let mut s = st.lock().unwrap();
            let id = s
                .jobs
                .values()
                .find(|j| j.state == St::Queued)
                .map(|j| j.id);
            if let Some(id) = id {
                let j = s.jobs.get_mut(&id).unwrap();
                j.state = St::Claimed;
                j.claimed_at = Some(Instant::now());
                let job = job_json(j, true);
                return result(json!({"ok": true, "job": job, "wait_s": effective,
                    "kasy": [{"id": 3, "name": "E2E Pokladna"}]}));
            }
        }
        if Instant::now() >= deadline {
            return result(json!({"ok": true, "job": null, "wait_s": effective,
                "kasy": [{"id": 3, "name": "E2E Pokladna"}]}));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn pending(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    let mut s = st.lock().unwrap();
    s.pending_at.push(Instant::now());
    if s.next_in_flight > 0 {
        s.overlap += 1;
    }
    let jobs: Vec<Value> = s
        .jobs
        .values()
        .filter(|j| matches!(j.state, St::Claimed | St::Unknown))
        .map(|j| job_json(j, false))
        .collect();
    result(json!({"ok": true, "jobs": jobs}))
}

async fn ack(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    let p = body["params"].clone();
    let mut s = st.lock().unwrap();
    s.acks.push(p.clone());
    let answer = apply_ack(&mut s, &p);
    result(answer)
}

async fn heartbeat(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if !authorized(&st, &headers) {
        return access_denied();
    }
    st.lock().unwrap().heartbeats.push(body["params"].clone());
    result(json!({"ok": true, "kasy": [{"id": 3, "name": "E2E Pokladna"}]}))
}

/// The acks a job received, in order.
pub fn acks_for(acks: &[Value], id: i64) -> Vec<Value> {
    acks.iter()
        .filter(|a| a["job_id"].as_i64() == Some(id))
        .cloned()
        .collect()
}

/// The FINAL acks of a job (not the `indeterminate` report, not `not_received`).
pub fn final_acks(acks: &[Value], id: i64) -> Vec<Value> {
    acks_for(acks, id)
        .into_iter()
        .filter(|a| {
            !matches!(
                a["error_code"].as_str(),
                Some("indeterminate" | "not_received")
            )
        })
        .collect()
}

/// A job ran once on the test device: its final ack is `done` with the test
/// device's record `executions: 1` via `via` (a re-sent identical ack is
/// fine — the first final ack wins in Odoo), and no `not_received` for it.
pub fn check_ran_once(acks: &[Value], id: i64, via: &str) -> Result<()> {
    let finals = final_acks(acks, id);
    let want = json!({"job_id": id, "result":
        {"device": "test", "job_id": id, "executions": 1, "via": via}});
    ensure!(!finals.is_empty(), "job {id}: no final ack");
    ensure!(
        finals.iter().all(|a| *a == want),
        "job {id}: every final ack must be {want}, got {finals:?}"
    );
    let not_received = acks_for(acks, id)
        .into_iter()
        .filter(|a| a["error_code"] == "not_received")
        .count();
    ensure!(not_received == 0, "job {id} ran but was acked not_received");
    Ok(())
}

/// A job refused before any device contact: one final ack `failed` with
/// `code` whose text says nothing ran.
pub fn check_refused(acks: &[Value], id: i64, code: &str) -> Result<()> {
    let finals = final_acks(acks, id);
    ensure!(
        finals.len() == 1,
        "job {id}: expected ONE final ack, got {finals:?}"
    );
    ensure!(
        finals[0]["error_code"] == code,
        "job {id}: error_code {} != {code}",
        finals[0]["error_code"]
    );
    ensure!(
        finals[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("nothing ran")),
        "job {id}: error text must say nothing ran: {}",
        finals[0]
    );
    Ok(())
}

/// A heartbeat from this client version with both test devices.
pub fn check_heartbeat(hb: &Value, version: &str) -> Result<()> {
    ensure!(
        hb["bridge_version"] == version,
        "heartbeat bridge_version {} != client {version}",
        hb["bridge_version"]
    );
    ensure!(
        hb["device_status"] == TEST_DEVICE_STATUS,
        "heartbeat device_status {} != {TEST_DEVICE_STATUS}",
        hb["device_status"]
    );
    Ok(())
}

/// Job ids unique per run (the client journal never re-runs a known id).
fn run_base_id() -> i64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    (ms % 1_000_000_000) * 10
}

fn script(outcome: &str) -> Value {
    json!({"devbridge_test": {"outcome": outcome}, "amount": 1.5, "currency": "EUR"})
}

async fn wait_until(
    state: &Shared,
    limit: Duration,
    what: &str,
    cond: impl Fn(&Fake) -> bool,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let met = {
            let s = state.lock().unwrap();
            cond(&s)
        };
        if met {
            return Ok(());
        }
        if start.elapsed() > limit {
            let s = state.lock().unwrap();
            bail!(
                "{what}: not within {}s (acks: {:?}, pending calls: {}, next calls: {})",
                limit.as_secs(),
                s.acks,
                s.pending_at.len(),
                s.next_waits.len()
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn raw_status(client: &reqwest::Client, client_host: &str) -> Result<Value> {
    let url = format!("http://{client_host}:{RAW_DASHBOARD_PORT}/api/status");
    client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .with_context(|| format!("GET {url}: not JSON"))
}

/// Step body — see the module docs.
pub async fn test_device_jobs(client: &reqwest::Client, client_host: &str) -> Result<()> {
    let base = run_base_id();
    let state: Shared = Arc::new(Mutex::new(Fake::default()));
    {
        let mut s = state.lock().unwrap();
        s.queue(base + 1, "fiscal.cash_register", script("done"));
        let mut zero = script("done");
        zero["amount"] = json!(0);
        s.queue(base + 2, "card.purchase", zero);
        s.queue(base + 3, "display.message", script("done"));
        // claimed by this bridge earlier, its next response never arrived
        s.queue(base + 4, "card.purchase", script("done"));
        s.jobs.get_mut(&(base + 4)).unwrap().state = St::Claimed;
    }
    let app = Router::new()
        .route("/pokladna/bridge/next", post(next))
        .route("/pokladna/bridge/pending", post(pending))
        .route("/pokladna/bridge/ack", post(ack))
        .route("/pokladna/bridge/heartbeat", post(heartbeat))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", FAKE_POKLADNA_PORT))
        .await
        .with_context(|| format!("bind fake Odoo (pokladna) on port {FAKE_POKLADNA_PORT}"))?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let outcome = verify(client, client_host, &state, base).await;
    server.abort();
    outcome?;
    println!("PASS");
    Ok(())
}

async fn verify(
    client: &reqwest::Client,
    client_host: &str,
    state: &Shared,
    base: i64,
) -> Result<()> {
    // 1-3. the first four jobs answered
    wait_until(
        state,
        FIRST_CONTACT_WAIT,
        "first four device jobs answered",
        |s| {
            (1..=3).all(|n| !final_acks(&s.acks, base + n).is_empty())
                && s.jobs[&(base + 4)].not_received
        },
    )
    .await?;
    {
        let s = state.lock().unwrap();
        check_ran_once(&s.acks, base + 1, "execute")?;
        check_refused(&s.acks, base + 2, "bad_amount")?;
        check_refused(&s.acks, base + 3, "unsupported")?;
        ensure!(
            s.jobs[&(base + 4)].state == St::Failed,
            "lost job not failed as not_received"
        );
    }
    println!(
        "  receipt ran once (done), bad_amount + unsupported refused, lost job acked not_received"
    );

    // The loop discipline is checked on the live process: after the crash the
    // dead process's long-poll stays "in flight" on this fake until its wait
    // ends, which would look like an overlap with the restarted one.
    {
        let s = state.lock().unwrap();
        ensure!(
            s.overlap == 0,
            "pending ran while a next was in flight ({}x)",
            s.overlap
        );
    }

    // 4. forced restart mid-job
    let crash = base + 5;
    state
        .lock()
        .unwrap()
        .queue(crash, "fiscal.cash_register", script("crash"));
    wait_until(state, CLAIM_WAIT, "crash job claimed", |s| {
        s.jobs[&crash].state != St::Queued
    })
    .await?;
    let claimed = Instant::now();
    wait_until(
        state,
        RESTART_WAIT,
        "crash job reconciled after the restart",
        |s| !final_acks(&s.acks, crash).is_empty(),
    )
    .await?;
    let status = raw_status(client, client_host).await?;
    let uptime = status["uptime_secs"]
        .as_u64()
        .context("RAW client /api/status without uptime_secs")?;
    let since_claim = claimed.elapsed().as_secs() + 1;
    ensure!(
        uptime <= since_claim,
        "the RAW client did not restart: uptime {uptime}s, crash job claimed {since_claim}s ago"
    );
    let version = status["version"]
        .as_str()
        .context("RAW client /api/status without version")?
        .to_string();
    {
        let s = state.lock().unwrap();
        check_ran_once(&s.acks, crash, "reconcile")?;
        ensure!(
            s.pending_at.iter().any(|at| *at > claimed),
            "the restarted client never called pending"
        );
    }
    println!(
        "  forced restart mid-job: client back (uptime {uptime}s), job reconciled done, executions 1"
    );

    // 5. heartbeat of the restarted client
    wait_until(
        state,
        HEARTBEAT_WAIT,
        "heartbeat with both test devices",
        |s| {
            s.heartbeats
                .iter()
                .any(|hb| check_heartbeat(hb, &version).is_ok())
        },
    )
    .await?;

    // 6. wait_s from the installer, loop discipline, auth
    let s = state.lock().unwrap();
    ensure!(
        !s.next_waits.is_empty() && s.next_waits.iter().all(|w| *w == E2E_POKLADNA_WAIT_S),
        "next must ask wait_s {E2E_POKLADNA_WAIT_S} (installer value): {:?}",
        s.next_waits
    );
    ensure!(
        s.unauthorized == 0,
        "{} request(s) without the right Bearer key",
        s.unauthorized
    );
    println!(
        "  heartbeat {TEST_DEVICE_STATUS} v{version}, {} next calls all wait_s {E2E_POKLADNA_WAIT_S}, {} pending calls",
        s.next_waits.len(),
        s.pending_at.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(id: i64, via: &str, executions: i64) -> Value {
        json!({"job_id": id, "result": {"device": "test", "job_id": id, "executions": executions, "via": via}})
    }

    #[test]
    fn test_final_acks_skip_reports_and_not_received() {
        let acks = vec![
            json!({"job_id": 1, "error_code": "indeterminate", "error": "x"}),
            done(1, "reconcile", 1),
            json!({"job_id": 2, "error_code": "not_received", "error": "x"}),
            done(3, "execute", 1),
        ];
        assert_eq!(final_acks(&acks, 1), vec![done(1, "reconcile", 1)]);
        assert!(final_acks(&acks, 2).is_empty());
        assert_eq!(acks_for(&acks, 2).len(), 1);
        assert_eq!(final_acks(&acks, 3).len(), 1);
    }

    #[test]
    fn test_check_ran_once() {
        let ok = vec![done(5, "reconcile", 1)];
        assert!(check_ran_once(&ok, 5, "reconcile").is_ok());
        assert!(check_ran_once(&ok, 5, "execute").is_err());
        let twice = vec![done(5, "execute", 1), done(5, "reconcile", 2)];
        assert!(check_ran_once(&twice, 5, "execute").is_err());
        let resent = vec![done(5, "execute", 1), done(5, "execute", 1)];
        assert!(check_ran_once(&resent, 5, "execute").is_ok());
        let second_run = vec![done(5, "reconcile", 2)];
        assert!(check_ran_once(&second_run, 5, "reconcile").is_err());
        let with_lost = vec![
            json!({"job_id": 5, "error_code": "not_received", "error": "x"}),
            done(5, "execute", 1),
        ];
        assert!(check_ran_once(&with_lost, 5, "execute").is_err());
        assert!(check_ran_once(&[], 5, "execute").is_err());
    }

    #[test]
    fn test_check_refused() {
        let acks = vec![json!({"job_id": 7, "error_code": "bad_amount",
            "error": "card.purchase amount must be ... nothing ran"})];
        assert!(check_refused(&acks, 7, "bad_amount").is_ok());
        assert!(check_refused(&acks, 7, "unsupported").is_err());
        let vague = vec![json!({"job_id": 7, "error_code": "bad_amount", "error": "no"})];
        assert!(check_refused(&vague, 7, "bad_amount").is_err());
        assert!(check_refused(&[done(7, "execute", 1)], 7, "bad_amount").is_err());
    }

    #[test]
    fn test_check_heartbeat() {
        let hb = json!({"bridge_version": "0.8.45", "device_status": TEST_DEVICE_STATUS});
        assert!(check_heartbeat(&hb, "0.8.45").is_ok());
        assert!(check_heartbeat(&hb, "0.8.44").is_err());
        let none =
            json!({"bridge_version": "0.8.45", "device_status": "portos=none; terminal=none"});
        assert!(check_heartbeat(&none, "0.8.45").is_err());
    }

    #[test]
    fn test_ack_rules_of_the_fake() {
        let mut f = Fake::default();
        f.queue(1, "fiscal.cash_register", json!({}));
        assert_eq!(
            apply_ack(&mut f, &json!({"job_id": 1}))["error"]["code"],
            "not_claimed"
        );
        assert_eq!(
            apply_ack(&mut f, &json!({"job_id": 9}))["error"]["code"],
            "not_found"
        );
        f.jobs.get_mut(&1).unwrap().state = St::Claimed;
        f.jobs.get_mut(&1).unwrap().claimed_at = Some(Instant::now());
        let early = apply_ack(&mut f, &json!({"job_id": 1, "error_code": "not_received"}));
        assert_eq!(early["error"]["code"], "too_early");
        let r = apply_ack(
            &mut f,
            &json!({"job_id": 1, "error_code": "indeterminate", "error": "x"}),
        );
        assert_eq!(r["job"]["state"], "unknown");
        let after = apply_ack(&mut f, &json!({"job_id": 1, "error_code": "not_received"}));
        assert_eq!(after["error"]["code"], "after_indeterminate");
        let done = apply_ack(&mut f, &json!({"job_id": 1, "result": {}}));
        assert_eq!(done["job"]["state"], "done");
        assert_eq!(done["duplicate"], false);
        let again = apply_ack(
            &mut f,
            &json!({"job_id": 1, "error": "late", "error_code": "x"}),
        );
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["job"]["state"], "done");
        // a job claimed before this run: not_received accepted at once
        f.queue(2, "card.purchase", json!({}));
        f.jobs.get_mut(&2).unwrap().state = St::Claimed;
        let lost = apply_ack(
            &mut f,
            &json!({"job_id": 2, "error_code": "not_received", "error": "x"}),
        );
        assert_eq!(lost["job"]["state"], "failed");
        assert!(f.jobs[&2].not_received);
    }

    #[test]
    fn test_job_json_shapes() {
        let mut f = Fake::default();
        f.queue(4, "card.purchase", json!({"amount": 2}));
        let j = &f.jobs[&4];
        let next = job_json(j, true);
        assert_eq!(next["payload"], json!({"amount": 2}));
        assert!(next.get("state").is_none());
        let pending = job_json(j, false);
        assert!(pending.get("payload").is_none());
        assert_eq!(pending["state"], "queued");
        assert_eq!(pending["client_uuid"], "e2e-4");
    }

    #[test]
    fn test_run_base_id_leaves_room_for_job_offsets() {
        let a = run_base_id();
        assert!(a >= 0 && a % 10 == 0);
    }
}
