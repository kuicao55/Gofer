//! GitHub Releases self-update.
//!
//! Checks `/repos/<repo>/releases/latest`, downloads the asset for the current
//! macOS architecture, verifies `SHA256SUMS`, atomically replaces the running
//! binary, and restarts it. For the private Gofer repository set
//! `GOFER_UPDATE_TOKEN`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const REPO: &str = "kuicao55/Gofer";
pub const ASSET_PREFIX: &str = "gofer";
pub const TOKEN_ENV: &str = "GOFER_UPDATE_TOKEN";
pub const SUPERVISED_ENV: &str = "GOFER_SUPERVISED";
pub const PID_FILE_ENV: &str = "GOFER_PID_FILE";
pub const LOG_FILE_ENV: &str = "GOFER_LOG_FILE";
pub const API_BASE_ENV: &str = "GOFER_UPDATE_API_BASE";

#[derive(Debug, Clone, Default)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub tag: String,
    pub newer: bool,
    pub html_url: String,
    pub published_at: String,
    pub notes: String,
    pub asset_name: String,
    pub error: String,
    pub asset_url: String,
    pub checksums_url: String,
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub version: String,
    pub supervised: bool,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Check GitHub Releases. Fetch errors are returned in `UpdateInfo::error`
/// so the panel can render a useful message instead of a 500 page.
pub async fn check() -> UpdateInfo {
    let current = current_version().to_string();
    let mut out = UpdateInfo {
        current: current.clone(),
        ..Default::default()
    };
    match fetch_latest().await {
        Ok(latest) => {
            out.latest = latest.version;
            out.tag = latest.tag;
            out.html_url = latest.html_url;
            out.published_at = latest.published_at;
            out.notes = latest.notes;
            out.asset_name = latest.asset_name;
            out.asset_url = latest.asset_url;
            out.checksums_url = latest.checksums_url;
            out.newer = is_newer(&out.latest, &current);
            if out.newer && out.asset_url.is_empty() {
                out.error = format!("release {} 没有当前平台的二进制资产", out.tag);
            } else if out.newer && out.checksums_url.is_empty() {
                out.error = format!("release {} 缺少 SHA256SUMS", out.tag);
            }
        }
        Err(e) => out.error = e.to_string(),
    }
    out
}

/// Download, verify and replace the current binary. Caller should exit shortly
/// after a successful return so the helper/launchd can start the new process.
pub async fn install_latest() -> Result<InstallOutcome> {
    let info = check().await;
    if !info.error.is_empty() {
        return Err(anyhow!(info.error));
    }
    if !info.newer {
        return Err(anyhow!("已经是最新版本 {}", info.current));
    }
    let token = update_token();
    let checksums = download_text(&info.checksums_url, token.as_deref()).await?;
    let expected = checksum_for(&checksums, &info.asset_name)
        .ok_or_else(|| anyhow!("SHA256SUMS 中没有 {}", info.asset_name))?;
    let bytes = download_bytes(&info.asset_url, token.as_deref()).await?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(anyhow!(
            "SHA256 校验失败: expected {expected}, got {actual}"
        ));
    }

    let exe = std::fs::canonicalize(std::env::current_exe().context("读取当前二进制路径失败")?)
        .context("规范化当前二进制路径失败")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("无法确定二进制目录"))?;
    let tmp = dir.join(format!(
        ".{}.update",
        exe.file_name().and_then(|s| s.to_str()).unwrap_or("gofer")
    ));
    fs::write(&tmp, &bytes).with_context(|| format!("写入 {} 失败", tmp.display()))?;
    let mut perms = fs::metadata(&tmp)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms)?;

    // A broken download must never replace a working binary.
    let probe = Command::new(&tmp)
        .arg("--version")
        .output()
        .context("运行新二进制失败")?;
    if !probe.status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!(
            "新二进制无法运行: {}",
            String::from_utf8_lossy(&probe.stderr)
        ));
    }

    let backup = exe.with_extension("bak");
    let _ = fs::copy(&exe, &backup);
    fs::rename(&tmp, &exe).with_context(|| {
        format!(
            "替换二进制失败（{} → {}）；如权限不足请检查安装目录",
            tmp.display(),
            exe.display()
        )
    })?;

    let supervised = supervised();
    if !supervised {
        spawn_restart_helper(&exe)?;
    }
    Ok(InstallOutcome {
        version: info.latest,
        supervised,
    })
}

struct LatestRelease {
    version: String,
    tag: String,
    html_url: String,
    published_at: String,
    notes: String,
    asset_name: String,
    asset_url: String,
    checksums_url: String,
}

