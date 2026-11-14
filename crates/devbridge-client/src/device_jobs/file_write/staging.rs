//! The staging folder of the `file.write` executor (#130): where a file is
//! written, synced and verified BEFORE it gets its name in the target folder.
//!
//! It lives in the BRIDGE's data dir (`<data_dir>\file-write-staging`), never
//! inside the target folder: Codex' import folder grants every local user
//! full control, so a predictable path there could be swapped for a junction
//! and turn the SYSTEM service's write / delete into one anywhere on the
//! machine (review of #130). The installer creates this folder with an ACL of
//! its own (SYSTEM + Administrators, files readable / movable by Users); the
//! service creates it when missing and refuses it whenever it is a link /
//! reparse point. The final name comes from a hard link, so staging and
//! target must be on ONE volume — probed at start.

use std::path::{Component, Path, PathBuf};

use super::super::journal::Journal;
use super::platform::is_reparse_point;

/// Name of the staging folder in the bridge's data dir.
pub const STAGING_DIR: &str = "file-write-staging";

/// Heartbeat status: the staging folder is a link / not a folder.
pub const STATUS_STAGING_UNSAFE: &str = "staging_unsafe";
/// Heartbeat status: staging and target are on different volumes.
pub const STATUS_NOT_SAME_VOLUME: &str = "not_same_volume";
/// Heartbeat status: the volume cannot hard-link (FAT / exFAT …).
pub const STATUS_NO_HARD_LINK: &str = "no_hard_link";

/// The staging folder of a bridge whose data dir is `data_dir`.
pub fn staging_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(STAGING_DIR)
}

/// The `.part` file of `job_id` in `staging`.
pub fn part_path(staging: &Path, job_id: i64) -> PathBuf {
    staging.join(format!("job-{job_id}.part"))
}

/// The job id of a staging file name `job-<id>.part` (`None` = not ours).
pub fn part_job_id(name: &str) -> Option<i64> {
    name.strip_prefix("job-")?
        .strip_suffix(".part")?
        .parse()
        .ok()
}

/// Why `staging` must not be used (`None` = a real folder): missing, a
/// symlink / junction / any reparse point, or not a folder. Never follows a
/// link.
pub fn staging_problem(staging: &Path) -> Option<String> {
    match std::fs::symlink_metadata(staging) {
        Err(e) => Some(format!("{} cannot be read ({e})", staging.display())),
        Ok(md) if md.file_type().is_symlink() || is_reparse_point(&md) => Some(format!(
            "{} is a link / reparse point - refused (it could point anywhere)",
            staging.display()
        )),
        Ok(md) if !md.is_dir() => Some(format!("{} is not a folder", staging.display())),
        Ok(_) => None,
    }
}

/// Create `staging` when missing (never its parents: the data dir exists),
/// then [`staging_problem`].
pub fn ensure_staging(staging: &Path) -> Option<String> {
    if let Err(e) = std::fs::create_dir(staging)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Some(format!("{} could not be created ({e})", staging.display()));
    }
    staging_problem(staging)
}

/// The volume a path lives on: the first component of its canonical form
/// (`\\?\C:` on Windows, `/` elsewhere). `None` = cannot be resolved.
pub fn volume_key(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let first = canonical.components().next()?;
    Some(match first {
        Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().to_ascii_uppercase(),
        other => other.as_os_str().to_string_lossy().into_owned(),
    })
}

/// Can `staging` hold a hard link? Creates and removes two probe files in it
/// (never in the target folder, whose reader imports every file).
pub fn probe_hard_link(staging: &Path) -> bool {
    let probe = staging.join("probe.tmp");
    let link = staging.join("probe-link.tmp");
    let _ = std::fs::remove_file(&link);
    let ok = std::fs::write(&probe, b"probe").is_ok() && std::fs::hard_link(&probe, &link).is_ok();
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The heartbeat status of the folders (`ok`, or what is wrong) — target
/// first, then staging, then the volume, then the probe result.
pub fn folders_status(target: &Path, staging: &Path, hard_link_ok: bool) -> &'static str {
    if !target.is_dir() {
        return super::STATUS_NO_TARGET_DIR;
    }
    if staging_problem(staging).is_some() {
        return STATUS_STAGING_UNSAFE;
    }
    if volume_key(target) != volume_key(staging) {
        return STATUS_NOT_SAME_VOLUME;
    }
    if !hard_link_ok {
        return STATUS_NO_HARD_LINK;
    }
    super::STATUS_OK
}

/// Remove the `.part` files no unknown outcome needs any more: the job is
/// final in the journal, or unknown to it (a `.part` that survived its
/// removal, e.g. a sharing violation while the reader held the file). A
/// `.part` of a job still `indeterminate` stays for its reconcile. Returns
/// how many were removed.
pub fn sweep(staging: &Path, journal: &Journal) -> usize {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(job_id) = part_job_id(&name) else {
            continue;
        };
        let still_needed = journal
            .get(job_id)
            .ok()
            .flatten()
            .is_some_and(|e| !e.state.is_final());
        if !still_needed && super::remove_part(&entry.path(), job_id) {
            removed += 1;
        }
    }
    removed
}
