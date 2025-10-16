use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use metrics::counter;
use serde_json::{to_string, to_value, Value};
use sqlx::{Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;

use twi_overlay_core::projector::Projector;
use twi_overlay_core::types::{
    Command, EnqueueCommand, Patch, QueueCompleteCommand, QueueEntry, QueueEntryStatus,
    QueueRemovalReason, QueueRemoveCommand, RedemptionUpdateCommand, Settings,
    SettingsUpdateCommand,
};
use twi_overlay_storage::{
    BroadcasterRepository, CommandLogError, DailyCounterError, DailyCounterRepository, Database,
    NewCommandLog, NewDailyCounter, NewQueueEntry, QueueError, QueueRepository, SettingsError,
    SettingsUpdateError,
};

use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload, TapHub};

#[derive(Debug, Clone)]
pub struct CommandApplication {
    pub version: u64,
    pub patches: Vec<Patch>,
    pub result: CommandApplyResult,
    pub duplicate: bool,
}

#[derive(Debug, Clone)]
pub enum CommandApplyResult {
    None,
    QueueMutation {
        entry_id: String,
        mode: QueueMutationMode,
        user_today_count: u32,
    },
    SettingsUpdated {
        applied: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMutationMode {
    Complete,
    Undo,
}

impl QueueMutationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueMutationMode::Complete => "COMPLETE",
            QueueMutationMode::Undo => "UNDO",
        }
    }
}

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

    #[allow(clippy::too_many_arguments)]
    async fn apply_command(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        timezone: &str,
        command: &Command,
        queue_repo: &QueueRepository,
        counter_repo: &DailyCounterRepository,
        broadcaster_repo: &BroadcasterRepository,
    ) -> Result<CommandApplication, CommandExecutorError> {
        match command {
            Command::Enqueue(enqueue) => {
                self.handle_enqueue(
                    tx,
                    broadcaster_id,
                    timezone,
                    enqueue,
                    queue_repo,
                    counter_repo,
                )
                .await
            }
            Command::RedemptionUpdate(update) => {
                self.handle_redemption_update(tx, broadcaster_id, update)
                    .await
            }
            Command::QueueComplete(complete) => {
                self.handle_queue_complete(
                    tx,
                    broadcaster_id,
                    timezone,
                    complete,
                    queue_repo,
                    counter_repo,
                )
                .await
            }
            Command::QueueRemove(remove) => {
                self.handle_queue_remove(
                    tx,
                    broadcaster_id,
                    timezone,
                    remove,
                    queue_repo,
                    counter_repo,
                )
                .await
            }
            Command::SettingsUpdate(update) => {
                self.handle_settings_update(tx, broadcaster_id, update, broadcaster_repo)
                    .await
            }
        }
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
        let broadcaster_repo = self.database.broadcasters();
        let mut patches = Vec::with_capacity(commands.len());

        for command in commands {
            let application = self
                .apply_command(
                    &mut tx,
                    broadcaster_id,
                    timezone,
                    command,
                    &queue_repo,
                    &counter_repo,
                    &broadcaster_repo,
                )
                .await?;
            patches.extend(application.patches);
        }

        tx.commit().await?;
        Ok(patches)
    }

    /// Executes a single admin command, returning its application details.
    pub async fn execute_admin_command(
        &self,
        broadcaster_id: &str,
        timezone: &str,
        command: Command,
    ) -> Result<CommandApplication, CommandExecutorError> {
        match command {
            Command::QueueComplete(_) | Command::QueueRemove(_) | Command::SettingsUpdate(_) => {}
            _ => {
                return Err(CommandExecutorError::UnsupportedCommand(
                    command.metric_kind(),
                ))
            }
        }

        let command_log_repo = self.database.command_log();
        let mut tx = command_log_repo.begin().await?;
        let queue_repo = self.database.queue();
        let counter_repo = self.database.daily_counters();
        let broadcaster_repo = self.database.broadcasters();

        let application = self
            .apply_command(
                &mut tx,
                broadcaster_id,
                timezone,
                &command,
                &queue_repo,
                &counter_repo,
                &broadcaster_repo,
            )
            .await?;

        tx.commit().await?;
        Ok(application)
    }

    async fn handle_enqueue(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        timezone: &str,
        command: &EnqueueCommand,
        queue_repo: &QueueRepository,
        counter_repo: &DailyCounterRepository,
    ) -> Result<CommandApplication, CommandExecutorError> {
        let serialized = to_string(command)?;
        let inserted_at = self.now();
        let version = self
            .append_command(
                tx,
                broadcaster_id,
                None,
                "enqueue",
                &serialized,
                inserted_at,
            )
            .await?;

        let command_enum = Command::Enqueue(command.clone());
        self.emit_command_event(broadcaster_id, version, "enqueue", &command_enum, None);

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
        self.emit_projector_event(broadcaster_id, version, &patch, &command_enum, None);
        counter!("projector_patches_total", "type" => patch.kind_str()).increment(1);

        Ok(CommandApplication {
            version,
            patches: vec![patch],
            result: CommandApplyResult::None,
            duplicate: false,
        })
    }

    async fn handle_redemption_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        command: &RedemptionUpdateCommand,
    ) -> Result<CommandApplication, CommandExecutorError> {
        let serialized = to_string(command)?;
        let inserted_at = self.now();
        let version = self
            .append_command(
                tx,
                broadcaster_id,
                None,
                "redemption.update",
                &serialized,
                inserted_at,
            )
            .await?;

        let command_enum = Command::RedemptionUpdate(command.clone());
        self.emit_command_event(
            broadcaster_id,
            version,
            "redemption.update",
            &command_enum,
            None,
        );

        let patch = Projector::redemption_updated(version, command.issued_at, command);
        self.emit_projector_event(broadcaster_id, version, &patch, &command_enum, None);
        counter!("projector_patches_total", "type" => patch.kind_str()).increment(1);

        Ok(CommandApplication {
            version,
            patches: vec![patch],
            result: CommandApplyResult::None,
            duplicate: false,
        })
    }

    async fn handle_queue_complete(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        timezone: &str,
        command: &QueueCompleteCommand,
        queue_repo: &QueueRepository,
        counter_repo: &DailyCounterRepository,
    ) -> Result<CommandApplication, CommandExecutorError> {
        let Some(entry) = queue_repo
            .find_entry_for_update(tx, broadcaster_id, &command.entry_id)
            .await?
        else {
            return Err(QueueError::NotFound.into());
        };

        let serialized = to_string(command)?;
        let existing_version = self
            .ensure_unique_op_id(
                tx,
                broadcaster_id,
                &command.op_id,
                "queue.complete",
                &serialized,
            )
            .await?;

        let day = compute_local_day(entry.enqueued_at, timezone)?;
        let user_today_count = counter_repo
            .fetch_value(tx, &day, broadcaster_id, &entry.user_id)
            .await?
            .unwrap_or(0);

        if let Some(version) = existing_version {
            return Ok(CommandApplication {
                version,
                patches: Vec::new(),
                result: CommandApplyResult::QueueMutation {
                    entry_id: command.entry_id.clone(),
                    mode: QueueMutationMode::Complete,
                    user_today_count,
                },
                duplicate: true,
            });
        }

        let updated_at = self.now();
        queue_repo
            .mark_completed(tx, broadcaster_id, &command.entry_id, updated_at)
            .await?;

        let version = self
            .append_command(
                tx,
                broadcaster_id,
                Some(&command.op_id),
                "queue.complete",
                &serialized,
                updated_at,
            )
            .await?;

        let command_enum = Command::QueueComplete(command.clone());
        self.emit_command_event(
            broadcaster_id,
            version,
            "queue.complete",
            &command_enum,
            Some(&command.op_id),
        );

        let patch = Projector::queue_completed(version, command.issued_at, &command.entry_id);
        self.emit_projector_event(
            broadcaster_id,
            version,
            &patch,
            &command_enum,
            Some(&command.op_id),
        );
        counter!("projector_patches_total", "type" => patch.kind_str()).increment(1);

        Ok(CommandApplication {
            version,
            patches: vec![patch],
            result: CommandApplyResult::QueueMutation {
                entry_id: command.entry_id.clone(),
                mode: QueueMutationMode::Complete,
                user_today_count,
            },
            duplicate: false,
        })
    }

    async fn handle_queue_remove(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        timezone: &str,
        command: &QueueRemoveCommand,
        queue_repo: &QueueRepository,
        counter_repo: &DailyCounterRepository,
    ) -> Result<CommandApplication, CommandExecutorError> {
        let Some(entry) = queue_repo
            .find_entry_for_update(tx, broadcaster_id, &command.entry_id)
            .await?
        else {
            return Err(QueueError::NotFound.into());
        };

        let serialized = to_string(command)?;
        let existing_version = self
            .ensure_unique_op_id(
                tx,
                broadcaster_id,
                &command.op_id,
                "queue.remove",
                &serialized,
            )
            .await?;

        let day = compute_local_day(entry.enqueued_at, timezone)?;
        let mode = queue_mode_from_reason(command.reason);

        let user_today_count = counter_repo
            .fetch_value(tx, &day, broadcaster_id, &entry.user_id)
            .await?
            .unwrap_or(0);

        if let Some(version) = existing_version {
            return Ok(CommandApplication {
                version,
                patches: Vec::new(),
                result: CommandApplyResult::QueueMutation {
                    entry_id: command.entry_id.clone(),
                    mode,
                    user_today_count,
                },
                duplicate: true,
            });
        }

        let updated_at = self.now();
        queue_repo
            .mark_removed(
                tx,
                broadcaster_id,
                &command.entry_id,
                command.reason,
                updated_at,
            )
            .await?;

        let new_count = if matches!(command.reason, QueueRemovalReason::Undo) {
            counter_repo
                .decrement(tx, &day, broadcaster_id, &entry.user_id, updated_at)
                .await?
                .unwrap_or(0)
        } else {
            user_today_count
        };

        let version = self
            .append_command(
                tx,
                broadcaster_id,
                Some(&command.op_id),
                "queue.remove",
                &serialized,
                updated_at,
            )
            .await?;

        let command_enum = Command::QueueRemove(command.clone());
        self.emit_command_event(
            broadcaster_id,
            version,
            "queue.remove",
            &command_enum,
            Some(&command.op_id),
        );

        let mut patches = Vec::new();
        let queue_patch = Projector::queue_removed(
            version,
            command.issued_at,
            &command.entry_id,
            command.reason,
            new_count,
        );
        self.emit_projector_event(
            broadcaster_id,
            version,
            &queue_patch,
            &command_enum,
            Some(&command.op_id),
        );
        counter!("projector_patches_total", "type" => queue_patch.kind_str()).increment(1);
        patches.push(queue_patch);

        if matches!(command.reason, QueueRemovalReason::Undo) {
            let counter_patch =
                Projector::counter_updated(version, command.issued_at, &entry.user_id, new_count);
            self.emit_projector_event(
                broadcaster_id,
                version,
                &counter_patch,
                &command_enum,
                Some(&command.op_id),
            );
            counter!("projector_patches_total", "type" => counter_patch.kind_str()).increment(1);
            patches.push(counter_patch);
        }

        Ok(CommandApplication {
            version,
            patches,
            result: CommandApplyResult::QueueMutation {
                entry_id: command.entry_id.clone(),
                mode,
                user_today_count: new_count,
            },
            duplicate: false,
        })
    }

    async fn handle_settings_update(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        command: &SettingsUpdateCommand,
        broadcaster_repo: &BroadcasterRepository,
    ) -> Result<CommandApplication, CommandExecutorError> {
        let serialized = to_string(command)?;
        let existing_version = self
            .ensure_unique_op_id(
                tx,
                broadcaster_id,
                &command.op_id,
                "settings.update",
                &serialized,
            )
            .await?;

        if let Some(version) = existing_version {
            return Ok(CommandApplication {
                version,
                patches: Vec::new(),
                result: CommandApplyResult::SettingsUpdated { applied: true },
                duplicate: true,
            });
        }

        let profile = broadcaster_repo.fetch_settings(broadcaster_id).await?;
        let merged = merge_settings_patch(&profile.settings, &command.patch)?;
        let updated_at = self.now();
        broadcaster_repo
            .update_settings(tx, broadcaster_id, &merged, updated_at)
            .await?;

        let version = self
            .append_command(
                tx,
                broadcaster_id,
                Some(&command.op_id),
                "settings.update",
                &serialized,
                updated_at,
            )
            .await?;

        let command_enum = Command::SettingsUpdate(command.clone());
        self.emit_command_event(
            broadcaster_id,
            version,
            "settings.update",
            &command_enum,
            Some(&command.op_id),
        );

        let patch = Projector::settings_updated(version, command.issued_at, &command.patch);
        self.emit_projector_event(
            broadcaster_id,
            version,
            &patch,
            &command_enum,
            Some(&command.op_id),
        );
        counter!("projector_patches_total", "type" => patch.kind_str()).increment(1);

        Ok(CommandApplication {
            version,
            patches: vec![patch],
            result: CommandApplyResult::SettingsUpdated { applied: true },
            duplicate: false,
        })
    }

    async fn ensure_unique_op_id(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        op_id: &str,
        command_type: &str,
        payload_json: &str,
    ) -> Result<Option<u64>, CommandExecutorError> {
        let Some(existing) = self
            .database
            .command_log()
            .find_by_op_id(tx, broadcaster_id, op_id)
            .await?
        else {
            return Ok(None);
        };

        if existing.command_type != command_type {
            return Err(CommandExecutorError::OpConflict {
                op_id: op_id.to_string(),
            });
        }

        let expected_payload = normalize_idempotent_payload(payload_json)?;
        let existing_payload = normalize_idempotent_payload(&existing.payload_json)?;

        if expected_payload != existing_payload {
            return Err(CommandExecutorError::OpConflict {
                op_id: op_id.to_string(),
            });
        }

        Ok(Some(existing.version))
    }

    async fn append_command(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        broadcaster_id: &str,
        op_id: Option<&str>,
        command_type: &str,
        payload_json: &str,
        created_at: DateTime<Utc>,
    ) -> Result<u64, CommandExecutorError> {
        let record = NewCommandLog {
            broadcaster_id,
            op_id,
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
        op_id: Option<&str>,
    ) {
        let payload = command.redacted();
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Command,
            trace_id: None,
            op_id: op_id.map(str::to_string),
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
        op_id: Option<&str>,
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
            op_id: op_id.map(str::to_string),
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

fn queue_mode_from_reason(reason: QueueRemovalReason) -> QueueMutationMode {
    match reason {
        QueueRemovalReason::Undo => QueueMutationMode::Undo,
        QueueRemovalReason::ExplicitRemove | QueueRemovalReason::StreamStartClear => {
            QueueMutationMode::Undo
        }
    }
}

fn merge_settings_patch(
    current: &Settings,
    patch: &Value,
) -> Result<Settings, CommandExecutorError> {
    if !patch.is_object() {
        return Err(CommandExecutorError::InvalidSettingsPatch(
            "patch must be a JSON object".to_string(),
        ));
    }

    let mut merged = to_value(current)?;
    merge_value(&mut merged, patch);
    let settings: Settings = serde_json::from_value(merged)?;
    Ok(settings)
}

fn merge_value(target: &mut Value, patch: &Value) {
    if let Value::Object(patch_map) = patch {
        if let Value::Object(target_map) = target {
            for (key, value) in patch_map {
                if value.is_null() {
                    target_map.remove(key);
                    continue;
                }

                match target_map.get_mut(key.as_str()) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        target_map.insert(key.clone(), value.clone());
                    }
                }
            }
            return;
        }
    }

    *target = patch.clone();
}

