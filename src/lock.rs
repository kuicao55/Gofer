//! 自动锁票：subprocess 调用 maoyan-booker（Python Playwright PoC）。
//!
//! 对齐 `maoyan-booker/README.md` 的 CLI 与「成功/失败检测」表：
//!   - 真锁成功：rc=0 且 stdout 含 `[lock] 已尝试锁票`
//!   - dry-run：rc=0 且 stdout 含 `[seat] 选中:` 或 `[seat] 手动选中:`
//!     （booker 范围模式的两种选座路径会各打一种：智能算座打 `[seat] 选中:`，
//!     落入「最佳区域」的精确点选打 `[seat] 手动选中:`；两者都算联调走通）
//!   - 流程异常：rc!=0 且 stdout 含 `[ERROR] 流程异常:`
//!   - 场次没了：rc!=0 且含 `找不到场次`
//!   - 超时：rc!=0 且含 `TimeoutError`
//!
//! 调用方式（README 推荐）：`uv run python booker.py ... --auto-seat manual --confirm`，
//! cwd 固定在 booker 项目目录，`--user-data-dir` 用 booker 自己的默认值
//! （`~/.maoyan-booker-profile/`，已登录 session 复用）。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};

use tokio::io::{AsyncBufReadExt, AsyncReadExt};

/// booker 子进程超时。范围模式下命中 N 场要逐场加载选座图评估，90s 可能不够，给 180s 余量。
pub const LOCK_TIMEOUT_SECS: u64 = 180;
/// booker 真锁成功后，用户需要在猫眼 App 限时支付的时间（README 提示 15 分钟）。
pub const PAY_WINDOW_MINUTES: u64 = 15;

/// booker 项目目录（默认 `~/Applications/maoyan-booker`）。可被 `MAOYAN_BOOKER_DIR` 覆盖，
/// 便于集成测试指向别的沙箱副本。
pub fn booker_dir() -> PathBuf {
    if let Ok(d) = std::env::var("MAOYAN_BOOKER_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join("Applications")
        .join("maoyan-booker")
}

/// booker 的截图目录。
pub fn booker_screenshots_dir() -> PathBuf {
    booker_dir().join("screenshots")
}

/// 一次锁票调用按 README 表分类的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockResult {
    /// 真锁成功（15 分钟内去 App 付款）
    Ok,
    /// dry-run：没传 `--confirm`，booker 停在确认前（未真锁）
    DryRun,
    /// 场次被抢光 / 时间错位 / nearest 失败
    ShowGone,
    /// 流程异常（`[ERROR] 流程异常:`）
    Error,
    /// 点击 / 加载超时（modal 拦路？风控？）
    Timeout,
    /// 其他未分类
    Unknown,
}

impl LockResult {
    /// 是否实际锁成。
    pub fn succeeded(self) -> bool {
        self == LockResult::Ok
    }

    /// 是否值得在本轮内对该影院继续重试。Ok/DryRun 不用重试；
    /// ShowGone/Error/Timeout/Unknown 会消耗一次重试额度。
    pub fn retryable(self) -> bool {
        matches!(
            self,
            LockResult::ShowGone | LockResult::Error | LockResult::Timeout | LockResult::Unknown
        )
    }

    /// 给通知/日志的短名。
    pub fn label(self) -> &'static str {
        match self {
            LockResult::Ok => "锁成功",
            LockResult::DryRun => "dry-run（未真锁）",
            LockResult::ShowGone => "场次没了",
            LockResult::Error => "流程异常",
            LockResult::Timeout => "超时",
            LockResult::Unknown => "未知",
        }
    }
}

/// 组装 booker CLI 所需的全部参数（与 ticket-tracker 侧字段对齐）。
#[derive(Debug, Clone)]
pub struct LockArgs {
    pub cinema_id: String,
    pub movie_id: i64,
    /// 场次日期（YYYY-MM-DD），对应 `--show-date`。booker 范围模式必须配日期。
    pub show_date: String,
    /// `--show-time-range "HH:MM-HH:MM"`。None = 交给 booker 当天全部场次排行。
    pub time_range: Option<String>,
    /// 智能选座票数 `--num-seats`（`seats` 为空时生效）。
    pub num_seats: u64,
    /// 只锁 IMAX 厅 `--imax-only`（来自 watch 的「只看IMAX」，与监测过滤一致）。
    pub imax_only: bool,
    /// 手动指定座位 `--seat "X排Y座"`（可多条）；非空时覆盖智能选座。
    pub seats: Vec<String>,
    /// 加 `--confirm` 真锁；false = dry-run（安全联调用）。
    pub confirm: bool,
    /// 无头模式 `--headless`；false = headed（调试用）。
    pub headless: bool,
}

