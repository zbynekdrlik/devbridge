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
    validate_odoo_config(config)?;
    match config.print_backend.as_str() {
        // Both spool to a local Windows printer (the RAW one byte-for-byte, #88).
        "windows_spooler" | "windows_spooler_raw" | "" => {
            validate_local_printer_against(printers, &config.target_printer)
        }
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

/// Validate `[client.odoo]` (#90) when enabled. The API key is only checked
/// for presence — its value never appears in an error.
fn validate_odoo_config(config: &ClientConfig) -> Result<()> {
    let odoo = &config.odoo;
    if !odoo.enabled {
        return Ok(());
    }
    let mut problems = Vec::new();
    let url = odoo.url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.contains(char::is_whitespace)
    {
        problems.push(format!(
            "url \"{}\" must be an http(s):// base URL",
            odoo.url
        ));
    }
    if odoo.api_key.trim().is_empty() {
        problems.push("api_key is empty".to_string());
    }
    if odoo.printer_name.trim().is_empty() {
        problems.push("printer_name (the Odoo food.printer name) is empty".to_string());
    }
    if config.print_backend != crate::backend_windows_spooler_raw::BACKEND_NAME {
        problems.push(format!(
            "print_backend is \"{}\" but Odoo labels are TSPL and need \"{}\"",
            config.print_backend,
            crate::backend_windows_spooler_raw::BACKEND_NAME
        ));
    }
    for (name, mm) in [
        ("label_width_mm", odoo.label_width_mm),
        ("label_height_mm", odoo.label_height_mm),
    ] {
        if !(mm.is_finite() && mm > 0.0 && mm <= 1000.0) {
            problems.push(format!("{name} = {mm} must be > 0 and <= 1000"));
        }
    }
    if !(100..=1200).contains(&odoo.dpi) {
        problems.push(format!("dpi = {} must be between 100 and 1200", odoo.dpi));
    }
    if odoo.poll_interval_secs == 0 || odoo.heartbeat_interval_secs == 0 {
        problems.push("poll_interval_secs and heartbeat_interval_secs must be >= 1".to_string());
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "[client.odoo] is enabled but invalid: {}. Suggestion: re-run the installer with \
         DEVBRIDGE_ODOO_URL / DEVBRIDGE_ODOO_API_KEY / DEVBRIDGE_ODOO_PRINTER_NAME set \
         (and DEVBRIDGE_PRINT_BACKEND=windows_spooler_raw).",
        problems.join("; ")
    )
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
            virtual_printer_driver: None,
            tls: Default::default(),
            serial_bridge: Default::default(),
            odoo: Default::default(),
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
    fn test_validate_client_config_windows_spooler_raw_uses_printer_list() {
        // The RAW label backend (#88) spools to a local Windows printer too —
        // a typo in target_printer must refuse the start, not drop labels.
        let cfg = make_config("windows_spooler_raw", "TSC ML241P", None);
        validate_client_config_against(&cfg, &["TSC ML241P".to_string()])
            .expect("installed label printer should pass");

        let cfg_bad = make_config("windows_spooler_raw", "TSC ML241", None);
        let err = validate_client_config_against(&cfg_bad, &["TSC ML241P".to_string()])
            .expect_err("missing label printer should fail");
        assert!(err.to_string().contains("TSC ML241P"), "{err}");
    }

    fn odoo_config() -> ClientConfig {
        let mut cfg = make_config("windows_spooler_raw", "TSC ML241P", None);
        cfg.odoo = devbridge_core::config::OdooClientConfig {
            enabled: true,
            url: "https://erp.example.test".into(),
            api_key: "top-secret-key".into(),
            printer_name: "TSC ML241P Spišská".into(),
            ..Default::default()
        };
        cfg
    }

    #[test]
    fn test_validate_odoo_disabled_is_not_checked() {
        let mut cfg = make_config("windows_spooler", "P", None);
        cfg.odoo.url = "garbage".into();
        validate_odoo_config(&cfg).expect("disabled [client.odoo] is ignored");
    }

    #[test]
    fn test_validate_odoo_valid_config_passes() {
        validate_odoo_config(&odoo_config()).expect("valid odoo config");
        let mut http = odoo_config();
        http.odoo.url = "http://10.88.1.100:9230".into();
        validate_odoo_config(&http).expect("plain http (E2E fake Odoo) is allowed");
        validate_client_config_against(&odoo_config(), &["TSC ML241P".to_string()])
            .expect("full client validation passes");
    }

    #[test]
    fn test_validate_odoo_each_problem_is_reported_and_key_never_leaks() {
        let mut cfg = odoo_config();
        cfg.print_backend = "windows_spooler".into();
        cfg.odoo.url = "erp.example.test".into();
        cfg.odoo.printer_name = " ".into();
        cfg.odoo.label_width_mm = 0.0;
        cfg.odoo.label_height_mm = f64::NAN;
        cfg.odoo.dpi = 50;
        cfg.odoo.poll_interval_secs = 0;
        let msg = validate_odoo_config(&cfg).unwrap_err().to_string();
        for needle in [
            "http(s)://",
            "printer_name",
            "need \"windows_spooler_raw\"",
            "label_width_mm = 0",
            "label_height_mm = NaN",
            "dpi = 50",
            "poll_interval_secs",
        ] {
            assert!(msg.contains(needle), "missing {needle}: {msg}");
        }
        assert!(!msg.contains("top-secret-key"), "{msg}");
        assert!(!msg.contains("api_key is empty"), "{msg}");

        let mut no_key = odoo_config();
        no_key.odoo.api_key = "  ".into();
        let msg = validate_odoo_config(&no_key).unwrap_err().to_string();
        assert!(msg.contains("api_key is empty"), "{msg}");

        let mut hb = odoo_config();
        hb.odoo.heartbeat_interval_secs = 0;
        assert!(validate_odoo_config(&hb).is_err());
        let mut big = odoo_config();
        big.odoo.label_height_mm = 1000.5;
        assert!(validate_odoo_config(&big).is_err());
        let mut edge = odoo_config();
        edge.odoo.label_height_mm = 1000.0;
        edge.odoo.dpi = 1200;
        validate_odoo_config(&edge).expect("1000 mm / 1200 dpi are the upper bounds");
        edge.odoo.dpi = 100;
        validate_odoo_config(&edge).expect("100 dpi is the lower bound");
        edge.odoo.dpi = 1201;
        assert!(validate_odoo_config(&edge).is_err());
        let mut spaced = odoo_config();
        spaced.odoo.url = "https://erp.example.test/a b".into();
        assert!(validate_odoo_config(&spaced).is_err());
    }

    #[test]
    fn test_validate_client_config_runs_odoo_validation_first() {
        let mut cfg = odoo_config();
        cfg.odoo.api_key = String::new();
        let err = validate_client_config_against(&cfg, &["TSC ML241P".to_string()]).unwrap_err();
        assert!(err.to_string().contains("[client.odoo]"), "{err}");
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
