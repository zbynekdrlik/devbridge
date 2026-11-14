//! `file.write` through the whole Odoo path (#130), with the registry a
//! SERVER-mode service builds (`server_registry_from_config`: the file
//! executor only): the file lands under its name with the exact bytes and is
//! acked `done`; the same name again is `file_exists` and never overwritten;
//! a path in the name is refused `bad_payload` before journaling; a `fiscal.*`
//! job is `unsupported` (no Portos on a server); the heartbeat names
//! `file=ok`; a job a dead process journaled but never named is reconciled
//! `not_executed` after the restart, one it named is reconciled `done`.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use devbridge_client::device_jobs::api::DeviceJob;
use devbridge_client::device_jobs::file_write::{STAGING_DIR, staging_path};
use devbridge_client::device_jobs::journal::{JobState, Journal};
use devbridge_client::device_jobs::server_registry_from_config;
use devbridge_core::config::{FileWriteConfig, PokladnaClientConfig};

use super::fake::*;

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file_payload(filename: &str, bytes: &[u8]) -> Value {
    json!({
        "filename": filename,
        "encoding": "cp1250",
        "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        "size": bytes.len(),
        "sha256": sha(bytes),
    })
}

fn server_config(url: &str, target_dir: &std::path::Path) -> PokladnaClientConfig {
    let mut cfg = config(url, 0);
    cfg.fiscal = false;
    cfg.file_write = Some(FileWriteConfig {
        target_dir: target_dir.to_string_lossy().into_owned(),
    });
    cfg
}

#[tokio::test]
async fn test_server_bridge_writes_files_exactly_and_never_overwrites() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("in");
    std::fs::create_dir(&target).unwrap();
    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let cfg = server_config(&url, &target);
    let registry = server_registry_from_config(&cfg, &journal).unwrap();
    assert_eq!(
        registry.device_status(),
        "portos=none; terminal=none; file=ok"
    );
    let order = b"HDR objedn\xe1vka \x8eilina\r\n".to_vec();
    {
        let mut s = fake.lock().unwrap();
        s.queue(
            701,
            "file.write",
            file_payload("ORDER_SNV408_P1_1.txt", &order),
        );
        s.queue(
            702,
            "file.write",
            file_payload("ORDER_SNV408_P1_1.txt", b"second"),
        );
        s.queue(703, "file.write", file_payload("in\\evil.txt", b"x"));
        s.queue(704, "fiscal.cash_register", script("done"));
    }
    let run = start(cfg, Arc::clone(&journal), registry, timing());
    eventually("all four file-server jobs answered", || {
        [701, 702, 703, 704]
            .iter()
            .all(|id| matches!(final_state(&fake, *id), St::Done | St::Failed))
    })
    .await;
    eventually("a heartbeat with the file executor", || {
        fake.lock()
            .unwrap()
            .heartbeats
            .iter()
            .any(|hb| hb["device_status"] == "portos=none; terminal=none; file=ok")
    })
    .await;
    run.stop().await;

    let s = fake.lock().unwrap();
    assert_eq!(
        s.acks_for(701),
        vec![json!({"job_id": 701, "result": {
            "filename": "ORDER_SNV408_P1_1.txt", "size": order.len(), "sha256": sha(&order)}})]
    );
    let refused = |id: i64, code: &str| {
        let acks = s.acks_for(id);
        assert_eq!(acks.len(), 1, "{id}: {acks:?}");
        assert_eq!(acks[0]["error_code"], code, "{id}: {acks:?}");
        assert!(
            acks[0]["error"].as_str().unwrap().contains("nothing ran"),
            "{id}: {acks:?}"
        );
    };
    refused(702, "file_exists");
    refused(703, "bad_payload");
    refused(704, "unsupported");
    drop(s);
    assert_eq!(
        std::fs::read(target.join("ORDER_SNV408_P1_1.txt")).unwrap(),
        order,
        "the first file, byte for byte - the second never overwrote it"
    );
    let mut names: Vec<String> = std::fs::read_dir(&target)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["ORDER_SNV408_P1_1.txt", STAGING_DIR]);
    assert_eq!(
        std::fs::read_dir(target.join(STAGING_DIR)).unwrap().count(),
        0
    );
    assert!(!dir.path().join("evil.txt").exists());
    assert_eq!(
        journal.get(703).unwrap().unwrap().state,
        JobState::Failed,
        "a refused job is journaled failed in one write"
    );
    assert_clean(&fake);
}

