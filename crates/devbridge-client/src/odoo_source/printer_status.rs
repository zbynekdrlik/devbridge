//! Printer state for the Odoo heartbeat (`paper_status` / `error_status`).
//!
//! Read from the Windows queue (`Get-Printer … PrinterStatus`). A USB TSC has
//! no paper back-channel beyond what its driver reports to the queue, so this
//! is the same signal an operator sees in Windows.

/// `(paper_status, error_status)` for a raw `PrinterStatus` value (e.g.
/// `Normal`, `Offline`, `PaperOut`, possibly several comma-separated), or
/// `None` when the queue could not be read.
///
/// - `paper_status`: `ok`, or the paper condition(s) in snake case
///   (`paper_out`, `paper_jam`, `paper_problem`).
/// - `error_status`: `ok` for a normal/idle queue, else every reported
///   condition in snake case (`offline`, `error`, `paused`, `paper_out`, …).
/// - both `unknown` when the status is unavailable.
pub fn map_printer_status(raw: Option<&str>) -> (String, String) {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return ("unknown".into(), "unknown".into());
    };
    let flags: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(snake_case)
        .collect();
    let paper: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|f| f.starts_with("paper_"))
        .collect();
    let problems: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|f| *f != "normal")
        .collect();
    (join_or_ok(&paper), join_or_ok(&problems))
}

/// `ok` for no conditions, else the conditions comma-joined.
fn join_or_ok(conditions: &[&str]) -> String {
    if conditions.is_empty() {
        "ok".to_string()
    } else {
        conditions.join(",")
    }
}

/// `PaperOut` → `paper_out`, `Offline` → `offline`.
fn snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// PowerShell printing the queue's `PrinterStatus` for `printer`.
pub fn status_query(printer: &str) -> String {
    format!(
        "Get-Printer -Name '{}' -ErrorAction Stop | ForEach-Object {{ [string]$_.PrinterStatus }}",
        printer.replace('\'', "''")
    )
}

/// Current raw `PrinterStatus` of `printer` (blocking; call from
/// `spawn_blocking`). `None` off Windows or when the query fails.
pub fn query_printer_status(printer: &str) -> Option<String> {
    platform::query(printer)
}

#[cfg(target_os = "windows")]
#[path = "platform_windows.rs"]
mod platform;

#[cfg(not(target_os = "windows"))]
mod platform {
    pub(super) fn query(_printer: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(raw: Option<&str>) -> (String, String) {
        map_printer_status(raw)
    }

    #[test]
    fn test_normal_is_ok_ok() {
        assert_eq!(map(Some("Normal")), ("ok".into(), "ok".into()));
        assert_eq!(map(Some(" Normal \r\n")), ("ok".into(), "ok".into()));
    }

    #[test]
    fn test_unknown_when_unreadable() {
        assert_eq!(map(None), ("unknown".into(), "unknown".into()));
        assert_eq!(map(Some("  ")), ("unknown".into(), "unknown".into()));
    }

    #[test]
    fn test_offline_and_error() {
        assert_eq!(map(Some("Offline")), ("ok".into(), "offline".into()));
        assert_eq!(map(Some("Error")), ("ok".into(), "error".into()));
    }

    #[test]
    fn test_paper_conditions_show_in_both_fields() {
        assert_eq!(
            map(Some("PaperOut")),
            ("paper_out".into(), "paper_out".into())
        );
        assert_eq!(
            map(Some("Error, PaperJam")),
            ("paper_jam".into(), "error,paper_jam".into())
        );
        assert_eq!(
            map(Some("PaperProblem,Offline")),
            ("paper_problem".into(), "paper_problem,offline".into())
        );
    }

    #[test]
    fn test_snake_case() {
        assert_eq!(snake_case("PaperOut"), "paper_out");
        assert_eq!(snake_case("Offline"), "offline");
        assert_eq!(snake_case("IOActive"), "i_o_active");
        assert_eq!(snake_case("normal"), "normal");
    }

    #[test]
    fn test_status_query_escapes_quotes() {
        let q = status_query("O'Brien TSC");
        assert!(q.contains("-Name 'O''Brien TSC'"), "{q}");
        assert!(q.contains("[string]$_.PrinterStatus"), "{q}");
        assert!(!q.contains('\n'));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_query_is_none_off_windows() {
        assert_eq!(query_printer_status("TSC ML241P"), None);
    }
}
