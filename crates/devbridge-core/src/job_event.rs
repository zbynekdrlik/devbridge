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
}

/// A single event in the print job audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrintJobEvent {
    pub job_id: String,
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
}

impl EventEmitter {
    /// Wrap an existing broadcast sender.
    pub fn new(sender: tokio::sync::broadcast::Sender<PrintJobEvent>) -> Self {
        Self {
            sender,
            verification: Arc::new(Mutex::new((String::new(), String::new()))),
        }
    }

    /// Emit an event. Errors are ignored when there are no receivers.
    pub fn emit(&self, event: PrintJobEvent) {
        if event.stage == PrintStage::Verified || !event.verification_method.is_empty() {
            if let Ok(mut v) = self.verification.lock() {
                *v = (
                    event.verification_method.clone(),
                    event.verification_evidence.clone(),
                );
            }
        }
        let _ = self.sender.send(event);
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
}
