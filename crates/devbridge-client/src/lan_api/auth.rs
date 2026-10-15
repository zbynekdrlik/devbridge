//! LAN API authentication (#101): every till has its own Bearer key
//! (`[[client.pokladna.kasy]] lan_key`), valid for THAT till only. The Odoo
//! bridge key is never accepted here, even if a till were given it.
//!
//! Keys are compared as SHA-256 digests in constant time, and every key is
//! compared on every call (no early exit), so neither the timing nor the
//! length of a guess tells how close it was.

use sha2::{Digest, Sha256};

use devbridge_core::config::PokladnaKasaConfig;

type KeyDigest = [u8; 32];

fn digest(key: &str) -> KeyDigest {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(key.as_bytes()));
    out
}

/// Constant-time equality of two digests.
pub fn ct_eq(a: &KeyDigest, b: &KeyDigest) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The token of an `Authorization: Bearer <token>` header (the scheme is
/// case-insensitive, RFC 6750); `None` for anything else or an empty token.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let header = header?;
    let (scheme, token) = header.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// The tills' LAN keys (digests only — the keys themselves are not kept).
pub struct KeyTable {
    keys: Vec<(i64, KeyDigest)>,
    bridge: Option<KeyDigest>,
}

impl std::fmt::Debug for KeyTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tills: Vec<i64> = self.keys.iter().map(|(id, _)| *id).collect();
        f.debug_struct("KeyTable").field("tills", &tills).finish()
    }
}

impl KeyTable {
    /// The keys of `kasy` (a till without `lan_key` cannot use the LAN API);
    /// `bridge_key` is the Odoo bridge key, refused even when a till has it.
    pub fn new(kasy: &[PokladnaKasaConfig], bridge_key: &str) -> Self {
        let keys = kasy
            .iter()
            .filter_map(|k| {
                let key = k.lan_key.as_deref()?.trim();
                (!key.is_empty()).then(|| (k.id, digest(key)))
            })
            .collect();
        let bridge_key = bridge_key.trim();
        Self {
            keys,
            bridge: (!bridge_key.is_empty()).then(|| digest(bridge_key)),
        }
    }

    /// Tills that have a LAN key.
    pub fn tills(&self) -> Vec<i64> {
        self.keys.iter().map(|(id, _)| *id).collect()
    }

    /// The till `token` belongs to — `None` for an unknown key and for the
    /// Odoo bridge key. Every key is compared.
    pub fn till_of(&self, token: Option<&str>) -> Option<i64> {
        let presented = digest(token?);
        let mut till = None;
        for (id, key) in &self.keys {
            if ct_eq(key, &presented) {
                till = Some(*id);
            }
        }
        let is_bridge = self.bridge.as_ref().is_some_and(|b| ct_eq(b, &presented));
        till.filter(|_| !is_bridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kasa(id: i64, key: Option<&str>) -> PokladnaKasaConfig {
        PokladnaKasaConfig {
            id,
            portos_url: None,
            terminal_ip: None,
            terminal_tid: None,
            lan_key: key.map(str::to_string),
        }
    }

    const K3: &str = "till-3-lan-key-0123456789abcdefABCDEF";
    const K4: &str = "till-4-lan-key-0123456789abcdefABCDEF";
    const BRIDGE: &str = "odoo-bridge-key-0123456789abcdefABCDEF";

    #[test]
    fn test_ct_eq() {
        let a = digest("a");
        assert!(ct_eq(&a, &digest("a")));
        assert!(!ct_eq(&a, &digest("b")));
        // two differences that would cancel out under XOR-accumulation
        let mut x = [0u8; 32];
        let mut y = [0u8; 32];
        x[0] = 1;
        x[1] = 1;
        assert!(!ct_eq(&x, &y));
        y[0] = 1;
        y[1] = 1;
        assert!(ct_eq(&x, &y), "equal non-zero digests");
        y[31] = 0x80;
        assert!(!ct_eq(&x, &y), "the last byte counts too");
    }

    #[test]
    fn test_bearer_token() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer abc ")), Some("abc"));
        assert_eq!(bearer_token(Some("BEARER  abc")), Some("abc"));
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(Some("Bearerabc")), None);
        assert_eq!(bearer_token(Some("")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn test_key_belongs_to_its_till_only() {
        let t = KeyTable::new(
            &[
                kasa(3, Some(K3)),
                kasa(4, Some(K4)),
                kasa(5, None),
                kasa(6, Some(" ")),
            ],
            BRIDGE,
        );
        assert_eq!(t.tills(), vec![3, 4], "a till without a key cannot call");
        assert_eq!(t.till_of(Some(K3)), Some(3));
        assert_eq!(t.till_of(Some(K4)), Some(4));
        assert_eq!(t.till_of(Some("wrong")), None);
        assert_eq!(t.till_of(Some("")), None);
        assert_eq!(t.till_of(Some(" ")), None, "the blank key of till 6");
        assert_eq!(t.till_of(None), None);
        assert_eq!(
            t.till_of(Some(BRIDGE)),
            None,
            "the Odoo bridge key is refused"
        );
        let dbg = format!("{t:?}");
        assert_eq!(dbg, "KeyTable { tills: [3, 4] }");
    }

    #[test]
    fn test_bridge_key_is_refused_even_as_a_till_key() {
        let t = KeyTable::new(&[kasa(3, Some(BRIDGE))], BRIDGE);
        assert_eq!(t.till_of(Some(BRIDGE)), None);
        let no_bridge = KeyTable::new(&[kasa(3, Some(K3))], " ");
        assert_eq!(no_bridge.till_of(Some(K3)), Some(3));
        assert_eq!(no_bridge.till_of(Some(" ")), None);
    }
}
