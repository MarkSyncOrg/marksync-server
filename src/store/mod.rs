//! Persistence for syncs and new-sync logs.
//!
//! Two backends are available: SQLite (default, zero-dependency self-hosting) and MongoDB,
//! which reads and writes the same collections/document shapes as the reference API so an
//! existing xBrowserSync database can be served without migration.

#[cfg(feature = "mongodb")]
pub mod mongo;
#[cfg(feature = "sqlite")]
pub mod sqlite;

use anyhow::Result;

use crate::config::{Config, DbType};

/// A stored sync. Timestamps are milliseconds since the Unix epoch, matching the
/// precision of the reference API's `Date` values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRecord {
    /// UUID v4 as 32 lowercase hex characters.
    pub id: String,
    pub bookmarks: Option<String>,
    pub version: Option<String>,
    pub last_updated: i64,
    pub last_accessed: i64,
}

/// Fields written by an update.
#[derive(Debug, Clone)]
pub struct SyncUpdate<'a> {
    pub bookmarks: &'a str,
    /// Replaces the stored version when set.
    pub version: Option<&'a str>,
    pub now: i64,
    /// Only apply the update if the stored `last_updated` still equals this value.
    pub expected_last_updated: Option<i64>,
}

pub enum Store {
    #[cfg(feature = "sqlite")]
    Sqlite(sqlite::SqliteStore),
    #[cfg(feature = "mongodb")]
    Mongo(mongo::MongoStore),
}

macro_rules! dispatch {
    ($self:ident, $store:ident => $body:expr) => {
        match $self {
            #[cfg(feature = "sqlite")]
            Store::Sqlite($store) => $body,
            #[cfg(feature = "mongodb")]
            Store::Mongo($store) => $body,
        }
    };
}

impl Store {
    /// Connects to the backend selected in the settings, preparing schema and indexes.
    pub async fn connect(config: &Config) -> Result<Self> {
        match config.db.kind {
            #[cfg(feature = "sqlite")]
            DbType::Sqlite => Ok(Self::Sqlite(sqlite::SqliteStore::open(&config.db.path).await?)),
            #[cfg(feature = "mongodb")]
            DbType::Mongodb => Ok(Self::Mongo(mongo::MongoStore::connect(&config.db).await?)),
            #[allow(unreachable_patterns)]
            other => anyhow::bail!("database type {other:?} is not supported by this build"),
        }
    }

    pub async fn create(&self, record: &SyncRecord) -> Result<()> {
        dispatch!(self, s => s.create(record).await)
    }

    pub async fn find(&self, id: &str) -> Result<Option<SyncRecord>> {
        dispatch!(self, s => s.find(id).await)
    }

    /// Sets `last_accessed` to `now` and returns the updated record.
    pub async fn touch(&self, id: &str, now: i64) -> Result<Option<SyncRecord>> {
        dispatch!(self, s => s.touch(id, now).await)
    }

    /// Applies an update; returns `false` if no sync matched the id (and expected timestamp).
    pub async fn update(&self, id: &str, update: &SyncUpdate<'_>) -> Result<bool> {
        dispatch!(self, s => s.update(id, update).await)
    }

    pub async fn count_syncs(&self) -> Result<u64> {
        dispatch!(self, s => s.count_syncs().await)
    }

    /// Counts unexpired new-sync logs for a client IP.
    pub async fn count_new_sync_logs(&self, ip: &str, now: i64) -> Result<u64> {
        dispatch!(self, s => s.count_new_sync_logs(ip, now).await)
    }

    pub async fn add_new_sync_log(&self, ip: &str, created: i64, expires_at: i64) -> Result<()> {
        dispatch!(self, s => s.add_new_sync_log(ip, created, expires_at).await)
    }

    /// Deletes expired new-sync logs and, when `stale_before` is set, syncs last accessed
    /// before it. Returns `(syncs_deleted, logs_deleted)`.
    pub async fn purge(&self, now: i64, stale_before: Option<i64>) -> Result<(u64, u64)> {
        dispatch!(self, s => s.purge(now, stale_before).await)
    }
}
