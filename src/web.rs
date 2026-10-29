//! Authenticated operational panel for Gofer.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use askama::Template;
use axum::body::Body;
use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::auth;
use crate::capabilities;
use crate::db::{CapabilityStat, RequestRow};
use crate::demo::DemoJob;
use crate::discord;
use crate::maoyan;
use crate::server::{self, AppState};

pub fn web_dir() -> String {
    if let Ok(dir) = std::env::var("GOFER_WEB_DIR") {
        if !dir.trim().is_empty() {
            return dir;
        }
    }
    let candidates = [
        "web".to_string(),
        format!("{}/web", env!("CARGO_MANIFEST_DIR")),
        "/app/web".to_string(),
    ];
    candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .cloned()
        .unwrap_or_else(|| "web".to_string())
}

#[derive(Debug, Clone)]
pub struct StatsView {
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub running: i64,
    pub queued: i64,
    pub success_rate: String,
    pub all_total: i64,
}

#[derive(Debug, Clone)]
pub struct CheckView {
    pub label: String,
    pub ok: bool,
    pub detail: String,
    /// Optional inline action label (e.g. "配置"). Empty = no action.
    pub action: String,
}

#[derive(Debug, Clone)]
pub struct CapabilityView {
    pub id: String,
    pub label: String,
    pub subtitle: String,
    pub ready: bool,
    pub status_text: String,
    pub status_class: String,
    pub checks: Vec<CheckView>,
    pub checks_ok: usize,
    pub checks_total: usize,
    pub requests_24h: i64,
    pub ok_24h: i64,
    pub failed_24h: i64,
    pub queue: usize,
    pub last_at: String,
    pub last_error: String,
}

#[derive(Debug, Clone)]
pub struct RequestView {
    pub id: String,
    pub time: String,
    pub capability: String,
    pub capability_class: String,
    pub status_text: String,
    pub status_class: String,
    pub duration: String,
    pub source: String,
    pub title: String,
    pub preview: String,
    pub summary: String,
    pub show_summary: bool,
    pub error: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    version: String,
    now: String,
    page: String,
    uptime: String,
    base_url: String,
    token_on: bool,
    token_source: String,
    lock_webhook: String,
    lock_webhook_on: bool,
    webhook_notice: String,
    dry_run: bool,
    paused: bool,
    stats: StatsView,
    caps_ready: usize,
    caps_total: usize,
    capabilities: Vec<CapabilityView>,
    requests: Vec<RequestView>,
}

#[derive(Template)]
#[template(path = "update.html")]
struct UpdateTemplate {
    version: String,
    now: String,
    page: String,
    info: crate::update::UpdateInfo,
}

#[derive(Template)]
#[template(path = "update_result.html")]
struct UpdateResultTemplate {
    version: String,
    now: String,
    page: String,
    ok: bool,
    title: String,
    message: String,
}

#[derive(Template)]
#[template(path = "requests.html")]
struct RequestsTemplate {
    version: String,
    now: String,
    page: String,
    base_url: String,
    token_on: bool,
    paused: bool,
    count: usize,
    requests: Vec<RequestView>,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    version: String,
    now: String,
    error: String,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    version: String,
    now: String,
    error: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    version: String,
    now: String,
    page: String,
    base_url: String,
    panel_origin: String,
    token_on: bool,
    token: String,
    token_source: String,
    token_updated_at: String,
    binding_command: String,
    env_token_present: bool,
    env_password_managed: bool,
    rotated: bool,
    reset: bool,
    error: String,
}

#[derive(Template)]
#[template(path = "tests.html")]
struct TestsTemplate {
    version: String,
    now: String,
    page: String,
    dry_run: bool,
    paused: bool,
    focus_id: String,
    error: String,
    recent: Vec<RequestView>,
}

#[derive(Template)]
#[template(path = "test_status.html")]
struct TestStatusTemplate {
    id: String,
    status_text: String,
    status_class: String,
    running: bool,
    duration: String,
    summary: String,
    error: String,
}

#[derive(Template)]
#[template(path = "demo.html")]
struct DemoTemplate {
    version: String,
    now: String,
    page: String,
    city_id: u32,
    cinema_id: String,
    movie_id: String,
    num_seats: u64,
    imax_only: bool,
    running: bool,
    demo_ready: bool,
    script_path: String,
    screenshot_dir: String,
    error: String,
    recent: Vec<RequestView>,
}

