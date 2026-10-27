use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gofer::db::Db;
use gofer::server::{self, AppState};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn state_with_paused(token: Option<&str>, dry_run: bool, paused: bool) -> Arc<AppState> {
    let db = Db::connect_memory().await.unwrap();
    AppState::new(
        db,
        token.map(String::from),
        dry_run,
        true,
        "http://127.0.0.1:8790",
        paused,
    )
}

async fn state(token: Option<&str>, dry_run: bool) -> Arc<AppState> {
    let db = Db::connect_memory().await.unwrap();
    AppState::new(
        db,
        token.map(String::from),
        dry_run,
        true,
        "http://127.0.0.1:8790",
        false,
    )
}

#[tokio::test]
async fn notify_dry_run_is_recorded_and_visible_in_panel() {
    let st = state(None, true).await;
    let app = server::router(st.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "channel": "wechat",
                        "title": "test",
                        "message": "hello",
                        "source": "unit"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let id = v["request_id"].as_str().unwrap().to_string();

    let mut row = None;
    for _ in 0..40 {
        row = st.db.get(&id).await.unwrap();
        if row.as_ref().is_some_and(|r| r.status == "ok") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let row = row.expect("request row");
    assert_eq!(row.status, "ok");
    assert_eq!(row.capability, "wechat");
    assert!(row.summary.contains("dry-run"));

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Gofer"));
    assert!(html.contains("微信"));
    assert!(html.contains("hello"));
}

#[tokio::test]
async fn token_is_required_when_configured() {
    let st = state(Some("s3cret"), true).await;
    let app = server::router(st);

    let body = serde_json::json!({"channel": "wechat", "message": "hello"}).to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .header("x-gofer-token", "s3cret")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn xhs_requires_group() {
    let st = state(None, true).await;
    let app = server::router(st);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"channel": "xhs", "message": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn lock_requires_cinema_and_show_date() {
    let st = state(None, true).await;
    let app = server::router(st);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/lock")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"movie_id": 1, "cinema_id": "", "show_date": ""})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn paused_service_rejects_new_requests() {
    let st = state_with_paused(None, true, true).await;
    let app = server::router(st);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"channel": "wechat", "message": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn service_switch_pauses_and_resumes() {
    let st = state(None, true).await;
    let app = server::router(st.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/actions/service/toggle")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["paused"], true);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"channel": "wechat", "message": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/actions/service/toggle")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["paused"], false);
}
