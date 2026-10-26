use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stages in the print pipeline audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrintStage {
    Received,
    Routed,
    Downloading,
    Downloaded,
    Rendering,
    Rendered,
    Sending,
    Sent,
    Verified,
    Acknowledged,
    Completed,
    Failed,
    Retrying,
    /// #114: the printer accepted the job but devbridge never learned its
    /// end — not a failure, never resent.
    Unconfirmed,
}

/// A single event in the print job audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrintJobEvent {
    pub job_id: String,
    #[serde(default)]
    pub requesting_user: Option<String>,
    pub stage: PrintStage,
    pub success: bool,
    pub detail: String,
    #[serde(default)]
    pub verification_method: String,
    #[serde(default)]
    pub verification_evidence: String,
    pub timestamp: DateTime<Utc>,
}

impl PrintJobEvent {
    /// Create a new event with timestamp set to now.
    pub fn new(
        job_id: impl Into<String>,
        stage: PrintStage,
        success: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            requesting_user: None,
            stage,
            success,
            detail: detail.into(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            timestamp: Utc::now(),
        }
    }

    /// Shorthand for a successful event.
    pub fn ok(job_id: impl Into<String>, stage: PrintStage, detail: impl Into<String>) -> Self {
        Self::new(job_id, stage, true, detail)
    }

    /// Shorthand for a failed event.
    pub fn fail(job_id: impl Into<String>, stage: PrintStage, detail: impl Into<String>) -> Self {
        Self::new(job_id, stage, false, detail)
    }

    /// Create a verified event with method and evidence.
    pub fn verified(
        job_id: impl Into<String>,
        method: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        let evidence_str = evidence.into();
        Self {
            job_id: job_id.into(),
            requesting_user: None,
            stage: PrintStage::Verified,
            success: true,
            detail: evidence_str.clone(),
            verification_method: method.into(),
            verification_evidence: evidence_str,
            timestamp: Utc::now(),
        }
    }
}

/// Wraps a broadcast sender so backends can emit events without knowing the consumer.
#[derive(Clone)]
pub struct EventEmitter {
    sender: tokio::sync::broadcast::Sender<PrintJobEvent>,
    verification: Arc<Mutex<(String, String)>>,
    /// #114: a printer accepted (part of) this job, see [`Self::printer_accepted`].
    printer_accepted: Arc<AtomicBool>,
}

