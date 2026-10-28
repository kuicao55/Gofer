//! SQLite persistence for request history and capability statistics.

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};
use std::path::Path;
use std::str::FromStr;

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct RequestRow {
    pub id: String,
    pub received_at: String,
    pub received_local: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub capability: String,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub source: String,
    pub title: String,
    pub preview: String,
    pub payload_json: String,
    pub result_json: Option<String>,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, FromRow)]
pub struct CapabilityStat {
    pub capability: String,
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub running: i64,
    pub queued: i64,
    pub last_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats24h {
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub running: i64,
    pub queued: i64,
}

const SELECT_REQUEST: &str = "SELECT id, received_at,
    datetime(received_at, 'localtime') AS received_local,
    started_at, finished_at, capability, status, duration_ms, source,
    title, preview, payload_json, result_json, summary, error
    FROM requests";

impl Db {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("连接数据库失败: {}", path.display()))?;
        let db = Self { pool };
        db.migrate().await?;
        db.restrict_db_permissions(path);
        Ok(db)
    }

    /// The settings table may contain the API token and admin password hash, so
    /// keep the database (and its WAL sidecars) readable only by the current user.
    #[cfg(unix)]
    fn restrict_db_permissions(&self, path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut targets = vec![path.to_path_buf()];
        targets.push(std::path::PathBuf::from(format!("{}-wal", path.display())));
        targets.push(std::path::PathBuf::from(format!("{}-shm", path.display())));
        targets.push(std::path::PathBuf::from(format!(
            "{}-journal",
            path.display()
        )));

        for target in targets {
            if !target.exists() {
                continue;
            }
            if let Err(e) =
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!("chmod 0600 {} failed: {e}", target.display());
            }
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!("chmod 0700 {} failed: {e}", parent.display());
            }
        }
    }

    #[cfg(not(unix))]
    fn restrict_db_permissions(&self, _path: &Path) {}

    pub async fn connect_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
            .journal_mode(SqliteJournalMode::Memory)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn insert_queued(
        &self,
        id: &str,
        capability: &str,
        source: &str,
        title: &str,
        preview: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO requests(id, capability, source, title, preview, payload_json)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(capability)
        .bind(source)
        .bind(title)
        .bind(preview)
        .bind(payload.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_running(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE requests SET status = 'running', started_at = datetime('now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn finish(
        &self,
        id: &str,
        ok: bool,
        duration_ms: i64,
        summary: &str,
        result: Option<&serde_json::Value>,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE requests SET status = ?, finished_at = datetime('now'),
                duration_ms = ?, summary = ?, result_json = ?, error = ?
             WHERE id = ?",
        )
        .bind(if ok { "ok" } else { "failed" })
        .bind(duration_ms)
        .bind(summary)
        .bind(result.map(|v| v.to_string()))
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail_queued(&self, id: &str, error: &str) -> Result<()> {
        self.finish(id, false, 0, error, None, Some(error)).await
    }

    pub async fn get(&self, id: &str) -> Result<Option<RequestRow>> {
        let sql = format!("{SELECT_REQUEST} WHERE id = ?");
        Ok(sqlx::query_as::<_, RequestRow>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn recent(&self, limit: i64) -> Result<Vec<RequestRow>> {
        let sql = format!("{SELECT_REQUEST} ORDER BY received_at DESC, id DESC LIMIT ?");
        Ok(sqlx::query_as::<_, RequestRow>(&sql)
            .bind(limit.clamp(1, 500))
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn stats_24h(&self) -> Result<Stats24h> {
        let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'queued' THEN 1 ELSE 0 END), 0)
             FROM requests WHERE received_at >= datetime('now', '-24 hours')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Stats24h {
            total: row.0,
            ok: row.1,
            failed: row.2,
            running: row.3,
            queued: row.4,
        })
    }

    pub async fn capability_stats(&self) -> Result<Vec<CapabilityStat>> {
        Ok(sqlx::query_as::<_, CapabilityStat>(
            "SELECT capability, COUNT(*) AS total,
                COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0) AS ok,
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failed,
                COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0) AS running,
                COALESCE(SUM(CASE WHEN status = 'queued' THEN 1 ELSE 0 END), 0) AS queued,
                datetime(MAX(received_at), 'localtime') AS last_at
             FROM requests
             WHERE received_at >= datetime('now', '-24 hours')
             GROUP BY capability",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn last_for_capability(
        &self,
        capability: &str,
        ok: bool,
    ) -> Result<Option<RequestRow>> {
        let sql = format!(
            "{SELECT_REQUEST} WHERE capability = ? AND status = ? \
             ORDER BY received_at DESC, id DESC LIMIT 1"
        );
        Ok(sqlx::query_as::<_, RequestRow>(&sql)
            .bind(capability)
            .bind(if ok { "ok" } else { "failed" })
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn paused(&self) -> Result<bool> {
        let v: i64 = sqlx::query_scalar("SELECT paused FROM service_state WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or(0);
        Ok(v != 0)
    }

    pub async fn set_paused(&self, paused: bool) -> Result<()> {
        sqlx::query(
            "INSERT INTO service_state(id, paused, updated_at)
             VALUES (1, ?, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET paused = excluded.paused,
                                           updated_at = datetime('now')",
        )
        .bind(if paused { 1 } else { 0 })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn total_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&self.pool)
            .await?)
    }

    // ------------------------- settings -------------------------

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings(key, value, updated_at)
             VALUES (?, ?, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                           updated_at = datetime('now')",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Insert only if the key is absent. Returns true when this call created it.
    /// Used to make first-run admin setup safe under concurrent requests.
    pub async fn set_setting_if_absent(&self, key: &str, value: &str) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO settings(key, value) VALUES (?, ?)
             ON CONFLICT(key) DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn delete_setting(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn prune(&self, days: u32) -> Result<u64> {
        let modifier = format!("-{} days", days.max(1));
        let r = sqlx::query("DELETE FROM requests WHERE received_at < datetime('now', ?)")
            .bind(modifier)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Mark requests of a capability that were left queued/running by a restart.
    pub async fn fail_incomplete_capability(&self, capability: &str, error: &str) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE requests SET status = 'failed', finished_at = datetime('now'),
                summary = ?, error = ?
             WHERE capability = ? AND status IN ('queued', 'running')",
        )
        .bind(error)
        .bind(error)
        .bind(capability)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn request_lifecycle_and_stats() {
        let db = Db::connect_memory().await.unwrap();
        db.insert_queued(
            "g_1",
            "wechat",
            "cli",
            "title",
            "hello",
            &json!({"channel": "wechat"}),
        )
        .await
        .unwrap();
        db.mark_running("g_1").await.unwrap();
        db.finish(
            "g_1",
            true,
            123,
            "微信已发送",
            Some(&json!({"kind": "notify"})),
            None,
        )
        .await
        .unwrap();
        let row = db.get("g_1").await.unwrap().unwrap();
        assert_eq!(row.status, "ok");
        assert_eq!(row.duration_ms, Some(123));
        assert_eq!(row.summary, "微信已发送");

        let stats = db.stats_24h().await.unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.ok, 1);
        let caps = db.capability_stats().await.unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "wechat");
        assert_eq!(caps[0].ok, 1);
    }

    #[tokio::test]
    async fn paused_state_roundtrip() {
        let db = Db::connect_memory().await.unwrap();
        assert!(!db.paused().await.unwrap());
        db.set_paused(true).await.unwrap();
        assert!(db.paused().await.unwrap());
        db.set_paused(false).await.unwrap();
        assert!(!db.paused().await.unwrap());
    }

    #[tokio::test]
    async fn failed_request_is_recorded() {
        let db = Db::connect_memory().await.unwrap();
        db.insert_queued("g_2", "lock", "api", "", "", &json!({}))
            .await
            .unwrap();
        db.finish("g_2", false, 9, "失败", None, Some("booker 不存在"))
            .await
            .unwrap();
        let row = db.get("g_2").await.unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("booker 不存在"));
    }

    #[tokio::test]
    async fn settings_roundtrip_and_absent_vs_empty() {
        let db = Db::connect_memory().await.unwrap();
        assert_eq!(db.get_setting("api.token").await.unwrap(), None);

        db.set_setting("api.token", "").await.unwrap();
        assert_eq!(
            db.get_setting("api.token").await.unwrap(),
            Some(String::new())
        );

        db.set_setting("api.token", "first").await.unwrap();
        assert_eq!(
            db.get_setting("api.token").await.unwrap().as_deref(),
            Some("first")
        );
        db.set_setting("api.token", "second").await.unwrap();
        assert_eq!(
            db.get_setting("api.token").await.unwrap().as_deref(),
            Some("second")
        );

        db.delete_setting("api.token").await.unwrap();
        assert_eq!(db.get_setting("api.token").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_setting_if_absent_only_wins_once() {
        let db = Db::connect_memory().await.unwrap();
        assert!(db
            .set_setting_if_absent("admin.password_hash", "one")
            .await
            .unwrap());
        assert!(!db
            .set_setting_if_absent("admin.password_hash", "two")
            .await
            .unwrap());
        assert_eq!(
            db.get_setting("admin.password_hash")
                .await
                .unwrap()
                .as_deref(),
            Some("one")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_db_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gofer.db");
        let db = Db::connect(&path).await.unwrap();
        db.set_setting("session.secret", "abc").await.unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }
}
