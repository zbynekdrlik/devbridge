//! Startup validation for client-mode configuration.
//!
//! Called once from `devbridge-service::runtime::run_client` before the
//! receiver starts. Refuses to start the service when target_printer or
//! printer_address would silently drop jobs at runtime.

use anyhow::{Result, bail};

use devbridge_core::config::{ClientConfig, POKLADNA_MAX_WAIT_S, valid_dns_name};

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
    validate_pokladna_config(config)?;
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
    problems.extend(base_url_problem(&odoo.url));
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

/// `url "…" must be an http(s):// base URL` when `url` is not one.
fn base_url_problem(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let ok = (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains(char::is_whitespace);
    (!ok).then(|| format!("url \"{url}\" must be an http(s):// base URL"))
}

/// Validate `[client.pokladna]` (#99) when enabled. Independent of the print
/// backend (device jobs never touch the printer). The API key is only checked
/// for presence — its value never appears in an error.
fn validate_pokladna_config(config: &ClientConfig) -> Result<()> {
    let p = &config.pokladna;
    if !p.enabled {
        return Ok(());
    }
    let mut problems = Vec::new();
    problems.extend(base_url_problem(&p.url));
    if p.api_key.trim().is_empty() {
        problems.push("api_key is empty".to_string());
    }
    if p.wait_s > POKLADNA_MAX_WAIT_S {
        problems.push(format!(
            "wait_s = {} must be between 0 and {POKLADNA_MAX_WAIT_S}",
            p.wait_s
        ));
    }
    if p.heartbeat_interval_secs == 0 {
        problems.push("heartbeat_interval_secs must be >= 1".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for kasa in &p.kasy {
        if kasa.id <= 0 {
            problems.push(format!(
                "kasy: id = {} must be a positive pos.config id",
                kasa.id
            ));
        } else if !seen.insert(kasa.id) {
            problems.push(format!("kasy: id = {} is listed twice", kasa.id));
        }
        // the till's Portos eKasa API (#100); absent = http://localhost:3010
        if let Some(problem) = kasa.portos_url.as_deref().and_then(base_url_problem) {
            problems.push(format!("kasy: id = {}: portos_{problem}", kasa.id));
        }
    }
    problems.extend(lan_api_problems(config));
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "[client.pokladna] is enabled but invalid: {}. Suggestion: re-run the installer with \
         DEVBRIDGE_POKLADNA_URL / DEVBRIDGE_POKLADNA_API_KEY set (DEVBRIDGE_POKLADNA_WAIT_S \
         0..{POKLADNA_MAX_WAIT_S}; LAN API: DEVBRIDGE_POKLADNA_KASY with each till's lan_key, \
         DEVBRIDGE_LAN_API_ENABLED / _PORT / _HOSTNAME).",
        problems.join("; ")
    )
}

/// A till's LAN API key (#101): 32–256 printable ASCII characters, no
/// space, quote or backslash (it travels in a header and a TOML string).
pub fn lan_key_ok(key: &str) -> bool {
    (32..=256).contains(&key.len())
        && key
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')
}

/// Problems of an ENABLED `[client.pokladna.lan_api]` (#101). Key values
/// never appear in a problem.
fn lan_api_problems(config: &ClientConfig) -> Vec<String> {
    let p = &config.pokladna;
    let lan = &p.lan_api;
    if !lan.enabled {
        return Vec::new();
    }
    let mut problems = Vec::new();
    if lan.port == 0 || lan.port == config.dashboard_port {
        problems.push(format!(
            "lan_api.port = {} must be a free port other than the dashboard's ({})",
            lan.port, config.dashboard_port
        ));
    }
    if !valid_dns_name(&lan.hostname) {
        problems.push(format!(
            "lan_api.hostname \"{}\" must be the PC's lower-case DNS name (<client_id>.<zone>)",
            lan.hostname
        ));
    }
    if lan.acme {
        let own = config
            .client_id
            .as_deref()
            .map(|id| format!("{}.", id.to_ascii_lowercase()));
        if !own.is_some_and(|prefix| lan.hostname.starts_with(&prefix)) {
            problems.push(
                "lan_api.hostname must be <client_id>.<zone> with [client] client_id set - \
                 the only name pz-server issues a certificate for"
                    .to_string(),
            );
        }
    }
    let keys: Vec<(i64, &str)> = p
        .kasy
        .iter()
        .filter_map(|k| k.lan_key.as_deref().map(|key| (k.id, key)))
        .collect();
    if keys.is_empty() {
        problems.push(
            "no [[client.pokladna.kasy]] has a lan_key - no till could call the LAN API"
                .to_string(),
        );
    }
    for (index, (id, key)) in keys.iter().enumerate() {
        if !lan_key_ok(key) {
            problems.push(format!(
                "kasy: id = {id}: lan_key must be 32-256 printable characters without spaces, quotes or backslashes"
            ));
        }
        if *key == p.api_key {
            problems.push(format!(
                "kasy: id = {id}: lan_key must not be the Odoo bridge key"
            ));
        }
        if keys[..index].iter().any(|(_, other)| other == key) {
            problems.push(format!(
                "kasy: id = {id}: lan_key is the same as another till's (a key belongs to one till)"
            ));
        }
    }
    problems
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
            pokladna: Default::default(),
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

    fn pokladna_config() -> ClientConfig {
        let mut cfg = make_config("direct_ipp", "ignored", Some("10.78.2.9:631"));
        cfg.pokladna = devbridge_core::config::PokladnaClientConfig {
            enabled: true,
            url: "https://erp.example.test".into(),
            api_key: "pokladna-secret-key".into(),
            ..Default::default()
        };
        cfg
    }

    #[test]
    fn test_validate_pokladna_disabled_is_not_checked() {
        let mut cfg = make_config("windows_spooler", "P", None);
        cfg.pokladna.url = "garbage".into();
        cfg.pokladna.wait_s = 999;
        validate_pokladna_config(&cfg).expect("disabled [client.pokladna] is ignored");
    }

    #[test]
    fn test_validate_pokladna_valid_configs_pass_with_any_backend() {
        validate_pokladna_config(&pokladna_config()).expect("valid pokladna config");
        let mut edge = pokladna_config();
        edge.pokladna.url = "http://10.88.1.100:9231".into();
        edge.pokladna.wait_s = 55;
        edge.pokladna.heartbeat_interval_secs = 1;
        edge.pokladna.kasy = vec![
            devbridge_core::config::PokladnaKasaConfig {
                id: 3,
                portos_url: Some("http://10.78.2.10:3010".into()),
                terminal_ip: None,
                terminal_tid: None,
                lan_key: None,
            },
            devbridge_core::config::PokladnaKasaConfig {
                id: 4,
                portos_url: None,
                terminal_ip: None,
                terminal_tid: None,
                lan_key: None,
            },
        ];
        validate_pokladna_config(&edge).expect("55 s and http are allowed");
        edge.pokladna.wait_s = 0;
        validate_pokladna_config(&edge).expect("wait_s 0 = pure short polling");
        validate_client_config_against(&pokladna_config(), &[])
            .expect("full client validation passes (direct_ipp backend)");
    }

    #[test]
    fn test_validate_pokladna_each_problem_is_reported_and_key_never_leaks() {
        let kasa = |id| devbridge_core::config::PokladnaKasaConfig {
            id,
            portos_url: None,
            terminal_ip: None,
            terminal_tid: None,
            lan_key: None,
        };
        let mut cfg = pokladna_config();
        cfg.pokladna.url = "erp.example.test".into();
        cfg.pokladna.wait_s = 56;
        cfg.pokladna.heartbeat_interval_secs = 0;
        let mut bad_portos = kasa(6);
        bad_portos.portos_url = Some("localhost:3010".into());
        cfg.pokladna.kasy = vec![kasa(0), kasa(5), kasa(5), kasa(-2), bad_portos];
        let msg = validate_pokladna_config(&cfg).unwrap_err().to_string();
        for needle in [
            "kasy: id = 6: portos_url \"localhost:3010\" must be an http(s):// base URL",
            "[client.pokladna] is enabled but invalid",
            "url \"erp.example.test\" must be an http(s):// base URL",
            "wait_s = 56 must be between 0 and 55",
            "heartbeat_interval_secs must be >= 1",
            "kasy: id = 0 must be a positive pos.config id",
            "kasy: id = 5 is listed twice",
            "kasy: id = -2 must be a positive pos.config id",
            "DEVBRIDGE_POKLADNA_URL",
        ] {
            assert!(msg.contains(needle), "missing {needle}: {msg}");
        }
        assert!(!msg.contains("pokladna-secret-key"), "{msg}");
        assert!(!msg.contains("api_key is empty"), "{msg}");

        let mut no_key = pokladna_config();
        no_key.pokladna.api_key = " ".into();
        let msg = validate_pokladna_config(&no_key).unwrap_err().to_string();
        assert!(msg.contains("api_key is empty"), "{msg}");
        let mut spaced = pokladna_config();
        spaced.pokladna.url = "https://erp.example.test/a b".into();
        assert!(validate_pokladna_config(&spaced).is_err());
    }

    #[test]
    fn test_validate_client_config_refuses_an_invalid_pokladna_block() {
        let mut cfg = pokladna_config();
        cfg.pokladna.api_key = String::new();
        let err = validate_client_config_against(&cfg, &[]).unwrap_err();
        assert!(err.to_string().contains("[client.pokladna]"), "{err}");
    }

    #[test]
    fn test_base_url_problem() {
        assert_eq!(base_url_problem("https://x.sk"), None);
        assert_eq!(base_url_problem(" http://x.sk "), None);
        assert_eq!(
            base_url_problem("ftp://x.sk").as_deref(),
            Some("url \"ftp://x.sk\" must be an http(s):// base URL")
        );
        assert!(base_url_problem("https://x.sk/a b").is_some());
        assert!(base_url_problem("").is_some());
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

    const LAN_KEY_3: &str = "lan-key-of-till-3-0123456789abcdefgh";
    /// A bridge key long enough to pass as a LAN key (test fixture).
    const BRIDGE_FIXTURE: &str = "bridge-fixture-not-a-secret-0123456789";
    const LAN_KEY_4: &str = "lan-key-of-till-4-0123456789abcdefgh";

    fn lan_kasa(id: i64, key: Option<&str>) -> devbridge_core::config::PokladnaKasaConfig {
        devbridge_core::config::PokladnaKasaConfig {
            id,
            portos_url: None,
            terminal_ip: None,
            terminal_tid: None,
            lan_key: key.map(str::to_string),
        }
    }

    /// A valid LAN API config (#101): pjsnvs, two tills, ACME name.
    fn lan_config() -> ClientConfig {
        let mut cfg = pokladna_config();
        cfg.client_id = Some("PJSNVS".into());
        cfg.pokladna.kasy = vec![lan_kasa(3, Some(LAN_KEY_3)), lan_kasa(4, Some(LAN_KEY_4))];
        cfg.pokladna.lan_api = devbridge_core::config::LanApiConfig {
            enabled: true,
            port: 9443,
            hostname: "pjsnvs.pos.example.sk".into(),
            acme: true,
        };
        cfg
    }

    #[test]
    fn test_lan_key_ok() {
        assert!(lan_key_ok(&"a".repeat(32)));
        assert!(lan_key_ok(&"a".repeat(256)));
        assert!(!lan_key_ok(&"a".repeat(31)));
        assert!(!lan_key_ok(&"a".repeat(257)));
        let base = "a".repeat(31);
        for ok in ['!', '~', 'Z', '0', '-', '_'] {
            assert!(lan_key_ok(&format!("{base}{ok}")), "{ok}");
        }
        for bad in [' ', '"', '\\', '\u{7f}', '\u{1f}', 'š'] {
            assert!(!lan_key_ok(&format!("{base}{bad}")), "{bad:?}");
        }
    }

    #[test]
    fn test_validate_lan_api_valid_and_disabled() {
        validate_pokladna_config(&lan_config()).expect("valid LAN API config");
        let mut files = lan_config();
        files.pokladna.lan_api.acme = false;
        files.client_id = None;
        files.pokladna.lan_api.hostname = "e2e-raw-client.lan-api.test".into();
        validate_pokladna_config(&files).expect("a file certificate needs no client_id");
        // disabled: not checked at all
        let mut off = lan_config();
        off.pokladna.lan_api.enabled = false;
        off.pokladna.lan_api.port = 0;
        off.pokladna.kasy = vec![lan_kasa(3, Some("short"))];
        validate_pokladna_config(&off).expect("a disabled LAN API is not checked");
    }

    #[test]
    fn test_validate_lan_api_each_problem_and_keys_never_leak() {
        let mut cfg = lan_config();
        cfg.pokladna.lan_api.port = 9120;
        cfg.pokladna.lan_api.hostname = "pjzav.pos.example.sk".into();
        cfg.pokladna.kasy = vec![
            lan_kasa(3, Some("short-key")),
            lan_kasa(4, Some(BRIDGE_FIXTURE)),
            lan_kasa(5, Some(LAN_KEY_3)),
            lan_kasa(6, Some(LAN_KEY_3)),
            lan_kasa(7, None),
        ];
        cfg.pokladna.api_key = BRIDGE_FIXTURE.to_string();
        let msg = validate_pokladna_config(&cfg).unwrap_err().to_string();
        for needle in [
            "lan_api.port = 9120 must be a free port other than the dashboard's (9120)",
            "lan_api.hostname must be <client_id>.<zone> with [client] client_id set",
            "kasy: id = 3: lan_key must be 32-256 printable characters",
            "kasy: id = 4: lan_key must not be the Odoo bridge key",
            "kasy: id = 6: lan_key is the same as another till's",
            "DEVBRIDGE_POKLADNA_KASY",
        ] {
            assert!(msg.contains(needle), "missing {needle}: {msg}");
        }
        assert!(
            !msg.contains("kasy: id = 5: lan_key is the same"),
            "the first one is fine: {msg}"
        );
        assert!(!msg.contains("kasy: id = 7"), "{msg}");
        for secret in ["short-key", BRIDGE_FIXTURE, LAN_KEY_3] {
            assert!(!msg.contains(secret), "{secret} leaked: {msg}");
        }

        let mut bad_name = lan_config();
        bad_name.pokladna.lan_api.hostname = "PJSNVS".into();
        bad_name.pokladna.lan_api.port = 0;
        let msg = validate_pokladna_config(&bad_name).unwrap_err().to_string();
        assert!(
            msg.contains("lan_api.port = 0 must be a free port"),
            "{msg}"
        );
        assert!(
            msg.contains("lan_api.hostname \"PJSNVS\" must be the PC's lower-case DNS name"),
            "{msg}"
        );

        let mut no_keys = lan_config();
        no_keys.pokladna.kasy = vec![lan_kasa(3, None)];
        let msg = validate_pokladna_config(&no_keys).unwrap_err().to_string();
        assert!(
            msg.contains("no [[client.pokladna.kasy]] has a lan_key"),
            "{msg}"
        );

        let mut no_client_id = lan_config();
        no_client_id.client_id = None;
        let msg = validate_pokladna_config(&no_client_id)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("with [client] client_id set"), "{msg}");
    }
}
