use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/jobs", get(get_jobs))
}

async fn get_jobs(State(state): State<AppState>) -> Json<Value> {
    let Some(queue) = &state.queue else {
        return Json(json!([]));
    };

    match queue.get_all_jobs() {
        Ok(jobs) => {
            let jobs_json: Vec<Value> = jobs
                .iter()
                .map(|j| {
                    json!({
                        "job_id": j.job_id,
                        "document_name": j.document_name,
                        "target_printer": j.target_printer,
                        "copies": j.copies,
                        "paper_size": j.paper_size,
                        "duplex": j.duplex,
                        "color": j.color,
                        "payload_size": j.payload_size,
                        "payload_sha256": j.payload_sha256,
                        "state": format!("{:?}", j.state).to_lowercase(),
                        "created_at": j.created_at.to_rfc3339(),
                        "updated_at": j.updated_at.to_rfc3339(),
                    })
                })
                .collect();
            Json(json!(jobs_json))
        }
        Err(_) => Json(json!([])),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_jobs_endpoint_returns_array() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "Expected JSON array, got: {}", json);
    }
}
