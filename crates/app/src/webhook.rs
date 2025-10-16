use std::{borrow::Cow, sync::Arc, time::Instant};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use metrics::{counter, histogram};
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{error, info, warn};
use twi_overlay_core::normalizer::{Normalizer, NormalizerError};
use twi_overlay_core::types::{Command, NormalizedEvent, Patch, Settings};
use twi_overlay_storage::{
    BroadcasterSettings, EventRawError, EventRawInsertOutcome, NewEventRaw, SettingsError,
};
use uuid::Uuid;

use crate::problem::ProblemResponse;
use crate::router::AppState;
use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload};

const HEADER_MESSAGE_ID: &str = "Twitch-Eventsub-Message-Id";
const HEADER_TIMESTAMP: &str = "Twitch-Eventsub-Message-Timestamp";
const HEADER_SIGNATURE: &str = "Twitch-Eventsub-Message-Signature";
const HEADER_MESSAGE_TYPE: &str = "Twitch-Eventsub-Message-Type";

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ProblemResponse> {
    let start = Instant::now();
    let message_type_value = get_required_header(&headers, HEADER_MESSAGE_TYPE)?;
    let message_type_header = message_type_value.to_string();
    let message_type = match MessageType::try_from(message_type_header.as_str()) {
        Ok(mt) => mt,
        Err(detail) => {
            histogram!("webhook_ack_latency_seconds", "type" => "unknown")
                .record(start.elapsed().as_secs_f64());
            return Err(ProblemResponse::new(
                StatusCode::BAD_REQUEST,
                "invalid_message_type",
                detail,
            ));
        }
    };
    let message_label = message_type.metric_label();

    let message_id = get_required_header(&headers, HEADER_MESSAGE_ID)?;
    let timestamp_raw = get_required_header(&headers, HEADER_TIMESTAMP)?;
    let signature = get_required_header(&headers, HEADER_SIGNATURE)?;

    let timestamp = parse_timestamp(timestamp_raw).map_err(|err| {
        histogram!("webhook_ack_latency_seconds", "type" => message_label)
            .record(start.elapsed().as_secs_f64());
        ProblemResponse::new(StatusCode::BAD_REQUEST, "invalid_timestamp", err)
    })?;

    let now = state.now();
    let skew = now.signed_duration_since(timestamp).num_seconds().abs();
    if skew > 600 {
        warn!(
            stage = "ingress",
            %message_id,
            %timestamp_raw,
            now = %now.to_rfc3339(),
            skew_seconds = skew,
            "timestamp outside ±10 minute window"
        );
        histogram!("webhook_ack_latency_seconds", "type" => message_label)
            .record(start.elapsed().as_secs_f64());
        return Err(ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "timestamp_out_of_range",
            "timestamp outside the allowed ±10 minute window",
        ));
    }

    let secret = state.webhook_secret();
    verify_signature(&secret, message_id, timestamp_raw, &body, signature).map_err(|err| {
        counter!("eventsub_invalid_signature_total", "type" => message_label).increment(1);
        histogram!("webhook_ack_latency_seconds", "type" => message_label)
            .record(start.elapsed().as_secs_f64());
        ProblemResponse::new(StatusCode::FORBIDDEN, "invalid_signature", err)
    })?;

    counter!("eventsub_ingress_total", "type" => message_label).increment(1);

    let body_len = body.len() as u64;
    let body_string = String::from_utf8(body.to_vec()).map_err(|_| {
        histogram!("webhook_ack_latency_seconds", "type" => message_label)
            .record(start.elapsed().as_secs_f64());
        ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "request body must be valid UTF-8",
        )
    })?;
    let json_value: Value = serde_json::from_str(&body_string).map_err(|err| {
        histogram!("webhook_ack_latency_seconds", "type" => message_label)
            .record(start.elapsed().as_secs_f64());
        ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("failed to parse payload: {err}"),
        )
    })?;

    let response = match message_type {
        MessageType::Verification => {
            let subscription = json_value.get("subscription");
            let event_type = subscription
                .and_then(|sub| sub.get("type").and_then(Value::as_str))
                .unwrap_or(message_label);
            let broadcaster = subscription
                .and_then(|sub| sub.get("condition"))
                .and_then(Value::as_object)
                .and_then(|cond| cond.get("broadcaster_user_id"))
                .and_then(Value::as_str);

            let challenge = json_value
                .get("challenge")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    histogram!("webhook_ack_latency_seconds", "type" => message_label)
                        .record(start.elapsed().as_secs_f64());
                    ProblemResponse::new(
                        StatusCode::BAD_REQUEST,
                        "missing_challenge",
                        "verification payload must include challenge",
                    )
                })?;

            emit_tap(TapPublish {
                state: &state,
                message_id,
                broadcaster_id: broadcaster,
                event_type,
                message_label,
                body_len,
                elapsed_secs: start.elapsed().as_secs_f64(),
                received_at: state.now(),
                duplicate: false,
                status: StatusCode::OK,
            });

            let response = Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/plain")
                .body(challenge.to_string().into())
                .unwrap();
            histogram!("webhook_ack_latency_seconds", "type" => message_label)
                .record(start.elapsed().as_secs_f64());
            return Ok(response);
        }
        MessageType::Notification | MessageType::Revocation => {
            match handle_persisted_message(
                &state,
                &json_value,
                &body_string,
                message_id,
                timestamp,
                message_label,
                start,
            )
            .await
            {
                Ok(response) => {
                    histogram!("webhook_ack_latency_seconds", "type" => message_label)
                        .record(start.elapsed().as_secs_f64());
                    response
                }
                Err(err) => {
                    histogram!("webhook_ack_latency_seconds", "type" => message_label)
                        .record(start.elapsed().as_secs_f64());
                    return Err(err);
                }
            }
        }
    };

    Ok(response)
}

