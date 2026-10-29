//! HTTP service, request queues and shared state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::auth::{self, ApiToken, AuthState, TokenSource};
use crate::capabilities::{self, Check, Ready};
use crate::db::{CapabilityStat, Db, RequestRow, Stats24h};
use crate::demo::{self, DemoConfig, DemoJob, DemoShared};
use crate::discord;
use crate::lock::{self, LockArgs, LockResult};
use crate::notify;
use crate::web;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct ServerConfig {
    pub bind: String,
    pub db: String,
    pub token: Option<String>,
    /// Optional panel admin password reset, read from GOFER_ADMIN_PASSWORD.
    pub admin_password: Option<String>,
    pub public_url: Option<String>,
    pub cookie_secure: bool,
    pub dry_run: bool,
    pub lock_headless: bool,
    pub retention_days: u32,
    pub demo_city_id: u32,
    pub demo_cinema_id: Option<String>,
    pub demo_movie_id: Option<String>,
    pub demo_timeout_secs: u64,
    pub demo_python: Option<String>,
    pub demo_cmd: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8790".into(),
            db: default_db_path(),
            token: None,
            admin_password: None,
            public_url: None,
            cookie_secure: false,
            dry_run: false,
            lock_headless: true,
            retention_days: 30,
            demo_city_id: 10,
            demo_cinema_id: None,
            demo_movie_id: None,
            demo_timeout_secs: 600,
            demo_python: None,
            demo_cmd: None,
        }
    }
}

pub fn default_state_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        });
    base.join("gofer")
}

pub fn default_db_path() -> String {
    default_state_dir().join("gofer.db").display().to_string()
}

pub struct AppState {
    pub db: Db,
    pub started_at: f64,
    pub base_url: String,
    pub public_url: Option<String>,
    pub cookie_secure: bool,
    pub api: RwLock<ApiToken>,
    pub env_token: Option<String>,
    pub token_updated_at: RwLock<Option<String>>,
    pub auth: Arc<AuthState>,
    pub dry_run: bool,
    pub lock_headless: bool,
    pub notify_tx: mpsc::Sender<NotifyJob>,
    pub lock_tx: mpsc::Sender<LockJob>,
    pub demo_tx: mpsc::Sender<DemoJob>,
    pub notify_queue: Arc<AtomicUsize>,
    pub lock_queue: Arc<AtomicUsize>,
    pub demo_queue: Arc<AtomicUsize>,
    pub demo: Arc<DemoShared>,
    pub demo_cfg: DemoConfig,
    pub browser_lock: Arc<tokio::sync::Mutex<()>>,
    pub http: reqwest::Client,
    pub paused: AtomicBool,
    pub stopping: AtomicBool,
}

pub struct AppStateInit {
    pub db: Db,
    pub api: ApiToken,
    pub env_token: Option<String>,
    pub token_updated_at: Option<String>,
    pub auth: Arc<AuthState>,
    pub dry_run: bool,
    pub lock_headless: bool,
    pub base_url: String,
    pub public_url: Option<String>,
    pub cookie_secure: bool,
    pub paused: bool,
    pub demo_cfg: DemoConfig,
}

#[derive(Debug, Clone)]
pub struct NotifyJob {
    pub request_id: String,
    pub channel: String,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub group: Option<String>,
    /// Resolved at enqueue time. Panel tests may override the global dry-run.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct LockJob {
    pub request_id: String,
    pub event_id: Option<String>,
    pub watch_id: Option<String>,
    pub movie_id: i64,
    pub movie_name: String,
    pub cinema_id: String,
    pub cinema_name: String,
    pub show_date: String,
    pub time_range: Option<String>,
    pub num_seats: u64,
    pub imax_only: bool,
    pub seats: Vec<String>,
    pub headless: bool,
    pub callback_url: Option<String>,
    pub callback_token: Option<String>,
}

