use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use metrics::{counter, histogram};
use serde_json::json;
use sqlx::Error as SqlxError;
use thiserror::Error;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, warn};
use twi_overlay_storage::Database;

use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload, TapHub};

const TTL_HOURS: i64 = 72;
const BATCH_LIMIT: i64 = 1000;
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Background worker responsible for TTL deletion and WAL checkpoints.
#[derive(Clone)]
pub struct MaintenanceWorker {
    database: Database,
    tap: TapHub,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    interval: Duration,
}

impl MaintenanceWorker {
    /// Creates a worker with default clock and cadence.
    pub fn new(database: Database, tap: TapHub) -> Self {
        Self {
            database,
            tap,
            clock: Arc::new(Utc::now),
            interval: DEFAULT_INTERVAL,
        }
    }

    /// Overrides the clock used for determining TTL thresholds.
    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Runs the worker loop in the background.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_loop().await;
        })
    }

    async fn run_loop(self) {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = self.run_once().await {
                error!(stage = "storage", error = %err, "maintenance run failed");
            }
        }
    }

    /// Executes one maintenance cycle (TTL + checkpoint).
    pub async fn run_once(&self) -> Result<(), MaintenanceError> {
        let now = (self.clock)();
        let threshold = now - ChronoDuration::hours(TTL_HOURS);

        let (event_deleted, event_busy) = self
            .delete_expired_rows("event_raw", threshold, |repo_threshold| async move {
                self.database
                    .event_raw()
                    .delete_older_than_batch(repo_threshold, BATCH_LIMIT)
                    .await
            })
            .await?;

        info!(
            stage = "storage",
            table = "event_raw",
            deleted = event_deleted,
            busy = event_busy,
            threshold = %threshold.to_rfc3339(),
            "event_raw TTL sweep completed"
        );
        self.publish_storage_event(
            "ttl.event_raw",
            json!({
                "table": "event_raw",
                "deleted": event_deleted,
                "threshold": threshold.to_rfc3339(),
                "busy": event_busy,
            }),
        );

        let (command_deleted, command_busy) = self
            .delete_expired_rows("command_log", threshold, |repo_threshold| async move {
                self.database
                    .command_log()
                    .delete_older_than_batch(repo_threshold, BATCH_LIMIT)
                    .await
            })
            .await?;

        info!(
            stage = "storage",
            table = "command_log",
            deleted = command_deleted,
            busy = command_busy,
            threshold = %threshold.to_rfc3339(),
            "command_log TTL sweep completed"
        );
        self.publish_storage_event(
            "ttl.command_log",
            json!({
                "table": "command_log",
                "deleted": command_deleted,
                "threshold": threshold.to_rfc3339(),
                "busy": command_busy,
            }),
        );

        self.run_checkpoint().await?;

        Ok(())
    }

    async fn delete_expired_rows<Fut>(
        &self,
        table: &'static str,
        threshold: DateTime<Utc>,
        mut delete_fn: impl FnMut(DateTime<Utc>) -> Fut,
    ) -> Result<(u64, bool), MaintenanceError>
    where
        Fut: std::future::Future<Output = Result<u64, SqlxError>>,
    {
        let mut total_deleted = 0u64;
        let mut busy = false;

        loop {
            match delete_fn(threshold).await {
                Ok(0) => break,
                Ok(batch_deleted) => {
                    total_deleted += batch_deleted;
                    counter!("db_ttl_deleted_total", "table" => table).increment(batch_deleted);
                }
                Err(err) => {
                    if is_sqlite_busy(&err) {
                        busy = true;
                        counter!("db_busy_total", "op" => "ttl").increment(1);
                        warn!(stage = "storage", %table, error = %err, "ttl delete hit busy timeout");
                        break;
                    }

                    return Err(MaintenanceError::TtlDelete { table, source: err });
                }
            }
        }

        Ok((total_deleted, busy))
    }

    async fn run_checkpoint(&self) -> Result<(), MaintenanceError> {
        let start = std::time::Instant::now();
        let checkpoint_result = self.database.wal_checkpoint_truncate().await;

        match checkpoint_result {
            Ok(stats) => {
                let duration = start.elapsed().as_secs_f64();
                histogram!("db_checkpoint_seconds").record(duration);
                let busy = stats.busy_frames > 0;
                if busy {
                    counter!("db_busy_total", "op" => "checkpoint").increment(1);
                    warn!(
                        stage = "storage",
                        busy_frames = stats.busy_frames,
                        log_frames = stats.log_frames,
                        checkpointed_frames = stats.checkpointed_frames,
                        duration_secs = duration,
                        "WAL checkpoint completed with busy frames"
                    );
                } else {
                    info!(
                        stage = "storage",
                        log_frames = stats.log_frames,
                        checkpointed_frames = stats.checkpointed_frames,
                        duration_secs = duration,
                        "WAL checkpoint completed"
                    );
                }

                self.publish_storage_event(
                    "wal.checkpoint",
                    json!({
                        "busy_frames": stats.busy_frames,
                        "log_frames": stats.log_frames,
                        "checkpointed_frames": stats.checkpointed_frames,
                        "busy": busy,
                        "duration_secs": duration,
                    }),
                );
            }
            Err(err) => {
                if is_sqlite_busy(&err) {
                    counter!("db_busy_total", "op" => "checkpoint").increment(1);
                    warn!(stage = "storage", error = %err, "WAL checkpoint hit busy timeout");
                    self.publish_storage_event(
                        "wal.checkpoint",
                        json!({
                            "busy": true,
                            "error": "database busy",
                        }),
                    );
                    return Ok(());
                }

                return Err(MaintenanceError::Checkpoint { source: err });
            }
        }

        Ok(())
    }

    fn publish_storage_event(&self, message: &str, payload: serde_json::Value) {
        let event = StageEvent {
            ts: (self.clock)(),
            stage: StageKind::Storage,
            trace_id: None,
            op_id: None,
            version: None,
            broadcaster_id: None,
            meta: StageMetadata {
                message: Some(message.to_string()),
                ..StageMetadata::default()
            },
            r#in: StagePayload::default(),
            out: StagePayload {
                redacted: false,
                payload,
                truncated: None,
            },
        };

        self.tap.publish(event);
    }
}

