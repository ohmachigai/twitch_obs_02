use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::Sse, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use metrics::counter;
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{error, info};
use twi_overlay_core::policy::PolicyEngine;
use twi_overlay_core::types::{
    Command, CommandSource, Patch, QueueCompleteCommand, QueueRemovalReason, QueueRemoveCommand,
    SettingsUpdateCommand,
};
use twi_overlay_storage::{Database, QueueError, SettingsError};
use uuid::Uuid;

use crate::command::{CommandApplyResult, CommandExecutor, CommandExecutorError};
use crate::problem::ProblemResponse;
use crate::sse::{Audience, SseHub, SseStream, SseTokenValidator, TokenError};
use crate::state::{build_state_snapshot, StateScope};
use crate::tap::{
    parse_stage_list, tap_keep_alive, tap_stream, StageEvent, StageKind, StageMetadata,
    StagePayload, TapFilter, TapHub,
};
use crate::webhook::emit_sse_stage;
use crate::{telemetry, webhook};
use twi_overlay_core::types::StateSnapshot;

#[derive(Clone)]
pub struct AppState {
    metrics: PrometheusHandle,
    tap: TapHub,
    storage: Database,
    webhook_secret: Arc<[u8]>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    policy_engine: Arc<PolicyEngine>,
    command_executor: CommandExecutor,
    sse: SseHub,
    token_validator: SseTokenValidator,
    sse_heartbeat_secs: u64,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        metrics: PrometheusHandle,
        tap: TapHub,
        storage: Database,
        webhook_secret: Arc<[u8]>,
        sse_token_secret: Vec<u8>,
        sse_ring_max: usize,
        sse_ring_ttl: Duration,
        sse_heartbeat_secs: u64,
    ) -> Self {
        let clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync> = Arc::new(Utc::now);
        let command_executor = CommandExecutor::new(storage.clone(), tap.clone(), clock.clone());
        let sse = SseHub::new(storage.clone(), sse_ring_max, sse_ring_ttl);
        let token_validator = SseTokenValidator::new(sse_token_secret);
        Self {
            metrics,
            tap,
            storage,
            webhook_secret,
            clock,
            policy_engine: Arc::new(PolicyEngine::new()),
            command_executor,
            sse,
            token_validator,
            sse_heartbeat_secs,
        }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        self.clock = clock.clone();
        self.command_executor = CommandExecutor::new(self.storage.clone(), self.tap.clone(), clock);
        self
    }

    pub fn metrics(&self) -> &PrometheusHandle {
        &self.metrics
    }

    pub fn tap(&self) -> &TapHub {
        &self.tap
    }

    pub fn storage(&self) -> &Database {
        &self.storage
    }

    pub fn webhook_secret(&self) -> Arc<[u8]> {
        self.webhook_secret.clone()
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    pub fn policy(&self) -> Arc<PolicyEngine> {
        self.policy_engine.clone()
    }

    pub fn command_executor(&self) -> &CommandExecutor {
        &self.command_executor
    }

    pub fn sse(&self) -> &SseHub {
        &self.sse
    }

    pub fn token_validator(&self) -> &SseTokenValidator {
        &self.token_validator
    }

    pub fn sse_heartbeat(&self) -> u64 {
        self.sse_heartbeat_secs
    }
}

pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/_debug/tap", get(debug_tap))
        .route("/overlay/sse", get(overlay_sse))
        .route("/admin/sse", get(admin_sse))
        .route("/api/state", get(state_snapshot))
        .route("/api/queue/dequeue", post(queue_dequeue))
        .route("/api/settings/update", post(settings_update))
        .route("/eventsub/webhook", post(webhook::handle))
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = telemetry::render_metrics(state.metrics());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(body))
        .unwrap()
}