async fn handle_persisted_message(
    state: &AppState,
    json_value: &Value,
    body_string: &str,
    message_id: &str,
    timestamp: DateTime<Utc>,
    message_label: &'static str,
    start: Instant,
) -> Result<Response, ProblemResponse> {
    let subscription = json_value.get("subscription").ok_or_else(|| {
        ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "missing_subscription",
            "payload missing subscription block",
        )
    })?;
    let event_type = subscription
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProblemResponse::new(
                StatusCode::BAD_REQUEST,
                "missing_event_type",
                "subscription.type is required",
            )
        })?;

    let broadcaster_id = subscription
        .get("condition")
        .and_then(Value::as_object)
        .and_then(|cond| cond.get("broadcaster_user_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            json_value
                .get("event")
                .and_then(|event| event.get("broadcaster_user_id"))
                .and_then(Value::as_str)
        })
        .ok_or_else(|| {
            ProblemResponse::new(
                StatusCode::BAD_REQUEST,
                "missing_broadcaster",
                "unable to resolve broadcaster id from payload",
            )
        })?;

    let repo = state.storage().event_raw();
    let received_at = state.now();
    let record = NewEventRaw {
        id: Cow::Owned(Uuid::new_v4().to_string()),
        broadcaster_id: Cow::Borrowed(broadcaster_id),
        msg_id: Cow::Borrowed(message_id),
        event_type: Cow::Borrowed(event_type),
        payload_json: Cow::Borrowed(body_string),
        event_at: timestamp,
        received_at,
        source: "webhook",
    };

    let insert_outcome = repo.insert(record).await.map_err(|err| match err {
        EventRawError::MissingBroadcaster => {
            error!(stage = "ingress", %message_id, broadcaster_id, "broadcaster missing in database");
            ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "missing_broadcaster",
                "broadcaster is not provisioned for webhook ingress",
            )
        }
        EventRawError::Database(db_err) => {
            error!(stage = "ingress", %message_id, error = %db_err, "failed to persist event raw");
            ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                "failed to persist webhook payload",
            )
        }
    })?;

    let duplicate = matches!(insert_outcome, EventRawInsertOutcome::Duplicate);
    if duplicate {
        info!(stage = "ingress", %message_id, broadcaster_id, "duplicate webhook message skipped");
    }

    emit_tap(TapPublish {
        state,
        message_id,
        broadcaster_id: Some(broadcaster_id),
        event_type,
        message_label,
        body_len: body_string.len() as u64,
        elapsed_secs: start.elapsed().as_secs_f64(),
        received_at,
        duplicate,
        status: StatusCode::NO_CONTENT,
    });

    if !duplicate {
        process_pipeline(
            state,
            json_value,
            body_string.len() as u64,
            event_type,
            broadcaster_id,
            message_id,
        )
        .await;
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(axum::body::Body::empty())
        .unwrap())
}