fn build_command(a: &LockArgs) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("uv");
    cmd.args(["run", "python", "booker.py"])
        .arg("--cinema-id")
        .arg(&a.cinema_id)
        .arg("--movie-id")
        .arg(a.movie_id.to_string())
        .arg("--show-date")
        .arg(&a.show_date)
        .current_dir(booker_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(r) = &a.time_range {
        cmd.arg("--show-time-range").arg(r);
    }
    if a.imax_only {
        cmd.arg("--imax-only");
    }
    if a.seats.is_empty() {
        cmd.arg("--num-seats").arg(a.num_seats.to_string());
        cmd.arg("--auto-seat").arg("manual");
    } else {
        for s in &a.seats {
            cmd.arg("--seat").arg(s);
        }
    }
    if a.confirm {
        cmd.arg("--confirm");
    }
    if a.headless {
        cmd.arg("--headless");
    }
    cmd
}

/// 一次锁票的完整结果（给通知/状态机用）。
#[derive(Debug, Clone)]
pub struct LockOutcome {
    pub result: LockResult,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl LockOutcome {
    /// 挑 stdout 里最有信息量的一行做通知摘要；没有则回退 stderr 首 200 字。
    ///
    /// 优先级：真锁成功行 → 带实际座位的选座结果行（`[seat] 手动选中:` / `[seat] 选中:`）
    /// → 其他 `[lock]` 行（pick 失败/不可行/click 失败）→ `[range]` → `[ERROR] 流程异常:`。
    /// 注意 `[seat] 自动检测 ...` 这种选座**开始前**的调试行不会命中，避免把无用的
    /// 调试信息当结果发给用户。
    pub fn summary(&self) -> String {
        for marker in [
            "[lock] 已尝试锁票",
            "[seat] 手动选中:",
            "[seat] 选中:",
            "[lock]",
            "[range]",
            "[ERROR] 流程异常:",
        ] {
            if let Some(line) = self.stdout.lines().find(|l| l.contains(marker)) {
                return line.trim().to_string();
            }
        }
        let ss = self.stderr.trim();
        if !ss.is_empty() {
            return ss.chars().take(200).collect();
        }
        "(无输出)".into()
    }

    /// 是否存在 booker 的锁票成功截图（走到了锁票步骤）。
    pub fn has_locked_screenshot(&self) -> bool {
        booker_screenshots_dir().join("05_locked.png").exists()
    }
}

/// booker 运行句柄：stdout 流式读，终态 marker 一出现就提前定案，但**进程/浏览器
/// 继续存活**；调用方先拿 `decided()` 立即发通知，再 `complete()` 等进程退出回收、
/// 回落 in-flight 标志。这样「选好座就通知」，不用等浏览器 30s 自动关。
pub struct LockRun {
    child: Option<tokio::process::Child>,
    stdout_shared: Arc<StdMutex<String>>,
    stdout_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<String>>,
    started: std::time::Instant,
    decided: Option<LockResult>,
}

impl LockRun {
    /// 终态 marker 已提前定案？调用方可**立即**据它发通知。
    /// `Some(Ok)`=真锁成功；`Some(DryRun)`=联调走通（dry-run 未真锁）；
    /// `Some(Timeout)`=进程超时被强杀。`None`=进程自然退出未命中 marker
    /// （多为失败），交给 `complete()` 按退出码分类。
    pub fn decided(&self) -> Option<LockResult> {
        self.decided
    }