impl AppState {
    pub fn new(init: AppStateInit) -> Arc<Self> {
        let AppStateInit {
            db,
            api,
            env_token,
            token_updated_at,
            auth,
            dry_run,
            lock_headless,
            base_url,
            public_url,
            cookie_secure,
            paused,
            demo_cfg,
        } = init;
        let (notify_tx, notify_rx) = mpsc::channel::<NotifyJob>(64);
        let (lock_tx, lock_rx) = mpsc::channel::<LockJob>(16);
        let (demo_tx, demo_rx) = mpsc::channel::<DemoJob>(2);
        let state = Arc::new(Self {
            db,
            started_at: now_secs(),
            base_url,
            public_url,
            cookie_secure,
            api: RwLock::new(api),
            env_token,
            token_updated_at: RwLock::new(token_updated_at),
            auth,
            dry_run,
            lock_headless,
            notify_tx,
            lock_tx,
            demo_tx,
            notify_queue: Arc::new(AtomicUsize::new(0)),
            lock_queue: Arc::new(AtomicUsize::new(0)),
            demo_queue: Arc::new(AtomicUsize::new(0)),
            demo: Arc::new(DemoShared::default()),
            demo_cfg,
            browser_lock: Arc::new(tokio::sync::Mutex::new(())),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("gofer http client"),
            paused: AtomicBool::new(paused),
            stopping: AtomicBool::new(false),
        });
        tokio::spawn(notify_worker(state.clone(), notify_rx));
        tokio::spawn(lock_worker(state.clone(), lock_rx));
        tokio::spawn(demo::demo_worker(state.clone(), demo_rx));
        state
    }

    pub fn diagnostics(&self) -> (Vec<Check>, Ready) {
        capabilities::diagnostics()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub fn queues(&self) -> (usize, usize) {
        (
            self.notify_queue.load(Ordering::SeqCst),
            self.lock_queue.load(Ordering::SeqCst),
        )
    }

    pub fn api_token(&self) -> Option<String> {
        self.api
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .clone()
    }

    pub fn token_required(&self) -> bool {
        self.api.read().unwrap_or_else(|e| e.into_inner()).is_on()
    }

    pub fn token_source(&self) -> TokenSource {
        self.api.read().unwrap_or_else(|e| e.into_inner()).source
    }

    pub fn set_api_token(&self, value: Option<String>, source: TokenSource) {
        *self.api.write().unwrap_or_else(|e| e.into_inner()) = ApiToken { value, source };
    }

    pub fn token_updated_at(&self) -> Option<String> {
        self.token_updated_at
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_token_updated_at(&self, value: Option<String>) {
        *self
            .token_updated_at
            .write()
            .unwrap_or_else(|e| e.into_inner()) = value;
    }

    pub fn display_base_url(&self) -> String {
        self.public_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| self.base_url.clone())
    }

    pub async fn stats(&self) -> Result<(Stats24h, Vec<CapabilityStat>, Vec<RequestRow>)> {
        let stats = self.db.stats_24h().await?;
        let caps = self.db.capability_stats().await?;
        let recent = self.db.recent(30).await?;
        Ok((stats, caps, recent))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/", get(web::dashboard))
        .route("/requests", get(web::requests_page))
        .route("/settings", get(web::settings_page))
        .route("/tests", get(web::tests_page))
        .route("/demo", get(web::demo_page))
        .route("/update", get(web::update_page))
        .route("/actions/service/toggle", post(toggle_service))
        .route("/actions/update/install", post(web::action_update_install))
        .route("/actions/lock/webhook", post(web::action_lock_webhook))
        .route(
            "/actions/lock/webhook/test",
            post(web::action_lock_webhook_test),
        )
        .route("/actions/token/rotate", post(web::action_token_rotate))
        .route(
            "/actions/token/reset-env",
            post(web::action_token_reset_env),
        )
        .route("/actions/test/wechat", post(web::action_test_wechat))
        .route("/actions/test/xhs", post(web::action_test_xhs))
        .route("/actions/demo/start", post(web::action_demo_start))
        .route("/actions/demo/stop", post(web::action_demo_stop))
        .route("/tests/status", get(web::test_status_fragment))
        .route("/demo/status", get(web::demo_status_fragment))
        .route("/demo/logs", get(web::demo_logs_fragment))
        .route("/demo/screenshot", get(web::demo_screenshot))
        .route("/logout", post(web::logout))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));

