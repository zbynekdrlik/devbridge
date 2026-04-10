//! Startup validation for client-mode configuration.
//!
//! Called once from `devbridge-service::runtime::run_client` before the
//! receiver starts. Refuses to start the service when target_printer or
//! printer_address would silently drop jobs at runtime.

use anyhow::{Result, bail};

use devbridge_core::config::ClientConfig;

/// Entry point: validate a `ClientConfig` against the live system.
///
/// Calls `crate::printer::list_printers()` for the windows_spooler branch.
/// On Linux CI this returns an empty list, so non-empty target names fail
/// fast — which is the desired behavior for tests.
pub fn validate_client_config(config: &ClientConfig) -> Result<()> {
    let printers = crate::printer::list_printers()
        .map(|list| list.into_iter().map(|p| p.name).collect::<Vec<_>>())
        .unwrap_or_default();
    validate_client_config_against(config, &printers)
}

/// DI variant for unit tests — pass the printer list explicitly instead of
/// shelling out to `Get-Printer` / `lpstat`.
fn validate_client_config_against(config: &ClientConfig, printers: &[String]) -> Result<()> {
    match config.print_backend.as_str() {
        "windows_spooler" | "" => validate_local_printer_against(printers, &config.target_printer),
        "direct_ipp" => validate_ipp_address(config.printer_address.as_deref()),
        // Other backends (cups, direct_raw, print_proxy) are not validated here.
        _ => Ok(()),
    }
}

/// Validate `target` is exactly one of the entries in `available` (case-insensitive).
fn validate_local_printer_against(available: &[String], target: &str) -> Result<()> {
    if available.is_empty() {
        bail!(
            "No printers installed on this machine. \
             Install the printer driver before configuring DevBridge \
             (target_printer = \"{}\")",
            target
        );
    }
    let target_lower = target.to_lowercase();
    if available.iter().any(|p| p.to_lowercase() == target_lower) {
        return Ok(());
    }
    let alternatives = available
        .iter()
        .map(|p| format!("    - {}", p))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "target_printer \"{}\" not found on this machine.\n  \
         Available printers:\n{}\n  \
         Suggestion: edit C:\\ProgramData\\DevBridge\\config.toml \
         and set target_printer to one of the names above, then restart \
         the DevBridge scheduled task.",
        target,
        alternatives
    );
}

