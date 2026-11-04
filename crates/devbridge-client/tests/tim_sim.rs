//! The TIM executor (#102 D1) with the REAL Worldline TIM C library against
//! the SDK's EftSimulator. The SDK is licensed partner material: this runs
//! LOCALLY on dev1 only, never in CI (design odoo-erp issue 7598 §7,
//! Worldline licence question 4), and never against a store's terminal.
//!
//! `cargo test -p devbridge-client --features tim-sim --test tim_sim -- --test-threads=1`
//! Env (both REQUIRED — a missing one FAILS the run, it never skips):
//! - `DEVBRIDGE_TIM_LIBRARY` — `TimSdk/C/TimApi/Libraries/Unix/64bit/libtimapi.so.3.42.0-9115`
//! - `DEVBRIDGE_TIM_SIM_IP` — the simulator's listen address (`127.0.0.1`)
//!
//! The simulator (a Swing app) runs headless:
//! `xvfb-run -a -s "-screen 0 1280x1024x24" java -jar EftSimulator-3.34.0-9115.jar`
//! and listens on `127.0.0.1:7784`. Connect + login need no card action.
#![cfg(feature = "tim-sim")]

use std::path::Path;

use devbridge_client::device_jobs::executor::DeviceExecutor;
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
        }),
        ..Default::default()
    }
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
    );
    assert_eq!(tim.status(), "tim-ok", "library loaded, no failed session");

    let settings =
        std::fs::read_to_string(logs::log_dir(data.path()).join(logs::SETTINGS_FILE)).unwrap();
    assert!(settings.contains("pos_id = KASA-3"), "{settings}");
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
        log.contains("<sixml:PosId>KASA-3</sixml:PosId>"),
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
    let ok = TimExecutor::start(&config(vec![kasa(3, &ip, None)]), data.path());
    ok.session(3)
        .await
        .expect("terminal released after the refused session");
}
