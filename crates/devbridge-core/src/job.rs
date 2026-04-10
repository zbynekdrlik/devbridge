use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Downloading,
    Printing,
    Completed,
    Failed,
    Cancelled,
}

/// Events emitted when job state changes, consumed by WebSocket clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    Created {
        job_id: String,
        document_name: String,
        requesting_user: Option<String>,
        target_printer: String,
    },
    StateChanged {
        job_id: String,
        new_state: JobState,
        requesting_user: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetadata {
    pub job_id: String,
    pub document_name: String,
    pub target_printer: String,
    pub target_client_id: Option<String>,
    pub copies: u32,
    pub paper_size: String,
    pub duplex: bool,
    pub color: bool,
    pub payload_size: u64,
    pub payload_sha256: String,
    pub state: JobState,
    pub retry_count: u32,
    pub error_detail: String,
    pub requesting_user: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_state_roundtrip_serde() {
        let variants = [
            JobState::Queued,
            JobState::Downloading,
            JobState::Printing,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: JobState = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn test_job_metadata_defaults() {
        let now = Utc::now();
        let meta = JobMetadata {
            job_id: "job-001".to_string(),
            document_name: "invoice.pdf".to_string(),
            target_printer: "Office Printer".to_string(),
            target_client_id: Some("client-abc".to_string()),
            copies: 2,
            paper_size: "A4".to_string(),
            duplex: true,
            color: false,
            payload_size: 4096,
            payload_sha256: "deadbeef".to_string(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&meta).unwrap();
        let restored: JobMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.job_id, "job-001");
        assert_eq!(restored.document_name, "invoice.pdf");
        assert_eq!(restored.target_printer, "Office Printer");
        assert_eq!(restored.target_client_id, Some("client-abc".to_string()));
        assert_eq!(restored.copies, 2);
        assert_eq!(restored.paper_size, "A4");
        assert!(restored.duplex);
        assert!(!restored.color);
        assert_eq!(restored.payload_size, 4096);
        assert_eq!(restored.payload_sha256, "deadbeef");
        assert_eq!(restored.state, JobState::Queued);
        assert_eq!(restored.requesting_user, None);
        assert_eq!(restored.created_at, now);
        assert_eq!(restored.updated_at, now);
    }

    #[test]
    fn test_requesting_user_serde_roundtrip() {
        let now = Utc::now();
        let meta = JobMetadata {
            job_id: "job-user-1".to_string(),
            document_name: "report.pdf".to_string(),
            target_printer: "Office Printer".to_string(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".to_string(),
            duplex: false,
            color: true,
            payload_size: 2048,
            payload_sha256: "abc123".to_string(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: Some("alice".to_string()),
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&meta).unwrap();
        let restored: JobMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.requesting_user, Some("alice".to_string()));
    }

    #[test]
    fn test_requesting_user_none_serde_roundtrip() {
        let now = Utc::now();
        let meta = JobMetadata {
            job_id: "job-user-2".to_string(),
            document_name: "report.pdf".to_string(),
            target_printer: "Office Printer".to_string(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".to_string(),
            duplex: false,
            color: true,
            payload_size: 2048,
            payload_sha256: "abc123".to_string(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&meta).unwrap();
        let restored: JobMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.requesting_user, None);
    }

    #[test]
    fn test_job_event_created_with_requesting_user() {
        let event = JobEvent::Created {
            job_id: "job-evt-1".to_string(),
            document_name: "test.pdf".to_string(),
            requesting_user: Some("bob".to_string()),
            target_printer: "pjsnvs printer".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let restored: JobEvent = serde_json::from_str(&json).unwrap();

        match restored {
            JobEvent::Created {
                job_id,
                document_name,
                requesting_user,
                target_printer,
            } => {
                assert_eq!(job_id, "job-evt-1");
                assert_eq!(document_name, "test.pdf");
                assert_eq!(requesting_user, Some("bob".to_string()));
                assert_eq!(target_printer, "pjsnvs printer");
            }
            _ => panic!("Expected JobEvent::Created"),
        }
    }

    #[test]
    fn test_job_event_state_changed_with_requesting_user() {
        let event = JobEvent::StateChanged {
            job_id: "job-evt-2".to_string(),
            new_state: JobState::Completed,
            requesting_user: Some("alice".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: JobEvent = serde_json::from_str(&json).unwrap();
        match restored {
            JobEvent::StateChanged {
                job_id,
                new_state,
                requesting_user,
            } => {
                assert_eq!(job_id, "job-evt-2");
                assert_eq!(new_state, JobState::Completed);
                assert_eq!(requesting_user, Some("alice".to_string()));
            }
            _ => panic!("Expected JobEvent::StateChanged"),
        }
    }
}