fn journaled(journal: &Journal, id: i64, payload: Value) {
    let job = DeviceJob {
        id,
        job_type: "file.write".into(),
        client_uuid: format!("edi-{id}"),
        kasa_id: None,
        kasa_name: String::new(),
        pos_order: Value::Null,
        payload,
        created_at: "2026-11-13 14:15:02".into(),
        claimed_at: "2026-11-13 14:15:03".into(),
    };
    journal.insert_received(&job).unwrap();
    assert!(journal.start_execution(id).unwrap());
}

#[tokio::test]
async fn test_restart_reconciles_file_jobs_through_the_folder() {
    let fake = new_fake();
    let url = start_fake(Arc::clone(&fake)).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("in");
    std::fs::create_dir(&target).unwrap();
    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    // a previous process: 801 died before naming (a .part left), 802 named
    // its file and died before journaling the outcome
    journaled(&journal, 801, file_payload("ORDER_801.txt", b"never named"));
    let part = staging_path(&target, 801);
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, b"never").unwrap();
    journaled(&journal, 802, file_payload("ORDER_802.txt", b"named"));
    journal.mark_device_contact(802).unwrap();
    std::fs::write(target.join("ORDER_802.txt"), b"named").unwrap();
    {
        let mut s = fake.lock().unwrap();
        s.claimed(
            801,
            "file.write",
            file_payload("ORDER_801.txt", b"never named"),
        );
        s.claimed(802, "file.write", file_payload("ORDER_802.txt", b"named"));
    }
    let cfg = server_config(&url, &target);
    let registry = server_registry_from_config(&cfg, &journal).unwrap();
    let run = start(cfg, Arc::clone(&journal), registry, timing());
    eventually("both reconciled", || {
        matches!(final_state(&fake, 801), St::Failed) && matches!(final_state(&fake, 802), St::Done)
    })
    .await;
    run.stop().await;
    let s = fake.lock().unwrap();
    let finals_801: Vec<Value> = s
        .acks_for(801)
        .into_iter()
        .filter(|a| a["error_code"] != "indeterminate")
        .collect();
    assert_eq!(finals_801.len(), 1, "{finals_801:?}");
    assert_eq!(finals_801[0]["error_code"], "not_executed");
    let finals_802: Vec<Value> = s
        .acks_for(802)
        .into_iter()
        .filter(|a| a["error_code"] != "indeterminate")
        .collect();
    assert_eq!(
        finals_802,
        vec![json!({"job_id": 802, "result": {
            "filename": "ORDER_802.txt", "size": 5, "sha256": sha(b"named")}})]
    );
    drop(s);
    assert!(!part.exists(), "the dead process's .part is removed");
    assert!(
        !target.join("ORDER_801.txt").exists(),
        "a reconcile never writes the file"
    );
    assert_clean(&fake);
}

#[test]
fn test_server_registry_needs_the_file_write_block() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(&dir.path().join("device-journal.db")).unwrap());
    let mut cfg = config("http://127.0.0.1:1", 0);
    let err = server_registry_from_config(&cfg, &journal)
        .expect_err("no file_write block")
        .to_string();
    assert!(
        err.contains("[client.pokladna.file_write] is missing"),
        "{err}"
    );
    // a till, a terminal and the test executor are never built on a server
    cfg.test_executor = true;
    cfg.file_write = Some(FileWriteConfig {
        target_dir: dir.path().to_string_lossy().into_owned(),
    });
    let registry = server_registry_from_config(&cfg, &journal).unwrap();
    assert_eq!(
        registry.kinds(),
        vec![devbridge_client::device_jobs::executor::DeviceKind::File]
    );
}