#[derive(Debug, Deserialize)]
struct TapQuery {
    #[serde(default)]
    s: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseQuery {
    broadcaster: String,
    #[serde(default)]
    since_version: Option<u64>,
    #[serde(default)]
    types: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StateQuery {
    broadcaster: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueueDequeueRequest {
    broadcaster: String,
    entry_id: String,
    mode: QueueDequeueMode,
    op_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum QueueDequeueMode {
    Complete,
    Undo,
}

impl QueueDequeueMode {
    fn as_str(&self) -> &'static str {
        match self {
            QueueDequeueMode::Complete => "COMPLETE",
            QueueDequeueMode::Undo => "UNDO",
        }
    }
}

#[derive(Debug, Serialize)]
struct QueueDequeueResultBody {
    entry_id: String,
    mode: String,
    user_today_count: u32,
}

#[derive(Debug, Serialize)]
struct QueueDequeueResponse {
    version: u64,
    result: QueueDequeueResultBody,
}

#[derive(Debug, Deserialize)]
struct SettingsUpdateRequest {
    broadcaster: String,
    patch: Value,
    op_id: String,
}

#[derive(Debug, Serialize)]
struct SettingsUpdateResultBody {
    applied: bool,
}

#[derive(Debug, Serialize)]
struct SettingsUpdateResponse {
    version: u64,
    result: SettingsUpdateResultBody,
}

async fn debug_tap(
    State(state): State<AppState>,
    Query(query): Query<TapQuery>,
) -> Result<
    Sse<impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, serde_json::Error>>>,
    (StatusCode, String),
> {
    let stages = parse_stage_list(query.s).map_err(|err| (StatusCode::BAD_REQUEST, err))?;
    let filter = TapFilter::from_stages(stages);
    let stream = tap_stream(state.tap().clone(), filter);

    Ok(Sse::new(stream).keep_alive(tap_keep_alive()))
}

async fn overlay_sse(
    State(state): State<AppState>,
    Query(query): Query<SseQuery>,
    headers: HeaderMap,
) -> Result<Sse<SseStream>, (StatusCode, String)> {
    sse_handler(state, query, headers, Audience::Overlay).await
}

async fn admin_sse(
    State(state): State<AppState>,
    Query(query): Query<SseQuery>,
    headers: HeaderMap,
) -> Result<Sse<SseStream>, (StatusCode, String)> {
    sse_handler(state, query, headers, Audience::Admin).await
}

async fn state_snapshot(
    State(state): State<AppState>,
    Query(query): Query<StateQuery>,
    headers: HeaderMap,
) -> Result<Json<StateSnapshot>, ProblemResponse> {
    let token = extract_bearer_token(&headers)
        .map(|value| value.to_string())
        .or_else(|| query.token.clone())
        .ok_or_else(|| {
            counter!("api_state_requests_total", "result" => "unauthorized").increment(1);
            ProblemResponse::new(
                StatusCode::UNAUTHORIZED,
                "missing_token",
                "state endpoint requires a bearer token",
            )
        })?;

    let now = state.now();
    let audience = match state.token_validator().validate_any(
        &token,
        &[Audience::Overlay, Audience::Admin],
        &query.broadcaster,
        now,
    ) {
        Ok(audience) => audience,
        Err(err) => {
            counter!("api_state_requests_total", "result" => "unauthorized").increment(1);
            return Err(problem_for_token_error(err));
        }
    };

    let scope = match parse_state_scope(query.scope.as_deref(), query.since.as_deref()) {
        Ok(scope) => scope,
        Err(problem) => {
            counter!("api_state_requests_total", "result" => "error").increment(1);
            return Err(problem);
        }
    };

    let profile = match state
        .storage()
        .broadcasters()
        .fetch_settings(&query.broadcaster)
        .await
    {
        Ok(profile) => profile,
        Err(SettingsError::NotFound) => {
            counter!("api_state_requests_total", "result" => "error").increment(1);
            return Err(ProblemResponse::new(
                StatusCode::NOT_FOUND,
                "broadcaster_not_found",
                "broadcaster is not provisioned",
            ));
        }
        Err(err) => {
            counter!("api_state_requests_total", "result" => "error").increment(1);
            error!(
                stage = "state",
                broadcaster = %query.broadcaster,
                error = %err,
                "failed to load broadcaster settings"
            );
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings_error",
                "failed to load broadcaster settings",
            ));
        }
    };

    let snapshot =
        match build_state_snapshot(state.storage(), &query.broadcaster, &profile, now, scope).await
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                counter!("api_state_requests_total", "result" => "error").increment(1);
                error!(
                    stage = "state",
                    broadcaster = %query.broadcaster,
                    error = %err,
                    "failed to build state snapshot"
                );
                return Err(ProblemResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "state_error",
                    "failed to build state snapshot",
                ));
            }
        };

    counter!("api_state_requests_total", "result" => "ok").increment(1);

    let scope_label = match scope {
        StateScope::Session => "session",
        StateScope::Since(_) => "since",
    };

    info!(
        stage = "state",
        broadcaster = %query.broadcaster,
        scope = scope_label,
        version = snapshot.version,
        audience = audience.as_str(),
        queue_len = snapshot.queue.len(),
        counter_len = snapshot.counters_today.len(),
        "state snapshot served"
    );

    let event = StageEvent {
        ts: state.now(),
        stage: StageKind::State,
        trace_id: None,
        op_id: None,
        version: Some(snapshot.version),
        broadcaster_id: Some(query.broadcaster.clone()),
        meta: StageMetadata {
            message: Some(scope_label.to_string()),
            ..StageMetadata::default()
        },
        r#in: StagePayload {
            redacted: true,
            payload: json!({
                "scope": scope_label,
                "since": query.since,
                "aud": audience.as_str(),
            }),
            truncated: None,
        },
        out: StagePayload {
            redacted: false,
            payload: json!({
                "queue_count": snapshot.queue.len(),
                "counter_count": snapshot.counters_today.len(),
            }),
            truncated: None,
        },
    };
    state.tap().publish(event);

    Ok(Json(snapshot))
}

