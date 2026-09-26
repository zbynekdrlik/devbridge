use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Windows driver used for a virtual printer's server-side Windows printer
/// when no override is set. Every printer created before #88 uses it.
pub const DEFAULT_WINDOWS_DRIVER: &str = "Microsoft IPP Class Driver";

/// Longest accepted Windows printer-driver name. Windows' own limit for a
/// driver name is well below this; anything longer is a typo or garbage.
pub const MAX_DRIVER_NAME_LEN: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualPrinter {
    pub id: String,
    pub display_name: String,
    pub ipp_name: String,
    pub paired_client_id: Option<String>,
    /// Optional Windows driver override for the server-side Windows printer
    /// (issue #88 — RAW passthrough for label printers: the vendor driver,
    /// e.g. `TSC ML241P`, renders the job and its bytes travel to the client
    /// unchanged). `None` = [`DEFAULT_WINDOWS_DRIVER`]. The reconciler only
    /// USES an already-installed driver, it never installs one.
    #[serde(default)]
    pub driver: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl VirtualPrinter {
    /// The Windows driver the reconciler registers this printer with.
    pub fn effective_driver(&self) -> &str {
        self.driver.as_deref().unwrap_or(DEFAULT_WINDOWS_DRIVER)
    }
}

/// Validate and normalise an optional driver-name override.
///
/// Returns `Ok(None)` for "no override" (absent, empty or whitespace-only —
/// the printer keeps [`DEFAULT_WINDOWS_DRIVER`]), `Ok(Some(trimmed))` for a
/// usable name. Rejects names that could break out of the quoted
/// `printui.dll /m "<driver>"` argument the reconciler builds, or out of a
/// TOML string: double quotes, backslashes, control characters, and names
/// longer than [`MAX_DRIVER_NAME_LEN`].
pub fn normalize_driver_name(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_DRIVER_NAME_LEN {
        return Err(format!(
            "driver name is longer than {MAX_DRIVER_NAME_LEN} characters"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| *c == '"' || *c == '\\' || c.is_control())
    {
        return Err(format!(
            "driver name contains a forbidden character {bad:?} (no quotes, backslashes or control characters)"
        ));
    }
    Ok(Some(name.to_string()))
}

/// Convert a display name to a URL-safe slug for IPP routing.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_printer_serde_roundtrip() {
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-001".into(),
            display_name: "Store A - Receipt Printer".into(),
            ipp_name: "store-a-receipt".into(),
            paired_client_id: Some("client-abc".into()),
            driver: Some("TSC ML241P".into()),
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&vp).unwrap();
        let restored: VirtualPrinter = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.id, "vp-001");
        assert_eq!(restored.display_name, "Store A - Receipt Printer");
        assert_eq!(restored.ipp_name, "store-a-receipt");
        assert_eq!(restored.paired_client_id, Some("client-abc".into()));
        assert_eq!(restored.driver.as_deref(), Some("TSC ML241P"));
        assert_eq!(restored.effective_driver(), "TSC ML241P");
    }

    #[test]
    fn test_virtual_printer_no_pairing() {
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-002".into(),
            display_name: "Unpaired".into(),
            ipp_name: "unpaired".into(),
            paired_client_id: None,
            driver: None,
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&vp).unwrap();
        let restored: VirtualPrinter = serde_json::from_str(&json).unwrap();

        assert!(restored.paired_client_id.is_none());
        assert!(restored.driver.is_none());
        assert_eq!(restored.effective_driver(), DEFAULT_WINDOWS_DRIVER);
    }

    #[test]
    fn test_virtual_printer_json_without_driver_field_defaults_to_none() {
        // JSON written before #88 (DB rows, reconcile-input.json) has no
        // `driver` key — it must still deserialize, with the default driver.
        let json = r#"{"id":"vp","display_name":"pjsnvs printer","ipp_name":"pjsnvs-printer",
            "paired_client_id":"pjsnvs","created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"}"#;
        let vp: VirtualPrinter = serde_json::from_str(json).unwrap();
        assert!(vp.driver.is_none());
        assert_eq!(vp.effective_driver(), "Microsoft IPP Class Driver");
    }

    #[test]
    fn test_normalize_driver_name_absent_or_blank_is_no_override() {
        assert_eq!(normalize_driver_name(None), Ok(None));
        assert_eq!(normalize_driver_name(Some("")), Ok(None));
        assert_eq!(normalize_driver_name(Some("   ")), Ok(None));
    }

    #[test]
    fn test_normalize_driver_name_trims_valid_name() {
        assert_eq!(
            normalize_driver_name(Some("  TSC ML241P ")),
            Ok(Some("TSC ML241P".to_string()))
        );
        assert_eq!(
            normalize_driver_name(Some("Generic / Text Only")),
            Ok(Some("Generic / Text Only".to_string()))
        );
    }

    #[test]
    fn test_normalize_driver_name_rejects_quote_backslash_control() {
        for bad in ["TSC\" /m \"x", "a\\b", "line\nbreak", "tab\there"] {
            let err = normalize_driver_name(Some(bad)).expect_err(bad);
            assert!(err.contains("forbidden character"), "{bad}: {err}");
        }
    }

    #[test]
    fn test_normalize_driver_name_length_boundary() {
        let max = "a".repeat(MAX_DRIVER_NAME_LEN);
        assert_eq!(normalize_driver_name(Some(&max)), Ok(Some(max.clone())));
        let over = "a".repeat(MAX_DRIVER_NAME_LEN + 1);
        let err = normalize_driver_name(Some(&over)).unwrap_err();
        assert!(err.contains("longer than"), "{err}");
    }

    #[test]
    fn test_slugify_strips_consecutive_separators() {
        // Input with consecutive non-alphanumeric chars produces empty segments.
        // The `!s.is_empty()` filter removes them — without `!`, they'd remain as `--`.
        assert_eq!(slugify("Hello  World"), "hello-world");
        assert_eq!(slugify("a--b"), "a-b");
        assert_eq!(slugify("  leading"), "leading");
        assert_eq!(slugify("trailing  "), "trailing");
    }
}
