import { afterEach, describe, expect, it, vi } from 'vitest';

import { AuthenticationError, sendMessage, subscribe } from './client';
import type { ClientConfig } from './client';

const config: ClientConfig = {
  baseUrl: 'http://127.0.0.1:7878/client',
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
