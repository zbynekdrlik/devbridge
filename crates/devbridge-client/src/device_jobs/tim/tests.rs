//! Unit tests of the TIM executor over a fake TIM library (CI — no SDK).

use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate, TimeZone};
use serde_json::{Value, json};

pub(super) use super::fake::{FakeTim, executor, job, spec};
use super::logs::{self, Housekeeping};
use super::*;
use crate::device_jobs::executor::{CODE_UNSUPPORTED, ExecutorRegistry, validate};
use crate::device_jobs::journal::Journal;
use devbridge_core::config::{PokladnaKasaConfig, TIM_DEFAULT_INTEGRATOR_ID, TimConfig};

#[test]
fn test_reconnect_wait() {
    let gap = Duration::from_secs(2);
    let t0 = Instant::now();
    assert_eq!(reconnect_wait(None, t0, gap), Duration::ZERO);
    assert_eq!(reconnect_wait(Some(t0), t0, gap), gap);
    assert_eq!(
        reconnect_wait(Some(t0), t0 + Duration::from_millis(500), gap),
        Duration::from_millis(1500)
    );
    assert_eq!(reconnect_wait(Some(t0), t0 + gap, gap), Duration::ZERO);
    assert_eq!(reconnect_wait(Some(t0), t0 + gap * 2, gap), Duration::ZERO);
    assert_eq!(RECONNECT_GAP, gap);
}

#[tokio::test]
async fn test_sessions_on_one_terminal_keep_the_reconnect_gap() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let gap = Duration::from_millis(300);
    let tim = TimExecutor::with_backend(
        vec![spec(3, "10.78.2.8", None)],
        Ok(Arc::new(fake.clone())),
        dir.path(),
    )
    .with_reconnect_gap(gap);
    let started = Instant::now();
    tim.session(3).await.unwrap();
    let first = started.elapsed();
    assert!(first < gap, "the first session does not wait: {first:?}");
    tim.session(3).await.unwrap();
    assert!(
        started.elapsed() >= gap,
        "second connect waited the gap: {:?}",
        started.elapsed()
    );
}

#[test]
fn test_session_call_order_and_tid_check() {
    let fake = FakeTim::failing(&[]);
    let report = run_session(&fake, &spec(3, "10.78.2.8", Some("12345678"))).unwrap();
    assert_eq!(report.terminal_id.as_deref(), Some("12345678"));
    assert_eq!(
        fake.calls(),
        [
            "create",
            "pos_id 3",
            "connect",
            "login",
            "terminal_id",
            "logout",
            "disconnect",
            "dispose"
        ]
    );

    let wrong = FakeTim::failing(&[]);
    let err = run_session(&wrong, &spec(3, "10.78.2.8", Some("87654321"))).unwrap_err();
    assert_eq!(
        err,
        SessionError::WrongTerminal {
            expected: "87654321".into(),
            reported: Some("12345678".into())
        }
    );
    assert!(
        err.to_string()
            .contains("reported id 12345678 but the till expects 87654321")
    );
    assert!(
        wrong
            .calls()
            .ends_with(&["logout".to_string(), "disconnect".into(), "dispose".into()]),
        "a wrong terminal is still released: {:?}",
        wrong.calls()
    );
}

#[test]
fn test_session_releases_the_terminal_after_any_failure() {
    // connect fails: nothing to log out of or disconnect, only disposed
    let fake = FakeTim::failing(&["connect"]);
    let err = run_session(&fake, &spec(3, "ip", None)).unwrap_err();
    assert_eq!(
        err,
        SessionError::Tim(TimError {
            call: "connect",
            code: 8
        })
    );
    assert_eq!(fake.calls(), ["create", "pos_id 3", "connect", "dispose"]);

    // login fails: disconnect, no logout
    let fake = FakeTim::failing(&["login"]);
    run_session(&fake, &spec(3, "ip", None)).unwrap_err();
    assert_eq!(
        fake.calls(),
        [
            "create",
            "pos_id 3",
            "connect",
            "login",
            "disconnect",
            "dispose"
        ]
    );

    // reading the id fails: logout + disconnect still run
    let fake = FakeTim::failing(&["terminal_id"]);
    run_session(&fake, &spec(3, "ip", None)).unwrap_err();
    assert_eq!(
        fake.calls(),
        [
            "create",
            "pos_id 3",
            "connect",
            "login",
            "terminal_id",
            "logout",
            "disconnect",
            "dispose"
        ]
    );

    // a failing logout / disconnect does not fail a good session
    let fake = FakeTim::failing(&["logout", "disconnect"]);
    assert!(run_session(&fake, &spec(3, "ip", None)).is_ok());

    // the terminal object cannot even be created
    let fake = FakeTim::failing(&["create"]);
    run_session(&fake, &spec(3, "ip", None)).unwrap_err();
    assert_eq!(fake.calls(), ["create"]);
}

