use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Pairing state for a client connecting to the server.
///
/// New clients start as `Pending` until an admin approves them.
/// Existing clients (migrated from before pairing was added) default to `Approved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingState {
    Pending,
    Approved,
    Rejected,
}

impl fmt::Display for PairingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairingState::Pending => write!(f, "pending"),
            PairingState::Approved => write!(f, "approved"),
            PairingState::Rejected => write!(f, "rejected"),
        }
    }
}

impl PairingState {
    /// Parse a pairing state from a string (case-insensitive).
    /// Returns `Pending` for unrecognised values.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "approved" => PairingState::Approved,
            "rejected" => PairingState::Rejected,
            _ => PairingState::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub machine_id: String,
    pub hostname: String,
    pub printer_names: Vec<String>,
    pub client_version: String,
    pub last_seen: DateTime<Utc>,
    pub is_online: bool,
    pub pairing_state: PairingState,
    pub virtual_printer_name: Option<String>,
    /// Windows driver override the client asked for its virtual printer
    /// (`[client] virtual_printer_driver`, #88). Applied when the server
    /// auto-creates the virtual printer on approval.
    #[serde(default)]
    pub virtual_printer_driver: Option<String>,
}

impl ClientRegistration {
    /// May this client be handed jobs from the server's DEFAULT (unpaired)
    /// queue? Not when it asked for a vendor-driver virtual printer (#88):
    /// such a client prints RAW to e.g. a label printer, and default-queue
    /// jobs (legacy `/ipp/print`, unpaired VPs) are IPP-Class-Driver/PDF
    /// data that would come out as garbage there. It only ever receives the
    /// jobs of the virtual printer paired to it.
    pub fn takes_default_queue(&self) -> bool {
        self.virtual_printer_driver
            .as_deref()
            .is_none_or(|d| d.trim().is_empty())
    }
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
            pairing_state: PairingState::Approved,
            virtual_printer_name: Some("store-a".into()),
            virtual_printer_driver: Some("TSC ML241P".into()),
        };

        let json = serde_json::to_string(&reg).unwrap();
        let restored: ClientRegistration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.machine_id, "abc123");
        assert_eq!(restored.hostname, "store-a-pc");
        assert_eq!(restored.printer_names.len(), 2);
        assert_eq!(restored.printer_names[0], "EPSON L3270");
        assert!(restored.is_online);
        assert_eq!(restored.pairing_state, PairingState::Approved);
        assert_eq!(restored.virtual_printer_name, Some("store-a".into()));
        assert_eq!(
            restored.virtual_printer_driver.as_deref(),
            Some("TSC ML241P")
        );
    }

    fn reg_with_driver(driver: Option<&str>) -> ClientRegistration {
        ClientRegistration {
            machine_id: "m".into(),
            hostname: "h".into(),
            printer_names: vec![],
            client_version: "0.8.40".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: PairingState::Approved,
            virtual_printer_name: Some("vp".into()),
            virtual_printer_driver: driver.map(String::from),
        }
    }

    #[test]
    fn test_takes_default_queue_only_without_driver_override() {
        // Normal store clients keep serving the default queue …
        assert!(reg_with_driver(None).takes_default_queue());
        assert!(reg_with_driver(Some("")).takes_default_queue());
        assert!(reg_with_driver(Some("  ")).takes_default_queue());
        // … a vendor-driver (RAW label) client never does (#88).
        assert!(!reg_with_driver(Some("TSC ML241P")).takes_default_queue());
    }

    #[test]
    fn test_pairing_state_display() {
        assert_eq!(PairingState::Pending.to_string(), "pending");
        assert_eq!(PairingState::Approved.to_string(), "approved");
        assert_eq!(PairingState::Rejected.to_string(), "rejected");
    }

    #[test]
    fn test_pairing_state_from_str_lossy() {
        assert_eq!(
            PairingState::from_str_lossy("approved"),
            PairingState::Approved
        );
        assert_eq!(
            PairingState::from_str_lossy("APPROVED"),
            PairingState::Approved
        );
        assert_eq!(
            PairingState::from_str_lossy("rejected"),
            PairingState::Rejected
        );
        assert_eq!(
            PairingState::from_str_lossy("pending"),
            PairingState::Pending
        );
        assert_eq!(
            PairingState::from_str_lossy("unknown"),
            PairingState::Pending
        );
    }

    #[test]
    fn test_pairing_state_serde_roundtrip() {
        let json = serde_json::to_string(&PairingState::Pending).unwrap();
        assert_eq!(json, "\"pending\"");
        let restored: PairingState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, PairingState::Pending);
    }
}
