//! `[client.pokladna.lan_api]` — the store's local HTTPS API for the
//! Android pokladňa (#101): eKasa receipts while the store's internet (and so
//! Odoo) is down, reported to Odoo once it is back.

use serde::{Deserialize, Serialize};

/// The LAN API's default HTTPS port (the dashboard keeps 9120).
pub const LAN_API_DEFAULT_PORT: u16 = 9443;

/// Longest DNS name (RFC 1035, without the trailing dot).
const MAX_DNS_NAME: usize = 253;
/// Longest DNS label.
const MAX_DNS_LABEL: usize = 63;

/// `[client.pokladna.lan_api]` — an HTTPS listener in the store LAN. The
/// till's Android app sends it `fiscal.*` jobs; they run on the till's Portos
/// through the SAME exactly-once path as the Odoo channel and are reported to
/// Odoo afterwards. Each till authenticates with its own
/// `[[client.pokladna.kasy]] lan_key` (never the Odoo bridge key).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanApiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// TCP port of the HTTPS listener (every interface; the installer opens
    /// it on the Private / Domain firewall profiles only).
    #[serde(default = "default_lan_api_port")]
    pub port: u16,
    /// DNS name the tills use for this PC (`<client_id>.<zone>`) — the name
    /// of the certificate. A till maps it to the PC's LAN IP itself, so no
    /// public A record is needed.
    #[serde(default)]
    pub hostname: String,
    /// `true` (default): pz-server issues the certificate (Let's Encrypt,
    /// ACME DNS-01) for a key that never leaves this PC, renewed 30 days
    /// before it expires. `false`: `lan-api/cert.pem` + `lan-api/key.pem`
    /// in the data dir are provided another way (the E2E test certificate)
    /// and devbridge never renews them.
    #[serde(default = "default_lan_api_acme")]
    pub acme: bool,
}

impl Default for LanApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_lan_api_port(),
            hostname: String::new(),
            acme: default_lan_api_acme(),
        }
    }
}

fn default_lan_api_port() -> u16 {
    LAN_API_DEFAULT_PORT
}

fn default_lan_api_acme() -> bool {
    true
}

/// One DNS label: 1–63 of `[a-z0-9-]`, no hyphen at either end.
pub fn valid_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_DNS_LABEL
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A lower-case DNS name of at least two labels (`pjsnvs.pos.example.sk`),
/// at most 253 characters — what a public certificate can name. No
/// wildcard, no IP address (the last label needs a letter), no trailing dot.
pub fn valid_dns_name(name: &str) -> bool {
    let top_has_letter = name
        .rsplit('.')
        .next()
        .is_some_and(|top| top.bytes().any(|b| b.is_ascii_lowercase()));
    name.len() <= MAX_DNS_NAME
        && name.split('.').count() >= 2
        && name.split('.').all(valid_dns_label)
        && top_has_letter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lan_api_defaults() {
        let d = LanApiConfig::default();
        assert!(!d.enabled);
        assert_eq!(d.port, 9443);
        assert_eq!(LAN_API_DEFAULT_PORT, 9443);
        assert!(d.hostname.is_empty());
        assert!(d.acme, "the certificate comes from pz-server by default");
        // serde defaults equal Default
        let parsed: LanApiConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, d);
        let set: LanApiConfig =
            toml::from_str("enabled = true\nport = 9243\nhostname = \"a.b\"\nacme = false")
                .unwrap();
        assert_eq!(
            set,
            LanApiConfig {
                enabled: true,
                port: 9243,
                hostname: "a.b".into(),
                acme: false
            }
        );
    }

    #[test]
    fn test_valid_dns_label() {
        for ok in ["a", "pjsnvs", "e2e-raw-client", "0", &"a".repeat(63)] {
            assert!(valid_dns_label(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a.b", "*", "á", &"a".repeat(64)] {
            assert!(!valid_dns_label(bad), "{bad}");
        }
    }

    #[test]
    fn test_valid_dns_name() {
        for ok in [
            "pjsnvs.pos.example.sk",
            "a.b",
            "e2e-raw-client.lan-api.test",
            "10.78.2.x10",
        ] {
            assert!(valid_dns_name(ok), "{ok}");
        }
        let long_ok = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(long_ok.len(), 253);
        assert!(valid_dns_name(&long_ok));
        let too_long = format!("{long_ok}e");
        assert!(!valid_dns_name(&too_long));
        for bad in [
            "",
            "localhost",
            "pjsnvs",
            "*.pos.example.sk",
            "a..b",
            ".a.b",
            "a.b.",
            "Pjsnvs.pos.example.sk",
            "10.78.2.10",
            "a.42",
            "a b.c",
        ] {
            assert!(!valid_dns_name(bad), "{bad}");
        }
    }
}
