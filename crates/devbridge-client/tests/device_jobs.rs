//! Integration tests for the Odoo device-job source (#99) against a fake Odoo
//! that implements the bridge side of the device-job contract (odoo-erp
//! `addons/company_pokladna_api/docs/device_job_api.md`): JSON-RPC 2.0
//! envelope, Bearer key, `{"ok": false, "error": {code, message}}`
//! refusals, a long-polling `next` that claims a job at most once, `pending`
//! (claimed + unknown, no payload) and an `ack` that follows the contract's
//! tables — first final ack wins (`duplicate`), `not_received` refused with
//! `too_early` inside the claim window and `after_indeterminate` after an
//! `indeterminate` report, late acks after `unknown`.
//!
//! The devices are the external systems replaced here: the library's own
//! `test` executor (with its durable device log, as in E2E) and a gated
//! executor for the serial-queue tests. Every fake also records whether a
//! `pending` (or a second `next`) ever arrived while a `next` was in flight.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use devbridge_client::device_jobs::api::DeviceJob;
use devbridge_client::device_jobs::executor::{
    DeviceExecutor, DeviceKind, ExecutorRegistry, Outcome, Reconciliation,
};
use devbridge_client::device_jobs::journal::{JobState, Journal, JournalEntry, Report};
use devbridge_client::device_jobs::test_executor::{SCRIPT_KEY, TestDevice, TestExecutor};
use devbridge_client::device_jobs::{DeviceJobSource, Timing};
use devbridge_core::config::PokladnaClientConfig;

const KEY: &str = "fake-pokladna-bridge-key-99";
const LONG: Duration = Duration::from_secs(10);

// ── fake Odoo ──────────────────────────────────────────────────────────────

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

#[derive(Debug, Clone)]
struct FJob {
    id: i64,
    job_type: String,
    payload: Value,
    state: St,
    claimed_at: Option<Instant>,
    indeterminate_reported: bool,
    not_received: bool,
    ran_after_not_received: bool,
    result: Option<Value>,
    error_code: Option<String>,
}

#[derive(Default)]
struct Fake {
    jobs: BTreeMap<i64, FJob>,
    /// Every ack's params, in arrival order (also the 502-answered ones).
    acks: Vec<Value>,
    heartbeats: Vec<Value>,
    next_calls: usize,
    next_waits: Vec<u64>,
    pending_calls: usize,
    pending_at: Vec<Instant>,
    next_started: Vec<Instant>,
    next_in_flight: usize,
    /// A `pending` or a second `next` while a `next` was in flight.
    overlap: usize,
    unauthorized: usize,
    /// `not_received` is refused (`too_early`) this long after the claim.
    too_early_window: Duration,
    /// Answer every `next` with `wait_s: 0` without waiting (no free slot).
    force_wait_zero: bool,
    /// Claim a job, then answer the call with HTTP 502 (lost response).
    lose_next_responses: usize,
    /// Never answer `next` (client timeout test).
    hang_next: bool,
    /// Answer the next N acks with HTTP 502 (not stored).
    fail_acks: usize,
    /// Store the next N acks, then answer HTTP 502 (the response is lost).
    lose_ack_responses: usize,
    /// Answer every pending call with a top-level JSON-RPC error.
    pending_rpc_error: bool,
    /// Refuse every final / indeterminate ack with this code.
    refuse_acks: Option<&'static str>,
    /// Answer acks of these jobs with a top-level JSON-RPC error.
    rpc_error_acks_for: Vec<i64>,
}

type Shared = Arc<Mutex<Fake>>;

impl Fake {
    fn queue(&mut self, id: i64, job_type: &str, payload: Value) {
        self.jobs.insert(
            id,
            FJob {
                id,
                job_type: job_type.into(),
                payload,
                state: St::Queued,
                claimed_at: None,
                indeterminate_reported: false,
                not_received: false,
                ran_after_not_received: false,
                result: None,
                error_code: None,
            },
        );
    }

    /// A job this bridge claimed earlier (a lost response / a previous process).
    fn claimed(&mut self, id: i64, job_type: &str, payload: Value) {
        self.queue(id, job_type, payload);
        let j = self.jobs.get_mut(&id).unwrap();
        j.state = St::Claimed;
        j.claimed_at = Some(Instant::now());
    }

    fn acks_for(&self, id: i64) -> Vec<Value> {
        self.acks
            .iter()
            .filter(|a| a["job_id"].as_i64() == Some(id))
            .cloned()
            .collect()
    }
}

