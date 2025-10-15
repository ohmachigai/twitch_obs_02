use std::borrow::Cow;

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{
    migrate::MigrateError, sqlite::SqlitePoolOptions, Row, Sqlite, SqlitePool, Transaction,
};
use thiserror::Error;
use uuid::Uuid;

use twi_overlay_core::types::{QueueEntry, QueueEntryStatus, Settings};

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

    /// Returns a handle for interacting with the command log.
    pub fn command_log(&self) -> CommandLogRepository {
        CommandLogRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle for manipulating the state index table.
    pub fn state_index(&self) -> StateIndexRepository {
        StateIndexRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle to operate on queue entries.
    pub fn queue(&self) -> QueueRepository {
        QueueRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle for manipulating daily counters.
    pub fn daily_counters(&self) -> DailyCounterRepository {
        DailyCounterRepository {
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
    pub async fn fetch_settings(
        &self,
        broadcaster_id: &str,
    ) -> Result<BroadcasterSettings, SettingsError> {
        let row = sqlx::query("SELECT settings_json, timezone FROM broadcasters WHERE id = ?")
            .bind(broadcaster_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(SettingsError::NotFound)?;

        let json_value: String = row.get("settings_json");
        let settings: Settings = serde_json::from_str(&json_value)?;
        let timezone: String = row.get("timezone");
        Ok(BroadcasterSettings { settings, timezone })
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

/// Settings bundle loaded from the broadcasters table.
#[derive(Debug, Clone)]
pub struct BroadcasterSettings {
    pub settings: Settings,
    pub timezone: String,
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

/// Repository managing the command log and versioning.
#[derive(Clone)]
pub struct CommandLogRepository {
    pool: SqlitePool,
}

impl CommandLogRepository {
    /// Begins a SQLite transaction.
    pub async fn begin(&self) -> Result<Transaction<'_, Sqlite>, sqlx::Error> {
        self.pool.begin().await
    }

    /// Appends a new record to the command log while incrementing the state version.
    pub async fn append(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        record: NewCommandLog<'_>,
    ) -> Result<u64, CommandLogError> {
        let updated_at = to_rfc3339(record.created_at);
        let version_row = sqlx::query(
            "UPDATE state_index \
             SET current_version = current_version + 1,\
                 updated_at = ? \
             WHERE broadcaster_id = ? \
             RETURNING current_version",
        )
        .bind(&updated_at)
        .bind(record.broadcaster_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(CommandLogError::Database)?;

        let Some(row) = version_row else {
            return Err(CommandLogError::MissingStateIndex);
        };

        let version: i64 = row.get("current_version");
        let payload_json = record.payload_json;
        sqlx::query(
            "INSERT INTO command_log \
             (broadcaster_id, version, op_id, type, payload_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(record.broadcaster_id)
        .bind(version)
        .bind(record.op_id)
        .bind(record.command_type)
        .bind(payload_json)
        .bind(&updated_at)
        .execute(&mut **tx)
        .await
        .map_err(CommandLogError::Database)?;

        Ok(version as u64)
    }
}

/// Payload required to append a command log record.
pub struct NewCommandLog<'a> {
    pub broadcaster_id: &'a str,
    pub op_id: Option<&'a str>,
    pub command_type: &'a str,
    pub payload_json: &'a str,
    pub created_at: DateTime<Utc>,
}

/// Errors that can occur while appending to the command log.
#[derive(Debug, Error)]
pub enum CommandLogError {
    #[error("state index is missing for broadcaster")]
    MissingStateIndex,
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

/// Repository to inspect and mutate the state index table.
#[derive(Clone)]
pub struct StateIndexRepository {
    pool: SqlitePool,
}

impl StateIndexRepository {
    /// Fetches the current version for the provided broadcaster.
    pub async fn fetch_current_version(
        &self,
        broadcaster_id: &str,
    ) -> Result<u64, StateIndexError> {
        let row = sqlx::query("SELECT current_version FROM state_index WHERE broadcaster_id = ?")
            .bind(broadcaster_id)
            .fetch_optional(&self.pool)
            .await?;

        let Some(row) = row else {
            return Err(StateIndexError::MissingBroadcaster);
        };

        let version: i64 = row.get("current_version");
        Ok(version as u64)
    }
}

/// Errors that can occur while reading the state index.
#[derive(Debug, Error)]
pub enum StateIndexError {
    #[error("broadcaster is not present in state_index")]
    MissingBroadcaster,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Repository for queue entries.
#[derive(Clone)]
pub struct QueueRepository {
    pool: SqlitePool,
}

impl QueueRepository {
    /// Inserts a new queue entry.
    pub async fn insert_entry(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        entry: &NewQueueEntry<'_>,
    ) -> Result<(), QueueError> {
        let managed = if entry.managed { 1 } else { 0 };
        sqlx::query(
            "INSERT INTO queue_entries \
             (id, broadcaster_id, user_id, user_login, user_display_name, user_avatar, reward_id, redemption_id, enqueued_at, status, status_reason, managed, last_updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&entry.id)
        .bind(entry.broadcaster_id)
        .bind(entry.user_id)
        .bind(&entry.user_login)
        .bind(&entry.user_display_name)
        .bind(&entry.user_avatar)
        .bind(entry.reward_id)
        .bind(&entry.redemption_id)
        .bind(to_rfc3339(entry.enqueued_at))
        .bind(entry.status.as_str())
        .bind(&entry.status_reason)
        .bind(managed)
        .bind(to_rfc3339(entry.last_updated_at))
        .execute(&mut **tx)
        .await
        .map_err(|err| match err {
            sqlx::Error::Database(db_err) => {
                if db_err.code().as_deref() == Some("2067") {
                    QueueError::DuplicateRedemption
                } else {
                    QueueError::Database(sqlx::Error::Database(db_err))
                }
            }
            other => QueueError::Database(other),
        })?;

        Ok(())
    }

    /// Lists the active queue entries ordered by daily count and enqueue timestamp.
    pub async fn list_active_with_counts(
        &self,
        broadcaster_id: &str,
        day: &str,
    ) -> Result<Vec<QueueEntryWithCount>, QueueError> {
        let rows = sqlx::query_as::<_, QueueEntryWithCount>(
            r#"
SELECT q.id,
       q.broadcaster_id,
       q.user_id,
       q.user_login,
       q.user_display_name,
       q.user_avatar,
       q.reward_id,
       q.redemption_id,
       q.enqueued_at as "enqueued_at: DateTime<Utc>",
       q.status,
       q.status_reason,
       q.managed,
       q.last_updated_at as "last_updated_at: DateTime<Utc>",
       COALESCE(dc.count, 0) as "today_count"
  FROM queue_entries AS q
  LEFT JOIN daily_counters AS dc
    ON dc.day = ?
   AND dc.broadcaster_id = q.broadcaster_id
   AND dc.user_id = q.user_id
 WHERE q.broadcaster_id = ?
   AND q.status = 'QUEUED'
 ORDER BY today_count ASC, q.enqueued_at ASC
            "#,
        )
        .bind(day)
        .bind(broadcaster_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// Parameters required to insert a queue entry.
pub struct NewQueueEntry<'a> {
    pub id: String,
    pub broadcaster_id: &'a str,
    pub user_id: &'a str,
    pub user_login: String,
    pub user_display_name: String,
    pub user_avatar: Option<String>,
    pub reward_id: &'a str,
    pub redemption_id: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub status: QueueEntryStatus,
    pub status_reason: Option<String>,
    pub managed: bool,
    pub last_updated_at: DateTime<Utc>,
}

/// Representation of a queue entry joined with the user's daily count.
#[derive(Debug, sqlx::FromRow)]
pub struct QueueEntryWithCount {
    pub id: String,
    pub broadcaster_id: String,
    pub user_id: String,
    pub user_login: String,
    pub user_display_name: String,
    pub user_avatar: Option<String>,
    pub reward_id: String,
    pub redemption_id: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub status: String,
    pub status_reason: Option<String>,
    pub managed: i64,
    pub last_updated_at: DateTime<Utc>,
    pub today_count: i64,
}

impl QueueEntryWithCount {
    /// Converts the database row into a domain queue entry and associated count.
    pub fn into_domain(self) -> (QueueEntry, u32) {
        let status = match self.status.as_str() {
            "QUEUED" => QueueEntryStatus::Queued,
            "COMPLETED" => QueueEntryStatus::Completed,
            "REMOVED" => QueueEntryStatus::Removed,
            _ => QueueEntryStatus::Queued,
        };
        (
            QueueEntry {
                id: self.id,
                broadcaster_id: self.broadcaster_id,
                user_id: self.user_id,
                user_login: self.user_login,
                user_display_name: self.user_display_name,
                user_avatar: self.user_avatar,
                reward_id: self.reward_id,
                redemption_id: self.redemption_id,
                enqueued_at: self.enqueued_at,
                status,
                status_reason: self.status_reason,
                managed: self.managed != 0,
                last_updated_at: self.last_updated_at,
            },
            self.today_count as u32,
        )
    }
}

/// Errors that can occur while mutating queue entries.
#[derive(Debug, Error)]
pub enum QueueError {
    #[error("queue entry with the same redemption id already exists")]
    DuplicateRedemption,
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

impl From<sqlx::Error> for QueueError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

/// Repository handling daily counters.
#[derive(Clone)]
pub struct DailyCounterRepository {
    pool: SqlitePool,
}

impl DailyCounterRepository {
    /// Increments the counter for the given day and user, returning the new value.
    pub async fn increment(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        record: &NewDailyCounter<'_>,
    ) -> Result<u32, DailyCounterError> {
        let row = sqlx::query(
            "INSERT INTO daily_counters(day, broadcaster_id, user_id, count, updated_at) \
             VALUES (?, ?, ?, 1, ?) \
             ON CONFLICT(day, broadcaster_id, user_id) DO UPDATE \
             SET count = count + 1, updated_at = excluded.updated_at \
             RETURNING count",
        )
        .bind(&record.day)
        .bind(record.broadcaster_id)
        .bind(record.user_id)
        .bind(to_rfc3339(record.updated_at))
        .fetch_one(&mut **tx)
        .await?;

        let count: i64 = row.get("count");
        Ok(count as u32)
    }

    /// Lists counters for a given day.
    pub async fn list_for_day(
        &self,
        broadcaster_id: &str,
        day: &str,
    ) -> Result<Vec<DailyCounterValue>, DailyCounterError> {
        let rows = sqlx::query_as::<_, DailyCounterValue>(
            "SELECT user_id, count FROM daily_counters WHERE day = ? AND broadcaster_id = ? ORDER BY user_id",
        )
        .bind(day)
        .bind(broadcaster_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// New counter increment payload.
pub struct NewDailyCounter<'a> {
    pub day: String,
    pub broadcaster_id: &'a str,
    pub user_id: &'a str,
    pub updated_at: DateTime<Utc>,
}

/// Counter value row.
#[derive(Debug, sqlx::FromRow)]
pub struct DailyCounterValue {
    pub user_id: String,
    pub count: i64,
}

/// Errors that can occur when mutating counters.
#[derive(Debug, Error)]
pub enum DailyCounterError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
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
        assert!(settings.settings.policy().target_rewards.is_empty());
        assert_eq!(settings.settings.policy().anti_spam_window_sec, 60);
        assert_eq!(settings.timezone, "UTC");
    }

    #[tokio::test]
    async fn fetch_settings_errors_for_missing_broadcaster() {
        let db = setup_db().await;
        let repo = db.broadcasters();
        let err = repo.fetch_settings("missing").await.unwrap_err();
        assert!(matches!(err, SettingsError::NotFound));
    }
}
