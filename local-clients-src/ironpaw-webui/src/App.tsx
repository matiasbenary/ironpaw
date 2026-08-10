import { useEffect, useRef, useState } from 'react';

import { Chat } from './Chat';
import { ConnectionScreen } from './ConnectionScreen';
import { Sidebar } from './Sidebar';
import {
  clearClientToken,
  defaultAddress,
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

const errorMessage = (error: unknown): string => error instanceof Error ? error.message : String(error);

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
  const [transients, setTransients] = useState<Record<string, TransientState>>({});
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const activeIdRef = useRef<string | null>(null);

  function updateTransient(id: string, patch: Partial<TransientState>) {
    setTransients((current) => ({
      ...current,
      [id]: { ...(current[id] || EMPTY_TRANSIENT), ...patch },
    }));
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

  useEffect(() => {
    if (!config) return;
    let cancelled = false;
    void listConversations()
      .then((stored) => {
        if (cancelled) return;
        setConversations(stored);
        void openConversation(stored[0]?.id || crypto.randomUUID());
      })
      .catch((error) => {
        if (cancelled) return;
        const id = crypto.randomUUID();
        activeIdRef.current = id;
        setActiveId(id);
        updateTransient(id, { error: errorMessage(error) });
      });
    return () => { cancelled = true; };
  }, [config]);

  useEffect(() => {
    if (!config) return;
    const controller = new AbortController();

    async function handleEvent(event: CallbackEvent) {
      const id = event.conversation_id;
      let messagesChanged = false;
      if (event.kind === 'message') {
        const externalId = event.id || crypto.randomUUID();
        await saveMessage({
          key: `${id}:assistant:${crypto.randomUUID()}`,
          conversation_id: id,
          external_id: externalId,
          role: 'assistant',
          text: event.content || '',
          attachments: event.attachments || [],
          tool_calls: event.tool_calls || [],
          created_at: Date.now(),
        });
        updateTransient(id, { activity: null });
        messagesChanged = true;
      } else if (event.kind === 'choice') {
        await saveMessage({
          key: `${id}:choice:${crypto.randomUUID()}`,
          conversation_id: id,
          external_id: event.id || crypto.randomUUID(),
          role: 'choice',
          question: event.question,
          options: event.options,
          created_at: Date.now(),
        });
        updateTransient(id, { activity: null });
        messagesChanged = true;
      } else if (event.kind === 'delete' && event.message_id) {
        messagesChanged = Boolean(await redactMessage(id, event.message_id));
      } else if (event.kind === 'activity') {
        updateTransient(id, {
          activity: typeof event.action === 'string'
            ? 'Writing…'
            : event.action?.progress?.text || 'Working…',
        });
      } else if (event.kind === 'error') {
        updateTransient(id, { error: event.message || 'Ironpaw reported an error.' });
      }
      if (messagesChanged) {
        await refreshConversations();
        if (activeIdRef.current === id) setMessages(await loadMessages(id));
      }
    }

    void subscribe(
      config,
      handleEvent,
      (connected) => {
        const id = activeIdRef.current;
        if (id) updateTransient(id, { error: connected ? null : 'Live updates disconnected. Reconnecting…' });
      },
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
      updateTransient(id, { error: errorMessage(error) });
      return false;
    }
  }

  async function answerChoice(value: string, choice: StoredMessage) {
    if (!(await postMessage(value, []))) return;
    const answered = { ...choice, answered: value };
    await saveMessage(answered);
    setMessages((current) => current.map((message) => message.key === choice.key ? answered : message));
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
      setConfig(candidate);
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
        conversationId={activeId}
        title={activeConversation?.title || 'New conversation'}
        canRename={Boolean(activeConversation)}
        messages={messages}
        transient={activeId ? transients[activeId] || EMPTY_TRANSIENT : EMPTY_TRANSIENT}
        onMenu={() => setSidebarOpen((open) => !open)}
        onRename={renameActive}
        onChoice={(value, message) => void answerChoice(value, message)}
        onSend={postMessage}
      />
      <button className="sidebar-backdrop" type="button" aria-label="Close conversations" onClick={() => setSidebarOpen(false)} />
    </div>
  );
}