async fn queue_dequeue(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<QueueDequeueRequest>,
) -> Result<Json<QueueDequeueResponse>, ProblemResponse> {
    let token = extract_bearer_token(&headers).ok_or_else(|| {
        counter!("api_queue_dequeue_requests_total", "result" => "unauthorized").increment(1);
        ProblemResponse::new(
            StatusCode::UNAUTHORIZED,
            "missing_token",
            "queue dequeue endpoint requires a bearer token",
        )
    })?;

    if Uuid::parse_str(&payload.op_id).is_err() {
        counter!("api_queue_dequeue_requests_total", "result" => "error").increment(1);
        return Err(ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "invalid_op_id",
            "op_id must be a valid UUID",
        ));
    }

    let now = state.now();
    if let Err(err) =
        state
            .token_validator()
            .validate(token, Audience::Admin, &payload.broadcaster, now)
    {
        counter!("api_queue_dequeue_requests_total", "result" => "unauthorized").increment(1);
        return Err(problem_for_token_error(err));
    }

    let profile = match state
        .storage()
        .broadcasters()
        .fetch_settings(&payload.broadcaster)
        .await
    {
        Ok(profile) => profile,
        Err(SettingsError::NotFound) => {
            counter!("api_queue_dequeue_requests_total", "result" => "error").increment(1);
            return Err(ProblemResponse::new(
                StatusCode::NOT_FOUND,
                "broadcaster_not_found",
                "broadcaster is not provisioned",
            ));
        }
        Err(err) => {
            counter!("api_queue_dequeue_requests_total", "result" => "error").increment(1);
            error!(
                stage = "mutation",
                broadcaster = %payload.broadcaster,
                error = %err,
                "failed to load broadcaster settings",
            );
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings_error",
                "failed to load broadcaster settings",
            ));
        }
    };

    let command = match payload.mode {
        QueueDequeueMode::Complete => Command::QueueComplete(QueueCompleteCommand {
            broadcaster_id: payload.broadcaster.clone(),
            issued_at: now,
            source: CommandSource::Admin,
            entry_id: payload.entry_id.clone(),
            op_id: payload.op_id.clone(),
        }),
        QueueDequeueMode::Undo => Command::QueueRemove(QueueRemoveCommand {
            broadcaster_id: payload.broadcaster.clone(),
            issued_at: now,
            source: CommandSource::Admin,
            entry_id: payload.entry_id.clone(),
            reason: QueueRemovalReason::Undo,
            op_id: payload.op_id.clone(),
        }),
    };

    let application = match state
        .command_executor()
        .execute_admin_command(&payload.broadcaster, &profile.timezone, command)
        .await
    {
        Ok(application) => application,
        Err(err) => {
            let (problem, label) = queue_error_response(&payload, err);
            counter!("api_queue_dequeue_requests_total", "result" => label).increment(1);
            return Err(problem);
        }
    };

    broadcast_patches(&state, &payload.broadcaster, &application.patches).await;

    let (entry_id, mode, user_today_count) = match application.result {
        CommandApplyResult::QueueMutation {
            entry_id,
            mode,
            user_today_count,
        } => (entry_id, mode, user_today_count),
        other => {
            counter!("api_queue_dequeue_requests_total", "result" => "error").increment(1);
            error!(
                stage = "mutation",
                broadcaster = %payload.broadcaster,
                mode = payload.mode.as_str(),
                op_id = %payload.op_id,
                result = ?other,
                "unexpected command result for queue dequeue",
            );
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected_result",
                "executor returned unexpected result",
            ));
        }
    };

    counter!("api_queue_dequeue_requests_total", "result" => "ok").increment(1);
    info!(
        stage = "mutation",
        kind = "queue.dequeue",
        broadcaster = %payload.broadcaster,
        entry_id = %entry_id,
        mode = mode.as_str(),
        op_id = %payload.op_id,
        duplicate = application.duplicate,
        version = application.version,
        user_today_count,
        "queue entry updated via admin mutation",
    );

    Ok(Json(QueueDequeueResponse {
        version: application.version,
        result: QueueDequeueResultBody {
            entry_id,
            mode: mode.as_str().to_string(),
            user_today_count,
        },
    }))
}

