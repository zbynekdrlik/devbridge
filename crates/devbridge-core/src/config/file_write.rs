//! `[client.pokladna.file_write]` — the folder the `file.write` device jobs
//! write into (#130: the Codex EDI ORDER files on the Codex server). The
//! folder lives in the config ONLY: a job carries a plain file name, never a
//! path (contract odoo-erp `addons/company_pokladna_api/docs/
//! device_job_file_write.md`).

use serde::{Deserialize, Serialize};

/// `[client.pokladna.file_write]` — present = `file.*` jobs run on this
/// bridge (the file executor). Absent = they are refused `unsupported`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileWriteConfig {
    /// Absolute folder the files land in, e.g. `C:\ORION\COMMUNICATOR\data\in`.
    /// It must exist: devbridge never creates it (only its own staging
    /// subfolder inside it, on the first job).
    pub target_dir: String,
}

/// Why `target_dir` is unusable (`None` = fine): an absolute local path
/// (`C:\…` / `C:/…`, or `/…` off Windows), no `..` component, no control
/// character, no UNC share (a network share gives no atomic no-replace
/// link and no durable sync).
pub fn target_dir_problem(dir: &str) -> Option<&'static str> {
    if dir.trim().is_empty() {
        return Some("target_dir is empty");
    }
    if dir.chars().any(char::is_control) {
        return Some("target_dir contains a control character");
    }
    if dir.starts_with("\\\\") || dir.starts_with("//") {
        return Some("target_dir must be a local folder, not a network share");
    }
    let bytes = dir.as_bytes();
    let windows_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !windows_absolute && !dir.starts_with('/') {
        return Some("target_dir must be an absolute path (e.g. C:\\folder)");
    }
    if dir.split(['\\', '/']).any(|part| part == "..") {
        return Some("target_dir must not contain a '..' component");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_dir_problem_accepts_absolute_local_folders() {
        for ok in [
            "C:\\ORION\\COMMUNICATOR\\data\\in",
            "C:\\ORION\\COMMUNICATOR\\data\\in\\",
            "c:/ProgramData/DevBridge-E2E/file-write-in",
            "/var/lib/devbridge/in",
            "D:\\a..b",
        ] {
            assert_eq!(target_dir_problem(ok), None, "{ok}");
        }
    }

    #[test]
    fn test_target_dir_problem_refuses_relative_shares_traversal_and_controls() {
        let cases = [
            ("", "target_dir is empty"),
            ("   ", "target_dir is empty"),
            ("data\\in", "absolute"),
            ("C:", "absolute"),
            ("C:in", "absolute"),
            ("1:\\in", "absolute"),
            ("\\\\server\\share\\in", "network share"),
            ("//server/share", "network share"),
            ("C:\\ORION\\..\\Windows", "'..'"),
            ("/var/../etc", "'..'"),
            ("C:\\in\n", "control character"),
            ("C:\\in\u{0}", "control character"),
        ];
        for (dir, want) in cases {
            let problem = target_dir_problem(dir).unwrap_or_else(|| panic!("{dir:?} accepted"));
            assert!(problem.contains(want), "{dir:?}: {problem}");
        }
    }

    #[test]
    fn test_file_write_config_parses() {
        let parsed: FileWriteConfig =
            toml::from_str("target_dir = 'C:\\ORION\\COMMUNICATOR\\data\\in'").unwrap();
        assert_eq!(parsed.target_dir, "C:\\ORION\\COMMUNICATOR\\data\\in");
        assert!(
            toml::from_str::<FileWriteConfig>("").is_err(),
            "target_dir is required"
        );
    }

    #[test]
    fn test_pokladna_block_carries_the_file_write_table() {
        let p: crate::config::PokladnaClientConfig = toml::from_str(
            "enabled = true\nurl = 'https://erp.example.test'\napi_key = 'k'\nfiscal = false\n\n[file_write]\ntarget_dir = 'C:/in'\n",
        )
        .unwrap();
        assert_eq!(
            p.file_write,
            Some(FileWriteConfig {
                target_dir: "C:/in".into()
            })
        );
        assert!(!p.fiscal);
        let absent: crate::config::PokladnaClientConfig = toml::from_str("enabled = true").unwrap();
        assert_eq!(absent.file_write, None, "absent = no file executor");
        assert!(
            format!("{p:?}")
                .contains("file_write: Some(FileWriteConfig { target_dir: \"C:/in\" })")
        );
    }
}
