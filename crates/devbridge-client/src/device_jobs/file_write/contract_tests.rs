//! Parity with odoo-erp's `file.write` contract (#130): the document and the
//! golden `next` answer in `testdata/` (copied byte-for-byte, see its README)
//! run through the REAL parser / checker / executor. A contract change on
//! either side turns one of these red.

use std::sync::Arc;

use serde_json::{Value, json};

use devbridge_core::config::FileWriteConfig;

use super::payload::{FILENAME_PATTERN, OPTIONAL_KEYS, REQUIRED_KEYS, sha256_hex};
use super::*;
use crate::device_jobs::api::parse_job;
use crate::device_jobs::executor::{DeviceExecutor, Outcome};
use crate::device_jobs::journal::Journal;

const CONTRACT: &str = include_str!("testdata/device_job_file_write.md");
const GOLDEN_NEXT: &str = include_str!("testdata/next_file_write_golden.json");

/// The golden job with its placeholders (`"<int>"`, `"<iso>"`) filled in.
fn golden_job() -> Value {
    let mut next: Value = serde_json::from_str(GOLDEN_NEXT).unwrap();
    let job = next["job"].as_object_mut().unwrap();
    assert_eq!(job["id"], "<int>", "the golden id is a placeholder");
    job.insert("id".into(), json!(812));
    job.insert("created_at".into(), json!("2026-11-13 14:15:02"));
    job.insert("claimed_at".into(), json!("2026-11-13 14:15:03"));
    next["job"].take()
}

#[test]
fn test_contract_filename_pattern_is_ours() {
    let quoted = format!("`filename` matches `{FILENAME_PATTERN}`");
    assert!(
        CONTRACT.contains(&quoted),
        "the contract's filename pattern changed - expected {quoted}"
    );
}

#[test]
fn test_contract_payload_keys_are_ours() {
    // the payload example of the contract: {"filename": …, "encoding": …, …}
    let start = CONTRACT
        .find("{\"filename\"")
        .expect("the contract's payload example");
    let example = &CONTRACT[start..start + CONTRACT[start..].find('}').unwrap() + 1];
    // a quoted string followed by ':' is a key
    let pieces: Vec<&str> = example.split('"').collect();
    let mut keys: Vec<&str> = (1..pieces.len())
        .step_by(2)
        .filter(|i| {
            pieces
                .get(i + 1)
                .is_some_and(|after| after.trim_start().starts_with(':'))
        })
        .map(|i| pieces[i])
        .collect();
    keys.sort_unstable();
    let mut ours: Vec<&str> = REQUIRED_KEYS
        .iter()
        .chain(OPTIONAL_KEYS.iter())
        .copied()
        .collect();
    ours.sort_unstable();
    assert_eq!(keys, ours, "payload keys of {example}");
}

#[test]
fn test_contract_names_our_codes_and_rules() {
    for (needle, why) in [
        (
            "`error_code: \"file_exists\"`",
            "the existing-name failure code",
        ),
        ("never an overwrite", "no overwrite"),
        (
            "never moves or deletes the file afterwards",
            "the file is left for the reader",
        ),
        ("temporary name", "the file is named only when complete"),
        ("checks `size` + `sha256`", "size + sha256 verified"),
        (
            "\"kasa\": null, \"pos_order\": null",
            "a file job has no till",
        ),
    ] {
        assert!(
            CONTRACT.contains(needle),
            "contract lost '{needle}' ({why})"
        );
    }
    assert_eq!(CODE_FILE_EXISTS, "file_exists");
    assert_eq!(JOB_TYPE, "file.write");
}

#[test]
fn test_golden_job_parses_and_passes_the_checker() {
    let job = parse_job(&golden_job()).unwrap();
    assert_eq!(job.id, 812);
    assert_eq!(job.job_type, JOB_TYPE);
    assert_eq!(job.client_uuid, "dj5235-f1");
    assert_eq!(job.kasa_id, None, "kasa: null");
    let payload = payload::parse_payload(&job.payload).unwrap();
    assert_eq!(payload.filename, "ORDER_SNV408_P00012_20261113.txt");
    assert_eq!(payload.bytes.len(), 23);
    // "HDR objednávka Žilina\r\n" in cp1250: á = 0xE1, Ž = 0x8E
    let mut want = b"HDR objedn".to_vec();
    want.push(0xE1);
    want.extend_from_slice(b"vka ");
    want.push(0x8E);
    want.extend_from_slice(b"ilina\r\n");
    assert_eq!(payload.bytes, want);
    assert_eq!(
        payload.sha256,
        "66f5904aa2710637d0125a58ebbda61ca52783af75ee92816db4aedafc12cff6"
    );
    // its Debug (logged) shows payload KEYS only, never the content
    let dbg = format!("{job:?}");
    assert!(!dbg.contains("SERSIG9i"), "{dbg}");
}

#[tokio::test]
async fn test_golden_job_runs_through_the_executor() {
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let executor = FileWriteExecutor::new(
        &FileWriteConfig {
            target_dir: dir.path().to_string_lossy().into_owned(),
        },
        data.path(),
        Arc::clone(&journal),
    )
    .unwrap();
    let job = parse_job(&golden_job()).unwrap();
    assert_eq!(executor.check(&job), Ok(()));
    journal.insert_received(&job).unwrap();
    let outcome = executor.execute(&job).await;
    let written = std::fs::read(dir.path().join("ORDER_SNV408_P00012_20261113.txt")).unwrap();
    assert_eq!(written.len(), 23);
    assert_eq!(
        outcome,
        Outcome::Done(json!({
            "filename": "ORDER_SNV408_P00012_20261113.txt",
            "size": 23,
            "sha256": sha256_hex(&written),
        }))
    );
    assert_eq!(
        sha256_hex(&written),
        "66f5904aa2710637d0125a58ebbda61ca52783af75ee92816db4aedafc12cff6",
        "the bytes on disk are exactly the contract's"
    );
}
