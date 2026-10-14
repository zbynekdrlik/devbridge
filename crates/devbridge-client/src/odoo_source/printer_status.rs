//! Printer state for the Odoo heartbeat (`paper_status` / `error_status`).
//!
//! Read natively from the Windows queue: winspool `OpenPrinterW` →
//! `GetPrinterW` level 2 → `PRINTER_INFO_2W.Status` / `.Attributes` (#97).
//! A USB TSC has no paper back-channel beyond what its driver reports to the
//! queue, so this is the same signal an operator sees in Windows.
//!
//! Until 0.8.43 every heartbeat started a new `powershell.exe` running
//! `Get-Printer`; its cold start (PrintManagement module load, 2.6 s on an
//! idle store PC) regularly ran past the 10 s timeout on a busy one — 447
//! `unknown` heartbeats on spisska-pc. The native read answers in
//! milliseconds and spawns no process.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

/// Value of both fields when the queue state could not be read.
pub const UNKNOWN: &str = "unknown";

/// `PRINTER_ATTRIBUTE_WORK_OFFLINE` ("Use Printer Offline" in Windows).
pub const ATTRIBUTE_WORK_OFFLINE: u32 = 0x400;

/// `PRINTER_STATUS_OFFLINE`.
const STATUS_OFFLINE: u32 = 0x80;

/// Every `PRINTER_STATUS_*` bit with its Odoo name, in ascending bit order.
///
/// The PowerShell `PrinterStatus` enum is the SAME bitmask, and these names
/// are its members snake-cased (`PaperOut` → `paper_out`, `IoActive` →
/// `io_active`) — exactly what the heartbeat sent while it used
/// `Get-Printer`, so Odoo keeps receiving the same values.
pub const STATUS_NAMES: [(u32, &str); 27] = [
    (0x1, "paused"),
    (0x2, "error"),
    (0x4, "pending_deletion"),
    (0x8, "paper_jam"),
    (0x10, "paper_out"),
    (0x20, "manual_feed"),
    (0x40, "paper_problem"),
    (0x80, "offline"),
    (0x100, "io_active"),
    (0x200, "busy"),
    (0x400, "printing"),
    (0x800, "output_bin_full"),
    (0x1000, "not_available"),
    (0x2000, "waiting"),
    (0x4000, "processing"),
    (0x8000, "initializing"),
    (0x1_0000, "warming_up"),
    (0x2_0000, "toner_low"),
    (0x4_0000, "no_toner"),
    (0x8_0000, "page_punt"),
    (0x10_0000, "user_intervention"),
    (0x20_0000, "out_of_memory"),
    (0x40_0000, "door_open"),
    (0x80_0000, "server_unknown"),
    (0x100_0000, "power_save"),
    (0x200_0000, "server_offline"),
    (0x400_0000, "driver_update_needed"),
];

/// The bits [`STATUS_NAMES`] covers (bits 0..=26).
const KNOWN_BITS: u32 = (1 << 27) - 1;

/// Longest wait for the spooler. The native read answers in milliseconds;
/// this only bounds a truly hung spooler — exactly when the status matters,
/// so the heartbeat still goes out (with `unknown`).
pub const QUERY_GUARD: Duration = Duration::from_secs(5);

/// Queue state as winspool reports it (`PRINTER_INFO_2W`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueState {
    pub status: u32,
    pub attributes: u32,
}

/// Why the queue state could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusError {
    /// `OpenPrinterW` failed with this Win32 error (1801 = no such printer).
    Open(i32),
    /// `GetPrinterW` level 2 failed with this Win32 error.
    Get(i32),
    /// The spooler did not answer within the guard.
    TimedOut,
    /// The previous read is still waiting for the spooler — not started again.
    StillRunning,
    /// The blocking read did not finish (panic or runtime shutdown).
    TaskFailed,
    /// Not a Windows build: there is no queue to read.
    Unsupported,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(code) => write!(f, "OpenPrinterW failed (Win32 error {code})"),
            Self::Get(code) => write!(f, "GetPrinterW level 2 failed (Win32 error {code})"),
            Self::TimedOut => write!(f, "the spooler did not answer in time (hung?)"),
            Self::StillRunning => write!(
                f,
                "the previous status read is still waiting for the spooler (hung?)"
            ),
            Self::TaskFailed => write!(f, "the status read did not finish (panic or shutdown)"),
            Self::Unsupported => write!(f, "no Windows print queue on this platform"),
        }
    }
}

/// `(paper_status, error_status)` for a winspool queue state.
///
/// - `paper_status`: `ok`, or the paper condition(s) (`paper_jam`,
///   `paper_out`, `paper_problem`).
/// - `error_status`: `ok` for a normal/idle queue (status 0), else every
///   reported condition comma-joined in ascending bit order (`offline`,
///   `error,paper_jam`, …). The "Use Printer Offline" attribute reports
///   `offline`; an undocumented bit is reported as `status_0x<hex>`, never
///   dropped.
pub fn map_status_bits(status: u32, attributes: u32) -> (String, String) {
    let mut bits = status;
    if attributes & ATTRIBUTE_WORK_OFFLINE != 0 {
        bits |= STATUS_OFFLINE;
    }
    let mut conditions: Vec<String> = STATUS_NAMES
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    let undocumented = bits & !KNOWN_BITS;
    if undocumented != 0 {
        conditions.push(format!("status_0x{undocumented:x}"));
    }
    let paper: Vec<&str> = conditions
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with("paper_"))
        .collect();
    let all: Vec<&str> = conditions.iter().map(String::as_str).collect();
    (join_or_ok(&paper), join_or_ok(&all))
}