fn normalize_idempotent_payload(payload_json: &str) -> Result<Value, CommandExecutorError> {
    let mut value: Value = serde_json::from_str(payload_json)?;

    if let Value::Object(map) = &mut value {
        map.remove("issued_at");
    }

    Ok(value)
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
    #[error("settings error: {0}")]
    Settings(#[from] SettingsError),
    #[error("settings update error: {0}")]
    SettingsUpdate(#[from] SettingsUpdateError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
    #[error("op_id conflict for command: {op_id}")]
    OpConflict { op_id: String },
    #[error("invalid settings patch: {0}")]
    InvalidSettingsPatch(String),
    #[error("unsupported command type: {0}")]
    UnsupportedCommand(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tap::TapHub;
    use serde_json::json;
    use twi_overlay_core::types::{
        CommandResult, CommandSource, NormalizedReward, NormalizedUser, RedemptionUpdateMode,
    };
    use uuid::Uuid;

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

    #[tokio::test]
    async fn queue_complete_marks_entry_completed_and_emits_patch() {
        let executor = setup_executor().await;
        let enqueue_patch = executor
            .execute("b-1", "UTC", &[enqueue_command()])
            .await
            .expect("enqueue");
        let entry_id = enqueue_patch[0].data["entry"]["id"]
            .as_str()
            .expect("entry id")
            .to_string();

        let command = Command::QueueComplete(QueueCompleteCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: Utc::now(),
            source: CommandSource::Admin,
            entry_id: entry_id.clone(),
            op_id: Uuid::new_v4().to_string(),
        });

        let result = executor
            .execute_admin_command("b-1", "UTC", command)
            .await
            .expect("queue complete");

        assert!(!result.duplicate);
        assert_eq!(result.patches.len(), 1);
        assert_eq!(result.patches[0].kind_str(), "queue.completed");

        match result.result {
            CommandApplyResult::QueueMutation {
                entry_id: ref reported_entry,
                mode,
                ..
            } => {
                assert_eq!(reported_entry, &entry_id);
                assert_eq!(mode, QueueMutationMode::Complete);
            }
            other => panic!("unexpected result {other:?}"),
        }

        let status: (String,) = sqlx::query_as("SELECT status FROM queue_entries WHERE id = ?")
            .bind(&entry_id)
            .fetch_one(executor.database.pool())
            .await
            .expect("fetch entry status");
        assert_eq!(status.0, QueueEntryStatus::Completed.as_str());
    }

    #[tokio::test]
    async fn queue_remove_undo_decrements_counter() {
        let executor = setup_executor().await;
        let enqueue_patch = executor
            .execute("b-1", "UTC", &[enqueue_command()])
            .await
            .expect("enqueue");
        let entry_id = enqueue_patch[0].data["entry"]["id"]
            .as_str()
            .expect("entry id")
            .to_string();

        let op_id = Uuid::new_v4().to_string();
        let command = Command::QueueRemove(QueueRemoveCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: Utc::now(),
            source: CommandSource::Admin,
            entry_id: entry_id.clone(),
            reason: QueueRemovalReason::Undo,
            op_id: op_id.clone(),
        });

        let result = executor
            .execute_admin_command("b-1", "UTC", command)
            .await
            .expect("queue remove");

        assert_eq!(result.patches.len(), 2);
        assert!(result
            .patches
            .iter()
            .any(|patch| patch.kind_str() == "queue.removed"));
        assert!(result
            .patches
            .iter()
            .any(|patch| patch.kind_str() == "counter.updated"));
        assert!(!result.duplicate);

        match result.result {
            CommandApplyResult::QueueMutation {
                entry_id: ref reported_entry,
                mode,
                user_today_count,
            } => {
                assert_eq!(reported_entry, &entry_id);
                assert_eq!(mode, QueueMutationMode::Undo);
                assert_eq!(user_today_count, 0);
            }
            other => panic!("unexpected result {other:?}"),
        }

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, status_reason FROM queue_entries WHERE id = ?")
                .bind(&entry_id)
                .fetch_one(executor.database.pool())
                .await
                .expect("fetch entry");
        assert_eq!(row.0, QueueEntryStatus::Removed.as_str());
        assert_eq!(row.1.as_deref(), Some(QueueRemovalReason::Undo.as_str()));

        let count: Option<i64> =
            sqlx::query_scalar("SELECT count FROM daily_counters WHERE user_id = 'u-1'")
                .fetch_optional(executor.database.pool())
                .await
                .expect("fetch counter");
        assert_eq!(count.unwrap_or(0), 0);

        let duplicate = executor
            .execute_admin_command(
                "b-1",
                "UTC",
                Command::QueueRemove(QueueRemoveCommand {
                    broadcaster_id: "b-1".to_string(),
                    issued_at: Utc::now(),
                    source: CommandSource::Admin,
                    entry_id: entry_id.clone(),
                    reason: QueueRemovalReason::Undo,
                    op_id,
                }),
            )
            .await
            .expect("duplicate remove");
        assert!(duplicate.duplicate);
        assert!(duplicate.patches.is_empty());
    }

    #[tokio::test]
    async fn settings_update_applies_patch_and_is_idempotent() {
        let executor = setup_executor().await;
        let op_id = Uuid::new_v4().to_string();
        let patch_value = json!({ "group_size": 3 });
        let command = Command::SettingsUpdate(SettingsUpdateCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: Utc::now(),
            source: CommandSource::Admin,
            patch: patch_value.clone(),
            op_id: op_id.clone(),
        });

        let result = executor
            .execute_admin_command("b-1", "UTC", command)
            .await
            .expect("settings update");

        assert_eq!(result.patches.len(), 1);
        assert_eq!(result.patches[0].kind_str(), "settings.updated");

        let settings_json: (String,) =
            sqlx::query_as("SELECT settings_json FROM broadcasters WHERE id = 'b-1'")
                .fetch_one(executor.database.pool())
                .await
                .expect("settings json");
        let settings: Settings = serde_json::from_str(&settings_json.0).expect("decode settings");
        assert_eq!(settings.group_size, 3);

        let duplicate = executor
            .execute_admin_command(
                "b-1",
                "UTC",
                Command::SettingsUpdate(SettingsUpdateCommand {
                    broadcaster_id: "b-1".to_string(),
                    issued_at: Utc::now(),
                    source: CommandSource::Admin,
                    patch: patch_value,
                    op_id,
                }),
            )
            .await
            .expect("duplicate settings update");
        assert!(duplicate.duplicate);
        assert!(duplicate.patches.is_empty());
    }
}
