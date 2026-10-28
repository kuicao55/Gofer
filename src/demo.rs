//! Non-submitting Maoyan seat-selection demo.
//!
//! Gofer runs a vendored, confirm-free copy of TixDash's booker in a headed
//! browser, keeps it alive at the seat map, streams its logs, and serves the
//! latest screenshot to the authenticated panel.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::lock::booker_dir;
use crate::notify::scripts_dir;
use crate::server::AppState;

const MAX_LOG_LINES: usize = 500;
const MAX_LOG_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct DemoConfig {
    pub city_id: u32,
    pub cinema_id: Option<String>,
    pub movie_id: Option<String>,
    pub default_num_seats: u64,
    pub timeout: Duration,
    pub screenshot_dir: PathBuf,
    pub script_path: PathBuf,
    pub booker_dir: PathBuf,
    pub profile_dir: PathBuf,
    pub python: Option<PathBuf>,
    /// Test-only escape hatch. Never set this in production.
    pub command_override: Option<PathBuf>,
}

impl DemoConfig {
    pub fn new(
        city_id: u32,
        cinema_id: Option<String>,
        movie_id: Option<String>,
        timeout_secs: u64,
        python: Option<String>,
        command_override: Option<String>,
    ) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self {
            city_id,
            cinema_id: cinema_id.filter(|v| !v.trim().is_empty()),
            movie_id: movie_id.filter(|v| !v.trim().is_empty()),
            default_num_seats: 2,
            timeout: Duration::from_secs(timeout_secs.max(30)),
            screenshot_dir: default_screenshot_dir(),
            script_path: scripts_dir().join("gofer-seat-demo.py"),
            booker_dir: booker_dir(),
            profile_dir: PathBuf::from(home).join(".maoyan-booker-profile"),
            python: python
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            command_override: command_override
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    pub fn for_test(screenshot_dir: PathBuf, command: PathBuf) -> Self {
        Self {
            city_id: 10,
            cinema_id: Some("1".into()),
            movie_id: None,
            default_num_seats: 2,
            timeout: Duration::from_secs(5),
            screenshot_dir,
            script_path: PathBuf::from("scripts/gofer-seat-demo.py"),
            booker_dir: PathBuf::from("."),
            profile_dir: PathBuf::from(".maoyan-booker-profile"),
            python: None,
            command_override: Some(command),
        }
    }
}

fn default_screenshot_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        });
    base.join("gofer").join("demo-screenshots")
}

#[derive(Debug, Clone)]
pub struct DemoJob {
    pub request_id: String,
    pub cinema_id: String,
    pub cinema_name: String,
    pub movie_id: Option<String>,
    pub num_seats: u64,
    pub imax_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DemoStatus {
    pub running: bool,
    pub request_id: String,
    pub phase: String,
    pub cinema_name: String,
    pub started_at: Option<Instant>,
    pub last_line: String,
    pub finished_ok: Option<bool>,
    pub finished_summary: String,
}

impl DemoStatus {
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0)
    }
}

#[derive(Default)]
pub struct DemoShared {
    status: Mutex<DemoStatus>,
    logs: Mutex<VecDeque<String>>,
    stop: Mutex<Option<watch::Sender<bool>>>,
}