    Router::new()
        .merge(protected)
        .route("/login", get(web::login_page).post(web::login_submit))
        .route("/setup", get(web::setup_page).post(web::setup_submit))
        .route("/health", get(health))
        .route("/api/v1/health", get(health))
        .route("/api/v1/capabilities", get(capabilities_api))
        .route("/api/v1/requests", get(requests_api))
        .route("/v1/requests", get(requests_api))
        .route("/api/v1/requests/:id", get(request_api))
        .route("/v1/requests/:id", get(request_api))
        .route("/api/v1/stats", get(stats_api))
        .route("/v1/stats", get(stats_api))
        .route("/api/v1/service", get(service_api))
        .route("/v1/service", get(service_api))
        .route("/api/v1/service/memory", get(service_memory_api))
        .route("/v1/service/memory", get(service_memory_api))
        .route("/api/v1/service/pause", post(service_pause))
        .route("/v1/service/pause", post(service_pause))
        .route("/api/v1/service/resume", post(service_resume))
        .route("/v1/service/resume", post(service_resume))
        .route("/api/v1/service/toggle", post(service_toggle))
        .route("/v1/service/toggle", post(service_toggle))
        .route("/api/v1/service/stop", post(service_stop))
        .route("/v1/service/stop", post(service_stop))
        .route("/notify", post(notify_handler))
        .route("/v1/notify", post(notify_handler))
        .route("/lock", post(lock_handler))
        .route("/v1/lock", post(lock_handler))
        .nest_service(
            "/static",
            tower_http::services::ServeDir::new(web::web_dir()),
        )
        .with_state(state)
}

// ------------------------- API types -------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotifyRequest {
    pub channel: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockRequest {
    #[serde(default)]
    pub movie_id: i64,
    #[serde(default)]
    pub movie_name: String,
    #[serde(default)]
    pub cinema_id: String,
    #[serde(default)]
    pub cinema_name: String,
    #[serde(default)]
    pub show_date: String,
    #[serde(default)]
    pub time_range: Option<String>,
    #[serde(default = "default_num_seats")]
    pub num_seats: u64,
    #[serde(default)]
    pub imax_only: bool,
    #[serde(default)]
    pub seats: Vec<String>,
    #[serde(default)]
    pub headless: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub watch_id: Option<String>,
    #[serde(default)]
    pub callback_url: Option<String>,
}