fn job_json(j: &FJob, with_payload: bool) -> Value {
    let mut v = json!({
        "id": j.id,
        "type": j.job_type,
        "state": j.state.as_str(),
        "client_uuid": format!("uuid-{}", j.id),
        "kasa": {"id": 3, "name": "Pokladňa Bastion 1"},
        "pos_order": {"id": 812, "name": "Bastion/0042", "pos_reference": "Order 00003-001-0042"},
        "created_at": "2026-10-15 09:12:05",
        "claimed_at": "2026-10-15 09:12:06",
    });
    if with_payload {
        v["payload"] = j.payload.clone();
        v.as_object_mut().unwrap().remove("state");
    }
    v
}

fn rpc_result(result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": result})).into_response()
}

fn refused(code: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": format!("refused: {code}")}})
}

fn ok_job(j: &FJob, duplicate: bool) -> Value {
    let mut v = json!({"ok": true, "job": {"id": j.id, "state": j.state.as_str()}});
    if duplicate {
        v["duplicate"] = json!(true);
    }
    v
}

fn auth(st: &Shared, headers: &HeaderMap) -> Option<Response> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {KEY}"));
    if ok {
        return None;
    }
    st.lock().unwrap().unauthorized += 1;
    Some(
        Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 100,
            "message": "Odoo Server Error", "data": {"message": "Access Denied"}}}))
        .into_response(),
    )
}

fn params(body: &Value) -> Value {
    assert_eq!(body["jsonrpc"], "2.0", "JSON-RPC 2.0 envelope: {body}");
    assert_eq!(body["method"], "call", "{body}");
    body["params"].clone()
}

/// Decrements the in-flight `next` count also when the client gives up.
struct InFlight(Shared);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.lock().unwrap().next_in_flight -= 1;
    }
}

async fn next(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = auth(&st, &headers) {
        return r;
    }
    let wait_s = params(&body)["wait_s"]
        .as_u64()
        .expect("next wait_s must be a non-negative integer");
    let hang = {
        let mut s = st.lock().unwrap();
        s.next_calls += 1;
        s.next_waits.push(wait_s);
        s.next_started.push(Instant::now());
        s.hang_next
    };
    if hang {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    let _guard = {
        let mut s = st.lock().unwrap();
        if s.next_in_flight > 0 {
            s.overlap += 1;
        }
        s.next_in_flight += 1;
        InFlight(Arc::clone(&st))
    };
    let effective = if st.lock().unwrap().force_wait_zero {
        0
    } else {
        wait_s.min(25)
    };
    let deadline = Instant::now() + Duration::from_secs(effective);
    loop {
        let claimed = {
            let mut s = st.lock().unwrap();
            let id = s
                .jobs
                .values()
                .find(|j| j.state == St::Queued)
                .map(|j| j.id);
            id.map(|id| {
                let j = s.jobs.get_mut(&id).unwrap();
                j.state = St::Claimed;
                j.claimed_at = Some(Instant::now());
                let job = job_json(j, true);
                let lose = s.lose_next_responses > 0;
                if lose {
                    s.lose_next_responses -= 1;
                }
                (job, lose)
            })
        };
        if let Some((job, lose)) = claimed {
            if lose {
                return (StatusCode::BAD_GATEWAY, "proxy lost the response").into_response();
            }
            return rpc_result(json!({"ok": true, "job": job, "wait_s": effective,
                "kasy": [{"id": 3, "name": "Pokladňa Bastion 1"}]}));
        }
        if Instant::now() >= deadline {
            return rpc_result(json!({"ok": true, "job": null, "wait_s": effective,
                "kasy": [{"id": 3, "name": "Pokladňa Bastion 1"}]}));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

async fn pending(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = auth(&st, &headers) {
        return r;
    }
    assert_eq!(params(&body), json!({}));
    let mut s = st.lock().unwrap();
    s.pending_calls += 1;
    s.pending_at.push(Instant::now());
    if s.next_in_flight > 0 {
        s.overlap += 1;
    }
    if s.pending_rpc_error {
        return Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 200,
            "message": "Odoo Server Error", "data": {"message": "boom"}}}))
        .into_response();
    }
    let jobs: Vec<Value> = s
        .jobs
        .values()
        .filter(|j| matches!(j.state, St::Claimed | St::Unknown))
        .map(|j| job_json(j, false))
        .collect();
    rpc_result(json!({"ok": true, "jobs": jobs}))
}

