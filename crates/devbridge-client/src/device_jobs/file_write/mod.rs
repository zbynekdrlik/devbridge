//! The `file.write` executor (#130): writes a file Odoo composes into the
//! folder of `[client.pokladna.file_write]` — the first one is the Codex EDI
//! ORDER of a store's order, written into Codex' import folder on the Codex
//! server (contract odoo-erp `addons/company_pokladna_api/docs/
//! device_job_file_write.md`, copy + golden job in `testdata/`).
//!
//! - The FOLDER comes from the config only; the payload names a plain file
//!   ([`payload::parse_payload`], checked before journaling → `bad_payload`).
//! - **Never a half-written file under the final name, never an overwrite:**
//!   the bytes go to `<data_dir>\file-write-staging\job-<id>.part`
//!   ([`staging`]: the BRIDGE's data dir, never the reader's folder — that
//!   one is writable by every local user, a predictable path there could be
//!   swapped for a junction), created new, synced and read back (size +
//!   sha256), the device contact is journaled, then [`std::fs::hard_link`]
//!   gives them the final name — atomic, and it FAILS when the name exists
//!   (`file_exists`, nothing touched). The `.part` name is removed after.
//!   `std::fs::rename` would OVERWRITE on Windows (`MoveFileExW` with
//!   `MOVEFILE_REPLACE_EXISTING`). Staging and target share one volume
//!   (probed at start, heartbeat `not_same_volume` / `no_hard_link`).
//! - The file is never moved or deleted afterwards (the reader archives it).
//! - Reconcile after a restart: no journaled device contact → `failed
//!   not_executed` (the link was never tried); the final file holds the
//!   job's sha256 → `done`; anything else stays unknown (the reader may have
//!   archived the file already, so "absent" proves nothing). A `.part` no
//!   unknown outcome needs is swept at start.
//!
//! Logs: job id, file name, size, sha256 and the outcome — never the content.

pub mod payload;
mod platform;
pub mod staging;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{info, warn};

use devbridge_core::config::{FileWriteConfig, target_dir_problem};

use self::payload::{FilePayload, parse_payload, sha256_hex};
use super::api::DeviceJob;
use super::executor::{
    CODE_UNSUPPORTED, DeviceExecutor, DeviceKind, Outcome, Reconciliation, Rejection,
};
use super::journal::{Journal, JournalEntry};

/// The one `file.*` type of the contract.
pub const JOB_TYPE: &str = "file.write";

/// The payload breaks the contract — refused before journaling, nothing ran.
pub const CODE_BAD_PAYLOAD: &str = "bad_payload";
/// A file of that name is already in the folder — never overwritten.
pub const CODE_FILE_EXISTS: &str = "file_exists";
/// The configured folder does not exist — nothing was written.
pub const CODE_NO_TARGET_DIR: &str = "no_target_dir";
/// Writing / verifying / naming the file failed — the final name never
/// appeared.
pub const CODE_WRITE_FAILED: &str = "write_failed";
/// The journal could not record the device contact — nothing was named.
pub const CODE_JOURNAL: &str = "bridge_error";
/// Reconcile: the file was never named (no device contact journaled).
pub const CODE_NOT_EXECUTED: &str = "not_executed";

/// Heartbeat status: the folder exists.
pub const STATUS_OK: &str = "ok";
/// Heartbeat status: the configured folder is missing.
pub const STATUS_NO_TARGET_DIR: &str = "no_target_dir";
pub use staging::{STATUS_NO_HARD_LINK, STATUS_NOT_SAME_VOLUME, STATUS_STAGING_UNSAFE};

/// The two folders of the executor: where files are named (the reader's)
/// and where they are written first (the bridge's own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folders {
    pub target: PathBuf,
    pub staging: PathBuf,
}

/// The `done` result: what is on disk under the final name.
pub fn done_result(payload: &FilePayload) -> Value {
    json!({
        "filename": payload.filename,
        "size": payload.bytes.len(),
        "sha256": payload.sha256,
    })
}

/// Remove our own `.part` file (best effort, logged) — never the target.
/// `true` = it is gone (removed, or never there).
pub fn remove_part(part: &Path, job_id: i64) -> bool {
    match std::fs::remove_file(part) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!(job_id, part = %part.display(), error = %e, "file.write: could not remove the staging file (harmless: the staging folder is the bridge's own; swept at the next start)");
            false
        }
    }
}

