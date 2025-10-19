import {
  applyPatch,
  createClientState,
  VersionMismatchError,
  type ClientState,
  type Patch,
  type QueueEntry,
  type QueueReorderUpdate,
} from '@twi/shared-state';
import {
  ApiError,
  createAdminSseConnection,
  fetchState,
  queueDequeue,
  queueReorder,
  updateSettings,
  type QueueMutationMode,
} from './api';
import { parseAdminConfig, type AdminConfig } from './config';
import { populateSettingsForm, readSettingsPatch } from './settings';
import { formatRelativeTime } from './time';
import { buildReorderEntries, computeReorderOrder } from './reorder';

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
let reorderInFlight = false;
let dragSourceId: string | null = null;
let dropIndicatorTarget: HTMLLIElement | null = null;

function canReorder(): boolean {
  return Boolean(clientState && !clientState.settings.prioritize_low_counts);
}

const statusEl = document.getElementById('connection-status') as HTMLDivElement | null;
const alertsEl = document.getElementById('alerts') as HTMLDivElement | null;
const queueListEl = document.getElementById('queue-list') as HTMLUListElement | null;
const queueEmptyEl = document.getElementById('queue-empty') as HTMLDivElement | null;
const reorderDisabledEl = document.getElementById('queue-reorder-disabled') as HTMLDivElement | null;
const completedListEl = document.getElementById('completed-list') as HTMLUListElement | null;
const completedEmptyEl = document.getElementById('completed-empty') as HTMLDivElement | null;
const countersTable = document.getElementById('counters-table') as HTMLTableElement | null;
const countersEmpty = document.getElementById('counters-empty') as HTMLDivElement | null;
const settingsForm = document.getElementById('settings-form') as HTMLFormElement | null;

const RELATIVE_TIME_INTERVAL_MS = 30_000;

interface RelativeTimeNode {
  element: HTMLElement;
  timestamp: Date;
  absolute: string;
  label: string;
}

let relativeTimeNodes: RelativeTimeNode[] = [];
let relativeTimeTimer: number | null = null;

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
  renderCompleted();
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
  queueListEl.classList.remove('queue-list--drop-end');
  dropIndicatorTarget = null;
  const prioritize = clientState.settings.prioritize_low_counts;
  if (reorderDisabledEl) {
    reorderDisabledEl.hidden = !prioritize || clientState.queue.length === 0;
  }
  queueListEl.classList.toggle('queue--reorder-disabled', prioritize);
  if (prioritize) {
    dragSourceId = null;
  }
  if (clientState.queue.length === 0) {
    queueEmptyEl.hidden = false;
    return;
  }

  queueEmptyEl.hidden = true;

  for (const entry of clientState.queue) {
    const item = document.createElement('li');
    item.className = 'queue-item';
    item.draggable = !prioritize;
    item.dataset.entryId = entry.id;
    if (!prioritize) {
      item.addEventListener('dragstart', (event) => handleQueueDragStart(event, entry.id));
      item.addEventListener('dragend', handleQueueDragEnd);
    }

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
    metaContainer.appendChild(createStatusBadge(entry));
    header.appendChild(metaContainer);

    const actions = document.createElement('div');
    actions.className = 'queue-actions';

    const completeButton = document.createElement('button');
    completeButton.textContent = 'Complete';
    completeButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'COMPLETE', completeButton);
    });

    actions.appendChild(completeButton);
    const cancelButton = document.createElement('button');
    cancelButton.textContent = 'Cancel';
    cancelButton.classList.add('danger');
    cancelButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'CANCEL', cancelButton);
    });
    actions.appendChild(cancelButton);

    item.appendChild(header);
    item.appendChild(actions);
    queueListEl.appendChild(item);
  }
}

function renderCompleted() {
  if (!clientState || !completedListEl || !completedEmptyEl) {
    return;
  }

  completedListEl.innerHTML = '';
  relativeTimeNodes = [];

  if (clientState.completed.length === 0) {
    completedEmptyEl.hidden = false;
    refreshRelativeTimeTimer();
    return;
  }

  completedEmptyEl.hidden = true;

  for (const entry of clientState.completed) {
    const item = document.createElement('li');

    const header = document.createElement('header');
    const name = document.createElement('div');
    name.textContent = entry.user_display_name ?? entry.user_login;
    header.appendChild(name);

    const metaContainer = document.createElement('div');
    metaContainer.className = 'queue-meta';
    const meta = document.createElement('small');
    meta.className = 'completed-meta';
    if (entry.completed_at) {
      const completedAt = new Date(entry.completed_at);
      const absolute = completedAt.toLocaleString();
      meta.textContent = `Completed ${absolute} (${formatRelativeTime(completedAt)})`;
      registerRelativeTime(meta, completedAt, 'Completed', absolute);
    } else {
      meta.textContent = 'Completed time unavailable';
    }
    metaContainer.appendChild(meta);
    metaContainer.appendChild(createStatusBadge(entry));
    header.appendChild(metaContainer);

    const actions = document.createElement('div');
    actions.className = 'queue-actions';

    const undoButton = document.createElement('button');
    undoButton.textContent = 'Undo';
    undoButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'UNDO', undoButton);
    });
    actions.appendChild(undoButton);

    const cancelButton = document.createElement('button');
    cancelButton.textContent = 'Cancel';
    cancelButton.classList.add('danger');
    cancelButton.addEventListener('click', () => {
      void handleQueueAction(entry.id, 'CANCEL', cancelButton);
    });
    actions.appendChild(cancelButton);

    item.appendChild(header);
    item.appendChild(actions);
    completedListEl.appendChild(item);
  }

  refreshRelativeTimeTimer();
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

