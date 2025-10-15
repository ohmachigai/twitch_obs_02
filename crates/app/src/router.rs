use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::Sse, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;
use twi_overlay_core::policy::PolicyEngine;
use twi_overlay_storage::{Database, SettingsError};

use crate::command::CommandExecutor;
use crate::sse::{Audience, SseHub, SseStream, SseTokenValidator};
use crate::tap::{parse_stage_list, tap_keep_alive, tap_stream, TapFilter, TapHub};
use crate::{telemetry, webhook};

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::time::Duration as StdDuration;
    use tokio::time::{self, Duration};
    use tower::ServiceExt;

    use crate::tap::StageEvent;

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
}