async fn fetch_latest() -> Result<LatestRelease> {
    let base = std::env::var(API_BASE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".into());
    let url = format!(
        "{}/repos/{}/releases/latest",
        base.trim_end_matches('/'),
        REPO
    );
    let token = update_token();
    let mut req = client()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = token.as_deref() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.context("请求 GitHub Releases 失败")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let hint = match status.as_u16() {
            404 => "（仓库还没有发布 Release）",
            401 | 403 => "（限流或私有仓库：请配置 GOFER_UPDATE_TOKEN）",
            _ => "",
        };
        return Err(anyhow!(
            "GitHub API {} {}{}",
            status,
            body.chars().take(200).collect::<String>(),
            hint
        ));
    }
    let v: Value = resp.json().await.context("解析 GitHub Release JSON 失败")?;
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let version = tag.trim_start_matches('v').to_string();
    let asset_name = expected_asset_name()?;
    let mut asset_url = String::new();
    let mut checksums_url = String::new();
    for asset in v
        .get("assets")
        .and_then(|x| x.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let name = asset.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let url = asset
            .get("browser_download_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name == asset_name {
            asset_url = url;
        } else if name == "SHA256SUMS" {
            checksums_url = url;
        }
    }
    Ok(LatestRelease {
        version,
        tag,
        html_url: v
            .get("html_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        published_at: v
            .get("published_at")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        notes: v
            .get("body")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .chars()
            .take(4000)
            .collect(),
        asset_name,
        asset_url,
        checksums_url,
    })
}

fn expected_asset_name() -> Result<String> {
    if std::env::consts::OS != "macos" {
        return Err(anyhow!("自动更新目前只支持 macOS"));
    }
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => return Err(anyhow!("不支持的 macOS 架构: {other}")),
    };
    Ok(format!("{ASSET_PREFIX}-{arch}-apple-darwin"))
}

fn update_token() -> Option<String> {
    [TOKEN_ENV, "GITHUB_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("{ASSET_PREFIX}/{}", current_version()))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("update http client")
}

async fn download_text(url: &str, token: Option<&str>) -> Result<String> {
    let mut req = client()
        .get(url)
        .header("Accept", "application/octet-stream");
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.context("下载 SHA256SUMS 失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("下载 SHA256SUMS HTTP {}", resp.status()));
    }
    Ok(resp.text().await?)
}

async fn download_bytes(url: &str, token: Option<&str>) -> Result<Vec<u8>> {
    let mut req = client()
        .get(url)
        .header("Accept", "application/octet-stream")
        .timeout(Duration::from_secs(180));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.context("下载新二进制失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("下载新二进制 HTTP {}", resp.status()));
    }
    Ok(resp.bytes().await?.to_vec())
}

fn checksum_for(text: &str, asset_name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        if name.trim_start_matches('*') == asset_name {
            return Some(hash.to_lowercase());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let l = version_parts(latest);
    let c = version_parts(current);
    for i in 0..l.len().max(c.len()) {
        let a = l.get(i).copied().unwrap_or(0);
        let b = c.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

fn version_parts(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn state_dir() -> PathBuf {
    let dir = PathBuf::from(crate::server::default_db_path())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn supervised() -> bool {
    std::env::var(SUPERVISED_ENV)
        .map(|v| v.eq_ignore_ascii_case("launchd"))
        .unwrap_or(false)
}

fn spawn_restart_helper(exe: &Path) -> Result<()> {
    let pid = std::process::id();
    let cwd = std::env::current_dir().context("读取当前目录失败")?;
    let args = std::env::args_os()
        .skip(1)
        .map(|a| shell_quote(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let state = state_dir();
    let pid_file = std::env::var(PID_FILE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.join("gofer.pid").display().to_string());
    let log_file = std::env::var(LOG_FILE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.join("gofer.log").display().to_string());
    let helper = state.join("update-restart.sh");
    let script = format!(
        "#!/bin/sh\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         cd {cwd}\n\
         nohup {exe} {args} >> {log} 2>&1 &\n\
         echo $! > {pidfile}\n\
         rm -f \"$0\"\n",
        pid = pid,
        cwd = shell_quote(&cwd.to_string_lossy()),
        exe = shell_quote(&exe.to_string_lossy()),
        args = args,
        log = shell_quote(&log_file),
        pidfile = shell_quote(&pid_file),
    );
    fs::write(&helper, script).with_context(|| format!("写入 {}", helper.display()))?;
    let mut perms = fs::metadata(&helper)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&helper, perms)?;
    Command::new("nohup")
        .arg("sh")
        .arg(&helper)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动重启助手失败")?;
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn schedule_exit() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(700)).await;
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("4.3.0", "4.2.9"));
        assert!(is_newer("v0.3.0", "0.2.9"));
        assert!(!is_newer("4.2.1", "4.2.1"));
        assert!(!is_newer("4.2.0", "4.2.1"));
    }

    #[test]
    fn checksum_lookup_handles_binary_marker() {
        let text = "abc123  tt-server-aarch64-apple-darwin\n\
                    def456 *gofer-x86_64-apple-darwin\n";
        assert_eq!(
            checksum_for(text, "tt-server-aarch64-apple-darwin"),
            Some("abc123".into())
        );
        assert_eq!(
            checksum_for(text, "gofer-x86_64-apple-darwin"),
            Some("def456".into())
        );
        assert_eq!(checksum_for(text, "missing"), None);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a b'c"), "'a b'\\''c'");
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
