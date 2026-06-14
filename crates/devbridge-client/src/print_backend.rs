use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

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
/// [`crate::receiver`] timeout fires (issue #51). Backends MUST check it at
/// every internal decision point — between page sends, at each
/// `poll_job_completion` / verification tick, and around any long-running
/// child process — and bail out promptly (killing child processes, dropping
/// in-flight HTTP connections) so an abandoned print task stops touching the
/// physical printer instead of racing a requeued retry.
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
