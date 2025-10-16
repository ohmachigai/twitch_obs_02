import {
  applyPatch,
  createClientState,
  VersionMismatchError,
  type ClientState,
  type Patch,
} from '@twi/shared-state';
import {
  ApiError,
  createAdminSseConnection,
  fetchState,
  queueDequeue,
  updateSettings,
  type QueueMutationMode,
} from './api';
import { parseAdminConfig, type AdminConfig } from './config';
import { populateSettingsForm, readSettingsPatch } from './settings';

type ConnectionStatus = 'idle' | 'loading' | 'live' | 'reconnecting' | 'error';

declare global {
  interface Window {
    adminDebug?: boolean;
  }
}

let config: AdminConfig | null = null;
let clientState: ClientState | null = null;
let eventSource: EventSource | null = null;
let pendingResync = false;

const statusEl = document.getElementById('connection-status') as HTMLDivElement | null;
const alertsEl = document.getElementById('alerts') as HTMLDivElement | null;
const queueListEl = document.getElementById('queue-list') as HTMLUListElement | null;
const queueEmptyEl = document.getElementById('queue-empty') as HTMLDivElement | null;
const countersTable = document.getElementById('counters-table') as HTMLTableElement | null;
const countersEmpty = document.getElementById('counters-empty') as HTMLDivElement | null;
const settingsForm = document.getElementById('settings-form') as HTMLFormElement | null;

function setStatus(status: ConnectionStatus, message?: string) {
  if (!statusEl) {
    return;
  }
  statusEl.textContent = message ?? status.toUpperCase();
  statusEl.className = `status status-${status}`;
}

function showAlert(kind: 'error' | 'success', message: string) {
  if (!alertsEl) {
    return;
  }
  const container = document.createElement('div');
  container.className = `alert alert-${kind}`;
  container.textContent = message;
  alertsEl.appendChild(container);
}

function clearAlerts() {
  if (!alertsEl) {
    return;
  }
  alertsEl.innerHTML = '';
}

function closeSse() {
  if (eventSource) {
    eventSource.close();
    eventSource = null;
  }
}

async function loadInitialState() {
  if (!config) {
    return;
  }
  setStatus('loading', 'Loading…');
  clearAlerts();
  try {
    const snapshot = await fetchState({
      baseUrl: config.baseUrl,
      broadcaster: config.broadcaster,
      token: config.token,
    });
    clientState = createClientState(snapshot);
    renderState();
    setStatus('live', 'Connected');
    connectSse(clientState.version);
  } catch (error) {
    handleError(error);
    setStatus('error', 'Failed to load');
  }
}

function connectSse(sinceVersion?: number) {
  if (!config) {
    return;
  }
  closeSse();
  eventSource = createAdminSseConnection({
    baseUrl: config.baseUrl,
    broadcaster: config.broadcaster,
    token: config.token,
    sinceVersion,
  });

  eventSource.addEventListener('open', () => {
    setStatus('live', 'Connected');
  });

  eventSource.addEventListener('error', () => {
    setStatus('reconnecting', 'Reconnecting…');
  });

  eventSource.addEventListener('patch', (event) => {
    if (!clientState) {
      return;
    }
    try {
      const patch = JSON.parse((event as MessageEvent<string>).data) as Patch;
      if (patch.type === 'redemption.updated') {
        handleRedemptionPatch(patch);
      }
      clientState = applyPatch(clientState, patch);
      renderState();
    } catch (err) {
      if (err instanceof VersionMismatchError) {
        scheduleResync();
      } else {
        console.error('failed to apply patch', err);
        showAlert('error', 'Failed to apply update.');
      }
    }
  });
}

function scheduleResync() {
  if (pendingResync || !config) {
    return;
  }
  pendingResync = true;
  closeSse();
  setStatus('reconnecting', 'Resyncing…');
  setTimeout(() => {
    pendingResync = false;
    void loadInitialState();
  }, 500);
}

function renderState() {
  if (!clientState) {
    return;
  }
  renderQueue();
  renderCounters();
  if (settingsForm) {
    populateSettingsForm(settingsForm, clientState.settings);
  }
}