#[derive(Template)]
#[template(path = "demo_status.html")]
struct DemoStatusTemplate {
    running: bool,
    phase: String,
    cinema_name: String,
    request_id: String,
    elapsed: String,
    last_line: String,
    db_status: String,
    db_status_class: String,
    db_summary: String,
    db_error: String,
    has_screenshot: bool,
    screenshot_version: u64,
}

#[derive(Template)]
#[template(path = "demo_logs.html")]
struct DemoLogsTemplate {
    logs: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct DemoStartForm {
    #[serde(default)]
    city_id: String,
    #[serde(default)]
    cinema_id: String,
    #[serde(default)]
    movie_id: String,
    #[serde(default)]
    num_seats: String,
    #[serde(default)]
    imax_only: Option<String>,
}

#[derive(Deserialize)]
pub struct SetupForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    password: String,
}

#[derive(Deserialize)]
pub struct WechatTestForm {
    #[serde(default)]
    message: String,
    #[serde(default)]
    simulate: Option<String>,
}

#[derive(Deserialize)]
pub struct XhsTestForm {
    #[serde(default)]
    group: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    simulate: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct SettingsQuery {
    rotated: Option<String>,
    reset: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct DashboardQuery {
    webhook: Option<String>,
}

#[derive(Deserialize)]
pub struct LockWebhookForm {
    #[serde(default)]
    webhook: String,
    #[serde(default)]
    clear: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct TestsQuery {
    focus: Option<String>,
}

#[derive(Deserialize)]
pub struct StatusQuery {
    id: String,
}

pub async fn dashboard(
    State(st): State<Arc<AppState>>,
    Query(q): Query<DashboardQuery>,
) -> Response {
    let notice = webhook_notice(q.webhook.as_deref());
    match build_dashboard(&st, notice).await {
        Ok(t) => render(&t).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn webhook_notice(code: Option<&str>) -> String {
    match code {
        Some("saved") => "Discord webhook 已保存。".into(),
        Some("cleared") => "已清除 Discord webhook。".into(),
        Some("invalid") => {
            "Webhook 地址无效：只允许 https://discord.com 或 discordapp.com 的 /api/webhooks/ 地址。"
                .into()
        }
        Some("save_failed") => "保存 Discord webhook 失败，请查看日志。".into(),
        Some("missing") => "还没有配置 Discord webhook。先保存再测试。".into(),
        Some("tested") => "测试通知已发送，请到 Discord 频道确认。".into(),
        Some("test_failed") => "测试通知发送失败，请检查 webhook 地址和网络。".into(),
        _ => String::new(),
    }
}

pub async fn action_lock_webhook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LockWebhookForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if form.clear.is_some() {
        if let Err(e) = st.db.delete_setting(discord::WEBHOOK_KEY).await {
            tracing::warn!("clear discord webhook failed: {e}");
            return Redirect::to("/?webhook=save_failed").into_response();
        }
        return Redirect::to("/?webhook=cleared").into_response();
    }

    let webhook = form.webhook.trim();
    if let Err(e) = discord::validate_webhook(webhook) {
        tracing::warn!("invalid discord webhook: {e}");
        return Redirect::to("/?webhook=invalid").into_response();
    }
    if let Err(e) = st.db.set_setting(discord::WEBHOOK_KEY, webhook).await {
        tracing::warn!("save discord webhook failed: {e}");
        return Redirect::to("/?webhook=save_failed").into_response();
    }
    Redirect::to("/?webhook=saved").into_response()
}

pub async fn action_lock_webhook_test(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    let webhook = match st.db.get_setting(discord::WEBHOOK_KEY).await {
        Ok(Some(url)) if !url.trim().is_empty() => url,
        _ => return Redirect::to("/?webhook=missing").into_response(),
    };
    match discord::send_test(&st.http, &webhook).await {
        Ok(()) => Redirect::to("/?webhook=tested").into_response(),
        Err(e) => {
            tracing::warn!("discord test notification failed: {e}");
            Redirect::to("/?webhook=test_failed").into_response()
        }
    }
}

// ------------------------- auth pages -------------------------

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn render_no_store<T: Template>(t: &T) -> Response {
    no_store(render(t).into_response())
}

fn render_no_store_status<T: Template>(t: &T, status: StatusCode) -> Response {
    let mut response = render(t).into_response();
    *response.status_mut() = status;
    no_store(response)
}

fn session_redirect(cookie: String, location: &str) -> Response {
    let mut response = Redirect::to(location).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    no_store(response)
}

pub async fn setup_page(State(st): State<Arc<AppState>>) -> Response {
    if st.auth.has_admin_password() {
        return Redirect::to("/login").into_response();
    }
    render_no_store(&SetupTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        error: String::new(),
    })
}

pub async fn setup_submit(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if st.auth.has_admin_password() {
        return Redirect::to("/login").into_response();
    }

    let len = form.password.chars().count();
    let error = if len < 8 {
        "管理员密码至少需要 8 个字符"
    } else if len > 256 {
        "管理员密码过长"
    } else if form.password != form.confirm {
        "两次输入的密码不一致"
    } else {
        ""
    };
    if !error.is_empty() {
        return render_no_store_status(
            &SetupTemplate {
                version: server::VERSION.to_string(),
                now: now_label(),
                error: error.into(),
            },
            StatusCode::BAD_REQUEST,
        );
    }

    let _guard = st.auth.setup_lock.lock().await;
    if st.auth.has_admin_password() {
        return Redirect::to("/login").into_response();
    }

    let hash = match auth::hash_password_async(form.password).await {
        Ok(hash) => hash,
        Err(e) => {
            return render_no_store_status(
                &SetupTemplate {
                    version: server::VERSION.to_string(),
                    now: now_label(),
                    error: format!("创建管理员密码失败: {e}"),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };

    match st
        .db
        .set_setting_if_absent(auth::ADMIN_HASH_KEY, &hash)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Redirect::to("/login").into_response(),
        Err(e) => {
            return render_no_store_status(
                &SetupTemplate {
                    version: server::VERSION.to_string(),
                    now: now_label(),
                    error: format!("保存管理员密码失败: {e}"),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    }

    st.auth.set_admin_hash(Some(hash));
    let secret = st.auth.rotate_session_secret();
    if let Err(e) = st
        .db
        .set_setting(auth::SESSION_SECRET_KEY, &auth::hex_encode(&secret))
        .await
    {
        tracing::warn!("持久化 session secret 失败: {e}");
    }

    let cookie = auth::session_cookie(&st.auth.issue_session(), st.cookie_secure);
    session_redirect(cookie, "/")
}

pub async fn login_page(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth::session_from_headers(&headers).is_some_and(|value| st.auth.valid_session(&value)) {
        return Redirect::to("/").into_response();
    }
    if !st.auth.has_admin_password() {
        return Redirect::to("/setup").into_response();
    }
    render_no_store(&LoginTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        error: String::new(),
    })
}

pub async fn login_submit(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if !st.auth.has_admin_password() {
        return Redirect::to("/setup").into_response();
    }
    if form.password.is_empty() {
        return render_no_store_status(
            &LoginTemplate {
                version: server::VERSION.to_string(),
                now: now_label(),
                error: "请输入管理员密码".into(),
            },
            StatusCode::BAD_REQUEST,
        );
    }

    if st.auth.verify_admin_password(form.password).await {
        let cookie = auth::session_cookie(&st.auth.issue_session(), st.cookie_secure);
        session_redirect(cookie, "/")
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        render_no_store_status(
            &LoginTemplate {
                version: server::VERSION.to_string(),
                now: now_label(),
                error: "密码错误".into(),
            },
            StatusCode::UNAUTHORIZED,
        )
    }
}

pub async fn logout(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    session_redirect(auth::clear_session_cookie(st.cookie_secure), "/login")
}

// ------------------------- settings / token -------------------------

fn panel_origin(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = st
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        return url.trim_end_matches('/').to_string();
    }
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        return format!("http://{host}");
    }
    st.base_url.clone()
}

fn settings_template(
    st: &AppState,
    headers: &HeaderMap,
    rotated: bool,
    reset: bool,
    error: String,
) -> SettingsTemplate {
    let controls = crate::service::controls(&st.base_url);
    let binding_command = controls
        .pointer("/start/binding_command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    SettingsTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "settings".into(),
        base_url: st.base_url.clone(),
        panel_origin: panel_origin(st, headers),
        token_on: st.token_required(),
        token: st.api_token().unwrap_or_default(),
        token_source: st.token_source().label().to_string(),
        token_updated_at: st.token_updated_at().unwrap_or_else(|| "—".to_string()),
        binding_command,
        env_token_present: st
            .env_token
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty()),
        env_password_managed: st.auth.env_password_managed(),
        rotated,
        reset,
        error,
    }
}

pub async fn settings_page(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SettingsQuery>,
) -> Response {
    render_no_store(&settings_template(
        &st,
        &headers,
        q.rotated.is_some(),
        q.reset.is_some(),
        String::new(),
    ))
}

pub async fn action_token_rotate(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    let _guard = st.auth.token_lock.lock().await;
    let token = auth::random_token();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    if let Err(e) = st.db.set_setting(auth::API_TOKEN_KEY, &token).await {
        return render_no_store_status(
            &settings_template(&st, &headers, false, false, format!("保存 token 失败: {e}")),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    if let Err(e) = st.db.set_setting(auth::API_TOKEN_UPDATED_KEY, &now).await {
        tracing::warn!("persist token update time failed: {e}");
    }
    st.set_api_token(Some(token), auth::TokenSource::Database);
    st.set_token_updated_at(Some(now));
    Redirect::to("/settings?rotated=1").into_response()
}

pub async fn action_token_reset_env(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    let _guard = st.auth.token_lock.lock().await;
    if let Err(e) = st.db.delete_setting(auth::API_TOKEN_KEY).await {
        return render_no_store_status(
            &settings_template(&st, &headers, false, false, format!("回退 token 失败: {e}")),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    let _ = st.db.delete_setting(auth::API_TOKEN_UPDATED_KEY).await;
    let api = auth::resolve_api_token(None, st.env_token.clone());
    st.set_api_token(api.value, api.source);
    st.set_token_updated_at(None);
    Redirect::to("/settings?reset=1").into_response()
}

// ------------------------- panel tests -------------------------

async fn build_tests(st: &Arc<AppState>, focus_id: String, error: String) -> TestsTemplate {
    let recent = st
        .db
        .recent(100)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.source == "panel" && (r.capability == "wechat" || r.capability == "xhs"))
        .take(20)
        .map(|r| request_view(&r))
        .collect();

    TestsTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "tests".into(),
        dry_run: st.dry_run,
        paused: st.is_paused(),
        focus_id,
        error,
        recent,
    }
}

pub async fn tests_page(State(st): State<Arc<AppState>>, Query(q): Query<TestsQuery>) -> Response {
    let t = build_tests(&st, q.focus.unwrap_or_default(), String::new()).await;
    render_no_store(&t)
}

pub async fn action_test_wechat(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<WechatTestForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if st.is_paused() {
        let t = build_tests(&st, String::new(), "服务已暂停，无法发送测试消息".into()).await;
        return render_no_store_status(&t, StatusCode::CONFLICT);
    }
    let simulate = form.simulate.is_some();
    let payload = serde_json::json!({
        "channel": "wechat",
        "message": &form.message,
        "simulate": simulate,
        "source": "panel",
    });
    match server::enqueue_notify(
        &st,
        server::NotifyEnqueue {
            channel: "wechat",
            title: Some("面板测试 · 微信"),
            message: &form.message,
            url: None,
            group: None,
            source: "panel",
            payload,
            dry_run: Some(simulate),
        },
    )
    .await
    {
        Ok(id) => Redirect::to(&format!("/tests?focus={id}")).into_response(),
        Err((code, message)) => {
            let t = build_tests(&st, String::new(), message).await;
            render_no_store_status(&t, code)
        }
    }
}

pub async fn action_test_xhs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<XhsTestForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if st.is_paused() {
        let t = build_tests(&st, String::new(), "服务已暂停，无法发送测试消息".into()).await;
        return render_no_store_status(&t, StatusCode::CONFLICT);
    }
    let simulate = form.simulate.is_some();
    let title = form.title.trim();
    let payload = serde_json::json!({
        "channel": "xhs",
        "group": &form.group,
        "title": &form.title,
        "message": &form.message,
        "url": &form.url,
        "simulate": simulate,
        "source": "panel",
    });
    match server::enqueue_notify(
        &st,
        server::NotifyEnqueue {
            channel: "xhs",
            title: if title.is_empty() {
                Some("面板测试 · 小红书")
            } else {
                Some(title)
            },
            message: &form.message,
            url: if form.url.trim().is_empty() {
                None
            } else {
                Some(form.url.as_str())
            },
            group: Some(form.group.as_str()),
            source: "panel",
            payload,
            dry_run: Some(simulate),
        },
    )
    .await
    {
        Ok(id) => Redirect::to(&format!("/tests?focus={id}")).into_response(),
        Err((code, message)) => {
            let t = build_tests(&st, String::new(), message).await;
            render_no_store_status(&t, code)
        }
    }
}

pub async fn test_status_fragment(
    State(st): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Response {
    let row = match st.db.get(&q.id).await {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::NOT_FOUND, "request not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let (status_text, status_class) = status_labels(&row.status);
    let t = TestStatusTemplate {
        id: row.id.clone(),
        status_text: status_text.into(),
        status_class: status_class.into(),
        running: matches!(row.status.as_str(), "queued" | "running"),
        duration: row
            .duration_ms
            .map(|ms| {
                if ms < 1000 {
                    format!("{ms}ms")
                } else {
                    format!("{:.1}s", ms as f64 / 1000.0)
                }
            })
            .unwrap_or_else(|| "—".into()),
        summary: row.summary.clone(),
        error: row.error.clone().unwrap_or_default(),
    };
    render_no_store(&t)
}

fn status_labels(status: &str) -> (&'static str, &'static str) {
    match status {
        "ok" => ("成功", "ok"),
        "failed" => ("失败", "bad"),
        "running" => ("执行中", "warn"),
        "queued" => ("排队中", "flat"),
        _ => ("未知", "flat"),
    }
}

// ------------------------- lock demo -------------------------

fn command_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn demo_ready(st: &AppState) -> bool {
    if st.demo_cfg.command_override.is_some() {
        return true;
    }
    if !st.demo_cfg.script_path.is_file() {
        return false;
    }
    if let Some(python) = &st.demo_cfg.python {
        return python.is_file() || command_on_path(&python.to_string_lossy());
    }
    st.demo_cfg.booker_dir.join(".venv/bin/python").is_file() || command_on_path("uv")
}

async fn build_demo(st: &Arc<AppState>, error: String) -> DemoTemplate {
    let recent = st
        .db
        .recent(100)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.capability == "lock_demo")
        .take(10)
        .map(|r| request_view(&r))
        .collect();
    let cfg = &st.demo_cfg;
    DemoTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "demo".into(),
        city_id: cfg.city_id,
        cinema_id: cfg.cinema_id.clone().unwrap_or_default(),
        movie_id: cfg.movie_id.clone().unwrap_or_default(),
        num_seats: cfg.default_num_seats.max(1),
        imax_only: false,
        running: st.demo.is_running(),
        demo_ready: demo_ready(st),
        script_path: cfg.script_path.display().to_string(),
        screenshot_dir: cfg.screenshot_dir.display().to_string(),
        error,
        recent,
    }
}

pub async fn demo_page(State(st): State<Arc<AppState>>) -> Response {
    let t = build_demo(&st, String::new()).await;
    render_no_store(&t)
}

pub async fn action_demo_start(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DemoStartForm>,
) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    if st.demo.is_running() || st.demo_queue.load(Ordering::SeqCst) > 0 {
        let t = build_demo(&st, "已有演示在进行，请先停止或等待结束".into()).await;
        return render_no_store_status(&t, StatusCode::CONFLICT);
    }
    if !demo_ready(&st) {
        let t = build_demo(
            &st,
            format!(
                "演示未就绪：需要 {}，以及 maoyan-booker 的 Python/Playwright 环境（或 uv）",
                st.demo_cfg.script_path.display()
            ),
        )
        .await;
        return render_no_store_status(&t, StatusCode::SERVICE_UNAVAILABLE);
    }

    let city_id = form
        .city_id
        .trim()
        .parse::<u32>()
        .unwrap_or(st.demo_cfg.city_id)
        .max(1);
    let manual_cinema = if form.cinema_id.trim().is_empty() {
        st.demo_cfg.cinema_id.clone()
    } else {
        Some(form.cinema_id.trim().to_string())
    };
    let (cinema_id, cinema_name) = match manual_cinema {
        Some(id) => (id.clone(), format!("影院 {id}")),
        None => match maoyan::random_cinema(&st.http, city_id).await {
            Ok(cinema) => (cinema.id, cinema.name),
            Err(e) => {
                let t = build_demo(&st, format!("随机选择影院失败: {e}")).await;
                return render_no_store_status(&t, StatusCode::BAD_GATEWAY);
            }
        },
    };
    let movie_id = if form.movie_id.trim().is_empty() {
        st.demo_cfg.movie_id.clone()
    } else {
        Some(form.movie_id.trim().to_string())
    };
    let num_seats = form
        .num_seats
        .trim()
        .parse::<u64>()
        .unwrap_or(st.demo_cfg.default_num_seats)
        .clamp(1, 6);
    let imax_only = form.imax_only.is_some();

    let id = server::new_id();
    let title = format!("演示选座 · {cinema_name}");
    let preview = format!(
        "city={city_id} · 影院={cinema_id} · {num_seats} 座{} · 不提交订单",
        if imax_only { " · IMAX" } else { "" }
    );
    let payload = serde_json::json!({
        "kind": "lock_demo",
        "city_id": city_id,
        "cinema_id": cinema_id,
        "cinema_name": cinema_name,
        "movie_id": movie_id.clone(),
        "num_seats": num_seats,
        "imax_only": imax_only,
        "order_submitted": false,
    });
    if let Err(e) = st
        .db
        .insert_queued(&id, "lock_demo", "panel", &title, &preview, &payload)
        .await
    {
        let t = build_demo(&st, format!("记录演示请求失败: {e}")).await;
        return render_no_store_status(&t, StatusCode::INTERNAL_SERVER_ERROR);
    }

    let job = DemoJob {
        request_id: id.clone(),
        cinema_id,
        cinema_name,
        movie_id,
        num_seats,
        imax_only,
    };
    if st.demo_tx.try_send(job).is_err() {
        let _ = st.db.fail_queued(&id, "demo queue full").await;
        let t = build_demo(&st, "演示队列已满，请等当前演示结束".into()).await;
        return render_no_store_status(&t, StatusCode::TOO_MANY_REQUESTS);
    }
    st.demo_queue.fetch_add(1, Ordering::SeqCst);
    Redirect::to("/demo").into_response()
}

pub async fn action_demo_stop(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !auth::same_origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request").into_response();
    }
    st.demo.request_stop();
    Redirect::to("/demo").into_response()
}

pub async fn demo_status_fragment(State(st): State<Arc<AppState>>) -> Response {
    let snapshot = st.demo.snapshot();
    let (db_status, db_status_class, db_summary, db_error) = if snapshot.request_id.is_empty() {
        (
            "—".to_string(),
            "flat".to_string(),
            String::new(),
            String::new(),
        )
    } else {
        match st.db.get(&snapshot.request_id).await {
            Ok(Some(row)) => {
                let (text, class) = status_labels(&row.status);
                (
                    text.to_string(),
                    class.to_string(),
                    row.summary,
                    row.error.unwrap_or_default(),
                )
            }
            _ => (
                "—".to_string(),
                "flat".to_string(),
                String::new(),
                String::new(),
            ),
        }
    };
    let has_screenshot = st.demo_cfg.screenshot_dir.join("latest.png").is_file();
    let elapsed = format_demo_elapsed(snapshot.elapsed_secs());
    let t = DemoStatusTemplate {
        running: snapshot.running,
        phase: snapshot.phase,
        cinema_name: snapshot.cinema_name,
        request_id: snapshot.request_id,
        elapsed,
        last_line: snapshot.last_line,
        db_status,
        db_status_class,
        db_summary,
        db_error,
        has_screenshot,
        screenshot_version: server::now_secs() as u64,
    };
    render_no_store(&t)
}

pub async fn demo_logs_fragment(State(st): State<Arc<AppState>>) -> Response {
    let t = DemoLogsTemplate {
        logs: st.demo.logs(),
    };
    render_no_store(&t)
}

pub async fn demo_screenshot(State(st): State<Arc<AppState>>) -> Response {
    let path = st.demo_cfg.screenshot_dir.join("latest.png");
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "screenshot not found").into_response(),
    }
}

fn format_demo_elapsed(secs: u64) -> String {
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

pub async fn requests_page(State(st): State<Arc<AppState>>) -> Response {
    let requests = st
        .db
        .recent(200)
        .await
        .unwrap_or_default()
        .iter()
        .map(request_view)
        .collect::<Vec<_>>();
    let t = RequestsTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "requests".into(),
        base_url: st.display_base_url(),
        token_on: st.token_required(),
        paused: st.is_paused(),
        count: requests.len(),
        requests,
    };
    render(&t).into_response()
}

pub async fn update_page() -> Response {
    let info = crate::update::check().await;
    let t = UpdateTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "update".into(),
        info,
    };
    render(&t).into_response()
}

