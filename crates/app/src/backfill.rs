use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use metrics::counter;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::mpsc,
    time::{interval, MissedTickBehavior},
};
use tracing::{error, info, warn};

use twi_overlay_core::policy::{PolicyEngine, PolicyOutcome};
use twi_overlay_core::types::{Command, NormalizedEvent, NormalizedReward, NormalizedUser, Patch};
use twi_overlay_storage::{
    Database, HelixBackfillCheckpoint, HelixBackfillError, HelixBackfillStatus, OauthFailure,
    OauthLink, OauthLinkError, QueueError, SettingsError,
};
use twi_overlay_twitch::{
    HelixClient, HelixError, HelixRedemption, HelixRedemptionStatus, ListRedemptionsParams,
};

use crate::command::{
    classify_helix_error, has_required_scopes, CommandExecutor, CommandExecutorError,
    ERR_OAUTH_EXPIRED, ERR_OAUTH_MISSING_SCOPE, ERR_OAUTH_NOT_LINKED, ERR_OAUTH_REAUTH,
};
use crate::problem::ProblemResponse;
use crate::router::AppState;
use crate::sse::{SseError, SseHub};
use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload, TapHub};

#[derive(Clone)]
pub struct BackfillService {
    sender: mpsc::Sender<BackfillCommand>,
}