async fn process_pipeline(
    state: &AppState,
    json_value: &Value,
    body_len: u64,
    event_type: &str,
    broadcaster_id: &str,
    message_id: &str,
) {
    let normalized = match normalize_payload(
        state,
        json_value,
        body_len,
        event_type,
        broadcaster_id,
        message_id,
    ) {
        Ok(event) => event,
        Err(_) => return,
    };

    let profile = match state
        .storage()
        .broadcasters()
        .fetch_settings(broadcaster_id)
        .await
    {
        Ok(settings) => settings,
        Err(err) => {
            emit_policy_error(state, broadcaster_id, &normalized, err);
            return;
        }
    };

    let outcome = evaluate_policy(state, broadcaster_id, &normalized, &profile.settings);

    if !outcome.commands.is_empty() {
        dispatch_commands(
            state,
            broadcaster_id,
            &profile,
            &outcome.commands,
            &normalized,
        )
        .await;
    }
}

fn normalize_payload(
    state: &AppState,
    json_value: &Value,
    body_len: u64,
    event_type: &str,
    broadcaster_id: &str,
    message_id: &str,
) -> Result<NormalizedEvent, ()> {
    let start = Instant::now();
    let normalized = match Normalizer::normalize(event_type, json_value) {
        Ok(event) => event,
        Err(err) => {
            let context = NormalizerErrorContext {
                json_value,
                event_type,
                broadcaster_id,
                message_id,
                body_len,
                latency_ms: start.elapsed().as_secs_f64() * 1000.0,
                error: err,
            };
            emit_normalizer_error(state, context);
            return Err(());
        }
    };

    emit_normalizer_stage(
        state,
        json_value,
        &normalized,
        broadcaster_id,
        message_id,
        body_len,
        start.elapsed().as_secs_f64(),
    );

    Ok(normalized)
}

fn emit_normalizer_stage(
    state: &AppState,
    json_value: &Value,
    normalized: &NormalizedEvent,
    broadcaster_id: &str,
    message_id: &str,
    body_len: u64,
    elapsed_secs: f64,
) {
    let sanitized = sanitize_payload(json_value);
    let event = StageEvent {
        ts: state.now(),
        stage: StageKind::Normalizer,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: Some(broadcaster_id.to_string()),
        meta: StageMetadata {
            msg_id: Some(message_id.to_string()),
            event_type: Some(normalized.event_type().to_string()),
            size_bytes: Some(body_len),
            latency_ms: Some(elapsed_secs * 1000.0),
            ..StageMetadata::default()
        },
        r#in: StagePayload {
            redacted: true,
            payload: sanitized,
            truncated: None,
        },
        out: StagePayload {
            redacted: true,
            payload: normalized.redacted(),
            truncated: None,
        },
    };
    state.tap().publish(event);
}