impl EventEmitter {
    /// Wrap an existing broadcast sender.
    pub fn new(sender: tokio::sync::broadcast::Sender<PrintJobEvent>) -> Self {
        Self {
            sender,
            verification: Arc::new(Mutex::new((String::new(), String::new()))),
            printer_accepted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Emit an event. Errors are ignored when there are no receivers.
    pub fn emit(&self, event: PrintJobEvent) {
        if event.stage == PrintStage::Acknowledged && event.success {
            self.printer_accepted.store(true, Ordering::SeqCst);
        }
        if (event.stage == PrintStage::Verified || !event.verification_method.is_empty())
            && let Ok(mut v) = self.verification.lock()
        {
            *v = (
                event.verification_method.clone(),
                event.verification_evidence.clone(),
            );
        }
        let _ = self.sender.send(event);
    }

    /// `true` once a printer ACCEPTED this job (#114): a successful
    /// `acknowledged` event — direct_ipp emits it with the printer's job-id.
    /// From then on a lost outcome is "unconfirmed" (never resent), not
    /// "nothing printed" (retried); every clone of the job's emitter sees it.
    pub fn printer_accepted(&self) -> bool {
        self.printer_accepted.load(Ordering::SeqCst)
    }

    /// Return the last verification (method, evidence) emitted.
    pub fn last_verification(&self) -> (String, String) {
        self.verification
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Emit a successful event.
    pub fn emit_ok(&self, job_id: impl Into<String>, stage: PrintStage, detail: impl Into<String>) {
        self.emit(PrintJobEvent::ok(job_id, stage, detail));
    }

    /// Emit a failed event.
    pub fn emit_fail(
        &self,
        job_id: impl Into<String>,
        stage: PrintStage,
        detail: impl Into<String>,
    ) {
        self.emit(PrintJobEvent::fail(job_id, stage, detail));
    }

    /// Emit a verified event with method and evidence.
    pub fn emit_verified(
        &self,
        job_id: impl Into<String>,
        method: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        self.emit(PrintJobEvent::verified(job_id, method, evidence));
    }

    /// Subscribe to receive future events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PrintJobEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_print_stage_serde_roundtrip() {
        let stages = [
            PrintStage::Received,
            PrintStage::Routed,
            PrintStage::Downloading,
            PrintStage::Downloaded,
            PrintStage::Rendering,
            PrintStage::Rendered,
            PrintStage::Sending,
            PrintStage::Sent,
            PrintStage::Verified,
            PrintStage::Acknowledged,
            PrintStage::Completed,
            PrintStage::Failed,
            PrintStage::Retrying,
        ];

        for stage in stages {
            let json = serde_json::to_string(&stage).expect("serialize stage");
            let roundtripped: PrintStage = serde_json::from_str(&json).expect("deserialize stage");
            assert_eq!(stage, roundtripped, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn test_print_job_event_ok_constructor() {
        let before = Utc::now();
        let event = PrintJobEvent::ok("job-123", PrintStage::Received, "IPP job received");
        let after = Utc::now();

        assert_eq!(event.job_id, "job-123");
        assert_eq!(event.stage, PrintStage::Received);
        assert!(event.success);
        assert_eq!(event.detail, "IPP job received");
        assert!(event.timestamp >= before && event.timestamp <= after);
    }

    #[test]
    fn test_print_job_event_fail_constructor() {
        let event = PrintJobEvent::fail("job-456", PrintStage::Sending, "connection refused");

        assert_eq!(event.job_id, "job-456");
        assert_eq!(event.stage, PrintStage::Sending);
        assert!(!event.success);
        assert_eq!(event.detail, "connection refused");
    }

    #[tokio::test]
    async fn test_event_emitter_send_receive() {
        let (tx, _rx) = tokio::sync::broadcast::channel::<PrintJobEvent>(16);
        let emitter = EventEmitter::new(tx);
        let mut subscriber = emitter.subscribe();

        emitter.emit_ok("job-789", PrintStage::Completed, "printed successfully");

        let received = subscriber
            .recv()
            .await
            .expect("should receive the emitted event");

        assert_eq!(received.job_id, "job-789");
        assert_eq!(received.stage, PrintStage::Completed);
        assert!(received.success);
        assert_eq!(received.detail, "printed successfully");
    }

    #[test]
    fn test_verified_stage_serde_roundtrip() {
        let json = serde_json::to_string(&PrintStage::Verified).expect("serialize verified");
        assert_eq!(json, "\"verified\"");
        let roundtripped: PrintStage = serde_json::from_str(&json).expect("deserialize verified");
        assert_eq!(roundtripped, PrintStage::Verified);
    }

    #[test]
    fn test_print_job_event_with_verification_fields() {
        let event = PrintJobEvent {
            job_id: "job-v1".into(),
            requesting_user: None,
            stage: PrintStage::Verified,
            success: true,
            detail: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: PrintJobEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.verification_method, "eventid_307");
        assert_eq!(
            restored.verification_evidence,
            "EventID 307: Document 42, eholla printer, USB002, 245KB"
        );
        assert_eq!(restored.stage, PrintStage::Verified);
    }

    #[test]
    fn test_print_job_event_default_verification_empty() {
        let event = PrintJobEvent::ok("job-old", PrintStage::Completed, "done");
        assert_eq!(event.verification_method, "");
        assert_eq!(event.verification_evidence, "");
    }

    #[tokio::test]
    async fn test_event_emitter_tracks_last_verification() {
        let (tx, _rx) = tokio::sync::broadcast::channel::<PrintJobEvent>(16);
        let emitter = EventEmitter::new(tx);

        // No verification yet
        let (method, evidence) = emitter.last_verification();
        assert_eq!(method, "");
        assert_eq!(evidence, "");

        // Emit a verified event
        emitter.emit_verified("job-1", "eventid_307", "EventID 307: Document 42");

        let (method, evidence) = emitter.last_verification();
        assert_eq!(method, "eventid_307");
        assert_eq!(evidence, "EventID 307: Document 42");
    }

    /// #114: the stage of a job the printer accepted but never confirmed.
    #[test]
    fn test_unconfirmed_stage_is_named_unconfirmed() {
        let json = serde_json::to_string(&PrintStage::Unconfirmed).unwrap();
        assert_eq!(json, "\"unconfirmed\"");
        let back: PrintStage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, PrintStage::Unconfirmed);
    }

    /// #114: once the printer ACCEPTED the job (a successful `acknowledged`
    /// event — direct_ipp emits it with the printer's job-id), the receiver
    /// must not report an outer timeout as "nothing printed": the job's
    /// emitter remembers it, shared by every clone.
    #[test]
    fn test_printer_accepted_is_set_only_by_a_successful_acknowledged_event() {
        let (tx, _rx) = tokio::sync::broadcast::channel::<PrintJobEvent>(16);
        let emitter = EventEmitter::new(tx);
        let backend_side = emitter.clone();
        assert!(!emitter.printer_accepted(), "nothing sent yet");
        backend_side.emit_ok("job-1", PrintStage::Sending, "IPP Print-Job");
        backend_side.emit_ok("job-1", PrintStage::Completed, "done");
        backend_side.emit_fail("job-1", PrintStage::Acknowledged, "refused");
        assert!(!emitter.printer_accepted(), "a refusal is no acceptance");
        backend_side.emit_ok(
            "job-1",
            PrintStage::Acknowledged,
            "HP LaserJet M110w accepted job-id=45, processing",
        );
        assert!(emitter.printer_accepted(), "seen through the clone");
        backend_side.emit_fail("job-1", PrintStage::Failed, "poll failed");
        assert!(emitter.printer_accepted(), "stays set for the job");
    }
}
