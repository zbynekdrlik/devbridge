use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use devbridge_core::job::JobState;
use devbridge_core::job_event::EventEmitter;

/// A print job descriptor passed to backends.
pub struct PrintJobInfo {
    pub job_id: String,
    pub document_name: String,
    pub copies: u32,
    pub duplex: bool,
    pub color: bool,
    pub printer_name: String,
    pub printer_display_name: Option<String>,
}

/// Trait for print backends that handle delivery of a job to a printer.
///
/// `cancel` is set by the receiver when the outer per-job
/// [`crate::receiver`] timeout fires (issue #51). Cancellation is
/// COOPERATIVE: backends MUST check it at every internal decision point —
/// between page sends, at each `poll_job_completion` / verification tick, and
/// around any long-running child process (killing child processes on cancel).
/// This bounds, but does not instantly abort, an abandoned print: a syscall
/// already in flight when the token is set (e.g. a `reqwest::blocking` raster
/// upload, or a socket `write_all`) runs until its own existing timeout
/// (direct_ipp's 120 s HTTP timeout, direct_raw's 30 s socket write timeout)
/// — the token is only observed at the next tick boundary. The in-flight
/// guard ([`crate::inflight`]) is what actually prevents a requeued retry from
/// racing the still-draining task, so the printer is never hit twice even
/// while a wedged upload finishes timing out.
pub trait PrintBackend: Send + Sync {
    fn name(&self) -> &str;
    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

/// Bail out of a backend's print flow if the cancellation token is set.
///
/// Backends call this at each decision point (between page sends, at each
/// poll/verify tick). On cancel it emits a Failed event and returns an error,
/// so the abandoned print task stops touching the printer instead of racing a
/// requeued retry (issue #51). Logs the check arm taken per the project's
/// verbose-on-every-branch logging mandate.
pub(crate) fn bail_if_cancelled(
    cancel: &CancellationToken,
    job_id: &str,
    events: &EventEmitter,
    at: &str,
) -> Result<()> {
    if cancel.is_cancelled() {
        tracing::warn!(
            job_id,
            at,
            "print cancelled by outer timeout — stopping before {at} (issue #51)"
        );
        events.emit_fail(
            job_id,
            devbridge_core::job_event::PrintStage::Failed,
            format!("print cancelled by outer timeout (at: {at})"),
        );
        anyhow::bail!("print cancelled by outer timeout (at: {at})");
    }
    tracing::trace!(job_id, at, "cancel check passed — continuing print");
    Ok(())
}

/// The outcome of a job the printer ACCEPTED but whose end devbridge never
/// learned (#114, owner ruling „neposielať znova"): the printer holds it, so
/// it may well print — resending it could print it twice. A backend returns
/// it as its error; the receiver reports `success = false, unconfirmed =
/// true` and the server ends the job `unconfirmed` („možno vytlačené"),
/// never resent. A definite verdict of the printer (aborted, canceled, an
/// IPP error status) is NOT this: it proves nothing printed and is retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    detail: String,
}

impl Unconfirmed {
    /// The printer answered the Print-Job with `printer_job_id`, then `why`
    /// its end is unknown (poll deadline, a lost poll, a cancel).
    pub fn after_acceptance(printer_job_id: u32, why: &str) -> Self {
        Self {
            detail: format!(
                "printer accepted job-id={printer_job_id}, completion not confirmed ({why}) — not resent"
            ),
        }
    }

    /// A per-page job (one Print-Job per page, e.g. `jpeg`): page
    /// `failed_index + 1` of `pages` failed (`cause`) after the pages before
    /// it were accepted — they may have printed, so a resend could print them
    /// twice.
    pub fn after_accepted_pages(failed_index: usize, pages: usize, cause: &anyhow::Error) -> Self {
        Self {
            detail: format!(
                "page {} of {pages} failed after page(s) 1..{failed_index} were accepted: {cause:#}",
                failed_index + 1
            ),
        }
    }

