import type { Attachment, CallbackEvent } from './types';

export interface ClientConfig {
  baseUrl: string;
  clientId: string;
  token: string;
}

export interface OAuthCallback {
  state: string;
  code?: string;
  error?: string;
  error_description?: string;
}

interface OutgoingMessage {
  content: string;
  message_id: string;
  attachments: Attachment[];
}

const CLIENT_ID_KEY = 'ironpaw-client-id';
const ADDR_KEY = 'ironpaw-address';
const TOKEN_KEY = 'ironpaw-token';

export class AuthenticationError extends Error {}

async function requireSuccessful(response: Response, fallback: string): Promise<void> {
  if (response.ok) return;
  const message = (await response.text()) || fallback;
  if (response.status === 401 || response.status === 403) {
    throw new AuthenticationError(message);
  }
  throw new Error(message);
}

export function defaultAddress(): string {
  const query = new URLSearchParams(location.search);
  return query.get('ironpaw')
    || localStorage.getItem(ADDR_KEY)
    || 'http://127.0.0.1:7878';
}

export function loadClientConfig(): ClientConfig | null {
  const query = new URLSearchParams(location.search);
  const address = defaultAddress();
  const queryToken = query.get('token');
  const token = queryToken
    || sessionStorage.getItem(TOKEN_KEY);
  if (!token) return null;

  const config = saveClientConfig(address, token);
  if (queryToken) {
    query.delete('token');
    const url = new URL(location.href);
    url.search = query.toString();
    history.replaceState(null, '', url);
  }
  return config;
}

export function saveClientConfig(address: string, token: string): ClientConfig {
  let clientId = localStorage.getItem(CLIENT_ID_KEY);
  if (!clientId) {
    clientId = crypto.randomUUID();
    localStorage.setItem(CLIENT_ID_KEY, clientId);
  }
  localStorage.setItem(ADDR_KEY, address);
  sessionStorage.setItem(TOKEN_KEY, token);
  return {
    baseUrl: address.replace(/\/$/, ''),
    clientId,
    token,
  };
}

export function clearClientToken(): void {
  sessionStorage.removeItem(TOKEN_KEY);
}

export async function verifyClient(config: ClientConfig): Promise<void> {
  let response: Response;
  try {
    response = await fetch(`${config.baseUrl}/client/status`, {
      headers: { Authorization: `Bearer ${config.token}` },
    });
  } catch {
    throw new Error('Could not reach Ironpaw. Make sure it is running and restart it after backend changes.');
  }
  await requireSuccessful(response, 'Could not connect to Ironpaw');
}

export async function sendMessage(
  config: ClientConfig,
  conversationId: string,
  message: OutgoingMessage,
): Promise<void> {
  const response = await fetch(
    `${config.baseUrl}/client/conversations/${encodeURIComponent(conversationId)}/messages`,
    {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${config.token}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ client_id: config.clientId, ...message }),
    },
  );
  await requireSuccessful(response, 'Could not send message');
}

export async function completeOAuth(
  config: ClientConfig,
  callback: OAuthCallback,
): Promise<void> {
  const response = await fetch(`${config.baseUrl}/oauth/callback`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${config.token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(callback),
  });
  await requireSuccessful(response, 'Could not complete authorization');
}

export async function subscribe(
  config: ClientConfig,
  onEvent: (event: CallbackEvent) => Promise<void>,
  onStatus: (connected: boolean) => void,
  onAuthenticationRequired: () => void,
  signal?: AbortSignal,
): Promise<void> {
  while (!signal?.aborted) {
    try {
      const url = new URL(`${config.baseUrl}/client/events`);
      url.searchParams.set('client_id', config.clientId);
      const response = await fetch(url, {
        headers: { Authorization: `Bearer ${config.token}` },
        signal,
      });
      await requireSuccessful(response, `Event stream returned ${response.status}`);
      if (!response.body) throw new Error('Event stream did not return a body');
      onStatus(true);
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        const frames = buffer.split('\n\n');
        buffer = frames.pop() || '';
        for (const frame of frames) {
          const data = frame.split('\n').find((line) => line.startsWith('data: '))?.slice(6);
          if (data) await onEvent(JSON.parse(data) as CallbackEvent);
        }
      }
    } catch (error) {
      if (signal?.aborted) return;
      if (error instanceof AuthenticationError) {
        onAuthenticationRequired();
        return;
      }
      onStatus(false);
    }
    await new Promise((resolve) => window.setTimeout(resolve, 1000));
  }
}
