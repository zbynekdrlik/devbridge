//! Version exposure check (issue #82).
//!
//! Every dashboard renders `v<version>` in its sidebar from the `version` field
//! of the dashboard API, so both the deployed E2E server and client must expose
//! a non-empty `MAJOR.MINOR.PATCH` version on `/api/status`. The DOM side of the
//! same feature is covered by `playwright/tests/version.spec.ts`.

use anyhow::{Result, ensure};

/// True when `v` is `MAJOR.MINOR.PATCH` with all three parts non-empty ASCII
/// digits. A pre-release / build suffix (`-dev.1`, `+abc`) after the patch
/// number is accepted.
pub fn is_semver(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Validate the `version` field of a `/api/status` response and return it.
/// `who` names the instance (server / client) in the error message.
pub fn check_status_version(status: &serde_json::Value, who: &str) -> Result<String> {
    let version = status["version"].as_str().unwrap_or("");
    ensure!(
        is_semver(version),
        "{who} /api/status 'version' is not a MAJOR.MINOR.PATCH version: {:?}",
        status["version"]
    );
    Ok(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_release_version() {
        assert!(is_semver("0.8.38"));
        assert!(is_semver("10.20.300"));
    }

    #[test]
    fn accepts_prerelease_and_build_suffix() {
        assert!(is_semver("0.8.38-dev.1"));
        assert!(is_semver("0.8.38+abc"));
    }

    #[test]
    fn rejects_malformed_versions() {
        assert!(!is_semver(""));
        assert!(!is_semver("0.8"));
        assert!(!is_semver("0.8.38.1"));
        assert!(!is_semver("0..38"));
        assert!(!is_semver("v0.8.38"));
        assert!(!is_semver("0.8.x"));
    }

    #[test]
    fn check_status_version_returns_version() {
        let status = json!({"status": "running", "version": "0.8.38"});
        assert_eq!(check_status_version(&status, "server").unwrap(), "0.8.38");
    }

    #[test]
    fn check_status_version_rejects_missing_field() {
        let status = json!({"status": "running"});
        let err = check_status_version(&status, "client")
            .unwrap_err()
            .to_string();
        assert!(err.contains("client /api/status 'version'"), "{err}");
    }

    #[test]
    fn check_status_version_rejects_empty_and_non_string() {
        assert!(check_status_version(&json!({"version": ""}), "server").is_err());
        assert!(check_status_version(&json!({"version": 838}), "server").is_err());
    }
}
