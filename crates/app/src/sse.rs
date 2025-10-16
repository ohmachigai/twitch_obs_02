use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::Infallible,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::response::sse::Event;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, DecodingKey, Validation};
use metrics::{counter, gauge, histogram};
use serde::{Deserialize, Serialize};
use serde_json::to_string;
use thiserror::Error;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};

use twi_overlay_core::projector::Projector;
use twi_overlay_core::types::Patch;
use twi_overlay_storage::{BroadcasterSettings, Database, StateIndexError};

use crate::command::CommandExecutorError;
use crate::state::{build_state_snapshot, StateError, StateScope};

const EVENT_NAME: &str = "patch";
const BROADCAST_BUFFER: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Audience {
    Overlay,
    Admin,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Overlay => "overlay",
            Audience::Admin => "admin",
        }
    }
}

#[derive(Clone)]
pub struct SseTokenValidator {
    decoding_key: DecodingKey,
    validation: Validation,
}

impl SseTokenValidator {
    pub fn new(secret: Vec<u8>) -> Self {
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_aud = false;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        Self {
            decoding_key: DecodingKey::from_secret(&secret),
            validation,
        }
    }

    pub fn validate(
        &self,
        token: &str,
        expected_audience: Audience,
        broadcaster_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), TokenError> {
        let claims = self.decode_claims(token)?;
        self.validate_claims(&claims, broadcaster_id, now)?;
        if claims.aud != expected_audience.as_str() {
            return Err(TokenError::Invalid("audience_mismatch".to_string()));
        }
        Ok(())
    }