#[test]
fn test_tim_error_text() {
    let e = TimError {
        call: "ta_terminal_connect",
        code: 8,
    };
    assert_eq!(
        e.to_string(),
        "ta_terminal_connect failed: rc 8 (api_connect_fail_terminal)"
    );
    assert_eq!(TimError { call: "x", code: 1 }.code_name(), "ok");
    assert_eq!(TimError { call: "x", code: 0 }.code_name(), "undefined");
    assert_eq!(
        TimError {
            call: "x",
            code: 10
        }
        .code_name(),
        "api_connection_lost_terminal"
    );
    assert_eq!(
        TimError {
            call: "x",
            code: 999
        }
        .code_name(),
        "other"
    );
    let names: Vec<_> = [0, 1, 2, 3, 5, 7, 8, 9, 10, 11, 13, 14, 139, 140, 4]
        .into_iter()
        .map(|code| TimError { call: "x", code }.code_name())
        .collect();
    assert_eq!(
        names,
        [
            "undefined",
            "ok",
            "api_cancel_ecr",
            "api_invalid_answer",
            "api_function_disallowed",
            "api_connect_fail_server",
            "api_connect_fail_terminal",
            "api_connection_lost_server",
            "api_connection_lost_terminal",
            "ethernet_disconnected",
            "api_timeout_server",
            "api_timeout_terminal",
            "invalid_argument",
            "out_of_memory",
            "other"
        ]
    );
}

#[test]
fn test_spec_pos_id_and_settings_lines() {
    let s = spec(3, "10.78.2.8", None);
    assert_eq!(s.pos_id(), "3");
    let lines = s.settings_lines();
    for want in [
        "pos_id = 3",
        "connection_mode = on_fix_ip",
        "connection_ip = 10.78.2.8",
        "connection_port = 7784",
        "integrator_id = 7dc442d7-6528-4a05-ba84-5e66558e48b7",
        "auto_commit = false",
        "commit_timeout_s = 60",
        "expected_terminal_id = (not set)",
    ] {
        assert!(lines.contains(want), "{want} missing in {lines}");
    }
}

#[test]
fn test_terminal_specs_from_config() {
    let kasa = |id, ip: Option<&str>| PokladnaKasaConfig {
        id,
        portos_url: None,
        terminal_ip: ip.map(str::to_string),
        terminal_tid: Some(format!("T{id}")),
        lan_key: None,
    };
    let mut cfg = PokladnaClientConfig {
        kasy: vec![kasa(3, Some("10.78.2.8")), kasa(4, None)],
        ..Default::default()
    };
    assert!(
        TimExecutor::terminal_specs(&cfg).is_empty(),
        "no [client.pokladna.tim]"
    );
    cfg.tim = Some(TimConfig {
        library_path: "lib".into(),
        integrator_id: "11111111-2222-3333-4444-555555555555".into(),
        commit_timeout_s: 90,
        rollback_deadline_s: 45,
    });
    assert_eq!(
        TimExecutor::terminal_specs(&cfg),
        vec![TerminalSpec {
            kasa_id: 3,
            ip: "10.78.2.8".into(),
            expected_tid: Some("T3".into()),
            integrator_id: "11111111-2222-3333-4444-555555555555".into(),
            commit_timeout_s: 90,
        }]
    );
}