fn default_num_seats() -> u64 {
    2
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct StopQuery {
    /// Close the Xiaohongshu browser and terminate Gofer's workers (default true).
    #[serde(default)]
    cleanup: Option<bool>,
    /// Run `agent-browser close` (default: same as `cleanup`).
    #[serde(default)]
    close_browser: Option<bool>,
}

// ------------------------- handlers -------------------------

async fn health(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let (checks, ready) = st.diagnostics();
    let (notify_q, lock_q) = st.queues();
    let demo_q = st.demo_queue.load(Ordering::SeqCst);
    Json(json!({
        "ok": true,
        "service": "gofer",
        "version": VERSION,
        "uptime_sec": (now_secs() - st.started_at).max(0.0) as u64,
        "dry_run": st.dry_run,
        "paused": st.is_paused(),
        "token_required": st.token_required(),
        "token_source": st.token_source().label(),
        "queue": {"notify": notify_q, "lock": lock_q, "demo": demo_q},
        "demo_running": st.demo.is_running(),
        "ready": ready,
        "checks": checks,
    }))
}

async fn toggle_service(State(st): State<Arc<AppState>>) -> Response {
    let next = !st.is_paused();
    st.set_paused(next);
    if let Err(e) = st.db.set_paused(next).await {
        tracing::warn!("persist paused state failed: {e}");
    }
    axum::response::Redirect::to("/").into_response()
}

async fn service_api(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    let (_, ready) = st.diagnostics();
    let memory = memory_snapshot().await;
    let (notify_q, lock_q) = st.queues();
    let display_url = st.display_base_url();
    let state = if st.is_stopping() {
        "stopping"
    } else if st.is_paused() {
        "paused"
    } else {
        "running"
    };
    Json(json!({
        "schema": "iglance.service.v1",
        "ok": true,
        "service": {
            "id": "gofer",
            "name": "Gofer",
            "version": VERSION,
            "kind": "headless-service",
        },
        "state": state,
        "paused": st.is_paused(),
        "stopping": st.is_stopping(),
        "pid": std::process::id(),
        "started_at": crate::service::started_at_iso(st.started_at),
        "uptime_sec": (now_secs() - st.started_at).max(0.0) as u64,
        "urls": {
            "base": st.base_url,
            "display": display_url,
            "health": format!("{}/health", display_url.trim_end_matches('/')),
        },
        "auth": {
            "header": "X-Gofer-Token",
            "token_required": st.token_required(),
        },
        "queue": {
            "notify": notify_q,
            "lock": lock_q,
            "demo": st.demo_queue.load(Ordering::SeqCst),
            "demo_running": st.demo.is_running(),
        },
        "ready": ready,
        "memory": memory,
        "controls": crate::service::controls(&st.base_url),
    }))
    .into_response()
}

async fn service_memory_api(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    Json(json!({
        "ok": true,
        "service": "gofer",
        "pid": std::process::id(),
        "memory": memory_snapshot().await,
    }))
    .into_response()
}

async fn service_pause(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    set_service_paused(&st, true).await;
    Json(json!({"ok": true, "state": "paused", "paused": true})).into_response()
}

async fn service_resume(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    set_service_paused(&st, false).await;
    Json(json!({"ok": true, "state": "running", "paused": false})).into_response()
}

async fn service_toggle(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    let paused = !st.is_paused();
    set_service_paused(&st, paused).await;
    let state = if paused { "paused" } else { "running" };
    Json(json!({"ok": true, "state": state, "paused": paused})).into_response()
}

async fn service_stop(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<StopQuery>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    if st.stopping.swap(true, Ordering::SeqCst) {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "state": "stopping", "already_stopping": true})),
        )
            .into_response();
    }

    let cleanup = q.cleanup.unwrap_or(true);
    let close_browser = q.close_browser.unwrap_or(cleanup);
    // Reject new work immediately, but do not persist the pause: after the
    // service is started again it should resume its previous paused state.
    st.set_paused(true);

    tokio::spawn(async move {
        // Give the HTTP response time to reach the client.
        tokio::time::sleep(Duration::from_millis(350)).await;
        if cleanup {
            crate::service::stop_cleanup(close_browser).await;
        }
        crate::service::remove_pid_file();
        crate::service::bootout_launchd().await;
        std::process::exit(0);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "state": "stopping",
            "cleanup": cleanup,
            "close_browser": close_browser,
            "message": "Gofer 正在停止；完全停止后使用 controls.start 中的命令重新启动。",
        })),
    )
        .into_response()
}

async fn set_service_paused(st: &Arc<AppState>, paused: bool) {
    st.set_paused(paused);
    if let Err(e) = st.db.set_paused(paused).await {
        tracing::warn!("persist paused state failed: {e}");
    }
}

async fn memory_snapshot() -> crate::service::MemorySnapshot {
    match tokio::task::spawn_blocking(crate::service::collect_memory).await {
        Ok(snapshot) => snapshot,
        Err(e) => crate::service::MemorySnapshot::unavailable(format!("内存采集任务失败: {e}")),
    }
}

async fn capabilities_api(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let (checks, ready) = st.diagnostics();
    Json(json!({"ready": ready, "checks": checks}))
}

