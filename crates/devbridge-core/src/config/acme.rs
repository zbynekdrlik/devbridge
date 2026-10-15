//! `[server.acme]` — pz-server issues the stores' LAN API certificates
//! (#101): a public Let's Encrypt certificate per store for
//! `<client_id>.<zone>`, ACME DNS-01 through Cloudflare. The store PC sends
//! only a CSR (its private key never leaves it); the DNS token lives on
//! pz-server alone.

use serde::{Deserialize, Serialize};

use super::lan_api::valid_dns_name;
use super::redact_secret;

/// Let's Encrypt's production directory (the default).
pub const LETS_ENCRYPT_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Cloudflare API v4 base URL (the default).
pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
/// Environment variable that overrides `[server.acme.cloudflare] api_token`
/// (the installer keeps the token out of config.toml this way).
pub const CLOUDFLARE_TOKEN_ENV: &str = "DEVBRIDGE_CLOUDFLARE_API_TOKEN";

/// `[server.acme]`. Absent / disabled = pz-server answers every
/// certificate request with `acme_disabled` (nothing is issued).
///
/// `Debug` is hand-written so the Cloudflare token never reaches a log line.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct AcmeServerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// ACME directory URL (Let's Encrypt production by default).
    #[serde(default = "default_directory_url")]
    pub directory_url: String,
    /// The certificates are `<client_id>.<zone>`, e.g. zone `pos.example.sk`
    /// → `pjsnvs.pos.example.sk`. Chosen by the owner.
    #[serde(default)]
    pub zone: String,
    /// ACME account contact e-mail (optional, `mailto:` is added).
    #[serde(default)]
    pub contact_email: String,
    /// PEM root of a private / test ACME CA to trust (Pebble in CI); empty =
    /// the system's roots.
    #[serde(default)]
    pub ca_file: String,
    /// Seconds between creating the `_acme-challenge` TXT record and asking
    /// the CA to validate it (DNS propagation).
    #[serde(default = "default_propagation_secs")]
    pub propagation_secs: u64,
    #[serde(default)]
    pub cloudflare: CloudflareConfig,
}

/// `[server.acme.cloudflare]` — the DNS provider of the zone.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudflareConfig {
    /// Cloudflare zone id of the DNS zone that holds `zone`.
    #[serde(default)]
    pub zone_id: String,
    /// API token with `Zone.DNS: Edit` on that zone. Secret — never logged;
    /// [`CLOUDFLARE_TOKEN_ENV`] overrides it.
    #[serde(default)]
    pub api_token: String,
    /// API base URL (a fake one in tests).
    #[serde(default = "default_cloudflare_api")]
    pub api_base: String,
}

impl Default for AcmeServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory_url: default_directory_url(),
            zone: String::new(),
            contact_email: String::new(),
            ca_file: String::new(),
            propagation_secs: default_propagation_secs(),
            cloudflare: CloudflareConfig::default(),
        }
    }
}

impl Default for CloudflareConfig {
    fn default() -> Self {
        Self {
            zone_id: String::new(),
            api_token: String::new(),
            api_base: default_cloudflare_api(),
        }
    }
}

impl std::fmt::Debug for AcmeServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeServerConfig")
            .field("enabled", &self.enabled)
            .field("directory_url", &self.directory_url)
            .field("zone", &self.zone)
            .field("contact_email", &self.contact_email)
            .field("ca_file", &self.ca_file)
            .field("propagation_secs", &self.propagation_secs)
            .field("cloudflare", &self.cloudflare)
            .finish()
    }
}

impl std::fmt::Debug for CloudflareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareConfig")
            .field("zone_id", &self.zone_id)
            .field("api_token", &redact_secret(&self.api_token))
            .field("api_base", &self.api_base)
            .finish()
    }
}

fn default_directory_url() -> String {
    LETS_ENCRYPT_DIRECTORY.to_string()
}

fn default_cloudflare_api() -> String {
    CLOUDFLARE_API.to_string()
}

fn default_propagation_secs() -> u64 {
    30
}

/// `https://…` without spaces or quotes.
fn https_url(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() > "https://".len()
        && !url.contains(|c: char| c.is_whitespace() || c == '"')
}

impl AcmeServerConfig {
    /// The Cloudflare token: `env_token` (the value of
    /// [`CLOUDFLARE_TOKEN_ENV`], if set and not blank) wins over the config.
    pub fn api_token(&self, env_token: Option<&str>) -> String {
        env_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(self.cloudflare.api_token.trim())
            .to_string()
    }