    pub fn validate_any(
        &self,
        token: &str,
        audiences: &[Audience],
        broadcaster_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Audience, TokenError> {
        let claims = self.decode_claims(token)?;
        self.validate_claims(&claims, broadcaster_id, now)?;
        let matched = audiences
            .iter()
            .copied()
            .find(|aud| claims.aud == aud.as_str())
            .ok_or_else(|| TokenError::Invalid("audience_mismatch".to_string()))?;
        Ok(matched)
    }

    fn decode_claims(&self, token: &str) -> Result<TokenClaims, TokenError> {
        let data = decode::<TokenClaims>(token, &self.decoding_key, &self.validation)
            .map_err(|err| TokenError::Invalid(format!("{err}")))?;
        Ok(data.claims)
    }

    fn validate_claims(
        &self,
        claims: &TokenClaims,
        broadcaster_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), TokenError> {
        if claims.sub != broadcaster_id {
            return Err(TokenError::Invalid("subject_mismatch".to_string()));
        }
        let now_ts = now.timestamp();
        if let Some(nbf) = claims.nbf {
            if now_ts < nbf as i64 {
                return Err(TokenError::Invalid("token_not_yet_valid".to_string()));
            }
        }
        if now_ts >= claims.exp as i64 {
            return Err(TokenError::Invalid("token_expired".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TokenClaims {
    pub sub: String,
    pub aud: String,
    pub exp: usize,
    #[serde(default)]
    pub nbf: Option<usize>,
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("invalid token: {0}")]
    Invalid(String),
}

#[derive(Clone)]
pub struct SseHub {
    database: Database,
    channels: Arc<RwLock<HashMap<ChannelKey, Arc<Channel>>>>,
    ring_max: usize,
    ring_ttl: Duration,
    counters: Arc<ClientCounters>,
}

impl SseHub {
    pub fn new(database: Database, ring_max: usize, ring_ttl: Duration) -> Self {
        Self {
            database,
            channels: Arc::new(RwLock::new(HashMap::new())),
            ring_max,
            ring_ttl,
            counters: Arc::new(ClientCounters::new()),
        }
    }

    async fn ensure_channel(&self, broadcaster_id: &str, audience: Audience) -> Arc<Channel> {
        let key = ChannelKey::new(broadcaster_id, audience);
        let mut guard = self.channels.write().await;
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(Channel::new()))
            .clone()
    }

    pub async fn broadcast_patch(
        &self,
        broadcaster_id: &str,
        patch: &Patch,
        now: DateTime<Utc>,
    ) -> Result<(), SseError> {
        let message = Arc::new(SseMessage::from_patch(patch)?);
        let latency = now.signed_duration_since(patch.at).num_milliseconds() as f64 / 1000.0;
        histogram!("sse_broadcast_latency_seconds", "type" => patch.kind_str()).record(latency);

        for audience in [Audience::Overlay, Audience::Admin] {
            let channel = self.ensure_channel(broadcaster_id, audience).await;
            {
                let mut ring = channel.ring.lock().await;
                ring.push_back(message.clone());
                while ring.len() > self.ring_max {
                    ring.pop_front();
                }
                while let Some(front) = ring.front() {
                    if front.created_at.elapsed() > self.ring_ttl {
                        ring.pop_front();
                    } else {
                        break;
                    }
                }
            }
            let _ = channel.sender.send(message.clone());
        }

        Ok(())
    }

    pub async fn subscribe(
        &self,
        broadcaster_id: &str,
        audience: Audience,
        since_version: Option<u64>,
        types: Option<HashSet<String>>,
    ) -> Subscription {
        let filter = types.map(Arc::new);
        let channel = self.ensure_channel(broadcaster_id, audience).await;

        let ring_snapshot = {
            let ring = channel.ring.lock().await;
            ring.iter().cloned().collect::<Vec<_>>()
        };
        let ring_miss = ring_snapshot
            .first()
            .map(|first| since_version.map(|v| v < first.version).unwrap_or(false))
            .unwrap_or(false);
        if ring_miss {
            counter!("sse_ring_miss_total", "aud" => audience.as_str()).increment(1);
        }

        let backlog = if ring_miss {
            Vec::new()
        } else {
            retain_backlog(filter.clone(), ring_snapshot, since_version)
        };

        let guard = ClientGuard::new(self.counters.clone(), audience);
        let receiver = BroadcastStream::new(channel.sender.subscribe());
        Subscription {
            backlog,
            receiver,
            filter,
            guard,
            ring_miss,
        }
    }

    pub async fn build_state_replace(
        &self,
        broadcaster_id: &str,
        profile: &BroadcasterSettings,
        now: DateTime<Utc>,
    ) -> Result<Patch, SseError> {
        let snapshot = build_state_snapshot(
            &self.database,
            broadcaster_id,
            profile,
            now,
            StateScope::Session,
        )
        .await
        .map_err(SseError::from)?;
        Ok(Projector::state_replace(snapshot.version, now, snapshot))
    }

    pub(crate) fn event_from_patch(&self, patch: &Patch) -> Result<Arc<SseMessage>, SseError> {
        Ok(Arc::new(SseMessage::from_patch(patch)?))
    }
}

fn retain_backlog(
    filter: Option<Arc<HashSet<String>>>,
    messages: Vec<Arc<SseMessage>>,
    since: Option<u64>,
) -> Vec<Arc<SseMessage>> {
    messages
        .into_iter()
        .filter(move |msg| since.map(|version| msg.version > version).unwrap_or(true))
        .filter(move |msg| {
            filter
                .as_ref()
                .map(|set| set.contains(msg.kind.as_str()))
                .unwrap_or(true)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChannelKey {
    broadcaster_id: String,
    audience: Audience,
}

impl ChannelKey {
    fn new(broadcaster_id: &str, audience: Audience) -> Self {
        Self {
            broadcaster_id: broadcaster_id.to_string(),
            audience,
        }
    }
}

struct Channel {
    sender: broadcast::Sender<Arc<SseMessage>>,
    ring: Mutex<VecDeque<Arc<SseMessage>>>,
}

impl Channel {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_BUFFER);
        Self {
            sender,
            ring: Mutex::new(VecDeque::new()),
        }
    }
}

pub struct Subscription {
    backlog: Vec<Arc<SseMessage>>,
    receiver: BroadcastStream<Arc<SseMessage>>,
    filter: Option<Arc<HashSet<String>>>,
    guard: ClientGuard,
    ring_miss: bool,
}

impl Subscription {
    pub fn ring_miss(&self) -> bool {
        self.ring_miss
    }

    pub fn into_stream(self) -> SseStream {
        self.into_stream_with_initial(Vec::new())
    }

    pub(crate) fn into_stream_with_initial(mut self, initial: Vec<Arc<SseMessage>>) -> SseStream {
        if !initial.is_empty() {
            self.backlog = initial;
        }

        if let Some(filter) = &self.filter {
            self.backlog
                .retain(|msg| filter.contains(msg.kind.as_str()));
        }

        let backlog_stream =
            tokio_stream::iter(self.backlog).map(|msg| Ok::<_, Infallible>(msg.to_event()));

        let filter_live = self.filter.clone();
        let live_stream = self.receiver.filter_map(move |result| match result {
            Ok(msg) => {
                let allow = filter_live
                    .as_ref()
                    .map(|set| set.contains(msg.kind.as_str()))
                    .unwrap_or(true);
                if allow {
                    Some(Ok(msg.to_event()))
                } else {
                    None
                }
            }
            Err(_) => None,
        });

        let stream = backlog_stream.chain(live_stream);
        SseStream {
            inner: Box::pin(stream),
            _guard: self.guard,
        }
    }
}

pub struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    _guard: ClientGuard,
}

impl Stream for SseStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

pub(crate) struct SseMessage {
    version: u64,
    kind: String,
    data: String,
    created_at: Instant,
}

impl SseMessage {
    fn from_patch(patch: &Patch) -> Result<Self, serde_json::Error> {
        let data = to_string(patch)?;
        Ok(Self {
            version: patch.version,
            kind: patch.kind_str().to_string(),
            data,
            created_at: Instant::now(),
        })
    }

    fn to_event(&self) -> Event {
        Event::default()
            .id(self.version.to_string())
            .event(EVENT_NAME)
            .data(self.data.clone())
    }
}

struct ClientCounters {
    overlay: AtomicUsize,
    admin: AtomicUsize,
}

impl ClientCounters {
    fn new() -> Self {
        Self {
            overlay: AtomicUsize::new(0),
            admin: AtomicUsize::new(0),
        }
    }

    fn increment(&self, audience: Audience) {
        let value = match audience {
            Audience::Overlay => self.overlay.fetch_add(1, Ordering::SeqCst) + 1,
            Audience::Admin => self.admin.fetch_add(1, Ordering::SeqCst) + 1,
        };
        gauge!("sse_clients", "aud" => audience.as_str()).set(value as f64);
    }

    fn decrement(&self, audience: Audience) {
        let value = match audience {
            Audience::Overlay => self
                .overlay
                .fetch_sub(1, Ordering::SeqCst)
                .saturating_sub(1),
            Audience::Admin => self.admin.fetch_sub(1, Ordering::SeqCst).saturating_sub(1),
        };
        gauge!("sse_clients", "aud" => audience.as_str()).set(value as f64);
    }
}

struct ClientGuard {
    counters: Arc<ClientCounters>,
    audience: Audience,
}

impl ClientGuard {
    fn new(counters: Arc<ClientCounters>, audience: Audience) -> Self {
        counters.increment(audience);
        Self { counters, audience }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.counters.decrement(self.audience);
    }
}

#[derive(Debug, Error)]
pub enum SseError {
    #[error("failed to serialize patch: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("storage error: {0}")]
    Storage(#[from] sqlx::Error),
    #[error("queue error: {0}")]
    Queue(#[from] twi_overlay_storage::QueueError),
    #[error("counter error: {0}")]
    Counter(#[from] twi_overlay_storage::DailyCounterError),
    #[error("command error: {0}")]
    Command(#[from] CommandExecutorError),
    #[error("state index error: {0}")]
    StateIndex(#[from] StateIndexError),
    #[error("state snapshot error: {0}")]
    State(#[from] StateError),
}
