//! `file.write` executor (#130): staged + verified + atomically named, never
//! an overwrite, never a half-written file under the final name; reconcile
//! after a restart; the executor seam (check / execute / status).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};

use devbridge_core::config::FileWriteConfig;

use super::payload::{FilePayload, parse_payload, sha256_hex};
use super::*;
use crate::device_jobs::api::DeviceJob;
use crate::device_jobs::executor::{DeviceExecutor, Outcome, Reconciliation};
use crate::device_jobs::journal::Journal;

fn payload_json(filename: &str, bytes: &[u8]) -> Value {
    json!({
        "filename": filename,
        "encoding": "cp1250",
        "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        "size": bytes.len(),
        "sha256": sha256_hex(bytes),
    })
}

fn payload(filename: &str, bytes: &[u8]) -> FilePayload {
    parse_payload(&payload_json(filename, bytes)).unwrap()
}

fn job(id: i64, job_type: &str, payload: Value) -> DeviceJob {
    DeviceJob {
        id,
        job_type: job_type.into(),
        client_uuid: format!("edi-{id}"),
        kasa_id: None,
        kasa_name: String::new(),
        pos_order: Value::Null,
        payload,
        created_at: "2026-11-13 14:15:02".into(),
        claimed_at: "2026-11-13 14:15:03".into(),
    }
}

/// Files directly in `dir` (not the staging subfolder).
fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn staging_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(STAGING_DIR)).map_or(0, |d| d.count())
}

fn ok_contact() -> anyhow::Result<()> {
    Ok(())
}

#[test]
fn test_write_file_names_the_exact_bytes_and_leaves_no_staging_file() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"HDR\r\nPOS 1\r\n";
    let p = payload("ORDER_A.txt", bytes);
    let mut contacted = false;
    let outcome = write_file(dir.path(), 812, &p, || {
        contacted = true;
        Ok(())
    });
    assert_eq!(
        outcome,
        Outcome::Done(
            json!({"filename": "ORDER_A.txt", "size": bytes.len(), "sha256": sha256_hex(bytes)})
        )
    );
    assert!(contacted, "the device contact is journaled before naming");
    assert_eq!(
        std::fs::read(dir.path().join("ORDER_A.txt")).unwrap(),
        bytes
    );
    assert_eq!(files_in(dir.path()), vec!["ORDER_A.txt"]);
    assert_eq!(staging_files(dir.path()), 0, "the .part name is removed");
    assert!(
        dir.path().join(STAGING_DIR).is_dir(),
        "the staging subfolder is created on the first job"
    );
}

#[test]
fn test_write_file_never_overwrites_an_existing_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ORDER_A.txt"), b"first").unwrap();
    let mut contacted = false;
    let outcome = write_file(dir.path(), 813, &payload("ORDER_A.txt", b"second"), || {
        contacted = true;
        Ok(())
    });
    let Outcome::Failed { code, text } = outcome else {
        panic!("not failed: {outcome:?}");
    };
    assert_eq!(code, CODE_FILE_EXISTS);
    assert!(text.contains("never overwritten, nothing ran"), "{text}");
    assert!(
        !contacted,
        "an existing name is seen before anything is staged"
    );
    assert_eq!(
        std::fs::read(dir.path().join("ORDER_A.txt")).unwrap(),
        b"first"
    );
    assert_eq!(staging_files(dir.path()), 0);
}

#[test]
fn test_write_file_refuses_a_name_that_appears_between_check_and_link() {
    // the authoritative check is the link itself: a file the reader drops in
    // after the pre-check (or a second job of the same name) is never replaced
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ORDER_RACE.txt");
    let outcome = write_file(dir.path(), 814, &payload("ORDER_RACE.txt", b"ours"), || {
        std::fs::write(&target, b"theirs").unwrap();
        Ok(())
    });
    assert!(
        matches!(&outcome, Outcome::Failed { code, .. } if code == CODE_FILE_EXISTS),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    assert_eq!(staging_files(dir.path()), 0, "our .part is gone");
}

#[test]
fn test_write_file_without_the_folder_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("in");
    let outcome = write_file(&missing, 815, &payload("ORDER_A.txt", b"x"), ok_contact);
    assert!(
        matches!(&outcome, Outcome::Failed { code, text } if code == CODE_NO_TARGET_DIR && text.contains("nothing ran")),
        "{outcome:?}"
    );
    assert!(!missing.exists(), "the target folder is never created");
}

