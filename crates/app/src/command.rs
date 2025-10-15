use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use metrics::counter;
use serde_json::to_string;
use sqlx::{Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use twi_overlay_core::projector::Projector;
use twi_overlay_core::types::{
    Command, EnqueueCommand, Patch, QueueEntry, QueueEntryStatus, RedemptionUpdateCommand,
};
use twi_overlay_storage::{
    CommandLogError, DailyCounterError, DailyCounterRepository, Database, NewCommandLog,
    NewDailyCounter, NewQueueEntry, QueueError, QueueRepository,
};

use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload, TapHub};

/// Executes commands by persisting them to the command log and mutating state tables.
#[derive(Clone)]
pub struct CommandExecutor {
    database: Database,
    tap: TapHub,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl CommandExecutor {
    pub fn new(
        database: Database,
        tap: TapHub,
        clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Self {
        Self {
            database,
            tap,
            clock,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Executes a batch of commands for the provided broadcaster, returning generated patches.
    pub async fn execute(
        &self,
        broadcaster_id: &str,
        timezone: &str,
        commands: &[Command],
    ) -> Result<Vec<Patch>, CommandExecutorError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }

        let command_log_repo = self.database.command_log();
        let mut tx = command_log_repo.begin().await?;
        let queue_repo = self.database.queue();
        let counter_repo = self.database.daily_counters();
        let mut patches = Vec::with_capacity(commands.len());

        for command in commands {
            match command {
                Command::Enqueue(enqueue) => {
                    let patch = self
                        .handle_enqueue(
                            &mut tx,
                            broadcaster_id,
                            timezone,
                            enqueue,
                            &queue_repo,
                            &counter_repo,
                        )
                        .await?;
                    patches.push(patch);
                }
                Command::RedemptionUpdate(update) => {
                    let patch = self
                        .handle_redemption_update(&mut tx, broadcaster_id, update)
                        .await?;
                    patches.push(patch);
                }
            }
        }

        tx.commit().await?;
        Ok(patches)
    }

    async fn handle_enqueue(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        timezone: &str,
        command: &EnqueueCommand,
        queue_repo: &QueueRepository,
        counter_repo: &DailyCounterRepository,
    ) -> Result<Patch, CommandExecutorError> {
        let serialized = to_string(command)?;
        let inserted_at = self.now();
        let version = self
            .append_command(tx, broadcaster_id, "enqueue", &serialized, inserted_at)
            .await?;

        let command_enum = Command::Enqueue(command.clone());
        self.emit_command_event(broadcaster_id, version, "enqueue", &command_enum);

        let entry = self.build_queue_entry(command);
        let new_entry = NewQueueEntry {
            id: entry.id.clone(),
            broadcaster_id,
            user_id: &command.user.id,
            user_login: entry.user_login.clone(),
            user_display_name: entry.user_display_name.clone(),
            user_avatar: entry.user_avatar.clone(),
            reward_id: &command.reward.id,
            redemption_id: Some(command.redemption_id.clone()),
            enqueued_at: entry.enqueued_at,
            status: entry.status,
            status_reason: entry.status_reason.clone(),
            managed: entry.managed,
            last_updated_at: entry.last_updated_at,
        };
        queue_repo.insert_entry(tx, &new_entry).await?;

        let day = compute_local_day(command.issued_at, timezone)?;
        let user_today_count = counter_repo
            .increment(
                tx,
                &NewDailyCounter {
                    day,
                    broadcaster_id,
                    user_id: &command.user.id,
                    updated_at: inserted_at,
                },
            )
            .await?;

        let patch = Projector::queue_enqueued(version, command.issued_at, entry, user_today_count);
        self.emit_projector_event(broadcaster_id, version, &patch, &command_enum);
        counter!("projector_patches_total", 1, "type" => patch.kind_str());

        Ok(patch)
    }

    async fn handle_redemption_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        command: &RedemptionUpdateCommand,
    ) -> Result<Patch, CommandExecutorError> {
        let serialized = to_string(command)?;
        let inserted_at = self.now();
        let version = self
            .append_command(
                tx,
                broadcaster_id,
                "redemption.update",
                &serialized,
                inserted_at,
            )
            .await?;

        let command_enum = Command::RedemptionUpdate(command.clone());
        self.emit_command_event(broadcaster_id, version, "redemption.update", &command_enum);

        let patch = Projector::redemption_updated(version, command.issued_at, command);
        self.emit_projector_event(broadcaster_id, version, &patch, &command_enum);
        counter!("projector_patches_total", 1, "type" => patch.kind_str());

        Ok(patch)
    }

    async fn append_command(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        command_type: &str,
        payload_json: &str,
        created_at: DateTime<Utc>,
    ) -> Result<u64, CommandExecutorError> {
        let record = NewCommandLog {
            broadcaster_id,
            op_id: None,
            command_type,
            payload_json,
            created_at,
        };
        let version = self.database.command_log().append(tx, record).await?;
        Ok(version)
    }

    fn emit_command_event(
        &self,
        broadcaster_id: &str,
        version: u64,
        kind: &str,
        command: &Command,
    ) {
        let payload = command.redacted();
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Command,
            trace_id: None,
            op_id: None,
            version: Some(version),
            broadcaster_id: Some(broadcaster_id.to_string()),
            meta: StageMetadata {
                message: Some(kind.to_string()),
                ..StageMetadata::default()
            },
            r#in: StagePayload {
                redacted: true,
                payload: payload.clone(),
                truncated: None,
            },
            out: StagePayload {
                redacted: true,
                payload,
                truncated: None,
            },
        };
        self.tap.publish(event);
    }

    fn emit_projector_event(
        &self,
        broadcaster_id: &str,
        version: u64,
        patch: &Patch,
        command: &Command,
    ) {
        let command_value = command.redacted();
        let patch_value = serde_json::json!({
            "type": patch.kind_str(),
            "version": patch.version,
        });
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Projector,
            trace_id: None,
            op_id: None,
            version: Some(version),
            broadcaster_id: Some(broadcaster_id.to_string()),
            meta: StageMetadata {
                message: Some(patch.kind_str().to_string()),
                ..StageMetadata::default()
            },
            r#in: StagePayload {
                redacted: true,
                payload: command_value,
                truncated: None,
            },
            out: StagePayload {
                redacted: true,
                payload: patch_value,
                truncated: None,
            },
        };
        self.tap.publish(event);
    }

    fn build_queue_entry(&self, command: &EnqueueCommand) -> QueueEntry {
        let issued_at = command.issued_at;
        let user_login = command
            .user
            .login
            .clone()
            .unwrap_or_else(|| command.user.id.clone());
        let user_display_name = command
            .user
            .display_name
            .clone()
            .or(command.user.login.clone())
            .unwrap_or_else(|| command.user.id.clone());

        QueueEntry {
            id: Uuid::new_v4().to_string(),
            broadcaster_id: command.broadcaster_id.clone(),
            user_id: command.user.id.clone(),
            user_login,
            user_display_name,
            user_avatar: None,
            reward_id: command.reward.id.clone(),
            redemption_id: Some(command.redemption_id.clone()),
            enqueued_at: issued_at,
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: command.managed.unwrap_or(false),
            last_updated_at: issued_at,
        }
    }
}

