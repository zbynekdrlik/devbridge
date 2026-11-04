//! `[client.pokladna.tim]` — the Worldline card terminals of the tills,
//! driven through the TIM API C library (#102, lane D1). The library is a
//! licensed Worldline SDK file that devbridge never ships: the service loads
//! it at runtime from `library_path`, so devbridge builds and runs without it.

use serde::{Deserialize, Serialize};

/// The integrator ID Worldline issued for devbridge (not a secret).
pub const TIM_DEFAULT_INTEGRATOR_ID: &str = "7dc442d7-6528-4a05-ba84-5e66558e48b7";

/// Default `commit_timeout_s`: the TIM SDK's own default (`TimApi.cfg`).
pub const TIM_DEFAULT_COMMIT_TIMEOUT_S: u32 = 60;

/// Smallest accepted `commit_timeout_s`: the bridge's own rollback deadline
/// (45 s after the authorisation, lane D2) must come BEFORE the terminal's
/// technical auto-reversal (design odoo-erp issue 7598 §2).
pub const TIM_MIN_COMMIT_TIMEOUT_S: u32 = 50;

/// Largest accepted `commit_timeout_s`.
pub const TIM_MAX_COMMIT_TIMEOUT_S: u32 = 300;

/// Default `rollback_deadline_s` (#102 D2, design §2): Odoo 30 s < the
/// bridge 45 s < the terminal's CommitTimeout.
pub const TIM_DEFAULT_ROLLBACK_DEADLINE_S: u32 = 45;

/// Smallest accepted `rollback_deadline_s`: above Odoo's largest
/// `card_commit_deadline_s` (40 s), so Odoo always decides first.
pub const TIM_MIN_ROLLBACK_DEADLINE_S: u32 = 41;

/// The bridge's deadline ends at least this long before the terminal's.
pub const TIM_ROLLBACK_BEFORE_COMMIT_TIMEOUT_S: u32 = 5;

/// Largest POS-ID: the SDK allows a number of at most six digits (EP2).
pub const TIM_MAX_POS_ID: i64 = 999_999;

/// `[client.pokladna.tim]` — present = the card terminals of
/// `[[client.pokladna.kasy]] terminal_ip` are driven through TIM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimConfig {
    /// Full path of the TIM C library (`timapi.dll`, `libtimapi.so.3…`).
    pub library_path: String,
    /// Integrator ID set on every terminal (`ta_terminal_settings_set_integrator_id`).
    #[serde(default = "default_integrator_id")]
    pub integrator_id: String,
    /// Seconds the terminal waits for a commit before it reverses the
    /// authorisation itself (`ta_terminal_settings_set_commit_timeout`).
    #[serde(default = "default_commit_timeout_s")]
    pub commit_timeout_s: u32,
    /// Seconds after an authorisation without `card.commit` after which the
    /// bridge rolls the payment back itself (lane D2).
    #[serde(default = "default_rollback_deadline_s")]
    pub rollback_deadline_s: u32,
}

impl TimConfig {
    /// Problems with the values (empty = valid) — checked at start-up.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.library_path.trim().is_empty() {
            problems.push("tim.library_path must name the TIM C library file".to_string());
        }
        if !valid_integrator_id(&self.integrator_id) {
            problems.push(format!(
                "tim.integrator_id \"{}\" must be a UUID (8-4-4-4-12 hex digits)",
                self.integrator_id
            ));
        }
        if !(TIM_MIN_COMMIT_TIMEOUT_S..=TIM_MAX_COMMIT_TIMEOUT_S).contains(&self.commit_timeout_s) {
            problems.push(format!(
                "tim.commit_timeout_s = {} must be {TIM_MIN_COMMIT_TIMEOUT_S}..={TIM_MAX_COMMIT_TIMEOUT_S}",
                self.commit_timeout_s
            ));
        }
        let max_deadline = self
            .commit_timeout_s
            .saturating_sub(TIM_ROLLBACK_BEFORE_COMMIT_TIMEOUT_S);
        if !(TIM_MIN_ROLLBACK_DEADLINE_S..=max_deadline).contains(&self.rollback_deadline_s) {
            problems.push(format!(
                "tim.rollback_deadline_s = {} must be {TIM_MIN_ROLLBACK_DEADLINE_S}..={max_deadline} (above Odoo's 40 s, {TIM_ROLLBACK_BEFORE_COMMIT_TIMEOUT_S} s before commit_timeout_s)",
                self.rollback_deadline_s
            ));
        }
        problems
    }
}

/// A UUID in its 8-4-4-4-12 hex form.
pub fn valid_integrator_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A till's `terminal_ip`: an IPv4 address (the terminal listens on TCP 7784).
pub fn valid_terminal_ip(ip: &str) -> bool {
    ip.parse::<std::net::Ipv4Addr>().is_ok()
}

