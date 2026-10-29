//! Service introspection for external supervisors (iGlance "Services" tab).
//!
//! Gofer itself is a headless service, but its useful footprint spans several
//! processes that are not all its children:
//!
//! - Gofer's own process plus short-lived workers (`python3`, `uv`, ...)
//! - the persistent Xiaohongshu `agent-browser` daemon and its Chrome helpers
//! - Playwright Chromium started by the lock/demo workers
//! - the user's WeChat app, which Gofer drives through the GUI
//!
//! RSS is collected from a single `ps` snapshot and grouped by process. Since
//! RSS can share pages across processes, the total is an estimate, not a
//! physical-footprint measurement.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

const MEMORY_SOURCE: &str = "ps-rss";
const DEFAULT_LAUNCHD_LABEL: &str = "com.kuicao.gofer";
/// Line prefix consumed by iGlance after running a service's start command.
pub const MANIFEST_PREFIX: &str = "IGLANCE_SERVICE_MANIFEST ";
pub const MANIFEST_SCHEMA: &str = "iglance.service.v1";

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ProcessGroup {
    /// Sum of resident set size, in bytes.
    pub rss_bytes: u64,
    pub processes: usize,
    pub pids: Vec<u32>,
}

impl ProcessGroup {
    fn add(&mut self, proc: &ProcInfo) {
        self.rss_bytes = self.rss_bytes.saturating_add(proc.rss_bytes);
        self.processes += 1;
        self.pids.push(proc.pid);
    }

