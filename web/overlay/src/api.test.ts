import { describe, expect, it, vi } from 'vitest';
import { createSseConnection } from './api';

describe('createSseConnection', () => {
  it('constructs EventSource with broadcaster, token, and since_version', () => {
    const urls: string[] = [];
    const original = globalThis.EventSource;
    const fake = vi.fn().mockImplementation((url: string) => {
      urls.push(url);
      return { url } as unknown as EventSource;
    });
    // @ts-expect-error - replace for test
    globalThis.EventSource = fake;

    const source = createSseConnection({
      baseUrl: 'http://localhost:8080',
      broadcaster: 'b-dev',
      token: 'secret',
      sinceVersion: 42,
    });

    expect(source).toBeDefined();
    expect(urls).toHaveLength(1);
    const url = new URL(urls[0]!);
    expect(url.pathname).toBe('/overlay/sse');
    expect(url.searchParams.get('broadcaster')).toBe('b-dev');
    expect(url.searchParams.get('token')).toBe('secret');
    expect(url.searchParams.get('since_version')).toBe('42');

    globalThis.EventSource = original;
  });
});
