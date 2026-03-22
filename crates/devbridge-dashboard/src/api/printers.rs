use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/printers", get(get_printers))
}

async fn get_printers(State(state): State<AppState>) -> Json<Value> {
    match state.mode.as_str() {
        "client" => {
            // On client, query Windows print subsystem for available printers
            let printers = tokio::task::spawn_blocking(devbridge_client::list_printers)
                .await
                .unwrap_or_else(|_| Ok(vec![]))
                .unwrap_or_default();
            Json(json!(printers))
        }
        _ => {
            // On server, return configured printer name as structured object
            let printers: Vec<devbridge_core::PrinterInfo> = state
                .target_printer
                .into_iter()
                .map(|name| devbridge_core::PrinterInfo {
                    name,
                    driver: "-".to_string(),
                    status: "unknown".to_string(),
                    jobs: 0,
                })
                .collect();
            Json(json!(printers))
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_printers_endpoint_returns_array() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "expected JSON array, got {:?}", json);
    }

    #[tokio::test]
    async fn test_printers_endpoint_returns_configured_printer() {
        let state = AppState::new("server".into()).with_target_printer("TestPrinter".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        // Must be an object, not a bare string
        let printer = arr[0].as_object().expect("printer must be a JSON object");
        assert_eq!(printer["name"].as_str().unwrap(), "TestPrinter");
    }

    #[tokio::test]
    async fn test_printers_returns_objects_with_required_fields() {
        let state = AppState::new("server".into()).with_target_printer("AnyPrinter".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();

        for item in arr {
            let obj = item
                .as_object()
                .expect("each printer must be a JSON object");
            assert!(obj.contains_key("name"), "missing 'name' field");
            assert!(obj.contains_key("driver"), "missing 'driver' field");
            assert!(obj.contains_key("status"), "missing 'status' field");
            assert!(obj.contains_key("jobs"), "missing 'jobs' field");

            assert!(obj["name"].is_string(), "'name' must be a string");
            assert!(obj["driver"].is_string(), "'driver' must be a string");
            assert!(obj["status"].is_string(), "'status' must be a string");
            assert!(obj["jobs"].is_u64(), "'jobs' must be a u64");
        }
    }

    #[tokio::test]
    async fn test_printers_server_mode_returns_configured_printer_as_object() {
        let state = AppState::new("server".into()).with_target_printer("TestPrinter".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);

        let printer = &arr[0];
        assert_eq!(printer["name"], "TestPrinter");
        assert_eq!(printer["driver"], "-");
        assert_eq!(printer["status"], "unknown");
        assert_eq!(printer["jobs"], 0);
    }
}
