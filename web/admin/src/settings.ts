import type { Settings, SettingsPatch } from '@twi/shared-state';

export function readSettingsPatch(form: HTMLFormElement): SettingsPatch {
  const data = new FormData(form);
  const patch: SettingsPatch = {};

  const groupSizeRaw = data.get('group_size');
  if (typeof groupSizeRaw === 'string' && groupSizeRaw.trim().length > 0) {
    const value = Number(groupSizeRaw);
    if (Number.isFinite(value) && value > 0) {
      patch.group_size = value;
    }
  }

  const overlayTheme = data.get('overlay_theme');
  if (typeof overlayTheme === 'string' && overlayTheme.trim().length > 0) {
    patch.overlay_theme = overlayTheme.trim();
  }

  const clearOnStreamStart = (form.elements.namedItem('clear_on_stream_start') as HTMLInputElement | null)?.checked;
  if (typeof clearOnStreamStart === 'boolean') {
    patch.clear_on_stream_start = clearOnStreamStart;
  }

  const clearDecrementCounts = (form.elements.namedItem('clear_decrement_counts') as HTMLInputElement | null)?.checked;
  if (typeof clearDecrementCounts === 'boolean') {
    patch.clear_decrement_counts = clearDecrementCounts;
  }

  const policy: NonNullable<SettingsPatch['policy']> = {};
  const windowRaw = data.get('policy.anti_spam_window_sec');
  if (typeof windowRaw === 'string' && windowRaw.trim().length > 0) {
    const value = Number(windowRaw);
    if (Number.isFinite(value) && value >= 0) {
      policy.anti_spam_window_sec = value;
    }
  }

  const duplicatePolicy = data.get('policy.duplicate_policy');
  if (typeof duplicatePolicy === 'string' && duplicatePolicy.length > 0) {
    if (duplicatePolicy === 'consume' || duplicatePolicy === 'refund') {
      policy.duplicate_policy = duplicatePolicy;
    }
  }

  if (Object.keys(policy).length > 0) {
    patch.policy = policy;
  }

  return patch;
}

export function populateSettingsForm(form: HTMLFormElement, settings: Settings): void {
  const groupSizeInput = form.elements.namedItem('group_size') as HTMLInputElement | null;
  if (groupSizeInput) {
    groupSizeInput.value = String(settings.group_size);
  }
  const overlayInput = form.elements.namedItem('overlay_theme') as HTMLInputElement | null;
  if (overlayInput) {
    overlayInput.value = settings.overlay_theme;
  }

  const clearOnStreamStart = form.elements.namedItem('clear_on_stream_start') as HTMLInputElement | null;
  if (clearOnStreamStart) {
    clearOnStreamStart.checked = settings.clear_on_stream_start;
  }

  const clearDecrementCounts = form.elements.namedItem('clear_decrement_counts') as HTMLInputElement | null;
  if (clearDecrementCounts) {
    clearDecrementCounts.checked = settings.clear_decrement_counts;
  }

  const windowInput = form.elements.namedItem('policy.anti_spam_window_sec') as HTMLInputElement | null;
  if (windowInput) {
    windowInput.value = String(settings.policy.anti_spam_window_sec);
  }

  const duplicateSelect = form.elements.namedItem('policy.duplicate_policy') as HTMLSelectElement | null;
  if (duplicateSelect) {
    duplicateSelect.value = settings.policy.duplicate_policy;
  }
}