#[test]
fn test_write_file_journal_failure_names_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = write_file(dir.path(), 816, &payload("ORDER_A.txt", b"x"), || {
        anyhow::bail!("disk I/O error")
    });
    let Outcome::Failed { code, text } = outcome else {
        panic!("not failed: {outcome:?}");
    };
    assert_eq!(code, CODE_JOURNAL);
    assert!(
        text.contains("disk I/O error") && text.contains("nothing ran"),
        "{text}"
    );
    assert!(files_in(dir.path()).is_empty());
    assert_eq!(staging_files(dir.path()), 0);
}

#[test]
fn test_write_file_staging_failure_is_write_failed() {
    // a FILE where the staging folder must be: creating it fails
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(STAGING_DIR), b"not a folder").unwrap();
    let mut contacted = false;
    let outcome = write_file(dir.path(), 817, &payload("ORDER_A.txt", b"x"), || {
        contacted = true;
        Ok(())
    });
    let Outcome::Failed { code, text } = outcome else {
        panic!("not failed: {outcome:?}");
    };
    assert_eq!(code, CODE_WRITE_FAILED);
    assert!(text.contains("writing the staging file failed"), "{text}");
    assert!(text.contains("nothing ran"), "{text}");
    assert!(!contacted);
    assert_eq!(files_in(dir.path()), vec![STAGING_DIR.to_string()]);
}

#[test]
fn test_staging_path_is_inside_the_staging_subfolder() {
    assert_eq!(
        staging_path(Path::new("/in"), 812),
        Path::new("/in").join(STAGING_DIR).join("job-812.part")
    );
    assert_eq!(STAGING_DIR, "devbridge-staging");
}

fn entry_for(journal: &Journal, j: &DeviceJob) -> JournalEntry {
    journal.insert_received(j).unwrap();
    journal.get(j.id).unwrap().unwrap()
}

#[test]
fn test_reconcile_without_device_contact_is_not_executed_and_cleans_staging() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(900, JOB_TYPE, payload_json("ORDER_R.txt", b"abc"));
    let entry = entry_for(&journal, &j);
    // a .part left behind by the dead process
    let part = staging_path(dir.path(), 900);
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, b"ab").unwrap();
    let answer = reconcile_file(dir.path(), &entry);
    let Reconciliation::Failed { code, text } = answer else {
        panic!("not failed: {answer:?}");
    };
    assert_eq!(code, CODE_NOT_EXECUTED);
    assert!(
        text.contains("ORDER_R.txt") && text.contains("nothing ran"),
        "{text}"
    );
    assert!(!part.exists(), "its .part is removed");
}

#[test]
fn test_reconcile_after_contact_reads_the_named_file() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(901, JOB_TYPE, payload_json("ORDER_R.txt", b"abc"));
    journal.insert_received(&j).unwrap();
    journal.mark_device_contact(901).unwrap();
    let entry = journal.get(901).unwrap().unwrap();

    // absent: never named, or the reader archived it already
    let absent = reconcile_file(dir.path(), &entry);
    assert!(
        matches!(&absent, Reconciliation::StillUnknown(r) if r.contains("already archived")),
        "{absent:?}"
    );
    // another file of that name: unknown, never "done"
    std::fs::write(dir.path().join("ORDER_R.txt"), b"other").unwrap();
    let other = reconcile_file(dir.path(), &entry);
    assert!(
        matches!(&other, Reconciliation::StillUnknown(r) if r.contains("other content")),
        "{other:?}"
    );
    // our bytes under the final name: done; a leftover .part is removed
    std::fs::write(dir.path().join("ORDER_R.txt"), b"abc").unwrap();
    let part = staging_path(dir.path(), 901);
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, b"abc").unwrap();
    assert_eq!(
        reconcile_file(dir.path(), &entry),
        Reconciliation::Done(
            json!({"filename": "ORDER_R.txt", "size": 3, "sha256": sha256_hex(b"abc")})
        )
    );
    assert!(!part.exists());
    assert_eq!(
        std::fs::read(dir.path().join("ORDER_R.txt")).unwrap(),
        b"abc",
        "reconcile never writes the target"
    );
}

#[test]
fn test_reconcile_unreadable_target_and_payload_stay_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(902, JOB_TYPE, payload_json("ORDER_D.txt", b"abc"));
    journal.insert_received(&j).unwrap();
    journal.mark_device_contact(902).unwrap();
    let entry = journal.get(902).unwrap().unwrap();
    // a FOLDER under the target name cannot be read as the file
    std::fs::create_dir(dir.path().join("ORDER_D.txt")).unwrap();
    let unreadable = reconcile_file(dir.path(), &entry);
    assert!(
        matches!(&unreadable, Reconciliation::StillUnknown(r) if r.contains("could not be read")),
        "{unreadable:?}"
    );
    let mut broken = entry.clone();
    broken.job.payload = json!({"filename": "x"});
    let answer = reconcile_file(dir.path(), &broken);
    assert!(
        matches!(&answer, Reconciliation::StillUnknown(r) if r.contains("payload is unreadable")),
        "{answer:?}"
    );
}

