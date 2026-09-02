import { afterEach, describe, expect, it, vi } from 'vitest';

import { AuthenticationError, completeOAuth, sendMessage, subscribe } from './client';
import type { ClientConfig } from './client';

const config: ClientConfig = {
  baseUrl: 'http://127.0.0.1:7878',
  clientId: 'browser-client',
  token: 'expired',
};

afterEach(() => vi.unstubAllGlobals());

describe('authentication failures', () => {
  it('identifies an expired token when sending', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('invalid token', { status: 401 })));

    await expect(sendMessage(config, 'chat', {
      content: 'hello',
      message_id: 'message',
      attachments: [],
    })).rejects.toBeInstanceOf(AuthenticationError);
  });

  it('stops reconnecting and requests authentication', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('invalid token', { status: 401 })));
    const onAuthenticationRequired = vi.fn();

    await subscribe(config, vi.fn(), vi.fn(), onAuthenticationRequired);

    expect(onAuthenticationRequired).toHaveBeenCalledOnce();
    expect(fetch).toHaveBeenCalledOnce();
  });
});

describe('OAuth callback', () => {
  it('forwards the one-use result to Ironpaw', async () => {
    const fetch = vi.fn(async () => new Response('{"ok":true}'));
    vi.stubGlobal('fetch', fetch);

    await completeOAuth(config, { state: 'state', code: 'code' });

    expect(fetch).toHaveBeenCalledWith('http://127.0.0.1:7878/oauth/callback', {
      method: 'POST',
      headers: {
        Authorization: 'Bearer expired',
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ state: 'state', code: 'code' }),
    });
  });
});