async fn requests_api(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    match st.db.recent(q.limit.unwrap_or(50)).await {
        Ok(rows) => Json(json!({"requests": rows})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn request_api(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    match st.db.get(&id).await {
        Ok(Some(row)) => Json(json!({"request": row})).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "request not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn stats_api(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    match st.stats().await {
        Ok((stats, caps, _)) => Json(json!({"stats": stats, "capabilities": caps})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(crate) struct NotifyEnqueue<'a> {
    pub channel: &'a str,
    pub title: Option<&'a str>,
    pub message: &'a str,
    pub url: Option<&'a str>,
    pub group: Option<&'a str>,
    pub source: &'a str,
    pub payload: Value,
    /// None = use the global GOFER_DRY_RUN. Some(true/false) overrides it for
    /// this job (used by the panel test forms).
    pub dry_run: Option<bool>,
}

pub(crate) async fn enqueue_notify(
    st: &Arc<AppState>,
    req: NotifyEnqueue<'_>,
) -> Result<String, (StatusCode, String)> {
    let channel = req.channel.trim().to_lowercase();
    if channel != "wechat" && channel != "xhs" {
        return Err((
            StatusCode::BAD_REQUEST,
            "channel must be wechat or xhs".into(),
        ));
    }
    if req.message.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message is required".into()));
    }
    let group = req
        .group
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(String::from);
    if channel == "xhs" && group.is_none() {
        return Err((StatusCode::BAD_REQUEST, "xhs requires group".into()));
    }
    let title = req
        .title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            if channel == "xhs" {
                "Gofer · 小红书"
            } else {
                "Gofer · 微信"
            }
            .into()
        });
    let id = new_id();
    let preview = preview_text(req.message);
    st.db
        .insert_queued(&id, &channel, req.source, &title, &preview, &req.payload)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let job = NotifyJob {
        request_id: id.clone(),
        channel: channel.clone(),
        title,
        message: req.message.to_string(),
        url: req.url.map(String::from),
        group,
        dry_run: req.dry_run.unwrap_or(st.dry_run),
    };
    if st.notify_tx.try_send(job).is_err() {
        let _ = st.db.fail_queued(&id, "notify queue full").await;
        return Err((StatusCode::TOO_MANY_REQUESTS, "notify queue full".into()));
    }
    st.notify_queue.fetch_add(1, Ordering::SeqCst);
    Ok(id)
}

async fn notify_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<NotifyRequest>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    if st.is_paused() {
        return (StatusCode::SERVICE_UNAVAILABLE, "gofer is paused").into_response();
    }
    let channel = req.channel.trim().to_lowercase();
    let group = req.group.clone().or_else(|| {
        req.config
            .as_ref()
            .and_then(|c| c.get("group").or_else(|| c.get("xhs")))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    let source = request_source(&headers, req.source.as_deref());
    let payload = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));
    match enqueue_notify(
        &st,
        NotifyEnqueue {
            channel: &channel,
            title: req.title.as_deref(),
            message: &req.message,
            url: req.url.as_deref(),
            group: group.as_deref(),
            source: &source,
            payload,
            dry_run: None,
        },
    )
    .await
    {
        Ok(id) => (
            StatusCode::ACCEPTED,
            Json(json!({"request_id": id, "status": "queued", "capability": channel})),
        )
            .into_response(),
        Err((code, message)) => (code, message).into_response(),
    }
}

async fn lock_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LockRequest>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    if st.is_paused() {
        return (StatusCode::SERVICE_UNAVAILABLE, "gofer is paused").into_response();
    }
    if req.cinema_id.trim().is_empty() || req.show_date.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "cinema_id and show_date are required",
        )
            .into_response();
    }
    let source = request_source(&headers, req.source.as_deref());
    let id = new_id();
    let title = if req.movie_name.trim().is_empty() {
        format!("锁票 · {} {}", req.cinema_id, req.show_date)
    } else {
        format!("锁票 · {} · {}", req.movie_name, req.cinema_id)
    };
    let preview = format!(
        "{} {} {} 座{}",
        req.show_date,
        req.time_range.as_deref().unwrap_or("不限"),
        req.num_seats.max(1),
        if req.imax_only { " · IMAX" } else { "" }
    );
    let payload = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));
    if let Err(e) = st
        .db
        .insert_queued(&id, "lock", &source, &title, &preview, &payload)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let callback_token = request_token(&headers);
    let job = LockJob {
        request_id: id.clone(),
        event_id: req.event_id,
        watch_id: req.watch_id,
        movie_id: req.movie_id,
        movie_name: req.movie_name,
        cinema_id: req.cinema_id,
        cinema_name: req.cinema_name,
        show_date: req.show_date,
        time_range: req.time_range,
        num_seats: req.num_seats.max(1),
        imax_only: req.imax_only,
        seats: req.seats,
        headless: req.headless.unwrap_or(st.lock_headless),
        callback_url: req.callback_url,
        callback_token,
    };
    if st.lock_tx.try_send(job).is_err() {
        let _ = st.db.fail_queued(&id, "lock queue full").await;
        return (StatusCode::TOO_MANY_REQUESTS, "lock queue full").into_response();
    }
    st.lock_queue.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::ACCEPTED,
        Json(json!({"request_id": id, "status": "queued", "capability": "lock"})),
    )
        .into_response()
}

