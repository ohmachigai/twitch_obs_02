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
  const queue = sortQueue(snapshot.queue, counters);
  return {
    version: snapshot.version,
    queue,
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
        counters
      );
      return {
        version: patch.version,
        queue,
        counters,
        settings: state.settings,
      };
    }
    case 'queue.removed':
    case 'queue.completed': {
      const queue = state.queue.filter((entry) => entry.id !== patch.data.entry_id);
      return {
        version: patch.version,
        queue,
        counters: state.counters,
        settings: state.settings,
      };
    }
    case 'counter.updated': {
      const counters = {
        ...state.counters,
        [patch.data.user_id]: patch.data.count,
      };
      const queue = sortQueue([...state.queue], counters);
      return {
        version: patch.version,
        queue,
        counters,
        settings: state.settings,
      };
    }
    case 'settings.updated': {
      const settings = mergeSettings(state.settings, patch.data.patch);
      return {
        version: patch.version,
        queue: state.queue,
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
      return {
        version: patch.version,
        queue,
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

function sortQueue(entries: QueueEntry[], counters: Record<string, number>): QueueEntry[] {
  return [...entries].sort((a, b) => {
    const countA = counters[a.user_id] ?? 0;
    const countB = counters[b.user_id] ?? 0;
    if (countA !== countB) {
      return countA - countB;
    }
    const timeA = Date.parse(a.enqueued_at);
    const timeB = Date.parse(b.enqueued_at);
    return timeA - timeB;
  });
}