/// Write `bytes` to a NEW `part` (an existing name is never opened, so a
/// planted file / link there is never followed), sync it, and read it back:
/// size + sha256 must match what was asked.
fn write_and_verify(part: &Path, payload: &FilePayload) -> std::io::Result<Result<(), String>> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)?;
    file.write_all(&payload.bytes)?;
    file.sync_all()?;
    drop(file);
    let back = std::fs::read(part)?;
    if back.len() != payload.bytes.len() {
        return Ok(Err(format!(
            "the staging file holds {} bytes, {} were written",
            back.len(),
            payload.bytes.len()
        )));
    }
    let sha = sha256_hex(&back);
    if sha != payload.sha256 {
        return Ok(Err(format!(
            "the staging file's sha256 {sha} is not the payload's {}",
            payload.sha256
        )));
    }
    Ok(Ok(()))
}

/// Run one `file.write` job (blocking I/O): stage, verify, journal the
/// device contact (`mark_contact`), name it — see the module docs. `failed`
/// only when the final name never appeared.
pub fn write_file(
    folders: &Folders,
    job_id: i64,
    payload: &FilePayload,
    mark_contact: impl FnOnce() -> Result<()>,
) -> Outcome {
    if !folders.target.is_dir() {
        return Outcome::failed(
            CODE_NO_TARGET_DIR,
            format!(
                "the target folder {} does not exist - nothing was written, nothing ran",
                folders.target.display()
            ),
        );
    }
    let target = folders.target.join(&payload.filename);
    if target.symlink_metadata().is_ok() {
        return Outcome::failed(CODE_FILE_EXISTS, exists_text(&payload.filename));
    }
    if let Some(problem) = staging::ensure_staging(&folders.staging) {
        return Outcome::failed(
            CODE_WRITE_FAILED,
            format!("{problem} - nothing was written, nothing ran"),
        );
    }
    let part = staging::part_path(&folders.staging, job_id);
    // a .part of this id can only be a leftover / planted name: never reused
    remove_part(&part, job_id);
    let staged = write_and_verify(&part, payload);
    let problem = match staged {
        Ok(Ok(())) => None,
        Ok(Err(mismatch)) => Some(mismatch),
        Err(e) => Some(format!("writing the staging file failed: {e}")),
    };
    if let Some(problem) = problem {
        remove_part(&part, job_id);
        return Outcome::failed(
            CODE_WRITE_FAILED,
            format!("{problem} - the file was not named, nothing ran"),
        );
    }
    if let Err(e) = mark_contact() {
        remove_part(&part, job_id);
        return Outcome::failed(
            CODE_JOURNAL,
            format!(
                "the journal could not record the write ({e:#}) - the file was not named, nothing ran"
            ),
        );
    }
    let linked = std::fs::hard_link(&part, &target);
    remove_part(&part, job_id);
    match linked {
        Ok(()) => Outcome::Done(done_result(payload)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Outcome::failed(CODE_FILE_EXISTS, exists_text(&payload.filename))
        }
        Err(e) => Outcome::failed(
            CODE_WRITE_FAILED,
            format!(
                "naming the file {} failed: {e} - nothing ran",
                payload.filename
            ),
        ),
    }
}

fn exists_text(filename: &str) -> String {
    format!(
        "a file named {filename} is already in the target folder - never overwritten, nothing ran"
    )
}

