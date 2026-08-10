import type { Attachment, CallbackEvent } from './types';

export interface ClientConfig {
  baseUrl: string;
  clientId: string;
  token: string;
}

interface OutgoingMessage {
  content: string;
  message_id: string;
  attachments: Attachment[];
}

const CLIENT_ID_KEY = 'ironpaw-client-id';
const ADDR_KEY = 'ironpaw-address';
const TOKEN_KEY = 'ironpaw-token';

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
    baseUrl: `${address.replace(/\/$/, '')}/client`,
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
    response = await fetch(`${config.baseUrl}/status`, {
      headers: { Authorization: `Bearer ${config.token}` },
    });
  } catch {
    throw new Error('Could not reach Ironpaw. Make sure it is running and restart it after backend changes.');
  }
  if (!response.ok) throw new Error((await response.text()) || 'Could not connect to Ironpaw');
}

export async function sendMessage(
  config: ClientConfig,
  conversationId: string,
  message: OutgoingMessage,
): Promise<void> {
  const response = await fetch(
    `${config.baseUrl}/conversations/${encodeURIComponent(conversationId)}/messages`,
    {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${config.token}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ client_id: config.clientId, ...message }),
    },
  );
  if (!response.ok) throw new Error((await response.text()) || 'Could not send message');
}

export async function subscribe(
  config: ClientConfig,
  onEvent: (event: CallbackEvent) => Promise<void>,
  onStatus: (connected: boolean) => void,
  signal?: AbortSignal,
): Promise<void> {
  while (!signal?.aborted) {
    try {
      const url = new URL(`${config.baseUrl}/events`);
      url.searchParams.set('client_id', config.clientId);
      const response = await fetch(url, {
        headers: { Authorization: `Bearer ${config.token}` },
        signal,
      });
      if (!response.ok || !response.body) {
        throw new Error((await response.text()) || `Event stream returned ${response.status}`);
      }
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
    } catch {
      if (signal?.aborted) return;
      onStatus(false);
    }
    await new Promise((resolve) => window.setTimeout(resolve, 1000));
  }
}