pub fn compute_local_day(
    occurred_at: DateTime<Utc>,
    timezone: &str,
) -> Result<String, CommandExecutorError> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| CommandExecutorError::InvalidTimezone(timezone.to_string()))?;
    let local_time = occurred_at.with_timezone(&tz);
    Ok(local_time.format("%Y-%m-%d").to_string())
}

#[derive(Debug, Error)]
pub enum CommandExecutorError {
    #[error("failed to serialize command: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("command log error: {0}")]
    CommandLog(#[from] CommandLogError),
    #[error("queue error: {0}")]
    Queue(#[from] QueueError),
    #[error("counter error: {0}")]
    Counter(#[from] DailyCounterError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tap::TapHub;
    use twi_overlay_core::types::{
        CommandResult, CommandSource, NormalizedReward, NormalizedUser, RedemptionUpdateMode,
    };

    async fn setup_executor() -> CommandExecutor {
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");
        sqlx::query(
            "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at) VALUES ('b-1','twitch-1','Example','UTC','{}','2024-01-01T00:00:00Z','2024-01-01T00:00:00Z')",
        )
        .execute(database.pool())
        .await
        .expect("insert broadcaster");
        sqlx::query(
            "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES ('b-1', 0, '2024-01-01T00:00:00Z')",
        )
        .execute(database.pool())
        .await
        .expect("insert state index");

        CommandExecutor::new(database, TapHub::new(), Arc::new(Utc::now))
    }

    fn enqueue_command() -> Command {
        Command::Enqueue(EnqueueCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: Utc::now(),
            source: CommandSource::Policy,
            user: NormalizedUser {
                id: "u-1".to_string(),
                login: Some("alice".to_string()),
                display_name: Some("Alice".to_string()),
            },
            reward: NormalizedReward {
                id: "r-join".to_string(),
                title: Some("Join".to_string()),
                cost: Some(1),
            },
            redemption_id: "red-1".to_string(),
            managed: Some(true),
        })
    }

    #[tokio::test]
    async fn enqueue_increments_version_and_returns_patch() {
        let executor = setup_executor().await;
        let patches = executor
            .execute("b-1", "UTC", &[enqueue_command()])
            .await
            .expect("execute");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].version, 1);
        assert_eq!(patches[0].kind_str(), "queue.enqueued");

        let row: (i64,) =
            sqlx::query_as("SELECT current_version FROM state_index WHERE broadcaster_id = 'b-1'")
                .fetch_one(executor.database.pool())
                .await
                .expect("state index");
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn redemption_update_generates_patch() {
        let executor = setup_executor().await;
        let command = Command::RedemptionUpdate(RedemptionUpdateCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: Utc::now(),
            source: CommandSource::Policy,
            redemption_id: "red-1".to_string(),
            mode: RedemptionUpdateMode::Consume,
            applicable: true,
            result: CommandResult::Skipped,
            error: None,
        });
        let patches = executor
            .execute("b-1", "UTC", &[command])
            .await
            .expect("execute");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].kind_str(), "redemption.updated");
    }

    #[tokio::test]
    async fn invalid_timezone_is_reported() {
        let executor = setup_executor().await;
        let err = executor
            .execute("b-1", "Invalid/Zone", &[enqueue_command()])
            .await
            .unwrap_err();
        assert!(matches!(err, CommandExecutorError::InvalidTimezone(_)));
    }
}
