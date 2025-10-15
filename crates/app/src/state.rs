use chrono::{DateTime, Utc};
use thiserror::Error;

use twi_overlay_core::types::{StateSnapshot, UserCounter};
use twi_overlay_storage::{
    BroadcasterSettings, DailyCounterError, Database, QueueError, StateIndexError,
};

use crate::command::{compute_local_day, CommandExecutorError};

#[derive(Debug, Clone, Copy)]
pub enum StateScope {
    Session,
    Since(DateTime<Utc>),
}

pub async fn build_state_snapshot(
    database: &Database,
    broadcaster_id: &str,
    profile: &BroadcasterSettings,
    now: DateTime<Utc>,
    scope: StateScope,
) -> Result<StateSnapshot, StateError> {
    let version = database
        .state_index()
        .fetch_current_version(broadcaster_id)
        .await?;

    let queue_repo = database.queue();
    let counter_repo = database.daily_counters();

    let snapshot_day = match scope {
        StateScope::Session => compute_local_day(now, &profile.timezone)?,
        StateScope::Since(since) => compute_local_day(since, &profile.timezone)?,
    };

    let queue_rows = match scope {
        StateScope::Session => {
            queue_repo
                .list_active_with_counts(broadcaster_id, &snapshot_day)
                .await?
        }
        StateScope::Since(since) => {
            queue_repo
                .list_active_with_counts_since(broadcaster_id, &snapshot_day, since)
                .await?
        }
    };
    let queue = queue_rows
        .into_iter()
        .map(|row| row.into_domain().0)
        .collect();

    let counters_rows = match scope {
        StateScope::Session => {
            counter_repo
                .list_for_day(broadcaster_id, &snapshot_day)
                .await?
        }
        StateScope::Since(since) => {
            counter_repo
                .list_updated_since(broadcaster_id, &snapshot_day, since)
                .await?
        }
    };
    let counters = counters_rows
        .into_iter()
        .map(|row| UserCounter {
            user_id: row.user_id,
            count: row.count as u32,
        })
        .collect();

    Ok(StateSnapshot {
        version,
        queue,
        counters_today: counters,
        settings: profile.settings.clone(),
    })
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to load state index: {0}")]
    StateIndex(#[from] StateIndexError),
    #[error("failed to load queue entries: {0}")]
    Queue(#[from] QueueError),
    #[error("failed to load counters: {0}")]
    Counter(#[from] DailyCounterError),
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
    #[error("unexpected error: {0}")]
    Unexpected(String),
}

impl From<CommandExecutorError> for StateError {
    fn from(err: CommandExecutorError) -> Self {
        match err {
            CommandExecutorError::InvalidTimezone(tz) => Self::InvalidTimezone(tz),
            other => Self::Unexpected(other.to_string()),
        }
    }
}
