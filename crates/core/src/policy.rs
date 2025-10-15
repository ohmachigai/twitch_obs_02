use std::{collections::HashMap, sync::Mutex};

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::types::{
    Command, CommandResult, CommandSource, EnqueueCommand, NormalizedEvent, NormalizedReward,
    NormalizedUser, RedemptionUpdateCommand, RedemptionUpdateMode, Settings,
};

/// Policy engine that evaluates normalized events and produces commands.
#[derive(Debug, Default)]
pub struct PolicyEngine {
    duplicate_window: Mutex<HashMap<DuplicateKey, DateTime<Utc>>>,
}

impl PolicyEngine {
    /// Creates a new policy engine instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates a normalized event with the provided settings and returns the resulting commands.
    pub fn evaluate(
        &self,
        settings: &Settings,
        event: &NormalizedEvent,
        issued_at: DateTime<Utc>,
    ) -> PolicyOutcome {
        match event {
            NormalizedEvent::RedemptionAdd {
                broadcaster_id,
                redemption_id,
                user,
                reward,
                occurred_at,
            } => self.evaluate_redemption_add(
                settings,
                broadcaster_id,
                redemption_id,
                user,
                reward,
                *occurred_at,
                issued_at,
            ),
            _ => PolicyOutcome::ignored("event_not_supported"),
        }
    }

