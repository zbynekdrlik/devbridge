//! RAW Windows-spooler backend (`print_backend = "windows_spooler_raw"`, #88).
//!
//! For label printers (pz-spisska TSC ML241P): the SERVER-side virtual
//! printer uses the vendor driver (e.g. `TSC ML241P`), so the job payload is
//! already the printer's native language (TSPL). This backend hands those
//! bytes to the local Windows printer UNCHANGED — winspool
//! `OpenPrinterW` → `StartDocPrinterW(datatype "RAW")` → `WritePrinter` →
//! `EndDocPrinter` — with no PDF rendering, no Ghostscript, no SumatraPDF.
//! N copies = the payload written N times inside ONE spooler document (the
//! same way `direct_raw` repeats the stream per copy).
//!
//! Verification is the Windows Print Service Operational log **EventID 307**
//! ("document printed"), correlated by the spooler job id `StartDocPrinterW`
//! returned and the printer name, and its byte count must equal what was
//! spooled. The event is read through its locale-independent `Properties`
//! (`[0]` job id, `[4]` printer, `[5]` port, `[6]` bytes) — the message text
//! is localized (Slovak on the store PCs). An EventID 842 with a non-zero
//! print-processor error for the job (e.g. a v4 XPS driver refusing RAW data)
//! fails the job at once instead of after the 60 s verify window.

use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo, bail_if_cancelled};

/// `print_backend` config value selecting this backend.
pub const BACKEND_NAME: &str = "windows_spooler_raw";

/// Verification method recorded on the job (same as `windows_spooler`).
pub const VERIFICATION_METHOD: &str = "eventid_307";

pub struct WindowsSpoolerRaw {
    target_printer: String,
}

impl WindowsSpoolerRaw {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }

    /// The configured local Windows printer (used when a job carries no
    /// per-job printer name).
    pub fn target_printer(&self) -> &str {
        &self.target_printer
    }
}

/// Bytes the spooler must report for a job: the payload once per copy
/// (a copy count of 0 is treated as 1, like every other backend).
pub fn expected_spooled_bytes(payload_len: usize, copies: u32) -> u64 {
    payload_len as u64 * u64::from(copies.max(1))
}

/// Spooler document name: the job's document name, or a DevBridge fallback
/// carrying the job id when the application sent none.
pub fn spool_document_name(document_name: &str, job_id: &str) -> String {
    let trimmed = document_name.trim();
    if trimmed.is_empty() {
        format!("DevBridge {job_id}")
    } else {
        trimmed.to_string()
    }
}

/// What the Print Service log says about our RAW job so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpoolerOutcome {
    /// EventID 307: the port monitor delivered `bytes` bytes via `port`.
    Printed { bytes: u64, port: String },
    /// EventID 842 with a non-zero Win32 error: the print processor refused
    /// the job (e.g. a v4 XPS driver cannot take RAW data — 87).
    ProcessorFailed { code: u64, processor: String },
    /// Nothing yet.
    Pending,
}

/// One-line PowerShell printing `307|<bytes>|<port>` and/or
/// `842|<win32 error>|<print processor>` for spooler job `spool_job_id` on
/// `printer`, or nothing while neither happened. Reads the event
/// `Properties` (307: [0] job, [4] printer, [5] port, [6] bytes;
/// 842: [0] job, [1] processor, [2] printer, [5] error) — never the
/// localized message. Only events newer than `since_local` (local time,
/// `yyyy-MM-ddTHH:mm:ss`) count, so a recycled job id cannot match an old one.
pub fn spooler_events_query(printer: &str, spool_job_id: u32, since_local: &str) -> String {
    format!(
        "$ev = Get-WinEvent -FilterHashtable @{{LogName='Microsoft-Windows-PrintService/Operational'; Id=@(307,842); StartTime=[datetime]'{since}'}} -ErrorAction SilentlyContinue; \
         $ev | Where-Object {{ $_.Id -eq 307 -and [string]$_.Properties[0].Value -eq '{job}' -and [string]$_.Properties[4].Value -eq '{printer}' }} | Select-Object -First 1 | ForEach-Object {{ '307|{{0}}|{{1}}' -f $_.Properties[6].Value, $_.Properties[5].Value }}; \
         $ev | Where-Object {{ $_.Id -eq 842 -and [string]$_.Properties[0].Value -eq '{job}' -and [string]$_.Properties[2].Value -eq '{printer}' -and [string]$_.Properties[5].Value -ne '0' }} | Select-Object -First 1 | ForEach-Object {{ '842|{{0}}|{{1}}' -f $_.Properties[5].Value, $_.Properties[1].Value }}",
        since = since_local,
        job = spool_job_id,
        printer = printer.replace('\'', "''"),
    )
}