#[test]
fn test_aggregate_status() {
    use TerminalState::*;
    assert_eq!(aggregate_status(true, &[Ready, Ready]), STATUS_OK);
    assert_eq!(aggregate_status(true, &[Ready, Busy]), STATUS_BUSY);
    assert_eq!(
        aggregate_status(true, &[Busy, Failed("x".into())]),
        STATUS_OFFLINE
    );
    assert_eq!(aggregate_status(true, &[]), STATUS_OFFLINE, "no terminal");
    assert_eq!(
        aggregate_status(false, &[Ready]),
        STATUS_OFFLINE,
        "no library"
    );
    assert_eq!(aggregate_status(false, &[Busy]), STATUS_OFFLINE);
    assert_eq!(effective_state(&Ready, 0), Ready);
    assert_eq!(effective_state(&Ready, 1), Busy);
    assert_eq!(effective_state(&Failed("x".into()), 0), Failed("x".into()));
    assert_eq!(effective_state(&Failed("x".into()), 2), Busy);
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let a = InFlight::enter(&counter);
    let b = InFlight::enter(&counter);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    drop(a);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(b);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_card_jobs_refused_before_any_terminal_contact() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let tim = Arc::new(executor(&fake, dir.path()).with_journal(Arc::clone(&journal)));
    let mut registry = ExecutorRegistry::default();
    registry.register(tim.clone()).unwrap();
    // a card type the terminal does not know stays unsupported
    for job_type in ["card.refund", "card.void", "card.tip"] {
        let rejection = validate(&job(job_type, 3, json!({"amount": 1.0})), &registry).unwrap_err();
        assert_eq!(rejection.code, CODE_UNSUPPORTED, "{job_type}");
        assert!(
            rejection.text.contains(&format!("'{job_type}'")),
            "{}",
            rejection.text
        );
        assert!(rejection.text.contains("nothing ran"), "{}", rejection.text);
    }
    let refused = |job_type: &str, kasa: i64, payload: Value| {
        validate(&job(job_type, kasa, payload), &registry)
            .unwrap_err()
            .code
    };
    // the shape check still comes first
    assert_eq!(
        refused("card.purchase", 3, json!({"amount": 0})),
        crate::device_jobs::executor::CODE_BAD_AMOUNT
    );
    assert_eq!(
        refused("card.purchase", 3, json!({"amount": 1.005})),
        crate::device_jobs::executor::CODE_BAD_AMOUNT,
        "not whole cents"
    );
    assert_eq!(
        refused("card.purchase", 9, json!({"amount": 1.0})),
        card::CODE_NO_TERMINAL
    );
    assert_eq!(
        refused(
            "card.purchase",
            3,
            json!({"amount": 1.0, "currency": "USD"})
        ),
        card::CODE_BAD_CURRENCY
    );
    assert_eq!(refused("card.commit", 3, json!({})), card::CODE_BAD_PAYLOAD);
    assert_eq!(
        refused("card.rollback", 3, json!({"card_job_id": -1})),
        card::CODE_BAD_PAYLOAD
    );
    assert!(
        validate(
            &job(
                "card.purchase",
                3,
                json!({"amount": 2.5, "currency": "EUR"})
            ),
            &registry
        )
        .is_ok()
    );
    assert!(validate(&job("card.commit", 3, json!({"card_job_id": 7})), &registry).is_ok());
    // lane D3 types: their own shape checks, then accepted
    assert_eq!(
        refused("card.credit", 3, json!({"amount": -1.0})),
        crate::device_jobs::executor::CODE_BAD_AMOUNT
    );
    assert_eq!(
        refused("card.credit", 3, json!({"amount": 1.5, "currency": "CZK"})),
        card::CODE_BAD_CURRENCY
    );
    assert_eq!(
        refused("card.credit", 9, json!({"amount": 1.5})),
        card::CODE_NO_TERMINAL
    );
    for payload in [
        json!({"amount": 2.5}),
        json!({"trans_seq": 100}),
        json!({"trans_seq": -1, "amount": 2.5}),
        json!({"trans_seq": 2_147_483_648_i64, "amount": 2.5}),
        json!({"trans_seq": "100", "amount": 2.5}),
        json!({"trans_seq": 100, "amount": 2.505}),
    ] {
        assert_eq!(
            refused("card.reversal", 3, payload.clone()),
            card::CODE_BAD_PAYLOAD,
            "{payload}"
        );
    }
    assert_eq!(
        refused("card.reversal", 9, json!({"trans_seq": 100, "amount": 2.5})),
        card::CODE_NO_TERMINAL
    );
    for job_type in ["card.activate", "card.deactivate", "card.balance"] {
        assert_eq!(refused(job_type, 9, json!({})), card::CODE_NO_TERMINAL);
        assert!(validate(&job(job_type, 3, json!({"session_id": 1})), &registry).is_ok());
    }
    assert_eq!(
        refused("card.print_copy", 3, json!({})),
        card::CODE_BAD_PAYLOAD
    );
    assert_eq!(
        refused("card.print_copy", 9, json!({"card_job_id": 7})),
        card::CODE_NO_TERMINAL
    );
    for (job_type, payload) in [
        ("card.credit", json!({"amount": 1.5, "currency": "EUR"})),
        (
            "card.reversal",
            json!({"pos_payment_id": 4, "trans_seq": 0, "amount": 2.5, "tid": "12345678"}),
        ),
        (
            "card.reversal",
            json!({"trans_seq": 2_147_483_647_i64, "amount": 2.5}),
        ),
        ("card.print_copy", json!({"card_job_id": 7})),
    ] {
        assert!(
            validate(&job(job_type, 3, payload), &registry).is_ok(),
            "{job_type}"
        );
    }

    // without a journal / library a purchase is refused, never sent
    let no_journal = executor(&fake, dir.path());
    assert_eq!(
        no_journal
            .check(&job("card.purchase", 3, json!({"amount": 1.0})))
            .unwrap_err()
            .code,
        card::CODE_TIM_UNAVAILABLE
    );
    assert_eq!(
        no_journal
            .check(&job("card.commit", 3, json!({"card_job_id": 7})))
            .unwrap_err()
            .code,
        card::CODE_TIM_UNAVAILABLE
    );
    let no_library = TimExecutor::with_backend(
        vec![spec(3, "10.78.2.8", None)],
        Err("missing".into()),
        dir.path(),
    )
    .with_journal(Arc::clone(&journal));
    assert_eq!(
        no_library
            .check(&job("card.purchase", 3, json!({"amount": 1.0})))
            .unwrap_err()
            .code,
        card::CODE_TIM_UNAVAILABLE
    );
    assert_eq!(
        fake.calls(),
        ["settings", "settings", "settings", "settings"],
        "only tim-settings.cfg touched the library"
    );
}