pub async fn action_update_install() -> Response {
    match crate::update::install_latest().await {
        Ok(out) => {
            crate::update::schedule_exit();
            let t = UpdateResultTemplate {
                version: server::VERSION.to_string(),
                now: now_label(),
                page: "update".into(),
                ok: true,
                title: format!("已更新到 {}", out.version),
                message: if out.supervised {
                    "服务正在由 launchd 重启；几秒后刷新页面。".into()
                } else {
                    "已替换二进制，重启助手会在当前进程退出后拉起新版本；几秒后刷新页面。".into()
                },
            };
            render(&t).into_response()
        }
        Err(e) => {
            let t = UpdateResultTemplate {
                version: server::VERSION.to_string(),
                now: now_label(),
                page: "update".into(),
                ok: false,
                title: "更新失败".into(),
                message: e.to_string(),
            };
            render(&t).into_response()
        }
    }
}

async fn build_dashboard(
    st: &Arc<AppState>,
    webhook_notice: String,
) -> anyhow::Result<DashboardTemplate> {
    let stats = st.db.stats_24h().await.unwrap_or_default();
    let cap_stats = st.db.capability_stats().await.unwrap_or_default();
    let recent = st.db.recent(12).await.unwrap_or_default();
    let all_total = st.db.total_count().await.unwrap_or(0);
    let (checks, ready) = st.diagnostics();
    let (notify_q, lock_q) = st.queues();
    let lock_webhook = st
        .db
        .get_setting(discord::WEBHOOK_KEY)
        .await
        .unwrap_or(None)
        .unwrap_or_default();

    let stat_map: HashMap<String, CapabilityStat> = cap_stats
        .into_iter()
        .map(|s| (s.capability.clone(), s))
        .collect();

    type CapDef = (
        &'static str,
        &'static str,
        &'static str,
        bool,
        &'static [&'static str],
        usize,
    );
    let cap_defs: [CapDef; 3] = [
        (
            "wechat",
            "微信（本机）",
            "发到当前打开的微信会话",
            ready.wechat,
            &["macos", "python3", "wechat_script", "wechat_app"],
            notify_q,
        ),
        (
            "xhs",
            "小红书（本机）",
            "发到指定群聊",
            ready.xhs,
            &[
                "macos",
                "python3",
                "xhs_script",
                "agent_browser",
                "xhs_profile",
            ],
            notify_q,
        ),
        (
            "lock",
            "自动锁票",
            "maoyan-booker 真锁",
            ready.lock,
            &["macos", "uv", "booker", "lock_script", "booker_profile"],
            lock_q,
        ),
    ];
    let mut caps = Vec::with_capacity(cap_defs.len());
    for (id, label, subtitle, ready, ids, queue) in cap_defs {
        let stat = stat_map.get(id).cloned().unwrap_or_default();
        let selected = capabilities::checks_for(&checks, ids);
        let mut check_views: Vec<CheckView> = selected
            .into_iter()
            .map(|c| CheckView {
                label: c.label.clone(),
                ok: c.ok,
                detail: c.detail.clone(),
                action: String::new(),
            })
            .collect();
        if id == "lock" {
            let webhook_on = !lock_webhook.trim().is_empty();
            check_views.push(CheckView {
                label: "Discord 通知".into(),
                ok: webhook_on,
                detail: if webhook_on { "已配置" } else { "未配置" }.into(),
                action: if webhook_on { "修改" } else { "配置" }.into(),
            });
        }
        let checks_ok = check_views.iter().filter(|c| c.ok).count();
        let checks_total = check_views.len();
        let last_error = st.db.last_for_capability(id, false).await.ok().flatten();
        caps.push(CapabilityView {
            id: id.to_string(),
            label: label.to_string(),
            subtitle: subtitle.to_string(),
            ready,
            status_text: if ready { "就绪" } else { "未就绪" }.into(),
            status_class: if ready { "ok" } else { "bad" }.into(),
            checks: check_views,
            checks_ok,
            checks_total,
            requests_24h: stat.total,
            ok_24h: stat.ok,
            failed_24h: stat.failed,
            queue,
            last_at: stat.last_at.unwrap_or_else(|| "—".into()),
            last_error: last_error
                .and_then(|r| r.error)
                .map(|e| truncate(e.as_str(), 140))
                .unwrap_or_default(),
        });
    }

    let stats = StatsView {
        total: stats.total,
        ok: stats.ok,
        failed: stats.failed,
        running: stats.running,
        queued: stats.queued,
        success_rate: if stats.total > 0 {
            format!("{:.1}%", stats.ok as f64 * 100.0 / stats.total as f64)
        } else {
            "—".into()
        },
        all_total,
    };
    let caps_ready = caps.iter().filter(|c| c.ready).count();
    let caps_total = caps.len();

    Ok(DashboardTemplate {
        version: server::VERSION.to_string(),
        now: now_label(),
        page: "overview".into(),
        uptime: format_uptime((server::now_secs() - st.started_at).max(0.0) as u64),
        base_url: st.display_base_url(),
        token_on: st.token_required(),
        token_source: st.token_source().label().to_string(),
        lock_webhook_on: !lock_webhook.trim().is_empty(),
        lock_webhook,
        webhook_notice,
        dry_run: st.dry_run,
        paused: st.is_paused(),
        stats,
        caps_ready,
        caps_total,
        capabilities: caps,
        requests: recent.iter().map(request_view).collect(),
    })
}

