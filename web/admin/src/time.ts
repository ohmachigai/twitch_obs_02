const SECOND = 1000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;
const MONTH = 30 * DAY;
const YEAR = 365 * DAY;

export interface RelativeTimeParts {
  value: number;
  unit: Intl.RelativeTimeFormatUnit;
}

export function getRelativeTimeParts(target: Date, base = new Date()): RelativeTimeParts {
  const diffMs = target.getTime() - base.getTime();
  const absDiff = Math.abs(diffMs);

  if (absDiff < MINUTE) {
    return { value: Math.round(diffMs / SECOND), unit: 'second' };
  }
  if (absDiff < HOUR) {
    return { value: Math.round(diffMs / MINUTE), unit: 'minute' };
  }
  if (absDiff < DAY) {
    return { value: Math.round(diffMs / HOUR), unit: 'hour' };
  }
  if (absDiff < WEEK) {
    return { value: Math.round(diffMs / DAY), unit: 'day' };
  }
  if (absDiff < MONTH) {
    return { value: Math.round(diffMs / WEEK), unit: 'week' };
  }
  if (absDiff < YEAR) {
    return { value: Math.round(diffMs / MONTH), unit: 'month' };
  }
  return { value: Math.round(diffMs / YEAR), unit: 'year' };
}

const DEFAULT_RELATIVE_TIME_FORMATTER = new Intl.RelativeTimeFormat(undefined, {
  numeric: 'auto',
});

export function formatRelativeTime(
  target: Date,
  base = new Date(),
  formatter: Intl.RelativeTimeFormat = DEFAULT_RELATIVE_TIME_FORMATTER
): string {
  const parts = getRelativeTimeParts(target, base);
  return formatter.format(parts.value, parts.unit);
}