/// The contract's ack rules (incl. the late-ack table after `not_received`).
fn apply_ack(s: &mut Fake, p: &Value) -> Value {
    let Some(id) = p["job_id"].as_i64() else {
        return refused("bad_request");
    };
    let window = s.too_early_window;
    let Some(j) = s.jobs.get_mut(&id) else {
        return refused("not_found");
    };
    if j.state == St::Queued {
        return refused("not_claimed");
    }
    let code = p["error_code"].as_str().map(str::to_string);
    let error = p["error"].as_str().filter(|e| !e.trim().is_empty());
    if code.as_deref() == Some("not_received") {
        if j.not_received || matches!(j.state, St::Done | St::Failed) {
            return ok_job(j, true);
        }
        if j.indeterminate_reported {
            return refused("after_indeterminate");
        }
        if j.claimed_at.is_some_and(|at| at.elapsed() < window) {
            return refused("too_early");
        }
        j.state = St::Failed;
        j.not_received = true;
        j.error_code = code;
        return ok_job(j, false);
    }
    if j.not_received {
        // late ack after not_received: recorded, the state stays failed
        if (code.is_none() && error.is_none()) || code.as_deref() == Some("indeterminate") {
            j.ran_after_not_received = true;
        }
        return ok_job(j, true);
    }
    if code.as_deref() == Some("indeterminate") {
        if matches!(j.state, St::Done | St::Failed) {
            return ok_job(j, true);
        }
        j.state = St::Unknown;
        j.indeterminate_reported = true;
        return ok_job(j, false);
    }
    if matches!(j.state, St::Done | St::Failed) {
        return ok_job(j, true); // the first final ack wins
    }
    if code.is_some() || error.is_some() {
        j.state = St::Failed;
        j.error_code = Some(code.unwrap_or_else(|| "bridge_error".into()));
    } else {
        j.state = St::Done;
    }
    j.result = p.get("result").cloned();
    ok_job(j, false)
}

async fn ack(State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = auth(&st, &headers) {
        return r;
    }
    let p = params(&body);
    let mut s = st.lock().unwrap();
    s.acks.push(p.clone());
    if s.fail_acks > 0 {
        s.fail_acks -= 1;
        return (StatusCode::BAD_GATEWAY, "upstream down").into_response();
    }
    if p["job_id"]
        .as_i64()
        .is_some_and(|id| s.rpc_error_acks_for.contains(&id))
    {
        return Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": 200,
            "message": "Odoo Server Error", "data": {"message": "MissingError"}}}))
        .into_response();
    }
    if let Some(code) = s.refuse_acks.filter(|_| p["error_code"] != "not_received") {
        return rpc_result(refused(code));
    }
    let answer = apply_ack(&mut s, &p);
    if s.lose_ack_responses > 0 {
        s.lose_ack_responses -= 1;
        return (StatusCode::BAD_GATEWAY, "response lost").into_response();
    }
    rpc_result(answer)
}

async fn heartbeat(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = auth(&st, &headers) {
        return r;
    }
    st.lock().unwrap().heartbeats.push(params(&body));
    rpc_result(json!({"ok": true, "kasy": [{"id": 3, "name": "Pokladňa Bastion 1"}]}))
}

async fn start_fake(state: Shared) -> String {
    let app = Router::new()
        .route("/pokladna/bridge/next", post(next))
        .route("/pokladna/bridge/pending", post(pending))
        .route("/pokladna/bridge/ack", post(ack))
        .route("/pokladna/bridge/heartbeat", post(heartbeat))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ── devices ────────────────────────────────────────────────────────────────

/// A device that blocks every job until the test releases it.
struct GateDevice {
    kind: DeviceKind,
    gate: Arc<Semaphore>,
    started: Arc<Mutex<Vec<i64>>>,
    finished: Arc<Mutex<Vec<i64>>>,
    running: AtomicUsize,
    max_running: Arc<AtomicUsize>,
    status: Arc<Mutex<String>>,
}

impl GateDevice {
    fn new(kind: DeviceKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            gate: Arc::new(Semaphore::new(0)),
            started: Arc::default(),
            finished: Arc::default(),
            running: AtomicUsize::new(0),
            max_running: Arc::default(),
            status: Arc::new(Mutex::new("ok".into())),
        })
    }
}

#[async_trait]
impl DeviceExecutor for GateDevice {
    fn kind(&self) -> DeviceKind {
        self.kind
    }
    fn name(&self) -> &str {
        "gate"
    }
    async fn execute(&self, job: &DeviceJob) -> Outcome {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(now, Ordering::SeqCst);
        self.started.lock().unwrap().push(job.id);
        self.gate.acquire().await.unwrap().forget();
        self.finished.lock().unwrap().push(job.id);
        self.running.fetch_sub(1, Ordering::SeqCst);
        Outcome::Done(json!({"gate": job.id}))
    }
    async fn reconcile(&self, _entry: &JournalEntry) -> Reconciliation {
        Reconciliation::StillUnknown("gate device cannot tell".into())
    }
    fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
}

// ── harness ────────────────────────────────────────────────────────────────

