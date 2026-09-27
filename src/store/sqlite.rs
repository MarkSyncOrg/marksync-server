use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use super::{SyncRecord, SyncUpdate};

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS bookmarks (
        id TEXT PRIMARY KEY NOT NULL,
        bookmarks TEXT,
        version TEXT,
        last_updated INTEGER NOT NULL,
        last_accessed INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS bookmarks_last_accessed ON bookmarks (last_accessed)",
    "CREATE TABLE IF NOT EXISTS new_sync_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ip_address TEXT NOT NULL,
        sync_created INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS new_sync_logs_ip_address ON new_sync_logs (ip_address, expires_at)",
    "CREATE INDEX IF NOT EXISTS new_sync_logs_expires_at ON new_sync_logs (expires_at)",
];

const SELECT_COLUMNS: &str = "id, bookmarks, version, last_updated, last_accessed";

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path`; `:memory:` gives a private
    /// in-memory database.
    pub async fn open(path: &str) -> Result<Self> {
        let in_memory = path == ":memory:";
        let options = if in_memory {
            SqliteConnectOptions::from_str("sqlite::memory:")?
        } else {
            if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("unable to create database directory {}", parent.display()))?;
            }
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
        };
        // Each in-memory connection is its own database, so keep exactly one alive.
        let pool_options = if in_memory {
            SqlitePoolOptions::new().max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None)
        } else {
            SqlitePoolOptions::new().max_connections(8)
        };
        let pool = pool_options
            .connect_with(options)
            .await
            .with_context(|| format!("unable to open SQLite database {path}"))?;
        for statement in SCHEMA {
            sqlx::query(statement).execute(&pool).await.context("unable to initialise SQLite schema")?;
        }
        tracing::info!(path, "storage: SQLite");
        Ok(Self { pool })
    }

    pub async fn create(&self, record: &SyncRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO bookmarks (id, bookmarks, version, last_updated, last_accessed) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.bookmarks)
        .bind(&record.version)
        .bind(record.last_updated)
        .bind(record.last_accessed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find(&self, id: &str) -> Result<Option<SyncRecord>> {
        let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM bookmarks WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| to_record(&r)).transpose()
    }

    pub async fn touch(&self, id: &str, now: i64) -> Result<Option<SyncRecord>> {
        let row =
            sqlx::query(&format!("UPDATE bookmarks SET last_accessed = ? WHERE id = ? RETURNING {SELECT_COLUMNS}"))
                .bind(now)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|r| to_record(&r)).transpose()
    }

    pub async fn update(&self, id: &str, update: &SyncUpdate<'_>) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE bookmarks
             SET bookmarks = ?, version = COALESCE(?, version), last_updated = ?, last_accessed = ?
             WHERE id = ? AND (? IS NULL OR last_updated = ?)",
        )
        .bind(update.bookmarks)
        .bind(update.version)
        .bind(update.now)
        .bind(update.now)
        .bind(id)
        .bind(update.expected_last_updated)
        .bind(update.expected_last_updated)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn count_syncs(&self) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks").fetch_one(&self.pool).await?;
        Ok(count as u64)
    }

    pub async fn count_new_sync_logs(&self, ip: &str, now: i64) -> Result<u64> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM new_sync_logs WHERE ip_address = ? AND expires_at > ?")
                .bind(ip)
                .bind(now)
                .fetch_one(&self.pool)
                .await?;
        Ok(count as u64)
    }

    pub async fn add_new_sync_log(&self, ip: &str, created: i64, expires_at: i64) -> Result<()> {
        sqlx::query("INSERT INTO new_sync_logs (ip_address, sync_created, expires_at) VALUES (?, ?, ?)")
            .bind(ip)
            .bind(created)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn purge(&self, now: i64, stale_before: Option<i64>) -> Result<(u64, u64)> {
        let logs = sqlx::query("DELETE FROM new_sync_logs WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let syncs = match stale_before {
            Some(cutoff) => sqlx::query("DELETE FROM bookmarks WHERE last_accessed < ?")
                .bind(cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
            None => 0,
        };
        Ok((syncs, logs))
    }
}

fn to_record(row: &sqlx::sqlite::SqliteRow) -> Result<SyncRecord> {
    Ok(SyncRecord {
        id: row.try_get("id")?,
        bookmarks: row.try_get("bookmarks")?,
        version: row.try_get("version")?,
        last_updated: row.try_get("last_updated")?,
        last_accessed: row.try_get("last_accessed")?,
    })
}
