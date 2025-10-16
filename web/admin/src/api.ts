import type { StateSnapshot, SettingsPatch } from '@twi/shared-state';

export class ApiError extends Error {
  constructor(public status: number, message: string, public problem?: ProblemDetails) {
    super(message);
  }
}

export interface ProblemDetails {
  type: string;
  title: string;
  detail: string;
}

export interface FetchStateOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
}

export async function fetchState(options: FetchStateOptions): Promise<StateSnapshot> {
  const { baseUrl, broadcaster, token } = options;
  const url = new URL('/api/state', baseUrl);
  url.searchParams.set('broadcaster', broadcaster);
  url.searchParams.set('scope', 'session');

  const response = await fetch(url.toString(), {
    headers: {
      Authorization: `Bearer ${token}`,
    },
  });

  if (!response.ok) {
    const problem = await parseProblem(response);
    throw new ApiError(response.status, problem?.detail ?? 'failed to fetch state', problem);
  }

  return (await response.json()) as StateSnapshot;
}

export type QueueMutationMode = 'COMPLETE' | 'UNDO';

export interface QueueDequeueOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
  entryId: string;
  mode: QueueMutationMode;
  opId: string;
}

export async function queueDequeue(options: QueueDequeueOptions) {
  const { baseUrl, broadcaster, token, entryId, mode, opId } = options;
  const response = await fetch(new URL('/api/queue/dequeue', baseUrl).toString(), {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      broadcaster,
      entry_id: entryId,
      mode,
      op_id: opId,
    }),
  });

  if (!response.ok) {
    const problem = await parseProblem(response);
    throw new ApiError(response.status, problem?.detail ?? 'failed to update queue', problem);
  }

  return (await response.json()) as QueueDequeueResponse;
}

export interface SettingsUpdateOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
  patch: SettingsPatch;
  opId: string;
}

export async function updateSettings(options: SettingsUpdateOptions) {
  const { baseUrl, broadcaster, token, patch, opId } = options;
  const response = await fetch(new URL('/api/settings/update', baseUrl).toString(), {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ broadcaster, patch, op_id: opId }),
  });

  if (!response.ok) {
    const problem = await parseProblem(response);
    throw new ApiError(response.status, problem?.detail ?? 'failed to update settings', problem);
  }

  return (await response.json()) as SettingsUpdateResponse;
}

export interface QueueDequeueResponse {
  version: number;
  result: {
    entry_id: string;
    mode: QueueMutationMode;
    user_today_count: number;
  };
}

export interface SettingsUpdateResponse {
  version: number;
  result: {
    applied: boolean;
  };
}

export interface SseOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
  sinceVersion?: number;
}

export function createAdminSseConnection(options: SseOptions): EventSource {
  const { baseUrl, broadcaster, token, sinceVersion } = options;
  const url = new URL('/admin/sse', baseUrl);
  url.searchParams.set('broadcaster', broadcaster);
  url.searchParams.set('token', token);
  if (typeof sinceVersion === 'number' && sinceVersion > 0) {
    url.searchParams.set('since_version', String(sinceVersion));
  }
  return new EventSource(url.toString());
}

async function parseProblem(response: Response): Promise<ProblemDetails | undefined> {
  const contentType = response.headers.get('content-type');
  if (contentType && contentType.includes('application/problem+json')) {
    try {
      return (await response.json()) as ProblemDetails;
    } catch (error) {
      console.warn('failed to parse problem response', error);
    }
  }
  return undefined;
}
