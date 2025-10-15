import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import './App.css';
import { ApiError, createSseConnection, fetchState } from './api';
import { applyPatch, createClientState, VersionMismatchError } from './state';
import type { ClientState } from './state';
import type { Patch } from './types';

type ConnectionStatus = 'idle' | 'loading' | 'live' | 'reconnecting' | 'error';

type OverlayConfig = {
  broadcaster: string;
  token: string;
  types?: string[];
  debug: boolean;
  groupSizeOverride?: number;
};

function App() {
  const [config, setConfig] = useState<OverlayConfig | null>(null);
  const [status, setStatus] = useState<ConnectionStatus>('idle');
  const [error, setError] = useState<string | null>(null);
  const [clientState, setClientState] = useState<ClientState | null>(null);
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const [debugLogs, setDebugLogs] = useState<string[]>([]);
  const eventSourceRef = useRef<EventSource | null>(null);

  const apiBase = useMemo(() => {
    return window.location.origin.replace(/:\d+$/, ':8080');
  }, []);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const broadcaster = params.get('broadcaster');
    const token = params.get('token');
    if (!broadcaster) {
      setError('Missing broadcaster parameter.');
      return;
    }
    if (!token) {
      setError('Missing token parameter.');
      return;
    }

    if (params.has('token')) {
      params.delete('token');
      const query = params.toString();
      const newUrl = `${window.location.pathname}${query ? `?${query}` : ''}`;
      window.history.replaceState({}, document.title, newUrl);
    }

    const typesParam = params.get('types');
    const types = typesParam
      ? typesParam
          .split(',')
          .map((value) => value.trim())
          .filter((value) => value.length > 0)
      : undefined;
    const debug = params.get('debug') === '1' || params.get('debug') === 'true';
    const groupSizeRaw = params.get('group_size');
    const groupSize = groupSizeRaw ? Number.parseInt(groupSizeRaw, 10) : undefined;

    setConfig({
      broadcaster,
      token,
      types,
      debug,
      groupSizeOverride: Number.isFinite(groupSize) && groupSize ? groupSize : undefined,
    });
  }, []);

  const storageKeys = useMemo(() => {
    if (!config) {
      return null;
    }
    const prefix = `overlay:${config.broadcaster}`;
    return {
      version: `${prefix}:lastVersion`,
      seenAt: `${prefix}:lastSeenAt`,
    };
  }, [config]);

  const persistVersion = useCallback(
    (version: number, at: string) => {
      if (!storageKeys) {
        return;
      }
      try {
        localStorage.setItem(storageKeys.version, String(version));
        localStorage.setItem(storageKeys.seenAt, at);
      } catch (err) {
        console.warn('failed to persist version metadata', err);
      }
    },
    [storageKeys]
  );

  const closeSse = useCallback(() => {
    if (eventSourceRef.current) {
      eventSourceRef.current.close();
      eventSourceRef.current = null;
    }
  }, []);

  const connectSse = useCallback(
    (sinceVersion?: number) => {
      if (!config) {
        return;
      }
      closeSse();
      const source = createSseConnection({
        baseUrl: apiBase,
        broadcaster: config.broadcaster,
        token: config.token,
        types: config.types,
        sinceVersion,
      });
      eventSourceRef.current = source;

      source.addEventListener('open', () => {
        setStatus('live');
      });
      source.addEventListener('error', () => {
        setStatus('reconnecting');
      });
      source.addEventListener('patch', (event) => {
        try {
          const patch = JSON.parse((event as MessageEvent<string>).data) as Patch;
          if (config.debug) {
            setDebugLogs((logs) => {
              const next = [`${patch.type}@${patch.version}`].concat(logs);
              return next.slice(0, 20);
            });
          }
          setClientState((current) => {
            if (!current) {
              return current;
            }
            try {
              const nextState = applyPatch(current, patch);
              persistVersion(nextState.version, patch.at);
              return nextState;
            } catch (err) {
              if (err instanceof VersionMismatchError) {
                setError('Version mismatch detected. Re-syncing…');
                setNeedsRefresh(true);
                closeSse();
              } else {
                console.error('failed to apply patch', err);
                setError('Failed to apply update.');
              }
              return current;
            }
          });
        } catch (err) {
          console.error('failed to parse patch', err);
        }
      });
    },
    [apiBase, closeSse, config, persistVersion]
  );

  const loadState = useCallback(
    async (scope: 'session' | 'since' = 'session') => {
      if (!config) {
        return;
      }
      setStatus('loading');
      try {
        const snapshot = await fetchState({
          baseUrl: apiBase,
          broadcaster: config.broadcaster,
          token: config.token,
          scope,
        });
        const state = createClientState(snapshot);
        setClientState(state);
        persistVersion(state.version, new Date().toISOString());
        setError(null);
        connectSse(state.version);
        setNeedsRefresh(false);
      } catch (err) {
        closeSse();
        if (err instanceof ApiError) {
          setError(err.problem?.detail ?? `Request failed with status ${err.status}`);
        } else {
          console.error(err);
          setError('Unable to load overlay state.');
        }
        setStatus('error');
      }
    },
    [apiBase, closeSse, config, connectSse, persistVersion]
  );

  useEffect(() => {
    if (config) {
      loadState('session');
    }
    return () => {
      closeSse();
    };
  }, [closeSse, config, loadState]);

  useEffect(() => {
    if (config && needsRefresh) {
      loadState('session');
    }
  }, [config, loadState, needsRefresh]);

  const statusLabel = useMemo(() => {
    switch (status) {
      case 'idle':
        return 'Idle';
      case 'loading':
        return 'Loading…';
      case 'live':
        return 'Live';
      case 'reconnecting':
        return 'Reconnecting…';
      case 'error':
        return 'Error';
      default:
        return status;
    }
  }, [status]);

  const groupSize = useMemo(() => {
    if (config?.groupSizeOverride) {
      return config.groupSizeOverride;
    }
    return clientState?.settings.group_size ?? 0;
  }, [clientState?.settings.group_size, config?.groupSizeOverride]);

  const queueToRender = useMemo(() => {
    if (!clientState) {
      return [];
    }
    const limit = groupSize && groupSize > 0 ? groupSize : clientState.queue.length;
    return clientState.queue.slice(0, limit);
  }, [clientState, groupSize]);

  return (
    <div className="overlay">
      <header className={`hud hud--${status}`}>
        <span className="hud__status">{statusLabel}</span>
        {clientState && <span className="hud__version">v{clientState.version}</span>}
      </header>
      {error && <div className="overlay__error">{error}</div>}
      {!error && !clientState && <div className="overlay__loading">Loading overlay…</div>}
      {clientState && (
        <main className="queue">
          {queueToRender.length === 0 ? (
            <div className="queue__empty">Queue is currently empty</div>
          ) : (
            <ul className="queue__list">
              {queueToRender.map((entry) => (
                <li key={entry.id} className="queue__entry">
                  <span className="queue__name">{entry.user_display_name}</span>
                  <span className="queue__meta">
                    #{clientState.counters[entry.user_id] ?? 0}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </main>
      )}
      {config?.debug && debugLogs.length > 0 && (
        <aside className="overlay__debug">
          <h2>Debug</h2>
          <ul>
            {debugLogs.map((log, index) => (
              <li key={index}>{log}</li>
            ))}
          </ul>
        </aside>
      )}
    </div>
  );
}

export default App;