    fn sort(&mut self) {
        self.pids.sort_unstable();
        self.pids.dedup();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGroup {
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub usage: ProcessGroup,
}

impl MemoryGroup {
    fn new(id: &str, label: &str, usage: &ProcessGroup) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            usage: usage.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemorySnapshot {
    pub available: bool,
    pub source: String,
    pub observed_at: String,
    /// Sum of all groups below (estimated RSS).
    pub total_rss_bytes: u64,
    pub total_processes: usize,
    /// Generic, display-oriented grouping. Other services should expose the
    /// same array so iGlance does not need service-specific code.
    pub groups: Vec<MemoryGroup>,
    /// The Gofer server process itself (Gofer-specific compatibility field).
    pub gofer: ProcessGroup,
    /// Gofer children that are not browsers (notification/lock scripts, uv, ...).
    pub workers: ProcessGroup,
    /// Persistent `agent-browser` daemon and Chrome used for Xiaohongshu.
    pub xhs_browser: ProcessGroup,
    /// Playwright Chromium used by the Maoyan lock/demo workers.
    pub booking_browser: ProcessGroup,
    /// All processes belonging to the user's WeChat app.
    pub wechat: ProcessGroup,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl MemorySnapshot {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self {
            source: MEMORY_SOURCE.to_string(),
            observed_at: now_iso(),
            error: Some(error.into()),
            ..Self::default()
        }
    }

    fn from_processes(procs: &[ProcInfo], root: u32) -> Self {
        let descendants = descendants_of(root, procs);
        let mut snapshot = Self {
            available: true,
            source: MEMORY_SOURCE.to_string(),
            observed_at: now_iso(),
            ..Self::default()
        };

        for proc in procs {
            match classify(proc, root, &descendants) {
                Some(Group::Gofer) => snapshot.gofer.add(proc),
                Some(Group::Workers) => snapshot.workers.add(proc),
                Some(Group::XhsBrowser) => snapshot.xhs_browser.add(proc),
                Some(Group::BookingBrowser) => snapshot.booking_browser.add(proc),
                Some(Group::Wechat) => snapshot.wechat.add(proc),
                None => {}
            }
        }

        for group in [
            &mut snapshot.gofer,
            &mut snapshot.workers,
            &mut snapshot.xhs_browser,
            &mut snapshot.booking_browser,
            &mut snapshot.wechat,
        ] {
            group.sort();
            snapshot.total_rss_bytes = snapshot.total_rss_bytes.saturating_add(group.rss_bytes);
            snapshot.total_processes += group.processes;
        }
        snapshot.groups = vec![
            MemoryGroup::new("service", "Gofer", &snapshot.gofer),
            MemoryGroup::new("workers", "Workers", &snapshot.workers),
            MemoryGroup::new("xhs_browser", "小红书浏览器", &snapshot.xhs_browser),
            MemoryGroup::new("booking_browser", "订票浏览器", &snapshot.booking_browser),
            MemoryGroup::new("wechat", "微信", &snapshot.wechat),
        ];
        snapshot
    }
}

/// Take a fresh memory snapshot. Errors are reported in the snapshot instead of
/// failing the HTTP request.
pub fn collect_memory() -> MemorySnapshot {
    match read_processes() {
        Ok(procs) => MemorySnapshot::from_processes(&procs, std::process::id()),
        Err(e) => MemorySnapshot::unavailable(format!("ps 采集失败: {e}")),
    }
}

/// Machine-readable instructions for an external supervisor.
///
/// A stopped Gofer cannot answer HTTP, so the only way to start it is a local
/// command. `start.command` is `null` when no launcher can be discovered; set
/// `GOFER_START_COMMAND` explicitly in that case.
pub fn controls(base_url: &str) -> Value {
    json!({
        "protocol": {
            "manifest_prefix": MANIFEST_PREFIX.trim_end(),
            "schema": MANIFEST_SCHEMA,
        },
        "status": {"method": "GET", "path": "/api/v1/service"},
        "memory": {"method": "GET", "path": "/api/v1/service/memory"},
        "pause": {"method": "POST", "path": "/api/v1/service/pause"},
        "resume": {"method": "POST", "path": "/api/v1/service/resume"},
        "toggle": {"method": "POST", "path": "/api/v1/service/toggle"},
        "stop": {
            "method": "POST",
            "path": "/api/v1/service/stop",
            "query": {
                "cleanup": "bool, default true: close the XHS browser and terminate Gofer's workers",
                "close_browser": "bool, default cleanup value: run `agent-browser close`"
            }
        },
        "start": start_spec(base_url),
    })
}

/// One-line handshake printed by a start command. iGlance runs the start
/// command, waits for a line with `MANIFEST_PREFIX`, then uses `base_url` and
/// `endpoints` to poll status/memory and send stop/pause/resume.
pub fn manifest_line(base_url: &str) -> String {
    let manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "service": {
            "id": "gofer",
            "name": "Gofer",
            "version": crate::server::VERSION,
        },
        "base_url": base_url.trim_end_matches('/'),
        "auth": {
            "type": "header",
            "header": "X-Gofer-Token",
        },
        "endpoints": {
            "health": "/health",
            "status": "/api/v1/service",
            "memory": "/api/v1/service/memory",
            "pause": "/api/v1/service/pause",
            "resume": "/api/v1/service/resume",
            "toggle": "/api/v1/service/toggle",
            "stop": "/api/v1/service/stop",
        },
    });
    format!("{MANIFEST_PREFIX}{manifest}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Gofer,
    Workers,
    XhsBrowser,
    BookingBrowser,
    Wechat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    rss_bytes: u64,
    command: String,
}

fn read_processes() -> std::io::Result<Vec<ProcInfo>> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,command="])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ps exited with {}",
            output.status
        )));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_ps(text: &str) -> Vec<ProcInfo> {
    text.lines().filter_map(parse_ps_line).collect()
}

fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let rss_kb: u64 = fields.next()?.parse().ok()?;
    let command = fields.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcInfo {
        pid,
        ppid,
        rss_bytes: rss_kb.saturating_mul(1024),
        command,
    })
}

fn descendants_of(root: u32, procs: &[ProcInfo]) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    seen
}

fn classify(proc: &ProcInfo, root: u32, descendants: &HashSet<u32>) -> Option<Group> {
    if proc.pid == root {
        return Some(Group::Gofer);
    }
    // The `ps` command used to take this snapshot is itself a child of Gofer.
    // Counting it would make the reported total depend on the measurement.
    if is_measurement_process(&proc.command) {
        return None;
    }
    if is_xhs_process(&proc.command) {
        return Some(Group::XhsBrowser);
    }
    if is_booking_browser(&proc.command) {
        return Some(Group::BookingBrowser);
    }
    if is_wechat_process(&proc.command) {
        return Some(Group::Wechat);
    }
    descendants.contains(&proc.pid).then_some(Group::Workers)
}

fn is_measurement_process(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    executable_basename(command) == "ps" && lower.contains("-axo") && lower.contains("command=")
}

