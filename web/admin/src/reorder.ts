import type { QueueReorderUpdate } from '@twi/shared-state';

export function computeReorderOrder(
  currentOrder: string[],
  sourceId: string,
  beforeId: string | null
): string[] | null {
  const sourceIndex = currentOrder.indexOf(sourceId);
  if (sourceIndex === -1) {
    return null;
  }

  const filtered = currentOrder.filter((id) => id !== sourceId);
  let insertIndex = beforeId ? filtered.indexOf(beforeId) : filtered.length;
  if (insertIndex < 0) {
    insertIndex = filtered.length;
  }
  if (insertIndex > filtered.length) {
    insertIndex = filtered.length;
  }

  const next = [...filtered.slice(0, insertIndex), sourceId, ...filtered.slice(insertIndex)];
  if (next.length !== currentOrder.length) {
    return null;
  }
  let changed = false;
  for (let i = 0; i < next.length; i += 1) {
    if (next[i] !== currentOrder[i]) {
      changed = true;
      break;
    }
  }
  return changed ? next : null;
}

export function buildReorderEntries(order: string[]): QueueReorderUpdate[] {
  return order.map((entryId, index) => ({
    entry_id: entryId,
    display_order: index + 1,
  }));
}