    /// Everything wrong with an ENABLED block (empty = usable). `token` is
    /// the effective Cloudflare token ([`Self::api_token`]); its value never
    /// appears in a problem.
    pub fn problems(&self, token: &str) -> Vec<String> {
        let mut problems = Vec::new();
        if !https_url(&self.directory_url) {
            problems.push(format!(
                "directory_url \"{}\" must be an https:// URL",
                self.directory_url
            ));
        }
        if !valid_dns_name(&self.zone) {
            problems.push(format!(
                "zone \"{}\" must be a lower-case DNS name such as pos.example.sk",
                self.zone
            ));
        }
        if self
            .contact_email
            .contains(|c: char| c.is_whitespace() || c == '"')
        {
            problems.push("contact_email must not contain spaces or quotes".to_string());
        }
        if self.cloudflare.zone_id.trim().is_empty() {
            problems.push("cloudflare.zone_id is empty".to_string());
        }
        if token.is_empty() {
            problems.push(format!(
                "no Cloudflare API token (cloudflare.api_token or {CLOUDFLARE_TOKEN_ENV})"
            ));
        }
        if !https_url(&self.cloudflare.api_base)
            && !self.cloudflare.api_base.starts_with("http://127.0.0.1:")
        {
            problems.push(format!(
                "cloudflare.api_base \"{}\" must be an https:// URL",
                self.cloudflare.api_base
            ));
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> AcmeServerConfig {
        AcmeServerConfig {
            enabled: true,
            zone: "pos.example.sk".into(),
            cloudflare: CloudflareConfig {
                zone_id: "zone123".into(),
                api_token: "cf-s3cr3t".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_acme_defaults_and_toml() {
        let d = AcmeServerConfig::default();
        assert!(!d.enabled);
        assert_eq!(d.directory_url, LETS_ENCRYPT_DIRECTORY);
        assert_eq!(
            LETS_ENCRYPT_DIRECTORY,
            "https://acme-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(
            d.cloudflare.api_base,
            "https://api.cloudflare.com/client/v4"
        );
        assert_eq!(d.propagation_secs, 30);
        assert!(d.zone.is_empty() && d.ca_file.is_empty() && d.contact_email.is_empty());
        let parsed: AcmeServerConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, d);
        let set: AcmeServerConfig = toml::from_str(
            "enabled = true\nzone = \"pos.example.sk\"\npropagation_secs = 0\n\
             [cloudflare]\nzone_id = \"z\"\napi_token = \"t\"",
        )
        .unwrap();
        assert!(set.enabled);
        assert_eq!(set.zone, "pos.example.sk");
        assert_eq!(set.propagation_secs, 0);
        assert_eq!(set.cloudflare.zone_id, "z");
        assert_eq!(set.cloudflare.api_token, "t");
        assert_eq!(set.cloudflare.api_base, CLOUDFLARE_API);
    }

    #[test]
    fn test_debug_never_prints_the_token() {
        let dbg = format!("{:?}", enabled());
        assert!(!dbg.contains("cf-s3cr3t"), "{dbg}");
        assert!(dbg.contains("api_token: \"<redacted>\""), "{dbg}");
        assert!(dbg.contains("zone: \"pos.example.sk\""), "{dbg}");
        assert!(dbg.contains("zone_id: \"zone123\""), "{dbg}");
        assert!(dbg.contains("propagation_secs: 30"), "{dbg}");
        assert!(dbg.contains("directory_url: \"https://acme-v02"), "{dbg}");
        assert!(dbg.contains("contact_email: \"\""), "{dbg}");
        assert!(dbg.contains("ca_file: \"\""), "{dbg}");
        assert!(dbg.contains("enabled: true"), "{dbg}");
        assert!(
            dbg.contains("api_base: \"https://api.cloudflare.com/client/v4\""),
            "{dbg}"
        );
    }

    #[test]
    fn test_api_token_env_wins_when_set() {
        let cfg = enabled();
        assert_eq!(cfg.api_token(None), "cf-s3cr3t");
        assert_eq!(cfg.api_token(Some(" env-token ")), "env-token");
        assert_eq!(
            cfg.api_token(Some("  ")),
            "cf-s3cr3t",
            "a blank env var is ignored"
        );
        let mut none = enabled();
        none.cloudflare.api_token = " ".into();
        assert_eq!(none.api_token(None), "");
        assert_eq!(CLOUDFLARE_TOKEN_ENV, "DEVBRIDGE_CLOUDFLARE_API_TOKEN");
    }

    #[test]
    fn test_problems() {
        let cfg = enabled();
        assert!(cfg.problems("t").is_empty(), "{:?}", cfg.problems("t"));
        let mut local = enabled();
        local.cloudflare.api_base = "http://127.0.0.1:8080/client/v4".into();
        assert!(
            local.problems("t").is_empty(),
            "a local fake API is allowed"
        );

        let bad = AcmeServerConfig {
            enabled: true,
            directory_url: "http://acme.test/dir".into(),
            zone: "Pos.Example".into(),
            contact_email: "a b@c".into(),
            cloudflare: CloudflareConfig {
                zone_id: " ".into(),
                api_token: String::new(),
                api_base: "ftp://x".into(),
            },
            ..Default::default()
        };
        let problems = bad.problems("").join("; ");
        for needle in [
            "directory_url \"http://acme.test/dir\" must be an https:// URL",
            "zone \"Pos.Example\" must be a lower-case DNS name",
            "contact_email must not contain spaces or quotes",
            "cloudflare.zone_id is empty",
            "no Cloudflare API token (cloudflare.api_token or DEVBRIDGE_CLOUDFLARE_API_TOKEN)",
            "cloudflare.api_base \"ftp://x\" must be an https:// URL",
        ] {
            assert!(problems.contains(needle), "missing {needle}: {problems}");
        }
        assert_eq!(bad.problems("").len(), 6);
    }

    #[test]
    fn test_https_url() {
        assert!(https_url("https://a"));
        assert!(!https_url("https://"));
        assert!(!https_url("http://a"));
        assert!(!https_url("https://a b"));
        assert!(!https_url("https://a\"b"));
    }
}
