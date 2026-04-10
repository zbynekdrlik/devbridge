//! WebSocket client that connects to the dashboard `/api/ws` endpoint
//! and forwards parsed events through an mpsc channel.

use devbridge_core::job::JobEvent;
use devbridge_core::job_event::PrintJobEvent;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

/// Events received from the dashboard WebSocket.
#[derive(Debug, Clone)]
pub enum WsEvent {
    Job(JobEvent),
    Print(PrintJobEvent),
}

/// Parse a WebSocket text message into a [`WsEvent`].
///
/// Tries `PrintJobEvent` first (has a `stage` field), then `JobEvent`
/// (tagged with `type`). Returns `None` if neither parse succeeds.
pub fn parse_ws_message(text: &str) -> Option<WsEvent> {
    // Try PrintJobEvent first — it has a distinctive `stage` field
    if let Ok(ev) = serde_json::from_str::<PrintJobEvent>(text) {
        return Some(WsEvent::Print(ev));
    }
    if let Ok(ev) = serde_json::from_str::<JobEvent>(text) {
        return Some(WsEvent::Job(ev));
    }
    None
}

/// Connect to the dashboard WebSocket and forward parsed events.
///
/// Converts the `dashboard_url` from `http://` to `ws://` (or `https://` to
/// `wss://`) and appends `/api/ws`. On disconnect the function reconnects with
/// exponential backoff (1 s initial, 60 s max).
pub async fn run_ws_client(dashboard_url: String, tx: mpsc::Sender<WsEvent>) {
    let ws_url = build_ws_url(&dashboard_url);
    let mut backoff = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(60);

    loop {
        tracing::info!("Connecting to WebSocket at {ws_url}");

        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                tracing::info!("WebSocket connected");
                backoff = std::time::Duration::from_secs(1); // reset on success

                let (_write, mut read) = ws_stream.split();

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Some(event) = parse_ws_message(&text) {
                                if tx.send(event).await.is_err() {
                                    tracing::warn!("WsEvent receiver dropped, stopping ws client");
                                    return;
                                }
                            } else {
                                tracing::debug!("Ignoring unparseable WS message: {text}");
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                            tracing::info!("WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("WebSocket read error: {e}");
                            break;
                        }
                        _ => {} // ping/pong/binary — ignore
                    }
                }
            }
            Err(e) => {
                tracing::warn!("WebSocket connection failed: {e}");
            }
        }

        tracing::info!("Reconnecting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Convert an HTTP dashboard URL to a WebSocket URL with `/api/ws` path.
fn build_ws_url(dashboard_url: &str) -> String {
    let base = dashboard_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    let base = base.trim_end_matches('/');
    format!("{base}/api/ws")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_job_created_event() {
        let json = r#"{"type":"created","job_id":"abc","document_name":"test.pdf","requesting_user":"alice","target_printer":"pjsnvs printer"}"#;
        let event = parse_ws_message(json).expect("should parse");
        match event {
            WsEvent::Job(JobEvent::Created {
                job_id,
                document_name,
                requesting_user,
                target_printer,
            }) => {
                assert_eq!(job_id, "abc");
                assert_eq!(document_name, "test.pdf");
                assert_eq!(requesting_user, Some("alice".to_string()));
                assert_eq!(target_printer, "pjsnvs printer");
            }
            other => panic!("expected Job(Created), got {other:?}"),
        }
    }

    #[test]
    fn parse_job_state_changed_event() {
        let json = r#"{"type":"state_changed","job_id":"xyz","new_state":"printing","requesting_user":"alice"}"#;
        let event = parse_ws_message(json).expect("should parse");
        match event {
            WsEvent::Job(JobEvent::StateChanged {
                job_id,
                new_state,
                requesting_user,
            }) => {
                assert_eq!(job_id, "xyz");
                assert_eq!(new_state, devbridge_core::job::JobState::Printing);
                assert_eq!(requesting_user, Some("alice".to_string()));
            }
            other => panic!("expected Job(StateChanged), got {other:?}"),
        }
    }

    #[test]
    fn parse_state_changed_to_completed() {
        let json = r#"{"type":"state_changed","job_id":"job-1","new_state":"completed","requesting_user":"alice"}"#;
        let event = parse_ws_message(json).expect("should parse");
        match event {
            WsEvent::Job(JobEvent::StateChanged { new_state, .. }) => {
                assert_eq!(new_state, devbridge_core::job::JobState::Completed);
            }
            other => panic!("expected StateChanged Completed, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_event() {
        let json = r#"{"job_id":"abc","requesting_user":"bob","stage":"completed","success":true,"detail":"done","verification_method":"","verification_evidence":"","timestamp":"2026-04-09T10:00:00Z"}"#;
        let event = parse_ws_message(json).expect("should parse");
        match event {
            WsEvent::Print(ev) => {
                assert_eq!(ev.job_id, "abc");
                assert_eq!(ev.requesting_user, Some("bob".to_string()));
                assert_eq!(ev.stage, devbridge_core::job_event::PrintStage::Completed);
                assert!(ev.success);
                assert_eq!(ev.detail, "done");
            }
            other => panic!("expected Print event, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_event_without_requesting_user() {
        let json = r#"{"job_id":"abc","stage":"failed","success":false,"detail":"timeout","verification_method":"","verification_evidence":"","timestamp":"2026-04-09T10:00:00Z"}"#;
        let event = parse_ws_message(json).expect("should parse");
        match event {
            WsEvent::Print(ev) => {
                assert_eq!(ev.requesting_user, None);
                assert!(!ev.success);
            }
            other => panic!("expected Print event, got {other:?}"),
        }
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_ws_message("not json").is_none());
        assert!(parse_ws_message(r#"{"unknown":"thing"}"#).is_none());
    }

    #[test]
    fn build_ws_url_http() {
        assert_eq!(
            build_ws_url("http://10.88.1.100:9120"),
            "ws://10.88.1.100:9120/api/ws"
        );
    }

    #[test]
    fn build_ws_url_https_trailing_slash() {
        assert_eq!(
            build_ws_url("https://example.com/"),
            "wss://example.com/api/ws"
        );
    }
}
