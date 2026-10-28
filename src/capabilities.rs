//! Static capability diagnostics. These checks never send real messages.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ready {
    pub wechat: bool,
    pub xhs: bool,
    pub lock: bool,
    pub demo: bool,
}

pub fn diagnostics() -> (Vec<Check>, Ready) {
    let home = std::env::var("HOME").unwrap_or_default();
    let scripts = crate::notify::scripts_dir();
    let wechat_script = scripts.join("wechat-notify-fast.py");
    let xhs_script = scripts.join("xhs-notify.py");
    let xhs_profile = PathBuf::from(&home).join(".agent-browser/profiles/xiaohongshu");
    let booker_dir = crate::lock::booker_dir();
    let booker_env = booker_dir.join("pyproject.toml");
    let booker_profile = PathBuf::from(&home).join(".maoyan-booker-profile");
    let lock_script = scripts.join("gofer-booker.py");

    let python3 = find_on_path("python3");
    let agent_browser = find_on_path("agent-browser");
    let uv = find_on_path("uv");
    let wechat_running = process_running("WeChat", false)
        || process_running("WeChat.app/Contents/MacOS/WeChat", true);

    let mut checks = Vec::new();
    checks.push(check(
        "macos",
        "macOS GUI",
        cfg!(target_os = "macos"),
        if cfg!(target_os = "macos") {
            "可用".to_string()
        } else {
            "非 macOS，无法执行 GUI 通知/锁票".to_string()
        },
    ));
    checks.push(bin_check("python3", "python3", python3.as_deref()));
    checks.push(path_check(
        "wechat_script",
        "微信通知脚本",
        &wechat_script.to_string_lossy(),
    ));
    checks.push(check(
        "wechat_app",
        "微信进程",
        wechat_running,
        if wechat_running {
            "运行中".to_string()
        } else {
            "未检测到 WeChat 进程（打开微信后重试）".to_string()
        },
    ));
    checks.push(path_check(
        "xhs_script",
        "小红书通知脚本",
        &xhs_script.to_string_lossy(),
    ));
    checks.push(bin_check(
        "agent_browser",
        "agent-browser",
        agent_browser.as_deref(),
    ));
    checks.push(path_check(
        "xhs_profile",
        "小红书登录 profile",
        &xhs_profile.to_string_lossy(),
    ));
    checks.push(bin_check("uv", "uv", uv.as_deref()));
    checks.push(path_check(
        "booker",
        "maoyan-booker 环境",
        &booker_env.to_string_lossy(),
    ));
    checks.push(path_check(
        "lock_script",
        "锁票脚本",
        &lock_script.to_string_lossy(),
    ));
    checks.push(path_check(
        "booker_profile",
        "booker 登录 profile",
        &booker_profile.to_string_lossy(),
    ));

    let demo_script = scripts.join("gofer-seat-demo.py");
    checks.push(path_check(
        "demo_script",
        "锁票演示脚本",
        &demo_script.to_string_lossy(),
    ));
    let demo_venv = booker_dir.join(".venv/bin/python");
    let demo_python_env = std::env::var("GOFER_DEMO_PYTHON")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let demo_python_ok = demo_python_env
        .as_deref()
        .map(|p| PathBuf::from(p).exists() || find_on_path(p).is_some())
        .unwrap_or(false)
        || demo_venv.exists()
        || uv.is_some();
    checks.push(check(
        "demo_python",
        "演示 Python/uv",
        demo_python_ok,
        if let Some(p) = demo_python_env {
            p
        } else if demo_venv.exists() {
            demo_venv.display().to_string()
        } else if let Some(uv) = uv.as_deref() {
            uv.display().to_string()
        } else {
            "未找到 GOFER_DEMO_PYTHON / .venv / uv".to_string()
        },
    ));

    let ok = |id: &str| checks.iter().find(|c| c.id == id).is_some_and(|c| c.ok);
    let ready = Ready {
        wechat: ok("macos") && ok("python3") && ok("wechat_script") && ok("wechat_app"),
        xhs: ok("macos")
            && ok("python3")
            && ok("xhs_script")
            && ok("agent_browser")
            && ok("xhs_profile"),
        lock: ok("macos") && ok("uv") && ok("booker") && ok("lock_script") && ok("booker_profile"),
        demo: ok("macos")
            && ok("demo_script")
            && ok("demo_python")
            && ok("booker")
            && ok("booker_profile"),
    };
    (checks, ready)
}

pub fn checks_for<'a>(checks: &'a [Check], ids: &[&str]) -> Vec<&'a Check> {
    ids.iter()
        .filter_map(|id| checks.iter().find(|c| c.id == *id))
        .collect()
}

fn check(id: &str, label: &str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        id: id.to_string(),
        label: label.to_string(),
        ok,
        detail: detail.into(),
    }
}

fn bin_check(id: &str, label: &str, path: Option<&std::path::Path>) -> Check {
    match path {
        Some(p) => check(id, label, true, p.display().to_string()),
        None => check(id, label, false, format!("找不到 {id}")),
    }
}

fn path_check(id: &str, label: &str, path: &str) -> Check {
    let exists = std::path::Path::new(path).exists();
    check(
        id,
        label,
        exists,
        if exists {
            "存在".to_string()
        } else {
            format!("未找到：{path}")
        },
    )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn process_running(pattern: &str, full: bool) -> bool {
    let mut cmd = std::process::Command::new("pgrep");
    if full {
        cmd.arg("-f");
    } else {
        cmd.arg("-x");
    }
    matches!(
        cmd.arg(pattern).output(),
        Ok(out) if out.status.success() && !out.stdout.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_has_three_capabilities() {
        let (checks, ready) = diagnostics();
        for id in [
            "python3",
            "wechat_script",
            "wechat_app",
            "xhs_script",
            "agent_browser",
            "xhs_profile",
            "uv",
            "booker",
            "booker_profile",
            "lock_script",
            "demo_script",
            "demo_python",
        ] {
            assert!(checks.iter().any(|c| c.id == id), "missing check {id}");
        }
        let _ = (ready.wechat, ready.xhs, ready.lock, ready.demo);
    }

    #[test]
    fn checks_for_filters_by_id() {
        let (checks, _) = diagnostics();
        let selected = checks_for(&checks, &["wechat_script", "xhs_script"]);
        assert_eq!(selected.len(), 2);
    }
}