async fn settings_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SettingsUpdateRequest>,
) -> Result<Json<SettingsUpdateResponse>, ProblemResponse> {
    let token = extract_bearer_token(&headers).ok_or_else(|| {
        counter!("api_settings_update_requests_total", "result" => "unauthorized").increment(1);
        ProblemResponse::new(
            StatusCode::UNAUTHORIZED,
            "missing_token",
            "settings update endpoint requires a bearer token",
        )
    })?;

    if Uuid::parse_str(&payload.op_id).is_err() {
        counter!("api_settings_update_requests_total", "result" => "error").increment(1);
        return Err(ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "invalid_op_id",
            "op_id must be a valid UUID",
        ));
    }

    let now = state.now();
    if let Err(err) =
        state
            .token_validator()
            .validate(token, Audience::Admin, &payload.broadcaster, now)
    {
        counter!("api_settings_update_requests_total", "result" => "unauthorized").increment(1);
        return Err(problem_for_token_error(err));
    }

    let profile = match state
        .storage()
        .broadcasters()
        .fetch_settings(&payload.broadcaster)
        .await
    {
        Ok(profile) => profile,
        Err(SettingsError::NotFound) => {
            counter!("api_settings_update_requests_total", "result" => "error").increment(1);
            return Err(ProblemResponse::new(
                StatusCode::NOT_FOUND,
                "broadcaster_not_found",
                "broadcaster is not provisioned",
            ));
        }
        Err(err) => {
            counter!("api_settings_update_requests_total", "result" => "error").increment(1);
            error!(
                stage = "mutation",
                broadcaster = %payload.broadcaster,
                error = %err,
                "failed to load broadcaster settings",
            );
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings_error",
                "failed to load broadcaster settings",
            ));
        }
    };

    let command = Command::SettingsUpdate(SettingsUpdateCommand {
        broadcaster_id: payload.broadcaster.clone(),
        issued_at: now,
        source: CommandSource::Admin,
        patch: payload.patch.clone(),
        op_id: payload.op_id.clone(),
    });

    let application = match state
        .command_executor()
        .execute_admin_command(&payload.broadcaster, &profile.timezone, command)
        .await
    {
        Ok(application) => application,
        Err(err) => {
            let (problem, label) = settings_error_response(&payload, err);
            counter!("api_settings_update_requests_total", "result" => label).increment(1);
            return Err(problem);
        }
    };

    broadcast_patches(&state, &payload.broadcaster, &application.patches).await;

    let applied = match application.result {
        CommandApplyResult::SettingsUpdated { applied } => applied,
        other => {
            counter!("api_settings_update_requests_total", "result" => "error").increment(1);
            error!(
                stage = "mutation",
                broadcaster = %payload.broadcaster,
                op_id = %payload.op_id,
                result = ?other,
                "unexpected command result for settings update",
            );
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected_result",
                "executor returned unexpected result",
            ));
        }
    };

    counter!("api_settings_update_requests_total", "result" => "ok").increment(1);
    info!(
        stage = "mutation",
        kind = "settings.update",
        broadcaster = %payload.broadcaster,
        op_id = %payload.op_id,
        duplicate = application.duplicate,
        version = application.version,
        "settings updated via admin mutation",
    );

    Ok(Json(SettingsUpdateResponse {
        version: application.version,
        result: SettingsUpdateResultBody { applied },
    }))
}

async fn sse_handler(
    state: AppState,
    query: SseQuery,
    headers: HeaderMap,
    audience: Audience,
) -> Result<Sse<SseStream>, (StatusCode, String)> {
    let token = query
        .token
        .as_deref()
        .ok_or((StatusCode::UNAUTHORIZED, "missing_token".to_string()))?;

    let since_version = headers
        .get("Last-Event-ID")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.parse::<u64>().ok())
        .or(query.since_version);

    let filter_types = parse_types(query.types.clone())?;

    state
        .token_validator()
        .validate(token, audience, &query.broadcaster, state.now())
        .map_err(|_| (StatusCode::FORBIDDEN, "invalid_token".to_string()))?;

    let profile = state
        .storage()
        .broadcasters()
        .fetch_settings(&query.broadcaster)
        .await
        .map_err(|err| match err {
            SettingsError::NotFound => (StatusCode::NOT_FOUND, "broadcaster_not_found".to_string()),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed_to_load_settings".to_string(),
            ),
        })?;

    let subscription = state
        .sse()
        .subscribe(&query.broadcaster, audience, since_version, filter_types)
        .await;

    let stream = if subscription.ring_miss() {
        let patch = state
            .sse()
            .build_state_replace(&query.broadcaster, &profile, state.now())
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed_to_build_state".to_string(),
                )
            })?;
        let message = state.sse().event_from_patch(&patch).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed_to_serialize_state".to_string(),
            )
        })?;
        subscription.into_stream_with_initial(vec![message])
    } else {
        subscription.into_stream()
    };

    let keep_alive = axum::response::sse::KeepAlive::new()
        .interval(Duration::from_secs(state.sse_heartbeat()))
        .text("heartbeat");

    Ok(Sse::new(stream).keep_alive(keep_alive))
}

