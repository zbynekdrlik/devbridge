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
mod platform {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use tokio_util::sync::CancellationToken;

    use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

    use super::{
        SpoolerOutcome, VERIFICATION_METHOD, check_spooled_bytes, eventid_307_evidence,
        parse_spooler_outcome, processor_failure_detail, spool_document_name, spooler_events_query,
    };
    use crate::print_backend::{PrintJobInfo, bail_if_cancelled};

    const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
    const VERIFY_POLL: Duration = Duration::from_secs(2);

    #[allow(clippy::too_many_arguments)]
    pub(super) fn print_and_verify(
        job: &PrintJobInfo,
        printer: &str,
        display: &str,
        data: &[u8],
        copies: u32,
        expected: u64,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Make sure EventID 307 gets logged (idempotent; same as windows_spooler).
        let enable = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "wevtutil sl 'Microsoft-Windows-PrintService/Operational' /e:true",
            ])
            .output();
        if let Err(e) = enable {
            tracing::warn!(job_id = %job.job_id, error = %e, "could not enable PrintService/Operational log");
        }
        // Local time, 5 s of slack for clock granularity.
        let since = (chrono::Local::now() - chrono::Duration::seconds(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        let doc_name = spool_document_name(&job.document_name, &job.job_id);
        let spool_job_id = super::winspool::spool_raw(printer, &doc_name, data, copies, |copy| {
            bail_if_cancelled(cancel, &job.job_id, events, "before RAW copy write")?;
            tracing::debug!(job_id = %job.job_id, copy, copies, "writing RAW copy");
            Ok(())
        })
        .inspect_err(|e| {
            // A cancellation already emitted its own Failed event.
            if !cancel.is_cancelled() {
                events.emit_fail(
                    &job.job_id,
                    PrintStage::Failed,
                    format!("RAW spool to {display} failed: {e:#}"),
                );
            }
        })?;

        tracing::info!(job_id = %job.job_id, printer, spool_job_id, expected, "RAW job spooled");
        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("Spooled as RAW job {spool_job_id} on {display} ({expected} B)"),
        );

        let deadline = Instant::now() + VERIFY_TIMEOUT;
        let query = spooler_events_query(printer, spool_job_id, &since);
        loop {
            bail_if_cancelled(cancel, &job.job_id, events, "during RAW EventID 307 verify")?;

            let out = Command::new("powershell")
                .args(["-NoProfile", "-Command", &query])
                .output()?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let outcome = parse_spooler_outcome(&stdout);
            if let SpoolerOutcome::ProcessorFailed { code, processor } = &outcome {
                // Fail fast instead of waiting the full 60 s (e.g. a v4 XPS
                // driver's MS_XPS_PROC refuses RAW data with error 87).
                let detail = processor_failure_detail(spool_job_id, printer, processor, *code);
                tracing::error!(job_id = %job.job_id, %detail, "RAW job refused by print processor");
                let mut fail = PrintJobEvent::fail(&job.job_id, PrintStage::Failed, &detail);
                fail.verification_method = VERIFICATION_METHOD.into();
                fail.verification_evidence = detail.clone();
                events.emit(fail);
                anyhow::bail!("{detail}");
            }
            if let SpoolerOutcome::Printed { bytes, port } = outcome {
                let evidence = eventid_307_evidence(spool_job_id, printer, &port, bytes, expected);
                if let Err(reason) = check_spooled_bytes(bytes, expected) {
                    tracing::error!(job_id = %job.job_id, %evidence, %reason, "RAW byte count mismatch");
                    let mut fail = PrintJobEvent::fail(&job.job_id, PrintStage::Failed, &reason);
                    fail.verification_method = VERIFICATION_METHOD.into();
                    fail.verification_evidence = evidence;
                    events.emit(fail);
                    anyhow::bail!("{reason}");
                }
                tracing::info!(job_id = %job.job_id, %evidence, "RAW delivery confirmed via EventID 307");
                events.emit_verified(&job.job_id, VERIFICATION_METHOD, &evidence);
                events.emit_ok(
                    &job.job_id,
                    PrintStage::Completed,
                    format!("Printed RAW on {display} (EventID 307, {bytes} B)"),
                );
                return Ok(());
            }

            if Instant::now() > deadline {
                let status_ps = format!(
                    "Get-PrintJob -PrinterName '{}' -ID {} -ErrorAction SilentlyContinue | ForEach-Object {{ [string]$_.JobStatus }}",
                    printer.replace('\'', "''"),
                    spool_job_id
                );
                let status = Command::new("powershell")
                    .args(["-NoProfile", "-Command", &status_ps])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                let detail = format!(
                    "No EventID 307 within {}s for RAW job {spool_job_id} on {printer} (spooler status: {})",
                    VERIFY_TIMEOUT.as_secs(),
                    if status.is_empty() {
                        "job gone"
                    } else {
                        status.as_str()
                    }
                );
                tracing::error!(job_id = %job.job_id, %detail, "RAW verification timed out");
                let mut fail = PrintJobEvent::fail(&job.job_id, PrintStage::Failed, &detail);
                fail.verification_method = VERIFICATION_METHOD.into();
                fail.verification_evidence = detail.clone();
                events.emit(fail);
                anyhow::bail!("{detail}");
            }
            std::thread::sleep(VERIFY_POLL);
        }
    }
}

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

