use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::types::{NormalizedEvent, NormalizedRedemptionStatus, NormalizedReward, NormalizedUser};

/// Errors that can occur during normalization of incoming webhook payloads.
#[derive(Debug, Error)]
pub enum NormalizerError {
    #[error("unsupported event type: {0}")]
    UnsupportedEventType(String),
    #[error("missing event block in payload")]
    MissingEvent,
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("failed to parse payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid timestamp for field '{field}': {source}")]
    InvalidTimestamp {
        field: &'static str,
        source: chrono::ParseError,
    },
}

/// Deterministic normalizer transforming EventSub JSON into [`NormalizedEvent`] values.
pub struct Normalizer;

impl Normalizer {
    /// Converts a raw EventSub payload into a [`NormalizedEvent`].
    pub fn normalize(
        event_type: &str,
        payload: &Value,
    ) -> Result<NormalizedEvent, NormalizerError> {
        match event_type {
            "channel.channel_points_custom_reward_redemption.add" => {
                Self::normalize_redemption(payload, false)
            }
            "channel.channel_points_custom_reward_redemption.update" => {
                Self::normalize_redemption(payload, true)
            }
            "stream.online" => Self::normalize_stream(payload, true),
            "stream.offline" => Self::normalize_stream(payload, false),
            other => Err(NormalizerError::UnsupportedEventType(other.to_string())),
        }
    }

    fn normalize_redemption(
        payload: &Value,
        include_status: bool,
    ) -> Result<NormalizedEvent, NormalizerError> {
        let data: RedemptionPayload = serde_json::from_value(payload.clone())?;
        let event = data.event.ok_or(NormalizerError::MissingEvent)?;

        let redeemed_at = DateTime::parse_from_rfc3339(&event.redeemed_at)
            .map_err(|source| NormalizerError::InvalidTimestamp {
                field: "redeemed_at",
                source,
            })?
            .with_timezone(&Utc);

        let user = NormalizedUser {
            id: event.user_id,
            login: event.user_login,
            display_name: event.user_name,
        };
        let reward = NormalizedReward {
            id: event.reward.id,
            title: event.reward.title,
            cost: event.reward.cost,
        };

        if include_status {
            let status = map_status(event.status.unwrap_or_else(|| "unknown".to_string()));
            Ok(NormalizedEvent::RedemptionUpdate {
                broadcaster_id: event.broadcaster_user_id,
                occurred_at: redeemed_at,
                redemption_id: event.id,
                status,
                user,
                reward,
            })
        } else {
            Ok(NormalizedEvent::RedemptionAdd {
                broadcaster_id: event.broadcaster_user_id,
                occurred_at: redeemed_at,
                redemption_id: event.id,
                user,
                reward,
            })
        }
    }

    fn normalize_stream(payload: &Value, online: bool) -> Result<NormalizedEvent, NormalizerError> {
        let data: StreamPayload = serde_json::from_value(payload.clone())?;
        let event = data.event.ok_or(NormalizerError::MissingEvent)?;

        let (timestamp_field, raw_time) = if online {
            (
                "started_at",
                event
                    .started_at
                    .ok_or(NormalizerError::MissingField("started_at"))?,
            )
        } else {
            (
                "ended_at",
                event
                    .ended_at
                    .ok_or(NormalizerError::MissingField("ended_at"))?,
            )
        };

        let occurred_at = DateTime::parse_from_rfc3339(&raw_time)
            .map_err(|source| NormalizerError::InvalidTimestamp {
                field: timestamp_field,
                source,
            })?
            .with_timezone(&Utc);

        if online {
            Ok(NormalizedEvent::StreamOnline {
                broadcaster_id: event.broadcaster_user_id,
                occurred_at,
            })
        } else {
            Ok(NormalizedEvent::StreamOffline {
                broadcaster_id: event.broadcaster_user_id,
                occurred_at,
            })
        }
    }
}

fn map_status(value: String) -> NormalizedRedemptionStatus {
    match value.as_str() {
        "UNFULFILLED" | "PENDING" => NormalizedRedemptionStatus::Pending,
        "FULFILLED" => NormalizedRedemptionStatus::Fulfilled,
        "CANCELED" | "CANCELLED" => NormalizedRedemptionStatus::Canceled,
        other => NormalizedRedemptionStatus::Unknown(other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct RedemptionPayload {
    event: Option<RedemptionEvent>,
}

#[derive(Debug, Deserialize)]
struct RedemptionEvent {
    id: String,
    broadcaster_user_id: String,
    user_id: String,
    #[serde(default)]
    user_login: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    redeemed_at: String,
    reward: RedemptionReward,
}

#[derive(Debug, Deserialize)]
struct RedemptionReward {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cost: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StreamPayload {
    event: Option<StreamEvent>,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    broadcaster_user_id: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redemption_add_is_deterministic() {
        let payload = sample_redemption_payload();
        let first = Normalizer::normalize(
            "channel.channel_points_custom_reward_redemption.add",
            &payload,
        )
        .expect("normalize add");
        let second = Normalizer::normalize(
            "channel.channel_points_custom_reward_redemption.add",
            &payload,
        )
        .expect("normalize add");

        assert_eq!(first, second);
        assert!(matches!(first, NormalizedEvent::RedemptionAdd { .. }));
    }

    #[test]
    fn redemption_update_includes_status() {
        let payload = sample_redemption_payload();
        let normalized = Normalizer::normalize(
            "channel.channel_points_custom_reward_redemption.update",
            &payload,
        )
        .expect("normalize update");

        match normalized {
            NormalizedEvent::RedemptionUpdate { status, .. } => {
                assert_eq!(status, NormalizedRedemptionStatus::Pending);
            }
            other => panic!("expected redemption.update, got {other:?}"),
        }
    }

    fn sample_redemption_payload() -> Value {
        json!({
            "event": {
                "id": "redemption-1",
                "broadcaster_user_id": "b-1",
                "user_id": "u-1",
                "user_login": "viewer",
                "user_name": "Viewer",
                "status": "UNFULFILLED",
                "redeemed_at": "2024-01-01T00:00:00Z",
                "reward": {
                    "id": "reward-1",
                    "title": "Join",
                    "cost": 123
                }
            }
        })
    }
}
