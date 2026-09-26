//! MongoDB backend, document-compatible with the reference API:
//!
//! * `bookmarks`: `{ _id: BinData(4, <uuid>), bookmarks, version, lastAccessed, lastUpdated }`
//! * `newsynclogs`: `{ _id: "<uuid>", ipAddress, syncCreated, expiresAt }`

use std::time::Duration;

use anyhow::{Context, Result};
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, DateTime, Document, doc};
use mongodb::options::{ClientOptions, IndexOptions, ReturnDocument};
use mongodb::{Client, Collection, IndexModel};

use super::{SyncRecord, SyncUpdate};
use crate::config::DbConfig;

pub struct MongoStore {
    bookmarks: Collection<Document>,
    logs: Collection<Document>,
}

impl MongoStore {
    pub async fn connect(config: &DbConfig) -> Result<Self> {
        let mut options =
            ClientOptions::parse(connection_uri(config)).await.context("invalid MongoDB connection settings")?;
        let timeout = Duration::from_millis(config.conn_timeout);
        options.connect_timeout = Some(timeout);
        options.server_selection_timeout = Some(timeout);
        options.app_name = Some("marksync-server".into());
        let client = Client::with_options(options)?;
        let db = client.database(&config.name);
        db.run_command(doc! { "ping": 1 }).await.context("unable to connect to MongoDB")?;

        let store = Self { bookmarks: db.collection("bookmarks"), logs: db.collection("newsynclogs") };
        store.ensure_indexes().await;
        Ok(store)
    }

    /// Creates the indexes the reference Docker setup defines. Failures (e.g. an existing
    /// index with different options) are logged and ignored: they only affect performance
    /// or expiry, which the purge task also handles.
    async fn ensure_indexes(&self) {
        let indexes = [
            (
                &self.logs,
                IndexModel::builder()
                    .keys(doc! { "expiresAt": 1 })
                    .options(IndexOptions::builder().expire_after(Duration::ZERO).build())
                    .build(),
            ),
            (&self.logs, IndexModel::builder().keys(doc! { "ipAddress": 1 }).build()),
            (&self.bookmarks, IndexModel::builder().keys(doc! { "lastAccessed": 1 }).build()),
        ];
        for (collection, index) in indexes {
            if let Err(err) = collection.create_index(index).await {
                tracing::debug!(error = %err, collection = collection.name(), "skipping index creation");
            }
        }
    }

    pub async fn create(&self, record: &SyncRecord) -> Result<()> {
        let mut document = doc! {
            "_id": id_to_binary(&record.id)?,
            "lastAccessed": DateTime::from_millis(record.last_accessed),
            "lastUpdated": DateTime::from_millis(record.last_updated),
        };
        if let Some(bookmarks) = &record.bookmarks {
            document.insert("bookmarks", bookmarks);
        }
        if let Some(version) = &record.version {
            document.insert("version", version);
        }
        self.bookmarks.insert_one(document).await?;
        Ok(())
    }

    pub async fn find(&self, id: &str) -> Result<Option<SyncRecord>> {
        let document = self.bookmarks.find_one(doc! { "_id": id_to_binary(id)? }).await?;
        document.map(|d| to_record(&d)).transpose()
    }

    pub async fn touch(&self, id: &str, now: i64) -> Result<Option<SyncRecord>> {
        let document = self
            .bookmarks
            .find_one_and_update(
                doc! { "_id": id_to_binary(id)? },
                doc! { "$set": { "lastAccessed": DateTime::from_millis(now) } },
            )
            .return_document(ReturnDocument::After)
            .await?;
        document.map(|d| to_record(&d)).transpose()
    }

    pub async fn update(&self, id: &str, update: &SyncUpdate<'_>) -> Result<bool> {
        let mut filter = doc! { "_id": id_to_binary(id)? };
        if let Some(expected) = update.expected_last_updated {
            filter.insert("lastUpdated", DateTime::from_millis(expected));
        }
        let now = DateTime::from_millis(update.now);
        let mut set = doc! { "bookmarks": update.bookmarks, "lastAccessed": now, "lastUpdated": now };
        if let Some(version) = update.version {
            set.insert("version", version);
        }
        let result = self.bookmarks.update_one(filter, doc! { "$set": set }).await?;
        Ok(result.matched_count > 0)
    }

    pub async fn count_syncs(&self) -> Result<u64> {
        Ok(self.bookmarks.estimated_document_count().await?)
    }

