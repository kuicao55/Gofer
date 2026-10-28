//! Best-effort Discord webhook notifications for real ticket locks.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub const WEBHOOK_KEY: &str = "lock.discord_webhook";

pub struct LockNotice<'a> {
    pub request_id: &'a str,
    pub cinema_id: &'a str,
    pub cinema_name: &'a str,
    pub movie_id: i64,
    pub movie_name: &'a str,
    pub show_date: &'a str,
    pub code: &'a str,
    pub summary: &'a str,
    pub elapsed_sec: f64,
}

/// Only allow real Discord webhook endpoints. The URL contains a secret token,
/// so it is never logged.
pub fn validate_webhook(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        return Err(anyhow!("webhook 不能为空"));
    }
    if url.len() > 2048 {
        return Err(anyhow!("webhook 过长"));
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("webhook 必须使用 https://"))?;
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let allowed =
        host == "discord.com" || host == "discordapp.com" || host.ends_with(".discord.com");
    if !allowed {
        return Err(anyhow!("只允许 Discord webhook 域名"));
    }
    if !rest.contains("/api/webhooks/") {
        return Err(anyhow!("不是 Discord webhook 路径"));
    }
    Ok(())
}

pub fn build_lock_payload(n: &LockNotice<'_>) -> Value {
    let (title, color) = match n.code {
        "Ok" => ("锁票成功", 0x3f6b4f),
        "DryRun" => ("锁票完成（未提交）", 0x9a6a22),
        _ => ("锁票未成功", 0xa64535),
    };
    let cinema = if n.cinema_name.trim().is_empty() {
        n.cinema_id.to_string()
    } else {
        format!("{} ({})", n.cinema_name, n.cinema_id)
    };
    let movie = if n.movie_name.trim().is_empty() {
        n.movie_id.to_string()
    } else {
        format!("{} ({})", n.movie_name, n.movie_id)
    };
    let show_date = if n.show_date.trim().is_empty() {
        "—".to_string()
    } else {
        n.show_date.to_string()
    };
    json!({
        "username": "Gofer",
        "allowed_mentions": {"parse": []},
        "embeds": [{
            "title": title,
            "description": truncate(n.summary, 1800),
            "color": color,
            "fields": [
                {"name": "结果", "value": n.code, "inline": true},
                {"name": "影院", "value": truncate(&cinema, 1024), "inline": true},
                {"name": "电影", "value": truncate(&movie, 1024), "inline": true},
                {"name": "场次", "value": truncate(&show_date, 1024), "inline": true},
                {"name": "耗时", "value": format!("{:.1}s", n.elapsed_sec), "inline": true},
                {"name": "请求", "value": n.request_id, "inline": false}
            ],
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    })
}

pub fn build_test_payload() -> Value {
    json!({
        "username": "Gofer",
        "allowed_mentions": {"parse": []},
        "embeds": [{
            "title": "Gofer 测试通知",
            "description": "自动锁票 Discord webhook 配置成功。",
            "color": 0x3f6b4f,
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    })
}

pub async fn send_lock_result(
    client: &reqwest::Client,
    webhook: &str,
    notice: &LockNotice<'_>,
) -> Result<()> {
    send(client, webhook, build_lock_payload(notice)).await
}

pub async fn send_test(client: &reqwest::Client, webhook: &str) -> Result<()> {
    send(client, webhook, build_test_payload()).await
}

async fn send(client: &reqwest::Client, webhook: &str, payload: Value) -> Result<()> {
    validate_webhook(webhook)?;
    // reqwest's Display for transport errors often includes the full URL, which
    // contains the webhook token. Log only the HTTP status / a generic reason.
    let response = client
        .post(webhook)
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            let detail = e
                .status()
                .map(|s| format!("HTTP {s}"))
                .unwrap_or_else(|| "网络错误".to_string());
            anyhow!("Discord 请求失败: {detail}")
        })?;
    if !response.status().is_success() {
        return Err(anyhow!("Discord webhook HTTP {}", response.status()));
    }
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_discord_webhooks() {
        assert!(validate_webhook("https://discord.com/api/webhooks/1/token").is_ok());
        assert!(validate_webhook("https://discordapp.com/api/webhooks/1/token").is_ok());
        assert!(validate_webhook("https://canary.discord.com/api/webhooks/1/token").is_ok());
        assert!(validate_webhook("http://discord.com/api/webhooks/1/token").is_err());
        assert!(validate_webhook("https://evil.example/api/webhooks/1/token").is_err());
        assert!(validate_webhook("https://discord.com/not-a-webhook").is_err());
        assert!(validate_webhook("").is_err());
    }

    #[test]
    fn builds_success_and_failure_payloads() {
        let notice = LockNotice {
            request_id: "g_test",
            cinema_id: "37534",
            cinema_name: "Test Cinema",
            movie_id: 1545360,
            movie_name: "Test Movie",
            show_date: "2026-11-12",
            code: "Ok",
            summary: "已锁票",
            elapsed_sec: 12.3,
        };
        let success = build_lock_payload(&notice);
        assert_eq!(success["embeds"][0]["title"], "锁票成功");
        assert_eq!(success["embeds"][0]["color"], 0x3f6b4f);
        assert!(success["embeds"][0]["fields"][5]["value"]
            .as_str()
            .unwrap()
            .contains("g_test"));

        let failure = build_lock_payload(&LockNotice {
            code: "ShowGone",
            summary: "场次没了",
            ..notice
        });
        assert_eq!(failure["embeds"][0]["title"], "锁票未成功");
        assert_eq!(failure["embeds"][0]["color"], 0xa64535);
    }

    #[test]
    fn builds_test_payload() {
        let payload = build_test_payload();
        assert_eq!(payload["embeds"][0]["title"], "Gofer 测试通知");
        assert_eq!(
            payload["allowed_mentions"]["parse"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
}