/// Parse [`spooler_events_query`] output. A 307 wins over an 842 (the job
/// was delivered after all); unparseable lines are ignored.
pub fn parse_spooler_outcome(stdout: &str) -> SpoolerOutcome {
    let mut failed = None;
    for line in stdout.lines().map(str::trim) {
        let mut parts = line.splitn(3, '|');
        let (Some(id), Some(num), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(num) = num.trim().parse::<u64>() else {
            continue;
        };
        match id.trim() {
            "307" => {
                return SpoolerOutcome::Printed {
                    bytes: num,
                    port: text.trim().to_string(),
                };
            }
            "842" if failed.is_none() => {
                failed = Some(SpoolerOutcome::ProcessorFailed {
                    code: num,
                    processor: text.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    failed.unwrap_or(SpoolerOutcome::Pending)
}

/// Failure detail for an EventID 842 print-processor error.
pub fn processor_failure_detail(
    spool_job_id: u32,
    printer: &str,
    processor: &str,
    code: u64,
) -> String {
    format!(
        "EventID 842: print processor {processor} refused RAW job {spool_job_id} on {printer} (Win32 error {code}) — the printer's driver cannot take RAW data; use a v3 driver (winprint)"
    )
}

/// Evidence line stored on the job for a confirmed EventID 307.
pub fn eventid_307_evidence(
    spool_job_id: u32,
    printer: &str,
    port: &str,
    bytes: u64,
    expected: u64,
) -> String {
    format!(
        "EventID 307: spooler job {spool_job_id} on {printer} via port {port}, {bytes} bytes (expected {expected})"
    )
}

/// The byte count the spooler printed must be exactly what we spooled —
/// anything else means the RAW stream was altered or truncated.
pub fn check_spooled_bytes(actual: u64, expected: u64) -> std::result::Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "EventID 307 reports {actual} bytes but {expected} bytes were spooled — RAW stream altered or truncated"
        ))
    }
}

impl PrintBackend for WindowsSpoolerRaw {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let printer = if job.printer_name.is_empty() {
            self.target_printer.as_str()
        } else {
            job.printer_name.as_str()
        };
        let display = job.printer_display_name.as_deref().unwrap_or(printer);

        bail_if_cancelled(cancel, &job.job_id, events, "before RAW spooler submit")?;

        // The payload file is named <job>.pdf by the receiver, but for this
        // backend it holds the vendor driver's native bytes — read verbatim.
        let data = std::fs::read(pdf_path)?;
        let copies = job.copies.max(1);
        let expected = expected_spooled_bytes(data.len(), copies);
        tracing::info!(
            job_id = %job.job_id,
            printer,
            payload_bytes = data.len(),
            copies,
            expected_bytes = expected,
            "RAW spooler submit (no rendering)"
        );
        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!(
                "Windows spooler RAW → {display} ({} B × {copies})",
                data.len()
            ),
        );

        platform::print_and_verify(
            job, printer, display, &data, copies, expected, events, cancel,
        )
    }
}

#[cfg(target_os = "windows")]
#[path = "platform_windows.rs"]
mod platform;

#[cfg(not(target_os = "windows"))]
mod platform {
    use anyhow::Result;
    use tokio_util::sync::CancellationToken;

    use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

    use crate::print_backend::PrintJobInfo;

