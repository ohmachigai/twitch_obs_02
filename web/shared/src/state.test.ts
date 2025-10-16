import { describe, expect, it } from 'vitest';
import { applyPatch, createClientState, VersionMismatchError } from './state';
import type { Patch, QueueEntry, SettingsPatch, StateSnapshot } from './types';

describe('shared state helpers', () => {
  const baseSnapshot: StateSnapshot = {
    version: 10,
    queue: [
      makeEntry('entry-1', 'user-1', '2024-01-01T10:00:00Z'),
      makeEntry('entry-2', 'user-2', '2024-01-01T10:05:00Z'),
    ],
    counters_today: [
      { user_id: 'user-1', count: 1 },
      { user_id: 'user-2', count: 1 },
    ],
    settings: defaultSettings(),
  };

  it('creates a sorted client state from snapshot', () => {
    const state = createClientState({
      ...baseSnapshot,
      queue: [
        makeEntry('entry-2', 'user-2', '2024-01-01T10:05:00Z'),
        makeEntry('entry-1', 'user-1', '2024-01-01T10:00:00Z'),
      ],
      counters_today: [
        { user_id: 'user-1', count: 2 },
        { user_id: 'user-2', count: 1 },
      ],
    });

    expect(state.queue[0].id).toBe('entry-2');
    expect(state.queue[1].id).toBe('entry-1');
    expect(state.counters['user-1']).toBe(2);
  });

  it('applies queue.enqueued and resorts by counter then time', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'queue.enqueued',
      version: 11,
      at: '2024-01-01T10:10:00Z',
      data: {
        entry: makeEntry('entry-3', 'user-3', '2024-01-01T10:10:00Z'),
        user_today_count: 0,
      },
    };

    const next = applyPatch(state, patch);
    expect(next.version).toBe(11);
    expect(next.queue[0].id).toBe('entry-3');
    expect(next.queue).toHaveLength(3);
  });

  it('removes entries on queue.removed', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'queue.removed',
      version: 11,
      at: '2024-01-01T10:10:00Z',
      data: { entry_id: 'entry-1', reason: 'UNDO', user_today_count: 0 },
    };
    const next = applyPatch(state, patch);
    expect(next.queue.map((entry) => entry.id)).toEqual(['entry-2']);
  });

  it('throws on version mismatch', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'queue.removed',
      version: 15,
      at: '2024-01-01T10:10:00Z',
      data: { entry_id: 'entry-1', reason: 'UNDO', user_today_count: 0 },
    };

    expect(() => applyPatch(state, patch)).toThrow(VersionMismatchError);
  });

  it('applies state.replace regardless of version gap', () => {
    const state = createClientState(baseSnapshot);
    const snapshot: StateSnapshot = {
      version: 25,
      queue: [makeEntry('entry-9', 'user-9', '2024-01-01T11:00:00Z')],
      counters_today: [{ user_id: 'user-9', count: 1 }],
      settings: defaultSettings(),
    };

    const patch: Patch = {
      type: 'state.replace',
      version: 25,
      at: '2024-01-01T11:00:00Z',
      data: { state: snapshot },
    };

    const next = applyPatch(state, patch);
    expect(next.version).toBe(25);
    expect(next.queue).toHaveLength(1);
    expect(next.queue[0].id).toBe('entry-9');
  });

  it('merges nested policy settings on settings.updated', () => {
    const state = createClientState(baseSnapshot);
    const patchPayload: SettingsPatch = {
      group_size: 3,
      policy: {
        duplicate_policy: 'refund',
      },
    };
    const patch: Patch = {
      type: 'settings.updated',
      version: 11,
      at: '2024-01-01T10:15:00Z',
      data: { patch: patchPayload },
    };

    const next = applyPatch(state, patch);
    expect(next.settings.group_size).toBe(3);
    expect(next.settings.policy.duplicate_policy).toBe('refund');
    expect(next.settings.policy.target_rewards).toEqual([]);
  });
});

function makeEntry(id: string, userId: string, enqueuedAt: string): QueueEntry {
  return {
    id,
    broadcaster_id: 'b-1',
    user_id: userId,
    user_login: userId,
    user_display_name: userId,
    reward_id: 'reward-1',
    enqueued_at: enqueuedAt,
    status: 'QUEUED',
    managed: false,
    last_updated_at: enqueuedAt,
  };
}

function defaultSettings() {
  return {
    overlay_theme: 'default',
    group_size: 1,
    clear_on_stream_start: false,
    clear_decrement_counts: false,
    policy: {
      anti_spam_window_sec: 60,
      duplicate_policy: 'consume' as const,
      target_rewards: [],
    },
  };
}