fn emit_normalizer_error(state: &AppState, context: NormalizerErrorContext<'_>) {
    let NormalizerErrorContext {
        json_value,
        event_type,
        broadcaster_id,
        message_id,
        body_len,
        latency_ms,
        error: err,
    } = context;
    error!(
        stage = "normalizer",
        %message_id,
        broadcaster_id,
        error = %err,
        "failed to normalize payload"
    );

    let sanitized = sanitize_payload(json_value);
    let event = StageEvent {
        ts: state.now(),
        stage: StageKind::Normalizer,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: Some(broadcaster_id.to_string()),
        meta: StageMetadata {
            msg_id: Some(message_id.to_string()),
            event_type: Some(event_type.to_string()),
            size_bytes: Some(body_len),
            latency_ms: Some(latency_ms),
            message: Some("normalization_failed".to_string()),
            ..StageMetadata::default()
        },
        r#in: StagePayload {
            redacted: true,
            payload: sanitized,
            truncated: None,
        },
        out: StagePayload {
            redacted: true,
            payload: json!({ "error": err.to_string() }),
            truncated: None,
        },
    };
    state.tap().publish(event);
}
struct NormalizerErrorContext<'a> {
    json_value: &'a Value,
    event_type: &'a str,
    broadcaster_id: &'a str,
    message_id: &'a str,
    body_len: u64,
    latency_ms: f64,
    error: NormalizerError,
}

fn evaluate_policy(
    state: &AppState,
    broadcaster_id: &str,
    normalized: &NormalizedEvent,
    settings: &Settings,
) -> twi_overlay_core::policy::PolicyOutcome {
    let issued_at = state.now();
    let start = Instant::now();
    let outcome = state.policy().evaluate(settings, normalized, issued_at);
    let mut meta = StageMetadata {
        event_type: Some(normalized.event_type().to_string()),
        latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
        ..StageMetadata::default()
    };

    if outcome.is_duplicate() {
        meta.message = Some("duplicate".to_string());
    }

    let event = StageEvent {
        ts: state.now(),
        stage: StageKind::Policy,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: Some(broadcaster_id.to_string()),
        meta,
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
    state.tap().publish(event);

    for command in &outcome.commands {
        counter!("policy_commands_total", "kind" => command.metric_kind()).increment(1);
    }
    outcome
}

fn emit_policy_error(
    state: &AppState,
    broadcaster_id: &str,
    normalized: &NormalizedEvent,
    err: SettingsError,
) {
    error!(
        stage = "policy",
        broadcaster_id,
        error = %err,
        "failed to load broadcaster settings"
    );

    let event = StageEvent {
        ts: state.now(),
        stage: StageKind::Policy,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: Some(broadcaster_id.to_string()),
        meta: StageMetadata {
            event_type: Some(normalized.event_type().to_string()),
            message: Some("settings_error".to_string()),
            ..StageMetadata::default()
        },
        r#in: StagePayload {
            redacted: true,
            payload: normalized.redacted(),
            truncated: None,
        },
        out: StagePayload {
            redacted: true,
            payload: json!({ "error": err.to_string() }),
            truncated: None,
        },
    };
    state.tap().publish(event);
}

async fn dispatch_commands(
    state: &AppState,
    broadcaster_id: &str,
    profile: &BroadcasterSettings,
    commands: &[Command],
    normalized: &NormalizedEvent,
) {
    match state
        .command_executor()
        .execute(broadcaster_id, &profile.timezone, commands)
        .await
    {
        Ok(patches) => {
            for patch in patches {
                if let Err(err) = state
                    .sse()
                    .broadcast_patch(broadcaster_id, &patch, state.now())
                    .await
                {
                    error!(
                        stage = "sse",
                        broadcaster_id,
                        error = %err,
                        "failed to broadcast patch"
                    );
                    continue;
                }
                emit_sse_stage(state, broadcaster_id, &patch);
            }
        }
        Err(err) => {
            error!(
                stage = "command",
                broadcaster_id,
                error = %err,
                event_type = normalized.event_type(),
                "failed to execute commands"
            );
        }
    }
}