/// What happened to a journaled job whose outcome is unknown (blocking I/O)
/// — see the module docs. Never writes the file.
pub fn reconcile_file(folders: &Folders, entry: &JournalEntry) -> Reconciliation {
    let job_id = entry.job.id;
    let payload = match parse_payload(&entry.job.payload) {
        Ok(payload) => payload,
        Err(problem) => {
            return Reconciliation::StillUnknown(format!(
                "the journaled payload is unreadable ({problem}) - a person must check the target folder"
            ));
        }
    };
    let part = staging::part_path(&folders.staging, job_id);
    if entry.device_contact_at.is_none() {
        remove_part(&part, job_id);
        return Reconciliation::Failed {
            code: CODE_NOT_EXECUTED.to_string(),
            text: format!(
                "devbridge stopped before it named {} (the journal has no write) - nothing ran",
                payload.filename
            ),
        };
    }
    let target = folders.target.join(&payload.filename);
    match std::fs::read(&target) {
        Ok(bytes) if sha256_hex(&bytes) == payload.sha256 => {
            remove_part(&part, job_id);
            Reconciliation::Done(done_result(&payload))
        }
        Ok(_) => Reconciliation::StillUnknown(format!(
            "a file named {} holds other content - this job's file may never have been named; a person must check",
            payload.filename
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Reconciliation::StillUnknown(format!(
                "{} is not in the target folder - it was never named, or the reader already archived it; a person must check",
                payload.filename
            ))
        }
        Err(e) => Reconciliation::StillUnknown(format!(
            "{} could not be read ({e}) - asked again later",
            payload.filename
        )),
    }
}

/// The `file.*` executor — see the module docs.
pub struct FileWriteExecutor {
    folders: Folders,
    journal: Arc<Journal>,
    /// The staging volume can hard-link (probed once at start).
    hard_link_ok: bool,
    status: Mutex<&'static str>,
}

impl FileWriteExecutor {
    /// An executor writing into `config.target_dir` (validated again here),
    /// staging in `data_dir` (the bridge's own data dir).
    pub fn new(config: &FileWriteConfig, data_dir: &Path, journal: Arc<Journal>) -> Result<Self> {
        if let Some(problem) = target_dir_problem(&config.target_dir) {
            bail!("[client.pokladna.file_write]: {problem}");
        }
        let folders = Folders {
            target: PathBuf::from(&config.target_dir),
            staging: staging::staging_dir(data_dir),
        };
        // one line, no branch: an unusable staging folder shows in `status`
        let staging_problem =
            staging::ensure_staging(&folders.staging).unwrap_or_else(|| "none".to_string());
        let hard_link_ok = staging::probe_hard_link(&folders.staging);
        let status = staging::folders_status(&folders.target, &folders.staging, hard_link_ok);
        info!(target_dir = %folders.target.display(), staging = %folders.staging.display(), staging_problem = %staging_problem, hard_link_ok, status, "file.write executor: files are staged in the bridge's data dir and named in the target folder (status != ok = every file job fails 'nothing ran' until fixed)");
        Ok(Self {
            folders,
            journal,
            hard_link_ok,
            status: Mutex::new(status),
        })
    }

    pub fn folders(&self) -> &Folders {
        &self.folders
    }
}

#[async_trait]
impl DeviceExecutor for FileWriteExecutor {
    fn kind(&self) -> DeviceKind {
        DeviceKind::File
    }

    fn name(&self) -> &str {
        "file"
    }

    fn check(&self, job: &DeviceJob) -> Result<(), Rejection> {
        if job.job_type != JOB_TYPE {
            return Err(Rejection {
                code: CODE_UNSUPPORTED,
                text: format!(
                    "devbridge runs only '{JOB_TYPE}' of the file jobs ('{}' is not one) - nothing was written, nothing ran",
                    job.job_type
                ),
            });
        }
        parse_payload(&job.payload)
            .map(|_| ())
            .map_err(|problem| Rejection {
                code: CODE_BAD_PAYLOAD,
                text: format!("{problem} - nothing was written, nothing ran"),
            })
    }

    async fn execute(&self, job: &DeviceJob) -> Outcome {
        let payload = match parse_payload(&job.payload) {
            Ok(payload) => payload,
            Err(problem) => {
                return Outcome::failed(
                    CODE_BAD_PAYLOAD,
                    format!("{problem} - nothing was written, nothing ran"),
                );
            }
        };
        let job_id = job.id;
        let filename = payload.filename.clone();
        let size = payload.bytes.len();
        let sha256 = payload.sha256.clone();
        let folders = self.folders.clone();
        let journal = Arc::clone(&self.journal);
        let outcome = tokio::task::spawn_blocking(move || {
            write_file(&folders, job_id, &payload, || {
                journal.mark_device_contact(job_id).map(|_| ())
            })
        })
        .await
        .unwrap_or_else(|e| {
            Outcome::Indeterminate(format!("the file write stopped unexpectedly: {e}"))
        });
        let detail = match &outcome {
            Outcome::Done(_) => String::new(),
            Outcome::Failed { code, text } => format!("{code}: {text}"),
            Outcome::Indeterminate(reason) => reason.clone(),
        };
        info!(job_id, filename = %filename, size, sha256 = %sha256, target_dir = %self.folders.target.display(), outcome = outcome.label(), detail = %detail, "file.write job finished");
        outcome
    }

    async fn reconcile(&self, entry: &JournalEntry) -> Reconciliation {
        let folders = self.folders.clone();
        let asked = entry.clone();
        tokio::task::spawn_blocking(move || reconcile_file(&folders, &asked))
            .await
            .unwrap_or_else(|e| Reconciliation::StillUnknown(format!("reconcile failed: {e}")))
    }

    fn status(&self) -> String {
        (*self.status.lock().unwrap_or_else(|p| p.into_inner())).to_string()
    }

    async fn refresh_status(&self) {
        let status = staging::folders_status(
            &self.folders.target,
            &self.folders.staging,
            self.hard_link_ok,
        );
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    }

    /// Once at start: sweep the `.part` files no unknown outcome needs.
    async fn background(&self) {
        let staging = self.folders.staging.clone();
        let journal = Arc::clone(&self.journal);
        let removed = tokio::task::spawn_blocking(move || staging::sweep(&staging, &journal))
            .await
            .unwrap_or_default();
        info!(staging = %self.folders.staging.display(), removed, "file.write: staging folder swept (.part files of finished jobs)");
    }
}

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod payload_tests;
#[cfg(test)]
mod staging_tests;
#[cfg(test)]
mod tests;