/// `agent-browser` daemon and Chrome instances using its profile/browser dir.
///
/// We match the executable or the `~/.agent-browser` path instead of a loose
/// substring so that unrelated commands such as `grep agent-browser` are not
/// counted.
fn is_xhs_process(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("/.agent-browser/") {
        return true;
    }
    let base = executable_basename(command);
    base == "agent-browser" || base.starts_with("agent-browser-")
}

/// Playwright Chromium used by maoyan-booker or the built-in demo.
fn is_booking_browser(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains(".maoyan-booker-profile")
        || (lower.contains("ms-playwright")
            && (lower.contains("chromium") || lower.contains("headless_shell")))
}

/// Any process inside WeChat/WeChatAppEx app bundles, including helpers.
fn is_wechat_process(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("/wechat.app/") || lower.contains("/wechatappex.app/") {
        return true;
    }
    let base = executable_basename(command);
    base == "wechat" || base == "wechatappex"
}

fn executable_basename(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn start_spec(base_url: &str) -> Value {
    let manifest_command = manifest_command(base_url);
    let shell = "/bin/zsh";
    let manifest_prefix = MANIFEST_PREFIX.trim_end();
    let hint = "Run this locally when the service is offline, then read the \
                IGLANCE_SERVICE_MANIFEST line from stdout.";

    if let Some(command) = env_nonempty("GOFER_START_COMMAND") {
        let binding_command = match &manifest_command {
            Some(manifest) => format!("{command} && {manifest}"),
            None => command.clone(),
        };
        return json!({
            "type": "command",
            "shell": shell,
            "command": command,
            "binding_command": binding_command,
            "manifest_command": manifest_command,
            "manifest_prefix": manifest_prefix,
            "source": "GOFER_START_COMMAND",
            "hint": "binding_command 会先执行自定义命令，再输出 manifest。",
        });
    }

    if supervised_launchd() {
        let label = launchd_label();
        let plist = launchd_plist(&label);
        let bootstrap = if plist.is_file() {
            format!(
                "launchctl bootstrap gui/$(id -u) {}",
                shell_quote(&plist.to_string_lossy())
            )
        } else {
            format!(
                "launchctl kickstart -k gui/$(id -u)/{}",
                shell_quote(&label)
            )
        };
        let command = match &manifest_command {
            Some(manifest) => format!("{bootstrap} && {manifest}"),
            None => bootstrap,
        };
        return json!({
            "type": "command",
            "shell": shell,
            "command": command,
            "binding_command": command,
            "manifest_command": manifest_command,
            "manifest_prefix": manifest_prefix,
            "source": "launchd",
            "launchd_label": label,
            "plist": plist.to_string_lossy(),
            "hint": "launchd 模式；停止会先 bootout，再用 bootstrap 重新加载 plist。",
        });
    }

    if let Some(script) = find_start_script() {
        let cwd = script.parent().map(|p| p.to_string_lossy().to_string());
        let command = format!(
            "{} start --manifest",
            shell_quote(&script.to_string_lossy())
        );
        return json!({
            "type": "command",
            "shell": shell,
            "command": command,
            "binding_command": command,
            "manifest_command": manifest_command,
            "manifest_prefix": manifest_prefix,
            "cwd": cwd,
            "source": "start-gofer.sh",
            "hint": hint,
        });
    }

    json!({
        "type": "command",
        "shell": shell,
        "command": Value::Null,
        "binding_command": Value::Null,
        "manifest_command": manifest_command,
        "manifest_prefix": manifest_prefix,
        "source": "unknown",
        "hint": "未找到 start-gofer.sh；请设置 GOFER_START_COMMAND，例如 'cd ~/Applications/Gofer && ./start-gofer.sh start --manifest'。",
    })
}

fn manifest_command(base_url: &str) -> Option<String> {
    std::env::current_exe().ok().map(|exe| {
        format!(
            "{} manifest --base-url {}",
            shell_quote(&exe.to_string_lossy()),
            shell_quote(base_url.trim_end_matches('/'))
        )
    })
}

pub fn supervised_launchd() -> bool {
    env_nonempty("GOFER_SUPERVISED")
        .map(|v| v.eq_ignore_ascii_case("launchd"))
        .unwrap_or(false)
}

fn launchd_label() -> String {
    env_nonempty("GOFER_LAUNCHD_LABEL").unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string())
}

