//! E2E coverage for `DEVBRIDGE_SERIAL_PORT` (issue #70).
//!
//! `deploy/e2e-setup-client-local.ps1` runs the REAL installer function
//! `Merge-DevBridgeSerialBridgeIntoConfig` (from `installer/DevBridgeInstallerLib.ps1`)
//! on the ISOLATED E2E client config with a port that does not exist on the
//! runner, then starts the E2E client. This step proves, against the deployed
//! client, that the installer-written `[client.serial_bridge]` block parses in
//! Rust and reaches `/api/status` with the exact values, and that the client
//! keeps running while its serial reader fails to open the missing port
//! (warn + back off, never crash). The production client config is never
//! touched.

use anyhow::{Context, Result, ensure};
use std::time::Duration;

/// Port the E2E setup passes to the installer merge (`-SerialPort`). Must match
/// the `$SerialPort` default in `deploy/e2e-setup-client-local.ps1`.
pub const E2E_SERIAL_PORT: &str = "COM250";

/// Baud rate the E2E setup passes to the installer merge (`-SerialBaudRate`).
/// Must match the `$SerialBaudRate` default in `deploy/e2e-setup-client-local.ps1`.
pub const E2E_SERIAL_BAUD: u64 = 9600;

/// Cap of the client serial reader's retry backoff in seconds (1s, 2s, 4s, ...
/// capped at 30s — `max_backoff` in
/// `devbridge-client/src/serial_bridge.rs::spawn_reader`). By this step the
/// client has been up for minutes, so the reader is already retrying at the cap.
const MAX_READER_BACKOFF_SECS: u64 = 30;

/// How long to wait before re-reading the status to prove the client survived
/// its serial reader failing on the missing port. Derived as the backoff cap
/// plus a margin, so at least one fresh open attempt on the missing port is
/// guaranteed to happen inside the window (a shorter window could contain zero
/// attempts and prove nothing).
const SURVIVAL_WINDOW: Duration = Duration::from_secs(MAX_READER_BACKOFF_SECS + 5);

/// Validate the client `/api/status` `serial_bridge` object against the values
/// the E2E setup wrote: `{"enabled": true, "port": <port>, "baud_rate": <baud>}`.
pub fn check_serial_bridge_status(status: &serde_json::Value, port: &str, baud: u64) -> Result<()> {
    let sb = status
        .get("serial_bridge")
        .context("client /api/status has no 'serial_bridge' key (issue #70 not deployed?)")?;
    ensure!(
        !sb.is_null(),
        "client /api/status serial_bridge is null — the installer-merged [client.serial_bridge] block did not load"
    );
    ensure!(
        sb.get("enabled").and_then(|v| v.as_bool()) == Some(true),
        "serial_bridge.enabled must be true, got {sb}"
    );
    ensure!(
        sb.get("port").and_then(|v| v.as_str()) == Some(port),
        "serial_bridge.port must be {port:?}, got {sb}"
    );
    ensure!(
        sb.get("baud_rate").and_then(|v| v.as_u64()) == Some(baud),
        "serial_bridge.baud_rate must be {baud}, got {sb}"
    );
    Ok(())
}

/// Validate that the second status read shows the SAME running process: status
/// `running` and an uptime that did not go backwards (a crash + relaunch would
/// reset it).
pub fn check_client_survived(before: &serde_json::Value, after: &serde_json::Value) -> Result<()> {
    let state = after.get("status").and_then(|s| s.as_str());
    ensure!(
        state == Some("running"),
        "client status must stay 'running' with a failing serial reader, got {state:?}"
    );
    let up_before = before
        .get("uptime_secs")
        .and_then(|u| u.as_u64())
        .context("first status read has no uptime_secs")?;
    let up_after = after
        .get("uptime_secs")
        .and_then(|u| u.as_u64())
        .context("second status read has no uptime_secs")?;
    ensure!(
        up_after >= up_before,
        "client uptime went backwards ({up_before}s -> {up_after}s): the service restarted with the serial bridge enabled"
    );
    Ok(())
}

async fn fetch_status(client: &reqwest::Client, client_base: &str) -> Result<serde_json::Value> {
    let resp = client
        .get(format!("{client_base}/api/status"))
        .send()
        .await
        .context("Failed to fetch client status")?
        .error_for_status()
        .context("client /api/status returned an error status")?;
    resp.json().await.context("client /api/status is not JSON")
}