#[tokio::test]
async fn test_queues_status_and_heartbeat() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    let tim = Arc::new(executor(&fake, dir.path()));
    assert_eq!(
        tim.endpoints(),
        [
            "10.78.2.18",
            "10.78.2.18#settle",
            "10.78.2.8",
            "10.78.2.8#settle"
        ]
    );
    assert_eq!(
        tim.endpoint(&job("card.purchase", 3, Value::Null)),
        "10.78.2.8"
    );
    assert_eq!(
        tim.endpoint(&job("card.commit", 3, Value::Null)),
        "10.78.2.8#settle"
    );
    assert_eq!(
        tim.endpoint(&job("card.rollback", 4, Value::Null)),
        "10.78.2.18#settle"
    );
    assert_eq!(
        tim.endpoint(&job("card.commit", 9, Value::Null)),
        "",
        "no terminal: the default queue"
    );
    assert_eq!(tim.endpoint(&job("card.purchase", 9, Value::Null)), "");
    assert_eq!((tim.kind(), tim.name()), (DeviceKind::Card, "tim"));
    let mut registry = ExecutorRegistry::default();
    registry.register(tim.clone()).unwrap();
    assert_eq!(registry.device_status(), "portos=none; terminal=tim-ok");

    // a failed session → offline until the next good one
    fake.0.lock().unwrap().fail = vec!["connect"];
    let err = tim.session(4).await.unwrap_err();
    assert_eq!(
        err,
        SessionError::Tim(TimError {
            call: "connect",
            code: 8
        })
    );
    assert_eq!(registry.device_status(), "portos=none; terminal=offline");
    fake.0.lock().unwrap().fail.clear();
    let report = tim.session(4).await.unwrap();
    assert_eq!(report.terminal_id.as_deref(), Some("12345678"));
    assert_eq!(tim.status(), STATUS_OK);
    assert_eq!(
        tim.session(9).await.unwrap_err(),
        SessionError::NoTerminal(9)
    );

    // no terminal at all
    let none_dir = tempfile::tempdir().unwrap();
    let none = TimExecutor::with_backend(Vec::new(), Ok(Arc::new(fake.clone())), none_dir.path());
    assert_eq!(none.status(), STATUS_OFFLINE);
    assert_eq!(none.endpoints(), [""]);
    let text = std::fs::read_to_string(none_dir.path().join(logs::SETTINGS_FILE)).unwrap();
    assert_eq!(
        text,
        "# devbridge TIM settings at start\n# no till has a terminal_ip\n"
    );
}