/// Validate that direct_ipp has a `printer_address` set.
fn validate_ipp_address(address: Option<&str>) -> Result<()> {
    match address {
        Some(s) if !s.is_empty() => Ok(()),
        _ => bail!(
            "direct_ipp backend requires printer_address in config. \
             Suggestion: set [client] printer_address = \"<host>:631\" \
             in C:\\ProgramData\\DevBridge\\config.toml."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_local_printer_exact_match_passes() {
        validate_local_printer_against(
            &["Canon MG3600 series Printer".to_string()],
            "Canon MG3600 series Printer",
        )
        .expect("exact match should pass");
    }

    #[test]
    fn test_validate_local_printer_case_insensitive_match_passes() {
        validate_local_printer_against(&["Canon MG3600".to_string()], "canon mg3600")
            .expect("case-insensitive match should pass");
    }

    #[test]
    fn test_validate_local_printer_missing_lists_alternatives() {
        let err = validate_local_printer_against(
            &[
                "Canon MG3600 series Printer".to_string(),
                "Microsoft Print to PDF".to_string(),
                "eholla printer".to_string(),
            ],
            "Brother DCP-1610W",
        )
        .expect_err("missing printer should fail");
        let msg = err.to_string();
        assert!(msg.contains("Brother DCP-1610W"), "msg: {}", msg);
        assert!(msg.contains("Canon MG3600 series Printer"), "msg: {}", msg);
        assert!(msg.contains("Microsoft Print to PDF"), "msg: {}", msg);
        assert!(msg.contains("eholla printer"), "msg: {}", msg);
    }

    #[test]
    fn test_validate_local_printer_empty_list_fails_with_hint() {
        let err =
            validate_local_printer_against(&[], "Anything").expect_err("empty list should fail");
        assert!(
            err.to_string().contains("No printers installed"),
            "msg: {}",
            err
        );
    }

    #[test]
    fn test_validate_ipp_address_missing_returns_error() {
        let err = validate_ipp_address(None).expect_err("None should fail");
        assert!(err.to_string().contains("printer_address"));
    }

    #[test]
    fn test_validate_ipp_address_present_passes() {
        validate_ipp_address(Some("10.78.2.9:631")).expect("valid address should pass");
    }

    #[test]
    fn test_validate_ipp_address_empty_string_fails() {
        let err = validate_ipp_address(Some("")).expect_err("empty string should fail");
        assert!(err.to_string().contains("printer_address"));
    }

    fn make_config(backend: &str, target: &str, addr: Option<&str>) -> ClientConfig {
        ClientConfig {
            server_address: "127.0.0.1:50051".into(),
            target_printer: target.into(),
            dashboard_port: 9120,
            reconnect_interval_secs: 5,
            max_reconnect_interval_secs: 60,
            client_id: None,
            print_backend: backend.into(),
            printer_address: addr.map(String::from),
            ghostscript_device: "jpeg".into(),
            ghostscript_resolution: 360,
            printer_tls: false,
            printer_display_name: None,
            print_proxy_url: None,
            virtual_printer_name: None,
            tls: Default::default(),
        }
    }

    #[test]
    fn test_validate_client_config_direct_ipp_with_address_passes() {
        let cfg = make_config("direct_ipp", "ignored", Some("10.78.2.9:631"));
        validate_client_config_against(&cfg, &[]).expect("direct_ipp with address should pass");
    }

    #[test]
    fn test_validate_client_config_direct_ipp_without_address_fails() {
        let cfg = make_config("direct_ipp", "ignored", None);
        validate_client_config_against(&cfg, &[])
            .expect_err("direct_ipp without printer_address should fail");
    }

    #[test]
    fn test_validate_client_config_windows_spooler_uses_printer_list() {
        let cfg = make_config("windows_spooler", "Canon MG3600", None);
        validate_client_config_against(&cfg, &["Canon MG3600".to_string()])
            .expect("matching printer should pass");

        let cfg_bad = make_config("windows_spooler", "NonExistent", None);
        validate_client_config_against(&cfg_bad, &["Canon MG3600".to_string()])
            .expect_err("non-matching printer should fail");
    }

    #[test]
    fn test_validate_client_config_unknown_backend_passes() {
        // Unknown backends (cups, print_proxy, etc.) are not validated here.
        let cfg = make_config("print_proxy", "ignored", None);
        validate_client_config_against(&cfg, &[]).expect("unknown backend should be skipped");
    }

    // ── Tests for the pub wrapper that shells out to list_printers() ──────
    // These exercise the codepath actually called from runtime.rs::run_client,
    // not just the DI variant. They use direct_ipp / print_proxy branches so
    // they are platform-agnostic (don't depend on the local printer list).

    #[test]
    fn test_pub_validate_client_config_direct_ipp_without_address_fails() {
        let cfg = make_config("direct_ipp", "ignored", None);
        let err = validate_client_config(&cfg).expect_err("None address should fail");
        assert!(err.to_string().contains("printer_address"));
    }

    #[test]
    fn test_pub_validate_client_config_direct_ipp_with_address_passes() {
        let cfg = make_config("direct_ipp", "ignored", Some("10.78.2.9:631"));
        validate_client_config(&cfg).expect("direct_ipp with address should pass");
    }

    #[test]
    fn test_pub_validate_client_config_unknown_backend_skips() {
        let cfg = make_config("print_proxy", "ignored", None);
        validate_client_config(&cfg).expect("unknown backend should be skipped");
    }
}
