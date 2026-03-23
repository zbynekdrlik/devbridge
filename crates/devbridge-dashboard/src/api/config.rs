use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/config", get(get_config))
}

async fn get_config(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "mode": state.mode,
        "version": state.version,
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_config_endpoint_returns_200() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_config_response_has_mode_and_version() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().expect("config must be a JSON object");

        assert!(obj.contains_key("mode"), "missing 'mode' field");
        assert!(obj.contains_key("version"), "missing 'version' field");
        assert!(obj["mode"].is_string(), "'mode' must be a string");
        assert!(obj["version"].is_string(), "'version' must be a string");
        assert_eq!(obj["mode"].as_str().unwrap(), "server");
    }
}
