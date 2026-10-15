//! Fake Odoo (device-job contract), scripted devices and the harness.

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
use devbridge_client::device_jobs::journal::{Journal, JournalEntry};
use devbridge_client::device_jobs::test_executor::{SCRIPT_KEY, TestDevice, TestExecutor};
use devbridge_client::device_jobs::{DeviceJobSource, Timing};
use devbridge_core::config::PokladnaClientConfig;

const KEY: &str = "fake-pokladna-bridge-key-99";
const LONG: Duration = Duration::from_secs(10);

// ── fake Odoo ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St {
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
pub struct FJob {
    pub id: i64,
    pub job_type: String,
    pub payload: Value,
    pub state: St,
    pub claimed_at: Option<Instant>,
    pub indeterminate_reported: bool,
    pub not_received: bool,
    pub ran_after_not_received: bool,
    pub result: Option<Value>,
    pub error_code: Option<String>,
}

#[derive(Default)]
pub struct Fake {
    pub jobs: BTreeMap<i64, FJob>,
    /// Every ack's params, in arrival order (also the 502-answered ones).
    pub acks: Vec<Value>,
    pub heartbeats: Vec<Value>,
    pub next_calls: usize,
    pub next_waits: Vec<u64>,
    pub pending_calls: usize,
    pub pending_at: Vec<Instant>,
    pub next_started: Vec<Instant>,
    pub next_in_flight: usize,
    /// A `pending` or a second `next` while a `next` was in flight.
    pub overlap: usize,
    pub unauthorized: usize,
    /// `not_received` is refused (`too_early`) this long after the claim.
    pub too_early_window: Duration,
    /// Answer every `next` with `wait_s: 0` without waiting (no free slot).
    pub force_wait_zero: bool,
    /// Claim a job, then answer the call with HTTP 502 (lost response).
    pub lose_next_responses: usize,
    /// Never answer `next` (client timeout test).
    pub hang_next: bool,
    /// Answer the next N acks with HTTP 502 (not stored).
    pub fail_acks: usize,
    /// Store the next N acks, then answer HTTP 502 (the response is lost).
    pub lose_ack_responses: usize,
    /// Answer every pending call with a top-level JSON-RPC error.
    pub pending_rpc_error: bool,
    /// Refuse every final / indeterminate ack with this code.
    pub refuse_acks: Option<&'static str>,
    /// Answer acks of these jobs with a top-level JSON-RPC error.
    pub rpc_error_acks_for: Vec<i64>,
}

pub type Shared = Arc<Mutex<Fake>>;

impl Fake {
    pub fn queue(&mut self, id: i64, job_type: &str, payload: Value) {
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
    pub fn claimed(&mut self, id: i64, job_type: &str, payload: Value) {
        self.queue(id, job_type, payload);
        let j = self.jobs.get_mut(&id).unwrap();
        j.state = St::Claimed;
        j.claimed_at = Some(Instant::now());
    }

    pub fn acks_for(&self, id: i64) -> Vec<Value> {
        self.acks
            .iter()
            .filter(|a| a["job_id"].as_i64() == Some(id))
            .cloned()
            .collect()
    }
}

pub fn job_json(j: &FJob, with_payload: bool) -> Value {
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

pub fn rpc_result(result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": null, "result": result})).into_response()
}

pub fn refused(code: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": format!("refused: {code}")}})
}

pub fn ok_job(j: &FJob, duplicate: bool) -> Value {
    let mut v = json!({"ok": true, "job": {"id": j.id, "state": j.state.as_str()}});
    if duplicate {
        v["duplicate"] = json!(true);
    }
    v
}

pub fn auth(st: &Shared, headers: &HeaderMap) -> Option<Response> {
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

pub fn params(body: &Value) -> Value {
    assert_eq!(body["jsonrpc"], "2.0", "JSON-RPC 2.0 envelope: {body}");
    assert_eq!(body["method"], "call", "{body}");
    body["params"].clone()
}

/// Decrements the in-flight `next` count also when the client gives up.
pub struct InFlight(pub Shared);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.lock().unwrap().next_in_flight -= 1;
    }
}

pub async fn next(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
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

pub async fn pending(
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
pub fn apply_ack(s: &mut Fake, p: &Value) -> Value {
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

pub async fn ack(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
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

pub async fn heartbeat(
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

pub async fn start_fake(state: Shared) -> String {
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
pub struct GateDevice {
    pub kind: DeviceKind,
    pub gate: Arc<Semaphore>,
    pub started: Arc<Mutex<Vec<i64>>>,
    pub finished: Arc<Mutex<Vec<i64>>>,
    pub running: AtomicUsize,
    pub max_running: Arc<AtomicUsize>,
    pub status: Arc<Mutex<String>>,
}

impl GateDevice {
    pub fn new(kind: DeviceKind) -> Arc<Self> {
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

pub fn timing() -> Timing {
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

pub fn config(url: &str, wait_s: u64) -> PokladnaClientConfig {
    PokladnaClientConfig {
        enabled: true,
        url: url.into(),
        api_key: KEY.into(),
        wait_s,
        heartbeat_interval_secs: 30,
        ..Default::default()
    }
}

pub fn test_registry(device: &Arc<TestDevice>) -> ExecutorRegistry {
    let mut r = ExecutorRegistry::default();
    for kind in DeviceKind::ALL {
        r.register(Arc::new(TestExecutor::new(kind, Arc::clone(device))))
            .unwrap();
    }
    r
}

pub fn script(outcome: &str) -> Value {
    json!({SCRIPT_KEY: {"outcome": outcome}, "amount": 12.40, "currency": "EUR"})
}

pub struct Running {
    pub shutdown: CancellationToken,
    pub handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(LONG, self.handle)
            .await
            .expect("source stops on shutdown")
            .unwrap()
            .unwrap();
    }
}

pub fn start(
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

pub async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < LONG, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn new_fake() -> Shared {
    Arc::new(Mutex::new(Fake::default()))
}

pub fn final_state(fake: &Shared, id: i64) -> St {
    fake.lock().unwrap().jobs[&id].state
}

pub fn assert_clean(fake: &Shared) {
    let s = fake.lock().unwrap();
    assert_eq!(
        s.overlap, 0,
        "pending / next ran while a next was in flight"
    );
    assert_eq!(s.unauthorized, 0, "a request without the right Bearer key");
}

pub fn open_files(dir: &Path) -> (Arc<Journal>, Arc<TestDevice>) {
    (
        Arc::new(Journal::open(&dir.join("device-journal.db")).unwrap()),
        Arc::new(TestDevice::open(&dir.join("test-device.db")).unwrap()),
    )
}