    /// RAW spooling is a winspool feature — fail loudly elsewhere instead of
    /// pretending the label printed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn print_and_verify(
        job: &PrintJobInfo,
        printer: &str,
        _display: &str,
        _data: &[u8],
        _copies: u32,
        _expected: u64,
        events: &EventEmitter,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        let detail = format!(
            "print_backend \"{}\" is only supported on Windows (printer {printer})",
            super::BACKEND_NAME
        );
        events.emit(PrintJobEvent::fail(
            &job.job_id,
            PrintStage::Failed,
            &detail,
        ));
        anyhow::bail!("{detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devbridge_core::job_event::PrintJobEvent;

    /// Payload file in the OS temp dir (the client crate has no tempfile dep).
    fn payload_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "devbridge-raw-test-{tag}-{}.bin",
            std::process::id()
        ));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn test_emitter() -> (
        EventEmitter,
        tokio::sync::broadcast::Receiver<PrintJobEvent>,
    ) {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        (EventEmitter::new(tx), rx)
    }

    #[test]
    fn test_name_is_windows_spooler_raw() {
        let b = WindowsSpoolerRaw::new("TSC ML241P".into());
        assert_eq!(b.name(), "windows_spooler_raw");
        assert_eq!(b.target_printer(), "TSC ML241P");
    }

    #[test]
    fn test_expected_spooled_bytes_repeats_payload_per_copy() {
        assert_eq!(expected_spooled_bytes(332, 1), 332);
        assert_eq!(expected_spooled_bytes(332, 2), 664);
        assert_eq!(expected_spooled_bytes(332, 3), 996);
        // copies = 0 behaves like 1 (same as every other backend)
        assert_eq!(expected_spooled_bytes(332, 0), 332);
        assert_eq!(expected_spooled_bytes(0, 5), 0);
    }

    #[test]
    fn test_spool_document_name_prefers_job_name() {
        assert_eq!(spool_document_name(" label.btw ", "j-1"), "label.btw");
        assert_eq!(spool_document_name("", "j-1"), "DevBridge j-1");
        assert_eq!(spool_document_name("   ", "j-2"), "DevBridge j-2");
    }

    #[test]
    fn test_spooler_events_query_correlates_by_job_printer_time_and_properties() {
        let q = spooler_events_query("TSC ML241P", 41, "2026-09-26T12:00:00");
        assert!(q.contains("Id=@(307,842)"), "{q}");
        assert!(
            q.contains("StartTime=[datetime]'2026-09-26T12:00:00'"),
            "{q}"
        );
        assert!(
            q.contains("$_.Id -eq 307 -and [string]$_.Properties[0].Value -eq '41' -and [string]$_.Properties[4].Value -eq 'TSC ML241P'"),
            "{q}"
        );
        assert!(
            q.contains("'307|{0}|{1}' -f $_.Properties[6].Value, $_.Properties[5].Value"),
            "{q}"
        );
        assert!(
            q.contains("$_.Id -eq 842 -and [string]$_.Properties[0].Value -eq '41' -and [string]$_.Properties[2].Value -eq 'TSC ML241P' -and [string]$_.Properties[5].Value -ne '0'"),
            "{q}"
        );
        assert!(
            q.contains("'842|{0}|{1}' -f $_.Properties[5].Value, $_.Properties[1].Value"),
            "{q}"
        );
        // never the localized message text, and one line (passed via -Command)
        assert!(!q.contains("Message"), "{q}");
        assert!(!q.contains('\n'), "{q}");
    }

    #[test]
    fn test_spooler_events_query_escapes_single_quotes_in_printer() {
        let q = spooler_events_query("O'Brien label", 7, "2026-09-26T12:00:00");
        assert!(q.contains("-eq 'O''Brien label'"), "{q}");
        assert!(!q.contains("-eq 'O'Brien"), "{q}");
    }

    #[test]
    fn test_parse_spooler_outcome() {
        // exact lines seen on pz-snv (Slovak Windows) for jobs 41 and 40
        assert_eq!(
            parse_spooler_outcome("307|664|NUL:\r\n"),
            SpoolerOutcome::Printed {
                bytes: 664,
                port: "NUL:".into()
            }
        );
        assert_eq!(
            parse_spooler_outcome("842|87|MS_XPS_PROC\r\n"),
            SpoolerOutcome::ProcessorFailed {
                code: 87,
                processor: "MS_XPS_PROC".into()
            }
        );
        // delivered wins over a processor error line
        assert_eq!(
            parse_spooler_outcome("842|87|X\r\n307|10|USB001\r\n"),
            SpoolerOutcome::Printed {
                bytes: 10,
                port: "USB001".into()
            }
        );
        assert_eq!(parse_spooler_outcome(""), SpoolerOutcome::Pending);
        assert_eq!(parse_spooler_outcome("  \r\n"), SpoolerOutcome::Pending);
        assert_eq!(parse_spooler_outcome("garbage"), SpoolerOutcome::Pending);
        assert_eq!(
            parse_spooler_outcome("307|abc|NUL:"),
            SpoolerOutcome::Pending
        );
        assert_eq!(parse_spooler_outcome("999|1|x"), SpoolerOutcome::Pending);
        // the FIRST 842 is reported when several are logged
        assert_eq!(
            parse_spooler_outcome("842|87|MS_XPS_PROC\r\n842|5|winprint\r\n"),
            SpoolerOutcome::ProcessorFailed {
                code: 87,
                processor: "MS_XPS_PROC".into()
            }
        );
    }

    #[test]
    fn test_processor_failure_detail() {
        let d = processor_failure_detail(40, "DevBridge-NullPrinter", "MS_XPS_PROC", 87);
        assert!(d.starts_with("EventID 842:"), "{d}");
        assert!(
            d.contains("MS_XPS_PROC") && d.contains("job 40") && d.contains("error 87"),
            "{d}"
        );
        assert!(d.contains("DevBridge-NullPrinter"), "{d}");
    }

    #[test]
    fn test_check_spooled_bytes() {
        assert_eq!(check_spooled_bytes(664, 664), Ok(()));
        let err = check_spooled_bytes(663, 664).unwrap_err();
        assert!(err.contains("663") && err.contains("664"), "{err}");
        assert!(check_spooled_bytes(665, 664).is_err());
    }

    #[test]
    fn test_eventid_307_evidence_carries_the_byte_count() {
        let e = eventid_307_evidence(41, "DevBridge-E2E-Raw", "NUL:", 664, 664);
        assert_eq!(
            e,
            "EventID 307: spooler job 41 on DevBridge-E2E-Raw via port NUL:, 664 bytes (expected 664)"
        );
    }

    #[test]
    fn test_print_honours_cancellation_before_submit() {
        let payload = payload_file("cancel", b"SIZE 50 mm,30 mm\r\nPRINT 1\r\n");
        let (events, mut rx) = test_emitter();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let job = PrintJobInfo {
            job_id: "job-raw".into(),
            document_name: "label".into(),
            copies: 1,
            duplex: false,
            color: false,
            printer_name: "TSC ML241P".into(),
            printer_display_name: None,
        };
        let err = WindowsSpoolerRaw::new("TSC ML241P".into())
            .print(&job, &payload, &events, &cancel)
            .unwrap_err();
        let _ = std::fs::remove_file(&payload);
        assert!(
            err.to_string().contains("before RAW spooler submit"),
            "{err}"
        );
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.stage, PrintStage::Failed);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_print_fails_loudly_off_windows() {
        let payload = payload_file("offwin", b"PRINT 1\r\n");
        let (events, mut rx) = test_emitter();
        let job = PrintJobInfo {
            job_id: "job-raw".into(),
            document_name: String::new(),
            copies: 2,
            duplex: false,
            color: false,
            printer_name: String::new(),
            printer_display_name: None,
        };
        let err = WindowsSpoolerRaw::new("TSC ML241P".into())
            .print(&job, &payload, &events, &CancellationToken::new())
            .unwrap_err();
        let _ = std::fs::remove_file(&payload);
        let msg = err.to_string();
        assert!(msg.contains("only supported on Windows"), "{msg}");
        // falls back to the configured target printer when the job has none
        assert!(msg.contains("TSC ML241P"), "{msg}");
        // Sending event first, then the Failed event
        assert_eq!(rx.try_recv().unwrap().stage, PrintStage::Sending);
        assert_eq!(rx.try_recv().unwrap().stage, PrintStage::Failed);
    }
}