fn parse_types(raw: Option<String>) -> Result<Option<HashSet<String>>, (StatusCode, String)> {
    let Some(value) = raw else {
        return Ok(None);
    };
    let mut set = HashSet::new();
    for item in value.split(',') {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        set.insert(trimmed.to_string());
    }
    Ok(Some(set))
}

async fn broadcast_patches(state: &AppState, broadcaster_id: &str, patches: &[Patch]) {
    for patch in patches {
        if let Err(err) = state
            .sse()
            .broadcast_patch(broadcaster_id, patch, state.now())
            .await
        {
            error!(
                stage = "sse",
                broadcaster = %broadcaster_id,
                error = %err,
                "failed to broadcast patch",
            );
            continue;
        }
        emit_sse_stage(state, broadcaster_id, patch);
    }
}

fn queue_error_response(
    request: &QueueDequeueRequest,
    err: CommandExecutorError,
) -> (ProblemResponse, &'static str) {
    match err {
        CommandExecutorError::Queue(QueueError::NotFound) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                mode = request.mode.as_str(),
                "queue entry not found",
            );
            (
                ProblemResponse::new(
                    StatusCode::NOT_FOUND,
                    "queue_entry_not_found",
                    "queue entry not found",
                ),
                "not_found",
            )
        }
        CommandExecutorError::Queue(QueueError::InvalidTransition(status)) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                mode = request.mode.as_str(),
                status = ?status,
                "queue entry is not queued",
            );
            (
                ProblemResponse::new(
                    StatusCode::CONFLICT,
                    "invalid_transition",
                    format!("queue entry is not queued (current={status:?})"),
                ),
                "conflict",
            )
        }
        CommandExecutorError::OpConflict { op_id } => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                op_id = %op_id,
                "op_id conflict for queue dequeue",
            );
            (
                ProblemResponse::new(
                    StatusCode::PRECONDITION_FAILED,
                    "op_conflict",
                    "op_id already used with different payload",
                ),
                "conflict",
            )
        }
        CommandExecutorError::InvalidTimezone(detail) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                detail = %detail,
                "invalid timezone while processing queue dequeue",
            );
            (
                ProblemResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid_timezone",
                    detail,
                ),
                "error",
            )
        }
        CommandExecutorError::Settings(SettingsError::NotFound) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                "broadcaster missing during queue dequeue",
            );
            (
                ProblemResponse::new(
                    StatusCode::NOT_FOUND,
                    "broadcaster_not_found",
                    "broadcaster is not provisioned",
                ),
                "error",
            )
        }
        other => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                entry_id = %request.entry_id,
                mode = request.mode.as_str(),
                error = %other,
                "failed to execute queue dequeue",
            );
            (
                ProblemResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "command_error",
                    "failed to execute queue dequeue",
                ),
                "error",
            )
        }
    }
}

fn settings_error_response(
    request: &SettingsUpdateRequest,
    err: CommandExecutorError,
) -> (ProblemResponse, &'static str) {
    match err {
        CommandExecutorError::OpConflict { op_id } => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                op_id = %op_id,
                "op_id conflict for settings update",
            );
            (
                ProblemResponse::new(
                    StatusCode::PRECONDITION_FAILED,
                    "op_conflict",
                    "op_id already used with different payload",
                ),
                "conflict",
            )
        }
        CommandExecutorError::InvalidSettingsPatch(detail) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                detail = %detail,
                "invalid settings patch",
            );
            (
                ProblemResponse::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_patch", detail),
                "error",
            )
        }
        CommandExecutorError::Settings(SettingsError::NotFound) => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                "broadcaster missing during settings update",
            );
            (
                ProblemResponse::new(
                    StatusCode::NOT_FOUND,
                    "broadcaster_not_found",
                    "broadcaster is not provisioned",
                ),
                "error",
            )
        }
        other => {
            error!(
                stage = "mutation",
                broadcaster = %request.broadcaster,
                error = %other,
                "failed to execute settings update",
            );
            (
                ProblemResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "command_error",
                    "failed to execute settings update",
                ),
                "error",
            )
        }
    }
}

fn problem_for_token_error(err: TokenError) -> ProblemResponse {
    ProblemResponse::new(StatusCode::FORBIDDEN, "invalid_token", err.to_string())
}