/// Both heartbeat fields for a read: the mapped state, or [`UNKNOWN`] twice.
pub fn heartbeat_status(read: &Result<QueueState, StatusError>) -> (String, String) {
    match read {
        Ok(q) => map_status_bits(q.status, q.attributes),
        Err(_) => (UNKNOWN.to_string(), UNKNOWN.to_string()),
    }
}

/// `ok` for no conditions, else the conditions comma-joined.
fn join_or_ok(conditions: &[&str]) -> String {
    if conditions.is_empty() {
        "ok".to_string()
    } else {
        conditions.join(",")
    }
}

/// What to log after a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLog {
    /// Nothing changed — stay quiet (no WARN every heartbeat).
    Quiet,
    /// The first failure, or a different failure than the previous read.
    Warn,
    /// Readable again after a failure.
    Recovered,
}

/// The log decision for a read failing with `now` (`None` = readable) after
/// a read that failed with `previous`.
pub fn status_log(previous: Option<&StatusError>, now: Option<&StatusError>) -> StatusLog {
    match (previous, now) {
        (prev, Some(err)) if prev != Some(err) => StatusLog::Warn,
        (Some(_), None) => StatusLog::Recovered,
        _ => StatusLog::Quiet,
    }
}

/// The blocking queue-state read ([`query_queue_state`] in production).
pub type QueryFn = fn(&str) -> Result<QueueState, StatusError>;

/// Current queue state of `printer` (blocking; milliseconds on a healthy
/// spooler). [`StatusError::Unsupported`] off Windows.
pub fn query_queue_state(printer: &str) -> Result<QueueState, StatusError> {
    platform::query(printer)
}

/// Reads the queue state for the heartbeat: the blocking read runs on the
/// blocking pool, bounded by a guard; never more than one read is in flight
/// (a hung spooler must not pile up a stuck thread every heartbeat); a WARN
/// only when a failure starts or changes, an INFO when it recovers.
pub struct PrinterStatusReader {
    query: QueryFn,
    guard: Duration,
    in_flight: Arc<AtomicBool>,
    last_error: Mutex<Option<StatusError>>,
}

impl Default for PrinterStatusReader {
    fn default() -> Self {
        Self::new(query_queue_state, QUERY_GUARD)
    }
}

impl PrinterStatusReader {
    pub fn new(query: QueryFn, guard: Duration) -> Self {
        Self {
            query,
            guard,
            in_flight: Arc::new(AtomicBool::new(false)),
            last_error: Mutex::new(None),
        }
    }

    /// `(paper_status, error_status)` of `printer` for the heartbeat.
    pub async fn read(&self, printer: &str) -> (String, String) {
        let started = Instant::now();
        let read = self.read_state(printer).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (paper_status, error_status) = heartbeat_status(&read);
        let error = read.as_ref().err().map(ToString::to_string);
        debug!(printer, %paper_status, %error_status, ?error, elapsed_ms, "printer status read");
        match self.record(&read) {
            StatusLog::Warn => warn!(
                printer,
                error = %error.unwrap_or_default(),
                guard_ms = self.guard.as_millis() as u64,
                elapsed_ms,
                "printer status unreadable — heartbeat reports unknown (logged again only when this changes)"
            ),
            StatusLog::Recovered => info!(
                printer,
                %paper_status,
                %error_status,
                elapsed_ms,
                "printer status readable again"
            ),
            StatusLog::Quiet => {}
        }
        (paper_status, error_status)
    }

    /// One bounded read of the queue state.
    async fn read_state(&self, printer: &str) -> Result<QueueState, StatusError> {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return Err(StatusError::StillRunning);
        }
        let done = InFlight(Arc::clone(&self.in_flight));
        let query = self.query;
        let printer = printer.to_string();
        let task = tokio::task::spawn_blocking(move || {
            let _done = done;
            query(&printer)
        });
        match tokio::time::timeout(self.guard, task).await {
            Ok(Ok(read)) => read,
            Ok(Err(_join)) => Err(StatusError::TaskFailed),
            Err(_elapsed) => Err(StatusError::TimedOut),
        }
    }

    /// Remember this read's failure (if any); what to log about it.
    pub fn record(&self, read: &Result<QueueState, StatusError>) -> StatusLog {
        let now = read.as_ref().err().cloned();
        let mut last = self.last_error.lock().unwrap_or_else(|p| p.into_inner());
        let log = status_log(last.as_ref(), now.as_ref());
        *last = now;
        log
    }
}

/// Clears the in-flight flag when the blocking read ends — also when it
/// panics or the closure is dropped unrun.
struct InFlight(Arc<AtomicBool>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "windows")]
#[path = "platform_windows.rs"]
mod platform;

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::{QueueState, StatusError};

    pub(super) fn query(_printer: &str) -> Result<QueueState, StatusError> {
        Err(StatusError::Unsupported)
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
