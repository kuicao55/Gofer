//! Gofer: server + CLI client.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use gofer::server::{self, ServerConfig};

#[derive(Parser, Debug)]
#[command(
    name = "gofer",
    version,
    about = "Local action service: WeChat / XHS / Maoyan lock"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the HTTP service and web panel.
    Serve {
        #[arg(long, env = "GOFER_BIND", default_value = "127.0.0.1:8790")]
        bind: String,
        #[arg(long, env = "GOFER_DB", default_value = "")]
        db: String,
        #[arg(long, env = "GOFER_TOKEN")]
        token: Option<String>,
        /// Simulate WeChat/XHS notifications. Locking is always real.
        #[arg(long, env = "GOFER_DRY_RUN", default_value_t = false)]
        dry_run: bool,
        #[arg(long, env = "GOFER_LOCK_HEADLESS", default_value_t = true)]
        lock_headless: bool,
        #[arg(long, env = "GOFER_RETENTION_DAYS", default_value_t = 30)]
        retention_days: u32,
        /// Optional public/base URL shown in the panel's copyable access block.
        #[arg(long, env = "GOFER_PUBLIC_URL")]
        public_url: Option<String>,
        /// Mark the admin session cookie Secure when TLS terminates in front of Gofer.
        #[arg(long, env = "GOFER_COOKIE_SECURE", default_value_t = false)]
        cookie_secure: bool,
        /// Maoyan city ID used to pick a random cinema for the demo.
        #[arg(long, env = "GOFER_DEMO_CITY_ID", default_value_t = 10)]
        demo_city_id: u32,
        /// Optional fixed cinema ID for the demo (skips the random city API).
        #[arg(long, env = "GOFER_DEMO_CINEMA_ID")]
        demo_cinema_id: Option<String>,
        /// Optional fixed movie ID for the demo.
        #[arg(long, env = "GOFER_DEMO_MOVIE_ID")]
        demo_movie_id: Option<String>,
        /// Demo timeout in seconds.
        #[arg(long, env = "GOFER_DEMO_TIMEOUT_SECS", default_value_t = 600)]
        demo_timeout_secs: u64,
        /// Optional Python interpreter for the vendored demo script.
        #[arg(long, env = "GOFER_DEMO_PYTHON")]
        demo_python: Option<String>,
        /// Test-only command override for the demo worker.
        #[arg(long, env = "GOFER_DEMO_CMD", hide = true)]
        demo_cmd: Option<String>,
    },

    /// Send a WeChat or Xiaohongshu notification.
    Notify {
        #[arg(long, value_parser = ["wechat", "xhs"])]
        channel: String,
        #[arg(long)]
        group: Option<String>,
        #[arg(long, short)]
        title: Option<String>,
        #[arg(long, short)]
        message: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value = "cli")]
        source: String,
        #[arg(long, env = "GOFER_URL", default_value = "http://127.0.0.1:8790")]
        server: String,
        #[arg(long, env = "GOFER_TOKEN")]
        token: Option<String>,
        /// Poll until the request finishes.
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },

    /// Lock a Maoyan ticket through maoyan-booker.
    Lock {
        #[arg(long, default_value_t = 0)]
        movie_id: i64,
        #[arg(long, default_value = "")]
        movie_name: String,
        #[arg(long)]
        cinema_id: String,
        #[arg(long, default_value = "")]
        cinema_name: String,
        #[arg(long)]
        show_date: String,
        #[arg(long)]
        time_range: Option<String>,
        #[arg(long, default_value_t = 2)]
        num_seats: u64,
        #[arg(long)]
        imax_only: bool,
        #[arg(long = "seat")]
        seats: Vec<String>,
        #[arg(long)]
        headed: bool,
        #[arg(long, default_value = "cli")]
        source: String,
        #[arg(long, env = "GOFER_URL", default_value = "http://127.0.0.1:8790")]
        server: String,
        #[arg(long, env = "GOFER_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },

    /// List recent requests.
    Requests {
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(long, env = "GOFER_URL", default_value = "http://127.0.0.1:8790")]
        server: String,
        #[arg(long, env = "GOFER_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Print service health and capability readiness.
    Health {
        #[arg(long, env = "GOFER_URL", default_value = "http://127.0.0.1:8790")]
        server: String,
        #[arg(long)]
        json: bool,
    },

    /// Print 24h request statistics.
    Status {
        #[arg(long, env = "GOFER_URL", default_value = "http://127.0.0.1:8790")]
        server: String,
        #[arg(long, env = "GOFER_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Print the one-line iGlance service manifest (no server required).
    Manifest {
        #[arg(long, env = "GOFER_BIND", default_value = "127.0.0.1:8790")]
        bind: String,
        /// Advertised base URL; defaults to the local URL derived from --bind.
        #[arg(long, env = "GOFER_PUBLIC_URL")]
        base_url: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            bind,
            db,
            token,
            dry_run,
            lock_headless,
            retention_days,
            public_url,
            cookie_secure,
            demo_city_id,
            demo_cinema_id,
            demo_movie_id,
            demo_timeout_secs,
            demo_python,
            demo_cmd,
        } => {
            init_tracing();
            let db = if db.trim().is_empty() {
                server::default_db_path()
            } else {
                db
            };
            let admin_password = std::env::var("GOFER_ADMIN_PASSWORD")
                .ok()
                .filter(|v| !v.trim().is_empty());
            server::run(ServerConfig {
                bind,
                db,
                token,
                admin_password,
                public_url,
                cookie_secure,
                dry_run,
                lock_headless,
                retention_days,
                demo_city_id,
                demo_cinema_id,
                demo_movie_id,
                demo_timeout_secs,
                demo_python,
                demo_cmd,
            })
            .await
        }
        Command::Notify {
            channel,
            group,
            title,
            message,
            url,
            source,
            server,
            token,
            wait,
            json,
        } => {
            let body = json!({
                "channel": channel,
                "group": group,
                "title": title,
                "message": message,
                "url": url,
                "source": source,
            });
            let client = Client::new(server, token)?;
            let created = client.post("/v1/notify", &body, &source).await?;
            print_created(&created, json)?;
            if wait {
                let id = created["request_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing request_id"))?;
                let ok = client.wait(id, 300).await?;
                if !ok {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        Command::Lock {
            movie_id,
            movie_name,
            cinema_id,
            cinema_name,
            show_date,
            time_range,
            num_seats,
            imax_only,
            seats,
            headed,
            source,
            server,
            token,
            wait,
            json,
        } => {
            let body = json!({
                "movie_id": movie_id,
                "movie_name": movie_name,
                "cinema_id": cinema_id,
                "cinema_name": cinema_name,
                "show_date": show_date,
                "time_range": time_range,
                "num_seats": num_seats,
                "imax_only": imax_only,
                "seats": seats,
                "headless": !headed,
                "source": source,
            });
            let client = Client::new(server, token)?;
            let created = client.post("/v1/lock", &body, &source).await?;
            print_created(&created, json)?;
            if wait {
                let id = created["request_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing request_id"))?;
                let ok = client.wait(id, 300).await?;
                if !ok {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        Command::Requests {
            limit,
            server,
            token,
            json,
        } => {
            let client = Client::new(server, token)?;
            let v = client.get(&format!("/v1/requests?limit={limit}")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let rows = v["requests"].as_array().cloned().unwrap_or_default();
            println!(
                "{:<10} {:<19} {:<8} {:<8} {:<8} TITLE / ERROR",
                "ID", "TIME", "CAP", "STATUS", "DURATION"
            );
            for r in rows {
                println!(
                    "{:<10} {:<19} {:<8} {:<8} {:<8} {}",
                    s(&r, "id"),
                    s(&r, "received_local"),
                    s(&r, "capability"),
                    s(&r, "status"),
                    r.get("duration_ms")
                        .and_then(|v| v.as_i64())
                        .map(|ms| format!("{ms}ms"))
                        .unwrap_or_else(|| "—".into()),
                    first_nonempty(&r, &["error", "summary", "title", "preview"])
                );
            }
            Ok(())
        }
        Command::Health { server, json } => {
            let client = Client::new(server, None)?;
            let v = client.get("/health").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!(
                    "gofer {} · ok={} · token_required={} · queue notify={} lock={} demo={}",
                    s(&v, "version"),
                    v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
                    v.get("token_required")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                    v.pointer("/queue/notify")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    v.pointer("/queue/lock")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    v.pointer("/queue/demo")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                );
                if let Some(r) = v.get("ready") {
                    println!(
                        "ready: wechat={} xhs={} lock={} demo={}",
                        r.get("wechat").and_then(|x| x.as_bool()).unwrap_or(false),
                        r.get("xhs").and_then(|x| x.as_bool()).unwrap_or(false),
                        r.get("lock").and_then(|x| x.as_bool()).unwrap_or(false),
                        r.get("demo").and_then(|x| x.as_bool()).unwrap_or(false)
                    );
                }
            }
            Ok(())
        }
        Command::Status {
            server,
            token,
            json,
        } => {
            let client = Client::new(server, token)?;
            let v = client.get("/v1/stats").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let st = &v["stats"];
            println!(
                "24h: total={} ok={} failed={} queued={} running={}",
                st.get("total").and_then(|x| x.as_i64()).unwrap_or(0),
                st.get("ok").and_then(|x| x.as_i64()).unwrap_or(0),
                st.get("failed").and_then(|x| x.as_i64()).unwrap_or(0),
                st.get("queued").and_then(|x| x.as_i64()).unwrap_or(0),
                st.get("running").and_then(|x| x.as_i64()).unwrap_or(0),
            );
            if let Some(caps) = v["capabilities"].as_array() {
                for c in caps {
                    println!(
                        "  {:<8} total={} ok={} failed={}",
                        s(c, "capability"),
                        c.get("total").and_then(|x| x.as_i64()).unwrap_or(0),
                        c.get("ok").and_then(|x| x.as_i64()).unwrap_or(0),
                        c.get("failed").and_then(|x| x.as_i64()).unwrap_or(0),
                    );
                }
            }
            Ok(())
        }
        Command::Manifest { bind, base_url } => {
            let base = base_url
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| server::base_url_for_bind(&bind));
            println!("{}", gofer::service::manifest_line(&base));
            Ok(())
        }
    }
}

struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    fn new(base: String, token: Option<String>) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("创建 HTTP client 失败")?;
        Ok(Self {
            base,
            token: token.filter(|t| !t.trim().is_empty()),
            http,
        })
    }

    async fn post(&self, path: &str, body: &Value, source: &str) -> Result<Value> {
        let mut req = self
            .http
            .post(format!("{}{}", self.base, path))
            .header("X-Gofer-Source", source)
            .json(body);
        if let Some(token) = self.token.as_deref() {
            req = req
                .header("X-Gofer-Token", token)
                .header("X-Agent-Token", token);
        }
        let resp = req.send().await.context("请求 Gofer 失败")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Gofer 返回 {}: {}",
                resp.status(),
                resp.text().await?
            ));
        }
        Ok(resp.json().await?)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(token) = self.token.as_deref() {
            req = req.header("X-Gofer-Token", token);
        }
        let resp = req.send().await.context("请求 Gofer 失败")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Gofer 返回 {}: {}",
                resp.status(),
                resp.text().await?
            ));
        }
        Ok(resp.json().await?)
    }

    async fn wait(&self, id: &str, timeout_sec: u64) -> Result<bool> {
        let start = std::time::Instant::now();
        loop {
            let v = self.get(&format!("/v1/requests/{id}")).await?;
            let status = v
                .pointer("/request/status")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if status == "ok" {
                let summary = v
                    .pointer("/request/summary")
                    .and_then(|x| x.as_str())
                    .unwrap_or("完成");
                println!("✓ {id} {status}: {summary}");
                return Ok(true);
            }
            if status == "failed" {
                let err = v
                    .pointer("/request/error")
                    .and_then(|x| x.as_str())
                    .unwrap_or("失败");
                eprintln!("✗ {id} {status}: {err}");
                return Ok(false);
            }
            if start.elapsed() > Duration::from_secs(timeout_sec) {
                eprintln!("! {id} 超时（最后状态 {status}）");
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn print_created(v: &Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        println!("{} queued: {}", s(v, "request_id"), s(v, "capability"));
    }
    Ok(())
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn first_nonempty(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
