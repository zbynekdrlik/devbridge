//! Integration test: verify `SimpleIppJobAttributes` exposes the new
//! `document_name` and `job_name` fields and that values flow unchanged
//! through a `SimpleIppServiceHandler::handle_document` call.
//!
//! This test pins the fork surface area so that if `[patch.crates-io]` is
//! ever removed or pointed at a stock ippper 0.4.0 release, compilation
//! fails loudly instead of silently regressing #30 at runtime.
//!
//! End-to-end verification (print job hitting the real server and surfacing
//! on the dashboard) lives in `playwright/tests/document-name.spec.ts` and
//! `crates/devbridge-e2e/src/main.rs::test_job_metadata_correct`.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use ipp::payload::IppPayload;
use ippper::service::simple::{SimpleIppDocument, SimpleIppJobAttributes, SimpleIppServiceHandler};

/// Capturing handler that records the `SimpleIppJobAttributes` it receives.
struct CapturingHandler {
    captured: Arc<Mutex<Option<SimpleIppJobAttributes>>>,
}

impl SimpleIppServiceHandler for CapturingHandler {
    async fn handle_document(&self, document: SimpleIppDocument) -> anyhow::Result<()> {
        let mut guard = self.captured.lock().unwrap();
        *guard = Some(document.job_attributes);
        Ok(())
    }
}

fn make_attrs(document_name: Option<&str>, job_name: Option<&str>) -> SimpleIppJobAttributes {
    SimpleIppJobAttributes {
        originating_user_name: "integration-test".into(),
        document_name: document_name.map(String::from),
        job_name: job_name.map(String::from),
        media: "iso_a4_210x297mm".into(),
        orientation: None,
        sides: "one-sided".into(),
        print_color_mode: "monochrome".into(),
        printer_resolution: None,
    }
}

fn empty_payload() -> IppPayload {
    IppPayload::new(Cursor::new(Vec::<u8>::new()))
}

#[tokio::test]
async fn handler_receives_document_name_and_job_name() {
    let captured: Arc<Mutex<Option<SimpleIppJobAttributes>>> = Arc::new(Mutex::new(None));
    let handler = CapturingHandler {
        captured: captured.clone(),
    };

    let attrs = make_attrs(Some("Integration-Test.pdf"), Some("job-label"));
    let document = SimpleIppDocument {
        format: Some("application/pdf".into()),
        job_attributes: attrs,
        payload: empty_payload(),
    };

    handler.handle_document(document).await.unwrap();

    let captured = captured.lock().unwrap();
    let got = captured
        .as_ref()
        .expect("handler should have captured attrs");
    assert_eq!(got.document_name.as_deref(), Some("Integration-Test.pdf"));
    assert_eq!(got.job_name.as_deref(), Some("job-label"));
}

#[tokio::test]
async fn handler_receives_none_when_both_missing() {
    let captured: Arc<Mutex<Option<SimpleIppJobAttributes>>> = Arc::new(Mutex::new(None));
    let handler = CapturingHandler {
        captured: captured.clone(),
    };

    let attrs = make_attrs(None, None);
    let document = SimpleIppDocument {
        format: Some("application/pdf".into()),
        job_attributes: attrs,
        payload: empty_payload(),
    };

    handler.handle_document(document).await.unwrap();

    let captured = captured.lock().unwrap();
    let got = captured
        .as_ref()
        .expect("handler should have captured attrs");
    assert_eq!(got.document_name, None);
    assert_eq!(got.job_name, None);
}
