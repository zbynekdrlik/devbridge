use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::Value;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/jobs/{id}/events", get(get_job_events))
}

async fn get_job_events(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Json<Vec<Value>> {
    let Some(queue) = &state.queue else {
        return Json(vec![]);
    };

    let events = queue.get_job_events(&job_id).unwrap_or_default();

    let json_events: Vec<Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "job_id": e.job_id,
                "stage": e.stage,
                "success": e.success,
                "detail": e.detail,
                "timestamp": e.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(json_events)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::AppState;

    #[tokio::test]
    async fn test_job_events_no_queue_returns_empty_array() {
        let app = crate::build_router(AppState::new("server".into()));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs/some-job-id/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "Expected JSON array, got: {json}");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_job_events_unknown_job_returns_empty_array() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = Arc::new(devbridge_server::JobQueue::new(storage).unwrap());
        let state = AppState::new("server".into()).with_queue(queue);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs/nonexistent-job/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "Expected JSON array, got: {json}");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_job_events_returns_events_for_job() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = devbridge_server::storage::Storage::new(&db_path).unwrap();
        let queue = Arc::new(devbridge_server::JobQueue::new(storage).unwrap());

        // Insert two events for the job
        let event1 = devbridge_core::job_event::PrintJobEvent::ok(
            "job-evt-1",
            devbridge_core::job_event::PrintStage::Received,
            "IPP job received",
        );
        let event2 = devbridge_core::job_event::PrintJobEvent::ok(
            "job-evt-1",
            devbridge_core::job_event::PrintStage::Routed,
            "routed to client",
        );
        queue.insert_job_event(&event1).unwrap();
        queue.insert_job_event(&event2).unwrap();

        let state = AppState::new("server".into()).with_queue(queue);
        let app = crate::build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs/job-evt-1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json.as_array().expect("response must be an array");
        assert_eq!(arr.len(), 2);

        // Verify response shape
        let first = arr[0].as_object().unwrap();
        assert!(first.contains_key("job_id"), "missing 'job_id'");
        assert!(first.contains_key("stage"), "missing 'stage'");
        assert!(first.contains_key("success"), "missing 'success'");
        assert!(first.contains_key("detail"), "missing 'detail'");
        assert!(first.contains_key("timestamp"), "missing 'timestamp'");

        assert_eq!(first["job_id"].as_str().unwrap(), "job-evt-1");
        assert_eq!(first["stage"].as_str().unwrap(), "received");
        assert!(first["success"].as_bool().unwrap());
        assert_eq!(first["detail"].as_str().unwrap(), "IPP job received");

        // Events for a different job must not appear
        let response2 = crate::build_router(AppState::new("server".into()))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/jobs/other-job/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body2 = response2.into_body().collect().await.unwrap().to_bytes();
        let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
        assert_eq!(json2.as_array().unwrap().len(), 0);
    }
}
