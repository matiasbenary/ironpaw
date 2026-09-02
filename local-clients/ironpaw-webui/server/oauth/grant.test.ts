import { afterEach, describe, expect, it } from 'vitest';

import { openGrant, sealGrant, verifyProof } from './grant';
import { createHmac, randomBytes } from 'node:crypto';

const originalKey = process.env.OAUTH_GRANT_KEY;

afterEach(() => {
  if (originalKey === undefined) delete process.env.OAUTH_GRANT_KEY;
  else process.env.OAUTH_GRANT_KEY = originalKey;
});

describe('OAuth grants', () => {
  it('encrypts credentials and requires proof from the bound instance', () => {
    process.env.OAUTH_GRANT_KEY = randomBytes(32).toString('base64url');
    const bindingKey = randomBytes(32).toString('base64url');
    const grant = sealGrant({ provider: 'google', refreshToken: 'refresh', bindingKey });
    const proof = createHmac('sha256', bindingKey).update(grant).digest('base64url');

    expect(grant).not.toContain('refresh');
    expect(openGrant(grant)).toEqual({ provider: 'google', refreshToken: 'refresh', bindingKey });
    expect(verifyProof(grant, bindingKey, proof)).toBe(true);
    expect(verifyProof(grant, 'different-instance', proof)).toBe(false);
  });
});