#[tokio::test]
async fn test_busy_while_a_session_runs() {
    /// A terminal whose connect waits until the test lets it go.
    struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl TimBackend for Gate {
        fn settings_text(&self, _: &TerminalSpec) -> Result<String, TimError> {
            Ok(String::new())
        }
        fn terminal(&self, _: &TerminalSpec) -> Result<Box<dyn TimTerminal>, TimError> {
            Ok(Box::new(GateTerminal(Arc::clone(&self.0))))
        }
    }
    struct GateTerminal(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl TimTerminal for GateTerminal {
        fn connect(&mut self) -> Result<(), TimError> {
            let (open, cv) = &*self.0;
            let _g = cv.wait_while(open.lock().unwrap(), |o| !*o).unwrap();
            Ok(())
        }
        fn login(&mut self) -> Result<(), TimError> {
            Ok(())
        }
        fn terminal_id(&mut self) -> Result<Option<String>, TimError> {
            Ok(None)
        }
        fn logout(&mut self) -> Result<(), TimError> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), TimError> {
            Ok(())
        }
        fn last_transaction(&mut self) -> Result<card::LastTransaction, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn transaction(&mut self, _: card::TwoPhase, _: i64) -> Result<card::TxFacts, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn commit(&mut self) -> Result<(), TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn rollback(&mut self) -> Result<(), TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn reversal(&mut self, _: i64) -> Result<card::TxFacts, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn activate(&mut self) -> Result<reports::ShiftReport, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn deactivate(&mut self) -> Result<reports::ShiftReport, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn balance(&mut self) -> Result<reports::BalanceReport, TimError> {
            unreachable!("the gate test runs a plain session")
        }
        fn reprint(&mut self) -> Result<reports::Receipts, TimError> {
            unreachable!("the gate test runs a plain session")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let tim = Arc::new(TimExecutor::with_backend(
        vec![spec(3, "10.78.2.8", None)],
        Ok(Arc::new(Gate(Arc::clone(&gate)))),
        dir.path(),
    ));
    let running = tokio::spawn({
        let tim = Arc::clone(&tim);
        async move { tim.session(3).await }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = tim.status();
    while seen != STATUS_BUSY && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
        seen = tim.status();
    }
    // open the gate BEFORE asserting: a blocked session thread would keep
    // the test runtime from ever shutting down (a mutant = a hang)
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert_eq!(seen, STATUS_BUSY, "never busy while the session ran");
    assert_eq!(running.await.unwrap().unwrap().terminal_id, None);
    assert_eq!(tim.status(), STATUS_OK);
}

#[test]
fn test_library_missing_is_offline_and_settings_file_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let tim = TimExecutor::with_backend(
        vec![spec(3, "10.78.2.8", None)],
        Err("cannot load the TIM library C:/x.dll".into()),
        dir.path(),
    );
    assert_eq!(tim.status(), STATUS_OFFLINE);
    let text = std::fs::read_to_string(dir.path().join(logs::SETTINGS_FILE)).unwrap();
    assert!(
        text.contains("# library not loaded: cannot load the TIM library C:/x.dll\n"),
        "{text}"
    );
    assert!(!text.contains("no till has a terminal_ip"), "{text}");
    assert!(text.contains("[terminal 10.78.2.8]\nkasa_id = 3"), "{text}");
    assert!(!text.contains("library_settings"), "{text}");
}

#[test]
fn test_settings_file_holds_the_library_dump() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeTim::failing(&[]);
    executor(&fake, dir.path());
    let text = std::fs::read_to_string(dir.path().join(logs::SETTINGS_FILE)).unwrap();
    assert!(
        text.starts_with("# devbridge TIM settings at start\n\n[terminal 10.78.2.18]\n"),
        "no problem line: {text}"
    );
    assert!(
        text.contains("library_settings = TerminalSettings(ip=10.78.2.8)"),
        "{text}"
    );
    assert!(text.contains("pos_id = 4"), "{text}");
    let failing = FakeTim::failing(&["settings"]);
    let dir2 = tempfile::tempdir().unwrap();
    executor(&failing, dir2.path());
    let text = std::fs::read_to_string(dir2.path().join(logs::SETTINGS_FILE)).unwrap();
    assert!(
        text.contains("library_settings_error = settings failed: rc 8"),
        "{text}"
    );
}

#[test]
fn test_start_without_a_loadable_library_never_fails() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = PokladnaClientConfig {
        kasy: vec![PokladnaKasaConfig {
            id: 3,
            portos_url: None,
            terminal_ip: Some("10.78.2.8".into()),
            terminal_tid: None,
            lan_key: None,
        }],
        tim: Some(TimConfig {
            library_path: dir.path().join("no-such-timapi.dll").display().to_string(),
            integrator_id: TIM_DEFAULT_INTEGRATOR_ID.into(),
            commit_timeout_s: 60,
            rollback_deadline_s: 45,
        }),
        ..Default::default()
    };
    let tim = TimExecutor::start(
        &cfg,
        dir.path(),
        Arc::new(Journal::open_in_memory().unwrap()),
        None,
    );
    assert_eq!(tim.status(), STATUS_OFFLINE);
    let settings = dir
        .path()
        .join("logs")
        .join("tim")
        .join(logs::SETTINGS_FILE);
    let text = std::fs::read_to_string(settings).unwrap();
    assert!(text.contains("cannot load the TIM library"), "{text}");
}

