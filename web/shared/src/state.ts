import type {
  Patch,
  QueueEntry,
  Settings,
  SettingsPatch,
  StateSnapshot,
} from './types';

export interface ClientState {
  version: number;
  queue: QueueEntry[];
  completed: QueueEntry[];
  counters: Record<string, number>;
  settings: Settings;
}

export class VersionMismatchError extends Error {
  constructor(public expected: number, public actual: number) {
    super(`expected patch version ${expected} but received ${actual}`);
  }
}

export function createClientState(snapshot: StateSnapshot): ClientState {
  const counters = Object.fromEntries(
    snapshot.counters_today.map((counter) => [counter.user_id, counter.count])
  );
  const queue = sortQueue(snapshot.queue, counters, snapshot.settings);
  const completed = sortCompleted(snapshot.completed ?? []);
  return {
    version: snapshot.version,
    queue,
    completed,
    counters,
    settings: snapshot.settings,
  };
}

export function applyPatch(state: ClientState, patch: Patch): ClientState {
  if (patch.type === 'state.replace') {
    return createClientState(patch.data.state);
  }

  const expected = state.version + 1;
  if (patch.version !== expected) {
    throw new VersionMismatchError(expected, patch.version);
  }

  switch (patch.type) {
    case 'queue.enqueued': {
      const { entry, user_today_count } = patch.data;
      const counters = {
        ...state.counters,
        [entry.user_id]: user_today_count,
      };
      const queue = sortQueue(
        [...state.queue.filter((item) => item.id !== entry.id), entry],
        counters,
        state.settings
      );
      const completed = state.completed.filter((item) => item.id !== entry.id);
      return {
        version: patch.version,
        queue,
        completed,
        counters,
        settings: state.settings,
      };
    }
    case 'queue.removed':
    case 'queue.completed': {
      if (patch.type === 'queue.completed') {
        const { entry } = patch.data;
        const queue = sortQueue(
          state.queue.filter((item) => item.id !== entry.id),
          state.counters,
          state.settings
        );
        const completed = sortCompleted([
          ...state.completed.filter((item) => item.id !== entry.id),
          entry,
        ]);
        return {
          version: patch.version,
          queue,
          completed,
          counters: state.counters,
          settings: state.settings,
        };
      }
      const target =
        state.queue.find((entry) => entry.id === patch.data.entry_id) ??
        state.completed.find((entry) => entry.id === patch.data.entry_id);
      const queue = state.queue.filter((entry) => entry.id !== patch.data.entry_id);
      const completed = state.completed.filter((entry) => entry.id !== patch.data.entry_id);
      const counters = target
        ? {
            ...state.counters,
            [target.user_id]: patch.data.user_today_count,
          }
        : state.counters;
      return {
        version: patch.version,
        queue,
        completed,
        counters,
        settings: state.settings,
      };
    }
    case 'queue.reordered': {
      const updates = new Map(
        patch.data.entries.map((entry) => [entry.entry_id, entry.display_order])
      );
      const queue = sortQueue(
        state.queue.map((item) => {
          const updated = updates.get(item.id);
          return typeof updated === 'number' ? { ...item, display_order: updated } : item;
        }),
        state.counters,
        state.settings
      );
      const completed = sortCompleted(
        state.completed.map((item) => {
          const updated = updates.get(item.id);
          return typeof updated === 'number' ? { ...item, display_order: updated } : item;
        })
      );
      return {
        version: patch.version,
        queue,
        completed,
        counters: state.counters,
        settings: state.settings,
      };
    }
    case 'counter.updated': {
      const counters = {
        ...state.counters,
        [patch.data.user_id]: patch.data.count,
      };
      const queue = state.settings.prioritize_low_counts
        ? sortQueue([...state.queue], counters, state.settings)
        : state.queue.map((entry) => ({ ...entry }));
      return {
        version: patch.version,
        queue,
        completed: state.completed,
        counters,
        settings: state.settings,
      };
    }
    case 'settings.updated': {
      const settings = mergeSettings(state.settings, patch.data.patch);
      const queue = sortQueue(state.queue, state.counters, settings);
      return {
        version: patch.version,
        queue,
        completed: state.completed,
        counters: state.counters,
        settings,
      };
    }
    case 'redemption.updated': {
      const { redemption_id, managed } = patch.data;
      const queue = state.queue.map((entry) => {
        if (entry.redemption_id === redemption_id) {
          return { ...entry, managed };
        }
        return entry;
      });
      const completed = state.completed.map((entry) => {
        if (entry.redemption_id === redemption_id) {
          return { ...entry, managed };
        }
        return entry;
      });
      return {
        version: patch.version,
        queue,
        completed,
        counters: state.counters,
        settings: state.settings,
      };
    }
    default: {
      return state;
    }
  }
}

function mergeSettings(current: Settings, patch: SettingsPatch): Settings {
  const mergedPolicy = patch.policy
    ? { ...current.policy, ...patch.policy }
    : current.policy;
  const base: Settings = {
    ...current,
    ...patch,
    policy: mergedPolicy,
  };
  return base;
}

function sortQueue(
  entries: QueueEntry[],
  counters: Record<string, number>,
  settings: Settings
): QueueEntry[] {
  return [...entries].sort((a, b) => {
    if (settings.prioritize_low_counts) {
      const countA = counters[a.user_id] ?? 0;
      const countB = counters[b.user_id] ?? 0;
      if (countA !== countB) {
        return countA - countB;
      }
    }
    return a.display_order - b.display_order;
  });
}

function sortCompleted(entries: QueueEntry[]): QueueEntry[] {
  return [...entries].sort((a, b) => {
    const completedA = a.completed_at ? Date.parse(a.completed_at) : Number.NEGATIVE_INFINITY;
    const completedB = b.completed_at ? Date.parse(b.completed_at) : Number.NEGATIVE_INFINITY;
    if (completedA !== completedB) {
      return completedB - completedA;
    }
    return a.display_order - b.display_order;
  });
}
