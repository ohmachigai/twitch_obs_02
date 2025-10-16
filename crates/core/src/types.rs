use std::fmt;

use chrono::{DateTime, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};
use std::str::FromStr;

/// Application settings persisted for a broadcaster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_overlay_theme")]
    pub overlay_theme: String,
    #[serde(default = "default_group_size")]
    pub group_size: u32,
    #[serde(default)]
    pub clear_on_stream_start: bool,
    #[serde(default)]
    pub clear_decrement_counts: bool,
    #[serde(default)]
    pub policy: PolicySettings,
}

/// Representation of a queue entry persisted for a broadcaster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub broadcaster_id: String,
    pub user_id: String,
    pub user_login: String,
    pub user_display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_avatar: Option<String>,
    pub reward_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redemption_id: Option<String>,
    pub enqueued_at: DateTime<Utc>,
    pub status: QueueEntryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    pub managed: bool,
    pub last_updated_at: DateTime<Utc>,
}

/// Queue entry status persisted in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueueEntryStatus {
    Queued,
    Completed,
    Removed,
}

impl QueueEntryStatus {
    /// Returns the canonical database representation for the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Completed => "COMPLETED",
            Self::Removed => "REMOVED",
        }
    }
}

fn default_overlay_theme() -> String {
    "default".to_string()
}

fn default_group_size() -> u32 {
    1
}

/// Policy specific settings that control the queue behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySettings {
    #[serde(default = "PolicySettings::default_window_sec")]
    pub anti_spam_window_sec: u64,
    #[serde(default)]
    pub duplicate_policy: DuplicatePolicy,
    #[serde(default)]
    pub target_rewards: Vec<String>,
}

impl PolicySettings {
    fn default_window_sec() -> u64 {
        60
    }

    /// Returns `true` when the provided reward identifier is enabled for policy evaluation.
    pub fn is_reward_enabled(&self, reward_id: &str) -> bool {
        self.target_rewards.iter().any(|value| value == reward_id)
    }
}

impl Default for PolicySettings {
    fn default() -> Self {
        Self {
            anti_spam_window_sec: Self::default_window_sec(),
            duplicate_policy: DuplicatePolicy::default(),
            target_rewards: Vec::new(),
        }
    }
}

/// Behaviour when a duplicate redemption is detected inside the spam window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicatePolicy {
    Consume,
    Refund,
}

impl Default for DuplicatePolicy {
    fn default() -> Self {
        Self::Consume
    }
}

impl Settings {
    /// Returns the policy configuration.
    pub fn policy(&self) -> &PolicySettings {
        &self.policy
    }
}

/// Deterministic representation of EventSub payloads used by the domain layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedEvent {
    #[serde(rename_all = "snake_case")]
    RedemptionAdd {
        broadcaster_id: String,
        occurred_at: DateTime<Utc>,
        redemption_id: String,
        user: NormalizedUser,
        reward: NormalizedReward,
    },
    #[serde(rename_all = "snake_case")]
    RedemptionUpdate {
        broadcaster_id: String,
        occurred_at: DateTime<Utc>,
        redemption_id: String,
        status: NormalizedRedemptionStatus,
        user: NormalizedUser,
        reward: NormalizedReward,
    },
    #[serde(rename_all = "snake_case")]
    StreamOnline {
        broadcaster_id: String,
        occurred_at: DateTime<Utc>,
    },
    #[serde(rename_all = "snake_case")]
    StreamOffline {
        broadcaster_id: String,
        occurred_at: DateTime<Utc>,
    },
}

impl NormalizedEvent {
    /// Returns the broadcaster associated with the event.
    pub fn broadcaster_id(&self) -> &str {
        match self {
            Self::RedemptionAdd { broadcaster_id, .. }
            | Self::RedemptionUpdate { broadcaster_id, .. }
            | Self::StreamOnline { broadcaster_id, .. }
            | Self::StreamOffline { broadcaster_id, .. } => broadcaster_id,
        }
    }

