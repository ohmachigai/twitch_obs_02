use std::borrow::Cow;

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{
    migrate::MigrateError, sqlite::SqlitePoolOptions, Row, Sqlite, SqlitePool, Transaction,
};
use thiserror::Error;
use uuid::Uuid;

use twi_overlay_core::types::{QueueEntry, QueueEntryStatus, QueueRemovalReason, Settings};

use serde_json::{self, to_string};

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

    /// Returns a handle for manipulating OAuth login states.
    pub fn oauth_login_states(&self) -> OauthLoginStateRepository {
        OauthLoginStateRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle for interacting with persisted OAuth links.
    pub fn oauth_links(&self) -> OauthLinkRepository {
        OauthLinkRepository {
            pool: self.pool.clone(),
        }
    }

    /// Returns a handle for Helix backfill checkpoints.
    pub fn helix_backfill(&self) -> HelixBackfillRepository {
        HelixBackfillRepository {
            pool: self.pool.clone(),
        }
    }

    /// Exposes the inner pool when lower level access is required.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Executes `PRAGMA wal_checkpoint(TRUNCATE)` and returns the reported counters.
    pub async fn wal_checkpoint_truncate(&self) -> Result<WalCheckpointStats, sqlx::Error> {
        let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
            .fetch_one(&self.pool)
            .await?;

        Ok(WalCheckpointStats {
            busy_frames: row.get::<i64, _>("busy"),
            log_frames: row.get::<i64, _>("log"),
            checkpointed_frames: row.get::<i64, _>("checkpointed"),
        })
    }
}

/// Counters returned by `PRAGMA wal_checkpoint`.
#[derive(Debug, Clone, Copy)]
pub struct WalCheckpointStats {
    pub busy_frames: i64,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
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

