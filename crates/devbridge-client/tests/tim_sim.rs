//! The TIM executor (#102 D1) with the REAL Worldline TIM C library against
//! the SDK's EftSimulator. The SDK is licensed partner material: this runs
//! LOCALLY on dev1 only, never in CI (design odoo-erp issue 7598 §7,
//! Worldline licence question 4), and never against a store's terminal.
//!
//! `cargo test -p devbridge-client --features tim-sim --test tim_sim -- --test-threads=1`
//! Env (both REQUIRED — a missing one FAILS the run, it never skips):
//! - `DEVBRIDGE_TIM_LIBRARY` — `TimSdk/C/TimApi/Libraries/Unix/64bit/libtimapi.so.3.42.0-9115`
//! - `DEVBRIDGE_TIM_SIM_IP` — the simulator's listen address (`127.0.0.1`)
//! - `DEVBRIDGE_TIM_SIM_XCTL` — the card tests (#102 D2) "present" the card
//!   by XTEST clicks: the path of an `xctl.py click <x> <y>` helper
//!   (`~/vendor/worldline/eft-probe-from-design-lane/xctl.py`); `DISPLAY` /
//!   `XAUTHORITY` of the simulator's Xvfb are inherited
//!
//! The simulator (a Swing app) runs headless:
//! `xvfb-run -a -s "-screen 0 1280x1024x24" java -jar EftSimulator-3.34.0-9115.jar`
//! and listens on `127.0.0.1:7784`. Connect + login need no card action.
#![cfg(feature = "tim-sim")]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use devbridge_client::device_jobs::api::{DeviceJob, parse_job};
use devbridge_client::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use devbridge_client::device_jobs::journal::{JobState, Journal};
use devbridge_client::device_jobs::tim::{RECONNECT_GAP, SessionError, TimExecutor, logs};
use devbridge_core::config::{
    PokladnaClientConfig, PokladnaKasaConfig, TIM_DEFAULT_INTEGRATOR_ID, TimConfig,
};

/// Both tests drive the ONE simulator: never at the same time, also when
/// `--test-threads=1` is forgotten.
static SIMULATOR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The id the EftSimulator reports (design §7, `eft-probe/probe.out`).
const SIM_TERMINAL_ID: &str = "12345678";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set (see the module docs)"))
}

fn kasa(id: i64, ip: &str, tid: Option<&str>) -> PokladnaKasaConfig {
    PokladnaKasaConfig {
        id,
        portos_url: None,
        terminal_ip: Some(ip.into()),
        terminal_tid: tid.map(str::to_string),
        lan_key: None,
    }
}

fn config(kasy: Vec<PokladnaKasaConfig>) -> PokladnaClientConfig {
    PokladnaClientConfig {
        kasy,
        tim: Some(TimConfig {
            library_path: env("DEVBRIDGE_TIM_LIBRARY"),
            integrator_id: TIM_DEFAULT_INTEGRATOR_ID.into(),
            commit_timeout_s: 60,
            rollback_deadline_s: 45,
        }),
        ..Default::default()
    }
}

fn journal() -> Arc<Journal> {
    Arc::new(Journal::open_in_memory().unwrap())
}

fn today_log(data_dir: &Path) -> String {
    let dir = logs::log_dir(data_dir);
    let name = logs::file_name(chrono::Local::now().date_naive());
    std::fs::read_to_string(dir.join(name)).expect("today's TIM log")
}