/// E2E step: the deployed E2E client reports the installer-merged serial
/// bridge on `/api/status` and stays running with it.
pub async fn test_client_serial_bridge(client: &reqwest::Client, client_base: &str) -> Result<()> {
    let before = fetch_status(client, client_base).await?;
    check_serial_bridge_status(&before, E2E_SERIAL_PORT, E2E_SERIAL_BAUD)?;

    tokio::time::sleep(SURVIVAL_WINDOW).await;

    let after = fetch_status(client, client_base).await?;
    check_client_survived(&before, &after)?;
    check_serial_bridge_status(&after, E2E_SERIAL_PORT, E2E_SERIAL_BAUD)?;

    println!(
        "PASS (serial_bridge port={E2E_SERIAL_PORT} baud={E2E_SERIAL_BAUD} enabled=true, status running, uptime {}s -> {}s)",
        before["uptime_secs"], after["uptime_secs"],
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status_with(sb: serde_json::Value) -> serde_json::Value {
        json!({ "mode": "client", "status": "running", "uptime_secs": 10, "serial_bridge": sb })
    }

    #[test]
    fn accepts_exact_expected_bridge() {
        let s = status_with(json!({ "enabled": true, "port": "COM250", "baud_rate": 9600 }));
        check_serial_bridge_status(&s, "COM250", 9600).unwrap();
    }

    #[test]
    fn rejects_missing_key() {
        let s = json!({ "mode": "client", "status": "running" });
        let err = check_serial_bridge_status(&s, "COM250", 9600).unwrap_err();
        assert!(err.to_string().contains("no 'serial_bridge' key"), "{err}");
    }

    #[test]
    fn rejects_null_bridge() {
        let s = status_with(serde_json::Value::Null);
        let err = check_serial_bridge_status(&s, "COM250", 9600).unwrap_err();
        assert!(err.to_string().contains("is null"), "{err}");
    }

    #[test]
    fn rejects_disabled_bridge() {
        let s = status_with(json!({ "enabled": false, "port": "COM250", "baud_rate": 9600 }));
        let err = check_serial_bridge_status(&s, "COM250", 9600).unwrap_err();
        assert!(err.to_string().contains("enabled must be true"), "{err}");
    }

    #[test]
    fn rejects_wrong_port() {
        let s = status_with(json!({ "enabled": true, "port": "COM5", "baud_rate": 9600 }));
        let err = check_serial_bridge_status(&s, "COM250", 9600).unwrap_err();
        assert!(err.to_string().contains("port must be"), "{err}");
    }

    #[test]
    fn rejects_wrong_baud() {
        let s = status_with(json!({ "enabled": true, "port": "COM250", "baud_rate": 19200 }));
        let err = check_serial_bridge_status(&s, "COM250", 9600).unwrap_err();
        assert!(err.to_string().contains("baud_rate must be"), "{err}");
    }

    #[test]
    fn survived_when_running_and_uptime_grew() {
        let before = json!({ "status": "running", "uptime_secs": 10 });
        let after = json!({ "status": "running", "uptime_secs": 15 });
        check_client_survived(&before, &after).unwrap();
    }

    #[test]
    fn survived_when_uptime_unchanged() {
        // Same second is not a restart (uptime is whole seconds).
        let before = json!({ "status": "running", "uptime_secs": 10 });
        check_client_survived(&before, &before).unwrap();
    }

    #[test]
    fn not_survived_when_uptime_reset() {
        let before = json!({ "status": "running", "uptime_secs": 100 });
        let after = json!({ "status": "running", "uptime_secs": 2 });
        let err = check_client_survived(&before, &after).unwrap_err();
        assert!(err.to_string().contains("went backwards"), "{err}");
    }

    #[test]
    fn not_survived_when_not_running() {
        let before = json!({ "status": "running", "uptime_secs": 10 });
        let after = json!({ "status": "stopped", "uptime_secs": 15 });
        let err = check_client_survived(&before, &after).unwrap_err();
        assert!(err.to_string().contains("stay 'running'"), "{err}");
    }

    #[test]
    fn not_survived_without_uptime() {
        let before = json!({ "status": "running" });
        let after = json!({ "status": "running", "uptime_secs": 15 });
        let err = check_client_survived(&before, &after).unwrap_err();
        assert!(err.to_string().contains("no uptime_secs"), "{err}");
    }
}