fn auth(st: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let token = st.api.read().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(expected) = token.label() else {
        return Ok(());
    };
    let direct = ["x-gofer-token", "x-agent-token"]
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|v| v.to_str().ok())
        .fold(false, |acc, v| acc | auth::token_matches(expected, v));
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| auth::token_matches(expected, v));
    if direct || bearer {
        Ok(())
    } else {
        Err(Box::new(
            (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response(),
        ))
    }
}

fn request_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gofer-token")
        .or_else(|| headers.get("x-agent-token"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(String::from)
        })
}

fn request_source(headers: &HeaderMap, body: Option<&str>) -> String {
    body.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            headers
                .get("x-gofer-source")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "api".into())
}

pub(crate) fn new_id() -> String {
    format!("g_{}", &uuid::Uuid::new_v4().to_string()[..8])
}

fn preview_text(message: &str) -> String {
    let one_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 160 {
        format!("{}…", one_line.chars().take(160).collect::<String>())
    } else {
        one_line
    }
}

// ------------------------- workers -------------------------

async fn notify_worker(st: Arc<AppState>, mut rx: mpsc::Receiver<NotifyJob>) {
    while let Some(job) = rx.recv().await {
        st.notify_queue.fetch_sub(1, Ordering::SeqCst);
        let _ = st.db.mark_running(&job.request_id).await;
        let started = Instant::now();
        let dry = job.dry_run;
        let job2 = job.clone();
        let result = tokio::task::spawn_blocking(move || execute_notify(&job2, dry))
            .await
            .unwrap_or_else(|e| Err(format!("worker join 错误: {e}")));
        let duration = started.elapsed().as_millis() as i64;
        match result {
            Ok(summary) => {
                let _ = st
                    .db
                    .finish(
                        &job.request_id,
                        true,
                        duration,
                        &summary,
                        Some(&json!({"kind": "notify", "dry_run": dry})),
                        None,
                    )
                    .await;
            }
            Err(e) => {
                let _ = st
                    .db
                    .finish(&job.request_id, false, duration, "发送失败", None, Some(&e))
                    .await;
            }
        }
    }
}

fn execute_notify(job: &NotifyJob, dry_run: bool) -> Result<String, String> {
    let text = if job.title.is_empty() {
        job.message.clone()
    } else {
        format!("{}\n\n{}", job.title, job.message)
    };
    match job.channel.as_str() {
        "wechat" => {
            if dry_run {
                Ok(format!("[dry-run] 微信: {}", text.replace('\n', " / ")))
            } else {
                notify::notify_wechat(&text)
            }
        }
        "xhs" => {
            let group = job.group.as_deref().unwrap_or("").trim();
            if group.is_empty() {
                return Err("小红书群名不能为空".into());
            }
            if dry_run {
                Ok(format!(
                    "[dry-run] 小红书 {group}: {}",
                    text.replace('\n', " / ")
                ))
            } else {
                notify::notify_xhs(group, &job.title, &job.message, job.url.as_deref())
            }
        }
        other => Err(format!("unknown channel: {other}")),
    }
}