pub(crate) fn emit_sse_stage(state: &AppState, broadcaster_id: &str, patch: &Patch) {
    let latency = state
        .now()
        .signed_duration_since(patch.at)
        .num_milliseconds() as f64;
    let event = StageEvent {
        ts: state.now(),
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
    state.tap().publish(event);
}

fn sanitize_payload(value: &Value) -> Value {
    let mut sanitized = serde_json::Map::new();
    if let Some(subscription_type) = value
        .get("subscription")
        .and_then(|sub| sub.get("type"))
        .and_then(Value::as_str)
    {
        sanitized.insert(
            "subscription_type".to_string(),
            Value::String(subscription_type.to_string()),
        );
    }
    if let Some(event) = value.get("event").and_then(Value::as_object) {
        if let Some(id) = event.get("id") {
            sanitized.insert("event_id".to_string(), id.clone());
        }
        if let Some(user_id) = event.get("user_id") {
            sanitized.insert("user_id".to_string(), user_id.clone());
        }
        if let Some(redeemed_at) = event.get("redeemed_at") {
            sanitized.insert("occurred_at".to_string(), redeemed_at.clone());
        }
        if let Some(reward_id) = event
            .get("reward")
            .and_then(Value::as_object)
            .and_then(|reward| reward.get("id"))
        {
            sanitized.insert("reward_id".to_string(), reward_id.clone());
        }
    }
    Value::Object(sanitized)
}

struct TapPublish<'a> {
    state: &'a AppState,
    message_id: &'a str,
    broadcaster_id: Option<&'a str>,
    event_type: &'a str,
    message_label: &'static str,
    body_len: u64,
    elapsed_secs: f64,
    received_at: DateTime<Utc>,
    duplicate: bool,
    status: StatusCode,
}

fn emit_tap(ctx: TapPublish<'_>) {
    let meta = StageMetadata {
        msg_id: Some(ctx.message_id.to_string()),
        event_type: Some(ctx.event_type.to_string()),
        size_bytes: Some(ctx.body_len),
        latency_ms: Some(ctx.elapsed_secs * 1000.0),
        ..StageMetadata::default()
    };

    let event = StageEvent {
        ts: ctx.received_at,
        stage: StageKind::Ingress,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: ctx.broadcaster_id.map(|id| id.to_string()),
        meta,
        r#in: StagePayload {
            redacted: true,
            payload: Value::Null,
            truncated: None,
        },
        out: StagePayload {
            redacted: false,
            payload: json!({
                "status": ctx.status.as_u16(),
                "type": ctx.message_label,
                "duplicate": ctx.duplicate,
            }),
            truncated: None,
        },
    };

    ctx.state.tap().publish(event);
}
fn get_required_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ProblemResponse> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ProblemResponse::new(
                StatusCode::BAD_REQUEST,
                "missing_header",
                format!("missing header {name}"),
            )
        })
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| format!("invalid RFC3339 timestamp: {err}"))
}

fn verify_signature(
    secret: &Arc<[u8]>,
    message_id: &str,
    timestamp: &str,
    body: &[u8],
    provided: &str,
) -> Result<(), String> {
    let hex_part = provided
        .strip_prefix("sha256=")
        .ok_or_else(|| "signature must start with 'sha256='".to_string())?;
    let provided_bytes =
        hex::decode(hex_part).map_err(|_| "signature is not valid hex".to_string())?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| "failed to initialize signature verifier".to_string())?;
    mac.update(message_id.as_bytes());
    mac.update(timestamp.as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    let expected_bytes: &[u8] = expected.as_ref();

    if expected_bytes.ct_eq(provided_bytes.as_slice()).into() {
        Ok(())
    } else {
        Err("signature mismatch".to_string())
    }
}

#[derive(Debug, Clone, Copy)]
enum MessageType {
    Verification,
    Notification,
    Revocation,
}

impl TryFrom<&str> for MessageType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "webhook_callback_verification" => Ok(Self::Verification),
            "notification" => Ok(Self::Notification),
            "revocation" => Ok(Self::Revocation),
            other => Err(format!("unsupported message type: {other}")),
        }
    }
}

