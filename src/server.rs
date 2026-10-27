//! HTTP service, request queues and shared state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::capabilities::{self, Check, Ready};
use crate::db::{CapabilityStat, Db, RequestRow, Stats24h};
use crate::lock::{self, LockArgs, LockResult};
use crate::notify;
use crate::web;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct ServerConfig {
    pub bind: String,
    pub db: String,
    pub token: Option<String>,
    pub dry_run: bool,
    pub lock_headless: bool,
    pub retention_days: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8790".into(),
            db: default_db_path(),
            token: None,
            dry_run: false,
            lock_headless: true,
            retention_days: 30,
        }
    }
}

pub fn default_db_path() -> String {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        });
    base.join("gofer").join("gofer.db").display().to_string()
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub started_at: f64,
    pub base_url: String,
    pub token: Option<String>,
    pub dry_run: bool,
    pub lock_headless: bool,
    pub notify_tx: mpsc::Sender<NotifyJob>,
    pub lock_tx: mpsc::Sender<LockJob>,
    pub notify_queue: Arc<AtomicUsize>,
    pub lock_queue: Arc<AtomicUsize>,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct NotifyJob {
    pub request_id: String,
    pub channel: String,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub group: Option<String>,
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
    pub fn new(
        db: Db,
        token: Option<String>,
        dry_run: bool,
        lock_headless: bool,
        base_url: impl Into<String>,
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel::<NotifyJob>(64);
        let (lock_tx, lock_rx) = mpsc::channel::<LockJob>(16);
        let state = Arc::new(Self {
            db,
            started_at: now_secs(),
            base_url: base_url.into(),
            token: token.filter(|t| !t.trim().is_empty()),
            dry_run,
            lock_headless,
            notify_tx,
            lock_tx,
            notify_queue: Arc::new(AtomicUsize::new(0)),
            lock_queue: Arc::new(AtomicUsize::new(0)),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("gofer http client"),
        });
        tokio::spawn(notify_worker(state.clone(), notify_rx));
        tokio::spawn(lock_worker(state.clone(), lock_rx));
        state
    }

    pub fn diagnostics(&self) -> (Vec<Check>, Ready) {
        capabilities::diagnostics()
    }

    pub fn queues(&self) -> (usize, usize) {
        (
            self.notify_queue.load(Ordering::SeqCst),
            self.lock_queue.load(Ordering::SeqCst),
        )
    }

    pub async fn stats(&self) -> Result<(Stats24h, Vec<CapabilityStat>, Vec<RequestRow>)> {
        let stats = self.db.stats_24h().await?;
        let caps = self.db.capability_stats().await?;
        let recent = self.db.recent(30).await?;
        Ok((stats, caps, recent))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(web::dashboard))
        .route("/requests", get(web::requests_page))
        .route("/health", get(health))
        .route("/api/v1/health", get(health))
        .route("/api/v1/capabilities", get(capabilities_api))
        .route("/api/v1/requests", get(requests_api))
        .route("/v1/requests", get(requests_api))
        .route("/api/v1/requests/:id", get(request_api))
        .route("/v1/requests/:id", get(request_api))
        .route("/api/v1/stats", get(stats_api))
        .route("/v1/stats", get(stats_api))
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

// ------------------------- handlers -------------------------

async fn health(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let (checks, ready) = st.diagnostics();
    let (notify_q, lock_q) = st.queues();
    Json(json!({
        "ok": true,
        "service": "gofer",
        "version": VERSION,
        "uptime_sec": (now_secs() - st.started_at).max(0.0) as u64,
        "dry_run": st.dry_run,
        "token_required": st.token.is_some(),
        "queue": {"notify": notify_q, "lock": lock_q},
        "ready": ready,
        "checks": checks,
    }))
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

async fn notify_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<NotifyRequest>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
    }
    let channel = req.channel.trim().to_lowercase();
    if channel != "wechat" && channel != "xhs" {
        return (StatusCode::BAD_REQUEST, "channel must be wechat or xhs").into_response();
    }
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "message is required").into_response();
    }
    let group = req.group.clone().or_else(|| {
        req.config
            .as_ref()
            .and_then(|c| c.get("group").or_else(|| c.get("xhs")))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    if channel == "xhs" && group.as_deref().unwrap_or("").trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "xhs requires group").into_response();
    }
    let title = req
        .title
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if channel == "xhs" {
                "Gofer · 小红书"
            } else {
                "Gofer · 微信"
            }
            .into()
        });
    let source = request_source(&headers, req.source.as_deref());
    let id = new_id();
    let preview = preview_text(&req.message);
    let payload = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));
    if let Err(e) = st
        .db
        .insert_queued(&id, &channel, &source, &title, &preview, &payload)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let job = NotifyJob {
        request_id: id.clone(),
        channel: channel.clone(),
        title,
        message: req.message,
        url: req.url,
        group,
    };
    if st.notify_tx.try_send(job).is_err() {
        let _ = st.db.fail_queued(&id, "notify queue full").await;
        return (StatusCode::TOO_MANY_REQUESTS, "notify queue full").into_response();
    }
    st.notify_queue.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::ACCEPTED,
        Json(json!({"request_id": id, "status": "queued", "capability": channel})),
    )
        .into_response()
}

async fn lock_handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LockRequest>,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return *r;
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
    let Some(expected) = st.token.as_deref() else {
        return Ok(());
    };
    let direct = ["x-gofer-token", "x-agent-token"]
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|v| v.to_str().ok())
        .any(|v| v == expected);
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| v == expected);
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

fn new_id() -> String {
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
        let dry = st.dry_run;
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
                        Some(&json!({"kind": "notify", "dry_run": st.dry_run})),
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
                let _ = st
                    .db
                    .finish(
                        &job.request_id,
                        false,
                        duration,
                        "锁票失败",
                        None,
                        Some(&e.to_string()),
                    )
                    .await;
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
        post_lock_callback(&st, &job, code, &summary, outcome.elapsed.as_secs_f64()).await;
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
    let state = AppState::new(
        db.clone(),
        cfg.token,
        cfg.dry_run,
        cfg.lock_headless,
        base_url_for_bind(&cfg.bind),
    );
    tracing::info!(
        "gofer listening on http://{}  db={}  token={}",
        cfg.bind,
        cfg.db,
        if state.token.is_some() { "on" } else { "off" }
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

fn base_url_for_bind(bind: &str) -> String {
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