#[derive(Debug, Error)]
pub enum MaintenanceError {
    #[error("failed to delete expired rows from {table}")]
    TtlDelete {
        table: &'static str,
        #[source]
        source: SqlxError,
    },
    #[error("failed to run WAL checkpoint")]
    Checkpoint {
        #[source]
        source: SqlxError,
    },
}

fn is_sqlite_busy(err: &SqlxError) -> bool {
    match err {
        SqlxError::Database(db_err) => matches!(db_err.code().as_deref(), Some("5") | Some("6")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    use crate::telemetry;
    use tokio::time::timeout;

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
    async fn run_once_deletes_old_rows_and_emits_events() {
        telemetry::init_metrics().expect("metrics");
        let db = setup_db().await;
        let now = Utc::now();

        let event_repo = db.event_raw();
        let old_event = twi_overlay_storage::NewEventRaw {
            id: Cow::Borrowed("evt-old"),
            broadcaster_id: Cow::Borrowed("b-1"),
            msg_id: Cow::Borrowed("msg-old"),
            event_type: Cow::Borrowed("test.event"),
            payload_json: Cow::Borrowed("{}"),
            event_at: now - ChronoDuration::hours(80),
            received_at: now - ChronoDuration::hours(80),
            source: "webhook",
        };
        let new_event = twi_overlay_storage::NewEventRaw {
            id: Cow::Borrowed("evt-new"),
            broadcaster_id: Cow::Borrowed("b-1"),
            msg_id: Cow::Borrowed("msg-new"),
            event_type: Cow::Borrowed("test.event"),
            payload_json: Cow::Borrowed("{}"),
            event_at: now,
            received_at: now,
            source: "webhook",
        };
        event_repo
            .insert(old_event)
            .await
            .expect("insert old event");
        event_repo
            .insert(new_event)
            .await
            .expect("insert new event");

        let command_repo = db.command_log();
        for (idx, created_at) in [now - ChronoDuration::hours(80), now]
            .into_iter()
            .enumerate()
        {
            let mut tx = command_repo.begin().await.expect("begin");
            command_repo
                .append(
                    &mut tx,
                    twi_overlay_storage::NewCommandLog {
                        broadcaster_id: "b-1",
                        op_id: Some(&format!("op-{idx}")),
                        command_type: "queue.enqueue",
                        payload_json: "{}",
                        created_at,
                    },
                )
                .await
                .expect("append");
            tx.commit().await.expect("commit");
        }

        let tap = TapHub::new();
        let mut tap_rx = tap.subscribe();
        let clock = Arc::new(move || now);
        let worker = MaintenanceWorker::new(db.clone(), tap.clone()).with_clock(clock.clone());
        worker.run_once().await.expect("run_once");

        let remaining_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_raw")
            .fetch_one(db.pool())
            .await
            .expect("count events");
        assert_eq!(remaining_events.0, 1);

        let remaining_commands: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM command_log")
            .fetch_one(db.pool())
            .await
            .expect("count commands");
        assert_eq!(remaining_commands.0, 1);

        let first = timeout(Duration::from_secs(1), tap_rx.recv())
            .await
            .expect("tap ttl event")
            .expect("ttl event");
        assert_eq!(first.stage, StageKind::Storage);
        assert_eq!(first.meta.message.as_deref(), Some("ttl.event_raw"));

        let second = timeout(Duration::from_secs(1), tap_rx.recv())
            .await
            .expect("tap ttl command")
            .expect("ttl command");
        assert_eq!(second.meta.message.as_deref(), Some("ttl.command_log"));

        let third = timeout(Duration::from_secs(1), tap_rx.recv())
            .await
            .expect("tap checkpoint")
            .expect("checkpoint event");
        assert_eq!(third.meta.message.as_deref(), Some("wal.checkpoint"));

        // Metrics exporter is initialised; individual counters are validated via integration tests.
    }
}
