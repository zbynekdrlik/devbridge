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
            // On server, return configured printer name if set
            let printers: Vec<String> = state.target_printer.into_iter().collect();
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
        assert_eq!(arr[0], "TestPrinter");
    }
}
