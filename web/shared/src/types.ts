export type QueueEntryStatus = 'QUEUED' | 'COMPLETED' | 'REMOVED';

export interface QueueEntry {
  id: string;
  broadcaster_id: string;
  user_id: string;
  user_login: string;
  user_display_name: string;
  user_avatar?: string;
  reward_id: string;
  redemption_id?: string;
  enqueued_at: string;
  status: QueueEntryStatus;
  status_reason?: string;
  managed: boolean;
  last_updated_at: string;
}

export interface UserCounter {
  user_id: string;
  count: number;
}

export type DuplicatePolicy = 'consume' | 'refund';

export interface PolicySettings {
  anti_spam_window_sec: number;
  duplicate_policy: DuplicatePolicy;
  target_rewards: string[];
}

export interface Settings {
  overlay_theme: string;
  group_size: number;
  clear_on_stream_start: boolean;
  clear_decrement_counts: boolean;
  policy: PolicySettings;
}

export type SettingsPatch = Partial<Omit<Settings, 'policy'>> & {
  policy?: Partial<PolicySettings>;
};

export interface StateSnapshot {
  version: number;
  queue: QueueEntry[];
  counters_today: UserCounter[];
  settings: Settings;
}

export interface QueueEnqueuedPatch {
  type: 'queue.enqueued';
  version: number;
  at: string;
  data: {
    entry: QueueEntry;
    user_today_count: number;
  };
}

export type QueueRemovalReason = 'UNDO' | 'EXPLICIT_REMOVE' | 'STREAM_START_CLEAR';

export interface QueueRemovedPatch {
  type: 'queue.removed';
  version: number;
  at: string;
  data: {
    entry_id: string;
    reason: QueueRemovalReason;
    user_today_count: number;
  };
}

export interface QueueCompletedPatch {
  type: 'queue.completed';
  version: number;
  at: string;
  data: {
    entry_id: string;
  };
}

export interface CounterUpdatedPatch {
  type: 'counter.updated';
  version: number;
  at: string;
  data: {
    user_id: string;
    count: number;
  };
}

export interface SettingsUpdatedPatch {
  type: 'settings.updated';
  version: number;
  at: string;
  data: {
    patch: SettingsPatch;
  };
}

export interface RedemptionUpdatedPatch {
  type: 'redemption.updated';
  version: number;
  at: string;
  data: {
    redemption_id: string;
    mode: string;
    applicable: boolean;
    result: string;
    managed: boolean;
    error?: string;
  };
}

export interface StateReplacePatch {
  type: 'state.replace';
  version: number;
  at: string;
  data: {
    state: StateSnapshot;
  };
}

export type Patch =
  | QueueEnqueuedPatch
  | QueueRemovedPatch
  | QueueCompletedPatch
  | CounterUpdatedPatch
  | SettingsUpdatedPatch
  | RedemptionUpdatedPatch
  | StateReplacePatch;
