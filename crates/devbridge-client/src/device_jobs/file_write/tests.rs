//! `file.write` executor (#130): staged in the bridge's own folder, verified,
//! atomically named, never an overwrite, never a half-written file under the
//! final name; reconcile after a restart; the executor seam (check / execute
//! / status / the start sweep).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};

use devbridge_core::config::FileWriteConfig;

use super::payload::{FilePayload, parse_payload, sha256_hex};
use super::staging::{STAGING_DIR, part_path, staging_dir};
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

/// A temp root holding `in` (the reader's folder) and `data` (the bridge's
/// data dir, staging inside).
struct Dirs {
    _root: tempfile::TempDir,
    folders: Folders,
    data: std::path::PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("in");
    let data = root.path().join("data");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&data).unwrap();
    Dirs {
        folders: Folders {
            target,
            staging: staging_dir(&data),
        },
        data,
        _root: root,
    }
}

/// Every name in `dir`, sorted (empty when it does not exist).
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn ok_contact() -> anyhow::Result<()> {
    Ok(())
}

#[test]
fn test_write_file_names_the_exact_bytes_and_leaves_nothing_else() {
    let d = dirs();
    let bytes = b"HDR\r\nPOS 1\r\n";
    let p = payload("ORDER_A.txt", bytes);
    let mut contacted = false;
    let outcome = write_file(&d.folders, 812, &p, || {
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
        std::fs::read(d.folders.target.join("ORDER_A.txt")).unwrap(),
        bytes
    );
    assert_eq!(
        names_in(&d.folders.target),
        vec!["ORDER_A.txt"],
        "nothing but the file in the reader's folder - no temp name, no staging folder"
    );
    assert!(d.folders.staging.is_dir(), "staging created on demand");
    assert!(
        names_in(&d.folders.staging).is_empty(),
        "the .part is removed"
    );
}

#[test]
fn test_write_file_never_overwrites_an_existing_name() {
    let d = dirs();
    std::fs::write(d.folders.target.join("ORDER_A.txt"), b"first").unwrap();
    let mut contacted = false;
    let outcome = write_file(&d.folders, 813, &payload("ORDER_A.txt", b"second"), || {
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
        std::fs::read(d.folders.target.join("ORDER_A.txt")).unwrap(),
        b"first"
    );
    assert!(names_in(&d.folders.staging).is_empty());
}

#[test]
fn test_write_file_refuses_a_name_that_appears_between_check_and_link() {
    // the authoritative check is the link itself: a file the reader drops in
    // after the pre-check (or a second job of the same name) is never replaced
    let d = dirs();
    let target = d.folders.target.join("ORDER_RACE.txt");
    let outcome = write_file(&d.folders, 814, &payload("ORDER_RACE.txt", b"ours"), || {
        std::fs::write(&target, b"theirs").unwrap();
        Ok(())
    });
    assert!(
        matches!(&outcome, Outcome::Failed { code, .. } if code == CODE_FILE_EXISTS),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    assert!(names_in(&d.folders.staging).is_empty(), "our .part is gone");
}

#[test]
fn test_write_file_link_failure_other_than_exists_is_write_failed() {
    // the folder vanishes after the contact is journaled: the link fails
    // with NotFound, the name never appeared
    let d = dirs();
    let target_dir = d.folders.target.clone();
    let outcome = write_file(&d.folders, 815, &payload("ORDER_GONE.txt", b"x"), || {
        std::fs::remove_dir(&target_dir).unwrap();
        Ok(())
    });
    let Outcome::Failed { code, text } = outcome else {
        panic!("not failed: {outcome:?}");
    };
    assert_eq!(code, CODE_WRITE_FAILED);
    assert!(
        text.contains("naming the file ORDER_GONE.txt failed") && text.contains("nothing ran"),
        "{text}"
    );
    assert!(names_in(&d.folders.staging).is_empty());
}

#[test]
fn test_write_file_without_the_folder_writes_nothing() {
    let d = dirs();
    let missing = Folders {
        target: d.folders.target.join("in"),
        staging: d.folders.staging.clone(),
    };
    let outcome = write_file(&missing, 816, &payload("ORDER_A.txt", b"x"), ok_contact);
    assert!(
        matches!(&outcome, Outcome::Failed { code, text } if code == CODE_NO_TARGET_DIR && text.contains("nothing ran")),
        "{outcome:?}"
    );
    assert!(
        !missing.target.exists(),
        "the target folder is never created"
    );
    assert!(!d.folders.staging.exists(), "nothing staged either");
}

#[test]
fn test_write_file_journal_failure_names_nothing() {
    let d = dirs();
    let outcome = write_file(&d.folders, 817, &payload("ORDER_A.txt", b"x"), || {
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
    assert!(names_in(&d.folders.target).is_empty());
    assert!(names_in(&d.folders.staging).is_empty());
}

#[test]
fn test_write_file_refuses_an_unusable_staging_folder() {
    // a FILE where the staging folder must be
    let d = dirs();
    std::fs::write(&d.folders.staging, b"not a folder").unwrap();
    let mut contacted = false;
    let outcome = write_file(&d.folders, 818, &payload("ORDER_A.txt", b"x"), || {
        contacted = true;
        Ok(())
    });
    let Outcome::Failed { code, text } = outcome else {
        panic!("not failed: {outcome:?}");
    };
    assert_eq!(code, CODE_WRITE_FAILED);
    assert!(
        text.contains("is not a folder") && text.contains("nothing ran"),
        "{text}"
    );
    assert!(!contacted);
    assert!(names_in(&d.folders.target).is_empty());
}

#[test]
fn test_write_file_cannot_create_the_staging_folder() {
    // the data dir itself is missing: staging is never created with parents
    let d = dirs();
    let orphan = Folders {
        target: d.folders.target.clone(),
        staging: d.data.join("gone").join(STAGING_DIR),
    };
    let outcome = write_file(&orphan, 819, &payload("ORDER_A.txt", b"x"), ok_contact);
    assert!(
        matches!(&outcome, Outcome::Failed { code, text } if code == CODE_WRITE_FAILED && text.contains("could not be created")),
        "{outcome:?}"
    );
    assert!(!d.data.join("gone").exists());
}

#[cfg(unix)]
#[test]
fn test_write_file_never_follows_a_staging_link() {
    // the junction attack: the staging folder replaced by a link elsewhere
    let d = dirs();
    let elsewhere = d.data.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &d.folders.staging).unwrap();
    let outcome = write_file(&d.folders, 820, &payload("ORDER_A.txt", b"x"), ok_contact);
    assert!(
        matches!(&outcome, Outcome::Failed { code, text } if code == CODE_WRITE_FAILED && text.contains("link / reparse point")),
        "{outcome:?}"
    );
    assert!(
        names_in(&elsewhere).is_empty(),
        "nothing written through the link"
    );
    assert!(names_in(&d.folders.target).is_empty());
}

#[cfg(unix)]
#[test]
fn test_write_file_replaces_a_planted_part_without_following_it() {
    // a planted job-<id>.part (here a link to a file elsewhere) is removed,
    // never opened: the file it points to stays untouched
    let d = dirs();
    std::fs::create_dir(&d.folders.staging).unwrap();
    let victim = d.data.join("victim.txt");
    std::fs::write(&victim, b"precious").unwrap();
    std::os::unix::fs::symlink(&victim, part_path(&d.folders.staging, 821)).unwrap();
    let outcome = write_file(&d.folders, 821, &payload("ORDER_P.txt", b"edi"), ok_contact);
    assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert_eq!(
        std::fs::read(d.folders.target.join("ORDER_P.txt")).unwrap(),
        b"edi"
    );
}

#[test]
fn test_staging_paths() {
    assert_eq!(
        staging_dir(Path::new("/data")),
        Path::new("/data").join("file-write-staging")
    );
    assert_eq!(STAGING_DIR, "file-write-staging");
    assert_eq!(
        part_path(Path::new("/s"), 812),
        Path::new("/s").join("job-812.part")
    );
}

fn entry_for(journal: &Journal, j: &DeviceJob) -> JournalEntry {
    journal.insert_received(j).unwrap();
    journal.get(j.id).unwrap().unwrap()
}

#[test]
fn test_reconcile_without_device_contact_is_not_executed_and_cleans_staging() {
    let d = dirs();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(900, JOB_TYPE, payload_json("ORDER_R.txt", b"abc"));
    let entry = entry_for(&journal, &j);
    // a .part left behind by the dead process
    std::fs::create_dir(&d.folders.staging).unwrap();
    let part = part_path(&d.folders.staging, 900);
    std::fs::write(&part, b"ab").unwrap();
    let answer = reconcile_file(&d.folders, &entry);
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
    let d = dirs();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(901, JOB_TYPE, payload_json("ORDER_R.txt", b"abc"));
    journal.insert_received(&j).unwrap();
    journal.mark_device_contact(901).unwrap();
    let entry = journal.get(901).unwrap().unwrap();

    // absent: never named, or the reader archived it already
    let absent = reconcile_file(&d.folders, &entry);
    assert!(
        matches!(&absent, Reconciliation::StillUnknown(r) if r.contains("already archived")),
        "{absent:?}"
    );
    // another file of that name: unknown, never "done"
    std::fs::write(d.folders.target.join("ORDER_R.txt"), b"other").unwrap();
    let other = reconcile_file(&d.folders, &entry);
    assert!(
        matches!(&other, Reconciliation::StillUnknown(r) if r.contains("other content")),
        "{other:?}"
    );
    // our bytes under the final name: done; a leftover .part is removed
    std::fs::write(d.folders.target.join("ORDER_R.txt"), b"abc").unwrap();
    std::fs::create_dir(&d.folders.staging).unwrap();
    let part = part_path(&d.folders.staging, 901);
    std::fs::write(&part, b"abc").unwrap();
    assert_eq!(
        reconcile_file(&d.folders, &entry),
        Reconciliation::Done(
            json!({"filename": "ORDER_R.txt", "size": 3, "sha256": sha256_hex(b"abc")})
        )
    );
    assert!(!part.exists());
    assert_eq!(
        std::fs::read(d.folders.target.join("ORDER_R.txt")).unwrap(),
        b"abc",
        "reconcile never writes the target"
    );
}

#[test]
fn test_reconcile_unreadable_target_and_payload_stay_unknown() {
    let d = dirs();
    let journal = Journal::open_in_memory().unwrap();
    let j = job(902, JOB_TYPE, payload_json("ORDER_D.txt", b"abc"));
    journal.insert_received(&j).unwrap();
    journal.mark_device_contact(902).unwrap();
    let entry = journal.get(902).unwrap().unwrap();
    // a FOLDER under the target name cannot be read as the file
    std::fs::create_dir(d.folders.target.join("ORDER_D.txt")).unwrap();
    let unreadable = reconcile_file(&d.folders, &entry);
    assert!(
        matches!(&unreadable, Reconciliation::StillUnknown(r) if r.contains("could not be read")),
        "{unreadable:?}"
    );
    let mut broken = entry.clone();
    broken.job.payload = json!({"filename": "x"});
    let answer = reconcile_file(&d.folders, &broken);
    assert!(
        matches!(&answer, Reconciliation::StillUnknown(r) if r.contains("payload is unreadable")),
        "{answer:?}"
    );
}

#[test]
fn test_remove_part_reports_whether_the_part_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("job-1.part");
    std::fs::write(&part, b"x").unwrap();
    assert!(remove_part(&part, 1), "removed");
    assert!(!part.exists());
    assert!(remove_part(&part, 1), "never there = gone");
    // a FOLDER under that name cannot be removed as a file
    std::fs::create_dir(&part).unwrap();
    assert!(!remove_part(&part, 1), "not removable = not gone");
    assert!(part.exists());
}

fn executor(d: &Dirs, journal: &Arc<Journal>) -> FileWriteExecutor {
    FileWriteExecutor::new(
        &FileWriteConfig {
            target_dir: d.folders.target.to_string_lossy().into_owned(),
        },
        &d.data,
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
        Path::new("/tmp"),
        journal,
    ) else {
        panic!("a relative folder is refused");
    };
    let err = err.to_string();
    assert!(err.contains("absolute"), "{err}");
}

#[test]
fn test_new_creates_staging_in_the_data_dir() {
    let d = dirs();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&d, &journal);
    assert_eq!(e.folders(), &d.folders);
    assert!(d.folders.staging.is_dir());
    assert!(
        names_in(&d.folders.staging).is_empty(),
        "the hard-link probe leaves nothing behind"
    );
    assert_eq!(e.status(), "ok");
}

#[test]
fn test_check_routes_only_file_write_with_a_valid_payload() {
    let d = dirs();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&d, &journal);
    assert_eq!(e.kind(), DeviceKind::File);
    assert_eq!(e.name(), "file");
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
    let d = dirs();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&d, &journal);
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
        std::fs::read(d.folders.target.join("ORDER_E.txt")).unwrap(),
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
async fn test_status_follows_the_folders() {
    let d = dirs();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&d, &journal);
    assert_eq!(e.status(), "ok");
    std::fs::remove_dir(&d.folders.staging).unwrap();
    std::fs::write(&d.folders.staging, b"file").unwrap();
    assert_eq!(e.status(), "ok", "status() never probes, it is cached");
    e.refresh_status().await;
    assert_eq!(e.status(), STATUS_STAGING_UNSAFE);
    std::fs::remove_dir(&d.folders.target).unwrap();
    e.refresh_status().await;
    assert_eq!(e.status(), STATUS_NO_TARGET_DIR);
    let outcome = e
        .execute(&job(960, JOB_TYPE, payload_json("A.txt", b"x")))
        .await;
    assert!(
        matches!(&outcome, Outcome::Failed { code, .. } if code == CODE_NO_TARGET_DIR),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_background_sweeps_only_parts_no_unknown_outcome_needs() {
    let d = dirs();
    let journal = Arc::new(Journal::open_in_memory().unwrap());
    let e = executor(&d, &journal);
    // 970: final (done) -> swept; 971: unknown to the journal -> swept;
    // 972: still in flight -> kept for its reconcile; other names untouched
    for id in [970, 972] {
        journal
            .insert_received(&job(id, JOB_TYPE, payload_json("A.txt", b"x")))
            .unwrap();
    }
    journal
        .record_outcome(970, &Outcome::Done(json!({})))
        .unwrap();
    for id in [970, 971, 972] {
        std::fs::write(part_path(&d.folders.staging, id), b"x").unwrap();
    }
    std::fs::write(d.folders.staging.join("notes.txt"), b"x").unwrap();
    e.background().await;
    assert_eq!(
        names_in(&d.folders.staging),
        vec!["job-972.part", "notes.txt"]
    );
}
