import { useEffect, useRef, useState } from 'react';

import { Chat } from './Chat';
import { ConnectionScreen } from './ConnectionScreen';
import { ReconnectDialog } from './ReconnectDialog';
import { Sidebar } from './Sidebar';
import {
  AuthenticationError,
  clearClientToken,
  defaultAddress,
  emailOauthComplete,
  loadClientConfig,
  saveClientConfig,
  sendMessage,
  subscribe,
  verifyClient,
} from './client';
import type { ClientConfig } from './client';
import {
  listConversations,
  loadMessages,
  redactMessage,
  renameConversation,
  saveMessage,
} from './db';
import type { Attachment, CallbackEvent, Conversation, StoredMessage, TransientState } from './types';

const EMPTY_TRANSIENT: TransientState = { activity: null, error: null };
type ConnectionState = 'connected' | 'offline' | 'authentication-required';

const errorMessage = (error: unknown): string => error instanceof Error ? error.message : String(error);

export function upsertMessage(current: StoredMessage[], changed: StoredMessage): StoredMessage[] {
  const existing = current.findIndex((message) => message.key === changed.key);
  if (existing === -1) return [...current, changed];
  return current.map((message, index) => index === existing ? changed : message);
}

const fileAsBase64 = (file: File): Promise<string> => new Promise((resolve, reject) => {
  const reader = new FileReader();
  reader.onload = () => {
    const encoded = String(reader.result).split(',')[1];
    if (encoded) resolve(encoded);
    else reject(new Error(`Could not read ${file.name}`));
  };
  reader.onerror = () => reject(reader.error);
  reader.readAsDataURL(file);
});