// ---- logs ----------------------------------------------------------------

fn day(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

#[test]
fn test_log_file_names() {
    assert_eq!(logs::file_name(day("2026-11-04")), "tim-2026-11-04.log");
    assert_eq!(
        logs::parse_file_name("tim-2026-11-04.log"),
        Some((day("2026-11-04"), false))
    );
    assert_eq!(
        logs::parse_file_name("tim-2026-11-04.log.gz"),
        Some((day("2026-11-04"), true))
    );
    for bad in [
        "tim-2026-11-4.log",
        "tim-2026-13-04.log",
        "tim-settings.cfg",
        "x-2026-11-04.log",
        "tim-2026-11-04.log.gz.tmp",
        "tim-2026-11-04.txt",
    ] {
        assert_eq!(logs::parse_file_name(bad), None, "{bad}");
    }
    assert_eq!(logs::log_dir(Path::new("/d")), Path::new("/d/logs/tim"));
}

#[test]
fn test_level_names_and_record_format() {
    let names: Vec<_> = (0..=6).map(logs::level_name).collect();
    assert_eq!(
        names,
        [
            "OFF", "SEVERE", "WARNING", "INFO", "FINE", "FINEST", "LEVEL?"
        ]
    );
    let now = Local.with_ymd_and_hms(2026, 11, 4, 12, 30, 5).unwrap();
    let line = logs::format_record(now, 5, 42, Some("Terminal::connect"), "<SIXml/>", &[]);
    assert!(line.starts_with("2026-11-04T12:30:05.000"), "{line}");
    assert!(
        line.ends_with(" FINEST [42] Terminal::connect: <SIXml/>\n"),
        "{line}"
    );
    assert!(logs::format_record(now, 3, 1, None, "m", &[]).ends_with(" INFO [1] -: m\n"));
    let with = logs::format_record(now, 4, 1, None, "m", &["a".into(), "b".into()]);
    assert!(with.ends_with(" FINE [1] -: m params=[a, b]\n"), "{with}");
}

#[test]
fn test_housekeep_gzips_past_days_and_keeps_100_days() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let today = day("2026-11-04");
    std::fs::write(d.join("tim-2026-11-04.log"), "today\n").unwrap();
    std::fs::write(d.join("tim-2026-11-03.log"), "yesterday\n").unwrap();
    std::fs::write(d.join("tim-2026-07-27.log.gz"), "x").unwrap(); // 100 days old: kept
    std::fs::write(d.join("tim-2026-07-26.log.gz"), "x").unwrap(); // 101 days: deleted
    std::fs::write(d.join("tim-2026-07-01.log"), "old\n").unwrap(); // gzipped, then deleted
    std::fs::write(d.join(logs::SETTINGS_FILE), "s").unwrap();
    std::fs::write(d.join("other.log"), "o").unwrap();
    let done = logs::housekeep(d, today).unwrap();
    assert_eq!(
        done,
        Housekeeping {
            compressed: vec!["tim-2026-07-01.log".into(), "tim-2026-11-03.log".into()],
            deleted: vec![
                "tim-2026-07-01.log.gz".into(),
                "tim-2026-07-26.log.gz".into()
            ],
        }
    );
    let mut names: Vec<String> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "other.log",
            "tim-2026-07-27.log.gz",
            "tim-2026-11-03.log.gz",
            "tim-2026-11-04.log",
            "tim-settings.cfg"
        ]
    );
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(d.join("tim-2026-11-03.log.gz")).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "yesterday\n");
    assert_eq!(
        logs::housekeep(d, today).unwrap(),
        Housekeeping::default(),
        "idempotent"
    );
}

