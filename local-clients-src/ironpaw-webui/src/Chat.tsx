import { useEffect, useRef, useState } from 'react';

import { Composer } from './Composer';
import { Icon } from './Icon';
import { MessageList } from './MessageList';
import type { StoredMessage, TransientState } from './types';

interface HeaderProps {
  title: string;
  canRename: boolean;
  onMenu: () => void;
  onRename: (title: string) => Promise<void>;
}

function ChatHeader({ title, canRename, onMenu, onRename }: HeaderProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(title);
  const cancel = useRef(false);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => { if (!editing) setDraft(title); }, [title, editing]);
  useEffect(() => { if (editing) input.current?.select(); }, [editing]);

  function finish() {
    setEditing(false);
    if (cancel.current || !draft.trim()) {
      cancel.current = false;
      setDraft(title);
      return;
    }
    void onRename(draft.trim());
  }

  return (
    <header className="chat-header">
      <button className="icon-button menu-button" type="button" aria-label="Open conversations" onClick={onMenu}><Icon name="menu" /></button>
      {editing ? (
        <input
          className="chat-title-input"
          ref={input}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onBlur={finish}
          onKeyDown={(event) => {
            if (event.key === 'Enter') event.currentTarget.blur();
            if (event.key === 'Escape') {
              cancel.current = true;
              event.currentTarget.blur();
            }
          }}
        />
      ) : <h1>{title}</h1>}
      <button className="icon-button chat-rename" type="button" title="Rename conversation" aria-label="Rename conversation" disabled={!canRename} onClick={() => setEditing(true)}>
        <Icon name="edit" />
      </button>
    </header>
  );
}

interface Props {
  conversationId: string | null;
  title: string;
  canRename: boolean;
  messages: StoredMessage[];
  transient: TransientState;
  onMenu: () => void;
  onRename: (title: string) => Promise<void>;
  onChoice: (value: string, message: StoredMessage) => void;
  onSend: (text: string, files: File[]) => Promise<boolean>;
}

export function Chat(props: Props) {
  return (
    <section className="chat">
      <ChatHeader title={props.title} canRename={props.canRename} onMenu={props.onMenu} onRename={props.onRename} />
      <MessageList key={props.conversationId} messages={props.messages} transient={props.transient} onChoice={props.onChoice} />
      <Composer onSend={props.onSend} />
    </section>
  );
}
