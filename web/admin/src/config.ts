export interface AdminConfig {
  broadcaster: string;
  token: string;
  baseUrl: string;
}

export interface ConfigParseResult {
  config: AdminConfig;
  sanitizedSearch: string;
}

export function parseAdminConfig(url: URL): ConfigParseResult {
  const params = new URLSearchParams(url.search);
  const broadcaster = params.get('broadcaster');
  const token = params.get('token');

  if (!broadcaster) {
    throw new Error('Missing broadcaster parameter.');
  }
  if (!token) {
    throw new Error('Missing token parameter.');
  }

  params.delete('token');
  const sanitizedSearch = params.toString();
  const baseUrl = url.origin.replace(/:\d+$/, ':8080');

  return {
    config: { broadcaster, token, baseUrl },
    sanitizedSearch,
  };
}
