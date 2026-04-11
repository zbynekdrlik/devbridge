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

#[cfg(test)]
mod tests {
    use super::*;

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
