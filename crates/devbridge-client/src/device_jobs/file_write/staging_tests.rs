//! The staging folder of `file.write` (#130): never a link, created without
//! parents, one volume with the target, a hard-link probe that leaves
//! nothing behind, `.part` names, the status order.

use std::path::Path;

use super::staging::*;
use super::{STATUS_NO_TARGET_DIR, STATUS_OK};

#[test]
fn test_part_job_id() {
    assert_eq!(part_job_id("job-812.part"), Some(812));
    assert_eq!(part_job_id("job--3.part"), Some(-3));
    for other in [
        "job-812.txt",
        "jobs-812.part",
        "job-x.part",
        "812.part",
        "probe.tmp",
        "",
    ] {
        assert_eq!(part_job_id(other), None, "{other}");
    }
}

#[test]
fn test_staging_problem_and_ensure() {
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join(STAGING_DIR);
    let missing = staging_problem(&staging).unwrap();
    assert!(missing.contains("cannot be read"), "{missing}");
    assert_eq!(ensure_staging(&staging), None, "created");
    assert!(staging.is_dir());
    assert_eq!(ensure_staging(&staging), None, "already there is fine");
    assert_eq!(staging_problem(&staging), None);
    let file = root.path().join("plain");
    std::fs::write(&file, b"x").unwrap();
    assert!(staging_problem(&file).unwrap().contains("is not a folder"));
    assert!(ensure_staging(&file).unwrap().contains("is not a folder"));
    let orphan = root.path().join("no-parent").join(STAGING_DIR);
    assert!(
        ensure_staging(&orphan)
            .unwrap()
            .contains("could not be created")
    );
}

#[cfg(unix)]
#[test]
fn test_staging_problem_refuses_links() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.path().join(STAGING_DIR);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(
        staging_problem(&link)
            .unwrap()
            .contains("link / reparse point")
    );
    assert!(
        ensure_staging(&link)
            .unwrap()
            .contains("link / reparse point")
    );
}

#[test]
fn test_volume_key() {
    let root = tempfile::tempdir().unwrap();
    let key = volume_key(root.path()).unwrap();
    #[cfg(unix)]
    assert_eq!(key, "/");
    #[cfg(windows)]
    assert!(key.ends_with(':'), "{key}");
    assert_eq!(volume_key(&root.path().join("missing")), None);
}

#[test]
fn test_probe_hard_link_leaves_nothing() {
    let root = tempfile::tempdir().unwrap();
    assert!(probe_hard_link(root.path()));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    assert!(!probe_hard_link(&root.path().join("missing")));
}

#[test]
fn test_folders_status_order() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("in");
    let staging = root.path().join(STAGING_DIR);
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&staging).unwrap();
    assert_eq!(folders_status(&target, &staging, true), STATUS_OK);
    assert_eq!(
        folders_status(&target, &staging, false),
        STATUS_NO_HARD_LINK
    );
    assert_eq!(
        folders_status(&target, &root.path().join("gone"), true),
        STATUS_STAGING_UNSAFE
    );
    assert_eq!(
        folders_status(&root.path().join("gone"), &staging, true),
        STATUS_NO_TARGET_DIR
    );
    assert_eq!(STATUS_NOT_SAME_VOLUME, "not_same_volume");
}

#[test]
fn test_sweep_without_a_staging_folder_is_zero() {
    let journal = crate::device_jobs::journal::Journal::open_in_memory().unwrap();
    assert_eq!(
        sweep(Path::new("/nonexistent-devbridge-staging"), &journal),
        0
    );
}