async fn lock_worker(st: Arc<AppState>, mut rx: mpsc::Receiver<LockJob>) {
    while let Some(job) = rx.recv().await {
        st.lock_queue.fetch_sub(1, Ordering::SeqCst);
        let _ = st.db.mark_running(&job.request_id).await;
        // Demo and real locking share ~/.maoyan-booker-profile; serialize them.
        let browser_guard = st.browser_lock.lock().await;
        let started = Instant::now();
        let args = LockArgs {
            cinema_id: job.cinema_id.clone(),
            movie_id: job.movie_id,
            show_date: job.show_date.clone(),
            time_range: job.time_range.clone(),
            num_seats: job.num_seats.max(1),
            imax_only: job.imax_only,
            seats: job.seats.clone(),
            confirm: true,
            headless: job.headless,
        };
        let outcome = match lock::run_lock(&args).await {
            Ok(run) => run.complete().await,
            Err(e) => {
                let duration = started.elapsed().as_millis() as i64;
                let details = e.to_string();
                let _ = st
                    .db
                    .finish(
                        &job.request_id,
                        false,
                        duration,
                        "锁票失败",
                        None,
                        Some(&details),
                    )
                    .await;
                let st_discord = st.clone();
                let job_discord = job.clone();
                let elapsed = started.elapsed().as_secs_f64();
                tokio::spawn(async move {
                    notify_lock_discord(
                        &st_discord,
                        &job_discord,
                        "Error",
                        &format!("锁票失败: {details}"),
                        elapsed,
                    )
                    .await;
                });
                continue;
            }
        };
        let duration = outcome.elapsed.as_millis() as i64;
        let ok = outcome.result == LockResult::Ok;
        let code = lock_result_code(outcome.result);
        let summary = outcome.summary();
        let result = json!({
            "result": code,
            "ok": ok,
            "elapsed_sec": outcome.elapsed.as_secs_f64(),
            "stdout_tail": tail(&outcome.stdout, 3000),
            "stderr_tail": tail(&outcome.stderr, 2000),
        });
        let _ = st
            .db
            .finish(
                &job.request_id,
                ok,
                duration,
                &summary,
                Some(&result),
                if ok { None } else { Some(&summary) },
            )
            .await;
        drop(browser_guard);
        {
            let st_discord = st.clone();
            let job_discord = job.clone();
            let summary_discord = summary.clone();
            let elapsed = outcome.elapsed.as_secs_f64();
            tokio::spawn(async move {
                notify_lock_discord(&st_discord, &job_discord, code, &summary_discord, elapsed)
                    .await;
            });
        }
        post_lock_callback(&st, &job, code, &summary, outcome.elapsed.as_secs_f64()).await;
    }
}

async fn notify_lock_discord(
    st: &Arc<AppState>,
    job: &LockJob,
    code: &str,
    summary: &str,
    elapsed_sec: f64,
) {
    let webhook = match st.db.get_setting(discord::WEBHOOK_KEY).await {
        Ok(Some(url)) if !url.trim().is_empty() => url,
        _ => return,
    };
    let notice = discord::LockNotice {
        request_id: &job.request_id,
        cinema_id: &job.cinema_id,
        cinema_name: &job.cinema_name,
        movie_id: job.movie_id,
        movie_name: &job.movie_name,
        show_date: &job.show_date,
        code,
        summary,
        elapsed_sec,
    };
    if let Err(e) = discord::send_lock_result(&st.http, &webhook, &notice).await {
        tracing::warn!("lock discord notification failed: {e}");
    }
}

