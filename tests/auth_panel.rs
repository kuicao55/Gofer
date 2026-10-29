use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use gofer::auth::{self, AuthState};
use gofer::db::Db;
use gofer::demo::DemoConfig;
use gofer::server::{self, AppState, AppStateInit};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

async fn auth_with_password() -> AuthState {
    let hash = auth::hash_password_async(PASSWORD.to_string())
        .await
        .unwrap();
    AuthState::new(Some(hash), vec![7u8; 32], false)
}

async fn app_with(
    db: Db,
    auth_state: AuthState,
    env_token: Option<&str>,
    dry_run: bool,
    demo_cfg: DemoConfig,
) -> Arc<AppState> {
    let env_token = env_token.map(String::from);
    let db_token = db.get_setting(auth::API_TOKEN_KEY).await.unwrap();
    let api = auth::resolve_api_token(db_token.as_deref(), env_token.clone());
    AppState::new(AppStateInit {
        db,
        api,
        env_token,
        token_updated_at: None,
        auth: Arc::new(auth_state),
        dry_run,
        lock_headless: true,
        base_url: "http://127.0.0.1:8790".into(),
        public_url: None,
        cookie_secure: false,
        paused: false,
        demo_cfg,
    })
}

fn test_demo_cfg() -> DemoConfig {
    DemoConfig::for_test(
        std::env::temp_dir().join("gofer-test-shots"),
        PathBuf::from("/bin/true"),
    )
}

fn cookie(st: &AppState) -> String {
    format!("{}={}", auth::SESSION_COOKIE, st.auth.issue_session())
}

fn form_request(method: &str, uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn set_cookie(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::to_string)
}