/// A till's `terminal_tid`: 1–16 ASCII letters / digits.
pub fn valid_terminal_tid(tid: &str) -> bool {
    (1..=16).contains(&tid.len()) && tid.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn default_integrator_id() -> String {
    TIM_DEFAULT_INTEGRATOR_ID.to_string()
}

fn default_commit_timeout_s() -> u32 {
    TIM_DEFAULT_COMMIT_TIMEOUT_S
}

fn default_rollback_deadline_s() -> u32 {
    TIM_DEFAULT_ROLLBACK_DEADLINE_S
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tim(path: &str, id: &str, timeout: u32) -> TimConfig {
        TimConfig {
            library_path: path.into(),
            integrator_id: id.into(),
            commit_timeout_s: timeout,
            rollback_deadline_s: 45,
        }
    }

    #[test]
    fn test_defaults_from_toml() {
        let cfg: TimConfig = toml::from_str("library_path = 'C:/TimApi/timapi.dll'").unwrap();
        assert_eq!(cfg.integrator_id, TIM_DEFAULT_INTEGRATOR_ID);
        assert_eq!(cfg.commit_timeout_s, 60);
        assert_eq!(cfg.rollback_deadline_s, 45);
        assert!(cfg.problems().is_empty(), "{:?}", cfg.problems());
        assert!(toml::from_str::<TimConfig>("integrator_id = 'x'").is_err());
    }

    #[test]
    fn test_problems() {
        let ok = TIM_DEFAULT_INTEGRATOR_ID;
        assert!(tim("lib", ok, 50).problems().is_empty());
        assert!(tim("lib", ok, 300).problems().is_empty());
        assert_eq!(
            tim("lib", ok, 49).problems().len(),
            2,
            "also below the deadline"
        );
        assert_eq!(tim("lib", ok, 301).problems().len(), 1);
        assert_eq!(tim(" ", ok, 60).problems().len(), 1);
        let bad = tim("", "nope", 0).problems();
        assert_eq!(bad.len(), 4, "{bad:?}");
        assert!(bad[1].contains("\"nope\""), "{bad:?}");
    }

    #[test]
    fn test_rollback_deadline_range() {
        let ok = TIM_DEFAULT_INTEGRATOR_ID;
        let deadline = |d: u32, timeout: u32| TimConfig {
            rollback_deadline_s: d,
            ..tim("lib", ok, timeout)
        };
        assert!(deadline(41, 60).problems().is_empty());
        assert!(
            deadline(55, 60).problems().is_empty(),
            "5 s before the terminal"
        );
        let late = deadline(56, 60).problems();
        assert_eq!(late.len(), 1, "{late:?}");
        assert!(late[0].contains("41..=55"), "{late:?}");
        assert_eq!(
            deadline(40, 60).problems().len(),
            1,
            "not above Odoo's 40 s"
        );
        assert!(deadline(45, 50).problems().is_empty());
        assert_eq!(deadline(46, 50).problems().len(), 1);
        assert!(deadline(295, 300).problems().is_empty());
    }

    #[test]
    fn test_valid_integrator_id() {
        assert!(valid_integrator_id(TIM_DEFAULT_INTEGRATOR_ID));
        assert!(valid_integrator_id("7DC442D7-6528-4A05-BA84-5E66558E48B7"));
        assert!(!valid_integrator_id("7dc442d7-6528-4a05-ba84-5e66558e48b"));
        assert!(!valid_integrator_id("7dc442d76-528-4a05-ba84-5e66558e48b7"));
        assert!(!valid_integrator_id(
            "7dc442d7-6528-4a05-ba84-5e66558e48b7-0"
        ));
        assert!(!valid_integrator_id("7dc442d7-6528-4a05-ba84-5e66558e48bz"));
        assert!(!valid_integrator_id(""));
    }

    #[test]
    fn test_valid_terminal_ip_and_tid() {
        assert!(valid_terminal_ip("10.78.2.8"));
        assert!(valid_terminal_ip("127.0.0.1"));
        assert!(!valid_terminal_ip("10.78.2.8:7784"));
        assert!(!valid_terminal_ip("terminal.local"));
        assert!(!valid_terminal_ip("::1"));
        assert!(valid_terminal_tid("12345678"));
        assert!(valid_terminal_tid("T0001"));
        assert!(valid_terminal_tid(&"9".repeat(16)));
        assert!(!valid_terminal_tid(&"9".repeat(17)));
        assert!(!valid_terminal_tid(""));
        assert!(!valid_terminal_tid("1234 5678"));
    }
}