async fn post_lock_callback(
    st: &Arc<AppState>,
    job: &LockJob,
    code: &str,
    summary: &str,
    elapsed_sec: f64,
) {
    let Some(url) = job.callback_url.as_deref().filter(|u| !u.trim().is_empty()) else {
        return;
    };
    let body = json!({
        "event_id": job.event_id,
        "watch_id": job.watch_id,
        "movie_id": job.movie_id,
        "movie_name": job.movie_name,
        "cinema_id": job.cinema_id,
        "cinema_name": job.cinema_name,
        "show_date": job.show_date,
        "result": code,
        "summary": summary,
        "elapsed_sec": elapsed_sec,
    });
    for attempt in 0..3 {
        let mut req = st.http.post(url).json(&body);
        if let Some(token) = job.callback_token.as_deref() {
            req = req.header("X-Agent-Token", token);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => tracing::warn!("lock callback HTTP {} ({})", r.status(), url),
            Err(e) => tracing::warn!("lock callback error: {e} ({url})"),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

fn lock_result_code(r: LockResult) -> &'static str {
    match r {
        LockResult::Ok => "Ok",
        LockResult::DryRun => "DryRun",
        LockResult::ShowGone => "ShowGone",
        LockResult::Error => "Error",
        LockResult::Timeout => "Timeout",
        LockResult::Unknown => "Unknown",
    }
}

fn tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

pub async fn run(cfg: ServerConfig) -> Result<()> {
    let db = Db::connect(&cfg.db)
        .await
        .with_context(|| format!("初始化 Gofer 数据库失败: {}", cfg.db))?;
    crate::assets::materialize().context("写入内置运行资源失败")?;
    let paused = db.paused().await.unwrap_or(false);

    let auth_state = Arc::new(AuthState::load(&db, cfg.admin_password.as_deref()).await?);
    if !auth_state.has_admin_password() && !auth::is_loopback_bind(&cfg.bind) {
        bail!(
            "面板尚未设置管理员密码。请先以 --bind 127.0.0.1:8790 启动并完成 /setup，\
             或设置 GOFER_ADMIN_PASSWORD 后重启。"
        );
    }

    let db_token = db.get_setting(auth::API_TOKEN_KEY).await?;
    let api = auth::resolve_api_token(db_token.as_deref(), cfg.token.clone());
    let token_source = api.source.label();
    let token_updated_at = db.get_setting(auth::API_TOKEN_UPDATED_KEY).await?;
    let demo_cfg = DemoConfig::new(
        cfg.demo_city_id,
        cfg.demo_cinema_id.clone(),
        cfg.demo_movie_id.clone(),
        cfg.demo_timeout_secs,
        cfg.demo_python.clone(),
        cfg.demo_cmd.clone(),
    );

    let state = AppState::new(AppStateInit {
        db: db.clone(),
        api,
        env_token: cfg.token.clone(),
        token_updated_at,
        auth: auth_state,
        dry_run: cfg.dry_run,
        lock_headless: cfg.lock_headless,
        base_url: base_url_for_bind(&cfg.bind),
        public_url: cfg.public_url.clone(),
        cookie_secure: cfg.cookie_secure,
        paused,
        demo_cfg,
    });

    if let Ok(n) = state
        .db
        .fail_incomplete_capability("lock_demo", "服务重启，演示中断")
        .await
    {
        if n > 0 {
            tracing::warn!("marked {n} stale lock_demo request(s) as failed");
        }
    }
    tracing::info!(
        "gofer listening on http://{}  db={}  token={} ({})  admin={}",
        cfg.bind,
        cfg.db,
        if state.token_required() { "on" } else { "off" },
        token_source,
        if state.auth.has_admin_password() {
            "configured"
        } else {
            "first-run setup"
        }
    );

    let prune_db = db.clone();
    let retention = cfg.retention_days;
    tokio::spawn(async move {
        loop {
            let _ = prune_db.prune(retention).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("监听失败: {}", cfg.bind))?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn base_url_for_bind(bind: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "8790"));
    let host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}")
}

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
