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

    fn map(status: u32, attributes: u32) -> (String, String) {
        map_status_bits(status, attributes)
    }

    fn pair(paper: &str, error: &str) -> (String, String) {
        (paper.to_string(), error.to_string())
    }

    #[test]
    fn test_zero_is_ok_ok() {
        assert_eq!(map(0, 0), pair("ok", "ok"));
    }

    #[test]
    fn test_every_status_bit_maps_to_its_name() {
        for (bit, name) in STATUS_NAMES {
            let paper = if name.starts_with("paper_") {
                name
            } else {
                "ok"
            };
            assert_eq!(map(bit, 0), pair(paper, name), "bit 0x{bit:x}");
        }
    }

    #[test]
    fn test_status_names_are_the_ascending_status_bits() {
        for (i, (bit, _)) in STATUS_NAMES.iter().enumerate() {
            assert_eq!(*bit, 1 << i, "entry {i}");
        }
        let all = STATUS_NAMES.iter().fold(0u32, |acc, (bit, _)| acc | bit);
        assert_eq!(all, KNOWN_BITS);
        assert_eq!(KNOWN_BITS, 0x7FF_FFFF);
    }

    #[test]
    fn test_names_match_the_old_get_printer_values() {
        // What 0.8.43 sent for these `Get-Printer` PrinterStatus strings.
        assert_eq!(map(0x80, 0), pair("ok", "offline"));
        assert_eq!(map(0x2, 0), pair("ok", "error"));
        assert_eq!(map(0x10, 0), pair("paper_out", "paper_out"));
        assert_eq!(map(0x1, 0), pair("ok", "paused"));
        assert_eq!(map(0x100, 0), pair("ok", "io_active"));
        assert_eq!(map(0x10_0000, 0), pair("ok", "user_intervention"));
    }

    #[test]
    fn test_combinations_join_in_bit_order() {
        assert_eq!(map(0x2 | 0x8, 0), pair("paper_jam", "error,paper_jam"));
        assert_eq!(
            map(0x40 | 0x80, 0),
            pair("paper_problem", "paper_problem,offline")
        );
        assert_eq!(
            map(0x8 | 0x10 | 0x40, 0),
            pair(
                "paper_jam,paper_out,paper_problem",
                "paper_jam,paper_out,paper_problem"
            )
        );
        assert_eq!(
            map(0x40_0000 | 0x10 | 0x2, 0),
            pair("paper_out", "error,paper_out,door_open")
        );
    }

    #[test]
    fn test_work_offline_attribute_reports_offline() {
        assert_eq!(map(0, ATTRIBUTE_WORK_OFFLINE), pair("ok", "offline"));
        // reported once when the status bit is set too
        assert_eq!(map(0x80, ATTRIBUTE_WORK_OFFLINE), pair("ok", "offline"));
        assert_eq!(
            map(0x10, ATTRIBUTE_WORK_OFFLINE),
            pair("paper_out", "paper_out,offline")
        );
    }

    #[test]
    fn test_other_attributes_do_not_change_the_status() {
        // Live values (pz-snv, 2026-10-14): Microsoft Print to PDF 0x240,
        // the Generic / Text Only E2E queue 0x40 -- both status 0.
        assert_eq!(map(0, 0x240), pair("ok", "ok"));
        assert_eq!(map(0, 0x40), pair("ok", "ok"));
        assert_eq!(map(0, !ATTRIBUTE_WORK_OFFLINE), pair("ok", "ok"));
    }

    #[test]
    fn test_undocumented_bits_are_reported_not_dropped() {
        assert_eq!(map(0x8000_0000, 0), pair("ok", "status_0x80000000"));
        assert_eq!(
            map(0x10 | 0x1800_0000, 0),
            pair("paper_out", "paper_out,status_0x18000000")
        );
    }

    #[test]
    fn test_heartbeat_status_is_unknown_when_unreadable() {
        let ok = Ok(QueueState {
            status: 0x10,
            attributes: 0,
        });
        assert_eq!(heartbeat_status(&ok), pair("paper_out", "paper_out"));
        for err in [
            StatusError::Open(1801),
            StatusError::Get(5),
            StatusError::TimedOut,
            StatusError::StillRunning,
            StatusError::TaskFailed,
            StatusError::Unsupported,
        ] {
            assert_eq!(
                heartbeat_status(&Err(err.clone())),
                pair(UNKNOWN, UNKNOWN),
                "{err:?}"
            );
        }
    }

    #[test]
    fn test_status_error_messages_name_the_cause() {
        assert_eq!(
            StatusError::Open(1801).to_string(),
            "OpenPrinterW failed (Win32 error 1801)"
        );
        assert_eq!(
            StatusError::Get(122).to_string(),
            "GetPrinterW level 2 failed (Win32 error 122)"
        );
        assert!(StatusError::TimedOut.to_string().contains("did not answer"));
        assert!(
            StatusError::StillRunning
                .to_string()
                .contains("previous status read")
        );
        assert!(
            StatusError::TaskFailed
                .to_string()
                .contains("did not finish")
        );
        assert!(StatusError::Unsupported.to_string().contains("no Windows"));
    }

    #[test]
    fn test_status_log_warns_on_first_failure_and_on_change_only() {
        let open = StatusError::Open(1801);
        let hung = StatusError::TimedOut;
        assert_eq!(status_log(None, None), StatusLog::Quiet);
        assert_eq!(status_log(None, Some(&open)), StatusLog::Warn);
        assert_eq!(status_log(Some(&open), Some(&open)), StatusLog::Quiet);
        assert_eq!(status_log(Some(&open), Some(&hung)), StatusLog::Warn);
        assert_eq!(
            status_log(Some(&StatusError::Open(5)), Some(&open)),
            StatusLog::Warn
        );
        assert_eq!(status_log(Some(&hung), None), StatusLog::Recovered);
    }

    fn ok_paper_out(_: &str) -> Result<QueueState, StatusError> {
        Ok(QueueState {
            status: 0x10,
            attributes: 0,
        })
    }

    fn no_such_printer(_: &str) -> Result<QueueState, StatusError> {
        Err(StatusError::Open(1801))
    }

    fn slow_ok(_: &str) -> Result<QueueState, StatusError> {
        std::thread::sleep(Duration::from_millis(600));
        Ok(QueueState {
            status: 0,
            attributes: 0,
        })
    }

    fn panics(_: &str) -> Result<QueueState, StatusError> {
        panic!("simulated crash inside the status read");
    }

    #[test]
    fn test_record_warns_once_per_state_change() {
        let reader = PrinterStatusReader::new(ok_paper_out, QUERY_GUARD);
        let ok = Ok(QueueState {
            status: 0,
            attributes: 0,
        });
        let missing = Err(StatusError::Open(1801));
        let hung = Err(StatusError::TimedOut);
        assert_eq!(reader.record(&ok), StatusLog::Quiet);
        assert_eq!(reader.record(&missing), StatusLog::Warn);
        assert_eq!(reader.record(&missing), StatusLog::Quiet);
        assert_eq!(reader.record(&missing), StatusLog::Quiet);
        assert_eq!(reader.record(&hung), StatusLog::Warn);
        assert_eq!(reader.record(&ok), StatusLog::Recovered);
        assert_eq!(reader.record(&ok), StatusLog::Quiet);
        assert_eq!(reader.record(&missing), StatusLog::Warn);
    }

    #[tokio::test]
    async fn test_read_maps_the_queue_state() {
        let reader = PrinterStatusReader::new(ok_paper_out, QUERY_GUARD);
        assert_eq!(reader.read("TSC").await, pair("paper_out", "paper_out"));
        // and again: the in-flight flag was released
        assert_eq!(reader.read("TSC").await, pair("paper_out", "paper_out"));
    }

    #[tokio::test]
    async fn test_read_reports_unknown_when_the_queue_is_unreadable() {
        let reader = PrinterStatusReader::new(no_such_printer, QUERY_GUARD);
        assert_eq!(reader.read("gone").await, pair(UNKNOWN, UNKNOWN));
        assert_eq!(
            reader.read_state("gone").await,
            Err(StatusError::Open(1801))
        );
    }

    #[tokio::test]
    async fn test_a_hung_spooler_is_bounded_by_the_guard() {
        let reader = PrinterStatusReader::new(slow_ok, Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(reader.read_state("TSC").await, Err(StatusError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        // The stuck read is still running: no second thread is started.
        assert_eq!(
            reader.read_state("TSC").await,
            Err(StatusError::StillRunning)
        );
        assert_eq!(reader.read("TSC").await, pair(UNKNOWN, UNKNOWN));
        // Once it finished, reads start again (and this one has time).
        tokio::time::sleep(Duration::from_millis(800)).await;
        let patient = PrinterStatusReader {
            guard: Duration::from_secs(5),
            ..reader
        };
        assert_eq!(patient.read("TSC").await, pair("ok", "ok"));
    }

    #[tokio::test]
    async fn test_a_crashed_read_releases_the_flag() {
        let reader = PrinterStatusReader::new(panics, QUERY_GUARD);
        assert_eq!(reader.read_state("TSC").await, Err(StatusError::TaskFailed));
        // not StillRunning: the flag was cleared while unwinding
        assert_eq!(reader.read_state("TSC").await, Err(StatusError::TaskFailed));
    }

    #[test]
    fn test_default_reader_has_the_short_guard() {
        let reader = PrinterStatusReader::default();
        assert_eq!(reader.guard, QUERY_GUARD);
        assert_eq!(QUERY_GUARD, Duration::from_secs(5));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_query_is_unsupported_off_windows() {
        assert_eq!(
            query_queue_state("TSC ML241P"),
            Err(StatusError::Unsupported)
        );
    }

    /// The default reader reads through `query_queue_state` (here: the
    /// non-Windows stub), not some other query.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_default_reader_uses_the_native_query() {
        let reader = PrinterStatusReader::default();
        assert_eq!(
            reader.read_state("TSC ML241P").await,
            Err(StatusError::Unsupported)
        );
        assert_eq!(reader.read("TSC ML241P").await, pair(UNKNOWN, UNKNOWN));
    }

    /// The native read on a real Windows queue (CI `windows-latest` + pz-snv
    /// both have `Microsoft Print to PDF`): answers fast, spawns nothing.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_native_query_reads_microsoft_print_to_pdf() {
        let started = Instant::now();
        let state = query_queue_state("Microsoft Print to PDF");
        let elapsed = started.elapsed();
        let state = state.expect("Microsoft Print to PDF must be readable");
        let (paper, error) = map_status_bits(state.status, state.attributes);
        assert_ne!(paper, UNKNOWN);
        assert_ne!(error, UNKNOWN);
        // Get-Printer's PowerShell cold start took 2.6 s idle; the native
        // read takes milliseconds (26 ms measured on pz-snv).
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_native_query_reports_a_missing_printer() {
        assert_eq!(
            query_queue_state("devbridge-no-such-printer-97"),
            Err(StatusError::Open(1801)),
            "ERROR_INVALID_PRINTER_NAME"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_status_table_matches_the_winspool_constants() {
        use windows_sys::Win32::Graphics::Printing as P;
        let expected = [
            (P::PRINTER_STATUS_PAUSED, "paused"),
            (P::PRINTER_STATUS_ERROR, "error"),
            (P::PRINTER_STATUS_PENDING_DELETION, "pending_deletion"),
            (P::PRINTER_STATUS_PAPER_JAM, "paper_jam"),
            (P::PRINTER_STATUS_PAPER_OUT, "paper_out"),
            (P::PRINTER_STATUS_MANUAL_FEED, "manual_feed"),
            (P::PRINTER_STATUS_PAPER_PROBLEM, "paper_problem"),
            (P::PRINTER_STATUS_OFFLINE, "offline"),
            (P::PRINTER_STATUS_IO_ACTIVE, "io_active"),
            (P::PRINTER_STATUS_BUSY, "busy"),
            (P::PRINTER_STATUS_PRINTING, "printing"),
            (P::PRINTER_STATUS_OUTPUT_BIN_FULL, "output_bin_full"),
            (P::PRINTER_STATUS_NOT_AVAILABLE, "not_available"),
            (P::PRINTER_STATUS_WAITING, "waiting"),
            (P::PRINTER_STATUS_PROCESSING, "processing"),
            (P::PRINTER_STATUS_INITIALIZING, "initializing"),
            (P::PRINTER_STATUS_WARMING_UP, "warming_up"),
            (P::PRINTER_STATUS_TONER_LOW, "toner_low"),
            (P::PRINTER_STATUS_NO_TONER, "no_toner"),
            (P::PRINTER_STATUS_PAGE_PUNT, "page_punt"),
            (P::PRINTER_STATUS_USER_INTERVENTION, "user_intervention"),
            (P::PRINTER_STATUS_OUT_OF_MEMORY, "out_of_memory"),
            (P::PRINTER_STATUS_DOOR_OPEN, "door_open"),
            (P::PRINTER_STATUS_SERVER_UNKNOWN, "server_unknown"),
            (P::PRINTER_STATUS_POWER_SAVE, "power_save"),
            (P::PRINTER_STATUS_SERVER_OFFLINE, "server_offline"),
            (
                P::PRINTER_STATUS_DRIVER_UPDATE_NEEDED,
                "driver_update_needed",
            ),
        ];
        assert_eq!(STATUS_NAMES, expected);
        assert_eq!(ATTRIBUTE_WORK_OFFLINE, P::PRINTER_ATTRIBUTE_WORK_OFFLINE);
        assert_eq!(STATUS_OFFLINE, P::PRINTER_STATUS_OFFLINE);
    }
}
