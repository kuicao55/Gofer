//! Read-only operational panel for Gofer.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};

use crate::capabilities;
use crate::db::{CapabilityStat, RequestRow};
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

pub async fn dashboard(State(st): State<Arc<AppState>>) -> Response {
    match build_dashboard(&st).await {
        Ok(t) => render(&t).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
        base_url: st.base_url.clone(),
        token_on: st.token.is_some(),
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

async fn build_dashboard(st: &Arc<AppState>) -> anyhow::Result<DashboardTemplate> {
    let stats = st.db.stats_24h().await.unwrap_or_default();
    let cap_stats = st.db.capability_stats().await.unwrap_or_default();
    let recent = st.db.recent(12).await.unwrap_or_default();
    let all_total = st.db.total_count().await.unwrap_or(0);
    let (checks, ready) = st.diagnostics();
    let (notify_q, lock_q) = st.queues();

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
            &["macos", "uv", "booker", "booker_profile"],
            lock_q,
        ),
    ];
    let mut caps = Vec::with_capacity(cap_defs.len());
    for (id, label, subtitle, ready, ids, queue) in cap_defs {
        let stat = stat_map.get(id).cloned().unwrap_or_default();
        let selected = capabilities::checks_for(&checks, ids);
        let checks_ok = selected.iter().filter(|c| c.ok).count();
        let checks_total = selected.len();
        let last_error = st.db.last_for_capability(id, false).await.ok().flatten();
        caps.push(CapabilityView {
            id: id.to_string(),
            label: label.to_string(),
            subtitle: subtitle.to_string(),
            ready,
            status_text: if ready { "就绪" } else { "未就绪" }.into(),
            status_class: if ready { "ok" } else { "bad" }.into(),
            checks: selected
                .into_iter()
                .map(|c| CheckView {
                    label: c.label.clone(),
                    ok: c.ok,
                    detail: c.detail.clone(),
                })
                .collect(),
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
        base_url: st.base_url.clone(),
        token_on: st.token.is_some(),
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
    let (status_text, status_class) = match r.status.as_str() {
        "ok" => ("成功", "ok"),
        "failed" => ("失败", "bad"),
        "running" => ("执行中", "warn"),
        "queued" => ("排队中", "flat"),
        _ => ("未知", "flat"),
    };
    let capability = match r.capability.as_str() {
        "wechat" => "微信",
        "xhs" => "小红书",
        "lock" => "锁票",
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
