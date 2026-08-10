import type { Conversation, StoredMessage } from './types';

const DB_NAME = 'ironpaw-webui';
const DB_VERSION = 1;

const openDatabase = (): Promise<IDBDatabase> => new Promise((resolve, reject) => {
  const request = indexedDB.open(DB_NAME, DB_VERSION);
  request.onerror = () => reject(request.error);
  request.onupgradeneeded = () => {
    const db = request.result;
    db.createObjectStore('conversations', { keyPath: 'id' });
    const messages = db.createObjectStore('messages', { keyPath: 'key' });
    messages.createIndex('conversation_id', 'conversation_id');
  };
  request.onsuccess = () => resolve(request.result);
});

const complete = (transaction: IDBTransaction): Promise<void> => new Promise((resolve, reject) => {
  transaction.oncomplete = () => resolve();
  transaction.onerror = () => reject(transaction.error);
  transaction.onabort = () => reject(transaction.error);
});

const result = <T>(request: IDBRequest<T>): Promise<T> => new Promise((resolve, reject) => {
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
});

const preview = (message: StoredMessage): string => message.text
  || message.question
  || message.attachments?.[0]?.filename
  || 'New message';

const shorten = (text: string, length: number): string => {
  const value = text.trim().replace(/\s+/g, ' ');
  return value.length > length ? `${value.slice(0, length)}…` : value;
};

export async function listConversations(): Promise<Conversation[]> {
  const db = await openDatabase();
  const transaction = db.transaction('conversations', 'readonly');
  const conversations = await result<Conversation[]>(transaction.objectStore('conversations').getAll());
  return conversations.sort((left, right) => right.updated_at - left.updated_at);
}

export async function loadMessages(conversationId: string): Promise<StoredMessage[]> {
  const db = await openDatabase();
  const transaction = db.transaction('messages', 'readonly');
  const messages = await result<StoredMessage[]>(
    transaction.objectStore('messages').index('conversation_id').getAll(conversationId),
  );
  return messages.sort((left, right) => left.created_at - right.created_at);
}

export async function saveMessage(message: StoredMessage): Promise<void> {
  const db = await openDatabase();
  const transaction = db.transaction(['messages', 'conversations'], 'readwrite');
  const messages = transaction.objectStore('messages');
  const conversations = transaction.objectStore('conversations');
  messages.put(message);
  const existing = await result<Conversation | undefined>(conversations.get(message.conversation_id));
  const text = preview(message);
  conversations.put({
    id: message.conversation_id,
    title: existing?.title || shorten(text, 44),
    preview: existing && existing.updated_at > message.created_at
      ? existing.preview
      : shorten(text, 64),
    updated_at: Math.max(existing?.updated_at || 0, message.created_at),
  });
  await complete(transaction);
}

export async function renameConversation(id: string, title: string): Promise<void> {
  const db = await openDatabase();
  const transaction = db.transaction('conversations', 'readwrite');
  const conversations = transaction.objectStore('conversations');
  const conversation = await result<Conversation | undefined>(conversations.get(id));
  if (conversation) conversations.put({ ...conversation, title });
  await complete(transaction);
}

export async function redactMessage(
  conversationId: string,
  externalId: string,
): Promise<StoredMessage | null> {
  const messages = await loadMessages(conversationId);
  const message = messages
    .slice()
    .reverse()
    .find((item) => item.external_id === externalId);
  if (!message) return null;
  message.text = '[redacted]';
  message.attachments = [];
  message.external_id = null;
  const db = await openDatabase();
  const transaction = db.transaction('messages', 'readwrite');
  transaction.objectStore('messages').put(message);
  await complete(transaction);
  return message;
}
