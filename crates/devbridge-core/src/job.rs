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
        assert_eq!(restored.created_at, now);
        assert_eq!(restored.updated_at, now);
    }
}
