use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
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

    let prioritize_low_counts = profile.settings.prioritize_low_counts;
    let queue_rows = match scope {
        StateScope::Session => {
            queue_repo
                .list_active_with_counts(broadcaster_id, &snapshot_day, prioritize_low_counts)
                .await?
        }
        StateScope::Since(since) => {
            queue_repo
                .list_active_with_counts_since(
                    broadcaster_id,
                    &snapshot_day,
                    since,
                    prioritize_low_counts,
                )
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

    let day_start = day_start_utc(&snapshot_day, &profile.timezone)?;
    let completed_since = match scope {
        StateScope::Session => day_start,
        StateScope::Since(since) => since,
    };
    let completed = queue_repo
        .list_completed_since(broadcaster_id, completed_since)
        .await?;

    Ok(StateSnapshot {
        version,
        queue,
        completed,
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

fn day_start_utc(day: &str, timezone: &str) -> Result<DateTime<Utc>, StateError> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| StateError::InvalidTimezone(timezone.to_string()))?;
    let naive = NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|err| StateError::Unexpected(err.to_string()))?;
    let naive_dt = naive
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| StateError::Unexpected("invalid day start".to_string()))?;
    let local = tz
        .from_local_datetime(&naive_dt)
        .single()
        .ok_or_else(|| StateError::Unexpected("ambiguous day start".to_string()))?;
    Ok(local.with_timezone(&Utc))
}
