import { describe, expect, it } from 'vitest';
import { buildReorderEntries, computeReorderOrder } from './reorder';

describe('reorder helpers', () => {
  it('moves source before target when both exist', () => {
    const next = computeReorderOrder(['a', 'b', 'c'], 'c', 'a');
    expect(next).toEqual(['c', 'a', 'b']);
  });

  it('appends to end when beforeId is null', () => {
    const next = computeReorderOrder(['a', 'b', 'c'], 'a', null);
    expect(next).toEqual(['b', 'c', 'a']);
  });

  it('returns null when order does not change', () => {
    const next = computeReorderOrder(['a', 'b', 'c'], 'a', 'b');
    expect(next).toBeNull();
  });

  it('returns null when source is missing', () => {
    const next = computeReorderOrder(['a', 'b'], 'z', null);
    expect(next).toBeNull();
  });

  it('builds reorder entries with incremental order', () => {
    const updates = buildReorderEntries(['a', 'b']);
    expect(updates).toEqual([
      { entry_id: 'a', display_order: 1 },
      { entry_id: 'b', display_order: 2 },
    ]);
  });
});