impl BackfillService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: Database,
        tap: TapHub,
        policy: Arc<PolicyEngine>,
        command_executor: CommandExecutor,
        sse: SseHub,
        helix: HelixClient,
        clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
        interval: Duration,
        page_size: u32,
    ) -> (Self, BackfillWorker) {
        let (sender, receiver) = mpsc::channel(32);
        let worker = BackfillWorker {
            database,
            tap,
            policy,
            command_executor,
            sse,
            helix,
            clock,
            receiver,
            interval,
            page_size,
        };
        (Self { sender }, worker)
    }

    pub async fn trigger(
        &self,
        broadcaster_id: impl Into<String>,
    ) -> Result<(), BackfillTriggerError> {
        self.sender
            .send(BackfillCommand::Single(broadcaster_id.into()))
            .await
            .map_err(|_| BackfillTriggerError::ChannelClosed)
    }

    #[allow(dead_code)]
    pub async fn trigger_all(&self) -> Result<(), BackfillTriggerError> {
        self.sender
            .send(BackfillCommand::All)
            .await
            .map_err(|_| BackfillTriggerError::ChannelClosed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackfillTriggerError {
    #[error("backfill worker channel closed")]
    ChannelClosed,
}

enum BackfillCommand {
    #[allow(dead_code)]
    All,
    Single(String),
}

pub struct BackfillWorker {
    database: Database,
    tap: TapHub,
    policy: Arc<PolicyEngine>,
    command_executor: CommandExecutor,
    sse: SseHub,
    helix: HelixClient,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    receiver: mpsc::Receiver<BackfillCommand>,
    interval: Duration,
    page_size: u32,
}

impl BackfillWorker {
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(mut self) {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(err) = self.run_all().await {
                        error!(stage = "oauth", error = %err, "backfill periodic run failed");
                    }
                }
                Some(cmd) = self.receiver.recv() => {
                    match cmd {
                        BackfillCommand::All => {
                            if let Err(err) = self.run_all().await {
                                error!(stage = "oauth", error = %err, "backfill ad-hoc run failed");
                            }
                        }
                        BackfillCommand::Single(broadcaster_id) => {
                            if let Err(err) = self.run_single(&broadcaster_id).await {
                                error!(stage = "oauth", broadcaster = %broadcaster_id, error = %err, "backfill run for broadcaster failed");
                            }
                        }
                    }
                }
                else => break,
            }
        }
    }

    async fn run_all(&mut self) -> Result<(), BackfillError> {
        let now = self.now();
        let links = self
            .database
            .oauth_links()
            .list_active(now)
            .await
            .map_err(BackfillError::Oauth)?;

        if links.is_empty() {
            return Ok(());
        }

        info!(
            stage = "oauth",
            count = links.len(),
            "starting helix backfill sweep"
        );
        for link in links {
            let broadcaster_id = link.broadcaster_id.clone();
            if let Err(err) = self.process_link(link).await {
                error!(
                    stage = "oauth",
                    broadcaster = %broadcaster_id,
                    error = %err,
                    "helix backfill failed for broadcaster"
                );
            }
        }

        Ok(())
    }

    async fn run_single(&mut self, broadcaster_id: &str) -> Result<(), BackfillError> {
        let link = self
            .database
            .oauth_links()
            .fetch_by_broadcaster(broadcaster_id)
            .await
            .map_err(BackfillError::Oauth)?;

        let Some(link) = link else {
            warn!(stage = "oauth", broadcaster = %broadcaster_id, "backfill trigger ignored: oauth link missing");
            self.update_checkpoint_status(
                broadcaster_id,
                HelixBackfillStatus::Error,
                None,
                None,
                None,
                Some(ERR_OAUTH_NOT_LINKED.to_string()),
            )
            .await?;
            return Ok(());
        };

        self.process_link(link).await
    }

    async fn process_link(&mut self, link: OauthLink) -> Result<(), BackfillError> {
        let broadcaster_id = link.broadcaster_id.clone();
        let now = self.now();

        if link.requires_reauth {
            self.update_checkpoint_status(
                &broadcaster_id,
                HelixBackfillStatus::Error,
                None,
                None,
                None,
                Some(ERR_OAUTH_REAUTH.to_string()),
            )
            .await?;
            return Ok(());
        }

        if link.expires_at <= now {
            self.update_checkpoint_status(
                &broadcaster_id,
                HelixBackfillStatus::Error,
                None,
                None,
                None,
                Some(ERR_OAUTH_EXPIRED.to_string()),
            )
            .await?;
            return Ok(());
        }

        if !has_required_scopes(&link) {
            self.update_checkpoint_status(
                &broadcaster_id,
                HelixBackfillStatus::Error,
                None,
                None,
                None,
                Some(ERR_OAUTH_MISSING_SCOPE.to_string()),
            )
            .await?;
            return Ok(());
        }

        let profile = self
            .database
            .broadcasters()
            .fetch_settings(&broadcaster_id)
            .await
            .map_err(BackfillError::Settings)?;

        let target_rewards: HashSet<_> = profile
            .settings
            .policy
            .target_rewards
            .iter()
            .cloned()
            .collect();
        if target_rewards.is_empty() {
            self.update_checkpoint_status(
                &broadcaster_id,
                HelixBackfillStatus::Idle,
                None,
                None,
                None,
                Some("policy:disabled".to_string()),
            )
            .await?;
            return Ok(());
        }

        self.update_checkpoint_status(
            &broadcaster_id,
            HelixBackfillStatus::Running,
            None,
            None,
            None,
            None,
        )
        .await?;

        let mut after: Option<String> = None;
        let mut last_redemption_id: Option<String> = None;
        let mut last_seen_at: Option<DateTime<Utc>> = None;
        let timezone = profile.timezone;
        let settings = profile.settings;

        loop {
            let page = self
                .helix
                .list_redemptions(
                    &link.access_token,
                    &ListRedemptionsParams {
                        broadcaster_id: &broadcaster_id,
                        reward_id: None,
                        status: HelixRedemptionStatus::Unfulfilled,
                        after: after.as_deref(),
                        first: Some(self.page_size),
                    },
                )
                .await;

            let page = match page {
                Ok(page) => page,
                Err(err) => {
                    let (code, requires_reauth) = classify_helix_error(&err);
                    self.record_oauth_failure(&link, code, requires_reauth)
                        .await;
                    self.update_checkpoint_status(
                        &broadcaster_id,
                        HelixBackfillStatus::Error,
                        after,
                        last_redemption_id,
                        last_seen_at,
                        Some(code.to_string()),
                    )
                    .await?;
                    return Err(BackfillError::Helix(err));
                }
            };

            if page.data.is_empty() {
                self.update_checkpoint_status(
                    &broadcaster_id,
                    HelixBackfillStatus::Idle,
                    page.cursor.clone(),
                    last_redemption_id,
                    last_seen_at,
                    None,
                )
                .await?;
                break;
            }

            for redemption in page.data {
                if !target_rewards.contains(&redemption.reward.id) {
                    continue;
                }

                match self
                    .apply_redemption(&settings, &timezone, &broadcaster_id, &redemption)
                    .await
                {
                    RedemptionApply::Processed => {
                        last_redemption_id = Some(redemption.id.clone());
                        last_seen_at = Some(redemption.redeemed_at);
                        self.publish_backfill_event(&broadcaster_id, &redemption, "ok", None);
                    }
                    RedemptionApply::Duplicate => {
                        counter!("backfill_duplicates_total").increment(1);
                        self.publish_backfill_event(
                            &broadcaster_id,
                            &redemption,
                            "duplicate",
                            None,
                        );
                    }
                    RedemptionApply::Skipped(reason) => {
                        self.publish_backfill_event(
                            &broadcaster_id,
                            &redemption,
                            "skipped",
                            Some(reason.as_str()),
                        );
                    }
                    RedemptionApply::Failed(err_code) => {
                        self.publish_backfill_event(
                            &broadcaster_id,
                            &redemption,
                            "error",
                            Some(err_code),
                        );
                    }
                }
            }

            after = page.cursor;
            if after.is_none() {
                self.update_checkpoint_status(
                    &broadcaster_id,
                    HelixBackfillStatus::Idle,
                    None,
                    last_redemption_id,
                    last_seen_at,
                    None,
                )
                .await?;
                break;
            }
        }

        Ok(())
    }

    async fn apply_redemption(
        &self,
        settings: &twi_overlay_core::types::Settings,
        timezone: &str,
        broadcaster_id: &str,
        redemption: &HelixRedemption,
    ) -> RedemptionApply {
        let normalized = NormalizedEvent::RedemptionAdd {
            broadcaster_id: broadcaster_id.to_string(),
            occurred_at: redemption.redeemed_at,
            redemption_id: redemption.id.clone(),
            user: NormalizedUser {
                id: redemption.user_id.clone(),
                login: Some(redemption.user_login.clone()),
                display_name: Some(redemption.user_name.clone()),
            },
            reward: NormalizedReward {
                id: redemption.reward.id.clone(),
                title: Some(redemption.reward.title.clone()),
                cost: (redemption.reward.cost >= 0).then_some(redemption.reward.cost as u64),
            },
        };

        let issued_at = self.now();
        let outcome = self.policy.evaluate(settings, &normalized, issued_at);

        if outcome.commands.is_empty() {
            let reason = outcome
                .reason
                .clone()
                .unwrap_or_else(|| "policy:ignored".to_string());
            return RedemptionApply::Skipped(reason);
        }

        self.publish_policy_tap(broadcaster_id, &normalized, &outcome);

        match self
            .command_executor
            .execute(broadcaster_id, timezone, &outcome.commands)
            .await
        {
            Ok(patches) => {
                counter!("backfill_processed_total").increment(1);
                if let Err(err) = self.broadcast_patches(broadcaster_id, patches).await {
                    warn!(stage = "sse", broadcaster = %broadcaster_id, error = %err, "failed to broadcast backfill patches");
                }
                RedemptionApply::Processed
            }
            Err(CommandExecutorError::Queue(QueueError::DuplicateRedemption)) => {
                if let Some(update_command) =
                    outcome.commands.iter().find_map(|command| match command {
                        Command::RedemptionUpdate(cmd) => Some(cmd.clone()),
                        _ => None,
                    })
                {
                    match self
                        .command_executor
                        .execute(
                            broadcaster_id,
                            timezone,
                            &[Command::RedemptionUpdate(update_command)],
                        )
                        .await
                    {
                        Ok(patches) => {
                            counter!("backfill_processed_total").increment(1);
                            if let Err(err) = self.broadcast_patches(broadcaster_id, patches).await
                            {
                                warn!(stage = "sse", broadcaster = %broadcaster_id, error = %err, "failed to broadcast backfill patches");
                            }
                            RedemptionApply::Processed
                        }
                        Err(err) => {
                            error!(stage = "oauth", broadcaster = %broadcaster_id, error = %err, "backfill redemption retry failed");
                            RedemptionApply::Failed("command:failed")
                        }
                    }
                } else {
                    warn!(
                        stage = "oauth",
                        broadcaster = %broadcaster_id,
                        "backfill commands missing redemption.update"
                    );
                    RedemptionApply::Duplicate
                }
            }
            Err(err) => {
                error!(stage = "oauth", broadcaster = %broadcaster_id, error = %err, "backfill command execution failed");
                RedemptionApply::Failed("command:failed")
            }
        }
    }

    async fn broadcast_patches(
        &self,
        broadcaster_id: &str,
        patches: Vec<Patch>,
    ) -> Result<(), SseError> {
        let now = self.now();
        for patch in patches {
            self.sse
                .broadcast_patch(broadcaster_id, &patch, now)
                .await?;
            self.publish_sse_stage(broadcaster_id, &patch);
        }
        Ok(())
    }

    fn publish_sse_stage(&self, broadcaster_id: &str, patch: &Patch) {
        let latency = self
            .now()
            .signed_duration_since(patch.at)
            .num_milliseconds() as f64;
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Sse,
            trace_id: None,
            op_id: None,
            version: Some(patch.version),
            broadcaster_id: Some(broadcaster_id.to_string()),
            meta: StageMetadata {
                message: Some(patch.kind_str().to_string()),
                latency_ms: Some(latency),
                ..StageMetadata::default()
            },
            r#in: StagePayload::default(),
            out: StagePayload {
                redacted: true,
                payload: json!({
                    "type": patch.kind_str(),
                    "version": patch.version,
                }),
                truncated: None,
            },
        };
        self.tap.publish(event);
    }

    fn publish_policy_tap(
        &self,
        broadcaster_id: &str,
        normalized: &NormalizedEvent,
        outcome: &PolicyOutcome,
    ) {
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Policy,
            trace_id: None,
            op_id: None,
            version: None,
            broadcaster_id: Some(broadcaster_id.to_string()),
            meta: StageMetadata {
                event_type: Some(normalized.event_type().to_string()),
                ..StageMetadata::default()
            },
            r#in: StagePayload {
                redacted: true,
                payload: normalized.redacted(),
                truncated: None,
            },
            out: StagePayload {
                redacted: true,
                payload: outcome.redacted(),
                truncated: None,
            },
        };
        self.tap.publish(event);
    }

    fn publish_backfill_event(
        &self,
        broadcaster_id: &str,
        redemption: &HelixRedemption,
        result: &str,
        error: Option<&str>,
    ) {
        let event = StageEvent {
            ts: self.now(),
            stage: StageKind::Oauth,
            trace_id: None,
            op_id: None,
            version: None,
            broadcaster_id: Some(broadcaster_id.to_string()),
            meta: StageMetadata {
                message: Some(result.to_string()),
                ..StageMetadata::default()
            },
            r#in: StagePayload {
                redacted: true,
                payload: json!({
                    "redemption_id": redemption.id,
                    "reward_id": redemption.reward.id,
                }),
                truncated: None,
            },
            out: StagePayload {
                redacted: true,
                payload: json!({
                    "result": result,
                    "error": error,
                }),
                truncated: None,
            },
        };
        self.tap.publish(event);
    }

    async fn update_checkpoint_status(
        &self,
        broadcaster_id: &str,
        status: HelixBackfillStatus,
        cursor: Option<String>,
        last_redemption_id: Option<String>,
        last_seen_at: Option<DateTime<Utc>>,
        error_message: Option<String>,
    ) -> Result<(), BackfillError> {
        let mut tx = self
            .database
            .pool()
            .begin()
            .await
            .map_err(BackfillError::from_sqlx)?;
        let checkpoint = HelixBackfillCheckpoint {
            broadcaster_id: broadcaster_id.to_string(),
            cursor,
            last_redemption_id,
            last_seen_at,
            last_run_at: self.now(),
            status,
            error_message,
            updated_at: self.now(),
        };
        self.database
            .helix_backfill()
            .upsert(&mut tx, &checkpoint)
            .await
            .map_err(BackfillError::Checkpoint)?;
        tx.commit().await.map_err(BackfillError::from_sqlx)?;
        Ok(())
    }

    async fn record_oauth_failure(&self, link: &OauthLink, reason: &str, requires_reauth: bool) {
        let mut tx = match self.database.pool().begin().await {
            Ok(tx) => tx,
            Err(err) => {
                error!(stage = "oauth", error = %err, "failed to open transaction for oauth failure");
                return;
            }
        };
        if let Err(err) = self
            .database
            .oauth_links()
            .mark_failure(
                &mut tx,
                &OauthFailure {
                    broadcaster_id: &link.broadcaster_id,
                    twitch_user_id: link.twitch_user_id.clone(),
                    occurred_at: self.now(),
                    reason,
                    requires_reauth,
                },
            )
            .await
        {
            warn!(stage = "oauth", error = %err, "failed to record oauth failure");
        }
        if let Err(err) = tx.commit().await {
            warn!(stage = "oauth", error = %err, "failed to commit oauth failure");
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

enum RedemptionApply {
    Processed,
    Duplicate,
    Skipped(String),
    Failed(&'static str),
}

#[derive(thiserror::Error, Debug)]
pub enum BackfillError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("oauth error: {0}")]
    Oauth(#[from] OauthLinkError),
    #[error("checkpoint error: {0}")]
    Checkpoint(HelixBackfillError),
    #[error("settings error: {0}")]
    Settings(SettingsError),
    #[error("helix error: {0}")]
    Helix(HelixError),
}

impl BackfillError {
    fn from_sqlx(err: sqlx::Error) -> Self {
        BackfillError::Database(err)
    }
}

#[derive(Debug, Deserialize)]
pub struct DebugHelixQuery {
    pub broadcaster: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DebugHelixResponse {
    broadcaster: String,
    token: Option<TokenStatus>,
    checkpoint: Option<CheckpointStatus>,
    managed_rewards: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenStatus {
    expires_at: DateTime<Utc>,
    requires_reauth: bool,
    last_validated_at: Option<DateTime<Utc>>,
    last_failure_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointStatus {
    status: String,
    last_run_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
    last_redemption_id: Option<String>,
    cursor: Option<String>,
    error_message: Option<String>,
    updated_at: DateTime<Utc>,
}

pub async fn debug_helix(
    State(state): State<AppState>,
    Query(query): Query<DebugHelixQuery>,
) -> Result<Json<DebugHelixResponse>, ProblemResponse> {
    crate::oauth::ensure_broadcaster(&state, &query.broadcaster).await?;

    let token = state
        .storage()
        .oauth_links()
        .fetch_by_broadcaster(&query.broadcaster)
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, broadcaster = %query.broadcaster, "failed to load oauth link for debug");
            ProblemResponse::new(StatusCode::INTERNAL_SERVER_ERROR, "debug_oauth_error", "failed to load OAuth link")
        })?;

    let checkpoint = state
        .storage()
        .helix_backfill()
        .fetch(&query.broadcaster)
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, broadcaster = %query.broadcaster, "failed to load backfill checkpoint");
            ProblemResponse::new(StatusCode::INTERNAL_SERVER_ERROR, "debug_backfill_error", "failed to load backfill checkpoint")
        })?;

    let managed_rewards = state
        .storage()
        .broadcasters()
        .fetch_settings(&query.broadcaster)
        .await
        .map(|profile| profile.settings.policy.target_rewards.clone())
        .unwrap_or_default();

    let token_status = token.map(|link| TokenStatus {
        expires_at: link.expires_at,
        requires_reauth: link.requires_reauth,
        last_validated_at: link.last_validated_at,
        last_failure_reason: link.last_failure_reason,
    });

    let checkpoint_status = checkpoint.map(|cp| CheckpointStatus {
        status: cp.status.as_str().to_string(),
        last_run_at: cp.last_run_at,
        last_seen_at: cp.last_seen_at,
        last_redemption_id: cp.last_redemption_id,
        cursor: cp.cursor,
        error_message: cp.error_message,
        updated_at: cp.updated_at,
    });

    Ok(Json(DebugHelixResponse {
        broadcaster: query.broadcaster,
        token: token_status,
        checkpoint: checkpoint_status,
        managed_rewards,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use chrono::{Duration as ChronoDuration, SecondsFormat, TimeZone, Utc};
    use http_body_util::BodyExt;
    use httpmock::prelude::*;
    use reqwest::Client;
    use serde_json::json;
    use sqlx::query;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
    use tower::ServiceExt;
    use twi_overlay_core::policy::PolicyEngine;
    use twi_overlay_core::types::{
        Command, CommandSource, EnqueueCommand, NormalizedReward, NormalizedUser,
    };
    use twi_overlay_storage::{
        Database, HelixBackfillCheckpoint, HelixBackfillStatus, NewOauthLink,
    };
    use twi_overlay_twitch::{HelixClient, TwitchOAuthClient};
    use url::Url;

    use crate::command::{CommandExecutor, ERR_HELIX_UNAUTHORIZED};
    use crate::router::{app_router, AppState};
    use crate::sse::SseHub;
    use crate::tap::TapHub;
    use crate::telemetry;

    const BROADCASTER_ID: &str = "b-1";

    #[tokio::test]
    async fn backfill_worker_processes_redemption_and_updates_checkpoint() {
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        provision_broadcaster(&database).await;
        insert_state_index(&database).await;
        insert_oauth_link(&database, ChronoDuration::hours(1), false).await;

        let tap = TapHub::new();
        let http = Client::builder().build().expect("client");
        let helix_server = MockServer::start();
        let helix_client = HelixClient::new(
            "client",
            Url::parse(&format!("{}/", helix_server.base_url())).expect("url"),
            http.clone(),
        );
        let policy = Arc::new(PolicyEngine::new());
        let clock_now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let clock = Arc::new(move || clock_now);
        let command_executor = CommandExecutor::new(
            database.clone(),
            tap.clone(),
            clock.clone(),
            helix_client.clone(),
        );
        let sse = SseHub::new(database.clone(), 64, StdDuration::from_secs(60));
        let (_service, mut worker) = BackfillService::new(
            database.clone(),
            tap,
            policy,
            command_executor,
            sse,
            helix_client.clone(),
            clock,
            StdDuration::from_secs(60),
            50,
        );

        let redeemed_at = "2024-01-01T00:00:00Z";
        helix_server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/channel_points/custom_rewards/redemptions")
                .query_param("broadcaster_id", BROADCASTER_ID)
                .query_param("status", "UNFULFILLED")
                .query_param("first", "50");
            then.status(200).json_body(json!({
                "data": [{
                    "id": "red-1",
                    "broadcaster_id": BROADCASTER_ID,
                    "broadcaster_login": "example",
                    "broadcaster_name": "Example",
                    "user_id": "user-1",
                    "user_login": "user1",
                    "user_name": "User 1",
                    "user_input": "",
                    "status": "UNFULFILLED",
                    "reward": {
                        "id": "reward-1",
                        "title": "Managed reward",
                        "prompt": null,
                        "cost": 1000
                    },
                    "redeemed_at": redeemed_at
                }],
                "pagination": {"cursor": null}
            }));
        });

        worker
            .run_single(BROADCASTER_ID)
            .await
            .expect("backfill run");

        let queue_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM queue_entries WHERE broadcaster_id = ?")
                .bind(BROADCASTER_ID)
                .fetch_one(database.pool())
                .await
                .expect("queue count");
        assert_eq!(queue_count.0, 1);

        let checkpoint = database
            .helix_backfill()
            .fetch(BROADCASTER_ID)
            .await
            .expect("fetch checkpoint")
            .expect("checkpoint present");
        assert_eq!(checkpoint.status, HelixBackfillStatus::Idle);
        assert_eq!(checkpoint.last_redemption_id.as_deref(), Some("red-1"));
    }

    #[tokio::test]
    async fn backfill_worker_retries_helix_when_queue_entry_exists() {
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        provision_broadcaster(&database).await;
        insert_state_index(&database).await;
        insert_oauth_link(&database, ChronoDuration::hours(1), false).await;

        let tap = TapHub::new();
        let http = Client::builder().build().expect("client");
        let helix_server = MockServer::start();
        let helix_client = HelixClient::new(
            "client",
            Url::parse(&format!("{}/", helix_server.base_url())).expect("url"),
            http.clone(),
        );
        let policy = Arc::new(PolicyEngine::new());
        let clock_now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let clock = Arc::new(move || clock_now);
        let command_executor = CommandExecutor::new(
            database.clone(),
            tap.clone(),
            clock.clone(),
            helix_client.clone(),
        );
        let sse = SseHub::new(database.clone(), 64, StdDuration::from_secs(60));
        let (_service, mut worker) = BackfillService::new(
            database.clone(),
            tap,
            policy,
            command_executor.clone(),
            sse,
            helix_client.clone(),
            clock,
            StdDuration::from_secs(60),
            50,
        );

        let enqueue = Command::Enqueue(EnqueueCommand {
            broadcaster_id: BROADCASTER_ID.to_string(),
            issued_at: clock_now,
            source: CommandSource::Policy,
            user: NormalizedUser {
                id: "user-1".to_string(),
                login: Some("user1".to_string()),
                display_name: Some("User 1".to_string()),
            },
            reward: NormalizedReward {
                id: "reward-1".to_string(),
                title: Some("Managed reward".to_string()),
                cost: Some(1000),
            },
            redemption_id: "red-1".to_string(),
            managed: Some(false),
        });
        command_executor
            .execute(BROADCASTER_ID, "UTC", &[enqueue])
            .await
            .expect("seed queue entry");

        helix_server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/channel_points/custom_rewards/redemptions")
                .query_param("broadcaster_id", BROADCASTER_ID)
                .query_param("status", "UNFULFILLED")
                .query_param("first", "50");
            then.status(200).json_body(json!({
                "data": [{
                    "id": "red-1",
                    "broadcaster_id": BROADCASTER_ID,
                    "broadcaster_login": "example",
                    "broadcaster_name": "Example",
                    "user_id": "user-1",
                    "user_login": "user1",
                    "user_name": "User 1",
                    "user_input": "",
                    "status": "UNFULFILLED",
                    "reward": {
                        "id": "reward-1",
                        "title": "Managed reward",
                        "prompt": null,
                        "cost": 1000
                    },
                    "redeemed_at": "2024-01-01T00:00:00Z"
                }],
                "pagination": {"cursor": null}
            }));
        });

        let patch_mock = helix_server.mock(|when, then| {
            when.method(httpmock::Method::PATCH)
                .path("/channel_points/custom_rewards/redemptions")
                .query_param("broadcaster_id", BROADCASTER_ID)
                .query_param("reward_id", "reward-1")
                .query_param("id", "red-1");
            then.status(200);
        });

        worker
            .run_single(BROADCASTER_ID)
            .await
            .expect("backfill run");

        patch_mock.assert();

        let (managed,): (i64,) = sqlx::query_as(
            "SELECT managed FROM queue_entries WHERE broadcaster_id = ? AND redemption_id = ?",
        )
        .bind(BROADCASTER_ID)
        .bind("red-1")
        .fetch_one(database.pool())
        .await
        .expect("managed flag");
        assert_eq!(managed, 1);

        let checkpoint = database
            .helix_backfill()
            .fetch(BROADCASTER_ID)
            .await
            .expect("fetch checkpoint")
            .expect("checkpoint present");
        assert_eq!(checkpoint.status, HelixBackfillStatus::Idle);
        assert_eq!(checkpoint.last_redemption_id.as_deref(), Some("red-1"));
    }

    #[tokio::test]
    async fn backfill_worker_marks_error_on_helix_failure() {
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        provision_broadcaster(&database).await;
        insert_state_index(&database).await;
        insert_oauth_link(&database, ChronoDuration::hours(1), false).await;

        let tap = TapHub::new();
        let http = Client::builder().build().expect("client");
        let helix_server = MockServer::start();
        let helix_client = HelixClient::new(
            "client",
            Url::parse(&format!("{}/", helix_server.base_url())).expect("url"),
            http.clone(),
        );
        let policy = Arc::new(PolicyEngine::new());
        let clock_now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let clock = Arc::new(move || clock_now);
        let command_executor = CommandExecutor::new(
            database.clone(),
            tap.clone(),
            clock.clone(),
            helix_client.clone(),
        );
        let sse = SseHub::new(database.clone(), 64, StdDuration::from_secs(60));
        let (_service, mut worker) = BackfillService::new(
            database.clone(),
            tap,
            policy,
            command_executor,
            sse,
            helix_client.clone(),
            clock,
            StdDuration::from_secs(60),
            50,
        );

        helix_server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/channel_points/custom_rewards/redemptions")
                .query_param("broadcaster_id", BROADCASTER_ID)
                .query_param("status", "UNFULFILLED")
                .query_param("first", "50");
            then.status(401)
                .json_body(json!({"error": "Unauthorized", "status": 401, "message": "invalid"}));
        });

        let result = worker.run_single(BROADCASTER_ID).await;
        assert!(matches!(result, Err(BackfillError::Helix(_))));

        let checkpoint = database
            .helix_backfill()
            .fetch(BROADCASTER_ID)
            .await
            .expect("fetch checkpoint")
            .expect("checkpoint present");
        assert_eq!(checkpoint.status, HelixBackfillStatus::Error);
        assert_eq!(
            checkpoint.error_message.as_deref(),
            Some(ERR_HELIX_UNAUTHORIZED)
        );

        let link = database
            .oauth_links()
            .fetch_by_broadcaster(BROADCASTER_ID)
            .await
            .expect("fetch link")
            .expect("link present");
        assert!(link.requires_reauth);
        assert_eq!(
            link.last_failure_reason.as_deref(),
            Some(ERR_HELIX_UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn debug_helix_returns_status_payload() {
        let metrics = telemetry::init_metrics().expect("metrics");
        let tap = TapHub::new();
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        provision_broadcaster(&database).await;
        insert_state_index(&database).await;
        insert_oauth_link(&database, ChronoDuration::hours(1), true).await;
        insert_checkpoint(&database, HelixBackfillStatus::Idle).await;

        let http = Client::builder().build().expect("client");
        let oauth_client = TwitchOAuthClient::new(
            "client",
            "secret",
            Url::parse("https://id.twitch.tv/oauth2/").expect("url"),
            http.clone(),
        );
        let helix_client = HelixClient::new(
            "client",
            Url::parse("https://api.twitch.tv/helix/").expect("url"),
            http,
        );

        let (state, _worker) = AppState::new(
            metrics,
            tap,
            database.clone(),
            Arc::from(b"secret".to_vec().into_boxed_slice()),
            b"token-secret".to_vec(),
            64,
            StdDuration::from_secs(60),
            25,
            helix_client,
            oauth_client,
            "http://localhost/oauth/callback".to_string(),
            StdDuration::from_secs(600),
            StdDuration::from_secs(300),
            50,
        );

        let response = app_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/_debug/helix?broadcaster=b-1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let payload: DebugHelixResponse = serde_json::from_slice(&body).expect("json decode");
        assert_eq!(payload.broadcaster, BROADCASTER_ID);
        assert!(payload.token.unwrap().requires_reauth);
        assert_eq!(payload.managed_rewards, vec!["reward-1".to_string()]);
        assert_eq!(payload.checkpoint.unwrap().status, "idle");
    }

    async fn provision_broadcaster(database: &Database) {
        let created_at = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let settings = json!({
            "overlay_theme": "neon",
            "group_size": 1,
            "clear_on_stream_start": false,
            "clear_decrement_counts": false,
            "policy": {
                "anti_spam_window_sec": 60,
                "duplicate_policy": "consume",
                "target_rewards": ["reward-1"]
            }
        });

        query(
            "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(BROADCASTER_ID)
        .bind("twitch-".to_string() + BROADCASTER_ID)
        .bind("Example")
        .bind("UTC")
        .bind(settings.to_string())
        .bind(created_at.to_rfc3339_opts(SecondsFormat::Secs, true))
        .bind(created_at.to_rfc3339_opts(SecondsFormat::Secs, true))
        .execute(database.pool())
        .await
        .expect("insert broadcaster");
    }

    async fn insert_state_index(database: &Database) {
        let created_at = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        query(
            "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES (?, 0, ?)",
        )
        .bind(BROADCASTER_ID)
        .bind(created_at.to_rfc3339_opts(SecondsFormat::Secs, true))
        .execute(database.pool())
        .await
        .expect("insert state index");
    }

    async fn insert_oauth_link(database: &Database, ttl: ChronoDuration, require_reauth: bool) {
        let repo = database.oauth_links();
        let command_repo = database.command_log();
        let mut tx = command_repo.begin().await.expect("begin");
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        repo.upsert_link(
            &mut tx,
            &NewOauthLink {
                id: "link-1".into(),
                broadcaster_id: BROADCASTER_ID,
                twitch_user_id: "twitch-123".into(),
                scopes: vec![
                    "channel:read:redemptions".into(),
                    "channel:manage:redemptions".into(),
                ],
                managed_scopes: vec![
                    "channel:read:redemptions".into(),
                    "channel:manage:redemptions".into(),
                ],
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now + ttl,
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("upsert link");
        tx.commit().await.expect("commit");

        if require_reauth {
            let command_repo = database.command_log();
            let mut tx = command_repo.begin().await.expect("begin mark");
            database
                .oauth_links()
                .mark_failure(
                    &mut tx,
                    &OauthFailure {
                        broadcaster_id: BROADCASTER_ID,
                        twitch_user_id: "twitch-123".into(),
                        occurred_at: now,
                        reason: "reauth",
                        requires_reauth: true,
                    },
                )
                .await
                .expect("mark failure");
            tx.commit().await.expect("commit mark");
        }
    }

    async fn insert_checkpoint(database: &Database, status: HelixBackfillStatus) {
        let repo = database.helix_backfill();
        let mut tx = database.pool().begin().await.expect("begin");
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let checkpoint = HelixBackfillCheckpoint {
            broadcaster_id: BROADCASTER_ID.to_string(),
            cursor: Some("cursor-1".into()),
            last_redemption_id: Some("red-1".into()),
            last_seen_at: Some(now),
            last_run_at: now,
            status,
            error_message: None,
            updated_at: now,
        };
        repo.upsert(&mut tx, &checkpoint)
            .await
            .expect("insert checkpoint");
        tx.commit().await.expect("commit checkpoint");
    }
}
