use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/printers", get(get_printers))
        .route("/printers/target", get(get_target).put(set_target))
}

async fn get_printers(State(state): State<AppState>) -> Json<Value> {
    let current_target = state.target_printer.read().await.clone();

    match state.mode.as_str() {
        "client" => {
            let mut printers = tokio::task::spawn_blocking(devbridge_client::list_printers)
                .await
                .unwrap_or_else(|_| Ok(vec![]))
                .unwrap_or_default();
            for p in &mut printers {
                p.is_target = p.name == current_target;
            }
            Json(json!(printers))
        }
        _ => {
            let printers: Vec<devbridge_core::PrinterInfo> = if current_target.is_empty() {
                vec![]
            } else {
                vec![devbridge_core::PrinterInfo {
                    name: current_target.clone(),
                    driver: "-".to_string(),
                    status: "unknown".to_string(),
                    jobs: 0,
                    is_target: true,
                }]
            };
            Json(json!(printers))
        }
    }
}

#[derive(Serialize)]
struct TargetResponse {
    name: String,
}

async fn get_target(State(state): State<AppState>) -> Json<TargetResponse> {
    let name = state.target_printer.read().await.clone();
    Json(TargetResponse { name })
}

#[derive(Deserialize)]
struct SetTargetRequest {
    name: String,
}

async fn set_target(
    State(state): State<AppState>,
    Json(body): Json<SetTargetRequest>,
) -> Result<Json<TargetResponse>, StatusCode> {
    if body.name.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let new_name = body.name.trim().to_string();

    // Update in-memory state
    *state.target_printer.write().await = new_name.clone();

    // Persist to config file if path is available
    if let Some(ref path) = state.config_path {
        let path = path.clone();
        let name = new_name.clone();
        let _ = tokio::task::spawn_blocking(move || {
            devbridge_core::config::update_target_printer(&path, &name)
        })
        .await;
    }

    Ok(Json(TargetResponse { name: new_name }))
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
            assert!(obj.contains_key("is_target"), "missing 'is_target' field");

            assert!(obj["name"].is_string(), "'name' must be a string");
            assert!(obj["driver"].is_string(), "'driver' must be a string");
            assert!(obj["status"].is_string(), "'status' must be a string");
            assert!(obj["jobs"].is_u64(), "'jobs' must be a u64");
            assert!(obj["is_target"].is_boolean(), "'is_target' must be a bool");
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
        assert_eq!(printer["is_target"], true);
    }

    #[tokio::test]
    async fn test_get_target_printer() {
        let state = AppState::new("server".into()).with_target_printer("CurrentPrinter".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers/target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"].as_str().unwrap(), "CurrentPrinter");
    }

    #[tokio::test]
    async fn test_set_target_printer_returns_200() {
        let state = AppState::new("server".into()).with_target_printer("OldPrinter".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/printers/target")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "NewPrinter"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"].as_str().unwrap(), "NewPrinter");
    }

    #[tokio::test]
    async fn test_set_target_printer_updates_state() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let shared = Arc::new(RwLock::new("OldPrinter".to_string()));
        let state = AppState::new("server".into()).with_shared_target_printer(shared.clone());
        let app = crate::build_router(state);

        // PUT to change target
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/printers/target")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": "NewPrinter"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Verify in-memory state was updated
        assert_eq!(*shared.read().await, "NewPrinter");

        // GET should reflect the new target
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/printers/target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"].as_str().unwrap(), "NewPrinter");
    }

    #[tokio::test]
    async fn test_set_target_printer_rejects_empty_name() {
        let state = AppState::new("server".into()).with_target_printer("Printer".into());
        let app = crate::build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/printers/target")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name": ""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn test_printers_include_is_target_field() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let shared = Arc::new(RwLock::new("TestPrinter".to_string()));
        let state = AppState::new("server".into()).with_shared_target_printer(shared);
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
        assert_eq!(arr[0]["is_target"], true);
    }
}