    /// Returns the canonical event type string used across telemetry.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::RedemptionAdd { .. } => "redemption.add",
            Self::RedemptionUpdate { .. } => "redemption.update",
            Self::StreamOnline { .. } => "stream.online",
            Self::StreamOffline { .. } => "stream.offline",
        }
    }

    /// Returns the occurrence timestamp of the event.
    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            Self::RedemptionAdd { occurred_at, .. }
            | Self::RedemptionUpdate { occurred_at, .. }
            | Self::StreamOnline { occurred_at, .. }
            | Self::StreamOffline { occurred_at, .. } => *occurred_at,
        }
    }

    /// Produces a redacted JSON representation suitable for Tap output.
    pub fn redacted(&self) -> Value {
        match self {
            Self::RedemptionAdd {
                broadcaster_id,
                occurred_at,
                redemption_id,
                user,
                reward,
            } => json!({
                "type": self.event_type(),
                "broadcaster_id": broadcaster_id,
                "occurred_at": occurred_at,
                "redemption_id": redemption_id,
                "user": user.redacted(),
                "reward": reward,
            }),
            Self::RedemptionUpdate {
                broadcaster_id,
                occurred_at,
                redemption_id,
                status,
                user,
                reward,
            } => json!({
                "type": self.event_type(),
                "broadcaster_id": broadcaster_id,
                "occurred_at": occurred_at,
                "redemption_id": redemption_id,
                "status": status,
                "user": user.redacted(),
                "reward": reward,
            }),
            Self::StreamOnline {
                broadcaster_id,
                occurred_at,
            }
            | Self::StreamOffline {
                broadcaster_id,
                occurred_at,
            } => json!({
                "type": self.event_type(),
                "broadcaster_id": broadcaster_id,
                "occurred_at": occurred_at,
            }),
        }
    }
}

/// Twitch redemption user information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedUser {
    pub id: String,
    pub login: Option<String>,
    pub display_name: Option<String>,
}

impl NormalizedUser {
    /// Redacts potentially sensitive user facing identifiers.
    pub fn redacted(&self) -> Value {
        json!({
            "id": self.id,
            "login": self.login.as_ref().map(|_| "***"),
            "display_name": self.display_name.as_ref().map(|_| "***"),
        })
    }
}

/// Twitch reward metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedReward {
    pub id: String,
    pub title: Option<String>,
    pub cost: Option<u64>,
}

/// Redemption status values emitted by Twitch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedRedemptionStatus {
    Pending,
    Fulfilled,
    Canceled,
    Unknown(String),
}

impl fmt::Display for NormalizedRedemptionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Fulfilled => write!(f, "fulfilled"),
            Self::Canceled => write!(f, "canceled"),
            Self::Unknown(value) => write!(f, "{value}"),
        }
    }
}

/// Commands generated by the policy engine.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Enqueue(EnqueueCommand),
    RedemptionUpdate(RedemptionUpdateCommand),
    QueueComplete(QueueCompleteCommand),
    QueueRemove(QueueRemoveCommand),
    SettingsUpdate(SettingsUpdateCommand),
}

impl Command {
    /// Returns the metrics label associated with the command.
    pub fn metric_kind(&self) -> &'static str {
        match self {
            Self::Enqueue(_) => "enqueue",
            Self::RedemptionUpdate(command) => match command.mode {
                RedemptionUpdateMode::Consume => "consume",
                RedemptionUpdateMode::Refund => "refund",
            },
            Self::QueueComplete(_) => "complete",
            Self::QueueRemove(_) => "undo",
            Self::SettingsUpdate(_) => "settings",
        }
    }

    /// Returns a redacted JSON representation of the command.
    pub fn redacted(&self) -> Value {
        match self {
            Self::Enqueue(command) => command.redacted(),
            Self::RedemptionUpdate(command) => command.redacted(),
            Self::QueueComplete(command) => command.redacted(),
            Self::QueueRemove(command) => command.redacted(),
            Self::SettingsUpdate(command) => command.redacted(),
        }
    }
}

/// Source of a generated command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    Policy,
    Admin,
}

/// Queue enqueue command emitted by the policy stage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnqueueCommand {
    pub broadcaster_id: String,
    pub issued_at: DateTime<Utc>,
    pub source: CommandSource,
    pub user: NormalizedUser,
    pub reward: NormalizedReward,
    pub redemption_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<bool>,
}

impl EnqueueCommand {
    fn redacted(&self) -> Value {
        json!({
            "type": "enqueue",
            "broadcaster_id": self.broadcaster_id,
            "issued_at": self.issued_at,
            "source": "policy",
            "user": self.user.redacted(),
            "reward": self.reward,
            "redemption_id": self.redemption_id,
        })
    }
}

/// Helix redemption update command (dry-run for now).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RedemptionUpdateCommand {
    pub broadcaster_id: String,
    pub issued_at: DateTime<Utc>,
    pub source: CommandSource,
    pub redemption_id: String,
    pub mode: RedemptionUpdateMode,
    pub applicable: bool,
    pub result: CommandResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RedemptionUpdateCommand {
    fn redacted(&self) -> Value {
        json!({
            "type": "redemption.update",
            "broadcaster_id": self.broadcaster_id,
            "issued_at": self.issued_at,
            "source": "policy",
            "redemption_id": self.redemption_id,
            "mode": self.mode,
            "applicable": self.applicable,
            "result": self.result,
            "error": self.error,
        })
    }
}

