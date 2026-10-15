//! Unit tests of the Portos executor's routing, job checks and status
//! (#100). The HTTP flows run against a fake Portos in
//! `tests/device_jobs/portos.rs`.

use super::*;
use devbridge_core::config::PokladnaKasaConfig;
use serde_json::{Value, json};

const ORP: &str = "88812345678900001";

fn kasa(id: i64, portos_url: Option<&str>) -> PokladnaKasaConfig {
    PokladnaKasaConfig {
        id,
        portos_url: portos_url.map(str::to_string),
        terminal_ip: None,
        terminal_tid: None,
    }
}

fn config_with(kasy: Vec<PokladnaKasaConfig>) -> PortosConfig {
    PortosConfig::from_pokladna(&PokladnaClientConfig {
        kasy,
        ..Default::default()
    })
}

fn executor(config: PortosConfig) -> PortosExecutor {
    PortosExecutor::with_send_log(config, SendLog::open_in_memory().unwrap()).unwrap()
}

fn job(job_type: &str, kasa_id: Option<i64>, payload: Value) -> DeviceJob {
    DeviceJob {
        id: 1,
        job_type: job_type.into(),
        client_uuid: "6f1c2a0e-7b1d".into(),
        kasa_id,
        kasa_name: String::new(),
        pos_order: Value::Null,
        payload,
        created_at: String::new(),
        claimed_at: String::new(),
    }
}

fn deposit() -> Value {
    json!({"data": {"cashRegisterCode": ORP, "amount": 10.0}})
}

#[test]
fn test_default_timing() {
    let t = PortosTiming::default();
    assert_eq!(t.lookup, Duration::from_secs(15));
    assert_eq!(t.register, Duration::from_secs(60));
    assert_eq!(t.control, Duration::from_secs(30));
    assert_eq!(t.probe, Duration::from_secs(5));
    let guard = crate::device_jobs::Timing::default().execute_guard;
    assert!(
        t.lookup + t.register < guard,
        "a receipt finishes before the execute guard"
    );
}

#[test]
fn test_config_routes_each_till_to_its_portos() {
    let c = config_with(vec![
        kasa(3, Some("http://10.78.2.10:3010/")),
        kasa(4, Some(" http://10.78.2.11:3010 ")),
        kasa(5, None),
        kasa(6, Some("  ")),
        kasa(7, Some("http://10.78.2.10:3010")),
    ]);
    assert_eq!(c.default_url, "http://localhost:3010");
    assert_eq!(c.url_for(Some(3)), "http://10.78.2.10:3010");
    assert_eq!(c.url_for(Some(4)), "http://10.78.2.11:3010");
    assert_eq!(
        c.url_for(Some(5)),
        DEFAULT_PORTOS_URL,
        "no portos_url → default"
    );
    assert_eq!(
        c.url_for(Some(6)),
        DEFAULT_PORTOS_URL,
        "blank portos_url → default"
    );
    assert_eq!(
        c.url_for(Some(99)),
        DEFAULT_PORTOS_URL,
        "an unlisted till → default"
    );
    assert_eq!(c.url_for(None), DEFAULT_PORTOS_URL);
    assert_eq!(
        c.endpoints(),
        vec![
            "http://10.78.2.10:3010",
            "http://10.78.2.11:3010",
            "http://localhost:3010"
        ],
        "one queue per Portos, two tills on one Portos share it"
    );
    assert_eq!(
        config_with(Vec::new()).endpoints(),
        vec![DEFAULT_PORTOS_URL]
    );
    assert_eq!(c.timing, PortosTiming::default());
}

#[test]
fn test_executor_identity_routing_and_initial_status() {
    let e = executor(config_with(vec![kasa(4, Some("http://b:3010"))]));
    assert_eq!(e.kind(), DeviceKind::Fiscal);
    assert_eq!(e.name(), "portos");
    assert_eq!(e.endpoints(), vec!["http://b:3010", DEFAULT_PORTOS_URL]);
    assert_eq!(
        e.endpoint(&job("fiscal.cash_register", Some(4), deposit())),
        "http://b:3010"
    );
    assert_eq!(
        e.endpoint(&job("fiscal.open_drawer", Some(3), json!({}))),
        DEFAULT_PORTOS_URL
    );
    assert_eq!(e.status(), "unknown", "nothing probed yet");
}

#[test]
fn test_record_state_reports_only_changes() {
    let e = executor(config_with(vec![kasa(4, Some("http://b:3010"))]));
    assert_eq!(
        e.record_state("http://b:3010", DeviceState::Ok),
        Some(DeviceState::Unknown)
    );
    assert_eq!(
        e.status(),
        "unknown",
        "the default Portos is not probed yet"
    );
    assert_eq!(
        e.record_state(DEFAULT_PORTOS_URL, DeviceState::Ok),
        Some(DeviceState::Unknown)
    );
    assert_eq!(e.status(), "ok");
    assert_eq!(
        e.record_state(DEFAULT_PORTOS_URL, DeviceState::Ok),
        None,
        "unchanged"
    );
    let degraded = DeviceState::Degraded("printer=Error,paper=Ready".into());
    assert_eq!(
        e.record_state("http://b:3010", degraded.clone()),
        Some(DeviceState::Ok)
    );
    assert_eq!(e.status(), "degraded(printer=Error,paper=Ready)");
    assert_eq!(e.record_state("http://b:3010", degraded), None);
    assert_eq!(
        e.record_state("http://b:3010", DeviceState::Offline),
        Some(DeviceState::Degraded("printer=Error,paper=Ready".into()))
    );
    assert_eq!(e.status(), "offline");
}