function registerRelativeTime(
  element: HTMLElement,
  timestamp: Date,
  label: string,
  absolute: string
) {
  relativeTimeNodes.push({ element, timestamp, label, absolute });
}

function updateRelativeTimeNodes(now = new Date()) {
  for (const node of relativeTimeNodes) {
    const relative = formatRelativeTime(node.timestamp, now);
    node.element.textContent = `${node.label} ${node.absolute} (${relative})`;
  }
}

function stopRelativeTimeTimer() {
  if (relativeTimeTimer !== null) {
    window.clearInterval(relativeTimeTimer);
    relativeTimeTimer = null;
  }
}

function refreshRelativeTimeTimer() {
  if (relativeTimeNodes.length === 0) {
    stopRelativeTimeTimer();
    return;
  }

  updateRelativeTimeNodes();
  if (relativeTimeTimer === null) {
    relativeTimeTimer = window.setInterval(() => {
      updateRelativeTimeNodes();
    }, RELATIVE_TIME_INTERVAL_MS);
  }
}

function createStatusBadge(entry: QueueEntry): HTMLSpanElement {
  const status = document.createElement('span');
  status.className = entry.managed
    ? 'queue-status queue-status--managed'
    : 'queue-status queue-status--manual';
  status.textContent = entry.managed ? 'Managed' : 'Manual';
  return status;
}

function handleQueueDragStart(event: DragEvent, entryId: string) {
  if (reorderInFlight || !canReorder()) {
    event.preventDefault();
    return;
  }
  dragSourceId = entryId;
  if (event.dataTransfer) {
    event.dataTransfer.effectAllowed = 'move';
    event.dataTransfer.setData('text/plain', entryId);
  }
  const target = event.currentTarget as HTMLElement | null;
  target?.classList.add('is-dragging');
}

function handleQueueDragEnd(event: DragEvent) {
  const target = event.currentTarget as HTMLElement | null;
  target?.classList.remove('is-dragging');
  dragSourceId = null;
  updateDropIndicator(null);
  queueListEl?.classList.remove('queue-list--drop-end');
}

function handleQueueDragOver(event: DragEvent) {
  if (!dragSourceId || reorderInFlight || !queueListEl || !canReorder()) {
    return;
  }
  event.preventDefault();
  const afterElement = getDragAfterElement(queueListEl, event.clientY);
  updateDropIndicator(afterElement);
}

function handleQueueDrop(event: DragEvent) {
  if (!dragSourceId || !queueListEl || !clientState || !canReorder()) {
    return;
  }
  event.preventDefault();
  const afterElement = getDragAfterElement(queueListEl, event.clientY);
  const beforeId = afterElement?.dataset.entryId ?? null;
  updateDropIndicator(null);
  queueListEl.classList.remove('queue-list--drop-end');
  const currentOrder = clientState.queue.map((entry) => entry.id);
  const nextOrder = computeReorderOrder(currentOrder, dragSourceId, beforeId);
  dragSourceId = null;
  if (!nextOrder) {
    return;
  }
  void submitQueueReorder(nextOrder);
}

function getDragAfterElement(
  container: HTMLUListElement,
  y: number
): HTMLLIElement | null {
  const elements = Array.from(
    container.querySelectorAll<HTMLLIElement>('li.queue-item:not(.is-dragging)')
  );
  let closest: { offset: number; element: HTMLLIElement | null } = {
    offset: Number.NEGATIVE_INFINITY,
    element: null,
  };
  for (const element of elements) {
    const box = element.getBoundingClientRect();
    const offset = y - (box.top + box.height / 2);
    if (offset < 0 && offset > closest.offset) {
      closest = { offset, element };
    }
  }
  return closest.element;
}

function updateDropIndicator(target: HTMLLIElement | null) {
  if (dropIndicatorTarget && dropIndicatorTarget !== target) {
    dropIndicatorTarget.classList.remove('queue-item--drop-before');
  }
  if (target) {
    target.classList.add('queue-item--drop-before');
  }
  dropIndicatorTarget = target;
  if (queueListEl) {
    queueListEl.classList.toggle('queue-list--drop-end', !target && dragSourceId !== null);
  }
}

async function submitQueueReorder(order: string[]) {
  if (!config || !clientState || reorderInFlight || !canReorder()) {
    return;
  }
  const updates = buildReorderEntries(order);
  reorderInFlight = true;
  try {
    const opId = crypto.randomUUID();
    const response = await queueReorder({
      baseUrl: config.baseUrl,
      broadcaster: config.broadcaster,
      token: config.token,
      entries: updates,
      opId,
    });
    applyOptimisticReorder(order, updates);
    showAlert('success', `Reorder accepted (version ${response.version}).`);
  } catch (error) {
    handleError(error);
  } finally {
    reorderInFlight = false;
  }
}

function applyOptimisticReorder(order: string[], updates: QueueReorderUpdate[]) {
  if (!clientState || !canReorder()) {
    return;
  }
  const updateMap = new Map(updates.map((entry) => [entry.entry_id, entry.display_order]));
  const lookup = new Map(clientState.queue.map((entry) => [entry.id, entry]));
  const nextQueue: QueueEntry[] = [];
  for (const id of order) {
    const existing = lookup.get(id);
    if (existing) {
      const displayOrder = updateMap.get(id) ?? existing.display_order;
      nextQueue.push({ ...existing, display_order: displayOrder });
    }
  }
  clientState = {
    ...clientState,
    queue: nextQueue,
  };
  renderQueue();
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

  if (queueListEl) {
    queueListEl.addEventListener('dragover', handleQueueDragOver);
    queueListEl.addEventListener('drop', handleQueueDrop);
  }

  window.addEventListener('beforeunload', () => {
    closeSse();
    stopRelativeTimeTimer();
  });

  void loadInitialState();
}

init();
