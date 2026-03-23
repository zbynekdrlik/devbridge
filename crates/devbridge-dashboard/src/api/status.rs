use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/status", get(get_status))
}

async fn get_status(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed();

    let jobs_today = state
        .queue
        .as_ref()
        .and_then(|q| q.count_jobs_today().ok())
        .unwrap_or(0);

    let connected = state.connected_clients.load(Ordering::Relaxed);

    Json(json!({
        "mode": state.mode,
        "version": state.version,
        "uptime_secs": uptime.as_secs(),
        "status": "running",
        "connected_clients": connected,
        "jobs_today": jobs_today,
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_status_endpoint_returns_200() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["mode"], "server");
        assert_eq!(json["status"], "running");
    }

    #[tokio::test]
    async fn test_status_response_matches_ui_contract() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().expect("status must be a JSON object");

        // Assert the exact keys the UI reads
        assert!(obj.contains_key("mode"), "missing 'mode' field");
        assert!(
            obj.contains_key("connected_clients"),
            "missing 'connected_clients' field"
        );
        assert!(obj.contains_key("jobs_today"), "missing 'jobs_today' field");

        // Assert types
        assert!(obj["mode"].is_string(), "'mode' must be a string");
        assert!(
            obj["connected_clients"].is_u64(),
            "'connected_clients' must be a u64"
        );
        assert!(obj["jobs_today"].is_u64(), "'jobs_today' must be a u64");

        // Assert values for server mode with no queue
        assert_eq!(obj["mode"].as_str().unwrap(), "server");
        assert_eq!(obj["connected_clients"].as_u64().unwrap(), 0);
        assert_eq!(obj["jobs_today"].as_u64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_status_connected_clients_reflects_real_count() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let connected = Arc::new(AtomicU64::new(0));
        let state = AppState::new("server".into()).with_connected_clients(Arc::clone(&connected));
        let app = crate::build_router(state);

        // Initially 0
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["connected_clients"], 0);

        // Set to 3
        connected.store(3, Ordering::Relaxed);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["connected_clients"], 3);
    }
}
