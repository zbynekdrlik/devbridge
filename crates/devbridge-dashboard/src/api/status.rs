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
    Json(json!({
        "mode": state.mode,
        "version": state.version,
        "uptime_secs": uptime.as_secs(),
        "status": "running",
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
}