impl MessageType {
    fn metric_label(self) -> &'static str {
        match self {
            Self::Verification => "verification",
            Self::Notification => "notification",
            Self::Revocation => "revocation",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{HeaderMap, HeaderValue, Method, Request, StatusCode},
    };
    use chrono::{Duration, SecondsFormat};
    use http_body_util::BodyExt;
    use sqlx::{query, query_scalar, Row};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
    use tower::ServiceExt;

    use crate::{router::app_router, telemetry};
    use twi_overlay_storage::Database;

    use reqwest::Client;
    use serde_json::{json, Value};
    use twi_overlay_twitch::{HelixClient, TwitchOAuthClient};
    use url::Url;

    const BROADCASTER_ID: &str = "b-123";
    const FIXED_NOW: &str = "2024-01-01T00:00:00Z";

    struct TestContext {
        state: AppState,
        database: Database,
        secret: String,
        now: DateTime<Utc>,
    }

    async fn setup_context() -> TestContext {
        let metrics = telemetry::init_metrics().expect("metrics init");
        let tap = crate::tap::TapHub::new();
        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        let now = DateTime::parse_from_rfc3339(FIXED_NOW)
            .expect("fixed time")
            .with_timezone(&Utc);

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
        .bind("Test Broadcaster")
        .bind("UTC")
        .bind(settings.to_string())
        .bind(now.to_rfc3339_opts(SecondsFormat::Secs, true))
        .bind(now.to_rfc3339_opts(SecondsFormat::Secs, true))
        .execute(database.pool())
        .await
        .expect("insert broadcaster");

        let secret = "test-secret".to_string();
        let secret_arc: Arc<[u8]> = Arc::from(secret.clone().into_bytes().into_boxed_slice());
        let fixed_now = now;
        let clock = Arc::new(move || fixed_now);
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
            secret_arc,
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
        let state = state.with_clock(clock);

        TestContext {
            state,
            database,
            secret,
            now,
        }
    }

    fn sign(secret: &str, message_id: &str, timestamp: &str, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac");
        mac.update(message_id.as_bytes());
        mac.update(timestamp.as_bytes());
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn headers(
        message_type: &str,
        message_id: &str,
        timestamp: &str,
        signature: &str,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_MESSAGE_TYPE,
            HeaderValue::from_str(message_type).expect("type header"),
        );
        headers.insert(
            HEADER_MESSAGE_ID,
            HeaderValue::from_str(message_id).expect("id header"),
        );
        headers.insert(
            HEADER_TIMESTAMP,
            HeaderValue::from_str(timestamp).expect("timestamp header"),
        );
        headers.insert(
            HEADER_SIGNATURE,
            HeaderValue::from_str(signature).expect("signature header"),
        );
        headers
    }

    async fn call_webhook(state: AppState, headers: HeaderMap, body: String) -> Response {
        let mut request_headers = headers;
        request_headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/eventsub/webhook")
            .body(Body::from(body))
            .expect("request");
        *request.headers_mut() = request_headers;

        let app = app_router(state);
        app.oneshot(request).await.expect("response")
    }

