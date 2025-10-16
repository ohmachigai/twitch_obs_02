import { describe, expect, it } from 'vitest';
import { readSettingsPatch, populateSettingsForm } from './settings';
import type { Settings } from '@twi/shared-state';

describe('settings helpers', () => {
  it('extracts numeric, boolean, and nested policy fields from the form', () => {
    document.body.innerHTML = `
      <form id="settings-form">
        <input type="number" name="group_size" value="3" />
        <input type="text" name="overlay_theme" value="midnight" />
        <input type="checkbox" name="clear_on_stream_start" checked />
        <input type="checkbox" name="clear_decrement_counts" />
        <input type="number" name="policy.anti_spam_window_sec" value="120" />
        <select name="policy.duplicate_policy">
          <option value="consume">Consume</option>
          <option value="refund" selected>Refund</option>
        </select>
      </form>
    `;

    const form = document.getElementById('settings-form') as HTMLFormElement;
    const patch = readSettingsPatch(form);
    expect(patch.group_size).toBe(3);
    expect(patch.overlay_theme).toBe('midnight');
    expect(patch.clear_on_stream_start).toBe(true);
    expect(patch.clear_decrement_counts).toBe(false);
    expect(patch.policy?.anti_spam_window_sec).toBe(120);
    expect(patch.policy?.duplicate_policy).toBe('refund');
  });

  it('populates form controls from settings', () => {
    document.body.innerHTML = `
      <form id="settings-form">
        <input type="number" name="group_size" />
        <input type="text" name="overlay_theme" />
        <input type="checkbox" name="clear_on_stream_start" />
        <input type="checkbox" name="clear_decrement_counts" />
        <input type="number" name="policy.anti_spam_window_sec" />
        <select name="policy.duplicate_policy">
          <option value="consume">Consume</option>
          <option value="refund">Refund</option>
        </select>
      </form>
    `;

    const form = document.getElementById('settings-form') as HTMLFormElement;
    const settings: Settings = {
      overlay_theme: 'default',
      group_size: 2,
      clear_on_stream_start: true,
      clear_decrement_counts: true,
      policy: {
        anti_spam_window_sec: 90,
        duplicate_policy: 'consume',
        target_rewards: [],
      },
    };

    populateSettingsForm(form, settings);

    expect((form.elements.namedItem('group_size') as HTMLInputElement).value).toBe('2');
    expect((form.elements.namedItem('overlay_theme') as HTMLInputElement).value).toBe('default');
    expect((form.elements.namedItem('clear_on_stream_start') as HTMLInputElement).checked).toBe(true);
    expect((form.elements.namedItem('clear_decrement_counts') as HTMLInputElement).checked).toBe(true);
    expect((form.elements.namedItem('policy.anti_spam_window_sec') as HTMLInputElement).value).toBe('90');
    expect((form.elements.namedItem('policy.duplicate_policy') as HTMLSelectElement).value).toBe('consume');
  });
});