    /// The outer print timeout fired after the printer had accepted the job.
    pub fn after_outer_timeout(timeout_secs: u64) -> Self {
        Self {
            detail: format!(
                "printer accepted the job, completion not confirmed (print task timed out after {timeout_secs}s) — not resent"
            ),
        }
    }

    /// The text the job event, the client log and pz-server show.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for Unconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for Unconfirmed {}

/// `true` when a finished print ended [`Unconfirmed`] (also behind any
/// `context`): its completion must carry `unconfirmed = true`.
pub fn is_unconfirmed(result: &Result<()>) -> bool {
    result
        .as_ref()
        .is_err_and(|e| e.downcast_ref::<Unconfirmed>().is_some())
}

/// What the receiver reports for a finished print: the server's
/// `JobCompletion` fields and the client's own job state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintReport {
    pub success: bool,
    /// #114: the printer accepted the job, its end is unknown — never resent.
    pub unconfirmed: bool,
    /// The whole `source()` chain (`{:#}`, #112): `to_string()` kept only the
    /// outermost message, so "connect timed out" / "refused" / "reset" never
    /// reached the client log or pz-server's retry log.
    pub error_detail: String,
    pub printer_status: &'static str,
    pub local_state: JobState,
}

impl PrintReport {
    pub fn of(result: &Result<()>) -> Self {
        match result {
            Ok(()) => Self {
                success: true,
                unconfirmed: false,
                error_detail: String::new(),
                printer_status: "delivered",
                local_state: JobState::Completed,
            },
            Err(e) => {
                let unconfirmed = is_unconfirmed(result);
                Self {
                    success: false,
                    unconfirmed,
                    error_detail: format!("{e:#}"),
                    printer_status: if unconfirmed { "unconfirmed" } else { "error" },
                    local_state: if unconfirmed {
                        JobState::Unconfirmed
                    } else {
                        JobState::Failed
                    },
                }
            }
        }
    }
}

/// The error of a print task the receiver's outer timeout abandoned: once the
/// printer had accepted the job (`printer_accepted`, #114) it is
/// [`Unconfirmed`] — never resent; before that nothing reached a printer and
/// the server retries it as before.
pub fn print_timeout_error(timeout_secs: u64, printer_accepted: bool) -> anyhow::Error {
    if printer_accepted {
        anyhow::Error::new(Unconfirmed::after_outer_timeout(timeout_secs))
    } else {
        anyhow::anyhow!("print task timed out after {timeout_secs}s — backend or spooler hung")
    }
}