export function App() {
  const [config, setConfig] = useState<ClientConfig | null>(() => loadClientConfig());
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [messages, setMessages] = useState<StoredMessage[]>([]);
  const [newMessageKeys, setNewMessageKeys] = useState<Record<string, string>>({});
  const [transients, setTransients] = useState<Record<string, TransientState>>({});
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [connectionState, setConnectionState] = useState<ConnectionState>('connected');
  const [reconnectOpen, setReconnectOpen] = useState(false);
  const activeIdRef = useRef<string | null>(null);
  const oauthStates = useRef(new Set<string>());
  const connectedRef = useRef(false);
  const greetingConfigRef = useRef(config ? `${config.baseUrl}:${config.token}` : null);
  const greetingPending = useRef(Boolean(config));

  function updateTransient(id: string, patch: Partial<TransientState>) {
    setTransients((current) => ({
      ...current,
      [id]: { ...(current[id] || EMPTY_TRANSIENT), ...patch },
    }));
  }

  function clearNewMessageMarker(id: string) {
    setNewMessageKeys((current) => {
      if (!current[id]) return current;
      const next = { ...current };
      delete next[id];
      return next;
    });
  }

  async function refreshConversations() {
    const stored = await listConversations();
    setConversations(stored);
    return stored;
  }

  async function openConversation(id: string) {
    activeIdRef.current = id;
    setActiveId(id);
    setMessages([]);
    setSidebarOpen(false);
    try {
      const stored = await loadMessages(id);
      if (activeIdRef.current === id) setMessages(stored);
      updateTransient(id, { error: null });
    } catch (error) {
      updateTransient(id, { error: `Could not load this conversation: ${errorMessage(error)}` });
    }
  }

  function newConversation() {
    void openConversation(crypto.randomUUID());
  }

  function requireAuthentication() {
    clearClientToken();
    setConnectionState('authentication-required');
    const id = activeIdRef.current;
    if (id) updateTransient(id, { activity: null, error: null });
  }

  function greetIfReady(candidate: ClientConfig) {
    const id = activeIdRef.current;
    const configKey = `${candidate.baseUrl}:${candidate.token}`;
    if (!connectedRef.current || !greetingPending.current || !id) return;
    greetingPending.current = false;
    void sendMessage(candidate, id, {
      content: 'hi',
      message_id: crypto.randomUUID(),
      attachments: [],
    }).catch((error) => {
      if (greetingConfigRef.current !== configKey) return;
      if (error instanceof AuthenticationError) requireAuthentication();
    });
  }

  useEffect(() => {
    if (!config) return;
    let cancelled = false;
    void listConversations()
      .then((stored) => {
        if (cancelled) return;
        setConversations(stored);
        void openConversation(stored[0]?.id || crypto.randomUUID());
        greetIfReady(config);
      })
      .catch((error) => {
        if (cancelled) return;
        const id = crypto.randomUUID();
        activeIdRef.current = id;
        setActiveId(id);
        updateTransient(id, { error: errorMessage(error) });
        greetIfReady(config);
      });
    return () => { cancelled = true; };
  }, [config]);

  useEffect(() => {
    if (!config) return;
    const controller = new AbortController();
    const configKey = `${config.baseUrl}:${config.token}`;
    if (greetingConfigRef.current !== configKey) {
      greetingConfigRef.current = configKey;
      greetingPending.current = true;
    }
    connectedRef.current = false;

    async function handleEvent(event: CallbackEvent) {
      const id = event.conversation_id;
      let changedMessage: StoredMessage | null = null;
      let marksNewMessage = false;
      if (event.kind === 'message') {
        const externalId = event.id || crypto.randomUUID();
        changedMessage = {
          key: `${id}:assistant:${crypto.randomUUID()}`,
          conversation_id: id,
          external_id: externalId,
          role: 'assistant',
          text: event.content || '',
          attachments: event.attachments || [],
          tool_calls: event.tool_calls || [],
          created_at: Date.now(),
        };
        await saveMessage(changedMessage);
        marksNewMessage = true;
        updateTransient(id, { activity: null });
      } else if (event.kind === 'choice') {
        const externalId = event.id || crypto.randomUUID();
        changedMessage = {
          key: `${id}:choice:${externalId}`,
          conversation_id: id,
          external_id: externalId,
          role: 'choice',
          question: event.question,
          options: event.options,
          created_at: Date.now(),
        };
        await saveMessage(changedMessage);
        marksNewMessage = true;
        updateTransient(id, { activity: null });
      } else if (event.kind === 'delete' && event.message_id) {
        changedMessage = await redactMessage(id, event.message_id);
      } else if (event.kind === 'activity') {
        updateTransient(id, {
          activity: typeof event.action === 'string'
            ? 'Writing…'
            : event.action?.progress?.text || 'Working…',
        });
      } else if (event.kind === 'error') {
        updateTransient(id, {
          activity: null,
          error: event.message || 'Ironpaw reported an error.',
        });
      }
      if (changedMessage) {
        if (marksNewMessage) {
          setNewMessageKeys((current) => current[id]
            ? current
            : { ...current, [id]: changedMessage.key });
        }
        await refreshConversations();
        if (activeIdRef.current === id) {
          setMessages((current) => upsertMessage(current, changedMessage));
        }
      }
    }

    void subscribe(
      config,
      handleEvent,
      (connected) => {
        connectedRef.current = connected;
        setConnectionState(connected ? 'connected' : 'offline');
        if (!connected) {
          setTransients((current) => Object.fromEntries(
            Object.entries(current).map(([id, transient]) => [id, { ...transient, activity: null }]),
          ));
        }
        if (connected) greetIfReady(config);
      },
      requireAuthentication,
      controller.signal,
    );
    return () => controller.abort();
  }, [config]);

  async function postMessage(text: string, files: File[]): Promise<boolean> {
    const id = activeIdRef.current;
    if (!config || !id) return false;
    if (files.length > 4) {
      updateTransient(id, { error: 'A message can include at most 4 attachments.' });
      return false;
    }
    const tooLarge = files.find((file) => file.size > 10 * 1024 * 1024);
    if (tooLarge) {
      updateTransient(id, { error: `${tooLarge.name} exceeds the 10 MB limit.` });
      return false;
    }
    try {
      const externalId = crypto.randomUUID();
      const attachments: Attachment[] = await Promise.all(files.map(async (file) => ({
        filename: file.name,
        mime_type: file.type || 'application/octet-stream',
        data_base64: await fileAsBase64(file),
      })));
      await sendMessage(config, id, { content: text, message_id: externalId, attachments });
      clearNewMessageMarker(id);
      const message: StoredMessage = {
        key: `${id}:${externalId}`,
        conversation_id: id,
        external_id: externalId,
        role: 'user',
        text: text.trim(),
        attachments,
        tool_calls: [],
        created_at: Date.now(),
      };
      await saveMessage(message);
      if (activeIdRef.current === id) setMessages((current) => [...current, message]);
      updateTransient(id, { error: null });
      await refreshConversations();
      return true;
    } catch (error) {
      if (error instanceof AuthenticationError) {
        requireAuthentication();
        return false;
      }
      updateTransient(id, { error: errorMessage(error) });
      return false;
    }
  }

  async function answerChoice(value: string, choice: StoredMessage) {
    const answered = { ...choice, answered: value };
    await saveMessage(answered);
    setMessages((current) => current.map((message) => message.key === choice.key ? answered : message));
    if (await postMessage(value, [])) return;
    await saveMessage(choice);
    setMessages((current) => current.map((message) => message.key === choice.key ? choice : message));
  }

  function watchOauth(href: string) {
    if (!config) return;
    const url = new URL(href);
    if (url.hostname !== 'accounts.google.com' && url.hostname !== 'login.microsoftonline.com') return;
    const state = url.searchParams.get('state');
    const conversationId = activeIdRef.current;
    if (!state || !conversationId || oauthStates.current.has(state)) return;
    oauthStates.current.add(state);
    void (async () => {
      try {
        for (let attempt = 0; attempt < 300; attempt += 1) {
          await new Promise((resolve) => window.setTimeout(resolve, 1000));
          if (await emailOauthComplete(config, state)) {
            await sendMessage(config, conversationId, {
              content: 'done',
              message_id: crypto.randomUUID(),
              attachments: [],
            });
            return;
          }
        }
        updateTransient(conversationId, { error: 'Gmail authorization timed out.' });
      } catch (error) {
        updateTransient(conversationId, { error: errorMessage(error) });
      } finally {
        oauthStates.current.delete(state);
      }
    })();
  }

  async function renameActive(title: string) {
    const id = activeIdRef.current;
    if (!id) return;
    setConversations((current) => current.map((item) => item.id === id ? { ...item, title } : item));
    await renameConversation(id, title);
  }

  async function connect(address: string, token: string) {
    const candidate = saveClientConfig(address, token);
    try {
      await verifyClient(candidate);
      greetingConfigRef.current = `${candidate.baseUrl}:${candidate.token}`;
      greetingPending.current = true;
      connectedRef.current = false;
      setConfig(candidate);
      setConnectionState('connected');
      setReconnectOpen(false);
    } catch (error) {
      clearClientToken();
      throw error;
    }
  }

  if (!config) {
    return <ConnectionScreen initialAddress={defaultAddress()} onConnect={({ address, token }) => connect(address, token)} />;
  }

  const activeConversation = conversations.find((item) => item.id === activeId);
  return (
    <div className={`shell${sidebarOpen ? ' sidebar-open' : ''}`}>
      <Sidebar conversations={conversations} activeId={activeId} onSelect={(id) => void openConversation(id)} onNew={newConversation} />
      <Chat
        connectionState={connectionState}
        conversationId={activeId}
        title={activeConversation?.title || 'New conversation'}
        canRename={Boolean(activeConversation)}
        messages={messages}
        newMessageKey={activeId ? newMessageKeys[activeId] : undefined}
        transient={activeId ? transients[activeId] || EMPTY_TRANSIENT : EMPTY_TRANSIENT}
        onMenu={() => setSidebarOpen((open) => !open)}
        onRename={renameActive}
        onReconnect={() => setReconnectOpen(true)}
        onChoice={(value, message) => void answerChoice(value, message)}
        onLink={watchOauth}
        onReadNewMessages={() => activeId && clearNewMessageMarker(activeId)}
        onSend={postMessage}
      />
      <button className="sidebar-backdrop" type="button" aria-label="Close conversations" onClick={() => setSidebarOpen(false)} />
      {reconnectOpen && (
        <ReconnectDialog
          initialAddress={defaultAddress()}
          onCancel={() => setReconnectOpen(false)}
          onConnect={(address, token) => connect(address, token)}
        />
      )}
    </div>
  );
}
