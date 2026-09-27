//! Business rules of the sync API, mirroring the reference `BookmarksService`,
//! `InfoService` and `NewSyncLogsService`.

use chrono::{DateTime, Days, Local, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::config::{API_VERSION, Config};
use crate::error::ApiError;
use crate::store::{Store, SyncRecord, SyncUpdate};

/// `/info` status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ServiceStatus {
    Online = 1,
    Offline = 2,
    NoNewSyncs = 3,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoResponse {
    pub location: String,
    pub max_sync_size: usize,
    pub message: String,
    pub status: u8,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateResponse {
    pub id: String,
    pub last_updated: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResponse {
    /// Empty until the first upload; the reference API omits the field in that case,
    /// the contract requires it, and clients treat both the same.
    pub bookmarks: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub last_updated: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastUpdatedResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VersionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

pub struct Service {
    pub config: Config,
    store: Store,
}

impl Service {
    pub fn new(config: Config, store: Store) -> Self {
        Self { config, store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn info(&self) -> InfoResponse {
        let mut status = ServiceStatus::Offline;
        if self.config.status.online {
            match self.is_accepting_new_syncs().await {
                Ok(true) => status = ServiceStatus::Online,
                Ok(false) => status = ServiceStatus::NoNewSyncs,
                Err(err) => tracing::error!(error = %format!("{err:#}"), "unable to determine service status"),
            }
        }
        InfoResponse {
            location: self.config.location.to_ascii_uppercase(),
            max_sync_size: self.config.max_sync_size,
            message: strip_scripts(&self.config.status.message),
            status: status as u8,
            version: API_VERSION,
        }
    }

    /// Legacy (`~1.0.0`) create: stores bookmarks immediately, no version.
    pub async fn create_v1(&self, bookmarks: String, client_ip: Option<&str>) -> Result<CreateResponse, ApiError> {
        self.create(Some(bookmarks), None, client_ip).await
    }

    /// Current (`^1.1.3`) create: an empty sync tagged with the client's sync version.
    pub async fn create_v2(&self, version: String, client_ip: Option<&str>) -> Result<CreateResponse, ApiError> {
        self.create(None, Some(version), client_ip).await
    }

    async fn create(
        &self,
        bookmarks: Option<String>,
        version: Option<String>,
        client_ip: Option<&str>,
    ) -> Result<CreateResponse, ApiError> {
        self.check_available()?;
        if !self.is_accepting_new_syncs().await? {
            tracing::warn!("new sync refused: service is not accepting new syncs (allowNewSyncs or maxSyncs)");
            return Err(ApiError::NewSyncsForbidden);
        }
        let limit = self.config.daily_new_syncs_limit;
        let now = now_millis();
        if limit > 0 {
            if let Some(ip) = client_ip {
                if self.store.count_new_sync_logs(ip, now).await? >= limit {
                    tracing::warn!(client_ip = ip, limit, "new sync refused: daily new syncs limit reached");
                    return Err(ApiError::NewSyncsLimitExceeded);
                }
            }
        }

        let record = SyncRecord {
            id: uuid::Uuid::new_v4().simple().to_string(),
            bookmarks,
            version,
            last_updated: now,
            last_accessed: now,
        };
        self.store.create(&record).await?;

        if limit > 0 {
            match client_ip {
                Some(ip) => self.store.add_new_sync_log(ip, now, start_of_tomorrow()).await?,
                None => tracing::info!("unable to determine client IP address"),
            }
        }
        tracing::info!(
            sync = short_id(&record.id),
            version = record.version.as_deref().unwrap_or("-"),
            legacy = record.bookmarks.is_some(),
            "sync created"
        );

        Ok(CreateResponse { id: record.id, last_updated: to_iso(record.last_updated), version: record.version })
    }

    pub async fn get_bookmarks(&self, id: &str) -> Result<GetResponse, ApiError> {
        let record = self.touch(id).await?;
        Ok(GetResponse {
            bookmarks: record.bookmarks.unwrap_or_default(),
            version: record.version,
            last_updated: to_iso(record.last_updated),
        })
    }

    pub async fn get_last_updated(&self, id: &str) -> Result<LastUpdatedResponse, ApiError> {
        let record = self.touch(id).await?;
        Ok(LastUpdatedResponse { last_updated: Some(to_iso(record.last_updated)) })
    }

    pub async fn get_version(&self, id: &str) -> Result<VersionResponse, ApiError> {
        let record = self.touch(id).await?;
        Ok(VersionResponse { version: record.version })
    }

    /// Legacy (`~1.0.0`) update: unconditional overwrite. Like the reference API, an
    /// unknown sync yields an empty object rather than an error.
    pub async fn update_v1(&self, id: &str, bookmarks: &str) -> Result<LastUpdatedResponse, ApiError> {
        self.check_available()?;
        let Some(existing) = self.store.find(id).await? else {
            tracing::info!(sync = short_id(id), "legacy update ignored: sync not found");
            return Ok(LastUpdatedResponse { last_updated: None });
        };
        let now = next_timestamp(existing.last_updated);
        let update = SyncUpdate { bookmarks, version: None, now, expected_last_updated: None };
        let updated = self.store.update(id, &update).await?;
        if updated {
            tracing::info!(sync = short_id(id), bytes = bookmarks.len(), legacy = true, "sync updated");
        }
        Ok(LastUpdatedResponse { last_updated: updated.then(|| to_iso(now)) })
    }

    /// Current (`^1.1.3`) update with optimistic concurrency: a truthy `lastUpdated` that
    /// differs from the stored timestamp's ISO string is a conflict.
    pub async fn update_v2(
        &self,
        id: &str,
        bookmarks: &str,
        last_updated: &Value,
        version: Option<&str>,
    ) -> Result<LastUpdatedResponse, ApiError> {
        self.check_available()?;
        let Some(existing) = self.store.find(id).await? else {
            tracing::info!(sync = short_id(id), "update rejected: sync not found");
            return Err(ApiError::SyncNotFound);
        };
        let supplied = is_truthy(last_updated);
        if supplied && last_updated.as_str() != Some(to_iso(existing.last_updated).as_str()) {
            tracing::info!(sync = short_id(id), "update rejected: sync conflict (stale lastUpdated)");
            return Err(ApiError::SyncConflict);
        }

        let now = next_timestamp(existing.last_updated);
        // When the client supplied a timestamp, guard the write so a concurrent update
        // between the read above and this write is also reported as a conflict.
        let expected_last_updated = supplied.then_some(existing.last_updated);
        let update = SyncUpdate { bookmarks, version, now, expected_last_updated };
        if !self.store.update(id, &update).await? {
            tracing::info!(sync = short_id(id), "update rejected: concurrent update");
            return Err(if supplied { ApiError::SyncConflict } else { ApiError::SyncNotFound });
        }
        tracing::info!(sync = short_id(id), bytes = bookmarks.len(), version = version.unwrap_or("-"), "sync updated");
        Ok(LastUpdatedResponse { last_updated: Some(to_iso(now)) })
    }

    async fn touch(&self, id: &str) -> Result<SyncRecord, ApiError> {
        self.check_available()?;
        let record = self.store.touch(id, now_millis()).await?;
        if record.is_none() {
            tracing::debug!(sync = short_id(id), "sync not found");
        }
        record.ok_or(ApiError::SyncNotFound)
    }

    fn check_available(&self) -> Result<(), ApiError> {
        if self.config.status.online { Ok(()) } else { Err(ApiError::ServiceNotAvailable) }
    }

    async fn is_accepting_new_syncs(&self) -> anyhow::Result<bool> {
        if !self.config.status.allow_new_syncs {
            return Ok(false);
        }
        if self.config.max_syncs == 0 {
            return Ok(true);
        }
        Ok(self.store.count_syncs().await? < self.config.max_syncs)
    }

    /// Deletes expired new-sync logs and syncs idle for longer than `syncExpiryDays`.
    pub async fn purge(&self) -> anyhow::Result<(u64, u64)> {
        let now = now_millis();
        let days = self.config.sync_expiry_days;
        let stale_before = (days > 0).then(|| now - i64::try_from(days).unwrap_or(i64::MAX / 86_400_000) * 86_400_000);
        self.store.purge(now, stale_before).await
    }
}

/// Validates a sync ID the way the reference API does (UUID parse + round-trip): hyphens
/// are ignored and the rest must be exactly 32 lowercase hex digits. Returns the
/// canonical, hyphen-free ID.
pub fn parse_sync_id(raw: &str) -> Result<String, ApiError> {
    let id: String = raw.chars().filter(|&c| c != '-').collect();
    if id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(id)
    } else {
        Err(ApiError::InvalidSyncId)
    }
}

/// First 8 characters of a sync ID: enough to correlate log lines, not enough to use the
/// sync (anyone holding a full ID can overwrite its data).
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// A new `lastUpdated` strictly greater than the previous one, so two writes within the
/// same millisecond still produce distinct timestamps for conflict detection.
fn next_timestamp(previous: i64) -> i64 {
    now_millis().max(previous + 1)
}

/// JavaScript `Date#toISOString` format: `YYYY-MM-DDTHH:mm:ss.sssZ`.
pub fn to_iso(millis: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap_or_default().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Midnight at the start of the next local day, when a client's new-sync count resets.
fn start_of_tomorrow() -> i64 {
    let tomorrow = Local::now().date_naive() + Days::new(1);
    tomorrow
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| now_millis() + 86_400_000)
}

/// JavaScript truthiness of a JSON value.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Mongoose `String` casting of a request field: `None` for falsy values, an error
/// (surfacing as `UnspecifiedException`) for values that cannot be cast.
pub fn cast_string(value: &Value) -> Result<Option<String>, ApiError> {
    if !is_truthy(value) {
        return Ok(None);
    }
    match value {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        _ => Err(ApiError::Unspecified),
    }
}

/// Removes `<script>…</script>` elements, like the reference API's regex
/// `/<script\b[^<]*(?:(?!<\/script>)<[^<]*)*<\/script>/gi`.
pub fn strip_scripts(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(found) = lower[pos..].find("<script") {
        let start = pos + found;
        let after = start + "<script".len();
        let boundary = lower[after..].chars().next().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let end = boundary.then(|| lower[after..].find("</script>")).flatten();
        match end {
            Some(end) => {
                out.push_str(&html[pos..start]);
                pos = after + end + "</script>".len();
            }
            None => {
                out.push_str(&html[pos..after]);
                pos = after;
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_matches_javascript() {
        assert_eq!(to_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(to_iso(1_600_000_000_123), "2020-09-13T12:26:40.123Z");
    }

    #[test]
    fn sync_id_validation() {
        assert_eq!(parse_sync_id("52758cb942814faa9ab255208025ae65").unwrap(), "52758cb942814faa9ab255208025ae65");
        assert_eq!(parse_sync_id("52758cb9-4281-4faa-9ab2-55208025ae65").unwrap(), "52758cb942814faa9ab255208025ae65");
        assert!(parse_sync_id("52758CB942814FAA9AB255208025AE65").is_err());
        assert!(parse_sync_id("52758cb942814faa9ab255208025ae6").is_err());
        assert!(parse_sync_id("52758cb942814faa9ab255208025ae650").is_err());
        assert!(parse_sync_id("zz758cb942814faa9ab255208025ae65").is_err());
    }

    #[test]
    fn strips_script_tags() {
        assert_eq!(strip_scripts(""), "");
        assert_eq!(strip_scripts("<b>hi</b>"), "<b>hi</b>");
        assert_eq!(strip_scripts("a<script>alert(1)</script>b"), "ab");
        assert_eq!(strip_scripts("a<SCRIPT src=x></ScRiPt>b<script>c</script>"), "ab");
        assert_eq!(strip_scripts("<scripts>x</script>"), "<scripts>x</script>");
        assert_eq!(strip_scripts("<script>unterminated"), "<script>unterminated");
    }

    #[test]
    fn truthiness_and_casting() {
        assert!(!is_truthy(&json!(null)));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!(0)));
        assert!(is_truthy(&json!("x")));
        assert!(is_truthy(&json!({})));
        assert_eq!(cast_string(&json!("1.1.13")).unwrap().as_deref(), Some("1.1.13"));
        assert_eq!(cast_string(&json!(2)).unwrap().as_deref(), Some("2"));
        assert_eq!(cast_string(&json!(false)).unwrap(), None);
        assert!(cast_string(&json!({ "a": 1 })).is_err());
    }
}