    /// Updates the persisted settings payload for a broadcaster.
    pub async fn update_settings(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        settings: &Settings,
        updated_at: DateTime<Utc>,
    ) -> Result<(), SettingsUpdateError> {
        let payload = to_string(settings).map_err(SettingsUpdateError::Encode)?;
        let updated_rows =
            sqlx::query("UPDATE broadcasters SET settings_json = ?, updated_at = ? WHERE id = ?")
                .bind(&payload)
                .bind(to_rfc3339(updated_at))
                .bind(broadcaster_id)
                .execute(&mut **tx)
                .await?;

        if updated_rows.rows_affected() == 0 {
            return Err(SettingsUpdateError::NotFound);
        }

        Ok(())
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

/// Errors that can occur while updating settings.
#[derive(Debug, Error)]
pub enum SettingsUpdateError {
    #[error("broadcaster not found")]
    NotFound,
    #[error("failed to encode settings json: {0}")]
    Encode(#[from] serde_json::Error),
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

    /// Deletes at most `limit` rows older than the given threshold.
    pub async fn delete_older_than_batch(
        &self,
        threshold: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM event_raw \
             WHERE rowid IN (\
                 SELECT rowid FROM event_raw \
                 WHERE received_at < ? \
                 ORDER BY received_at \
                 LIMIT ?\
             )",
        )
        .bind(to_rfc3339(threshold))
        .bind(limit)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
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

    /// Deletes at most `limit` rows older than the given threshold.
    pub async fn delete_older_than_batch(
        &self,
        threshold: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM command_log \
             WHERE rowid IN (\
                 SELECT rowid FROM command_log \
                 WHERE created_at < ? \
                 ORDER BY created_at \
                 LIMIT ?\
             )",
        )
        .bind(to_rfc3339(threshold))
        .bind(limit)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Finds an existing command log entry by `op_id`.
    pub async fn find_by_op_id(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        op_id: &str,
    ) -> Result<Option<LoggedCommand>, CommandLogError> {
        let row = sqlx::query(
            "SELECT version, type, payload_json FROM command_log WHERE broadcaster_id = ? AND op_id = ?",
        )
        .bind(broadcaster_id)
        .bind(op_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(CommandLogError::Database)?;

        Ok(row.map(|row| {
            let version: i64 = row.get("version");
            LoggedCommand {
                version: version as u64,
                command_type: row.get("type"),
                payload_json: row.get("payload_json"),
            }
        }))
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

/// Persisted command log entry returned when querying by `op_id`.
#[derive(Debug, Clone)]
pub struct LoggedCommand {
    pub version: u64,
    pub command_type: String,
    pub payload_json: String,
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

    pub async fn list_active_with_counts_since(
        &self,
        broadcaster_id: &str,
        day: &str,
        since: DateTime<Utc>,
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
   AND q.last_updated_at >= ?
 ORDER BY today_count ASC, q.enqueued_at ASC
            "#,
        )
        .bind(day)
        .bind(broadcaster_id)
        .bind(to_rfc3339(since))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Finds a queue entry within an ongoing transaction.
    pub async fn find_entry_for_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        entry_id: &str,
    ) -> Result<Option<QueueEntry>, QueueError> {
        let row = sqlx::query_as::<_, QueueEntryRow>(
            r#"
SELECT id,
       broadcaster_id,
       user_id,
       user_login,
       user_display_name,
       user_avatar,
       reward_id,
       redemption_id,
       enqueued_at as "enqueued_at: DateTime<Utc>",
       status,
       status_reason,
       managed,
       last_updated_at as "last_updated_at: DateTime<Utc>"
  FROM queue_entries
 WHERE broadcaster_id = ?
   AND id = ?
            "#,
        )
        .bind(broadcaster_id)
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(QueueEntryRow::into_domain))
    }

    /// Finds a queue entry by redemption identifier within the given transaction.
    pub async fn find_entry_by_redemption_for_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        redemption_id: &str,
    ) -> Result<Option<QueueEntry>, QueueError> {
        let row = sqlx::query_as::<_, QueueEntryRow>(
            r#"
SELECT id,
       broadcaster_id,
       user_id,
       user_login,
       user_display_name,
       user_avatar,
       reward_id,
       redemption_id,
       enqueued_at as "enqueued_at: DateTime<Utc>",
       status,
       status_reason,
       managed,
       last_updated_at as "last_updated_at: DateTime<Utc>"
  FROM queue_entries
 WHERE broadcaster_id = ?
   AND redemption_id = ?
            "#,
        )
        .bind(broadcaster_id)
        .bind(redemption_id)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(QueueEntryRow::into_domain))
    }

    /// Marks an entry as completed, returning the updated representation.
    pub async fn mark_completed(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        entry_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<QueueEntry, QueueError> {
        let existing = self
            .find_entry_for_update(tx, broadcaster_id, entry_id)
            .await?;

        let Some(entry) = existing else {
            return Err(QueueError::NotFound);
        };

        if entry.status != QueueEntryStatus::Queued {
            return Err(QueueError::InvalidTransition(entry.status));
        }

        let row = sqlx::query_as::<_, QueueEntryRow>(
            r#"
UPDATE queue_entries
   SET status = 'COMPLETED',
       status_reason = NULL,
       last_updated_at = ?
 WHERE broadcaster_id = ?
   AND id = ?
 RETURNING id,
           broadcaster_id,
           user_id,
           user_login,
           user_display_name,
           user_avatar,
           reward_id,
           redemption_id,
           enqueued_at as "enqueued_at: DateTime<Utc>",
           status,
           status_reason,
           managed,
           last_updated_at as "last_updated_at: DateTime<Utc>"
            "#,
        )
        .bind(to_rfc3339(updated_at))
        .bind(broadcaster_id)
        .bind(entry_id)
        .fetch_one(&mut **tx)
        .await?;

        Ok(row.into_domain())
    }

    /// Marks an entry as removed with the provided reason.
    pub async fn mark_removed(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        entry_id: &str,
        reason: QueueRemovalReason,
        updated_at: DateTime<Utc>,
    ) -> Result<QueueEntry, QueueError> {
        let existing = self
            .find_entry_for_update(tx, broadcaster_id, entry_id)
            .await?;

        let Some(entry) = existing else {
            return Err(QueueError::NotFound);
        };

        if entry.status != QueueEntryStatus::Queued {
            return Err(QueueError::InvalidTransition(entry.status));
        }

        let row = sqlx::query_as::<_, QueueEntryRow>(
            r#"
UPDATE queue_entries
   SET status = 'REMOVED',
       status_reason = ?,
       last_updated_at = ?
 WHERE broadcaster_id = ?
   AND id = ?
 RETURNING id,
           broadcaster_id,
           user_id,
           user_login,
           user_display_name,
           user_avatar,
           reward_id,
           redemption_id,
           enqueued_at as "enqueued_at: DateTime<Utc>",
           status,
           status_reason,
           managed,
           last_updated_at as "last_updated_at: DateTime<Utc>"
            "#,
        )
        .bind(reason.as_str())
        .bind(to_rfc3339(updated_at))
        .bind(broadcaster_id)
        .bind(entry_id)
        .fetch_one(&mut **tx)
        .await?;

        Ok(row.into_domain())
    }

    /// Updates the managed flag for a queue entry, returning the refreshed representation.
    pub async fn update_managed(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        entry_id: &str,
        managed: bool,
        updated_at: DateTime<Utc>,
    ) -> Result<QueueEntry, QueueError> {
        let row = sqlx::query_as::<_, QueueEntryRow>(
            r#"
UPDATE queue_entries
   SET managed = ?,
       last_updated_at = ?
 WHERE broadcaster_id = ?
   AND id = ?
 RETURNING id,
           broadcaster_id,
           user_id,
           user_login,
           user_display_name,
           user_avatar,
           reward_id,
           redemption_id,
           enqueued_at as "enqueued_at: DateTime<Utc>",
           status,
           status_reason,
           managed,
           last_updated_at as "last_updated_at: DateTime<Utc>"
            "#,
        )
        .bind(if managed { 1 } else { 0 })
        .bind(to_rfc3339(updated_at))
        .bind(broadcaster_id)
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await?;

        let Some(row) = row else {
            return Err(QueueError::NotFound);
        };

        Ok(row.into_domain())
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
    #[sqlx(rename = "enqueued_at: DateTime<Utc>")]
    pub enqueued_at: DateTime<Utc>,
    pub status: String,
    pub status_reason: Option<String>,
    pub managed: i64,
    #[sqlx(rename = "last_updated_at: DateTime<Utc>")]
    pub last_updated_at: DateTime<Utc>,
    pub today_count: i64,
}

impl QueueEntryWithCount {
    /// Converts the database row into a domain queue entry and associated count.
    pub fn into_domain(self) -> (QueueEntry, u32) {
        let status = map_status(&self.status);
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
    #[error("queue entry not found")]
    NotFound,
    #[error("queue entry is not queued (current={0:?})")]
    InvalidTransition(QueueEntryStatus),
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

    pub async fn list_updated_since(
        &self,
        broadcaster_id: &str,
        day: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<DailyCounterValue>, DailyCounterError> {
        let rows = sqlx::query_as::<_, DailyCounterValue>(
            "SELECT user_id, count FROM daily_counters WHERE day = ? AND broadcaster_id = ? AND updated_at >= ? ORDER BY user_id",
        )
        .bind(day)
        .bind(broadcaster_id)
        .bind(to_rfc3339(since))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Decrements the counter for the given day, returning the new value when present.
    pub async fn decrement(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        day: &str,
        broadcaster_id: &str,
        user_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<Option<u32>, DailyCounterError> {
        let row = sqlx::query(
            "UPDATE daily_counters SET count = CASE WHEN count > 0 THEN count - 1 ELSE 0 END, updated_at = ? WHERE day = ? AND broadcaster_id = ? AND user_id = ? RETURNING count",
        )
        .bind(to_rfc3339(updated_at))
        .bind(day)
        .bind(broadcaster_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(|row| {
            let count: i64 = row.get("count");
            count as u32
        }))
    }

    /// Fetches the counter value for the provided day and user.
    pub async fn fetch_value(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        day: &str,
        broadcaster_id: &str,
        user_id: &str,
    ) -> Result<Option<u32>, DailyCounterError> {
        let row = sqlx::query(
            "SELECT count FROM daily_counters WHERE day = ? AND broadcaster_id = ? AND user_id = ?",
        )
        .bind(day)
        .bind(broadcaster_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(|row| {
            let count: i64 = row.get("count");
            count as u32
        }))
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

/// Repository managing ephemeral OAuth login state records.
#[derive(Clone)]
pub struct OauthLoginStateRepository {
    pool: SqlitePool,
}

impl OauthLoginStateRepository {
    /// Inserts a freshly generated OAuth login state.
    pub async fn insert(
        &self,
        record: &NewOauthLoginState<'_>,
    ) -> Result<(), OauthLoginStateError> {
        sqlx::query(
            r#"
INSERT INTO oauth_login_states(state, broadcaster_id, code_verifier, redirect_to, created_at, expires_at)
VALUES(?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&record.state)
        .bind(record.broadcaster_id)
        .bind(&record.code_verifier)
        .bind(&record.redirect_to)
        .bind(to_rfc3339(record.created_at))
        .bind(to_rfc3339(record.expires_at))
        .execute(&self.pool)
        .await
        .map_err(OauthLoginStateError::Database)?;

        Ok(())
    }

    /// Consumes an existing OAuth login state, deleting it atomically.
    pub async fn consume(
        &self,
        state: &str,
    ) -> Result<Option<OauthLoginState>, OauthLoginStateError> {
        let row = sqlx::query_as::<_, OauthLoginStateRow>(
            r#"
DELETE FROM oauth_login_states
 WHERE state = ?
 RETURNING state,
           broadcaster_id,
           code_verifier,
           redirect_to,
           created_at,
           expires_at
            "#,
        )
        .bind(state)
        .fetch_optional(&self.pool)
        .await
        .map_err(OauthLoginStateError::Database)?;

        row.map(|row| row.try_into())
            .transpose()
            .map_err(OauthLoginStateError::Decode)
    }

    /// Deletes expired login states to enforce TTL.
    pub async fn purge_expired(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, OauthLoginStateError> {
        let result = sqlx::query(
            r#"
DELETE FROM oauth_login_states
 WHERE rowid IN (
    SELECT rowid
      FROM oauth_login_states
     WHERE expires_at <= ?
     ORDER BY expires_at
     LIMIT ?
 )
            "#,
        )
        .bind(to_rfc3339(now))
        .bind(limit)
        .execute(&self.pool)
        .await
        .map_err(OauthLoginStateError::Database)?;

        Ok(result.rows_affected())
    }

    /// Returns true when a non-expired login state already exists for the broadcaster.
    pub async fn has_active(
        &self,
        broadcaster_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, OauthLoginStateError> {
        let row = sqlx::query(
            r#"
SELECT 1
  FROM oauth_login_states
 WHERE broadcaster_id = ?
   AND expires_at > ?
 LIMIT 1
            "#,
        )
        .bind(broadcaster_id)
        .bind(to_rfc3339(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(OauthLoginStateError::Database)?;

        Ok(row.is_some())
    }
}

/// Domain representation of the OAuth login state row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthLoginState {
    pub state: String,
    pub broadcaster_id: String,
    pub code_verifier: String,
    pub redirect_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// New OAuth login state payload.
pub struct NewOauthLoginState<'a> {
    pub state: String,
    pub broadcaster_id: &'a str,
    pub code_verifier: String,
    pub redirect_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum OauthLoginStateError {
    #[error("database error: {0}")]
    Database(sqlx::Error),
    #[error("failed to decode oauth login state: {0}")]
    Decode(#[from] OauthStateDecodeError),
}

#[derive(Debug, sqlx::FromRow)]
struct OauthLoginStateRow {
    state: String,
    broadcaster_id: String,
    code_verifier: String,
    redirect_to: Option<String>,
    created_at: String,
    expires_at: String,
}

impl TryFrom<OauthLoginStateRow> for OauthLoginState {
    type Error = OauthStateDecodeError;

    fn try_from(value: OauthLoginStateRow) -> Result<Self, Self::Error> {
        Ok(Self {
            state: value.state,
            broadcaster_id: value.broadcaster_id,
            code_verifier: value.code_verifier,
            redirect_to: value.redirect_to,
            created_at: parse_datetime(&value.created_at)?,
            expires_at: parse_datetime(&value.expires_at)?,
        })
    }
}

#[derive(Debug, Error)]
pub enum OauthStateDecodeError {
    #[error("invalid timestamp format: {0}")]
    Timestamp(#[from] chrono::ParseError),
}

/// Repository managing OAuth link persistence.
#[derive(Clone)]
pub struct OauthLinkRepository {
    pool: SqlitePool,
}

impl OauthLinkRepository {
    /// Persists a new or existing OAuth link, resetting failure markers.
    pub async fn upsert_link(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        record: &NewOauthLink<'_>,
    ) -> Result<OauthLink, OauthLinkError> {
        let scopes = serde_json::to_string(&record.scopes)?;
        let managed_scopes = serde_json::to_string(&record.managed_scopes)?;
        let row = sqlx::query_as::<_, OauthLinkRow>(
            r#"
INSERT INTO oauth_links(
    id,
    broadcaster_id,
    twitch_user_id,
    scopes_json,
    managed_scopes_json,
    access_token,
    refresh_token,
    expires_at,
    created_at,
    updated_at,
    last_validated_at,
    last_refreshed_at,
    last_failure_at,
    last_failure_reason,
    requires_reauth
)
VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, 0)
ON CONFLICT(broadcaster_id, twitch_user_id) DO UPDATE SET
    scopes_json = excluded.scopes_json,
    managed_scopes_json = excluded.managed_scopes_json,
    access_token = excluded.access_token,
    refresh_token = excluded.refresh_token,
    expires_at = excluded.expires_at,
    updated_at = excluded.updated_at,
    last_validated_at = NULL,
    last_refreshed_at = NULL,
    last_failure_at = NULL,
    last_failure_reason = NULL,
    requires_reauth = 0
RETURNING id,
          broadcaster_id,
          twitch_user_id,
          scopes_json,
          managed_scopes_json,
          access_token,
          refresh_token,
          expires_at,
          created_at,
          updated_at,
          last_validated_at,
          last_refreshed_at,
          last_failure_at,
          last_failure_reason,
          requires_reauth
            "#,
        )
        .bind(&record.id)
        .bind(record.broadcaster_id)
        .bind(&record.twitch_user_id)
        .bind(scopes)
        .bind(managed_scopes)
        .bind(&record.access_token)
        .bind(&record.refresh_token)
        .bind(to_rfc3339(record.expires_at))
        .bind(to_rfc3339(record.created_at))
        .bind(to_rfc3339(record.updated_at))
        .fetch_one(&mut **tx)
        .await?;

        row.try_into().map_err(OauthLinkError::Decode)
    }

    /// Retrieves the OAuth link for the provided broadcaster.
    pub async fn fetch_by_broadcaster(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<OauthLink>, OauthLinkError> {
        let row = sqlx::query_as::<_, OauthLinkRow>(
            r#"
SELECT id,
       broadcaster_id,
       twitch_user_id,
       scopes_json,
       managed_scopes_json,
       access_token,
       refresh_token,
       expires_at,
       created_at,
       updated_at,
       last_validated_at,
       last_refreshed_at,
       last_failure_at,
       last_failure_reason,
       requires_reauth
  FROM oauth_links
 WHERE broadcaster_id = ?
 ORDER BY updated_at DESC
 LIMIT 1
            "#,
        )
        .bind(broadcaster_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| row.try_into())
            .transpose()
            .map_err(OauthLinkError::Decode)
    }

    /// Lists OAuth links that are still active and not flagged for reauthorization.
    pub async fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<OauthLink>, OauthLinkError> {
        let rows = sqlx::query_as::<_, OauthLinkRow>(
            r#"
SELECT id,
       broadcaster_id,
       twitch_user_id,
       scopes_json,
       managed_scopes_json,
       access_token,
       refresh_token,
       expires_at,
       created_at,
       updated_at,
       last_validated_at,
       last_refreshed_at,
       last_failure_at,
       last_failure_reason,
       requires_reauth
  FROM oauth_links
 WHERE expires_at > ?
   AND requires_reauth = 0
 ORDER BY updated_at DESC
            "#,
        )
        .bind(to_rfc3339(now))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| row.try_into())
            .collect::<Result<Vec<_>, _>>()
            .map_err(OauthLinkError::Decode)
    }

    /// Retrieves the OAuth link for the provided broadcaster using the supplied transaction.
    pub async fn fetch_by_broadcaster_for_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
    ) -> Result<Option<OauthLink>, OauthLinkError> {
        let row = sqlx::query_as::<_, OauthLinkRow>(
            r#"
SELECT id,
       broadcaster_id,
       twitch_user_id,
       scopes_json,
       managed_scopes_json,
       access_token,
       refresh_token,
       expires_at,
       created_at,
       updated_at,
       last_validated_at,
       last_refreshed_at,
       last_failure_at,
       last_failure_reason,
       requires_reauth
  FROM oauth_links
 WHERE broadcaster_id = ?
 ORDER BY updated_at DESC
 LIMIT 1
            "#,
        )
        .bind(broadcaster_id)
        .fetch_optional(&mut **tx)
        .await?;

        row.map(|row| row.try_into())
            .transpose()
            .map_err(OauthLinkError::Decode)
    }

    /// Updates tokens after a successful refresh/validation cycle.
    pub async fn update_tokens(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        update: &OauthTokenUpdate<'_>,
    ) -> Result<OauthLink, OauthLinkError> {
        let scopes = serde_json::to_string(&update.scopes)?;
        let managed_scopes = serde_json::to_string(&update.managed_scopes)?;
        let refreshed_at = to_rfc3339(update.refreshed_at);
        let validated_at = to_rfc3339(update.validated_at);
        let row = sqlx::query_as::<_, OauthLinkRow>(
            r#"
UPDATE oauth_links
   SET access_token = ?,
       refresh_token = ?,
       expires_at = ?,
       scopes_json = ?,
       managed_scopes_json = ?,
       last_refreshed_at = ?,
       last_validated_at = ?,
       updated_at = ?,
       last_failure_at = NULL,
       last_failure_reason = NULL,
       requires_reauth = 0
 WHERE broadcaster_id = ?
   AND twitch_user_id = ?
 RETURNING id,
           broadcaster_id,
           twitch_user_id,
           scopes_json,
           managed_scopes_json,
           access_token,
           refresh_token,
           expires_at,
           created_at,
           updated_at,
           last_validated_at,
           last_refreshed_at,
           last_failure_at,
           last_failure_reason,
           requires_reauth
            "#,
        )
        .bind(&update.access_token)
        .bind(&update.refresh_token)
        .bind(to_rfc3339(update.expires_at))
        .bind(scopes)
        .bind(managed_scopes)
        .bind(&refreshed_at)
        .bind(&validated_at)
        .bind(to_rfc3339(update.updated_at))
        .bind(update.broadcaster_id)
        .bind(&update.twitch_user_id)
        .fetch_optional(&mut **tx)
        .await?;

        let Some(row) = row else {
            return Err(OauthLinkError::NotFound);
        };

        row.try_into().map_err(OauthLinkError::Decode)
    }

    /// Records a validation outcome without changing tokens.
    pub async fn mark_validation_result(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        result: &OauthValidationResult<'_>,
    ) -> Result<(), OauthLinkError> {
        let validated_at = to_rfc3339(result.validated_at);
        let failure_at = result
            .failure
            .as_ref()
            .map(|failure| to_rfc3339(failure.occurred_at));
        let rows = sqlx::query(
            r#"
UPDATE oauth_links
   SET last_validated_at = ?,
       requires_reauth = ?,
       last_failure_at = ?,
       last_failure_reason = ?
 WHERE broadcaster_id = ?
   AND twitch_user_id = ?
            "#,
        )
        .bind(&validated_at)
        .bind(if result.requires_reauth { 1 } else { 0 })
        .bind(failure_at.as_deref())
        .bind(result.failure.as_ref().map(|failure| failure.reason))
        .bind(result.broadcaster_id)
        .bind(&result.twitch_user_id)
        .execute(&mut **tx)
        .await?;

        if rows.rows_affected() == 0 {
            return Err(OauthLinkError::NotFound);
        }

        Ok(())
    }

    /// Marks a refresh/validation failure and optionally toggles re-authentication.
    pub async fn mark_failure(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        failure: &OauthFailure<'_>,
    ) -> Result<(), OauthLinkError> {
        let failure_at = to_rfc3339(failure.occurred_at);
        let rows = sqlx::query(
            r#"
UPDATE oauth_links
   SET last_failure_at = ?,
       last_failure_reason = ?,
       requires_reauth = ?
 WHERE broadcaster_id = ?
   AND twitch_user_id = ?
            "#,
        )
        .bind(&failure_at)
        .bind(failure.reason)
        .bind(if failure.requires_reauth { 1 } else { 0 })
        .bind(failure.broadcaster_id)
        .bind(&failure.twitch_user_id)
        .execute(&mut **tx)
        .await?;

        if rows.rows_affected() == 0 {
            return Err(OauthLinkError::NotFound);
        }

        Ok(())
    }
}

/// Complete OAuth link record.
#[derive(Debug, Clone, PartialEq)]
pub struct OauthLink {
    pub id: String,
    pub broadcaster_id: String,
    pub twitch_user_id: String,
    pub scopes: Vec<String>,
    pub managed_scopes: Vec<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_validated_at: Option<DateTime<Utc>>,
    pub last_refreshed_at: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub last_failure_reason: Option<String>,
    pub requires_reauth: bool,
}

/// New OAuth link payload.
pub struct NewOauthLink<'a> {
    pub id: String,
    pub broadcaster_id: &'a str,
    pub twitch_user_id: String,
    pub scopes: Vec<String>,
    pub managed_scopes: Vec<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// OAuth token refresh/update payload.
pub struct OauthTokenUpdate<'a> {
    pub broadcaster_id: &'a str,
    pub twitch_user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub scopes: Vec<String>,
    pub managed_scopes: Vec<String>,
    pub refreshed_at: DateTime<Utc>,
    pub validated_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Validation outcome payload.
pub struct OauthValidationResult<'a> {
    pub broadcaster_id: &'a str,
    pub twitch_user_id: String,
    pub validated_at: DateTime<Utc>,
    pub requires_reauth: bool,
    pub failure: Option<OauthValidationFailure<'a>>,
}

/// Validation failure details.
pub struct OauthValidationFailure<'a> {
    pub occurred_at: DateTime<Utc>,
    pub reason: &'a str,
}

/// Generic OAuth failure payload.
pub struct OauthFailure<'a> {
    pub broadcaster_id: &'a str,
    pub twitch_user_id: String,
    pub occurred_at: DateTime<Utc>,
    pub reason: &'a str,
    pub requires_reauth: bool,
}

#[derive(Debug, Error)]
pub enum OauthLinkError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("failed to decode oauth link: {0}")]
    Decode(#[from] OauthLinkDecodeError),
    #[error("oauth link not found")]
    NotFound,
    #[error("failed to encode scopes json: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum OauthLinkDecodeError {
    #[error("invalid json payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid timestamp: {0}")]
    Timestamp(#[from] chrono::ParseError),
}

#[derive(Debug, sqlx::FromRow)]
struct OauthLinkRow {
    id: String,
    broadcaster_id: String,
    twitch_user_id: String,
    scopes_json: String,
    managed_scopes_json: String,
    access_token: String,
    refresh_token: String,
    expires_at: String,
    created_at: String,
    updated_at: String,
    last_validated_at: Option<String>,
    last_refreshed_at: Option<String>,
    last_failure_at: Option<String>,
    last_failure_reason: Option<String>,
    requires_reauth: i64,
}

impl TryFrom<OauthLinkRow> for OauthLink {
    type Error = OauthLinkDecodeError;

    fn try_from(value: OauthLinkRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            broadcaster_id: value.broadcaster_id,
            twitch_user_id: value.twitch_user_id,
            scopes: serde_json::from_str(&value.scopes_json)?,
            managed_scopes: serde_json::from_str(&value.managed_scopes_json)?,
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_at: parse_datetime(&value.expires_at)?,
            created_at: parse_datetime(&value.created_at)?,
            updated_at: parse_datetime(&value.updated_at)?,
            last_validated_at: parse_optional_datetime(value.last_validated_at)?,
            last_refreshed_at: parse_optional_datetime(value.last_refreshed_at)?,
            last_failure_at: parse_optional_datetime(value.last_failure_at)?,
            last_failure_reason: value.last_failure_reason,
            requires_reauth: value.requires_reauth != 0,
        })
    }
}

/// Repository handling Helix backfill checkpoints.
#[derive(Clone)]
pub struct HelixBackfillRepository {
    pool: SqlitePool,
}

impl HelixBackfillRepository {
    /// Retrieves the checkpoint for a broadcaster.
    pub async fn fetch(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<HelixBackfillCheckpoint>, HelixBackfillError> {
        let row = sqlx::query_as::<_, HelixBackfillRow>(
            r#"
SELECT broadcaster_id,
       cursor,
       last_redemption_id,
       last_seen_at,
       last_run_at,
       status,
       error_message,
       updated_at
  FROM helix_backfill_checkpoints
 WHERE broadcaster_id = ?
            "#,
        )
        .bind(broadcaster_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(HelixBackfillError::Database)?;

        row.map(|row| row.try_into())
            .transpose()
            .map_err(HelixBackfillError::Decode)
    }

    /// Upserts the checkpoint atomically.
    pub async fn upsert(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        checkpoint: &HelixBackfillCheckpoint,
    ) -> Result<(), HelixBackfillError> {
        let last_seen_at = checkpoint.last_seen_at.map(to_rfc3339);
        let error_message = checkpoint.error_message.clone();
        sqlx::query(
            r#"
INSERT INTO helix_backfill_checkpoints(
    broadcaster_id,
    cursor,
    last_redemption_id,
    last_seen_at,
    last_run_at,
    status,
    error_message,
    updated_at
)
VALUES(?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(broadcaster_id) DO UPDATE SET
    cursor = excluded.cursor,
    last_redemption_id = excluded.last_redemption_id,
    last_seen_at = excluded.last_seen_at,
    last_run_at = excluded.last_run_at,
    status = excluded.status,
    error_message = excluded.error_message,
    updated_at = excluded.updated_at
            "#,
        )
        .bind(&checkpoint.broadcaster_id)
        .bind(&checkpoint.cursor)
        .bind(&checkpoint.last_redemption_id)
        .bind(last_seen_at.as_deref())
        .bind(to_rfc3339(checkpoint.last_run_at))
        .bind(checkpoint.status.as_str())
        .bind(error_message)
        .bind(to_rfc3339(checkpoint.updated_at))
        .execute(&mut **tx)
        .await
        .map_err(HelixBackfillError::Database)?;

        Ok(())
    }
}

/// Helix backfill checkpoint domain object.
#[derive(Debug, Clone, PartialEq)]
pub struct HelixBackfillCheckpoint {
    pub broadcaster_id: String,
    pub cursor: Option<String>,
    pub last_redemption_id: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_run_at: DateTime<Utc>,
    pub status: HelixBackfillStatus,
    pub error_message: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Status of the Helix backfill worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelixBackfillStatus {
    Idle,
    Running,
    Error,
}

impl HelixBackfillStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Error => "error",
        }
    }

    fn from_str(value: &str) -> Result<Self, HelixBackfillDecodeError> {
        match value {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "error" => Ok(Self::Error),
            other => Err(HelixBackfillDecodeError::Status(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum HelixBackfillError {
    #[error("database error: {0}")]
    Database(sqlx::Error),
    #[error("failed to decode checkpoint: {0}")]
    Decode(#[from] HelixBackfillDecodeError),
}

#[derive(Debug, Error)]
pub enum HelixBackfillDecodeError {
    #[error("invalid timestamp: {0}")]
    Timestamp(#[from] chrono::ParseError),
    #[error("invalid status value: {0}")]
    Status(String),
}

#[derive(Debug, sqlx::FromRow)]
struct HelixBackfillRow {
    broadcaster_id: String,
    cursor: Option<String>,
    last_redemption_id: Option<String>,
    last_seen_at: Option<String>,
    last_run_at: String,
    status: String,
    error_message: Option<String>,
    updated_at: String,
}

impl TryFrom<HelixBackfillRow> for HelixBackfillCheckpoint {
    type Error = HelixBackfillDecodeError;

    fn try_from(value: HelixBackfillRow) -> Result<Self, Self::Error> {
        Ok(Self {
            broadcaster_id: value.broadcaster_id,
            cursor: value.cursor,
            last_redemption_id: value.last_redemption_id,
            last_seen_at: parse_optional_datetime(value.last_seen_at)?,
            last_run_at: parse_datetime(&value.last_run_at)?,
            status: HelixBackfillStatus::from_str(&value.status)?,
            error_message: value.error_message,
            updated_at: parse_datetime(&value.updated_at)?,
        })
    }
}

fn to_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value).map(|dt| dt.with_timezone(&Utc))
}

fn parse_optional_datetime(
    value: Option<String>,
) -> Result<Option<DateTime<Utc>>, chrono::ParseError> {
    value
        .map(|v| DateTime::parse_from_rfc3339(&v).map(|dt| dt.with_timezone(&Utc)))
        .transpose()
}

fn map_status(value: &str) -> QueueEntryStatus {
    match value {
        "QUEUED" => QueueEntryStatus::Queued,
        "COMPLETED" => QueueEntryStatus::Completed,
        "REMOVED" => QueueEntryStatus::Removed,
        _ => QueueEntryStatus::Queued,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[tokio::test]
    async fn oauth_login_state_insert_and_consume() {
        let db = setup_db().await;
        let repo = db.oauth_login_states();
        let state = NewOauthLoginState {
            state: "state-1".into(),
            broadcaster_id: "b-1",
            code_verifier: "verifier".into(),
            redirect_to: Some("/admin".into()),
            created_at: Utc::now(),
            expires_at: Utc::now() + ChronoDuration::minutes(10),
        };

        repo.insert(&state).await.expect("insert login state");

        let consumed = repo
            .consume("state-1")
            .await
            .expect("consume state")
            .expect("state present");
        assert_eq!(consumed.state, "state-1");
        assert_eq!(consumed.redirect_to.as_deref(), Some("/admin"));

        let missing = repo.consume("state-1").await.expect("consume again");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn oauth_login_state_purge_expired_removes_rows() {
        let db = setup_db().await;
        let repo = db.oauth_login_states();
        let now = Utc::now();
        for idx in 0..3 {
            repo.insert(&NewOauthLoginState {
                state: format!("state-{idx}"),
                broadcaster_id: "b-1",
                code_verifier: "verifier".into(),
                redirect_to: None,
                created_at: now - ChronoDuration::minutes(20 + idx),
                expires_at: now - ChronoDuration::minutes(5 + idx),
            })
            .await
            .expect("insert state");
        }

        let purged = repo.purge_expired(now, 2).await.expect("purge");
        assert_eq!(purged, 2);

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM oauth_login_states")
            .fetch_one(db.pool())
            .await
            .expect("count states");
        assert_eq!(remaining.0, 1);
    }

    #[tokio::test]
    async fn oauth_login_state_has_active_respects_expiration() {
        let db = setup_db().await;
        let repo = db.oauth_login_states();
        let now = Utc::now();
        repo.insert(&NewOauthLoginState {
            state: "state-1".into(),
            broadcaster_id: "b-1",
            code_verifier: "verifier".into(),
            redirect_to: None,
            created_at: now,
            expires_at: now + ChronoDuration::minutes(10),
        })
        .await
        .expect("insert state");

        assert!(repo
            .has_active("b-1", now + ChronoDuration::minutes(1))
            .await
            .expect("has active"));

        assert!(!repo
            .has_active("b-1", now + ChronoDuration::minutes(15))
            .await
            .expect("has active after expiry"));
    }

    #[tokio::test]
    async fn oauth_link_upsert_and_fetch_roundtrip() {
        let db = setup_db().await;
        let repo = db.oauth_links();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let link = repo
            .upsert_link(
                &mut tx,
                &NewOauthLink {
                    id: "link-1".into(),
                    broadcaster_id: "b-1",
                    twitch_user_id: "twitch-123".into(),
                    scopes: vec!["scope:a".into(), "scope:b".into()],
                    managed_scopes: vec!["scope:b".into()],
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: now + ChronoDuration::hours(1),
                    created_at: now,
                    updated_at: now,
                },
            )
            .await
            .expect("upsert");
        tx.commit().await.expect("commit");

        assert_eq!(link.twitch_user_id, "twitch-123");
        assert_eq!(link.scopes.len(), 2);
        assert!(link.last_validated_at.is_none());

        let fetched = repo
            .fetch_by_broadcaster("b-1")
            .await
            .expect("fetch")
            .expect("link present");
        assert_eq!(fetched.twitch_user_id, "twitch-123");
        assert_eq!(fetched.access_token, "access");
        assert!(!fetched.requires_reauth);
    }

    #[tokio::test]
    async fn oauth_link_updates_tokens_and_validation() {
        let db = setup_db().await;
        let repo = db.oauth_links();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        repo.upsert_link(
            &mut tx,
            &NewOauthLink {
                id: "link-1".into(),
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-123".into(),
                scopes: vec!["scope:a".into()],
                managed_scopes: vec!["scope:a".into()],
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now + ChronoDuration::hours(1),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("upsert");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin upd");
        repo.update_tokens(
            &mut tx,
            &OauthTokenUpdate {
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-123".into(),
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
                expires_at: now + ChronoDuration::hours(2),
                scopes: vec!["scope:a".into(), "scope:b".into()],
                managed_scopes: vec!["scope:b".into()],
                refreshed_at: now,
                validated_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("update tokens");
        tx.commit().await.expect("commit upd");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin validate");
        repo.mark_validation_result(
            &mut tx,
            &OauthValidationResult {
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-123".into(),
                validated_at: now,
                requires_reauth: true,
                failure: Some(OauthValidationFailure {
                    occurred_at: now,
                    reason: "invalid token",
                }),
            },
        )
        .await
        .expect("mark validation");
        tx.commit().await.expect("commit validate");

        let fetched = repo
            .fetch_by_broadcaster("b-1")
            .await
            .expect("fetch")
            .expect("link present");
        assert_eq!(fetched.access_token, "new-access");
        assert!(fetched.requires_reauth);
        assert_eq!(
            fetched.last_failure_reason.as_deref(),
            Some("invalid token")
        );
    }

    #[tokio::test]
    async fn oauth_link_marks_failure() {
        let db = setup_db().await;
        let repo = db.oauth_links();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        repo.upsert_link(
            &mut tx,
            &NewOauthLink {
                id: "link-1".into(),
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-123".into(),
                scopes: vec!["scope:a".into()],
                managed_scopes: vec!["scope:a".into()],
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now + ChronoDuration::hours(1),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("upsert");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin fail");
        repo.mark_failure(
            &mut tx,
            &OauthFailure {
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-123".into(),
                occurred_at: now,
                reason: "token expired",
                requires_reauth: true,
            },
        )
        .await
        .expect("mark failure");
        tx.commit().await.expect("commit fail");

        let fetched = repo
            .fetch_by_broadcaster("b-1")
            .await
            .expect("fetch")
            .expect("link present");
        assert!(fetched.requires_reauth);
        assert_eq!(
            fetched.last_failure_reason.as_deref(),
            Some("token expired")
        );
    }

    #[tokio::test]
    async fn oauth_link_list_active_filters_expired_and_reauth() {
        let db = setup_db().await;
        let repo = db.oauth_links();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();

        repo.upsert_link(
            &mut tx,
            &NewOauthLink {
                id: "link-1".into(),
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-1".into(),
                scopes: vec!["scope:a".into()],
                managed_scopes: vec!["scope:a".into()],
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now + ChronoDuration::hours(1),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("upsert active");

        repo.upsert_link(
            &mut tx,
            &NewOauthLink {
                id: "link-2".into(),
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-2".into(),
                scopes: vec!["scope:a".into()],
                managed_scopes: vec!["scope:a".into()],
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now - ChronoDuration::minutes(5),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("upsert expired");
        tx.commit().await.expect("commit initial");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin failure");
        repo.mark_failure(
            &mut tx,
            &OauthFailure {
                broadcaster_id: "b-1",
                twitch_user_id: "twitch-2".into(),
                occurred_at: now,
                reason: "expired",
                requires_reauth: true,
            },
        )
        .await
        .expect("mark failure");
        tx.commit().await.expect("commit failure");

        let active = repo.list_active(now).await.expect("list active");

        assert_eq!(active.len(), 1);
        assert_eq!(active[0].broadcaster_id, "b-1");
    }

    #[tokio::test]
    async fn helix_backfill_upsert_and_fetch() {
        let db = setup_db().await;
        let repo = db.helix_backfill();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let checkpoint = HelixBackfillCheckpoint {
            broadcaster_id: "b-1".into(),
            cursor: Some("cursor".into()),
            last_redemption_id: Some("red-1".into()),
            last_seen_at: Some(now - ChronoDuration::minutes(5)),
            last_run_at: now,
            status: HelixBackfillStatus::Running,
            error_message: Some("processing".into()),
            updated_at: now,
        };
        repo.upsert(&mut tx, &checkpoint).await.expect("upsert");
        tx.commit().await.expect("commit");

        let fetched = repo
            .fetch("b-1")
            .await
            .expect("fetch")
            .expect("checkpoint present");
        assert_eq!(fetched.cursor.as_deref(), Some("cursor"));
        assert_eq!(fetched.status, HelixBackfillStatus::Running);
        assert_eq!(fetched.error_message.as_deref(), Some("processing"));
    }
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
        sqlx::query(
            "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES ('b-1', 0, '2024-01-01T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("insert state index");
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
    async fn delete_older_event_raw_rows_in_batches() {
        let db = setup_db().await;
        let repo = db.event_raw();
        let now = Utc::now();

        for idx in 0..2 {
            let record = NewEventRaw {
                id: Cow::Owned(format!("evt-old-{idx}")),
                broadcaster_id: Cow::Borrowed("b-1"),
                msg_id: Cow::Owned(format!("msg-old-{idx}")),
                event_type: Cow::Borrowed("test.event"),
                payload_json: Cow::Borrowed("{}"),
                event_at: now - ChronoDuration::hours(80 + idx),
                received_at: now - ChronoDuration::hours(80 + idx),
                source: "webhook",
            };
            assert!(matches!(
                repo.insert(record).await,
                Ok(EventRawInsertOutcome::Inserted)
            ));
        }

        let record = NewEventRaw {
            id: Cow::Borrowed("evt-new"),
            broadcaster_id: Cow::Borrowed("b-1"),
            msg_id: Cow::Borrowed("msg-new"),
            event_type: Cow::Borrowed("test.event"),
            payload_json: Cow::Borrowed("{}"),
            event_at: now,
            received_at: now,
            source: "webhook",
        };
        assert!(matches!(
            repo.insert(record).await,
            Ok(EventRawInsertOutcome::Inserted)
        ));

        let threshold = now - ChronoDuration::hours(72);
        let deleted = repo
            .delete_older_than_batch(threshold, 1000)
            .await
            .expect("delete batch");
        assert_eq!(deleted, 2);

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_raw")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(remaining.0, 1);
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

    #[tokio::test]
    async fn update_settings_persists_payload() {
        let db = setup_db().await;
        let repo = db.broadcasters();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let mut settings = repo
            .fetch_settings("b-1")
            .await
            .expect("fetch settings")
            .settings;
        settings.overlay_theme = "dark".into();
        settings.group_size = 3;

        repo.update_settings(&mut tx, "b-1", &settings, Utc::now())
            .await
            .expect("update settings");
        tx.commit().await.expect("commit");

        let reloaded = repo
            .fetch_settings("b-1")
            .await
            .expect("fetch updated settings");
        assert_eq!(reloaded.settings.overlay_theme, "dark");
        assert_eq!(reloaded.settings.group_size, 3);
    }

    #[tokio::test]
    async fn update_settings_errors_when_missing() {
        let db = setup_db().await;
        let repo = db.broadcasters();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let template = repo
            .fetch_settings("b-1")
            .await
            .expect("fetch settings")
            .settings;
        let err = repo
            .update_settings(&mut tx, "missing", &template, Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, SettingsUpdateError::NotFound));
    }

    #[tokio::test]
    async fn command_log_find_by_op_id_returns_entry() {
        let db = setup_db().await;
        let repo = db.command_log();
        let mut tx = repo.begin().await.expect("begin");
        let record = NewCommandLog {
            broadcaster_id: "b-1",
            op_id: Some("op-1"),
            command_type: "queue.complete",
            payload_json: "{}",
            created_at: Utc::now(),
        };
        repo.append(&mut tx, record).await.expect("append");
        tx.commit().await.expect("commit");

        let mut tx = repo.begin().await.expect("begin read");
        let found = repo
            .find_by_op_id(&mut tx, "b-1", "op-1")
            .await
            .expect("find");
        assert!(found.is_some());
        let entry = found.unwrap();
        assert_eq!(entry.command_type, "queue.complete");
        assert_eq!(entry.payload_json, "{}");
    }

    #[tokio::test]
    async fn delete_older_command_log_rows_respects_limit() {
        let db = setup_db().await;
        let repo = db.command_log();
        let now = Utc::now();

        for idx in 0..3 {
            let mut tx = repo.begin().await.expect("begin");
            repo.append(
                &mut tx,
                NewCommandLog {
                    broadcaster_id: "b-1",
                    op_id: Some(&format!("op-old-{idx}")),
                    command_type: "queue.enqueue",
                    payload_json: "{}",
                    created_at: now - ChronoDuration::hours(90 - idx as i64),
                },
            )
            .await
            .expect("append");
            tx.commit().await.expect("commit");
        }

        let threshold = now - ChronoDuration::hours(72);
        let deleted = repo
            .delete_older_than_batch(threshold, 1)
            .await
            .expect("delete batch");
        assert_eq!(deleted, 1);

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM command_log")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(remaining.0, 2);
    }

    #[tokio::test]
    async fn wal_checkpoint_returns_counters() {
        let db = setup_db().await;
        let stats = db.wal_checkpoint_truncate().await.expect("wal checkpoint");

        assert!(stats.busy_frames >= -1);
        assert!(stats.log_frames >= -1);
        assert!(stats.checkpointed_frames >= -1);
    }

    #[tokio::test]
    async fn queue_mark_completed_transitions_entry() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let new_entry = NewQueueEntry {
            id: "q-1".into(),
            broadcaster_id: "b-1",
            user_id: "user-1",
            user_login: "alice".into(),
            user_display_name: "Alice".into(),
            user_avatar: None,
            reward_id: "reward-1",
            redemption_id: Some("red-1".into()),
            enqueued_at: now,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: true,
            last_updated_at: now,
        };
        queue_repo
            .insert_entry(&mut tx, &new_entry)
            .await
            .expect("insert entry");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin update");
        let updated = queue_repo
            .mark_completed(&mut tx, "b-1", "q-1", Utc::now())
            .await
            .expect("mark completed");
        assert_eq!(updated.status, QueueEntryStatus::Completed);
        assert!(updated.status_reason.is_none());
        tx.commit().await.expect("commit update");
    }

    #[tokio::test]
    async fn queue_mark_removed_sets_reason() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let new_entry = NewQueueEntry {
            id: "q-2".into(),
            broadcaster_id: "b-1",
            user_id: "user-2",
            user_login: "bob".into(),
            user_display_name: "Bob".into(),
            user_avatar: None,
            reward_id: "reward-1",
            redemption_id: Some("red-2".into()),
            enqueued_at: now,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: false,
            last_updated_at: now,
        };
        queue_repo
            .insert_entry(&mut tx, &new_entry)
            .await
            .expect("insert entry");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin update");
        let updated = queue_repo
            .mark_removed(&mut tx, "b-1", "q-2", QueueRemovalReason::Undo, Utc::now())
            .await
            .expect("mark removed");
        assert_eq!(updated.status, QueueEntryStatus::Removed);
        assert_eq!(updated.status_reason.as_deref(), Some("UNDO"));
        tx.commit().await.expect("commit update");
    }

    #[tokio::test]
    async fn queue_mark_completed_errors_for_missing_entry() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin update");
        let err = queue_repo
            .mark_completed(&mut tx, "b-1", "missing", Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, QueueError::NotFound));
    }

    #[tokio::test]
    async fn queue_mark_completed_errors_for_terminal_entry() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let new_entry = NewQueueEntry {
            id: "q-3".into(),
            broadcaster_id: "b-1",
            user_id: "user-3",
            user_login: "cara".into(),
            user_display_name: "Cara".into(),
            user_avatar: None,
            reward_id: "reward-1",
            redemption_id: Some("red-3".into()),
            enqueued_at: now,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: false,
            last_updated_at: now,
        };
        queue_repo
            .insert_entry(&mut tx, &new_entry)
            .await
            .expect("insert entry");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin update");
        queue_repo
            .mark_completed(&mut tx, "b-1", "q-3", Utc::now())
            .await
            .expect("mark completed");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin second");
        let err = queue_repo
            .mark_completed(&mut tx, "b-1", "q-3", Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QueueError::InvalidTransition(QueueEntryStatus::Completed)
        ));
    }

    #[tokio::test]
    async fn queue_find_by_redemption_within_transaction() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let new_entry = NewQueueEntry {
            id: "q-lookup".into(),
            broadcaster_id: "b-1",
            user_id: "user-lookup",
            user_login: "lookup".into(),
            user_display_name: "Lookup".into(),
            user_avatar: None,
            reward_id: "reward-lookup",
            redemption_id: Some("red-lookup".into()),
            enqueued_at: now,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: false,
            last_updated_at: now,
        };
        queue_repo
            .insert_entry(&mut tx, &new_entry)
            .await
            .expect("insert entry");

        let found = queue_repo
            .find_entry_by_redemption_for_update(&mut tx, "b-1", "red-lookup")
            .await
            .expect("fetch")
            .expect("entry");
        assert_eq!(found.id, "q-lookup");
        assert_eq!(found.redemption_id.as_deref(), Some("red-lookup"));
    }

    #[tokio::test]
    async fn queue_update_managed_toggles_flag() {
        let db = setup_db().await;
        let queue_repo = db.queue();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        let new_entry = NewQueueEntry {
            id: "q-managed".into(),
            broadcaster_id: "b-1",
            user_id: "user-flag",
            user_login: "flag".into(),
            user_display_name: "Flag".into(),
            user_avatar: None,
            reward_id: "reward-flag",
            redemption_id: Some("red-flag".into()),
            enqueued_at: now,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: false,
            last_updated_at: now,
        };
        queue_repo
            .insert_entry(&mut tx, &new_entry)
            .await
            .expect("insert entry");

        let updated = queue_repo
            .update_managed(&mut tx, "b-1", "q-managed", true, Utc::now())
            .await
            .expect("update managed");
        assert!(updated.managed);
    }

    #[tokio::test]
    async fn counter_decrement_clamps_to_zero() {
        let db = setup_db().await;
        let counter_repo = db.daily_counters();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc::now();
        counter_repo
            .increment(
                &mut tx,
                &NewDailyCounter {
                    day: "2024-01-01".into(),
                    broadcaster_id: "b-1",
                    user_id: "user-1",
                    updated_at: now,
                },
            )
            .await
            .expect("increment");
        counter_repo
            .increment(
                &mut tx,
                &NewDailyCounter {
                    day: "2024-01-01".into(),
                    broadcaster_id: "b-1",
                    user_id: "user-1",
                    updated_at: now,
                },
            )
            .await
            .expect("increment");
        tx.commit().await.expect("commit");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin dec");
        let value = counter_repo
            .decrement(&mut tx, "2024-01-01", "b-1", "user-1", Utc::now())
            .await
            .expect("decrement");
        assert_eq!(value, Some(1));
        tx.commit().await.expect("commit dec");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin dec2");
        let value = counter_repo
            .decrement(&mut tx, "2024-01-01", "b-1", "user-1", Utc::now())
            .await
            .expect("decrement");
        assert_eq!(value, Some(0));
        tx.commit().await.expect("commit dec2");

        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin dec3");
        let value = counter_repo
            .decrement(&mut tx, "2024-01-01", "b-1", "user-1", Utc::now())
            .await
            .expect("decrement");
        assert_eq!(value, Some(0));
    }

    #[tokio::test]
    async fn counter_fetch_value_reads_current_count() {
        let db = setup_db().await;
        let counter_repo = db.daily_counters();
        let command_repo = db.command_log();
        let mut tx = command_repo.begin().await.expect("begin");

        sqlx::query(
            "INSERT INTO daily_counters(day, broadcaster_id, user_id, count, updated_at) VALUES ('2024-01-01','b-1','user-1', 5, '2024-01-01T00:00:00Z')",
        )
        .execute(&mut *tx)
        .await
        .expect("insert counter");

        let value = counter_repo
            .fetch_value(&mut tx, "2024-01-01", "b-1", "user-1")
            .await
            .expect("fetch value");
        assert_eq!(value, Some(5));

        let missing = counter_repo
            .fetch_value(&mut tx, "2024-01-01", "b-1", "user-2")
            .await
            .expect("fetch missing");
        assert_eq!(missing, None);
    }
}

#[derive(Debug, sqlx::FromRow)]
struct QueueEntryRow {
    id: String,
    broadcaster_id: String,
    user_id: String,
    user_login: String,
    user_display_name: String,
    user_avatar: Option<String>,
    reward_id: String,
    redemption_id: Option<String>,
    #[sqlx(rename = "enqueued_at: DateTime<Utc>")]
    enqueued_at: DateTime<Utc>,
    status: String,
    status_reason: Option<String>,
    managed: i64,
    #[sqlx(rename = "last_updated_at: DateTime<Utc>")]
    last_updated_at: DateTime<Utc>,
}

impl QueueEntryRow {
    fn into_domain(self) -> QueueEntry {
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
            status: map_status(&self.status),
            status_reason: self.status_reason,
            managed: self.managed != 0,
            last_updated_at: self.last_updated_at,
        }
    }
}