    /// 已读到的 stdout（已含 marker 行）。
    pub fn stdout(&self) -> String {
        self.stdout_shared.lock().unwrap().clone()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// 等 booker 进程真正退出（浏览器保持的收尾会自动结束它）并回收进程、读齐输出。
    /// 返回最终结果：`decided()` 已定案则原样返回；否则用「退出码 + 完整输出」分类。
    pub async fn complete(mut self) -> LockOutcome {
        let mut rc: Option<i32> = None;
        if let Some(mut c) = self.child.take() {
            match tokio::time::timeout(Duration::from_secs(LOCK_TIMEOUT_SECS), c.wait()).await {
                Ok(Ok(st)) => rc = st.code(),
                _ => {
                    // 兜底：进程卡死则强杀（浏览器随进程结束）
                    let _ = c.kill().await;
                    let _ = c.wait().await;
                }
            }
        }
        // stdout 消费任务随 EOF 自然结束（已持有读完的缓冲）
        if let Some(t) = self.stdout_task.take() {
            let _ = t.await;
        }
        let stdout = self.stdout_shared.lock().unwrap().clone();
        let stderr = match self.stderr_task.take() {
            Some(t) => t.await.unwrap_or_default(),
            None => String::new(),
        };
        let result = match self.decided {
            Some(LockResult::Timeout) => LockResult::Timeout,
            Some(r) => r,
            None => classify(rc, &stdout, &stderr),
        };
        LockOutcome {
            result,
            stdout,
            stderr,
            elapsed: self.started.elapsed(),
        }
    }
}

/// 执行一次锁票调用。stdout 后台逐行消费，一旦出现**终态 marker** 就提前返回
/// （进程/浏览器继续存活）：
/// - 真锁（`--confirm`）：以 `[lock] 已尝试锁票` 为终态——它在选座**之后**才打，
///   不会把真锁误判成 dry-run。
/// - dry-run：`[seat] 选中:` / `[seat] 手动选中:` 即是终态（选好座就通知）。
///
/// 定案前若超时则强杀并标记 `Timeout`。启动失败返回 `Err`，其余不会 panic。
/// 调用方拿到 `LockRun` 应尽快 `decided()` 发通知，再 `complete()` 回收进程。
pub async fn run_lock(a: &LockArgs) -> Result<LockRun> {
    if !booker_dir().join("booker.py").exists() {
        return Err(anyhow!("booker.py 不存在: {}", booker_dir().display()));
    }
    let started = std::time::Instant::now();
    let mut child = build_command(a)
        .spawn()
        .map_err(|e| anyhow!("启动 booker 失败: {}", e))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // stderr 后台排空（booker 输出小，不会撑爆 pipe）。
    let stderr_task = tokio::spawn(async move {
        match stderr_pipe {
            Some(p) => pipe_to_string(p).await,
            None => String::new(),
        }
    });

    // stdout 后台逐行读入共享缓冲，每行到了 try_send 一个「有新行」信号。
    let stdout_shared = Arc::new(StdMutex::new(String::new()));
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<()>(1);
    let stdout_task = tokio::spawn({
        let buf = Arc::clone(&stdout_shared);
        let tx = line_tx;
        async move {
            let mut lines = match stdout_pipe {
                Some(p) => tokio::io::BufReader::new(p).lines(),
                None => return,
            };
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut b = buf.lock().unwrap();
                    b.push_str(&line);
                    b.push('\n');
                }
                // 队列满丢信号没关系：内容在共享缓冲，主协程只需「有新行」。
                let _ = tx.try_send(());
            } // EOF（进程退出）自然结束
        }
    });

    // 主协程只等终态 marker（180s 超时兜底）。
    let mut decided: Option<LockResult> = None;
    let timed_out = tokio::time::timeout(Duration::from_secs(LOCK_TIMEOUT_SECS), async {
        while line_rx.recv().await.is_some() {
            let s = stdout_shared.lock().unwrap().clone();
            if s.contains("[lock] 已尝试锁票") {
                decided = Some(LockResult::Ok);
                break;
            }
            if !a.confirm && (s.contains("[seat] 选中:") || s.contains("[seat] 手动选中:")) {
                decided = Some(LockResult::DryRun);
                break;
            }
        }
    })
    .await
    .is_err();

    if timed_out {
        // 超时：强杀，避免僵尸进程挂着 cookie 锁
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Ok(LockRun {
            child: None,
            stdout_shared,
            stdout_task: Some(stdout_task),
            stderr_task: Some(stderr_task),
            started,
            decided: Some(LockResult::Timeout),
        });
    }

    Ok(LockRun {
        child: Some(child),
        stdout_shared,
        stdout_task: Some(stdout_task),
        stderr_task: Some(stderr_task),
        started,
        decided,
    })
}

