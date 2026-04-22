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

    let mut resp = json!({
        "mode": state.mode,
        "version": state.version,
        "uptime_secs": uptime.as_secs(),
        "status": "running",
        "connected_clients": connected,
        "jobs_today": jobs_today,
    });

    // Add client identity fields when in client mode
    if let Some(ref id) = state.client_id {
        resp["client_id"] = json!(id);
    }
    if let Some(ref name) = state.printer_display_name {
        resp["printer_display_name"] = json!(name);
    }
    if let Some(ref addr) = state.printer_address {
        resp["printer_address"] = json!(addr);
    }
    if let Some(ref backend) = state.print_backend {
        resp["print_backend"] = json!(backend);
    }
    if let Some(ref server) = state.server_address {
        resp["server_address"] = json!(server);
    }

    Json(resp)
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
    async fn test_status_client_mode_includes_identity_fields() {
        use devbridge_core::config::{ClientConfig, TlsConfig};

        let client_config = ClientConfig {
            server_address: "10.77.8.200:50051".to_string(),
            target_printer: "Canon MG3600".to_string(),
            dashboard_port: 9120,
            reconnect_interval_secs: 5,
            max_reconnect_interval_secs: 60,
            client_id: Some("store-01".to_string()),
            print_backend: "windows_spooler".to_string(),
            printer_address: Some("10.78.5.9:9100".to_string()),
            ghostscript_device: "ppmraw".to_string(),
            ghostscript_resolution: 600,
            printer_tls: false,
            printer_display_name: Some("Canon MG3600".to_string()),
            virtual_printer_name: None,
            print_proxy_url: None,
            tls: TlsConfig {
                cert_file: "client.crt".to_string(),
                key_file: "client.key".to_string(),
                ca_file: "ca.crt".to_string(),
            },
            serial_bridge: Default::default(),
        };

        let state = AppState::new("client".into()).with_client_config(&client_config);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().expect("must be a JSON object");

        assert_eq!(obj["mode"].as_str().unwrap(), "client");
        assert_eq!(obj["client_id"].as_str().unwrap(), "store-01");
        assert_eq!(
            obj["printer_display_name"].as_str().unwrap(),
            "Canon MG3600"
        );
        assert_eq!(obj["printer_address"].as_str().unwrap(), "10.78.5.9:9100");
        assert_eq!(obj["print_backend"].as_str().unwrap(), "windows_spooler");
        assert_eq!(obj["server_address"].as_str().unwrap(), "10.77.8.200:50051");
    }

    #[tokio::test]
    async fn test_status_server_mode_excludes_client_identity_fields() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().expect("must be a JSON object");

        // Server mode must NOT include client identity fields
        assert!(
            !obj.contains_key("client_id"),
            "server must not have client_id"
        );
        assert!(
            !obj.contains_key("printer_display_name"),
            "server must not have printer_display_name"
        );
        assert!(
            !obj.contains_key("printer_address"),
            "server must not have printer_address"
        );
        assert!(
            !obj.contains_key("print_backend"),
            "server must not have print_backend"
        );
        assert!(
            !obj.contains_key("server_address"),
            "server must not have server_address"
        );
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