function renderQueue() {
  if (!clientState || !queueListEl || !queueEmptyEl) {
    return;
  }

  queueListEl.innerHTML = '';
  if (clientState.queue.length === 0) {
    queueEmptyEl.hidden = false;
    return;
  }

  queueEmptyEl.hidden = true;

  for (const entry of clientState.queue) {
    const item = document.createElement('li');

    const header = document.createElement('header');
    const name = document.createElement('div');
    name.textContent = entry.user_display_name ?? entry.user_login;
    const meta = document.createElement('small');
    const enqueuedAt = new Date(entry.enqueued_at).toLocaleString();
    meta.textContent = `Enqueued ${enqueuedAt}`;
    header.appendChild(name);
    const metaContainer = document.createElement('div');
    metaContainer.className = 'queue-meta';
    metaContainer.appendChild(meta);
    const status = document.createElement('span');
    status.className = entry.managed
      ? 'queue-status queue-status--managed'
      : 'queue-status queue-status--manual';
    status.textContent = entry.managed ? 'Managed' : 'Manual';
    metaContainer.appendChild(status);
    header.appendChild(metaContainer);

    const actions = document.createElement('div');
    actions.className = 'queue-actions';

    const completeButton = document.createElement('button');
    completeButton.textContent = 'Complete';
    completeButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'COMPLETE', completeButton);
    });

    const undoButton = document.createElement('button');
    undoButton.textContent = 'Undo';
    undoButton.classList.add('secondary');
    undoButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'UNDO', undoButton);
    });

    actions.appendChild(completeButton);
    actions.appendChild(undoButton);

    item.appendChild(header);
    item.appendChild(actions);
    queueListEl.appendChild(item);
  }
}

function renderCounters() {
  if (!clientState || !countersTable || !countersEmpty) {
    return;
  }

  const entries = Object.entries(clientState.counters).sort(([, a], [, b]) => b - a);
  const tbody = countersTable.tBodies[0] ?? countersTable.createTBody();
  tbody.innerHTML = '';

  if (entries.length === 0) {
    countersTable.hidden = true;
    countersEmpty.hidden = false;
    return;
  }

  countersTable.hidden = false;
  countersEmpty.hidden = true;

  for (const [userId, count] of entries) {
    const row = document.createElement('tr');
    const userCell = document.createElement('td');
    userCell.textContent = userId;
    const countCell = document.createElement('td');
    countCell.textContent = String(count);
    row.appendChild(userCell);
    row.appendChild(countCell);
    tbody.appendChild(row);
  }
}

async function handleQueueAction(entryId: string, mode: QueueMutationMode, button: HTMLButtonElement) {
  if (!config) {
    return;
  }
  button.disabled = true;
  try {
    const opId = crypto.randomUUID();
    const response = await queueDequeue({
      baseUrl: config.baseUrl,
      broadcaster: config.broadcaster,
      token: config.token,
      entryId,
      mode,
      opId,
    });
    showAlert('success', `${mode} accepted (version ${response.version}).`);
  } catch (error) {
    handleError(error);
  } finally {
    button.disabled = false;
  }
}

async function handleSettingsSubmit(event: SubmitEvent) {
  event.preventDefault();
  if (!config || !settingsForm) {
    return;
  }
  const submitButton = settingsForm.querySelector('button[type="submit"]') as HTMLButtonElement | null;
  if (submitButton) {
    submitButton.disabled = true;
  }
  try {
    const patch = readSettingsPatch(settingsForm);
    const opId = crypto.randomUUID();
    const response = await updateSettings({
      baseUrl: config.baseUrl,
      broadcaster: config.broadcaster,
      token: config.token,
      patch,
      opId,
    });
    showAlert('success', `Settings updated (version ${response.version}).`);
  } catch (error) {
    handleError(error);
  } finally {
    if (submitButton) {
      submitButton.disabled = false;
    }
  }
}

function handleError(error: unknown) {
  if (error instanceof ApiError) {
    showAlert('error', error.problem?.detail ?? `Request failed (${error.status})`);
  } else if (error instanceof Error) {
    showAlert('error', error.message);
  } else {
    showAlert('error', 'Unexpected error occurred.');
  }
}

function handleRedemptionPatch(patch: Extract<Patch, { type: 'redemption.updated' }>) {
  if (patch.data.result === 'failed') {
    const reason = patch.data.error ?? 'twitch:error';
    showAlert('error', `Helix update failed (${reason}). Entry requires manual handling.`);
  } else if (!patch.data.applicable && patch.data.error) {
    showAlert('error', `Helix skipped (${patch.data.error}). Entry remains manual.`);
  }
}

function init() {
  try {
    const url = new URL(window.location.href);
    const result = parseAdminConfig(url);
    config = result.config;
    if (result.sanitizedSearch !== url.search.slice(1)) {
      const newUrl = `${url.pathname}${result.sanitizedSearch ? `?${result.sanitizedSearch}` : ''}`;
      window.history.replaceState({}, document.title, newUrl);
    }
  } catch (error) {
    handleError(error);
    setStatus('error', 'Configuration error');
    return;
  }

  if (settingsForm) {
    settingsForm.addEventListener('submit', (event) => {
      void handleSettingsSubmit(event);
    });
  }

  window.addEventListener('beforeunload', () => {
    closeSse();
  });

  void loadInitialState();
}

init();
