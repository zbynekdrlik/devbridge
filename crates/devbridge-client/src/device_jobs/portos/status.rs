//! The Portos part of the heartbeat's `device_status` (#100): `portos=ok`,
//! `portos=degraded(printer=Error,paper=Empty)`, `portos=offline` or
//! `portos=unknown` (not probed yet). With several Portos (one per till) the
//! WORST state is reported; the state of every one is logged on a change.

use std::collections::BTreeMap;

use serde_json::Value;

use super::http::Reply;

/// What a probe of one Portos found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    /// `product/info` answered and the printer is ready.
    Ok,
    /// Not probed yet.
    Unknown,
    /// Portos answers but the printer is not ready (details).
    Degraded(String),
    /// `product/info` did not answer.
    Offline,
}

impl DeviceState {
    /// Higher = worse.
    fn severity(&self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Unknown => 1,
            Self::Degraded(_) => 2,
            Self::Offline => 3,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Ok => "ok".to_string(),
            Self::Unknown => "unknown".to_string(),
            Self::Degraded(detail) => format!("degraded({detail})"),
            Self::Offline => "offline".to_string(),
        }
    }
}

/// The worst state of all Portos (`unknown` when there are none).
pub fn worst(states: &BTreeMap<String, DeviceState>) -> String {
    states
        .values()
        .max_by_key(|s| s.severity())
        .map(DeviceState::label)
        .unwrap_or_else(|| DeviceState::Unknown.label())
}

/// One probe: `GET /api/v1/product/info` and `GET /api/v1/printers/status`.
/// A printer is fine when it is `Ready` and its paper is not `Empty` /
/// `CoverOpen` (`NearEnd` still prints).
pub fn probe_state(info: &Reply, printer: &Reply) -> DeviceState {
    let info_ok = matches!(info, Reply::Answer { status: 200, body } if body.is_object());
    if !info_ok {
        return DeviceState::Offline;
    }
    let Reply::Answer { status: 200, body } = printer else {
        return DeviceState::Degraded("printer status unreadable".to_string());
    };
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let (state, paper) = (text("state"), text("paperState"));
    let printer_ready = state == "Ready" && !matches!(paper.as_str(), "Empty" | "CoverOpen");
    if printer_ready {
        DeviceState::Ok
    } else {
        DeviceState::Degraded(format!("printer={state},paper={paper}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(body: Value) -> Reply {
        Reply::Answer { status: 200, body }
    }

    fn info() -> Reply {
        ok(
            json!({"vendorName": "Nine Digit, s.r.o.", "api": {"name": "Portos eKasa", "version": "7.0.4"}}),
        )
    }

    #[test]
    fn test_labels_and_worst() {
        assert_eq!(DeviceState::Ok.label(), "ok");
        assert_eq!(DeviceState::Unknown.label(), "unknown");
        assert_eq!(DeviceState::Offline.label(), "offline");
        assert_eq!(
            DeviceState::Degraded("printer=Error,paper=Empty".into()).label(),
            "degraded(printer=Error,paper=Empty)"
        );
        let mut m = BTreeMap::new();
        assert_eq!(worst(&m), "unknown");
        m.insert("a".to_string(), DeviceState::Ok);
        assert_eq!(worst(&m), "ok");
        m.insert("b".to_string(), DeviceState::Unknown);
        assert_eq!(worst(&m), "unknown");
        m.insert(
            "c".to_string(),
            DeviceState::Degraded("printer=Error,paper=Ready".into()),
        );
        assert_eq!(worst(&m), "degraded(printer=Error,paper=Ready)");
        m.insert("d".to_string(), DeviceState::Offline);
        assert_eq!(worst(&m), "offline");
        m.remove("d");
        m.remove("c");
        m.insert("b".to_string(), DeviceState::Ok);
        assert_eq!(worst(&m), "ok");
        // the worst wins wherever it sits in the map
        let pair = |a: DeviceState, b: DeviceState| {
            BTreeMap::from([("a".to_string(), a), ("b".to_string(), b)])
        };
        assert_eq!(
            worst(&pair(DeviceState::Offline, DeviceState::Ok)),
            "offline"
        );
        assert_eq!(
            worst(&pair(
                DeviceState::Offline,
                DeviceState::Degraded("x".into())
            )),
            "offline"
        );
        assert_eq!(
            worst(&pair(
                DeviceState::Degraded("x".into()),
                DeviceState::Unknown
            )),
            "degraded(x)"
        );
        assert_eq!(
            worst(&pair(DeviceState::Unknown, DeviceState::Ok)),
            "unknown"
        );
    }

    #[test]
    fn test_probe_state() {
        let printer = |state: &str, paper: &str| ok(json!({"state": state, "paperState": paper}));
        assert_eq!(
            probe_state(&info(), &printer("Ready", "Ready")),
            DeviceState::Ok
        );
        assert_eq!(
            probe_state(&info(), &printer("Ready", "NearEnd")),
            DeviceState::Ok
        );
        assert_eq!(
            probe_state(&info(), &ok(json!({"state": "Ready"}))),
            DeviceState::Ok,
            "no paper sensor"
        );
        assert_eq!(
            probe_state(&info(), &printer("Ready", "Empty")),
            DeviceState::Degraded("printer=Ready,paper=Empty".into())
        );
        assert_eq!(
            probe_state(&info(), &printer("Ready", "CoverOpen")),
            DeviceState::Degraded("printer=Ready,paper=CoverOpen".into())
        );
        assert_eq!(
            probe_state(&info(), &printer("Error", "Ready")),
            DeviceState::Degraded("printer=Error,paper=Ready".into())
        );
        assert_eq!(
            probe_state(&info(), &ok(json!({}))),
            DeviceState::Degraded("printer=?,paper=?".into())
        );
        assert_eq!(
            probe_state(&info(), &Reply::Lost("timeout".into())),
            DeviceState::Degraded("printer status unreadable".into())
        );
        assert_eq!(
            probe_state(
                &info(),
                &Reply::Answer {
                    status: 500,
                    body: Value::Null
                }
            ),
            DeviceState::Degraded("printer status unreadable".into())
        );
        let ready = printer("Ready", "Ready");
        assert_eq!(
            probe_state(&Reply::Unreachable("refused".into()), &ready),
            DeviceState::Offline
        );
        assert_eq!(
            probe_state(&Reply::Lost("timeout".into()), &ready),
            DeviceState::Offline
        );
        assert_eq!(
            probe_state(
                &Reply::Answer {
                    status: 503,
                    body: json!({})
                },
                &ready
            ),
            DeviceState::Offline
        );
        assert_eq!(probe_state(&ok(Value::Null), &ready), DeviceState::Offline);
    }
}
