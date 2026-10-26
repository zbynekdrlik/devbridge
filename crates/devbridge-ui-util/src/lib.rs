//! Pure UI helpers shared between `devbridge-ui` (WASM) and the native
//! workspace (for unit tests).
//!
//! Must not depend on any non-WASM-compatible crates.

/// Decide whether a stored `document_name` should be shown in the UI.
///
/// Returns `Some(display_string)` when the name is a real IPP document
/// name worth showing to the user. Returns `None` when:
/// - the name is empty or whitespace-only (no real name was captured)
/// - the name starts with `job-` (legacy rows from before issue #30 that
///   have `job-<uuid>` instead of a real name)
///
/// Long names are truncated at 79 characters plus a trailing `…` so the
/// total display length is at most 80 characters.
pub fn display_document_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("job-") {
        return None;
    }
    if trimmed.chars().count() > 80 {
        // char_indices-aware truncation: take 79 chars by count, then append
        // the ellipsis. Prevents splitting a multi-byte UTF-8 sequence.
        let truncated: String = trimmed.chars().take(79).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(trimmed.to_string())
    }
}

/// Build the sidebar version label (issue #82) from the `version` string the
/// dashboard API returns (`/api/config`, `/api/status`).
///
/// Returns `Some("v<version>")` for a non-empty version (surrounding whitespace
/// trimmed) and `None` when the version is empty or whitespace-only, so the UI
/// renders nothing rather than a bare `v`.
pub fn version_label(version: &str) -> Option<String> {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("v{trimmed}"))
}

/// Status of a job the printer accepted but never confirmed (#114).
pub const UNCONFIRMED: &str = "unconfirmed";

/// The label a job status shows (#114): `unconfirmed` reads „možno
/// vytlačené" (owner ruling: the printer accepted the job, its end is
/// unknown, it is never resent); every other status as the API sends it.
pub fn job_status_label(status: &str) -> &str {
    if status == UNCONFIRMED {
        "možno vytlačené"
    } else {
        status
    }
}

/// Whether a job offers the manual Reprint: a finished job — completed,
/// failed, or unconfirmed (#114: the store reprints it only when a page is
/// really missing).
pub fn job_can_reprint(status: &str) -> bool {
    matches!(status, "completed" | "failed" | UNCONFIRMED)
}

/// The icon of one job event: ⚠️ for `unconfirmed` (#114, neither printed
/// nor failed), else ✅ / ❌ by its success.
pub fn event_icon(stage: &str, success: bool) -> &'static str {
    if stage == UNCONFIRMED {
        "\u{26A0}\u{FE0F}"
    } else if success {
        "\u{2705}"
    } else {
        "\u{274C}"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #114: a job the printer accepted but never confirmed reads „možno
    /// vytlačené" (owner ruling); every other status as the API sends it.
    #[test]
    fn test_job_status_label() {
        assert_eq!(job_status_label("unconfirmed"), "možno vytlačené");
        for status in ["completed", "failed", "queued", "printing", "online", ""] {
            assert_eq!(job_status_label(status), status);
        }
    }

    /// #114: an unconfirmed job keeps the manual Reprint (the store reprints
    /// only when a page is really missing); a job still on its way has none.
    #[test]
    fn test_job_can_reprint() {
        for status in ["completed", "failed", "unconfirmed"] {
            assert!(job_can_reprint(status), "{status}");
        }
        for status in [
            "queued",
            "downloading",
            "printing",
            "cancelled",
            "",
            "Completed",
        ] {
            assert!(!job_can_reprint(status), "{status}");
        }
    }

    /// #114: an `unconfirmed` event is a warning, neither ✅ nor ❌.
    #[test]
    fn test_event_icon() {
        assert_eq!(event_icon("unconfirmed", false), "\u{26A0}\u{FE0F}");
        assert_eq!(event_icon("unconfirmed", true), "\u{26A0}\u{FE0F}");
        assert_eq!(event_icon("completed", true), "\u{2705}");
        assert_eq!(event_icon("acknowledged", true), "\u{2705}");
        assert_eq!(event_icon("failed", false), "\u{274C}");
        assert_eq!(event_icon("sending", false), "\u{274C}");
    }

    #[test]
    fn test_version_label_prefixes_v() {
        assert_eq!(version_label("0.8.38"), Some("v0.8.38".to_string()));
    }

    #[test]
    fn test_version_label_trims_whitespace() {
        assert_eq!(version_label("  0.8.38\n"), Some("v0.8.38".to_string()));
    }

    #[test]
    fn test_version_label_hides_empty() {
        assert_eq!(version_label(""), None);
    }

    #[test]
    fn test_version_label_hides_whitespace_only() {
        assert_eq!(version_label("   "), None);
    }

    #[test]
    fn test_hides_empty() {
        assert_eq!(display_document_name(""), None);
    }

    #[test]
    fn test_hides_whitespace_only() {
        assert_eq!(display_document_name("   "), None);
    }

    #[test]
    fn test_hides_legacy_uuid_name() {
        assert_eq!(display_document_name("job-abc-123"), None);
        assert_eq!(
            display_document_name("job-7d70b087-9021-45fd-bf90-676fd4ce83e8"),
            None
        );
    }

    #[test]
    fn test_shows_real_name() {
        assert_eq!(
            display_document_name("invoice.pdf"),
            Some("invoice.pdf".to_string())
        );
    }

    #[test]
    fn test_trims_surrounding_whitespace() {
        assert_eq!(
            display_document_name("  receipt.pdf  "),
            Some("receipt.pdf".to_string())
        );
    }

    #[test]
    fn test_truncates_long_name() {
        let long = "a".repeat(200);
        let result = display_document_name(&long).expect("non-empty name should show");
        // 79 chars + 1 ellipsis char = 80 chars total
        assert_eq!(result.chars().count(), 80);
        assert!(result.ends_with('…'));
        assert!(result.starts_with(&"a".repeat(79)));
    }

    #[test]
    fn test_short_name_not_truncated() {
        let exactly_80 = "a".repeat(80);
        // 80 is not > 80, so no truncation
        assert_eq!(display_document_name(&exactly_80), Some(exactly_80.clone()));
    }

    #[test]
    fn test_name_just_over_80_truncated() {
        let just_over = "a".repeat(81);
        let result = display_document_name(&just_over).expect("non-empty name should show");
        assert_eq!(result.chars().count(), 80);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_real_filename_with_extension() {
        assert_eq!(
            display_document_name("Invoice-2026-001.pdf"),
            Some("Invoice-2026-001.pdf".to_string())
        );
    }

    #[test]
    fn test_windows_title_with_spaces() {
        assert_eq!(
            display_document_name("Untitled - Notepad"),
            Some("Untitled - Notepad".to_string())
        );
    }

    #[test]
    fn test_job_prefix_only_not_treated_as_legacy() {
        // Anything starting with "job-" is treated as legacy sentinel
        assert_eq!(display_document_name("job-summary.txt"), None);
        // "jobbook" has no dash, so it's not the legacy sentinel
        assert_eq!(
            display_document_name("jobbook.pdf"),
            Some("jobbook.pdf".to_string())
        );
    }

    #[test]
    fn test_utf8_multibyte_not_split() {
        // Verify char-based truncation doesn't split a multibyte sequence
        let utf8_heavy = "日本語".repeat(50); // 150 chars, 450 bytes
        let result = display_document_name(&utf8_heavy).expect("non-empty");
        assert_eq!(result.chars().count(), 80);
        // The string must be valid UTF-8 (can't split a multi-byte char)
        assert!(result.is_char_boundary(result.len()));
    }
}