/// Mode of the Helix redemption update command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedemptionUpdateMode {
    Refund,
    Consume,
}

/// Queue completion command emitted by the admin interface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueCompleteCommand {
    pub broadcaster_id: String,
    pub issued_at: DateTime<Utc>,
    pub source: CommandSource,
    pub entry_id: String,
    pub op_id: String,
}

impl QueueCompleteCommand {
    fn redacted(&self) -> Value {
        json!({
            "type": "queue.complete",
            "broadcaster_id": self.broadcaster_id,
            "issued_at": self.issued_at,
            "source": self.source,
            "entry_id": self.entry_id,
        })
    }
}

/// Queue removal command emitted by the admin interface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueRemoveCommand {
    pub broadcaster_id: String,
    pub issued_at: DateTime<Utc>,
    pub source: CommandSource,
    pub entry_id: String,
    pub reason: QueueRemovalReason,
    pub op_id: String,
}

impl QueueRemoveCommand {
    fn redacted(&self) -> Value {
        json!({
            "type": "queue.remove",
            "broadcaster_id": self.broadcaster_id,
            "issued_at": self.issued_at,
            "source": self.source,
            "entry_id": self.entry_id,
            "reason": self.reason,
        })
    }
}

/// Reason provided when removing a queue entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueueRemovalReason {
    Undo,
    ExplicitRemove,
    StreamStartClear,
}

impl QueueRemovalReason {
    /// Returns the canonical database representation for the reason.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undo => "UNDO",
            Self::ExplicitRemove => "EXPLICIT_REMOVE",
            Self::StreamStartClear => "STREAM_START_CLEAR",
        }
    }
}

/// Settings update command emitted by the admin interface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingsUpdateCommand {
    pub broadcaster_id: String,
    pub issued_at: DateTime<Utc>,
    pub source: CommandSource,
    pub patch: Value,
    pub op_id: String,
}

impl SettingsUpdateCommand {
    fn redacted(&self) -> Value {
        json!({
            "type": "settings.update",
            "broadcaster_id": self.broadcaster_id,
            "issued_at": self.issued_at,
            "source": self.source,
        })
    }
}

/// Result of attempting to execute a command. Currently a placeholder until Helix integration lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResult {
    Ok,
    Failed,
    Skipped,
}

/// Aggregated state used when emitting `state.replace` patches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub version: u64,
    pub queue: Vec<QueueEntry>,
    pub counters_today: Vec<UserCounter>,
    pub settings: Settings,
}

/// Daily counter value for a user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCounter {
    pub user_id: String,
    pub count: u32,
}

/// Patch emitted through SSE channels representing a state change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub version: u64,
    #[serde(rename = "type")]
    pub kind: PatchKind,
    pub at: DateTime<Utc>,
    pub data: Value,
}

impl Patch {
    /// Returns the patch type string.
    pub fn kind_str(&self) -> &'static str {
        self.kind.as_str()
    }
}

/// Enumerates the supported patch kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    QueueEnqueued,
    QueueRemoved,
    QueueCompleted,
    CounterUpdated,
    SettingsUpdated,
    RedemptionUpdated,
    StateReplace,
}

impl PatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueEnqueued => "queue.enqueued",
            Self::QueueRemoved => "queue.removed",
            Self::QueueCompleted => "queue.completed",
            Self::CounterUpdated => "counter.updated",
            Self::SettingsUpdated => "settings.updated",
            Self::RedemptionUpdated => "redemption.updated",
            Self::StateReplace => "state.replace",
        }
    }
}

impl Serialize for PatchKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PatchKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        PatchKind::from_str(&value).map_err(|_| D::Error::custom("unknown patch kind"))
    }
}

impl FromStr for PatchKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queue.enqueued" => Ok(Self::QueueEnqueued),
            "queue.removed" => Ok(Self::QueueRemoved),
            "queue.completed" => Ok(Self::QueueCompleted),
            "counter.updated" => Ok(Self::CounterUpdated),
            "settings.updated" => Ok(Self::SettingsUpdated),
            "redemption.updated" => Ok(Self::RedemptionUpdated),
            "state.replace" => Ok(Self::StateReplace),
            _ => Err(()),
        }
    }
}
