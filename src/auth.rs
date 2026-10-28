//! Panel authentication, admin sessions and the runtime API token.
//!
//! The panel admin password is completely separate from the API token: a caller
//! that knows `GOFER_TOKEN` must not be able to open the panel or rotate it.

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::db::Db;
use crate::server::AppState;

pub const SESSION_COOKIE: &str = "gofer_session";
pub const SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;
pub const ADMIN_HASH_KEY: &str = "admin.password_hash";
pub const SESSION_SECRET_KEY: &str = "session.secret";
pub const API_TOKEN_KEY: &str = "api.token";
pub const API_TOKEN_UPDATED_KEY: &str = "api.token_updated_at";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    None,
    Env,
    Database,
}

impl TokenSource {
    pub fn label(self) -> &'static str {
        match self {
            TokenSource::None => "未设置",
            TokenSource::Env => "环境变量 GOFER_TOKEN",
            TokenSource::Database => "面板管理（数据库）",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiToken {
    pub value: Option<String>,
    pub source: TokenSource,
}

impl ApiToken {
    pub fn none() -> Self {
        Self {
            value: None,
            source: TokenSource::None,
        }
    }

    pub fn is_on(&self) -> bool {
        self.value.as_deref().is_some_and(|v| !v.trim().is_empty())
    }

    pub fn label(&self) -> Option<&str> {
        self.value.as_deref().filter(|v| !v.trim().is_empty())
    }
}

/// Resolve the effective API token.
///
/// A non-empty DB value wins. An explicitly empty DB value disables API auth
/// even if `GOFER_TOKEN` is set. An absent DB value falls back to the env token.
pub fn resolve_api_token(db_value: Option<&str>, env_token: Option<String>) -> ApiToken {
    match db_value {
        Some(v) if !v.trim().is_empty() => ApiToken {
            value: Some(v.to_string()),
            source: TokenSource::Database,
        },
        Some(_) => ApiToken::none(),
        None => {
            let env = env_token
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            match env {
                Some(v) => ApiToken {
                    value: Some(v),
                    source: TokenSource::Env,
                },
                None => ApiToken::none(),
            }
        }
    }
}

pub struct AuthState {
    admin_hash: RwLock<Option<String>>,
    session_secret: RwLock<Vec<u8>>,
    env_password_managed: bool,
    /// Serializes first-run setup so only one request can create the password.
    pub setup_lock: tokio::sync::Mutex<()>,
    /// Serializes API token rotations (DB write + memory update).
    pub token_lock: tokio::sync::Mutex<()>,
}

impl AuthState {
    pub fn new(
        admin_hash: Option<String>,
        session_secret: Vec<u8>,
        env_password_managed: bool,
    ) -> Self {
        Self {
            admin_hash: RwLock::new(admin_hash),
            session_secret: RwLock::new(session_secret),
            env_password_managed,
            setup_lock: tokio::sync::Mutex::new(()),
            token_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Load (and, when needed, initialise) admin credentials from SQLite.
    ///
    /// `env_password` is an optional reset path: when it differs from the stored
    /// hash it is hashed and persisted, and the session secret is rotated so all
    /// existing sessions are invalidated.
    pub async fn load(db: &Db, env_password: Option<&str>) -> Result<Self> {
        let env_password = env_password.map(str::trim).filter(|p| !p.is_empty());

        if let Some(pw) = env_password {
            if pw.chars().count() < 8 {
                return Err(anyhow!("GOFER_ADMIN_PASSWORD 至少需要 8 个字符"));
            }
        }

        let mut admin_hash = db.get_setting(ADMIN_HASH_KEY).await?;
        if let Some(hash) = admin_hash.as_deref() {
            if PasswordHash::new(hash).is_err() {
                tracing::error!("管理员密码哈希损坏；请设置 GOFER_ADMIN_PASSWORD 后重启以重置");
            }
        }

        let mut rotate_sessions = false;
        if let Some(pw) = env_password {
            let matches = match admin_hash.as_deref() {
                Some(hash) => verify_password_async(pw.to_string(), hash.to_string())
                    .await
                    .unwrap_or(false),
                None => false,
            };
            if !matches {
                let hash = hash_password_async(pw.to_string()).await?;
                db.set_setting(ADMIN_HASH_KEY, &hash).await?;
                admin_hash = Some(hash);
                rotate_sessions = true;
                tracing::info!("已从 GOFER_ADMIN_PASSWORD 重置管理员密码");
            }
        }

        let existing_secret = db.get_setting(SESSION_SECRET_KEY).await?;
        let valid_existing = existing_secret
            .as_deref()
            .and_then(decode_hex)
            .filter(|b| b.len() == 32);
        let secret = match (rotate_sessions, valid_existing) {
            (false, Some(secret)) => secret,
            _ => {
                let secret = random_bytes(32);
                db.set_setting(SESSION_SECRET_KEY, &hex_encode(&secret))
                    .await?;
                secret
            }
        };

        Ok(Self::new(admin_hash, secret, env_password.is_some()))
    }

    pub fn admin_hash(&self) -> Option<String> {
        self.admin_hash
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_admin_hash(&self, hash: Option<String>) {
        *self.admin_hash.write().unwrap_or_else(|e| e.into_inner()) = hash;
    }

    pub fn has_admin_password(&self) -> bool {
        self.admin_hash()
            .as_deref()
            .is_some_and(|h| !h.trim().is_empty())
    }

    pub fn env_password_managed(&self) -> bool {
        self.env_password_managed
    }

    pub fn session_secret(&self) -> Vec<u8> {
        self.session_secret
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_session_secret(&self, secret: Vec<u8>) {
        *self
            .session_secret
            .write()
            .unwrap_or_else(|e| e.into_inner()) = secret;
    }

    pub fn rotate_session_secret(&self) -> Vec<u8> {
        let secret = random_bytes(32);
        self.set_session_secret(secret.clone());
        secret
    }

    pub async fn verify_admin_password(&self, password: String) -> bool {
        let Some(hash) = self.admin_hash() else {
            return false;
        };
        verify_password_async(password, hash).await.unwrap_or(false)
    }

    pub fn issue_session(&self) -> String {
        self.issue_session_at(now_secs())
    }

    fn issue_session_at(&self, issued_at: u64) -> String {
        let expires_at = issued_at.saturating_add(SESSION_TTL_SECS);
        let nonce = random_hex(16);
        let payload = format!("v1.{issued_at}.{expires_at}.{nonce}");
        let signature = sign(&self.session_secret(), payload.as_bytes());
        format!("{payload}.{signature}")
    }

    pub fn valid_session(&self, value: &str) -> bool {
        self.valid_session_at(value, now_secs())
    }

    fn valid_session_at(&self, value: &str, now: u64) -> bool {
        let Some((payload, signature)) = value.rsplit_once('.') else {
            return false;
        };
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 4 || parts[0] != "v1" {
            return false;
        }
        let Ok(issued_at) = parts[1].parse::<u64>() else {
            return false;
        };
        let Ok(expires_at) = parts[2].parse::<u64>() else {
            return false;
        };
        if expires_at <= now || issued_at > now.saturating_add(300) {
            return false;
        }
        let Some(signature) = decode_hex(signature) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(&self.session_secret())
            .expect("hmac accepts arbitrary key lengths");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).is_ok()
    }
}

pub async fn hash_password_async(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| anyhow!("密码哈希任务失败: {e}"))?
}

pub async fn verify_password_async(password: String, phc: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || verify_password(&password, &phc))
        .await
        .map_err(|e| anyhow!("密码校验任务失败: {e}"))?
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("Argon2 哈希失败: {e}"))
}

fn verify_password(password: &str, phc: &str) -> Result<bool> {
    let parsed = PasswordHash::new(phc).map_err(|e| anyhow!("无效的密码哈希: {e}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// Constant-time API token comparison (hash both sides first so the length is
/// not the only thing being compared).
pub fn token_matches(expected: &str, provided: &str) -> bool {
    let expected = Sha256::digest(expected.as_bytes());
    let provided = Sha256::digest(provided.as_bytes());
    expected.as_slice().ct_eq(provided.as_slice()).into()
}

pub fn session_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if name == SESSION_COOKIE {
            Some(value.to_string())
        } else {
            None
        }
    })
}

pub fn session_cookie(value: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL_SECS}{secure}"
    )
}