#[tokio::test]
async fn test_session_on_the_simulator_with_the_real_library() {
    let _simulator = SIMULATOR.lock().await;
    let ip = env("DEVBRIDGE_TIM_SIM_IP");
    let data = tempfile::tempdir().unwrap();
    // another test's executor may have just disconnected from the simulator
    tokio::time::sleep(RECONNECT_GAP).await;
    let tim = TimExecutor::start(
        &config(vec![kasa(3, &ip, Some(SIM_TERMINAL_ID))]),
        data.path(),
        journal(),
        None,
    );
    assert_eq!(tim.status(), "tim-ok", "library loaded, no failed session");

    let settings =
        std::fs::read_to_string(logs::log_dir(data.path()).join(logs::SETTINGS_FILE)).unwrap();
    assert!(settings.contains("pos_id = 3\n"), "{settings}");
    assert!(settings.contains("library_settings = "), "{settings}");
    assert!(
        settings.contains(TIM_DEFAULT_INTEGRATOR_ID),
        "the library's dump names the integrator ID: {settings}"
    );

    let report = tim
        .session(3)
        .await
        .expect("connect + login + logout + disconnect");
    assert_eq!(report.terminal_id.as_deref(), Some(SIM_TERMINAL_ID));
    assert_eq!(tim.status(), "tim-ok");

    // the library's own records reached today's file (TIM 3.42 publishes
    // up to FINE; every level is written)
    let log = today_log(data.path());
    assert!(log.contains(" FINE ["), "no FINE record in the TIM log");
    assert!(log.contains("ta_terminal_connect"), "the connect is logged");
    assert!(
        log.contains("<sixml:PosId>3</sixml:PosId>"),
        "the POS-ID travels in the SIXml login"
    );

    // a second session works: nothing was held after the first, and the
    // executor waits the reconnect gap itself
    let started = std::time::Instant::now();
    tim.session(3).await.expect("second per-use session");
    assert!(started.elapsed() >= RECONNECT_GAP - std::time::Duration::from_millis(100));
}

#[tokio::test]
async fn test_wrong_or_unreachable_terminal_is_offline() {
    let _simulator = SIMULATOR.lock().await;
    let ip = env("DEVBRIDGE_TIM_SIM_IP");
    let data = tempfile::tempdir().unwrap();
    // another test's executor may have just disconnected from the simulator
    tokio::time::sleep(RECONNECT_GAP).await;
    // 127.0.0.2: nothing listens there -> connect refused
    let tim = TimExecutor::start(
        &config(vec![
            kasa(3, &ip, Some("99999999")),
            kasa(4, "127.0.0.2", None),
        ]),
        data.path(),
        journal(),
        None,
    );
    match tim.session(3).await {
        Err(SessionError::WrongTerminal { expected, reported }) => {
            assert_eq!(expected, "99999999");
            assert_eq!(reported.as_deref(), Some(SIM_TERMINAL_ID));
        }
        other => panic!("expected WrongTerminal, got {other:?}"),
    }
    assert_eq!(tim.status(), "offline");
    match tim.session(4).await {
        Err(SessionError::Tim(e)) => assert_eq!(e.call, "ta_terminal_connect", "{e}"),
        other => panic!("expected a connect failure, got {other:?}"),
    }
    // the simulator is free again after the wrong-terminal session (a new
    // executor does not know the old one's last session: wait the gap)
    tokio::time::sleep(RECONNECT_GAP).await;
    let ok = TimExecutor::start(
        &config(vec![kasa(3, &ip, None)]),
        data.path(),
        journal(),
        None,
    );
    ok.session(3)
        .await
        .expect("terminal released after the refused session");
}

// ---- lane D2: card payments on the simulator --------------------------

/// Simulator buttons (1280x1024 Xvfb, the window as it opens, 4.11.2026).
const TAP: (u32, u32) = (725, 607);
const ACCEPT: (u32, u32) = (551, 744);
const DECLINE: (u32, u32) = (499, 776);

fn click((x, y): (u32, u32)) {
    let status = std::process::Command::new("python3")
        .arg(env("DEVBRIDGE_TIM_SIM_XCTL"))
        .args(["click", &x.to_string(), &y.to_string()])
        .status()
        .expect("xctl.py runs");
    assert!(status.success(), "click {x},{y} failed");
}

fn card_job(id: i64, job_type: &str, payload: serde_json::Value) -> DeviceJob {
    parse_job(&serde_json::json!({
        "id": id, "type": job_type, "client_uuid": format!("sim-{id}"),
        "kasa": {"id": 3, "name": "sim"}, "payload": payload,
    }))
    .unwrap()
}

/// A unique job id per run (the simulator keeps its own sequence).
fn job_id() -> i64 {
    chrono::Utc::now().timestamp_millis() % 1_000_000_000 + 1
}

fn executing(journal: &Journal, job: &DeviceJob) {
    journal.insert_received(job).unwrap();
    assert!(journal.start_execution(job.id).unwrap());
}