impl DemoShared {
    fn begin(&self, job: &DemoJob) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        *self.stop.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        self.logs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = DemoStatus {
            running: true,
            request_id: job.request_id.clone(),
            phase: "排队中".into(),
            cinema_name: job.cinema_name.clone(),
            started_at: Some(Instant::now()),
            last_line: String::new(),
            finished_ok: None,
            finished_summary: String::new(),
        };
        rx
    }

    pub fn snapshot(&self) -> DemoStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_running(&self) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
    }

    pub fn request_stop(&self) {
        if let Some(tx) = self.stop.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(true);
        }
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn push_log(&self, line: impl Into<String>) {
        let line = line.into();
        {
            let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            logs.push_back(line.clone());
            while logs.len() > MAX_LOG_LINES {
                logs.pop_front();
            }
            while logs.iter().map(String::len).sum::<usize>() > MAX_LOG_BYTES {
                logs.pop_front();
            }
        }
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.phase = phase_for_line(&line, &status.phase).to_string();
        status.last_line = line;
    }

    fn finish(&self, ok: bool, summary: &str) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.running = false;
        status.finished_ok = Some(ok);
        status.finished_summary = summary.to_string();
        status.phase = if ok { "完成" } else { "失败" }.into();
        *self.stop.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn phase_for_line(line: &str, current: &str) -> &'static str {
    if line.contains("[demo] 完成") {
        "演示完成"
    } else if line.contains("[demo] 已选好座位") || line.contains("dry-run 停点") {
        "已停在选座页"
    } else if line.contains("[ERROR]") {
        "出错"
    } else if line.contains("[seat]") {
        "选择座位"
    } else if line.contains("[range]") || line.contains("[range-eval]") {
        "评估场次/座位"
    } else if line.contains("[wait-login]") {
        "等待扫码登录"
    } else if line.contains("[nav]") || line.contains("[cfg]") {
        "打开浏览器"
    } else {
        match current {
            "" => "启动中",
            other => {
                // Keep the previous stable phase; this branch only needs a
                // &'static str, so map known values back.
                match other {
                    "排队中" => "排队中",
                    "启动中" => "启动中",
                    "打开浏览器" => "打开浏览器",
                    "等待扫码登录" => "等待扫码登录",
                    "评估场次/座位" => "评估场次/座位",
                    "选择座位" => "选择座位",
                    "已停在选座页" => "已停在选座页",
                    "演示完成" => "演示完成",
                    "出错" => "出错",
                    "失败" => "失败",
                    _ => "运行中",
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

pub fn build_command(cfg: &DemoConfig, job: &DemoJob) -> DemoCommand {
    let mut script_args = vec![
        "--cinema-id".to_string(),
        job.cinema_id.clone(),
        "--show-time-range".to_string(),
        "00:00-23:59".to_string(),
        "--num-seats".to_string(),
        job.num_seats.max(1).to_string(),
        "--auto-seat".to_string(),
        "manual".to_string(),
        "--screenshot-dir".to_string(),
        cfg.screenshot_dir.display().to_string(),
        "--user-data-dir".to_string(),
        cfg.profile_dir.display().to_string(),
    ];
    if let Some(movie) = job.movie_id.as_deref().filter(|v| !v.trim().is_empty()) {
        script_args.push("--movie-id".into());
        script_args.push(movie.to_string());
    }
    if job.imax_only {
        script_args.push("--imax-only".into());
    }

    if let Some(program) = &cfg.command_override {
        return DemoCommand {
            program: program.clone(),
            args: script_args,
            cwd: None,
        };
    }

    let script = cfg.script_path.display().to_string();
    if let Some(python) = &cfg.python {
        let mut args = vec![script];
        args.extend(script_args);
        return DemoCommand {
            program: python.clone(),
            args,
            cwd: Some(cfg.booker_dir.clone()),
        };
    }

    let venv_python = cfg.booker_dir.join(".venv/bin/python");
    if venv_python.exists() {
        let mut args = vec![script];
        args.extend(script_args);
        return DemoCommand {
            program: venv_python,
            args,
            cwd: Some(cfg.booker_dir.clone()),
        };
    }

    let mut args = vec![
        "run".to_string(),
        "--project".to_string(),
        cfg.booker_dir.display().to_string(),
        "python".to_string(),
        script,
    ];
    args.extend(script_args);
    DemoCommand {
        program: PathBuf::from("uv"),
        args,
        cwd: Some(cfg.booker_dir.clone()),
    }
}

pub async fn demo_worker(st: Arc<AppState>, mut rx: mpsc::Receiver<DemoJob>) {
    while let Some(job) = rx.recv().await {
        st.demo_queue.fetch_sub(1, Ordering::SeqCst);
        run_demo(&st, job).await;
    }
}

async fn run_demo(st: &Arc<AppState>, job: DemoJob) {
    let started = Instant::now();
    let _ = st.db.mark_running(&job.request_id).await;
    clear_screenshots(&st.demo_cfg.screenshot_dir);
    let mut stop_rx = st.demo.begin(&job);

    let lock_guard = tokio::select! {
        guard = st.browser_lock.lock() => guard,
        _ = stop_rx.changed() => {
            record(st, &job, started, false, false, false, None, "演示已停止（等待浏览器锁）", false).await;
            return;
        }
        _ = tokio::time::sleep(st.demo_cfg.timeout) => {
            record(st, &job, started, false, false, true, None, "演示超时（等待浏览器锁）", false).await;
            return;
        }
    };

    let cmd = build_command(&st.demo_cfg, &job);
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &cmd.cwd {
        command.current_dir(cwd);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            drop(lock_guard);
            record(
                st,
                &job,
                started,
                false,
                false,
                false,
                None,
                &format!("启动演示脚本失败: {e}"),
                false,
            )
            .await;
            return;
        }
    };

    st.demo
        .push_log(format!("[demo] 启动: {}", cmd.program.display()));
    let mut stdin = child.stdin.take();
    let stdout_task = spawn_reader(st.clone(), child.stdout.take(), false);
    let stderr_task = spawn_reader(st.clone(), child.stderr.take(), true);

    let mut stopped = false;
    let mut timed_out = false;
    let mut exit_code = None;
    tokio::select! {
        status = child.wait() => {
            exit_code = status.ok().and_then(|s| s.code());
        }
        _ = stop_rx.changed() => { stopped = true; }
        _ = tokio::time::sleep(st.demo_cfg.timeout) => { timed_out = true; }
    }

    if stopped || timed_out {
        if let Some(mut input) = stdin.take() {
            let _ = input.write_all(b"\n").await;
            let _ = input.flush().await;
            drop(input);
        }
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }
    drop(stdin);
    // A killed Python process can leave Chromium holding the pipe open. Don't
    // let that block the demo worker forever; the log buffer already has the
    // lines received so far.
    let _ = tokio::time::timeout(Duration::from_secs(2), stdout_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), stderr_task).await;

    let logs = st.demo.logs();
    let joined = logs.join("\n");
    let selected = joined.contains("[seat] 选中:") || joined.contains("[seat] 手动选中:");
    let error_line = logs.iter().rev().find(|line| {
        line.contains("[ERROR] 流程异常:")
            || line.contains("找不到场次")
            || line.contains("TimeoutError")
    });
    let (ok, summary) =
        classify_outcome(selected, stopped, timed_out, error_line.map(String::as_str));
    drop(lock_guard);
    record(
        st, &job, started, ok, stopped, timed_out, exit_code, &summary, selected,
    )
    .await;
}

fn classify_outcome(
    selected: bool,
    stopped: bool,
    timed_out: bool,
    error_line: Option<&str>,
) -> (bool, String) {
    if timed_out {
        (false, "演示超时（已关闭浏览器）".into())
    } else if stopped && selected {
        (true, "演示完成：已停在选座前，未提交订单".into())
    } else if stopped {
        (false, "演示已停止（尚未选座）".into())
    } else if selected {
        (true, "演示完成：已选好座位，未提交订单".into())
    } else if let Some(line) = error_line {
        (false, format!("演示失败: {line}"))
    } else {
        (false, "演示未完成（脚本退出）".into())
    }
}

#[allow(clippy::too_many_arguments)]
async fn record(
    st: &Arc<AppState>,
    job: &DemoJob,
    started: Instant,
    ok: bool,
    stopped: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    summary: &str,
    selected: bool,
) {
    let elapsed = started.elapsed().as_secs_f64();
    let logs = st.demo.logs();
    let result = json!({
        "kind": "lock_demo",
        "order_submitted": false,
        "cinema_id": job.cinema_id,
        "cinema_name": job.cinema_name,
        "movie_id": job.movie_id,
        "num_seats": job.num_seats,
        "imax_only": job.imax_only,
        "selected": selected,
        "stopped_by_user": stopped,
        "timed_out": timed_out,
        "exit_code": exit_code,
        "elapsed_sec": elapsed,
        "screenshot": st.demo_cfg.screenshot_dir.join("latest.png").display().to_string(),
        "stdout_tail": tail(&logs.join("\n"), 4000),
    });
    let _ = st
        .db
        .finish(
            &job.request_id,
            ok,
            (elapsed * 1000.0) as i64,
            summary,
            Some(&result),
            if ok { None } else { Some(summary) },
        )
        .await;
    st.demo.push_log(format!("[demo] {summary}"));
    st.demo.finish(ok, summary);
}

fn spawn_reader(
    st: Arc<AppState>,
    pipe: Option<impl AsyncRead + Unpin + Send + 'static>,
    is_stderr: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(pipe) = pipe else {
            return;
        };
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = if is_stderr {
                format!("[stderr] {line}")
            } else {
                line
            };
            st.demo.push_log(line);
        }
    })
}