fn request_view(r: &RequestRow) -> RequestView {
    let (status_text, status_class) = status_labels(&r.status);
    let capability = match r.capability.as_str() {
        "wechat" => "微信",
        "xhs" => "小红书",
        "lock" => "锁票",
        "lock_demo" => "演示选座",
        other => other,
    };
    RequestView {
        id: r.id.clone(),
        time: short_time(&r.received_local),
        capability: capability.into(),
        capability_class: r.capability.clone(),
        status_text: status_text.into(),
        status_class: status_class.into(),
        duration: r
            .duration_ms
            .map(|ms| {
                if ms < 1000 {
                    format!("{ms}ms")
                } else {
                    format!("{:.1}s", ms as f64 / 1000.0)
                }
            })
            .unwrap_or_else(|| "—".into()),
        source: r.source.clone(),
        title: r.title.clone(),
        preview: r.preview.clone(),
        summary: truncate(&r.summary, 160),
        show_summary: r.status == "ok" && !r.summary.is_empty(),
        error: r
            .error
            .clone()
            .map(|e| truncate(&e, 160))
            .unwrap_or_default(),
    }
}

fn short_time(s: &str) -> String {
    // stored as local "YYYY-MM-DD HH:MM:SS"
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 19 {
        chars[5..].iter().collect()
    } else {
        s.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn now_label() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

fn format_uptime(sec: u64) -> String {
    let d = sec / 86_400;
    let h = (sec % 86_400) / 3_600;
    let m = (sec % 3_600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn render<T: Template>(t: &T) -> Html<String> {
    match t.render() {
        Ok(s) => Html(s),
        Err(e) => Html(format!(
            "<pre style=\"padding:24px;font-family:monospace\">template error: {e}</pre>"
        )),
    }
}