/// Create the appropriate backend from config values.
pub fn create_backend(
    backend_type: &str,
    printer_address: Option<&str>,
    ghostscript_device: &str,
    ghostscript_resolution: u32,
    target_printer: &str,
    printer_tls: bool,
    print_proxy_url: Option<&str>,
) -> Result<Box<dyn PrintBackend>> {
    match backend_type {
        "direct_ipp" => {
            let addr = printer_address
                .ok_or_else(|| anyhow::anyhow!("direct_ipp requires printer_address"))?;
            Ok(Box::new(crate::backend_direct_ipp::DirectIpp::new(
                addr.to_string(),
                ghostscript_device.to_string(),
                ghostscript_resolution,
                printer_tls,
            )))
        }
        "direct_raw" => {
            let addr = printer_address
                .ok_or_else(|| anyhow::anyhow!("direct_raw requires printer_address"))?;
            Ok(Box::new(crate::backend_direct_raw::DirectRaw::new(
                addr.to_string(),
                ghostscript_device.to_string(),
                ghostscript_resolution,
            )))
        }
        "print_proxy" => {
            let url = print_proxy_url
                .ok_or_else(|| anyhow::anyhow!("print_proxy requires print_proxy_url"))?;
            Ok(Box::new(
                crate::backend_print_proxy::PrintProxyBackend::new(url.to_string()),
            ))
        }
        "cups" => Ok(Box::new(crate::backend_cups::CupsBackend::new(
            target_printer.to_string(),
        ))),
        "windows_spooler" | "" => Ok(Box::new(
            crate::backend_windows_spooler::WindowsSpooler::new(target_printer.to_string()),
        )),
        // Vendor-driver bytes spooled unchanged (label printers, #88).
        crate::backend_windows_spooler_raw::BACKEND_NAME => Ok(Box::new(
            crate::backend_windows_spooler_raw::WindowsSpoolerRaw::new(target_printer.to_string()),
        )),
        other => anyhow::bail!("unknown print_backend: {}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_emitter() -> EventEmitter {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        EventEmitter::new(tx)
    }

    #[test]
    fn test_bail_if_cancelled_passes_when_token_not_set() {
        let cancel = CancellationToken::new();
        let events = test_emitter();
        let r = bail_if_cancelled(&cancel, "job-x", &events, "unit");
        assert!(
            r.is_ok(),
            "must continue (Ok) when the cancel token is not set"
        );
    }

    #[test]
    fn test_bail_if_cancelled_errors_when_token_set() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let events = test_emitter();
        let r = bail_if_cancelled(&cancel, "job-x", &events, "unit-point");
        assert!(r.is_err(), "must bail (Err) when the cancel token is set");
        assert!(
            r.unwrap_err().to_string().contains("unit-point"),
            "error must name the cancellation point for debuggability"
        );
    }

    /// #114: the outcome of a job the printer ACCEPTED but whose end was
    /// never confirmed names the printer's job and why — the text the store
    /// and pz-server see.
    #[test]
    fn test_unconfirmed_names_the_printer_job_and_why() {
        let u = Unconfirmed::after_acceptance(45, "no final job-state within 60s, last state=6");
        assert_eq!(
            u.detail(),
            "printer accepted job-id=45, completion not confirmed \
             (no final job-state within 60s, last state=6) — not resent"
        );
        assert_eq!(u.to_string(), u.detail());
        assert_eq!(
            Unconfirmed::after_outer_timeout(1800).detail(),
            "printer accepted the job, completion not confirmed \
             (print task timed out after 1800s) — not resent"
        );
    }

    #[test]
    fn test_is_unconfirmed_through_any_context() {
        assert!(!is_unconfirmed(&Ok(())));
        assert!(!is_unconfirmed(&Err(anyhow::anyhow!(
            "IPP Print-Job send failed (10.78.10.9:631)"
        ))));
        let unconfirmed = || anyhow::Error::new(Unconfirmed::after_acceptance(7, "lost"));
        assert!(is_unconfirmed(&Err(unconfirmed())));
        assert!(is_unconfirmed(&Err(unconfirmed().context("page 1 of 2"))));
    }

    /// #112: the completion carries the whole cause chain; #114: an
    /// unconfirmed print is reported as such, locally too.
    #[test]
    fn test_print_report_of_each_outcome() {
        assert_eq!(
            PrintReport::of(&Ok(())),
            PrintReport {
                success: true,
                unconfirmed: false,
                error_detail: String::new(),
                printer_status: "delivered",
                local_state: JobState::Completed,
            }
        );
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out");
        let err = anyhow::Error::new(io)
            .context("error sending request for url (http://10.78.10.9:631/ipp/print)")
            .context("IPP Print-Job send failed (10.78.10.9:631)");
        assert_eq!(
            PrintReport::of(&Err(err)),
            PrintReport {
                success: false,
                unconfirmed: false,
                error_detail: "IPP Print-Job send failed (10.78.10.9:631): error sending request \
                               for url (http://10.78.10.9:631/ipp/print): connect timed out"
                    .into(),
                printer_status: "error",
                local_state: JobState::Failed,
            }
        );
        let lost = Unconfirmed::after_acceptance(45, "no final job-state within 60s, last state=6");
        assert_eq!(
            PrintReport::of(&Err(anyhow::Error::new(lost.clone()))),
            PrintReport {
                success: false,
                unconfirmed: true,
                error_detail: lost.detail().to_string(),
                printer_status: "unconfirmed",
                local_state: JobState::Unconfirmed,
            }
        );
    }

    /// #114: the outer print timeout is "nothing printed" (retried) only
    /// while the printer has not accepted the job yet.
    #[test]
    fn test_outer_timeout_is_unconfirmed_only_after_the_printer_accepted() {
        let accepted = print_timeout_error(1800, true);
        assert_eq!(
            format!("{accepted:#}"),
            Unconfirmed::after_outer_timeout(1800).detail()
        );
        assert!(is_unconfirmed(&Err(accepted)));
        let not_yet = print_timeout_error(1800, false);
        assert_eq!(
            format!("{not_yet:#}"),
            "print task timed out after 1800s — backend or spooler hung"
        );
        assert!(!is_unconfirmed(&Err(not_yet)));
    }

    #[test]
    fn test_create_backend_windows_spooler() {
        let backend = create_backend(
            "windows_spooler",
            None,
            "ppmraw",
            600,
            "TestPrinter",
            false,
            None,
        );
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().name(), "windows_spooler");
    }

    #[test]
    fn test_create_backend_windows_spooler_raw() {
        // RAW needs no printer_address / Ghostscript / proxy — only the
        // local Windows printer name (#88).
        let backend = create_backend(
            "windows_spooler_raw",
            None,
            "ppmraw",
            600,
            "TSC ML241P",
            false,
            None,
        )
        .expect("windows_spooler_raw must be a known backend");
        assert_eq!(backend.name(), "windows_spooler_raw");
    }

    #[test]
    fn test_create_backend_raw_is_distinct_from_pdf_spooler() {
        // The PDF path must stay the PDF path: plain windows_spooler is NOT
        // silently turned into RAW (existing stores unchanged).
        let pdf = create_backend("windows_spooler", None, "ppmraw", 600, "P", false, None).unwrap();
        let raw =
            create_backend("windows_spooler_raw", None, "ppmraw", 600, "P", false, None).unwrap();
        assert_ne!(pdf.name(), raw.name());
    }

    #[test]
    fn test_create_backend_default_empty() {
        let backend = create_backend("", None, "ppmraw", 600, "TestPrinter", false, None);
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().name(), "windows_spooler");
    }

    #[test]
    fn test_create_backend_direct_raw_requires_address() {
        let backend = create_backend(
            "direct_raw",
            None,
            "ppmraw",
            600,
            "TestPrinter",
            false,
            None,
        );
        assert!(backend.is_err());
        assert!(
            backend
                .err()
                .unwrap()
                .to_string()
                .contains("printer_address")
        );
    }

    #[test]
    fn test_create_backend_direct_ipp_requires_address() {
        let backend = create_backend(
            "direct_ipp",
            None,
            "pwgraster",
            600,
            "TestPrinter",
            false,
            None,
        );
        assert!(backend.is_err());
    }

    #[test]
    fn test_create_backend_print_proxy() {
        let backend = create_backend(
            "print_proxy",
            None,
            "ppmraw",
            600,
            "TestPrinter",
            false,
            Some("http://127.0.0.1:9632/print"),
        );
        assert!(backend.is_ok());
        assert_eq!(backend.unwrap().name(), "print_proxy");
    }

    #[test]
    fn test_create_backend_print_proxy_requires_url() {
        let backend = create_backend(
            "print_proxy",
            None,
            "ppmraw",
            600,
            "TestPrinter",
            false,
            None,
        );
        assert!(backend.is_err());
        assert!(
            backend
                .err()
                .unwrap()
                .to_string()
                .contains("print_proxy_url")
        );
    }

    #[test]
    fn test_create_backend_unknown_type_errors() {
        let backend = create_backend(
            "laser_beam",
            None,
            "ppmraw",
            600,
            "TestPrinter",
            false,
            None,
        );
        assert!(backend.is_err());
        assert!(backend.err().unwrap().to_string().contains("unknown"));
    }
}