fn timing() -> Timing {
    Timing {
        short_poll_sleep: Duration::from_millis(100),
        min_long_poll: Duration::from_millis(5),
        next_timeout_margin: Duration::from_secs(2),
        call_timeout: Duration::from_secs(2),
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        pending_retry: Duration::from_millis(100),
        reconcile_interval: Duration::from_millis(50),
        execute_guard: Duration::from_secs(5),
        // long on purpose: every ack must be woken by its outcome, not by the
        // safety tick
        ack_idle_tick: Duration::from_secs(30),
        status_check: Duration::from_millis(20),
    }
}

fn config(url: &str, wait_s: u64) -> PokladnaClientConfig {
    PokladnaClientConfig {
        enabled: true,
        url: url.into(),
        api_key: KEY.into(),
        wait_s,
        heartbeat_interval_secs: 30,
        ..Default::default()
    }
}

fn test_registry(device: &Arc<TestDevice>) -> ExecutorRegistry {
    let mut r = ExecutorRegistry::default();
    for kind in DeviceKind::ALL {
        r.register(Arc::new(TestExecutor::new(kind, Arc::clone(device))))
            .unwrap();
    }
    r
}

fn script(outcome: &str) -> Value {
    json!({SCRIPT_KEY: {"outcome": outcome}, "amount": 12.40, "currency": "EUR"})
}

struct Running {
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(LONG, self.handle)
            .await
            .expect("source stops on shutdown")
            .unwrap()
            .unwrap();
    }
}

