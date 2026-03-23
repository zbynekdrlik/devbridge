use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub machine_id: String,
    pub hostname: String,
    pub printer_names: Vec<String>,
    pub client_version: String,
    pub last_seen: DateTime<Utc>,
    pub is_online: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_registration_serde_roundtrip() {
        let now = Utc::now();
        let reg = ClientRegistration {
            machine_id: "abc123".into(),
            hostname: "store-a-pc".into(),
            printer_names: vec!["EPSON L3270".into(), "Canon MG3600".into()],
            client_version: "0.1.0".into(),
            last_seen: now,
            is_online: true,
        };

        let json = serde_json::to_string(&reg).unwrap();
        let restored: ClientRegistration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.machine_id, "abc123");
        assert_eq!(restored.hostname, "store-a-pc");
        assert_eq!(restored.printer_names.len(), 2);
        assert_eq!(restored.printer_names[0], "EPSON L3270");
        assert!(restored.is_online);
    }
}
