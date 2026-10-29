//! Local GUI notification scripts.
//!
//! - WeChat: `wechat-notify-fast.py --message` sends to the currently open chat.
//! - Xiaohongshu: `xhs-notify.py --group ...` sends to a named group.
//!
//! The scripts live in `scripts/` by default; set `GOFER_SCRIPTS_DIR` to point
//! at another directory when deploying the binary separately.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// Scripts directory.
///
/// Release binaries are built on GitHub Actions, so compile-time paths such as
/// `CARGO_MANIFEST_DIR` point at `/Users/runner/...` and must not be used at
/// runtime. Search next to the executable (repo checkout) first, then common
/// install locations; `GOFER_SCRIPTS_DIR` always wins.
pub fn scripts_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GOFER_SCRIPTS_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for anc in dir.ancestors().take(5) {
                candidates.push(anc.join("scripts"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("scripts"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Applications/Gofer/scripts"));
    }
    candidates
        .into_iter()
        .find(|p| p.join("xhs-notify.py").is_file() || p.join("wechat-notify-fast.py").is_file())
        .unwrap_or_else(|| PathBuf::from("scripts"))
}

fn script(name: &str) -> PathBuf {
    scripts_dir().join(name)
}

fn output_error(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr.chars().take(2000).collect();
    }
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    if stdout.is_empty() {
        "脚本执行失败（无输出）".into()
    } else {
        stdout.chars().take(2000).collect()
    }
}

fn run_script(mut cmd: Command, label: &str) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("启动 {label} 失败: {e}"))?;
    if out.status.success() {
        Ok(format!("{label} 已发送"))
    } else {
        Err(output_error(&out.stdout, &out.stderr))
    }
}

/// Send text to the currently open WeChat conversation.
pub fn notify_wechat(message: &str) -> Result<String, String> {
    let script = script("wechat-notify-fast.py");
    if !script.exists() {
        return Err(format!("微信通知脚本不存在: {}", script.display()));
    }
    let mut cmd = Command::new("python3");
    cmd.arg(&script).arg("--message").arg(message);
    run_script(cmd, "微信")
}

/// Send a message to a Xiaohongshu group by name.
pub fn notify_xhs(
    group: &str,
    title: &str,
    body: &str,
    url: Option<&str>,
) -> Result<String, String> {
    let group = group.trim();
    if group.is_empty() {
        return Err("小红书群名不能为空".into());
    }
    let script = script("xhs-notify.py");
    if !script.exists() {
        return Err(format!("小红书通知脚本不存在: {}", script.display()));
    }
    let mut cmd = Command::new("python3");
    cmd.arg(&script)
        .arg("--group")
        .arg(group)
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body);
    if let Some(url) = url.filter(|u| !u.trim().is_empty()) {
        cmd.arg("--url").arg(url);
    }
    run_script(cmd, &format!("小红书 {group}"))
}

/// Actions for the persistent Xiaohongshu `agent-browser` daemon.
#[derive(Debug, Clone, Copy)]
pub enum XhsBrowserAction {
    Status,
    Open,
    Close,
}

impl XhsBrowserAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Open => "open",
            Self::Close => "close",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            // A reset may close and relaunch Chromium, then navigate to /chat.
            Self::Open => Duration::from_secs(120),
            Self::Close => Duration::from_secs(30),
            Self::Status => Duration::from_secs(10),
        }
    }
}

/// Open/close/prewarm the WeChat-adjacent Xiaohongshu browser.
///
/// The Python helper owns the profile/headless/login details; Gofer only
/// serializes access with the notify worker and reports the result.
pub async fn xhs_browser_control(action: XhsBrowserAction) -> Result<Value, String> {
    let script = script("xhs-notify.py");
    if !script.exists() {
        return Err(format!("小红书脚本不存在: {}", script.display()));
    }

    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(&script)
        .arg("--browser")
        .arg(action.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::time::timeout(action.timeout(), cmd.output())
        .await
        .map_err(|_| format!("小红书浏览器 {} 超时", action.as_str()))?
        .map_err(|e| format!("启动小红书浏览器控制失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let last = stdout.lines().last().unwrap_or("").trim();
    let value: Value = serde_json::from_str(last).map_err(|e| {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        format!("小红书浏览器控制输出无法解析: {e}; {detail}")
    })?;

    if !output.status.success() || value.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(if stderr.is_empty() {
                "小红书浏览器控制失败"
            } else {
                &stderr
            });
        return Err(error.to_string());
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_xhs_group_is_rejected_before_spawning() {
        let err = notify_xhs("", "t", "b", None).unwrap_err();
        assert!(err.contains("群名"));
    }
}
