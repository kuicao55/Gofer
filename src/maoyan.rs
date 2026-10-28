//! Small Maoyan helpers for the demo: pick a random cinema in a city.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cinema {
    pub id: String,
    pub name: String,
    pub addr: String,
}

/// Fetch a few pages of the city cinema list and pick one uniformly at random.
pub async fn random_cinema(client: &reqwest::Client, city_id: u32) -> Result<Cinema> {
    let mut all = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for offset in [0u32, 10, 20] {
        let (items, has_more) = fetch_page(client, city_id, offset).await?;
        let count = items.len();
        for cinema in items {
            if seen.insert(cinema.id.clone()) {
                all.push(cinema);
            }
        }
        if !has_more || count < 10 {
            break;
        }
    }

    if all.is_empty() {
        return Err(anyhow!("该城市没有可用影院（city_id={city_id}）"));
    }
    let idx = rand::random::<usize>() % all.len();
    all.into_iter()
        .nth(idx)
        .ok_or_else(|| anyhow!("随机选择影院失败"))
}

async fn fetch_page(
    client: &reqwest::Client,
    city_id: u32,
    offset: u32,
) -> Result<(Vec<Cinema>, bool)> {
    let url = format!(
        "https://m.maoyan.com/ajax/cinemaList?ci={city_id}&districtId=-1&lineId=-1\
         &hallType=-1&type=-1&offset={offset}"
    );
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, MOBILE_UA)
        .header(reqwest::header::REFERER, "https://m.maoyan.com/shows/")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(anyhow!("猫眼影院列表 HTTP {}", response.status()));
    }
    let data: Value = response.json().await?;
    Ok(parse_page(&data))
}

fn parse_page(data: &Value) -> (Vec<Cinema>, bool) {
    let mut items = Vec::new();
    if let Some(array) = data.get("cinemas").and_then(|v| v.as_array()) {
        for cinema in array {
            let id = cinema.get("id").and_then(|v| {
                if let Some(n) = v.as_i64() {
                    (n > 0).then(|| n.to_string())
                } else {
                    v.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                }
            });
            let name = cinema
                .get("nm")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let addr = cinema
                .get("addr")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if let Some(id) = id.filter(|v| !v.is_empty()) {
                if !name.is_empty() {
                    items.push(Cinema { id, name, addr });
                }
            }
        }
    }
    let has_more = data
        .get("paging")
        .and_then(|p| p.get("hasMore"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (items, has_more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_numeric_and_string_ids() {
        let data = json!({
            "cinemas": [
                {"id": 27664, "nm": "CGV", "addr": "A"},
                {"id": "38685", "nm": "CMG", "addr": "B"},
                {"id": 0, "nm": "bad", "addr": "C"},
                {"id": 1, "nm": "", "addr": "D"}
            ],
            "paging": {"hasMore": true}
        });
        let (items, has_more) = parse_page(&data);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "27664");
        assert_eq!(items[1].id, "38685");
        assert!(has_more);
    }

    #[test]
    fn handles_empty_and_missing_paging() {
        assert!(parse_page(&json!({})).0.is_empty());
        let (items, has_more) = parse_page(&json!({"cinemas": []}));
        assert!(items.is_empty());
        assert!(!has_more);
    }
}
