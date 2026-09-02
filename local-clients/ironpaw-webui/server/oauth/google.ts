import 'server-only';

const AUTHORIZE_URL = 'https://accounts.google.com/o/oauth2/v2/auth';
const TOKEN_URL = 'https://oauth2.googleapis.com/token';
const ALLOWED_SCOPES = new Set([
  'https://www.googleapis.com/auth/gmail.readonly',
  'https://www.googleapis.com/auth/gmail.modify',
  'https://www.googleapis.com/auth/gmail.send',
]);

function credentials() {
  const clientId = process.env.GOOGLE_OAUTH_CLIENT_ID;
  const clientSecret = process.env.GOOGLE_OAUTH_CLIENT_SECRET;
  if (!clientId || !clientSecret) throw new Error('Google OAuth is not configured');
  return { clientId, clientSecret };
}

function validateRedirect(redirectUri: string): void {
  const allowed = (process.env.OAUTH_CALLBACK_URLS
    || 'http://localhost:3000/oauth/callback,https://ironpaw.app/oauth/callback')
    .split(',')
    .map((value) => value.trim());
  if (!allowed.includes(redirectUri)) throw new Error('OAuth callback URL is not allowed');
}

function validateScopes(scopes: string[]): void {
  if (!scopes.length || scopes.some((scope) => !ALLOWED_SCOPES.has(scope))) {
    throw new Error('unsupported Google OAuth scope');
  }
}

export function authorizationUrl(input: {
  redirectUri: string;
  state: string;
  codeChallenge: string;
  loginHint: string;
  scopes: string[];
}): string {
  validateRedirect(input.redirectUri);
  validateScopes(input.scopes);
  const { clientId } = credentials();
  const url = new URL(AUTHORIZE_URL);
  url.searchParams.set('client_id', clientId);
  url.searchParams.set('redirect_uri', input.redirectUri);
  url.searchParams.set('response_type', 'code');
  url.searchParams.set('scope', input.scopes.join(' '));
  url.searchParams.set('state', input.state);
  url.searchParams.set('code_challenge', input.codeChallenge);
  url.searchParams.set('code_challenge_method', 'S256');
  url.searchParams.set('login_hint', input.loginHint);
  url.searchParams.set('access_type', 'offline');
  url.searchParams.set('prompt', 'consent');
  return url.toString();
}

export async function exchangeCode(input: {
  code: string;
  codeVerifier: string;
  redirectUri: string;
}): Promise<string> {
  validateRedirect(input.redirectUri);
  const { clientId, clientSecret } = credentials();
  const token = await tokenRequest({
    client_id: clientId,
    client_secret: clientSecret,
    code: input.code,
    code_verifier: input.codeVerifier,
    grant_type: 'authorization_code',
    redirect_uri: input.redirectUri,
  });
  if (!token.refresh_token) throw new Error('Google returned no refresh token');
  return token.refresh_token;
}

export async function refreshAccessToken(refreshToken: string): Promise<string> {
  const { clientId, clientSecret } = credentials();
  const token = await tokenRequest({
    client_id: clientId,
    client_secret: clientSecret,
    grant_type: 'refresh_token',
    refresh_token: refreshToken,
  });
  if (!token.access_token) throw new Error('Google returned no access token');
  return token.access_token;
}

async function tokenRequest(form: Record<string, string>): Promise<{
  access_token?: string;
  refresh_token?: string;
}> {
  const response = await fetch(TOKEN_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams(form),
    cache: 'no-store',
  });
  if (!response.ok) throw new Error(`Google token request failed (${response.status})`);
  return response.json() as Promise<{ access_token?: string; refresh_token?: string }>;
}
