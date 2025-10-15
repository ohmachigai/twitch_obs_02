use chrono::{DateTime, Utc};
use serde_json::json;

use crate::types::{Patch, PatchKind, QueueEntry, RedemptionUpdateCommand, StateSnapshot};

/// Pure projector helpers that transform commands into patches.
pub struct Projector;

impl Projector {
    /// Builds a `queue.enqueued` patch for the provided entry.
    pub fn queue_enqueued(
        version: u64,
        at: DateTime<Utc>,
        entry: QueueEntry,
        user_today_count: u32,
    ) -> Patch {
        Patch {
            version,
            kind: PatchKind::QueueEnqueued,
            at,
            data: json!({
                "entry": entry,
                "user_today_count": user_today_count,
            }),
        }
    }

    /// Builds a `redemption.updated` patch reflecting the Helix command status.
    pub fn redemption_updated(
        version: u64,
        at: DateTime<Utc>,
        command: &RedemptionUpdateCommand,
    ) -> Patch {
        Patch {
            version,
            kind: PatchKind::RedemptionUpdated,
            at,
            data: json!({
                "redemption_id": command.redemption_id,
                "mode": command.mode,
                "applicable": command.applicable,
                "result": command.result,
            }),
        }
    }

    /// Builds a `state.replace` patch with the provided snapshot.
    pub fn state_replace(version: u64, at: DateTime<Utc>, snapshot: StateSnapshot) -> Patch {
        Patch {
            version,
            kind: PatchKind::StateReplace,
            at,
            data: json!({ "state": snapshot }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CommandResult, QueueEntryStatus, Settings, UserCounter};

    fn sample_entry() -> QueueEntry {
        QueueEntry {
            id: "entry-1".to_string(),
            broadcaster_id: "b-1".to_string(),
            user_id: "u-1".to_string(),
            user_login: "alice".to_string(),
            user_display_name: "Alice".to_string(),
            user_avatar: None,
            reward_id: "r-join".to_string(),
            redemption_id: Some("red-1".to_string()),
            enqueued_at: Utc::now(),
            status: QueueEntryStatus::Queued,
            status_reason: None,
            managed: true,
            last_updated_at: Utc::now(),
        }
    }

    #[test]
    fn queue_enqueued_embeds_entry() {
        let entry = sample_entry();
        let at = Utc::now();
        let patch = Projector::queue_enqueued(5, at, entry.clone(), 3);
        assert_eq!(patch.version, 5);
        assert_eq!(patch.kind_str(), "queue.enqueued");
        assert_eq!(patch.data["user_today_count"].as_u64(), Some(3));
        assert_eq!(patch.data["entry"]["id"].as_str(), Some("entry-1"));
    }

    #[test]
    fn redemption_patch_reflects_command() {
        let at = Utc::now();
        let command = RedemptionUpdateCommand {
            broadcaster_id: "b-1".to_string(),
            issued_at: at,
            source: crate::types::CommandSource::Policy,
            redemption_id: "red-1".to_string(),
            mode: crate::types::RedemptionUpdateMode::Consume,
            applicable: true,
            result: CommandResult::Ok,
            error: None,
        };
        let patch = Projector::redemption_updated(10, at, &command);
        assert_eq!(patch.kind_str(), "redemption.updated");
        assert_eq!(patch.data["redemption_id"].as_str(), Some("red-1"));
    }

    #[test]
    fn state_replace_wraps_snapshot() {
        let at = Utc::now();
        let settings: Settings = serde_json::from_str("{}").unwrap();
        let snapshot = StateSnapshot {
            version: 12,
            queue: vec![sample_entry()],
            counters_today: vec![UserCounter {
                user_id: "u-1".to_string(),
                count: 2,
            }],
            settings,
        };
        let patch = Projector::state_replace(12, at, snapshot.clone());
        assert_eq!(patch.kind_str(), "state.replace");
        assert_eq!(patch.data["state"]["version"].as_u64(), Some(12));
        assert_eq!(patch.data["state"]["queue"].as_array().unwrap().len(), 1);
        assert_eq!(
            patch.data["state"]["counters_today"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
