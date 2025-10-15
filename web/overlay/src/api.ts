import type { StateSnapshot } from './types';

export interface ProblemDetails {
  type: string;
  title: string;
  detail: string;
}

export class ApiError extends Error {
  constructor(public status: number, message: string, public problem?: ProblemDetails) {
    super(message);
  }
}

export interface FetchStateOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
  scope?: 'session' | 'since';
  since?: string;
}

export async function fetchState(options: FetchStateOptions): Promise<StateSnapshot> {
  const { baseUrl, broadcaster, token, scope = 'session', since } = options;
  const url = new URL('/api/state', baseUrl);
  url.searchParams.set('broadcaster', broadcaster);
  url.searchParams.set('scope', scope);
  if (scope === 'since' && since) {
    url.searchParams.set('since', since);
  }

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

export interface SseOptions {
  baseUrl: string;
  broadcaster: string;
  token: string;
  types?: string[];
  sinceVersion?: number;
}

export function createSseConnection(options: SseOptions): EventSource {
  const { baseUrl, broadcaster, token, types, sinceVersion } = options;
  const url = new URL('/overlay/sse', baseUrl);
  url.searchParams.set('broadcaster', broadcaster);
  url.searchParams.set('token', token);
  if (types && types.length > 0) {
    url.searchParams.set('types', types.join(','));
  }
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
