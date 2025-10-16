import { describe, expect, it } from 'vitest';
import { parseAdminConfig } from './config';

describe('parseAdminConfig', () => {
  it('parses broadcaster and token and strips token from search', () => {
    const url = new URL('http://localhost:5174/?broadcaster=b-dev&token=abc123&foo=bar');
    const { config, sanitizedSearch } = parseAdminConfig(url);
    expect(config.broadcaster).toBe('b-dev');
    expect(config.token).toBe('abc123');
    expect(config.baseUrl).toBe('http://localhost:8080');
    expect(sanitizedSearch).toBe('broadcaster=b-dev&foo=bar');
  });

  it('throws when broadcaster is missing', () => {
    const url = new URL('http://localhost:5174/?token=abc');
    expect(() => parseAdminConfig(url)).toThrow('Missing broadcaster parameter.');
  });

  it('throws when token is missing', () => {
    const url = new URL('http://localhost:5174/?broadcaster=b-dev');
    expect(() => parseAdminConfig(url)).toThrow('Missing token parameter.');
  });
});