#[test]
fn test_folder_status() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(folder_status(dir.path()), STATUS_OK);
    assert_eq!(
        folder_status(&dir.path().join("gone")),
        STATUS_NO_TARGET_DIR
    );
}

fn executor(dir: &Path, journal: &Arc<Journal>) -> FileWriteExecutor {
    FileWriteExecutor::new(
        &FileWriteConfig {
            target_dir: dir.to_string_lossy().into_owned(),
        },
        Arc::clone(journal),
    )
    .unwrap()
}

#[test]
fn test_new_refuses_an_unusable_folder() {
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let Err(err) = FileWriteExecutor::new(
        &FileWriteConfig {
            target_dir: "data\\in".into(),
        },
        journal,
    ) else {
        panic!("a relative folder is refused");
    };
    let err = err.to_string();
    assert!(err.contains("absolute"), "{err}");
}

#[test]
fn test_check_routes_only_file_write_with_a_valid_payload() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(dir.path(), &journal);
    assert_eq!(e.kind(), DeviceKind::File);
    assert_eq!(e.name(), "file");
    assert_eq!(e.target_dir(), dir.path());
    assert_eq!(
        e.check(&job(1, JOB_TYPE, payload_json("A.txt", b"x"))),
        Ok(())
    );
    let other = e
        .check(&job(2, "file.delete", payload_json("A.txt", b"x")))
        .unwrap_err();
    assert_eq!(other.code, CODE_UNSUPPORTED);
    assert!(other.text.contains("'file.delete'") && other.text.contains("nothing ran"));
    let bad = e
        .check(&job(3, JOB_TYPE, payload_json("..\\A.txt", b"x")))
        .unwrap_err();
    assert_eq!(bad.code, CODE_BAD_PAYLOAD);
    assert!(
        bad.text.ends_with("nothing was written, nothing ran"),
        "{}",
        bad.text
    );
}

#[tokio::test]
async fn test_execute_journals_the_contact_and_writes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(dir.path(), &journal);
    let j = job(950, JOB_TYPE, payload_json("ORDER_E.txt", b"EDI\r\n"));
    journal.insert_received(&j).unwrap();
    let outcome = e.execute(&j).await;
    assert_eq!(
        outcome,
        Outcome::Done(
            json!({"filename": "ORDER_E.txt", "size": 5, "sha256": sha256_hex(b"EDI\r\n")})
        )
    );
    assert!(
        journal
            .get(950)
            .unwrap()
            .unwrap()
            .device_contact_at
            .is_some(),
        "the write is journaled before the file is named"
    );
    // the same name again (a second job) is refused, the file unchanged
    let again = job(951, JOB_TYPE, payload_json("ORDER_E.txt", b"OTHER"));
    journal.insert_received(&again).unwrap();
    let second = e.execute(&again).await;
    assert!(
        matches!(&second, Outcome::Failed { code, .. } if code == CODE_FILE_EXISTS),
        "{second:?}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("ORDER_E.txt")).unwrap(),
        b"EDI\r\n"
    );
    // a payload execute cannot read (never past check in production)
    let broken = e.execute(&job(952, JOB_TYPE, json!({}))).await;
    assert!(
        matches!(&broken, Outcome::Failed { code, .. } if code == CODE_BAD_PAYLOAD),
        "{broken:?}"
    );
    // reconcile through the executor: done (the file holds the bytes)
    let entry = journal.get(950).unwrap().unwrap();
    assert!(matches!(e.reconcile(&entry).await, Reconciliation::Done(_)));
}

#[tokio::test]
async fn test_status_follows_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("in");
    std::fs::create_dir(&target).unwrap();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&target, &journal);
    assert_eq!(e.status(), "ok");
    std::fs::remove_dir(&target).unwrap();
    assert_eq!(e.status(), "ok", "status() never probes, it is cached");
    e.refresh_status().await;
    assert_eq!(e.status(), "no_target_dir");
    let outcome = e
        .execute(&job(960, JOB_TYPE, payload_json("A.txt", b"x")))
        .await;
    assert!(
        matches!(&outcome, Outcome::Failed { code, .. } if code == CODE_NO_TARGET_DIR),
        "{outcome:?}"
    );
}
