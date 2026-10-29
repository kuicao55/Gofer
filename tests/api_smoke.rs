use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gofer::auth::AuthState;
use gofer::db::Db;
use gofer::demo::DemoConfig;
use gofer::server::{self, AppState, AppStateInit};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn state_with_paused(token: Option<&str>, dry_run: bool, paused: bool) -> Arc<AppState> {
    let db = Db::connect_memory().await.unwrap();
    let env_token = token.map(String::from);
    let api = gofer::auth::resolve_api_token(None, env_token.clone());
    let auth = Arc::new(AuthState::new(
        Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$Zm9v".into()),
        vec![7u8; 32],
        false,
    ));
    AppState::new(AppStateInit {
        db,
        api,
        env_token,
        token_updated_at: None,
        auth,
        dry_run,
        lock_headless: true,
        base_url: "http://127.0.0.1:8790".into(),
        public_url: None,
        cookie_secure: false,
        paused,
        demo_cfg: DemoConfig::for_test(
            PathBuf::from("/tmp/gofer-test-shots"),
            PathBuf::from("/bin/true"),
        ),
    })
}

async fn state(token: Option<&str>, dry_run: bool) -> Arc<AppState> {
    state_with_paused(token, dry_run, false).await
}

fn session_cookie(st: &AppState) -> String {
    format!(
        "{}={}",
        gofer::auth::SESSION_COOKIE,
        st.auth.issue_session()
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
        .oneshot(
            Request::builder()
                .uri("/")
                .header("cookie", session_cookie(&st))
                .body(Body::empty())
                .unwrap(),
        )
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
                .header("cookie", session_cookie(&st))
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
                .header("cookie", session_cookie(&st))
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

#[tokio::test]
async fn service_api_reports_memory_and_controls() {
    let st = state(Some("secret"), false).await;
    let app = server::router(st.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/service")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/service")
                .header("x-gofer-token", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["schema"], "iglance.service.v1");
    assert_eq!(v["service"]["id"], "gofer");
    assert_eq!(v["state"], "running");
    assert_eq!(v["auth"]["token_required"], true);
    assert_eq!(v["memory"]["source"], "ps-rss");
    assert!(v["memory"]["gofer"]["processes"].as_u64().unwrap() >= 1);
    assert!(v["memory"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["id"] == "service"));
    assert!(v["controls"]["start"].is_object());
    assert!(v["controls"]["start"]["binding_command"].is_string());
    assert!(v["controls"]["start"]["manifest_command"].is_string());
    assert_eq!(v["controls"]["protocol"]["schema"], "iglance.service.v1");
    assert_eq!(v["controls"]["stop"]["method"], "POST");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/service/memory")
                .header("x-gofer-token", "secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(v["memory"]["total_rss_bytes"].as_u64().unwrap() > 0);

    for (uri, paused) in [
        ("/api/v1/service/pause", true),
        ("/api/v1/service/resume", false),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("x-gofer-token", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(st.is_paused(), paused);
    }
}