/// Thin safe wrapper over the winspool RAW-document calls.
#[cfg(target_os = "windows")]
mod winspool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Graphics::Printing::{
        AbortPrinter, ClosePrinter, DOC_INFO_1W, EndDocPrinter, OpenPrinterW, PRINTER_HANDLE,
        StartDocPrinterW, WritePrinter,
    };

    /// Largest single `WritePrinter` call.
    const WRITE_CHUNK: usize = 1024 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Closes the printer handle on every exit path.
    struct PrinterHandle(PRINTER_HANDLE);

    impl Drop for PrinterHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful OpenPrinterW and is
            // closed exactly once, here.
            unsafe {
                ClosePrinter(self.0);
            }
        }
    }

    /// Spool `data` `copies` times as ONE RAW document on `printer` and
    /// return the spooler job id. `before_copy(n)` runs before copy `n`
    /// (1-based) and may abort (cancellation) — the half-written document is
    /// then deleted with `AbortPrinter`, never left to print.
    pub(super) fn spool_raw(
        printer: &str,
        doc_name: &str,
        data: &[u8],
        copies: u32,
        mut before_copy: impl FnMut(u32) -> Result<()>,
    ) -> Result<u32> {
        let printer_w = wide(printer);
        let mut raw_handle = PRINTER_HANDLE::default();
        // SAFETY: printer_w is NUL-terminated and outlives the call;
        // raw_handle is a valid out pointer; null defaults are allowed.
        let opened = unsafe { OpenPrinterW(printer_w.as_ptr(), &mut raw_handle, std::ptr::null()) };
        if opened == 0 {
            bail!(
                "OpenPrinterW(\"{printer}\") failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let handle = PrinterHandle(raw_handle);

        let mut doc_w = wide(doc_name);
        let mut datatype_w = wide("RAW");
        let info = DOC_INFO_1W {
            pDocName: doc_w.as_mut_ptr(),
            pOutputFile: std::ptr::null_mut(),
            pDatatype: datatype_w.as_mut_ptr(),
        };
        // SAFETY: handle is open; info and its strings live until after the call.
        let job_id = unsafe { StartDocPrinterW(handle.0, 1, &info) };
        if job_id == 0 {
            bail!(
                "StartDocPrinterW(RAW) on \"{printer}\" failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let written = (1..=copies).try_for_each(|copy| {
            before_copy(copy)?;
            write_all(&handle, data)
        });
        if let Err(e) = written {
            // SAFETY: handle is open with a started document.
            unsafe {
                AbortPrinter(handle.0);
            }
            return Err(e.context(format!("RAW job {job_id} on \"{printer}\" aborted")));
        }

        // SAFETY: handle is open with a started document.
        if unsafe { EndDocPrinter(handle.0) } == 0 {
            bail!(
                "EndDocPrinter for RAW job {job_id} on \"{printer}\" failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(job_id)
    }

    fn write_all(handle: &PrinterHandle, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let chunk = data.len().min(WRITE_CHUNK);
            let mut written: u32 = 0;
            // SAFETY: data[..chunk] is valid for reads; `written` is a valid
            // out pointer; chunk <= 1 MiB fits in u32.
            let ok =
                unsafe { WritePrinter(handle.0, data.as_ptr().cast(), chunk as u32, &mut written) };
            if ok == 0 {
                bail!("WritePrinter failed: {}", std::io::Error::last_os_error());
            }
            if written == 0 {
                bail!("WritePrinter accepted 0 bytes");
            }
            data = &data[written as usize..];
        }
        Ok(())
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