fn rejection(e: &PortosExecutor, j: &DeviceJob) -> Rejection {
    e.check(j).expect_err("refused")
}

#[test]
fn test_check_accepts_valid_jobs_of_every_type() {
    let e = executor(config_with(Vec::new()));
    let sale = json!({"data": {"cashRegisterCode": ORP, "items": [
        {"type": "Positive", "name": "Rožok", "quantity": {"amount": 1}, "unitPrice": 0.12, "price": 0.12}]}});
    for (t, payload) in [
        ("fiscal.cash_register", sale.clone()),
        ("fiscal.invalid", sale),
        ("fiscal.deposit", deposit()),
        (
            "fiscal.withdraw",
            json!({"data": {"cashRegisterCode": ORP, "amount": -10.0}}),
        ),
        (
            "fiscal.print_copy",
            json!({"cashRegisterCode": ORP, "externalId": "dbj-3-a"}),
        ),
        ("fiscal.open_drawer", json!({})),
    ] {
        assert_eq!(e.check(&job(t, Some(3), payload)), Ok(()), "{t}");
    }
    // drawer / copy do not build an externalId: any client_uuid is fine
    let mut drawer = job("fiscal.open_drawer", Some(3), json!({}));
    drawer.client_uuid = "has space".into();
    assert_eq!(e.check(&drawer), Ok(()));
}

#[test]
fn test_check_refusals_name_the_reason_and_say_nothing_ran() {
    let e = executor(config_with(Vec::new()));
    let r = rejection(&e, &job("fiscal.paragon", Some(3), json!({})));
    assert_eq!(r.code, CODE_UNSUPPORTED);
    assert!(
        r.text.contains("'fiscal.paragon'") && r.text.ends_with("nothing ran"),
        "{}",
        r.text
    );

    let r = rejection(&e, &job("fiscal.deposit", None, deposit()));
    assert_eq!(r.code, CODE_NO_KASA);
    assert!(r.text.ends_with("nothing ran"), "{}", r.text);
    let r = rejection(&e, &job("fiscal.open_drawer", None, json!({})));
    assert_eq!(
        r.code, CODE_NO_KASA,
        "every fiscal job needs its till's Portos"
    );

    let mut bad_uuid = job("fiscal.deposit", Some(3), deposit());
    bad_uuid.client_uuid = "a b".into();
    let r = rejection(&e, &bad_uuid);
    assert_eq!(r.code, CODE_BAD_CLIENT_UUID);
    assert!(r.text.ends_with("nothing ran"), "{}", r.text);
    bad_uuid.client_uuid = String::new();
    assert_eq!(rejection(&e, &bad_uuid).code, CODE_BAD_CLIENT_UUID);

    let r = rejection(
        &e,
        &job(
            "fiscal.deposit",
            Some(3),
            json!({"data": {"cashRegisterCode": ORP, "amount": 1.234}}),
        ),
    );
    assert_eq!(r.code, CODE_BAD_PAYLOAD);
    assert_eq!(
        r.text,
        "payload.data.amount must have at most 2 decimal places - Portos was not contacted, nothing ran"
    );
    assert_eq!(CODE_BAD_PAYLOAD, "bad_payload");
}

#[tokio::test]
async fn test_unknown_type_never_runs_and_drawer_copy_reconcile_retry_safe() {
    let e = executor(config_with(Vec::new()));
    match e.execute(&job("fiscal.paragon", Some(3), json!({}))).await {
        Outcome::Failed { code, text } => {
            assert_eq!(code, CODE_UNSUPPORTED);
            assert!(text.ends_with("nothing ran"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    let entry = |t: &str, payload: Value| {
        let journal = crate::device_jobs::journal::Journal::open_in_memory().unwrap();
        let j = job(t, Some(3), payload);
        journal.insert_received(&j).unwrap();
        journal.get(1).unwrap().unwrap()
    };
    for (t, payload, needle) in [
        ("fiscal.open_drawer", json!({}), "the drawer opened"),
        (
            "fiscal.print_copy",
            json!({"cashRegisterCode": ORP, "externalId": "x"}),
            "the receipt copy was printed",
        ),
    ] {
        match e.reconcile(&entry(t, payload)).await {
            Reconciliation::Failed { code, text } => {
                assert_eq!(code, CODE_RETRY_SAFE, "{t}");
                assert!(text.contains(needle), "{text}");
            }
            other => panic!("{t}: {other:?}"),
        }
    }
    match e.reconcile(&entry("fiscal.paragon", json!({}))).await {
        Reconciliation::StillUnknown(reason) => {
            assert!(reason.contains("'fiscal.paragon'"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    // a receipt without a cash register code cannot be looked up
    match e
        .reconcile(&entry("fiscal.deposit", json!({"data": {}})))
        .await
    {
        Reconciliation::StillUnknown(reason) => {
            assert!(
                reason.contains("names no till or cash register code"),
                "{reason}"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_open_creates_the_send_log_and_prunes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("portos-sent.db");
    let e = PortosExecutor::open(config_with(Vec::new()), &path).unwrap();
    assert!(path.exists());
    e.sent.record("dbj-3-a", 1, DEFAULT_PORTOS_URL).unwrap();
    drop(e);
    let again = PortosExecutor::open(config_with(Vec::new()), &path).unwrap();
    assert!(again.sent.was_sent("dbj-3-a").unwrap(), "reopened, kept");
}