fn launchd_plist(label: &str) -> PathBuf {
    if let Some(path) = env_nonempty("GOFER_LAUNCHD_PLIST") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

fn find_start_script() -> Option<PathBuf> {
    if let Some(path) = env_nonempty("GOFER_START_SCRIPT") {
        return Some(PathBuf::from(path));
    }

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for ancestor in dir.ancestors().take(5) {
                candidates.push(ancestor.join("start-gofer.sh"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("start-gofer.sh"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Applications/Gofer/start-gofer.sh"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

pub fn started_at_iso(started_at: f64) -> String {
    let uptime_ms = ((server_now_secs() - started_at).max(0.0) * 1000.0) as i64;
    (chrono::Local::now() - chrono::Duration::milliseconds(uptime_ms))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn server_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Best-effort cleanup immediately before Gofer exits.
///
/// - optionally close the persistent Xiaohongshu browser via `agent-browser close`
/// - terminate any remaining Gofer children (python/uv/Playwright)
/// - remove start-gofer.sh's PID file if it still points at this process
///
/// WeChat is intentionally left running: it is the user's app, not a Gofer
/// child.
pub async fn stop_cleanup(close_browser: bool) {
    if close_browser {
        close_agent_browser().await;
    }
    terminate_descendants().await;
}

async fn close_agent_browser() {
    let mut child = match tokio::process::Command::new("agent-browser")
        .arg("close")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    if tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

async fn terminate_descendants() {
    let root = std::process::id();
    let Ok(procs) = read_processes() else {
        return;
    };
    let pids: Vec<u32> = descendants_of(root, &procs)
        .into_iter()
        .filter(|pid| *pid != root)
        .collect();
    if pids.is_empty() {
        return;
    }
    signal(&pids, "-TERM").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    signal(&pids, "-KILL").await;
}

async fn signal(pids: &[u32], signal: &str) {
    let mut cmd = tokio::process::Command::new("/bin/kill");
    cmd.arg(signal);
    for pid in pids {
        cmd.arg(pid.to_string());
    }
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

pub fn remove_pid_file() {
    let Some(path) = env_nonempty("GOFER_PID_FILE") else {
        return;
    };
    let path = PathBuf::from(path);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if text.trim().parse::<u32>().ok() == Some(std::process::id()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove the launchd job so `KeepAlive` does not immediately restart Gofer.
/// Only meaningful when `GOFER_SUPERVISED=launchd`.
pub async fn bootout_launchd() {
    if !supervised_launchd() {
        return;
    }
    let label = launchd_label();
    let target = format!("gui/{}/{}", current_uid(), label);
    let _ = tokio::process::Command::new("launchctl")
        .args(["bootout", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

fn current_uid() -> String {
    std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, rss_kb: u64, command: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_bytes: rss_kb * 1024,
            command: command.to_string(),
        }
    }

    #[test]
    fn parses_ps_output() {
        let text = "  100     1   2048 /Users/me/Gofer/target/release/gofer serve\n\
                    \n\
                    101   100   1024 python3 xhs-notify.py --group test\n";
        let procs = parse_ps(text);
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0].pid, 100);
        assert_eq!(procs[0].ppid, 1);
        assert_eq!(procs[0].rss_bytes, 2 * 1024 * 1024);
        assert_eq!(procs[1].command, "python3 xhs-notify.py --group test");
    }

    #[test]
    fn groups_gofer_workers_browsers_and_wechat() {
        let procs = vec![
            proc(100, 1, 20_000, "/Users/me/Gofer/gofer serve"),
            proc(101, 100, 5_000, "python3 /Users/me/Gofer/scripts/xhs-notify.py --group test"),
            proc(
                102,
                1,
                130_000,
                "/Users/me/.agent-browser/browsers/chrome/Google Chrome for Testing --user-data-dir=/Users/me/.agent-browser/profiles/xiaohongshu",
            ),
            proc(
                103,
                100,
                90_000,
                "/Users/me/.cache/ms-playwright/chromium-1/chrome-mac/Chromium.app/Contents/MacOS/Chromium --user-data-dir=/Users/me/.maoyan-booker-profile",
            ),
            proc(104, 1, 400_000, "/Applications/WeChat.app/Contents/MacOS/WeChat"),
            proc(
                105,
                104,
                80_000,
                "/Applications/WeChat.app/Contents/Frameworks/WeChat Helper.app/Contents/MacOS/WeChat Helper",
            ),
            proc(106, 1, 1_000, "/sbin/launchd"),
        ];

        let snapshot = MemorySnapshot::from_processes(&procs, 100);
        assert!(snapshot.available);
        assert_eq!(snapshot.gofer.processes, 1);
        assert_eq!(snapshot.gofer.rss_bytes, 20_000 * 1024);
        assert_eq!(snapshot.workers.processes, 1);
        assert_eq!(snapshot.workers.pids, vec![101]);
        assert_eq!(snapshot.xhs_browser.processes, 1);
        assert_eq!(snapshot.booking_browser.processes, 1);
        assert_eq!(snapshot.wechat.processes, 2);
        assert_eq!(snapshot.wechat.rss_bytes, 480_000 * 1024);
        assert_eq!(snapshot.total_processes, 6);
        assert_eq!(
            snapshot.total_rss_bytes,
            (20_000 + 5_000 + 130_000 + 90_000 + 400_000 + 80_000) * 1024
        );
        assert_eq!(snapshot.groups.len(), 5);
        assert_eq!(snapshot.groups[0].id, "service");
        assert_eq!(snapshot.groups[0].label, "Gofer");
        assert_eq!(snapshot.groups[0].usage.rss_bytes, snapshot.gofer.rss_bytes);
        assert_eq!(snapshot.groups[4].id, "wechat");
        assert_eq!(snapshot.groups[4].usage.processes, 2);
    }

    #[test]
    fn ignores_unrelated_processes_even_when_command_mentions_keywords() {
        let procs = vec![
            proc(100, 1, 1_000, "/Users/me/Gofer/gofer serve"),
            proc(101, 1, 1_000, "/usr/bin/grep agent-browser"),
            proc(102, 1, 1_000, "/bin/zsh -c echo WeChat"),
        ];
        let snapshot = MemorySnapshot::from_processes(&procs, 100);
        assert_eq!(snapshot.total_processes, 1);
        assert_eq!(snapshot.xhs_browser.processes, 0);
        assert_eq!(snapshot.wechat.processes, 0);
    }

    #[test]
    fn excludes_the_ps_process_used_for_measurement() {
        let procs = vec![
            proc(100, 1, 1_000, "/Users/me/Gofer/gofer serve"),
            proc(101, 100, 1_200, "ps -axo pid=,ppid=,rss=,command="),
        ];
        let snapshot = MemorySnapshot::from_processes(&procs, 100);
        assert_eq!(snapshot.workers.processes, 0);
        assert_eq!(snapshot.total_processes, 1);
    }

    #[test]
    fn detects_agent_browser_daemon_by_executable_name() {
        let procs = vec![
            proc(100, 1, 1_000, "/Users/me/Gofer/gofer serve"),
            proc(
                101,
                1,
                10_000,
                "/usr/local/Cellar/node/25/bin/agent-browser-darwin-x64",
            ),
        ];
        let snapshot = MemorySnapshot::from_processes(&procs, 100);
        assert_eq!(snapshot.xhs_browser.processes, 1);
        assert_eq!(snapshot.xhs_browser.pids, vec![101]);
    }

    #[test]
    fn start_spec_is_machine_readable() {
        let spec = start_spec("http://127.0.0.1:8790/");
        assert_eq!(spec["type"], "command");
        assert_eq!(spec["shell"], "/bin/zsh");
        assert_eq!(spec["manifest_prefix"], "IGLANCE_SERVICE_MANIFEST");
    }

    #[test]
    fn manifest_line_is_self_describing() {
        let line = manifest_line("http://127.0.0.1:8790/");
        let payload = line.strip_prefix(MANIFEST_PREFIX).expect("manifest prefix");
        let v: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["schema"], "iglance.service.v1");
        assert_eq!(v["service"]["id"], "gofer");
        assert_eq!(v["base_url"], "http://127.0.0.1:8790");
        assert_eq!(v["auth"]["header"], "X-Gofer-Token");
        assert_eq!(v["endpoints"]["health"], "/health");
        assert_eq!(v["endpoints"]["status"], "/api/v1/service");
        assert_eq!(v["endpoints"]["memory"], "/api/v1/service/memory");
        assert_eq!(v["endpoints"]["stop"], "/api/v1/service/stop");
        assert!(!payload.contains("change-me"));
    }
}
