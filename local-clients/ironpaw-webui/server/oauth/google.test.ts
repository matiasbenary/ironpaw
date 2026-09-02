import { afterEach, describe, expect, it } from 'vitest';

import { authorizationUrl } from './google';

const originalClientId = process.env.GOOGLE_OAUTH_CLIENT_ID;
const originalClientSecret = process.env.GOOGLE_OAUTH_CLIENT_SECRET;

afterEach(() => {
  if (originalClientId === undefined) delete process.env.GOOGLE_OAUTH_CLIENT_ID;
  else process.env.GOOGLE_OAUTH_CLIENT_ID = originalClientId;
  if (originalClientSecret === undefined) delete process.env.GOOGLE_OAUTH_CLIENT_SECRET;
  else process.env.GOOGLE_OAUTH_CLIENT_SECRET = originalClientSecret;
});

describe('Google OAuth authorization', () => {
  it('uses the server client and accepts only registered callbacks and scopes', () => {
    process.env.GOOGLE_OAUTH_CLIENT_ID = 'server-client';
    process.env.GOOGLE_OAUTH_CLIENT_SECRET = 'server-secret';
    const url = new URL(authorizationUrl({
      redirectUri: 'http://localhost:3000/oauth/callback',
      state: 'state',
      codeChallenge: 'challenge',
      loginHint: 'user@example.com',
      scopes: ['https://www.googleapis.com/auth/gmail.readonly'],
    }));

    expect(url.searchParams.get('client_id')).toBe('server-client');
    expect(url.searchParams.get('state')).toBe('state');
    expect(() => authorizationUrl({
      redirectUri: 'https://attacker.example/callback',
      state: 'state',
      codeChallenge: 'challenge',
      loginHint: 'user@example.com',
      scopes: ['https://www.googleapis.com/auth/drive'],
    })).toThrow();
    expect(() => authorizationUrl({
      redirectUri: 'http://localhost:3000/oauth/callback',
      state: 'state',
      codeChallenge: 'challenge',
      loginHint: 'user@example.com',
      scopes: ['https://www.googleapis.com/auth/drive'],
    })).toThrow();
  });
});
