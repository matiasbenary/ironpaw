import { Icon, Logo } from './Icon';
import type { Conversation } from './types';

interface Props {
  conversations: Conversation[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
}

export function Sidebar({ conversations, activeId, onSelect, onNew }: Props) {
  return (
    <aside className="sidebar">
      <div className="brand">
        <span className="brand-mark"><Logo /></span>
        <span>ironpaw</span>
        <button className="icon-button new-chat" type="button" title="New conversation" onClick={onNew}>
          <Icon name="plus" />
        </button>
      </div>
      <p className="sidebar-label">Chats</p>
      <nav className="conversation-list">
        {!conversations.length && <p className="empty-list">Your conversations will appear here.</p>}
        {conversations.map((conversation) => (
          <button
            className={`conversation-item${conversation.id === activeId ? ' active' : ''}`}
            type="button"
            key={conversation.id}
            onClick={() => onSelect(conversation.id)}
          >
            <span className="conversation-title">{conversation.title}</span>
          </button>
        ))}
      </nav>
    </aside>
  );
}