fn start(
    cfg: PokladnaClientConfig,
    journal: Arc<Journal>,
    registry: ExecutorRegistry,
    timing: Timing,
) -> Running {
    let source = DeviceJobSource::new(cfg, journal, registry, timing).unwrap();
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(source.run(shutdown.clone()));
    Running { shutdown, handle }
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < LONG, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn new_fake() -> Shared {
    Arc::new(Mutex::new(Fake::default()))
}

fn final_state(fake: &Shared, id: i64) -> St {
    fake.lock().unwrap().jobs[&id].state
}

fn assert_clean(fake: &Shared) {
    let s = fake.lock().unwrap();
    assert_eq!(
        s.overlap, 0,
        "pending / next ran while a next was in flight"
    );
    assert_eq!(s.unauthorized, 0, "a request without the right Bearer key");
}

fn open_files(dir: &Path) -> (Arc<Journal>, Arc<TestDevice>) {
    (
        Arc::new(Journal::open(&dir.join("device-journal.db")).unwrap()),
        Arc::new(TestDevice::open(&dir.join("test-device.db")).unwrap()),
    )
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_job_runs_once_and_is_acked_done_with_heartbeat() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(101, "fiscal.cash_register", script("done"));

    eventually("job 101 done in Odoo", || {
        final_state(&fake, 101) == St::Done
    })
    .await;
    eventually("journal acked", || {
        journal.get(101).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    eventually("a heartbeat", || {
        !fake.lock().unwrap().heartbeats.is_empty()
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.acks_for(101),
        vec![json!({"job_id": 101, "result":
            {"device": "test", "job_id": 101, "executions": 1, "via": "execute"}})]
    );
    assert_eq!(device.runs(101).unwrap(), 1);
    assert_eq!(
        s.jobs[&101].result,
        Some(json!({"device": "test", "job_id": 101, "executions": 1, "via": "execute"}))
    );
    let e = journal.get(101).unwrap().unwrap();
    assert_eq!(e.state, JobState::Done);
    assert_eq!(e.ack_note.as_deref(), Some("ok"));
    assert_eq!(e.job.client_uuid, "uuid-101");
    assert_eq!(e.job.kasa_id, Some(3));
    assert_eq!(
        s.heartbeats[0],
        json!({"bridge_version": env!("CARGO_PKG_VERSION"),
               "device_status": "portos=test; terminal=test"})
    );
    assert!(s.next_waits.iter().all(|w| *w == 1), "{:?}", s.next_waits);
    assert!(s.pending_calls >= 1, "pending runs at start");
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_invalid_and_unsupported_jobs_are_refused_without_device_contact() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    // only a card executor: fiscal jobs have none
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(TestExecutor::new(
            DeviceKind::Card,
            Arc::clone(&device),
        )))
        .unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        let mut zero = script("done");
        zero["amount"] = json!(0);
        s.queue(201, "card.purchase", zero);
        let mut text = script("done");
        text["amount"] = json!("12.40");
        s.queue(202, "card.purchase", text);
        s.queue(203, "display.text", script("done"));
        s.queue(204, "fiscal.cash_register", script("done"));
    }
    eventually("all four refused", || {
        (201..=204).all(|id| final_state(&fake, id) == St::Failed)
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    for (id, code) in [
        (201, "bad_amount"),
        (202, "bad_amount"),
        (203, "unsupported"),
        (204, "unsupported"),
    ] {
        let acks = s.acks_for(id);
        assert_eq!(acks.len(), 1, "{id}: {acks:?}");
        assert_eq!(acks[0]["error_code"], code, "{id}");
        assert_eq!(s.jobs[&id].error_code.as_deref(), Some(code), "{id}");
        assert!(
            acks[0]["error"].as_str().unwrap().contains("nothing ran"),
            "{acks:?}"
        );
        assert_eq!(device.runs(id).unwrap(), 0, "{id} reached a device");
        assert_eq!(journal.get(id).unwrap().unwrap().state, JobState::Failed);
    }
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_crash_after_received_restart_reconciles_through_the_device_never_executes() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    // The previous process journaled the claimed job and died before the
    // device was touched.
    fake.lock()
        .unwrap()
        .claimed(301, "fiscal.cash_register", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&301],
        true,
    ))
    .unwrap();
    journal.insert_received(&job).unwrap();
    drop(journal);

    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("job 301 resolved", || final_state(&fake, 301) == St::Failed).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(301);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "not_executed");
    assert_eq!(
        device.runs(301).unwrap(),
        0,
        "never executed after the restart"
    );
    assert_eq!(journal.get(301).unwrap().unwrap().state, JobState::Failed);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_crash_mid_job_restart_reconciles_without_a_second_execution() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let crashed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&crashed);
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(
            TestExecutor::new(DeviceKind::Fiscal, Arc::clone(&device))
                .with_crash_hook(Arc::new(move |id: i64| seen.lock().unwrap().push(id))),
        ))
        .unwrap();
    let first = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock()
        .unwrap()
        .queue(401, "fiscal.cash_register", script("crash"));
    eventually("the device ran job 401 and the process 'died'", || {
        crashed.lock().unwrap().contains(&401)
    })
    .await;
    assert_eq!(
        journal.get(401).unwrap().unwrap().state,
        JobState::Executing
    );
    first.stop().await;
    drop(journal);
    drop(device);
    // the dead process's long-poll stays open on the fake until its 1 s wait
    // ends - let it, so the restart's pending is not counted as an overlap
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(fake.lock().unwrap().acks_for(401).is_empty());

    // The restarted process: fresh handles on the same files.
    let (journal, device) = open_files(dir.path());
    let second = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("job 401 reconciled done", || {
        final_state(&fake, 401) == St::Done
    })
    .await;
    eventually("journal acked", || {
        journal.get(401).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    second.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.acks_for(401),
        vec![json!({"job_id": 401, "result":
            {"device": "test", "job_id": 401, "executions": 1, "via": "reconcile"}})]
    );
    assert_eq!(device.runs(401).unwrap(), 1, "exactly one execution");
    assert!(!s.jobs[&401].ran_after_not_received);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_lost_next_response_is_acked_not_received_after_too_early() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        let mut s = fake.lock().unwrap();
        // two responses lost: both jobs are too_early in the same pending
        // round, both must be asked again
        s.lose_next_responses = 2;
        s.too_early_window = Duration::from_millis(400);
        s.queue(501, "card.purchase", script("done"));
        s.queue(503, "fiscal.cash_register", script("done"));
    }
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("jobs 501 + 503 failed as not_received", || {
        let s = fake.lock().unwrap();
        s.jobs[&501].not_received && s.jobs[&503].not_received
    })
    .await;
    // keeps working afterwards
    fake.lock()
        .unwrap()
        .queue(502, "card.purchase", script("done"));
    eventually("job 502 done", || final_state(&fake, 502) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(501);
    assert!(
        acks.len() >= 2,
        "first not_received must be too_early: {acks:?}"
    );
    assert!(
        acks.iter().all(|a| a["error_code"] == "not_received"),
        "{acks:?}"
    );
    for id in [501, 503] {
        assert_eq!(device.runs(id).unwrap(), 0, "a lost job never runs");
        assert!(
            journal.get(id).unwrap().is_none(),
            "never journaled = never received"
        );
    }
    assert!(s.acks_for(503).len() >= 2, "{:?}", s.acks_for(503));
    assert_eq!(device.runs(502).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_executor_past_the_guard_is_reported_indeterminate_then_acked_late() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let mut t = timing();
    t.execute_guard = Duration::from_millis(150);
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        t,
    );
    fake.lock().unwrap().queue(
        601,
        "card.purchase",
        json!({SCRIPT_KEY: {"outcome": "done", "delay_ms": 700}, "amount": 5, "currency": "EUR"}),
    );
    eventually("reported indeterminate", || {
        fake.lock().unwrap().jobs[&601].state == St::Unknown
    })
    .await;
    eventually("late done", || final_state(&fake, 601) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(601);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(
        acks[1],
        json!({"job_id": 601, "result":
            {"device": "test", "job_id": 601, "executions": 1, "via": "execute"}})
    );
    assert_eq!(device.runs(601).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_indeterminate_outcome_is_reconciled_through_the_device_and_acked_late() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(701, "card.purchase", script("indeterminate"));
    eventually("late done", || final_state(&fake, 701) == St::Done).await;
    run.stop().await;

    let s = fake.lock().unwrap();
    let acks = s.acks_for(701);
    // the indeterminate report goes out at once; the reconcile (next tick)
    // only rarely beats it, then the late ack alone is owed
    assert!(acks.len() <= 2, "{acks:?}");
    if acks.len() == 2 {
        assert_eq!(acks[0]["error_code"], "indeterminate", "{acks:?}");
    }
    assert_eq!(
        acks.last().unwrap(),
        &json!({"job_id": 701, "result":
            {"device": "test", "job_id": 701, "executions": 1, "via": "reconcile"}})
    );
    assert_eq!(device.runs(701).unwrap(), 1);
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_ack_transport_errors_are_retried_and_a_duplicate_is_success() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        let mut s = fake.lock().unwrap();
        // 1st ack: 502, not stored; 2nd: stored but its response lost (502);
        // 3rd: Odoo answers duplicate = accepted.
        s.fail_acks = 1;
        s.lose_ack_responses = 1;
    }
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(801, "fiscal.cash_register", script("done"));
    eventually("ack accepted after retries", || {
        journal.get(801).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let acks = s.acks_for(801);
    assert_eq!(acks.len(), 3, "{acks:?}");
    assert!(
        acks.windows(2).all(|w| w[0] == w[1]),
        "the same ack is retried: {acks:?}"
    );
    assert_eq!(s.jobs[&801].state, St::Done);
    assert_eq!(
        journal.get(801).unwrap().unwrap().ack_note.as_deref(),
        Some("duplicate")
    );
    assert_eq!(device.runs(801).unwrap(), 1, "re-acking never re-executes");
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_wait_zero_reply_sleeps_between_next_calls() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().force_wait_zero = true;
    let run = start(
        config(&url, 25),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    run.stop().await;
    let s = fake.lock().unwrap();
    // 100 ms short-poll sleep: ~8 calls in 800 ms, never a tight loop
    assert!(
        (3..=12).contains(&s.next_calls),
        "next calls in 800 ms: {}",
        s.next_calls
    );
    assert!(s.next_waits.iter().all(|w| *w == 25), "asks its wait_s");
    let gaps: Vec<Duration> = s.next_started.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= Duration::from_millis(90)),
        "{gaps:?}"
    );
    drop(s);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_next_http_timeout_is_wait_plus_margin_then_pending_before_next() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().hang_next = true;
    let mut t = timing();
    t.next_timeout_margin = Duration::from_millis(300);
    let run = start(
        config(&url, 0),
        Arc::clone(&journal),
        test_registry(&device),
        t,
    );
    eventually("a second pending after the timed-out next", || {
        fake.lock().unwrap().pending_calls >= 2
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let next_at = s.next_started[0];
    let pending_after = s.pending_at[1];
    let waited = pending_after - next_at;
    // wait_s 0 + margin 300 ms: never shorter (a late job would lose its
    // response), and the pending only after the call ended
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    assert!(s.pending_at[0] <= next_at, "pending runs first at start");
}

#[tokio::test]
async fn test_card_wait_never_blocks_a_receipt_and_a_device_runs_one_job_at_a_time() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let card = GateDevice::new(DeviceKind::Card);
    let fiscal = GateDevice::new(DeviceKind::Fiscal);
    let mut registry = ExecutorRegistry::default();
    registry.register(card.clone()).unwrap();
    registry.register(fiscal.clone()).unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    {
        let mut s = fake.lock().unwrap();
        s.queue(901, "card.purchase", json!({"amount": 10}));
        s.queue(902, "card.purchase", json!({"amount": 20}));
        s.queue(903, "fiscal.cash_register", json!({}));
    }
    eventually("card 901 and receipt 903 started", || {
        card.started.lock().unwrap().contains(&901) && fiscal.started.lock().unwrap().contains(&903)
    })
    .await;
    fiscal.gate.add_permits(1);
    eventually("receipt done while the card waits", || {
        final_state(&fake, 903) == St::Done
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        *card.started.lock().unwrap(),
        vec![901],
        "card 902 waits for 901"
    );
    assert_eq!(final_state(&fake, 902), St::Claimed);
    card.gate.add_permits(1);
    eventually("card 902 started", || {
        card.started.lock().unwrap().contains(&902)
    })
    .await;
    card.gate.add_permits(1);
    eventually("both cards done", || {
        final_state(&fake, 901) == St::Done && final_state(&fake, 902) == St::Done
    })
    .await;
    run.stop().await;
    assert_eq!(*card.finished.lock().unwrap(), vec![901, 902]);
    assert_eq!(card.max_running.load(Ordering::SeqCst), 1);
    assert_eq!(fiscal.max_running.load(Ordering::SeqCst), 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_heartbeat_at_start_and_on_device_status_change() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let card = GateDevice::new(DeviceKind::Card);
    let mut registry = ExecutorRegistry::default();
    registry.register(card.clone()).unwrap();
    let run = start(config(&url, 1), journal, registry, timing());
    eventually("first heartbeat", || {
        !fake.lock().unwrap().heartbeats.is_empty()
    })
    .await;
    *card.status.lock().unwrap() = "offline".into();
    eventually("heartbeat on change", || {
        fake.lock().unwrap().heartbeats.len() >= 2
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    assert_eq!(s.heartbeats[0]["device_status"], "portos=none; terminal=ok");
    assert_eq!(
        s.heartbeats[1]["device_status"],
        "portos=none; terminal=offline"
    );
    // interval is 30 s: no third one in this short run
    assert_eq!(s.heartbeats.len(), 2);
}

#[tokio::test]
async fn test_unknown_outcome_without_an_executor_is_reported_once_and_never_run() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock()
        .unwrap()
        .claimed(1001, "card.purchase", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1001],
        true,
    ))
    .unwrap();
    journal.insert_received(&job).unwrap();
    journal.start_execution(1001).unwrap();
    // restart with ONLY a fiscal executor: nobody can ask the terminal
    let mut registry = ExecutorRegistry::default();
    registry
        .register(Arc::new(TestExecutor::new(
            DeviceKind::Fiscal,
            Arc::clone(&device),
        )))
        .unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    eventually("reported indeterminate", || {
        fake.lock().unwrap().jobs[&1001].state == St::Unknown
    })
    .await;
    // several reconcile intervals later: still exactly one report
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.stop().await;
    let e = journal.get(1001).unwrap().unwrap();
    assert_eq!(e.state, JobState::Indeterminate);
    assert_eq!(e.report, Report::Sent);
    assert_eq!(e.reconcile_attempts, 1);
    let acks = fake.lock().unwrap().acks_for(1001);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(device.runs(1001).unwrap(), 0);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_journaled_job_handed_out_again_is_never_executed_again() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock()
        .unwrap()
        .queue(1101, "fiscal.cash_register", script("done"));
    let job = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1101],
        true,
    ))
    .unwrap();
    // journaled + executed + acked by this bridge before (Odoo bug re-queues it)
    journal.insert_received(&job).unwrap();
    journal.start_execution(1101).unwrap();
    journal
        .record_outcome(1101, &Outcome::Done(json!({"receipt": "R-1"})))
        .unwrap();
    journal.mark_acked(1101, "ok").unwrap();
    // and one whose final ack we think Odoo stored, but `pending` still lists it
    fake.lock()
        .unwrap()
        .claimed(1102, "card.purchase", script("done"));
    let other = devbridge_client::device_jobs::api::parse_job(&job_json(
        &fake.lock().unwrap().jobs[&1102],
        true,
    ))
    .unwrap();
    journal.insert_received(&other).unwrap();
    journal
        .record_outcome(1102, &Outcome::failed("declined", "zamietnutá"))
        .unwrap();
    journal.mark_acked(1102, "ok").unwrap();
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("the stored outcomes are re-sent", || {
        final_state(&fake, 1101) == St::Done && final_state(&fake, 1102) == St::Failed
    })
    .await;
    run.stop().await;
    assert_eq!(device.runs(1101).unwrap(), 0, "NOT executed again");
    assert_eq!(device.runs(1102).unwrap(), 0, "NOT executed again");
    assert_eq!(
        fake.lock().unwrap().acks_for(1101),
        vec![json!({"job_id": 1101, "result": {"receipt": "R-1"}})]
    );
    assert_eq!(
        fake.lock().unwrap().acks_for(1102),
        vec![json!({"job_id": 1102, "error": "zamietnutá", "error_code": "declined"})]
    );
    assert_clean(&fake);
}

#[tokio::test]
async fn test_no_next_before_pending_succeeds() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().pending_rpc_error = true;
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("pending retried with backoff", || {
        fake.lock().unwrap().pending_calls >= 3
    })
    .await;
    assert_eq!(fake.lock().unwrap().next_calls, 0, "next before pending");
    fake.lock().unwrap().pending_rpc_error = false;
    eventually("next once pending succeeded", || {
        fake.lock().unwrap().next_calls >= 1
    })
    .await;
    run.stop().await;
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_refused_ack_is_journaled_and_not_retried() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().refuse_acks = Some("not_claimed_by_you");
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    fake.lock()
        .unwrap()
        .queue(1301, "fiscal.cash_register", script("done"));
    eventually("refusal journaled", || {
        journal.get(1301).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.stop().await;
    let e = journal.get(1301).unwrap().unwrap();
    assert_eq!(
        e.ack_note.as_deref(),
        Some("refused: Odoo refused (not_claimed_by_you): refused: not_claimed_by_you")
    );
    assert_eq!(
        fake.lock().unwrap().acks_for(1301).len(),
        1,
        "a refusal is final"
    );
    assert_eq!(device.runs(1301).unwrap(), 1);
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_failing_ack_stays_owed_without_blocking_the_others() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    fake.lock().unwrap().rpc_error_acks_for = vec![1401];
    let run = start(
        config(&url, 1),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    {
        let mut s = fake.lock().unwrap();
        s.queue(1401, "fiscal.cash_register", script("done"));
        s.queue(1402, "fiscal.cash_register", script("done"));
    }
    eventually("1402 accepted while 1401 fails", || {
        journal.get(1402).unwrap().is_some_and(|e| e.acked)
            && fake.lock().unwrap().acks_for(1401).len() >= 2
    })
    .await;
    assert!(
        !journal.get(1401).unwrap().unwrap().acked,
        "1401 stays owed"
    );
    fake.lock().unwrap().rpc_error_acks_for.clear();
    eventually("1401 accepted once Odoo recovers", || {
        journal.get(1401).unwrap().is_some_and(|e| e.acked)
    })
    .await;
    run.stop().await;
    assert_eq!(final_state(&fake, 1401), St::Done);
    assert_eq!(
        device.runs(1401).unwrap(),
        1,
        "a failing ack never re-executes"
    );
    assert_clean(&fake);
}

/// A device that cannot tell for its first `unknown_answers` reconciles.
struct SlowToAnswer {
    reconciles: AtomicUsize,
    unknown_answers: usize,
}

#[async_trait]
impl DeviceExecutor for SlowToAnswer {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Card
    }
    fn name(&self) -> &str {
        "slow"
    }
    async fn execute(&self, _job: &DeviceJob) -> Outcome {
        Outcome::Indeterminate("terminal connection lost mid-payment".into())
    }
    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        let n = self.reconciles.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.unknown_answers {
            Reconciliation::StillUnknown(format!("terminal offline ({n})"))
        } else {
            Reconciliation::Done(json!({"auth": format!("A-{}", entry.job.id)}))
        }
    }
    fn status(&self) -> String {
        "ok".into()
    }
}

#[tokio::test]
async fn test_unknown_device_answer_is_asked_again_until_it_answers() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let device = Arc::new(SlowToAnswer {
        reconciles: AtomicUsize::new(0),
        unknown_answers: 2,
    });
    let mut registry = ExecutorRegistry::default();
    registry.register(device.clone()).unwrap();
    let run = start(config(&url, 1), Arc::clone(&journal), registry, timing());
    fake.lock().unwrap().queue(
        1501,
        "card.purchase",
        json!({"amount": 7, "currency": "EUR"}),
    );
    eventually("late done", || final_state(&fake, 1501) == St::Done).await;
    run.stop().await;
    assert_eq!(device.reconciles.load(Ordering::SeqCst), 3);
    let e = journal.get(1501).unwrap().unwrap();
    assert_eq!(e.reconcile_attempts, 2);
    assert_eq!(e.result, Some(json!({"auth": "A-1501"})));
    let acks = fake.lock().unwrap().acks_for(1501);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["error_code"], "indeterminate");
    assert_eq!(
        acks[1],
        json!({"job_id": 1501, "result": {"auth": "A-1501"}})
    );
    assert_clean(&fake);
}

#[tokio::test]
async fn test_a_refused_not_received_is_not_retried() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, device) = open_files(dir.path());
    {
        // claimed by this bridge, reported indeterminate, but missing from
        // the journal: Odoo refuses not_received (after_indeterminate)
        let mut s = fake.lock().unwrap();
        s.claimed(1601, "card.purchase", script("done"));
        let j = s.jobs.get_mut(&1601).unwrap();
        j.state = St::Unknown;
        j.indeterminate_reported = true;
    }
    // wait_s 0: a next round every 100 ms - a retry would show at once
    let run = start(
        config(&url, 0),
        Arc::clone(&journal),
        test_registry(&device),
        timing(),
    );
    eventually("not_received tried", || {
        !fake.lock().unwrap().acks_for(1601).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    run.stop().await;
    let s = fake.lock().unwrap();
    assert_eq!(s.acks_for(1601).len(), 1, "{:?}", s.acks_for(1601));
    assert_eq!(s.jobs[&1601].state, St::Unknown);
    assert!(s.next_calls >= 3, "next keeps running: {}", s.next_calls);
    drop(s);
    assert_clean(&fake);
}