    pub async fn count_new_sync_logs(&self, ip: &str, now: i64) -> Result<u64> {
        let filter = doc! { "ipAddress": ip, "expiresAt": { "$gt": DateTime::from_millis(now) } };
        Ok(self.logs.count_documents(filter).await?)
    }

    pub async fn add_new_sync_log(&self, ip: &str, created: i64, expires_at: i64) -> Result<()> {
        self.logs
            .insert_one(doc! {
                "_id": uuid::Uuid::new_v4().to_string(),
                "expiresAt": DateTime::from_millis(expires_at),
                "ipAddress": ip,
                "syncCreated": DateTime::from_millis(created),
            })
            .await?;
        Ok(())
    }

    pub async fn purge(&self, now: i64, stale_before: Option<i64>) -> Result<(u64, u64)> {
        let logs =
            self.logs.delete_many(doc! { "expiresAt": { "$lte": DateTime::from_millis(now) } }).await?.deleted_count;
        let syncs = match stale_before {
            Some(cutoff) => {
                self.bookmarks
                    .delete_many(doc! { "lastAccessed": { "$lt": DateTime::from_millis(cutoff) } })
                    .await?
                    .deleted_count
            }
            None => 0,
        };
        Ok((syncs, logs))
    }
}

/// Builds the connection string the same way as the reference API.
fn connection_uri(config: &DbConfig) -> String {
    if !config.uri.is_empty() {
        return config.uri.clone();
    }
    let creds = if !config.username.is_empty() && !config.password.is_empty() {
        format!("{}:{}@", encode(&config.username), encode(&config.password))
    } else {
        String::new()
    };
    let mut uri = if config.use_srv {
        format!("mongodb+srv://{creds}{}/{}", config.host, config.name)
    } else {
        format!("mongodb://{creds}{}:{}/{}", config.host, config.port, config.name)
    };
    let mut params = Vec::new();
    if !config.auth_source.is_empty() {
        params.push(format!("authSource={}", encode(&config.auth_source)));
    }
    if config.ssl || config.use_srv {
        params.push("tls=true".to_owned());
    }
    if !params.is_empty() {
        uri.push('?');
        uri.push_str(&params.join("&"));
    }
    uri
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn id_to_binary(id: &str) -> Result<Binary> {
    let uuid = uuid::Uuid::try_parse(id).with_context(|| format!("invalid sync id {id:?}"))?;
    Ok(Binary { subtype: BinarySubtype::Uuid, bytes: uuid.as_bytes().to_vec() })
}

fn to_record(document: &Document) -> Result<SyncRecord> {
    let id = match document.get("_id") {
        Some(Bson::Binary(binary)) if binary.bytes.len() == 16 => {
            binary.bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        other => anyhow::bail!("unexpected sync _id {other:?}"),
    };
    let string = |key: &str| document.get_str(key).ok().map(str::to_owned);
    let millis = |key: &str| document.get_datetime(key).map(|d| d.timestamp_millis()).unwrap_or_default();
    Ok(SyncRecord {
        id,
        bookmarks: string("bookmarks"),
        version: string("version"),
        last_updated: millis("lastUpdated"),
        last_accessed: millis("lastAccessed"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn builds_uri_like_reference_api() {
        let config = Config::with_overrides(json!({ "db": { "type": "mongodb" } })).unwrap();
        assert_eq!(connection_uri(&config.db), "mongodb://127.0.0.1:27017/xbrowsersync?authSource=admin");

        let config = Config::with_overrides(json!({
            "db": { "username": "u@x", "password": "p:w", "host": "db", "useSRV": true, "authSource": "" }
        }))
        .unwrap();
        assert_eq!(connection_uri(&config.db), "mongodb+srv://u%40x:p%3Aw@db/xbrowsersync?tls=true");

        let config = Config::with_overrides(json!({ "db": { "uri": "mongodb://custom/db" } })).unwrap();
        assert_eq!(connection_uri(&config.db), "mongodb://custom/db");
    }

    #[test]
    fn round_trips_uuid_ids() {
        let id = "52758cb942814faa9ab255208025ae65";
        let binary = id_to_binary(id).unwrap();
        assert_eq!(binary.subtype, BinarySubtype::Uuid);
        let record = to_record(&doc! { "_id": binary }).unwrap();
        assert_eq!(record.id, id);
    }
}
