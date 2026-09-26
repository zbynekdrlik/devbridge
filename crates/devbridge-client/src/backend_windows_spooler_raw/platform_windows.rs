//! Windows half of the RAW spooler backend (#88): winspool RAW submit +
//! EventID 307/842 verification. Compiled only on Windows, so it lives in its
//! own file that `.cargo/mutants.toml` excludes (the Linux mutation gate
//! cannot build or run it); every pure decision it makes is a tested helper in
//! `mod.rs`.

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
    let spool_job_id = winspool::spool_raw(printer, &doc_name, data, copies, |copy| {
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

/// Thin safe wrapper over the winspool RAW-document calls.
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