/// Start a purchase in the background; once the terminal was asked,
/// present the card and press `answer`.
async fn present(
    tim: &Arc<TimExecutor>,
    journal: &Arc<Journal>,
    id: i64,
    cents: i64,
    answer: (u32, u32),
) -> tokio::task::JoinHandle<Outcome> {
    let job = card_job(
        id,
        "card.purchase",
        serde_json::json!({"amount": cents as f64 / 100.0}),
    );
    executing(journal, &job);
    let running = tokio::spawn({
        let tim = Arc::clone(tim);
        async move { tim.execute(&job).await }
    });
    let until = Instant::now() + Duration::from_secs(20);
    while journal
        .get(id)
        .unwrap()
        .unwrap()
        .device_contact_at
        .is_none()
    {
        assert!(
            Instant::now() < until,
            "the purchase never reached the terminal"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    click(TAP);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    click(answer);
    running
}

async fn wait_authorised(journal: &Journal, id: i64) {
    let until = Instant::now() + Duration::from_secs(20);
    while journal
        .card_payment(id)
        .unwrap()
        .and_then(|c| c.authorized_at)
        .is_none()
    {
        assert!(Instant::now() < until, "never authorised");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn card_executor(ip: &str, data: &Path, journal: &Arc<Journal>) -> Arc<TimExecutor> {
    Arc::new(TimExecutor::start(
        &config(vec![kasa(3, ip, Some(SIM_TERMINAL_ID))]),
        data,
        Arc::clone(journal),
        None,
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_purchase_approve_commit_and_rollback_on_the_simulator() {
    let _simulator = SIMULATOR.lock().await;
    let ip = env("DEVBRIDGE_TIM_SIM_IP");
    env("DEVBRIDGE_TIM_SIM_XCTL");
    let data = tempfile::tempdir().unwrap();
    let journal = journal();
    tokio::time::sleep(RECONNECT_GAP).await;
    let tim = card_executor(&ip, data.path(), &journal);

    // approve + commit
    let id = job_id();
    let running = present(&tim, &journal, id, 250, ACCEPT).await;
    wait_authorised(&journal, id).await;
    let auth = journal.card_payment(id).unwrap().unwrap();
    let result = auth.auth_result.clone().unwrap();
    assert_eq!(result["transaction"]["amount"], 2.5);
    assert_eq!(result["transaction"]["currency"], "EUR");
    assert_eq!(result["terminal"]["tid"], SIM_TERMINAL_ID);
    assert!(result["transaction"]["trans_seq"].as_i64().unwrap() > auth.baseline_seq.unwrap());
    assert_eq!(result["card"]["card_number_printable"], "XXXXXXXXXXXX5240");
    assert!(
        result["receipts"]["cardholder"]
            .as_str()
            .unwrap()
            .contains("Cardholder receipt")
    );
    let commit = card_job(
        id + 1,
        "card.commit",
        serde_json::json!({"card_job_id": id}),
    );
    executing(&journal, &commit);
    assert_eq!(tim.execute(&commit).await.label(), "done");
    match running.await.unwrap() {
        Outcome::Done(r) => assert_eq!(r["state"], "committed"),
        other => panic!("{other:?}"),
    }

    // approve + card.rollback
    tokio::time::sleep(RECONNECT_GAP).await;
    let id = job_id();
    let running = present(&tim, &journal, id, 330, ACCEPT).await;
    wait_authorised(&journal, id).await;
    let rollback = card_job(
        id + 1,
        "card.rollback",
        serde_json::json!({"card_job_id": id}),
    );
    executing(&journal, &rollback);
    assert_eq!(tim.execute(&rollback).await.label(), "done");
    match running.await.unwrap() {
        Outcome::Failed { code, .. } => assert_eq!(code, "rolled_back"),
        other => panic!("{other:?}"),
    }
    assert_eq!(tim.status(), "tim-ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_purchase_declined_on_the_simulator() {
    let _simulator = SIMULATOR.lock().await;
    let ip = env("DEVBRIDGE_TIM_SIM_IP");
    env("DEVBRIDGE_TIM_SIM_XCTL");
    let data = tempfile::tempdir().unwrap();
    let journal = journal();
    tokio::time::sleep(RECONNECT_GAP).await;
    let tim = card_executor(&ip, data.path(), &journal);
    let id = job_id();
    let running = present(&tim, &journal, id, 410, DECLINE).await;
    match running.await.unwrap() {
        Outcome::Failed { code, text } => {
            assert_eq!(code, "declined");
            assert!(text.contains("rc 82"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        journal
            .card_payment(id)
            .unwrap()
            .unwrap()
            .authorized_at
            .is_none()
    );
}

/// The child of the kill test: authorise, then die before any commit.
const CHILD_JOURNAL: &str = "DEVBRIDGE_TIM_SIM_CHILD_JOURNAL";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_purchase_killed_before_commit_is_reconciled() {
    if let Ok(path) = std::env::var(CHILD_JOURNAL) {
        // the CHILD: a bridge that dies right after the authorisation
        let journal = Arc::new(Journal::open(Path::new(&path)).unwrap());
        let data = tempfile::tempdir().unwrap();
        let tim = card_executor(&env("DEVBRIDGE_TIM_SIM_IP"), data.path(), &journal);
        let id: i64 = std::env::var("DEVBRIDGE_TIM_SIM_CHILD_ID")
            .unwrap()
            .parse()
            .unwrap();
        let job = card_job(id, "card.purchase", serde_json::json!({"amount": 5.2}));
        executing(&journal, &job);
        let _running = tokio::spawn(async move { tim.execute(&job).await });
        wait_authorised(&journal, id).await;
        std::process::exit(9);
    }
    let _simulator = SIMULATOR.lock().await;
    let ip = env("DEVBRIDGE_TIM_SIM_IP");
    env("DEVBRIDGE_TIM_SIM_XCTL");
    tokio::time::sleep(RECONNECT_GAP).await;
    // the child's journal is empty: the terminal's last transaction must
    // carry a trans_seq (a decline does not) — an approve + rollback first
    {
        let journal = journal();
        let data = tempfile::tempdir().unwrap();
        let tim = card_executor(&ip, data.path(), &journal);
        let id = job_id();
        let running = present(&tim, &journal, id, 110, ACCEPT).await;
        wait_authorised(&journal, id).await;
        let rollback = card_job(
            id + 1,
            "card.rollback",
            serde_json::json!({"card_job_id": id}),
        );
        executing(&journal, &rollback);
        assert_eq!(tim.execute(&rollback).await.label(), "done");
        assert_eq!(running.await.unwrap().label(), "failed");
        tokio::time::sleep(RECONNECT_GAP).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device-journal.db");
    let id = job_id();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "test_purchase_killed_before_commit_is_reconciled",
            "--test-threads=1",
        ])
        .env(CHILD_JOURNAL, &path)
        .env("DEVBRIDGE_TIM_SIM_CHILD_ID", id.to_string())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    while !path.exists()
        || Journal::open(&path)
            .unwrap()
            .get(id)
            .unwrap()
            .and_then(|e| e.device_contact_at)
            .is_none()
    {
        assert!(
            Instant::now() < until,
            "the child never reached the terminal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    click(TAP);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    click(ACCEPT);
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(9),
        "the child died after the authorisation"
    );

    // the restarted bridge: in flight -> indeterminate, reconciled via the terminal
    let journal = Arc::new(Journal::open(&path).unwrap());
    journal.recover_after_restart().unwrap();
    let entry = journal.get(id).unwrap().unwrap();
    assert_eq!(entry.state, JobState::Indeterminate);
    let data = tempfile::tempdir().unwrap();
    let tim = card_executor(&ip, data.path(), &journal);
    assert!(
        matches!(tim.reconcile(&entry).await, Reconciliation::StillUnknown(why) if why.starts_with("waiting until"))
    );
    // the commit timeout (60 s) + 10 s after this executor's start
    tokio::time::sleep(Duration::from_secs(71)).await;
    match tim.reconcile(&entry).await {
        // the simulator models no auto-reversal: the uncommitted
        // authorisation still reads successful (design §2 caveat)
        Reconciliation::Done(result) => {
            assert_eq!(result["state"], "successful");
            assert_eq!(result["transaction"]["amount"], 5.2);
        }
        other => panic!("{other:?}"),
    }
}
