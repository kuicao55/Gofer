//! Local GUI notification scripts.
//!
//! - WeChat: `wechat-notify-fast.py --message` sends to the currently open chat.
//! - Xiaohongshu: `xhs-notify.py --group ...` sends to a named group.
//!
//! The scripts live in `scripts/` by default; set `GOFER_SCRIPTS_DIR` to point
//! at another directory when deploying the binary separately.

use std::path::PathBuf;
use std::process::{Command, Stdio};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_xhs_group_is_rejected_before_spawning() {
        let err = notify_xhs("", "t", "b", None).unwrap_err();
        assert!(err.contains("群名"));
    }
}
