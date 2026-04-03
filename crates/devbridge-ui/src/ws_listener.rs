use futures_util::StreamExt;
use leptos::prelude::*;

use crate::api;
use crate::components::toast::{ToastLevel, ToastState};

/// Start a background task that listens for WebSocket job events
/// and shows toast notifications for completed/failed jobs.
pub fn start_ws_listener() {
    leptos::task::spawn_local(async {
        ws_loop().await;
    });
}

async fn ws_loop() {
    loop {
        match api::connect_ws() {
            Ok(ws) => {
                let (_write, mut read) = ws.split();
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(gloo_net::websocket::Message::Text(text)) => {
                            handle_event(&text);
                        }
                        Err(_) => break,
                        _ => {}
                    }
                }
            }
            Err(_) => {}
        }
        // Reconnect after 5 seconds
        gloo_timers::future::TimeoutFuture::new(5_000).await;
    }
}

fn handle_event(text: &str) {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };

    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

    let toast_state = use_context::<ToastState>();
    let Some(toast) = toast_state else {
        return;
    };

    match event_type {
        "state_changed" => {
            let state = event
                .get("new_state")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let job_id = event
                .get("job_id")
                .and_then(|j| j.as_str())
                .unwrap_or("unknown");

            match state {
                "completed" => {
                    toast.push(format!("Job {job_id} completed"), ToastLevel::Success);
                }
                "failed" => {
                    toast.push(format!("Job {job_id} failed"), ToastLevel::Error);
                }
                _ => {}
            }
        }
        "created" => {
            let name = event
                .get("document_name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            toast.push(format!("New job: {name}"), ToastLevel::Info);
        }
        _ => {}
    }
}