pub fn clear_session_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}")
}

/// Best-effort CSRF defence for cookie-authenticated POSTs. SameSite=Strict is
/// the primary protection; this also rejects obviously cross-origin browser
/// requests when the browser sends Origin/Referer.
pub fn same_origin_ok(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }

    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !authority_matches(origin, host) {
            return false;
        }
    }
    if let Some(referer) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
        if !authority_matches(referer, host) {
            return false;
        }
    }
    true
}

fn authority_matches(url: &str, host: &str) -> bool {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    authority.eq_ignore_ascii_case(host)
}

pub fn is_loopback_bind(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let host = host.trim_matches(|c| c == '[' || c == ']');
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub async fn require_admin(
    State(st): State<std::sync::Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let authenticated =
        session_from_headers(req.headers()).is_some_and(|value| st.auth.valid_session(&value));
    if !authenticated {
        return auth_redirect(&st, req.headers());
    }

    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn auth_redirect(st: &AppState, headers: &HeaderMap) -> Response {
    let target = if st.auth.has_admin_password() {
        "/login"
    } else {
        "/setup"
    };
    if headers.contains_key("hx-request") {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        if let Ok(value) = HeaderValue::from_str(target) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("hx-redirect"), value);
        }
        return response;
    }
    Redirect::to(target).into_response()
}