fn parse_state_scope(
    scope: Option<&str>,
    since: Option<&str>,
) -> Result<StateScope, ProblemResponse> {
    match scope {
        None | Some("session") => Ok(StateScope::Session),
        Some("since") => {
            let since_raw = since.ok_or_else(|| {
                ProblemResponse::new(
                    StatusCode::BAD_REQUEST,
                    "missing_since",
                    "scope=since requires the since parameter",
                )
            })?;
            let parsed = DateTime::parse_from_rfc3339(since_raw).map_err(|err| {
                ProblemResponse::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_since",
                    format!("invalid since timestamp: {err}"),
                )
            })?;
            Ok(StateScope::Since(parsed.with_timezone(&Utc)))
        }
        Some(other) => Err(ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            format!("unsupported scope: {other}"),
        )),
    }
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{HeaderValue, Method, Request, StatusCode},
    };
    use chrono::{Duration as ChronoDuration, SecondsFormat, TimeZone};
    use http_body_util::BodyExt;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::Value;
    use serde_urlencoded::to_string;
    use sqlx::query;
    use std::{sync::Arc, time::Duration as StdDuration};
    use tokio::time::{self, Duration};
    use tower::ServiceExt;

    use crate::sse::TokenClaims;
    use crate::tap::StageEvent;
    use twi_overlay_core::types::{QueueEntryStatus, Settings};

    async fn setup_state() -> AppState {
        let metrics = telemetry::init_metrics().expect("metrics init");
        let tap = TapHub::new();

        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        let secret: Arc<[u8]> = Arc::from(b"test-secret".to_vec().into_boxed_slice());
        AppState::new(
            metrics,
            tap,
            database,
            secret,
            b"token-secret".to_vec(),
            64,
            StdDuration::from_secs(60),
            25,
        )
    }

    fn issue_token(
        secret: &[u8],
        broadcaster: &str,
        audience: &str,
        exp: chrono::DateTime<Utc>,
    ) -> String {
        let claims = TokenClaims {
            sub: broadcaster.to_string(),
            aud: audience.to_string(),
            exp: exp.timestamp() as usize,
            nbf: None,
        };
        let header = Header::new(Algorithm::HS256);
        encode(&header, &claims, &EncodingKey::from_secret(secret)).expect("token encode")
    }

    fn bearer(value: &str) -> HeaderValue {
        let header_value = format!("Bearer {value}");
        HeaderValue::from_str(&header_value).expect("header value")
    }

    fn fmt_time(value: chrono::DateTime<Utc>) -> String {
        value.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    async fn provision_broadcaster(state: &AppState, version: i64) {
        let pool = state.storage().pool();
        let created_at = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        query(
            "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at) VALUES ('b-1','twitch-1','Example','UTC',?, ?, ?)",
        )
        .bind("{}")
        .bind(fmt_time(created_at))
        .bind(fmt_time(created_at))
        .execute(pool)
        .await
        .expect("insert broadcaster");

        query(
            "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES ('b-1', ?, ?)",
        )
        .bind(version)
        .bind(fmt_time(created_at))
        .execute(pool)
        .await
        .expect("insert state index");
    }

    async fn insert_queue_entry(
        state: &AppState,
        id: &str,
        user_id: &str,
        enqueued_at: chrono::DateTime<Utc>,
        last_updated_at: chrono::DateTime<Utc>,
    ) {
        let pool = state.storage().pool();
        query(
            r#"INSERT INTO queue_entries (
                id, broadcaster_id, user_id, user_login, user_display_name, user_avatar,
                reward_id, redemption_id, enqueued_at, status, status_reason, managed, last_updated_at
            ) VALUES (?, 'b-1', ?, ?, ?, ?, 'reward-1', ?, ?, 'QUEUED', NULL, 0, ?)"#,
        )
        .bind(id)
        .bind(user_id)
        .bind(format!("{user_id}-login"))
        .bind(format!("User {user_id}"))
        .bind(Option::<String>::None)
        .bind(Option::<String>::Some(format!("red-{id}")))
        .bind(fmt_time(enqueued_at))
        .bind(fmt_time(last_updated_at))
        .execute(pool)
        .await
        .expect("insert queue entry");
    }

    async fn insert_counter(
        state: &AppState,
        user_id: &str,
        count: i64,
        updated_at: chrono::DateTime<Utc>,
    ) {
        let pool = state.storage().pool();
        let day = updated_at.format("%Y-%m-%d").to_string();
        query(
            "INSERT INTO daily_counters(day, broadcaster_id, user_id, count, updated_at) VALUES (?, 'b-1', ?, ?, ?)",
        )
        .bind(day)
        .bind(user_id)
        .bind(count)
        .bind(fmt_time(updated_at))
        .execute(pool)
        .await
        .expect("insert counter");
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = app_router(setup_state().await);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_exports_build_info() {
        let app = app_router(setup_state().await);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let collected = response
            .into_body()
            .collect()
            .await
            .expect("body should read");
        let body = String::from_utf8(collected.to_bytes().to_vec()).expect("utf-8");
        assert!(body.contains("app_build_info"));
        assert!(body.contains("app_uptime_seconds"));
    }

    #[tokio::test]
    async fn tap_stream_emits_events() {
        let state = setup_state().await;
        let tap = state.tap().clone();
        let app = app_router(state);

        let request = Request::builder()
            .uri("/_debug/tap")
            .body(Body::empty())
            .unwrap();

        let publish = tokio::spawn(async move {
            time::sleep(Duration::from_millis(25)).await;
            tap.publish(StageEvent::mock("test.event"));
        });

        let mut response = app.oneshot(request).await.expect("handler should respond");

        let frame = time::timeout(Duration::from_secs(1), response.body_mut().frame())
            .await
            .expect("stream produced chunk")
            .expect("chunk ok")
            .expect("chunk available");

        let data = match frame.into_data() {
            Ok(data) => data,
            Err(_) => panic!("expected data frame"),
        };
        let text = String::from_utf8(data.to_vec()).expect("utf-8");
        assert!(text.contains("data:"));
        assert!(text.contains("\"stage\":\"sse\""));

        publish.await.expect("publish task");
    }

    #[tokio::test]
    async fn state_snapshot_requires_token() {
        let app = app_router(setup_state().await);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/state?broadcaster=b-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn state_snapshot_returns_session_scope() {
        let fixed_now = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let clock_now = fixed_now;
        let state = setup_state().await.with_clock(Arc::new(move || clock_now));
        provision_broadcaster(&state, 42).await;
        insert_queue_entry(
            &state,
            "entry-1",
            "user-1",
            fixed_now - ChronoDuration::minutes(5),
            fixed_now - ChronoDuration::minutes(5),
        )
        .await;
        insert_counter(&state, "user-1", 3, fixed_now - ChronoDuration::minutes(4)).await;

        let token = issue_token(
            b"token-secret",
            "b-1",
            Audience::Overlay.as_str(),
            fixed_now + ChronoDuration::minutes(10),
        );
        let app = app_router(state.clone());

        state
            .token_validator()
            .validate_any(
                &token,
                &[Audience::Overlay, Audience::Admin],
                "b-1",
                fixed_now,
            )
            .expect("token should be accepted");

        let profile = state
            .storage()
            .broadcasters()
            .fetch_settings("b-1")
            .await
            .expect("profile should load");
        build_state_snapshot(
            state.storage(),
            "b-1",
            &profile,
            fixed_now,
            StateScope::Session,
        )
        .await
        .expect("snapshot should build");

        let mut response = app
            .oneshot(
                Request::builder()
                    .uri("/api/state?broadcaster=b-1")
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        if response.status() != StatusCode::OK {
            let body = response.body_mut().collect().await.unwrap().to_bytes();
            panic!(
                "unexpected status {} body {}",
                response.status(),
                String::from_utf8_lossy(&body)
            );
        }

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json.get("version").and_then(Value::as_u64), Some(42));
        assert_eq!(
            json.get("queue")
                .and_then(Value::as_array)
                .map(|arr| arr.len()),
            Some(1)
        );
        assert_eq!(
            json.get("counters_today")
                .and_then(Value::as_array)
                .map(|arr| arr.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn state_snapshot_since_filters_old_records() {
        let fixed_now = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let clock_now = fixed_now;
        let state = setup_state().await.with_clock(Arc::new(move || clock_now));
        provision_broadcaster(&state, 55).await;
        let earlier = fixed_now - ChronoDuration::minutes(30);
        let recent = fixed_now - ChronoDuration::minutes(5);
        insert_queue_entry(&state, "entry-early", "user-1", earlier, earlier).await;
        insert_queue_entry(&state, "entry-recent", "user-2", recent, recent).await;
        insert_counter(&state, "user-1", 2, earlier).await;
        insert_counter(&state, "user-2", 5, recent).await;

        let token = issue_token(
            b"token-secret",
            "b-1",
            Audience::Overlay.as_str(),
            fixed_now + ChronoDuration::minutes(10),
        );
        let since = (fixed_now - ChronoDuration::minutes(10)).to_rfc3339();
        let query = to_string([
            ("broadcaster", "b-1"),
            ("scope", "since"),
            ("since", since.as_str()),
        ])
        .expect("query should serialize");
        let uri = format!("/api/state?{query}");
        let app = app_router(state.clone());

        state
            .token_validator()
            .validate_any(
                &token,
                &[Audience::Overlay, Audience::Admin],
                "b-1",
                fixed_now,
            )
            .expect("token should be accepted");

        let profile = state
            .storage()
            .broadcasters()
            .fetch_settings("b-1")
            .await
            .expect("profile should load");
        build_state_snapshot(
            state.storage(),
            "b-1",
            &profile,
            fixed_now,
            StateScope::Since(fixed_now - ChronoDuration::minutes(10)),
        )
        .await
        .expect("snapshot should build");

        let mut response = app
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        if response.status() != StatusCode::OK {
            let body = response.body_mut().collect().await.unwrap().to_bytes();
            panic!(
                "unexpected status {} body {}",
                response.status(),
                String::from_utf8_lossy(&body)
            );
        }

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).expect("json");
        let queue = json
            .get("queue")
            .and_then(Value::as_array)
            .expect("queue array");
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].get("id").and_then(Value::as_str),
            Some("entry-recent")
        );
        let counters = json
            .get("counters_today")
            .and_then(Value::as_array)
            .expect("counters array");
        assert_eq!(counters.len(), 1);
        assert_eq!(
            counters[0].get("user_id").and_then(Value::as_str),
            Some("user-2")
        );
    }

    #[tokio::test]
    async fn queue_dequeue_complete_succeeds() {
        let fixed_now = Utc::now();
        let state = setup_state().await.with_clock(Arc::new(move || fixed_now));
        provision_broadcaster(&state, 1).await;
        insert_queue_entry(&state, "entry-1", "user-1", fixed_now, fixed_now).await;
        insert_counter(&state, "user-1", 2, fixed_now).await;

        let token = issue_token(
            b"token-secret",
            "b-1",
            Audience::Admin.as_str(),
            fixed_now + ChronoDuration::minutes(10),
        );
        let op_id = Uuid::new_v4();
        let body = serde_json::to_string(&json!({
            "broadcaster": "b-1",
            "entry_id": "entry-1",
            "mode": "COMPLETE",
            "op_id": op_id,
        }))
        .expect("serialize body");

        let response = app_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/queue/dequeue")
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["result"]["mode"].as_str(), Some("COMPLETE"));

        let status: (String,) =
            sqlx::query_as("SELECT status FROM queue_entries WHERE id = 'entry-1'")
                .fetch_one(state.storage().pool())
                .await
                .expect("entry status");
        assert_eq!(status.0, QueueEntryStatus::Completed.as_str());

        let count: (i64,) =
            sqlx::query_as("SELECT count FROM daily_counters WHERE user_id = 'user-1'")
                .fetch_one(state.storage().pool())
                .await
                .expect("counter");
        assert_eq!(count.0, 2);
    }

    #[tokio::test]
    async fn queue_dequeue_undo_decrements_counter() {
        let fixed_now = Utc::now();
        let state = setup_state().await.with_clock(Arc::new(move || fixed_now));
        provision_broadcaster(&state, 1).await;
        insert_queue_entry(&state, "entry-1", "user-1", fixed_now, fixed_now).await;
        insert_counter(&state, "user-1", 3, fixed_now).await;

        let token = issue_token(
            b"token-secret",
            "b-1",
            Audience::Admin.as_str(),
            fixed_now + ChronoDuration::minutes(10),
        );
        let op_id = Uuid::new_v4();
        let body = serde_json::to_string(&json!({
            "broadcaster": "b-1",
            "entry_id": "entry-1",
            "mode": "UNDO",
            "op_id": op_id,
        }))
        .expect("serialize body");

        let response = app_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/queue/dequeue")
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["result"]["mode"].as_str(), Some("UNDO"));
        assert_eq!(json["result"]["user_today_count"].as_u64(), Some(2));

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, status_reason FROM queue_entries WHERE id = 'entry-1'")
                .fetch_one(state.storage().pool())
                .await
                .expect("entry status");
        assert_eq!(row.0, QueueEntryStatus::Removed.as_str());
        assert_eq!(row.1.as_deref(), Some(QueueRemovalReason::Undo.as_str()));

        let count: (i64,) =
            sqlx::query_as("SELECT count FROM daily_counters WHERE user_id = 'user-1'")
                .fetch_one(state.storage().pool())
                .await
                .expect("counter");
        assert_eq!(count.0, 2);

        let conflict_body = serde_json::to_string(&json!({
            "broadcaster": "b-1",
            "entry_id": "entry-1",
            "mode": "COMPLETE",
            "op_id": op_id,
        }))
        .expect("serialize body");

        let conflict = app_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/queue/dequeue")
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(conflict_body))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(conflict.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn settings_update_applies_patch() {
        let fixed_now = Utc::now();
        let state = setup_state().await.with_clock(Arc::new(move || fixed_now));
        provision_broadcaster(&state, 1).await;

        let token = issue_token(
            b"token-secret",
            "b-1",
            Audience::Admin.as_str(),
            fixed_now + ChronoDuration::minutes(10),
        );
        let body = serde_json::to_string(&json!({
            "broadcaster": "b-1",
            "patch": {"group_size": 4},
            "op_id": Uuid::new_v4(),
        }))
        .expect("serialize body");

        let response = app_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/settings/update")
                    .header(axum::http::header::AUTHORIZATION, bearer(&token))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let payload = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(json["result"]["applied"].as_bool(), Some(true));

        let settings_json: (String,) =
            sqlx::query_as("SELECT settings_json FROM broadcasters WHERE id = 'b-1'")
                .fetch_one(state.storage().pool())
                .await
                .expect("settings json");
        let settings: Settings = serde_json::from_str(&settings_json.0).expect("decode settings");
        assert_eq!(settings.group_size, 4);
    }
}