#[test]
fn test_daily_log_appends_and_rotates_on_a_new_day() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("logs").join("tim");
    let mut log = logs::DailyLog::new(&d).unwrap();
    let day1 = Local.with_ymd_and_hms(2026, 11, 3, 23, 59, 59).unwrap();
    let day2 = Local.with_ymd_and_hms(2026, 11, 4, 0, 0, 1).unwrap();
    log.write(day1, "a\n").unwrap();
    log.write(day1, "b\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("tim-2026-11-03.log")).unwrap(),
        "a\nb\n"
    );
    log.write(day2, "c\n").unwrap();
    // today's file is open at once; yesterday is gzipped on its own thread
    assert_eq!(
        std::fs::read_to_string(d.join("tim-2026-11-04.log")).unwrap(),
        "c\n"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.join("tim-2026-11-03.log").exists() || !d.join("tim-2026-11-03.log.gz").exists() {
        assert!(
            Instant::now() < deadline,
            "yesterday never gzipped after the day change"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // a new DailyLog (restart) appends to today's file
    let mut again = logs::DailyLog::new(&d).unwrap();
    again.write(day2, "d\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("tim-2026-11-04.log")).unwrap(),
        "c\nd\n"
    );
}

#[test]
fn test_global_sink_receives_records() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("tim");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("tim-2000-01-01.log"), "old\n").unwrap();
    logs::set_global_sink(&d).unwrap();
    assert!(
        !d.join("tim-2000-01-01.log").exists(),
        "housekept at install"
    );
    assert!(
        !d.join("tim-2000-01-01.log.gz").exists(),
        "older than 100 days"
    );
    let now = Local::now();
    logs::write_global(now, "record one\n");
    let text = std::fs::read_to_string(d.join(logs::file_name(now.date_naive()))).unwrap();
    assert_eq!(text, "record one\n");
}

#[test]
fn test_check_and_c_text() {
    assert_eq!(check("ta_terminal_login", 1), Ok(()));
    assert_eq!(
        check("ta_terminal_login", 8),
        Err(TimError {
            call: "ta_terminal_login",
            code: 8
        })
    );
    assert_eq!(check("x", 0), Err(TimError { call: "x", code: 0 }));
    assert_eq!(unsafe { c_text(std::ptr::null()) }, None);
    let owned = std::ffi::CString::new("Terminal::connect").unwrap();
    assert_eq!(
        unsafe { c_text(owned.as_ptr()) }.as_deref(),
        Some("Terminal::connect")
    );
    let lossy = std::ffi::CString::new(vec![b'a', 0xff, b'b']).unwrap();
    assert_eq!(
        unsafe { c_text(lossy.as_ptr()) }.as_deref(),
        Some("a\u{fffd}b")
    );
}

#[test]
fn test_housekeeping_error_never_costs_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("tim");
    let mut log = logs::DailyLog::new(&d).unwrap();
    // a DIRECTORY with a log file's name: gzipping it fails
    std::fs::create_dir(d.join("tim-2026-11-01.log")).unwrap();
    assert!(logs::housekeep(&d, day("2026-11-04")).is_err());
    let now = Local.with_ymd_and_hms(2026, 11, 4, 8, 0, 0).unwrap();
    log.write(now, "kept\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(d.join("tim-2026-11-04.log")).unwrap(),
        "kept\n"
    );
    logs::set_global_sink(&d).expect("a housekeeping error is logged, not fatal");
}
