export interface Attachment {
  filename: string;
  mime_type: string;
  data_base64?: string;
}

export interface ChoiceOption {
  label: string;
  value: string;
}

export interface StoredMessage {
  key: string;
  conversation_id: string;
  external_id: string | null;
  role: 'user' | 'assistant' | 'choice';
  created_at: number;
  text?: string;
  attachments?: Attachment[];
  tool_calls?: unknown[];
  question?: string;
  options?: ChoiceOption[];
  answered?: string;
}

export interface Conversation {
  id: string;
  title: string;
  preview: string;
  updated_at: number;
}

export interface TransientState {
  activity: string | null;
  error: string | null;
}

export interface CallbackEvent {
  conversation_id: string;
  kind: 'message' | 'choice' | 'delete' | 'activity' | 'error';
  id?: string;
  content?: string;
  attachments?: Attachment[];
  tool_calls?: unknown[];
  question?: string;
  options?: ChoiceOption[];
  message_id?: string;
  action?: string | { progress?: { text?: string } };
  message?: string;
}