fn location(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn first_run_setup_login_logout() {
    let db = Db::connect_memory().await.unwrap();
    let st = app_with(
        db,
        AuthState::new(None, vec![7u8; 32], false),
        None,
        true,
        test_demo_cfg(),
    )
    .await;
    let app = server::router(st.clone());

    let resp = app.clone().oneshot(get_request("/", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/setup"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get("hx-redirect")
            .and_then(|v| v.to_str().ok()),
        Some("/setup")
    );

    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/setup",
            &format!("password={PASSWORD}&confirm={PASSWORD}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/"));
    let setup_cookie = set_cookie(&resp).expect("setup should set a session cookie");

    let resp = app
        .clone()
        .oneshot(get_request("/", Some(&setup_cookie)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dashboard = body_text(resp).await;
    assert!(dashboard.contains("工具与浏览器"));
    assert!(dashboard.contains("小红书浏览器"));
    assert!(dashboard.contains("锁票浏览器"));
    assert!(dashboard.contains("打开浏览器") || dashboard.contains("关闭浏览器"));

    // A second setup attempt must not replace the password.
    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/setup",
            "password=another-one&confirm=another-one",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/login"));

    // Login with a wrong password fails, then succeeds.
    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/login",
            "password=wrong-password",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&resp).is_none());

    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/login",
            &format!("password={PASSWORD}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let login_cookie = set_cookie(&resp).expect("login should set a cookie");

    // Logout clears the cookie.
    let resp = app
        .clone()
        .oneshot(form_request("POST", "/logout", "", Some(&login_cookie)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/login"));
    let cleared = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("logout should clear the cookie");
    assert!(cleared.contains("Max-Age=0"));

    // The API token header must not authorize the panel.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("x-gofer-token", "api-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/login"));

    // A panel cookie must not authorize the API token endpoints.
    let st2 = app_with(
        Db::connect_memory().await.unwrap(),
        auth_with_password().await,
        Some("api-token"),
        true,
        test_demo_cfg(),
    )
    .await;
    let app2 = server::router(st2.clone());
    let resp = app2
        .oneshot(
            Request::builder()
                .uri("/v1/requests")
                .header(header::COOKIE, cookie(&st2))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_rotation_is_immediate_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("gofer.db");
    let db = Db::connect(&db_path).await.unwrap();
    let st = app_with(
        db.clone(),
        auth_with_password().await,
        Some("env-token"),
        true,
        test_demo_cfg(),
    )
    .await;
    let app = server::router(st.clone());
    let c = cookie(&st);

    let resp = app
        .clone()
        .oneshot(get_request("/settings", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let settings_html = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(settings_html.contains("iGlance / Services"));
    assert!(settings_html.contains("IGLANCE_SERVICE_MANIFEST"));

    let resp = app
        .clone()
        .oneshot(form_request("POST", "/actions/token/rotate", "", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/settings?rotated=1"));

    let new_token = db
        .get_setting(auth::API_TOKEN_KEY)
        .await
        .unwrap()
        .expect("rotated token should be persisted");
    assert!(new_token.starts_with("gof_"));
    assert_ne!(new_token, "env-token");

    let notify = serde_json::json!({"channel":"wechat","message":"hello"}).to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-gofer-token", "env-token")
                .body(Body::from(notify.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-gofer-token", &new_token)
                .body(Body::from(notify))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // A fresh AppState from the same DB must keep the DB token, not the env token.
    let auth_state = AuthState::load(&db, None).await.unwrap();
    let st2 = app_with(
        db.clone(),
        auth_state,
        Some("other-env-token"),
        true,
        test_demo_cfg(),
    )
    .await;
    let app2 = server::router(st2);
    let resp = app2
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/requests")
                .header("x-gofer-token", "other-env-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app2
        .oneshot(
            Request::builder()
                .uri("/v1/requests")
                .header("x-gofer-token", &new_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn panel_wechat_test_uses_simulate_override() {
    let db = Db::connect_memory().await.unwrap();
    let st = app_with(
        db.clone(),
        auth_with_password().await,
        None,
        false,
        test_demo_cfg(),
    )
    .await;
    let app = server::router(st.clone());
    let c = cookie(&st);

    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/actions/test/wechat",
            "message=panel+hello&simulate=1",
            Some(&c),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = location(&resp).expect("focus location");
    assert!(location.starts_with("/tests?focus="));

    let mut row = None;
    for _ in 0..40 {
        row = db
            .recent(10)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.source == "panel");
        if row.as_ref().is_some_and(|r| r.status == "ok") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let row = row.expect("panel test row");
    assert_eq!(row.status, "ok");
    assert!(row.summary.contains("dry-run"));
    let result: serde_json::Value =
        serde_json::from_str(row.result_json.as_deref().unwrap()).unwrap();
    assert_eq!(result["dry_run"], true);

    // The focused status fragment is available to the authenticated panel.
    let id = row.id;
    let resp = app
        .oneshot(get_request(&format!("/tests/status?id={id}"), Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("成功"));
}

#[tokio::test]
async fn demo_runs_without_submitting_and_can_be_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let shots = dir.path().join("shots");
    std::fs::create_dir_all(&shots).unwrap();
    let script = dir.path().join("fake-demo.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --screenshot-dir) dir="$2"; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$dir"
echo "[seat] 选中: ['G14', 'G15']"
printf 'PNG' > "$dir/latest.png"
echo "[demo] 完成: 未提交订单"
read _ || sleep 30
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let db = Db::connect_memory().await.unwrap();
    let st = app_with(
        db.clone(),
        auth_with_password().await,
        None,
        true,
        DemoConfig::for_test(shots.clone(), script),
    )
    .await;
    let app = server::router(st.clone());
    let c = cookie(&st);

    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/actions/demo/start",
            "city_id=10&cinema_id=123&num_seats=2",
            Some(&c),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/demo"));

    // Wait until the script has selected seats (the worker keeps it alive).
    let mut row = None;
    for _ in 0..80 {
        row = db
            .recent(10)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.capability == "lock_demo");
        if st.demo.logs().iter().any(|l| l.contains("[seat] 选中:")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(row.is_some(), "demo request row should exist");
    assert!(
        st.demo.logs().iter().any(|l| l.contains("[seat] 选中:")),
        "demo script output should be streamed"
    );

    let resp = app
        .clone()
        .oneshot(get_request("/demo/status", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("demo-status"));

    let resp = app
        .clone()
        .oneshot(get_request("/demo/logs", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_text(resp).await.contains("[seat] 选中:"));

    let resp = app
        .clone()
        .oneshot(get_request("/demo/screenshot", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );

    let resp = app
        .clone()
        .oneshot(form_request("POST", "/actions/demo/stop", "", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let mut final_row = None;
    for _ in 0..80 {
        if let Some(row) = db
            .recent(10)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.capability == "lock_demo")
        {
            if row.status == "ok" || row.status == "failed" {
                final_row = Some(row);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let final_row = final_row.expect("demo should finish after stop");
    assert_eq!(final_row.status, "ok");
    assert!(final_row.summary.contains("未提交订单"));
    let result: serde_json::Value =
        serde_json::from_str(final_row.result_json.as_deref().unwrap()).unwrap();
    assert_eq!(result["order_submitted"], false);
    assert_eq!(result["selected"], true);
}

#[tokio::test]
async fn lock_discord_webhook_config() {
    let db = Db::connect_memory().await.unwrap();
    let st = app_with(
        db.clone(),
        auth_with_password().await,
        None,
        true,
        test_demo_cfg(),
    )
    .await;
    let app = server::router(st.clone());
    let c = cookie(&st);

    let resp = app
        .clone()
        .oneshot(get_request("/", Some(&c)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("lock-notify-dialog"));
    assert!(html.contains("Discord 通知"));
    assert!(html.contains("未配置"));
    assert!(html.contains("配置"));

    let invalid = "https://evil.example/api/webhooks/1/token";
    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/actions/lock/webhook",
            &format!("webhook={}", encode_form(invalid)),
            Some(&c),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/?webhook=invalid"));
    assert_eq!(
        db.get_setting(gofer::discord::WEBHOOK_KEY).await.unwrap(),
        None
    );

    let webhook = "https://discord.com/api/webhooks/123/token";
    let resp = app
        .clone()
        .oneshot(form_request(
            "POST",
            "/actions/lock/webhook",
            &format!("webhook={}", encode_form(webhook)),
            Some(&c),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/?webhook=saved"));
    assert_eq!(
        db.get_setting(gofer::discord::WEBHOOK_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(webhook)
    );

    let resp = app
        .clone()
        .oneshot(get_request("/", Some(&c)))
        .await
        .unwrap();
    let html = body_text(resp).await;
    assert!(html.contains("已配置"));
    assert!(html.contains("修改"));

    let resp = app
        .oneshot(form_request(
            "POST",
            "/actions/lock/webhook",
            "webhook=&clear=1",
            Some(&c),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp).as_deref(), Some("/?webhook=cleared"));
    assert_eq!(
        db.get_setting(gofer::discord::WEBHOOK_KEY).await.unwrap(),
        None
    );
}

fn encode_form(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(':', "%3A")
        .replace('/', "%2F")
        .replace('?', "%3F")
        .replace('&', "%26")
        .replace('=', "%3D")
}

#[tokio::test]
async fn demo_script_has_no_submit_path() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/gofer-seat-demo.py");
    let text = std::fs::read_to_string(&script).unwrap();
    for forbidden in [
        "--confirm",
        "args.confirm",
        "click_confirm",
        "ConfirmClickedError",
        ".confirm-btn",
    ] {
        assert!(
            !text.contains(forbidden),
            "demo script must not contain {forbidden}"
        );
    }
    assert!(text.contains("stop_at_seat_map"));
    assert!(text.contains("[seat] 选中:"));
}