    fn notification_body() -> String {
        json!({
            "subscription": {
                "type": "channel.channel_points_custom_reward_redemption.add",
                "version": "1",
                "condition": {"broadcaster_user_id": BROADCASTER_ID}
            },
            "event": {
                "broadcaster_user_id": BROADCASTER_ID,
                "id": "event-1",
                "user_id": "user-1",
                "user_login": "viewer_one",
                "user_name": "Viewer One",
                "status": "UNFULFILLED",
                "redeemed_at": FIXED_NOW,
                "reward": {
                    "id": "reward-1",
                    "title": "Join Queue",
                    "cost": 1
                }
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn verification_returns_challenge() {
        let ctx = setup_context().await;
        let body = serde_json::json!({
            "challenge": "TEST",
            "subscription": {
                "type": "channel.channel_points_custom_reward_redemption.add",
                "condition": {"broadcaster_user_id": BROADCASTER_ID},
                "version": "1"
            }
        })
        .to_string();

        let timestamp = ctx.now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign(&ctx.secret, "msg-verification", &timestamp, &body);
        let headers = headers(
            "webhook_callback_verification",
            "msg-verification",
            &timestamp,
            &signature,
        );

        let response = call_webhook(ctx.state.clone(), headers, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = response.into_body().collect().await.expect("body");
        assert_eq!(body_bytes.to_bytes(), &b"TEST"[..]);

        let count: i64 = query_scalar("SELECT COUNT(*) FROM event_raw")
            .fetch_one(ctx.database.pool())
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn notification_persists_payload_and_emits_tap() {
        let ctx = setup_context().await;
        let body = notification_body();
        let timestamp = ctx.now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let message_id = "msg-1";
        let signature = sign(&ctx.secret, message_id, &timestamp, &body);
        let headers = headers("notification", message_id, &timestamp, &signature);
        let tap = ctx.state.tap().clone();
        let mut receiver = tap.subscribe();

        let response = call_webhook(ctx.state.clone(), headers, body.clone()).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let record = query("SELECT msg_id, type, payload_json FROM event_raw")
            .fetch_one(ctx.database.pool())
            .await
            .expect("record");
        assert_eq!(record.get::<String, _>("msg_id"), message_id);
        assert!(record
            .get::<String, _>("payload_json")
            .contains("channel.channel_points_custom_reward_redemption.add"));

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), receiver.recv())
            .await
            .expect("tap event available")
            .expect("event value");
        assert_eq!(event.stage, StageKind::Ingress);
        assert_eq!(event.meta.msg_id.as_deref(), Some(message_id));

        let normalizer =
            tokio::time::timeout(std::time::Duration::from_millis(200), receiver.recv())
                .await
                .expect("normalizer event available")
                .expect("event value");
        assert_eq!(normalizer.stage, StageKind::Normalizer);
        assert_eq!(
            normalizer.meta.event_type.as_deref(),
            Some("redemption.add")
        );

        let policy = tokio::time::timeout(std::time::Duration::from_millis(200), receiver.recv())
            .await
            .expect("policy event available")
            .expect("event value");
        assert_eq!(policy.stage, StageKind::Policy);
        assert_eq!(policy.meta.event_type.as_deref(), Some("redemption.add"));
        let commands = policy
            .out
            .payload
            .get("commands")
            .and_then(Value::as_array)
            .expect("commands array");
        assert_eq!(commands.len(), 2);
    }

    #[tokio::test]
    async fn duplicate_notification_is_idempotent() {
        let ctx = setup_context().await;
        let body = notification_body();
        let timestamp = ctx.now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let message_id = "msg-dup";
        let signature = sign(&ctx.secret, message_id, &timestamp, &body);
        let headers = headers("notification", message_id, &timestamp, &signature);

        let response = call_webhook(ctx.state.clone(), headers.clone(), body.clone()).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = call_webhook(ctx.state.clone(), headers, body).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let count: i64 = query_scalar("SELECT COUNT(*) FROM event_raw")
            .fetch_one(ctx.database.pool())
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn rejects_invalid_signature() {
        let ctx = setup_context().await;
        let body = notification_body();
        let timestamp = ctx.now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let headers = headers("notification", "msg-bad", &timestamp, "sha256=deadbeef");

        let response = call_webhook(ctx.state.clone(), headers, body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_timestamp_outside_window() {
        let ctx = setup_context().await;
        let body = notification_body();
        let timestamp =
            (ctx.now - Duration::minutes(11)).to_rfc3339_opts(SecondsFormat::Millis, true);
        let signature = sign(&ctx.secret, "msg-skew", &timestamp, &body);
        let headers = headers("notification", "msg-skew", &timestamp, &signature);

        let response = call_webhook(ctx.state.clone(), headers, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