    fn evaluate_redemption_add(
        &self,
        settings: &Settings,
        broadcaster_id: &str,
        redemption_id: &str,
        user: &NormalizedUser,
        reward: &NormalizedReward,
        occurred_at: DateTime<Utc>,
        issued_at: DateTime<Utc>,
    ) -> PolicyOutcome {
        let policy = settings.policy();
        if policy.target_rewards.is_empty() {
            return PolicyOutcome::ignored("policy_disabled");
        }

        if !policy.is_reward_enabled(&reward.id) {
            return PolicyOutcome::ignored("reward_not_targeted");
        }

        let key = DuplicateKey {
            broadcaster_id: broadcaster_id.to_string(),
            user_id: user.id.clone(),
            reward_id: reward.id.clone(),
        };

        let mut duplicates = self.duplicate_window.lock().expect("duplicate guard");
        let duplicate = duplicates
            .get(&key)
            .map(|last| occurred_at - *last < Duration::seconds(policy.anti_spam_window_sec as i64))
            .unwrap_or(false);
        duplicates.insert(key, occurred_at);
        drop(duplicates);

        if duplicate {
            let update = Command::RedemptionUpdate(RedemptionUpdateCommand {
                broadcaster_id: broadcaster_id.to_string(),
                issued_at,
                source: CommandSource::Policy,
                redemption_id: redemption_id.to_string(),
                mode: match policy.duplicate_policy {
                    crate::types::DuplicatePolicy::Consume => RedemptionUpdateMode::Consume,
                    crate::types::DuplicatePolicy::Refund => RedemptionUpdateMode::Refund,
                },
                applicable: false,
                result: CommandResult::Skipped,
                error: None,
            });
            PolicyOutcome::duplicate(vec![update])
        } else {
            let enqueue = Command::Enqueue(EnqueueCommand {
                broadcaster_id: broadcaster_id.to_string(),
                issued_at,
                source: CommandSource::Policy,
                user: user.clone(),
                reward: reward.clone(),
                redemption_id: redemption_id.to_string(),
                managed: None,
            });
            let update = Command::RedemptionUpdate(RedemptionUpdateCommand {
                broadcaster_id: broadcaster_id.to_string(),
                issued_at,
                source: CommandSource::Policy,
                redemption_id: redemption_id.to_string(),
                mode: RedemptionUpdateMode::Consume,
                applicable: false,
                result: CommandResult::Skipped,
                error: None,
            });
            PolicyOutcome::applied(vec![enqueue, update])
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DuplicateKey {
    broadcaster_id: String,
    user_id: String,
    reward_id: String,
}

/// Policy evaluation result.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyOutcome {
    pub commands: Vec<Command>,
    pub action: PolicyAction,
    pub reason: Option<String>,
}

impl PolicyOutcome {
    fn applied(commands: Vec<Command>) -> Self {
        Self {
            commands,
            action: PolicyAction::Applied,
            reason: None,
        }
    }

    fn duplicate(commands: Vec<Command>) -> Self {
        Self {
            commands,
            action: PolicyAction::Duplicate,
            reason: Some("duplicate_within_window".to_string()),
        }
    }

    fn ignored(reason: &str) -> Self {
        Self {
            commands: Vec::new(),
            action: PolicyAction::Ignored,
            reason: Some(reason.to_string()),
        }
    }

    /// Returns a redacted payload suitable for Tap output.
    pub fn redacted(&self) -> Value {
        json!({
            "action": self.action.as_str(),
            "reason": self.reason,
            "commands": self.commands.iter().map(Command::redacted).collect::<Vec<_>>(),
        })
    }

    /// Returns `true` when the outcome represents a duplicate event.
    pub fn is_duplicate(&self) -> bool {
        matches!(self.action, PolicyAction::Duplicate)
    }
}

/// Classification of a policy outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAction {
    Applied,
    Duplicate,
    Ignored,
}

impl PolicyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Duplicate => "duplicate",
            Self::Ignored => "ignored",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DuplicatePolicy, PolicySettings};

    fn settings(target_reward: &str, duplicate_policy: DuplicatePolicy) -> Settings {
        Settings {
            overlay_theme: "test".into(),
            group_size: 1,
            clear_on_stream_start: false,
            clear_decrement_counts: false,
            policy: PolicySettings {
                anti_spam_window_sec: 60,
                duplicate_policy,
                target_rewards: vec![target_reward.to_string()],
            },
        }
    }

    fn redemption_event() -> NormalizedEvent {
        NormalizedEvent::RedemptionAdd {
            broadcaster_id: "b-1".to_string(),
            occurred_at: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            redemption_id: "r-1".to_string(),
            user: NormalizedUser {
                id: "user-1".to_string(),
                login: Some("viewer".to_string()),
                display_name: Some("Viewer".to_string()),
            },
            reward: NormalizedReward {
                id: "reward-1".to_string(),
                title: Some("Join".to_string()),
                cost: Some(1),
            },
        }
    }

    #[test]
    fn ignores_non_target_rewards() {
        let engine = PolicyEngine::new();
        let mut event = redemption_event();
        if let NormalizedEvent::RedemptionAdd { reward, .. } = &mut event {
            reward.id = "other".to_string();
        }

        let outcome = engine.evaluate(
            &settings("reward-1", DuplicatePolicy::Consume),
            &event,
            Utc::now(),
        );

        assert!(outcome.commands.is_empty());
        assert_eq!(outcome.action, PolicyAction::Ignored);
    }

    #[test]
    fn produces_commands_for_first_redemption() {
        let engine = PolicyEngine::new();
        let event = redemption_event();
        let issued_at = event.occurred_at();
        let outcome = engine.evaluate(
            &settings("reward-1", DuplicatePolicy::Consume),
            &event,
            issued_at,
        );

        assert_eq!(outcome.commands.len(), 2);
        assert_eq!(outcome.action, PolicyAction::Applied);
    }

    #[test]
    fn duplicate_within_window_uses_policy_mode() {
        let engine = PolicyEngine::new();
        let mut event = redemption_event();
        let issued_at = event.occurred_at();

        // First event primes the cache
        let _ = engine.evaluate(
            &settings("reward-1", DuplicatePolicy::Refund),
            &event,
            issued_at,
        );

        if let NormalizedEvent::RedemptionAdd { occurred_at, .. } = &mut event {
            *occurred_at += Duration::seconds(30);
        }

        let outcome = engine.evaluate(
            &settings("reward-1", DuplicatePolicy::Refund),
            &event,
            issued_at,
        );

        assert_eq!(outcome.commands.len(), 1);
        assert!(outcome.is_duplicate());
        assert_eq!(outcome.commands[0].metric_kind(), "refund");
    }

    #[test]
    fn outside_window_resets_duplicate_state() {
        let engine = PolicyEngine::new();
        let mut event = redemption_event();
        let issued_at = event.occurred_at();

        let settings = settings("reward-1", DuplicatePolicy::Consume);
        let _ = engine.evaluate(&settings, &event, issued_at);

        if let NormalizedEvent::RedemptionAdd { occurred_at, .. } = &mut event {
            *occurred_at += Duration::seconds(61);
        }

        let outcome = engine.evaluate(&settings, &event, issued_at);
        assert_eq!(outcome.action, PolicyAction::Applied);
        assert_eq!(outcome.commands.len(), 2);
    }
}
