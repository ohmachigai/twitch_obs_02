use std::{collections::HashSet, time::Duration};

use axum::response::sse::{Event, KeepAlive};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StageKind {
    Ingress,
    Normalizer,
    Policy,
    Command,
    Projector,
    Sse,
    Storage,
    Oauth,
}

impl StageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StageKind::Ingress => "ingress",
            StageKind::Normalizer => "normalizer",
            StageKind::Policy => "policy",
            StageKind::Command => "command",
            StageKind::Projector => "projector",
            StageKind::Sse => "sse",
            StageKind::Storage => "storage",
            StageKind::Oauth => "oauth",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct StageMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StagePayload {
    pub redacted: bool,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

impl Default for StagePayload {
    fn default() -> Self {
        Self {
            redacted: false,
            payload: Value::Null,
            truncated: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StageEvent {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub stage: StageKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcaster_id: Option<String>,
    #[serde(default, skip_serializing_if = "StageMetadata::is_empty")]
    pub meta: StageMetadata,
    pub r#in: StagePayload,
    pub out: StagePayload,
}

impl StageMetadata {
    fn is_empty(&self) -> bool {
        self.msg_id.is_none()
            && self.event_type.is_none()
            && self.size_bytes.is_none()
            && self.latency_ms.is_none()
            && self.thread.is_none()
            && self.message.is_none()
    }
}

impl StageEvent {
    pub fn event_name(&self) -> &'static str {
        self.stage.as_str()
    }

    pub fn into_sse_event(self) -> Result<Event, serde_json::Error> {
        let mut event = Event::default().event(self.event_name());
        if let Some(version) = self.version {
            event = event.id(version.to_string());
        }
        let data = serde_json::to_string(&self)?;
        Ok(event.data(data))
    }

    pub fn mock(message: &str) -> Self {
        Self {
            ts: chrono::Utc::now(),
            stage: StageKind::Sse,
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
                payload: json!({ "message": message }),
                truncated: None,
            },
        }
    }
}

#[derive(Clone)]
pub struct TapHub {
    sender: broadcast::Sender<StageEvent>,
}

impl TapHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(128);
        Self { sender }
    }

    pub fn publish(&self, event: StageEvent) {
        if let Err(err) = self.sender.send(event) {
            warn!(stage = "sse", error = %err, "failed to broadcast tap event");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StageEvent> {
        self.sender.subscribe()
    }

    pub fn spawn_mock_publisher(&self) {
        let hub = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                hub.publish(StageEvent::mock("tap.dev.heartbeat"));
            }
        });
    }
}

#[derive(Debug, Clone, Default)]
pub struct TapFilter {
    stages: Option<HashSet<StageKind>>,
}

impl TapFilter {
    pub fn from_stages(stages: Option<HashSet<StageKind>>) -> Self {
        Self { stages }
    }

    pub fn matches(&self, event: &StageEvent) -> bool {
        match &self.stages {
            Some(stages) => stages.contains(&event.stage),
            None => true,
        }
    }
}

pub fn tap_stream(
    hub: TapHub,
    filter: TapFilter,
) -> impl Stream<Item = Result<Event, serde_json::Error>> + Send + 'static {
    BroadcastStream::new(hub.subscribe()).filter_map(move |result| {
        let filter = filter.clone();
        match result {
            Ok(event) if filter.matches(&event) => Some(event.into_sse_event()),
            Ok(_) => None,
            Err(_) => None,
        }
    })
}

pub fn tap_keep_alive() -> KeepAlive {
    KeepAlive::new()
        .interval(Duration::from_secs(20))
        .text("heartbeat")
}

pub fn parse_stage_list(value: Option<String>) -> Result<Option<HashSet<StageKind>>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let mut set = HashSet::new();
    for item in raw.split(',').filter(|s| !s.is_empty()) {
        let stage = match item.trim().to_lowercase().as_str() {
            "ingress" => StageKind::Ingress,
            "normalizer" => StageKind::Normalizer,
            "policy" => StageKind::Policy,
            "command" => StageKind::Command,
            "projector" => StageKind::Projector,
            "sse" => StageKind::Sse,
            "storage" => StageKind::Storage,
            "oauth" => StageKind::Oauth,
            other => {
                return Err(format!("unknown stage '{other}'"));
            }
        };
        set.insert(stage);
    }

    if set.is_empty() {
        Ok(None)
    } else {
        Ok(Some(set))
    }
}
