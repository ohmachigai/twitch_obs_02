import { describe, expect, it } from 'vitest';
import { formatRelativeTime, getRelativeTimeParts } from './time';

describe('getRelativeTimeParts', () => {
  const base = new Date('2024-01-01T12:00:00Z');

  it('uses seconds for sub-minute differences', () => {
    const target = new Date(base.getTime() - 59 * 1000);
    expect(getRelativeTimeParts(target, base)).toEqual({ value: -59, unit: 'second' });
  });

  it('switches to minutes once a full minute elapses', () => {
    const target = new Date(base.getTime() - 61 * 1000);
    expect(getRelativeTimeParts(target, base)).toEqual({ value: -1, unit: 'minute' });
  });

  it('returns hours for multi-hour spans', () => {
    const target = new Date(base.getTime() - 3 * 60 * 60 * 1000);
    expect(getRelativeTimeParts(target, base)).toEqual({ value: -3, unit: 'hour' });
  });

  it('returns days for spans longer than a day', () => {
    const target = new Date(base.getTime() - 2 * 24 * 60 * 60 * 1000);
    expect(getRelativeTimeParts(target, base)).toEqual({ value: -2, unit: 'day' });
  });

  it('handles future timestamps symmetrically', () => {
    const target = new Date(base.getTime() + 90 * 60 * 1000);
    expect(getRelativeTimeParts(target, base)).toEqual({ value: 2, unit: 'hour' });
  });
});

describe('formatRelativeTime', () => {
  it('delegates to Intl.RelativeTimeFormat', () => {
    const base = new Date('2024-01-01T12:00:00Z');
    const target = new Date(base.getTime() - 5 * 60 * 1000);
    const formatter = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
    expect(formatRelativeTime(target, base, formatter)).toBe('5 minutes ago');
  });
});