fn clear_screenshots(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DemoConfig {
        // A path that does not exist, so build_command falls back to `uv`.
        let booker = tempfile::tempdir().unwrap().path().join("booker");
        DemoConfig {
            city_id: 10,
            cinema_id: None,
            movie_id: None,
            default_num_seats: 2,
            timeout: Duration::from_secs(5),
            screenshot_dir: PathBuf::from("/tmp/gofer-shots"),
            script_path: PathBuf::from("/tmp/gofer-seat-demo.py"),
            booker_dir: booker,
            profile_dir: PathBuf::from("/tmp/profile"),
            python: None,
            command_override: None,
        }
    }

    fn job() -> DemoJob {
        DemoJob {
            request_id: "g_test".into(),
            cinema_id: "37534".into(),
            cinema_name: "Test Cinema".into(),
            movie_id: Some("1545360".into()),
            num_seats: 2,
            imax_only: true,
        }
    }

    #[test]
    fn command_never_contains_confirm() {
        let command = build_command(&cfg(), &job());
        assert!(!command.args.iter().any(|arg| arg.contains("confirm")));
        assert!(command.args.iter().any(|arg| arg == "--cinema-id"));
        assert!(command.args.iter().any(|arg| arg == "--screenshot-dir"));
        assert!(command.args.iter().any(|arg| arg == "--user-data-dir"));
        assert!(command.args.iter().any(|arg| arg == "--movie-id"));
        assert!(command.args.iter().any(|arg| arg == "--imax-only"));
    }

    #[test]
    fn command_uses_uv_when_no_venv() {
        let command = build_command(&cfg(), &job());
        assert_eq!(command.program, PathBuf::from("uv"));
        assert_eq!(command.args.first().map(String::as_str), Some("run"));
    }

    #[test]
    fn outcome_classification() {
        assert!(classify_outcome(true, true, false, None).0);
        assert!(classify_outcome(true, false, false, None).0);
        assert!(!classify_outcome(false, true, false, None).0);
        assert!(!classify_outcome(false, false, true, None).0);
        assert!(!classify_outcome(false, false, false, Some("[ERROR] boom")).0);
    }

    #[test]
    fn phase_progression() {
        assert_eq!(phase_for_line("[cfg] headless = False", ""), "打开浏览器");
        assert_eq!(phase_for_line("[wait-login] 等待", ""), "等待扫码登录");
        assert_eq!(phase_for_line("[range] 评估", ""), "评估场次/座位");
        assert_eq!(phase_for_line("[seat] 选中: [G14]", ""), "选择座位");
        assert_eq!(phase_for_line("[demo] 已选好座位", ""), "已停在选座页");
        assert_eq!(phase_for_line("[ERROR] 流程异常", ""), "出错");
    }

    #[test]
    fn logs_are_bounded() {
        let shared = DemoShared::default();
        for i in 0..600 {
            shared.push_log(format!("line {i}"));
        }
        let logs = shared.logs();
        assert!(logs.len() <= MAX_LOG_LINES);
        assert!(logs.iter().map(String::len).sum::<usize>() <= MAX_LOG_BYTES + 64);
    }
}
