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
    completed: [],
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

  it('honours manual display order when prioritization is disabled', () => {
    const snapshot: StateSnapshot = {
      ...baseSnapshot,
      queue: [
        { ...makeEntry('entry-a', 'user-a', '2024-01-01T09:05:00Z'), display_order: 2 },
        { ...makeEntry('entry-b', 'user-b', '2024-01-01T09:00:00Z'), display_order: 1 },
      ],
      counters_today: [
        { user_id: 'user-a', count: 5 },
        { user_id: 'user-b', count: 0 },
      ],
      settings: { ...defaultSettings(), prioritize_low_counts: false },
    };

    const state = createClientState(snapshot);
    expect(state.queue.map((entry) => entry.id)).toEqual(['entry-b', 'entry-a']);
  });

  it('removes entries on queue.removed', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'queue.removed',
      version: 11,
      at: '2024-01-01T10:10:00Z',
      data: { entry_id: 'entry-1', reason: 'EXPLICIT_REMOVE', user_today_count: 0 },
    };
    const next = applyPatch(state, patch);
    expect(next.queue.map((entry) => entry.id)).toEqual(['entry-2']);
    expect(next.completed).toHaveLength(0);
    expect(next.counters['user-1']).toBe(0);
  });

  it('moves entries to completed on queue.completed', () => {
    const state = createClientState(baseSnapshot);
    const entry = makeEntry('entry-1', 'user-1', '2024-01-01T10:00:00Z');
    const patch: Patch = {
      type: 'queue.completed',
      version: 11,
      at: '2024-01-01T10:10:00Z',
      data: {
        entry: { ...entry, status: 'COMPLETED', completed_at: '2024-01-01T10:10:00Z' },
      },
    };
    const next = applyPatch(state, patch);
    expect(next.queue.map((item) => item.id)).toEqual(['entry-2']);
    expect(next.completed[0].id).toBe('entry-1');
    expect(next.completed[0].completed_at).toBe('2024-01-01T10:10:00Z');
  });

  it('restores completed entries on queue.enqueued after undo', () => {
    const entry = makeEntry('entry-1', 'user-1', '2024-01-01T10:00:00Z');
    const completed: Patch = {
      type: 'queue.completed',
      version: 11,
      at: '2024-01-01T10:10:00Z',
      data: {
        entry: { ...entry, status: 'COMPLETED', completed_at: '2024-01-01T10:10:00Z' },
      },
    };
    const afterComplete = applyPatch(createClientState(baseSnapshot), completed);
    const undo: Patch = {
      type: 'queue.enqueued',
      version: 12,
      at: '2024-01-01T10:12:00Z',
      data: {
        entry,
        user_today_count: 1,
      },
    };
    const restored = applyPatch(afterComplete, undo);
    expect(restored.queue.map((item) => item.id)).toEqual(['entry-1', 'entry-2']);
    expect(restored.completed).toHaveLength(0);
    expect(restored.counters['user-1']).toBe(1);
  });

  it('removes completed entries on queue.removed', () => {
    const entry = makeEntry('entry-1', 'user-1', '2024-01-01T10:00:00Z');
    const completedState = applyPatch(
      createClientState(baseSnapshot),
      {
        type: 'queue.completed',
        version: 11,
        at: '2024-01-01T10:10:00Z',
        data: {
          entry: { ...entry, status: 'COMPLETED', completed_at: '2024-01-01T10:10:00Z' },
        },
      }
    );
    const removed = applyPatch(completedState, {
      type: 'queue.removed',
      version: 12,
      at: '2024-01-01T10:12:00Z',
      data: { entry_id: 'entry-1', reason: 'EXPLICIT_REMOVE', user_today_count: 0 },
    });
    expect(removed.queue.map((item) => item.id)).toEqual(['entry-2']);
    expect(removed.completed).toHaveLength(0);
    expect(removed.counters['user-1']).toBe(0);
  });

  it('updates display order on queue.reordered', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'queue.reordered',
      version: 11,
      at: '2024-01-01T10:12:00Z',
      data: {
        entries: [
          { entry_id: 'entry-1', display_order: 5 },
          { entry_id: 'entry-2', display_order: 1 },
        ],
      },
    };
    const next = applyPatch(state, patch);
    expect(next.version).toBe(11);
    expect(next.queue.map((item) => item.id)).toEqual(['entry-2', 'entry-1']);
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
      completed: [],
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

  it('resorts queue when prioritize_low_counts changes', () => {
    const snapshot: StateSnapshot = {
      ...baseSnapshot,
      queue: [
        { ...makeEntry('entry-a', 'user-a', '2024-01-01T09:05:00Z'), display_order: 2 },
        { ...makeEntry('entry-b', 'user-b', '2024-01-01T09:00:00Z'), display_order: 1 },
      ],
      counters_today: [
        { user_id: 'user-a', count: 0 },
        { user_id: 'user-b', count: 5 },
      ],
      settings: defaultSettings(),
    };
    const state = createClientState(snapshot);
    const disable: Patch = {
      type: 'settings.updated',
      version: state.version + 1,
      at: '2024-01-01T10:30:00Z',
      data: { patch: { prioritize_low_counts: false } },
    };
    const disabled = applyPatch(state, disable);
    expect(disabled.queue.map((entry) => entry.id)).toEqual(['entry-b', 'entry-a']);

    const enable: Patch = {
      type: 'settings.updated',
      version: disabled.version + 1,
      at: '2024-01-01T10:35:00Z',
      data: { patch: { prioritize_low_counts: true } },
    };
    const enabled = applyPatch(disabled, enable);
    expect(enabled.queue.map((entry) => entry.id)).toEqual(['entry-a', 'entry-b']);
  });

  it('updates queue managed flag on redemption.updated', () => {
    const state = createClientState(baseSnapshot);
    const patch: Patch = {
      type: 'redemption.updated',
      version: 11,
      at: '2024-01-01T10:20:00Z',
      data: {
        redemption_id: 'entry-1-redemption',
        mode: 'consume',
        applicable: true,
        result: 'ok',
        managed: true,
      },
    };

    const next = applyPatch(state, patch);
    const entry = next.queue.find((item) => item.id === 'entry-1');
    expect(entry?.managed).toBe(true);
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
    redemption_id: `${id}-redemption`,
    enqueued_at: enqueuedAt,
    display_order: Date.parse(enqueuedAt) / 1000,
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
    prioritize_low_counts: true,
    policy: {
      anti_spam_window_sec: 60,
      duplicate_policy: 'consume' as const,
      target_rewards: [],
    },
  };
}