async fn pipe_to_string<R: tokio::io::AsyncRead + Unpin>(mut p: R) -> String {
    let mut buf = Vec::new();
    let _ = p.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// 把 (exit code, stdout, stderr) 归入 README 的结果分类。
fn classify(rc: Option<i32>, stdout: &str, stderr: &str) -> LockResult {
    let all = format!("{stdout}\n{stderr}");
    match rc {
        Some(0) => {
            if stdout.contains("[lock] 已尝试锁票") {
                LockResult::Ok
            } else if stdout.contains("[seat] 选中:") || stdout.contains("[seat] 手动选中:") {
                // 智能算座成功打 `[seat] 选中:`；范围模式「最佳区域」精确点选成功打
                // `[seat] 手动选中:`。两者都代表联调走通（dry-run 未真锁）。
                LockResult::DryRun
            } else {
                LockResult::Unknown
            }
        }
        _ => {
            if all.contains("[ERROR] 流程异常:") {
                LockResult::Error
            } else if all.contains("找不到场次") {
                LockResult::ShowGone
            } else if all.contains("TimeoutError") {
                LockResult::Timeout
            } else {
                LockResult::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_readme_table() {
        // rc=0 + [lock] → 成功
        assert_eq!(
            classify(Some(0), "[lock] 已尝试锁票成功", ""),
            LockResult::Ok
        );
        // rc=0 + [seat] 无 [lock] → dry-run
        assert_eq!(
            classify(Some(0), "[seat] 选中: ['H8', 'H9']", ""),
            LockResult::DryRun
        );
        // rc=0 + [seat] 手动选中（范围模式最佳区域精确点选）→ 也是 dry-run
        assert_eq!(
            classify(Some(0), "[seat] 手动选中: ['G8', 'G9']", ""),
            LockResult::DryRun
        );
        // rc=0 + 只有选座前的调试行（无实际选中）→ 仍 Unknown
        assert_eq!(
            classify(Some(0), "[seat] 自动检测 11 排 (['A','B'])", ""),
            LockResult::Unknown
        );
        // rc!=0 + [ERROR] → 流程异常优先
        assert_eq!(
            classify(Some(1), "[ERROR] 流程异常: 崩了", "traceback"),
            LockResult::Error
        );
        // rc!=0 + 找不到场次 → 场次没了
        assert_eq!(classify(Some(2), "", "找不到场次"), LockResult::ShowGone);
        // rc!=0 + TimeoutError → 超时（写到 stdout 也算）
        assert_eq!(
            classify(Some(3), "TimeoutError: click 超时", ""),
            LockResult::Timeout
        );
        // rc!=0 无特征 → Unknown
        assert_eq!(classify(Some(1), "", "啥也没"), LockResult::Unknown);
        // rc==0 但没 [lock]/[seat] → Unknown
        assert_eq!(
            classify(Some(0), "[range] 没有可锁场次", ""),
            LockResult::Unknown
        );
        // rc==0 空输出 → Unknown
        assert_eq!(classify(Some(0), "", ""), LockResult::Unknown);
    }

    #[test]
    fn outcome_summary_picks_key_line() {
        let o = LockOutcome {
            result: LockResult::Ok,
            stdout: "[range] 15:30 落入最佳区域 ✓\n[lock] 已尝试锁票成功".into(),
            stderr: String::new(),
            elapsed: Duration::from_secs(0),
        };
        assert_eq!(o.summary(), "[lock] 已尝试锁票成功");
        let d = LockOutcome {
            result: LockResult::DryRun,
            stdout: "[seat] 选中: ['H8', 'H9'] (平均分 0.487)".into(),
            stderr: String::new(),
            elapsed: Duration::from_secs(0),
        };
        assert_eq!(d.summary(), "[seat] 选中: ['H8', 'H9'] (平均分 0.487)");
        // 调试行 `[seat] 自动检测` 排在前面也不该被摘要选中；手动选中结果优先
        let z = LockOutcome {
            result: LockResult::DryRun,
            stdout: "[seat] 自动检测 11 排 (['A','B','C'])，偏好: ['G','F']\n\
                     [seat] 手动选中: ['G8', 'G9']"
                .into(),
            stderr: String::new(),
            elapsed: Duration::from_secs(0),
        };
        assert_eq!(z.summary(), "[seat] 手动选中: ['G8', 'G9']");
        // 失败原因行（[lock] pick 失败）优先于 [seat] 调试行
        let f = LockOutcome {
            result: LockResult::Unknown,
            stdout:
                "[seat] 自动检测 3 排 (['A','B','C'])，偏好: ['B']\n[lock] 20:00 pick 失败: caught"
                    .into(),
            stderr: String::new(),
            elapsed: Duration::from_secs(0),
        };
        assert_eq!(f.summary(), "[lock] 20:00 pick 失败: caught");
    }
}
