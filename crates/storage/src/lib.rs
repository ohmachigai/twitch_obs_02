use std::borrow::Cow;

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{migrate::MigrateError, sqlite::SqlitePoolOptions, Row, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

use twi_overlay_core::types::Settings;

/// Top-level database handle that owns the SQLite connection pool.
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Establishes a new SQLite connection pool for the provided connection string.
    pub async fn connect(database_url: &str) -> Result<Self, StorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(StorageError::Connect)?;

        apply_pragmas(&pool).await?;

        Ok(Self { pool })
    }

    /// Applies migrations located under `migrations/`.
    pub async fn run_migrations(&self) -> Result<(), StorageError> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(StorageError::Migration)?;
        Ok(())
    }

    /// Returns a handle to interact with the EventRaw repository.
    pub fn event_raw(&self) -> EventRawRepository {
        EventRawRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle for interacting with broadcasters metadata.
    pub fn broadcasters(&self) -> BroadcasterRepository {
        BroadcasterRepository {
            pool: self.pool.clone(),
        }
    }

    /// Exposes the inner pool when lower level access is required.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

async fn apply_pragmas(pool: &SqlitePool) -> Result<(), StorageError> {
    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(pool)
        .await
        .map_err(StorageError::Pragma)?;

    sqlx::query("PRAGMA journal_mode = WAL;")
        .fetch_one(pool)
        .await
        .map_err(StorageError::Pragma)?;

    sqlx::query("PRAGMA synchronous = NORMAL;")
        .execute(pool)
        .await
        .map_err(StorageError::Pragma)?;

    sqlx::query("PRAGMA busy_timeout = 5000;")
        .execute(pool)
        .await
        .map_err(StorageError::Pragma)?;

    Ok(())
}

/// General storage level errors.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("failed to connect to sqlite: {0}")]
    Connect(sqlx::Error),
    #[error("failed to apply pragma: {0}")]
    Pragma(sqlx::Error),
    #[error("failed to run database migrations: {0}")]
    Migration(MigrateError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Repository used to query broadcaster metadata and settings.
#[derive(Clone)]
pub struct BroadcasterRepository {
    pool: SqlitePool,
}

impl BroadcasterRepository {
    /// Loads the settings JSON for the provided broadcaster.
    pub async fn fetch_settings(&self, broadcaster_id: &str) -> Result<Settings, SettingsError> {
        let row = sqlx::query("SELECT settings_json FROM broadcasters WHERE id = ?")
            .bind(broadcaster_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(SettingsError::NotFound)?;

        let json_value: String = row.get("settings_json");
        let settings: Settings = serde_json::from_str(&json_value)?;
        Ok(settings)
    }
}

/// Errors that can occur while reading settings.
#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("broadcaster not found")]
    NotFound,
    #[error("failed to decode settings json: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Repository responsible for interacting with the `event_raw` table.
#[derive(Clone)]
pub struct EventRawRepository {
    pool: SqlitePool,
}

impl EventRawRepository {
    /// Inserts a new EventSub payload into the `event_raw` table.
    pub async fn insert(
        &self,
        record: NewEventRaw<'_>,
    ) -> Result<EventRawInsertOutcome, EventRawError> {
        let result = sqlx::query(
            "INSERT INTO event_raw \
             (id, broadcaster_id, msg_id, type, payload_json, event_at, received_at, source) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.broadcaster_id)
        .bind(&record.msg_id)
        .bind(&record.event_type)
        .bind(&record.payload_json)
        .bind(to_rfc3339(record.event_at))
        .bind(to_rfc3339(record.received_at))
        .bind(record.source)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(EventRawInsertOutcome::Inserted),
            Err(sqlx::Error::Database(db_err)) => {
                if let Some(code) = db_err.code() {
                    if code == Cow::Borrowed("2067") {
                        return Ok(EventRawInsertOutcome::Duplicate);
                    }
                    if code == Cow::Borrowed("787") {
                        return Err(EventRawError::MissingBroadcaster);
                    }
                }

                Err(EventRawError::Database(sqlx::Error::Database(db_err)))
            }
            Err(err) => Err(EventRawError::Database(err)),
        }
    }
}

/// Result of attempting to insert into `event_raw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventRawInsertOutcome {
    Inserted,
    Duplicate,
}

impl EventRawInsertOutcome {
    pub fn is_duplicate(self) -> bool {
        matches!(self, Self::Duplicate)
    }
}

/// Error type for operations on the `event_raw` repository.
#[derive(Debug, Error)]
pub enum EventRawError {
    #[error("broadcaster is missing for incoming payload")]
    MissingBroadcaster,
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

/// Data required to create a new entry in `event_raw`.
#[derive(Clone)]
pub struct NewEventRaw<'a> {
    pub id: Cow<'a, str>,
    pub broadcaster_id: Cow<'a, str>,
    pub msg_id: Cow<'a, str>,
    pub event_type: Cow<'a, str>,
    pub payload_json: Cow<'a, str>,
    pub event_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub source: &'a str,
}

impl<'a> NewEventRaw<'a> {
    pub fn with_generated_id(self) -> Self {
        Self {
            id: Cow::Owned(Uuid::new_v4().to_string()),
            ..self
        }
    }
}

fn to_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup_db() -> Database {
        let db = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        sqlx::query(
            "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at) \
             VALUES ('b-1', 'twitch-1', 'Example', 'UTC', '{}', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("insert broadcaster");
        db
    }

    #[tokio::test]
    async fn insert_returns_duplicate_on_conflict() {
        let db = setup_db().await;
        let repo = db.event_raw();
        let record = NewEventRaw {
            id: Cow::Borrowed("id-1"),
            broadcaster_id: Cow::Borrowed("b-1"),
            msg_id: Cow::Borrowed("msg-1"),
            event_type: Cow::Borrowed("test.event"),
            payload_json: Cow::Borrowed("{}"),
            event_at: Utc::now(),
            received_at: Utc::now(),
            source: "webhook",
        };

        let outcome = repo.insert(record.clone()).await.expect("insert succeeds");
        assert_eq!(outcome, EventRawInsertOutcome::Inserted);

        let outcome = repo
            .insert(record.with_generated_id())
            .await
            .expect("duplicate should be ok");
        assert!(outcome.is_duplicate());
    }

    #[tokio::test]
    async fn insert_errors_when_broadcaster_missing() {
        let db = setup_db().await;
        let repo = db.event_raw();
        let record = NewEventRaw {
            id: Cow::Borrowed("id-2"),
            broadcaster_id: Cow::Borrowed("missing"),
            msg_id: Cow::Borrowed("msg-2"),
            event_type: Cow::Borrowed("test.event"),
            payload_json: Cow::Borrowed("{}"),
            event_at: Utc::now(),
            received_at: Utc::now(),
            source: "webhook",
        };

        let outcome = repo.insert(record).await;
        assert!(matches!(outcome, Err(EventRawError::MissingBroadcaster)));
    }

    #[tokio::test]
    async fn migrations_apply() {
        let db = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");

        let tables: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
                .fetch_one(db.pool())
                .await
                .expect("fetch tables");
        assert!(tables.0 >= 6, "expected core tables to be created");
    }

    #[tokio::test]
    async fn fetch_settings_returns_defaults() {
        let db = setup_db().await;
        let repo = db.broadcasters();
        let settings = repo.fetch_settings("b-1").await.expect("settings load");
        assert!(settings.policy().target_rewards.is_empty());
        assert_eq!(settings.policy().anti_spam_window_sec, 60);
    }

    #[tokio::test]
    async fn fetch_settings_errors_for_missing_broadcaster() {
        let db = setup_db().await;
        let repo = db.broadcasters();
        let err = repo.fetch_settings("missing").await.unwrap_err();
        assert!(matches!(err, SettingsError::NotFound));
    }
}
