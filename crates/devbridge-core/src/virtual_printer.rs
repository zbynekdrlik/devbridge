use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualPrinter {
    pub id: String,
    pub display_name: String,
    pub ipp_name: String,
    pub paired_client_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Convert a display name to a URL-safe slug for IPP routing.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_printer_serde_roundtrip() {
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-001".into(),
            display_name: "Store A - Receipt Printer".into(),
            ipp_name: "store-a-receipt".into(),
            paired_client_id: Some("client-abc".into()),
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&vp).unwrap();
        let restored: VirtualPrinter = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.id, "vp-001");
        assert_eq!(restored.display_name, "Store A - Receipt Printer");
        assert_eq!(restored.ipp_name, "store-a-receipt");
        assert_eq!(restored.paired_client_id, Some("client-abc".into()));
    }

    #[test]
    fn test_virtual_printer_no_pairing() {
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-002".into(),
            display_name: "Unpaired".into(),
            ipp_name: "unpaired".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&vp).unwrap();
        let restored: VirtualPrinter = serde_json::from_str(&json).unwrap();

        assert!(restored.paired_client_id.is_none());
    }
}
