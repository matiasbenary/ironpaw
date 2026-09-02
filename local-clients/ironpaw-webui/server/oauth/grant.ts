import 'server-only';

import {
  createCipheriv,
  createDecipheriv,
  createHmac,
  randomBytes,
  timingSafeEqual,
} from 'node:crypto';

export interface Grant {
  provider: string;
  refreshToken: string;
  bindingKey: string;
}

function encryptionKey(): Buffer {
  const key = Buffer.from(process.env.OAUTH_GRANT_KEY || '', 'base64url');
  if (key.length !== 32) throw new Error('OAUTH_GRANT_KEY must contain 32 base64url bytes');
  return key;
}

export function sealGrant(grant: Grant): string {
  const nonce = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', encryptionKey(), nonce);
  const ciphertext = Buffer.concat([
    cipher.update(JSON.stringify(grant), 'utf8'),
    cipher.final(),
  ]);
  return ['v1', nonce, ciphertext, cipher.getAuthTag()]
    .map((part) => typeof part === 'string' ? part : part.toString('base64url'))
    .join('.');
}

export function openGrant(value: string): Grant {
  const [version, nonce, ciphertext, tag] = value.split('.');
  if (version !== 'v1' || !nonce || !ciphertext || !tag) throw new Error('invalid OAuth grant');
  const decipher = createDecipheriv(
    'aes-256-gcm',
    encryptionKey(),
    Buffer.from(nonce, 'base64url'),
  );
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  return JSON.parse(Buffer.concat([
    decipher.update(Buffer.from(ciphertext, 'base64url')),
    decipher.final(),
  ]).toString('utf8')) as Grant;
}

export function verifyProof(grant: string, bindingKey: string, proof: string): boolean {
  const expected = createHmac('sha256', bindingKey).update(grant).digest();
  const supplied = Buffer.from(proof, 'base64url');
  return supplied.length === expected.length && timingSafeEqual(supplied, expected);
}