pub fn random_hex(bytes: usize) -> String {
    hex_encode(&random_bytes(bytes))
}

pub fn random_token() -> String {
    format!("gof_{}", random_hex(32))
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub fn decode_hex(value: &str) -> Option<Vec<u8>> {
    // Avoid `usize::is_multiple_of` to keep the project's Rust 1.75 MSRV.
    if value.len() & 1 != 0 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
        .collect()
}

fn sign(secret: &[u8], payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts arbitrary key lengths");
    mac.update(payload);
    hex_encode(&mac.finalize().into_bytes())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn auth() -> AuthState {
        AuthState::new(
            Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$Zm9v".into()),
            vec![7u8; 32],
            false,
        )
    }

    #[test]
    fn session_roundtrip_and_tampering() {
        let auth = auth();
        let now = 1_700_000_000;
        let value = auth.issue_session_at(now);
        assert!(auth.valid_session_at(&value, now + 1));
        assert!(!auth.valid_session_at(&value, now + SESSION_TTL_SECS));
        assert!(!auth.valid_session_at(&format!("{value}x"), now + 1));
        assert!(!auth.valid_session_at("v1.1.2.abc", now + 1));

        let other = AuthState::new(None, vec![8u8; 32], false);
        other.set_session_secret(vec![9u8; 32]);
        assert!(!other.valid_session_at(&value, now + 1));
    }

    #[test]
    fn session_rejects_future_issue_time() {
        let auth = auth();
        let now = 1_700_000_000;
        let value = auth.issue_session_at(now + 301);
        assert!(!auth.valid_session_at(&value, now));
    }

    #[test]
    fn cookie_parsing_and_attributes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; gofer_session=abc.def; b=2"),
        );
        assert_eq!(session_from_headers(&headers).as_deref(), Some("abc.def"));
        let cookie = session_cookie("abc", true);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Secure"));
        assert!(clear_session_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn same_origin_checks() {
        let mut headers = HeaderMap::new();
        assert!(same_origin_ok(&headers));
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8790"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8790"),
        );
        assert!(same_origin_ok(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(!same_origin_ok(&headers));
        headers.remove(header::ORIGIN);
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin_ok(&headers));
    }

    #[test]
    fn loopback_and_token_resolution() {
        assert!(is_loopback_bind("127.0.0.1:8790"));
        assert!(is_loopback_bind("localhost:8790"));
        assert!(is_loopback_bind("[::1]:8790"));
        assert!(!is_loopback_bind("0.0.0.0:8790"));

        let db = resolve_api_token(Some("db-token"), Some("env-token".into()));
        assert_eq!(db.source, TokenSource::Database);
        assert_eq!(db.value.as_deref(), Some("db-token"));

        let env = resolve_api_token(None, Some("env-token".into()));
        assert_eq!(env.source, TokenSource::Env);
        assert_eq!(env.value.as_deref(), Some("env-token"));

        let disabled = resolve_api_token(Some(""), Some("env-token".into()));
        assert_eq!(disabled.source, TokenSource::None);
        assert!(!disabled.is_on());

        let none = resolve_api_token(None, Some("  ".into()));
        assert_eq!(none.source, TokenSource::None);
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("abc", "ab"));
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = random_bytes(32);
        let encoded = hex_encode(&bytes);
        assert_eq!(decode_hex(&encoded), Some(bytes));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[tokio::test]
    async fn password_hash_and_verify() {
        let hash = hash_password_async("correct horse".into()).await.unwrap();
        assert!(verify_password_async("correct horse".into(), hash.clone())
            .await
            .unwrap());
        assert!(!verify_password_async("wrong".into(), hash).await.unwrap());
    }
}
