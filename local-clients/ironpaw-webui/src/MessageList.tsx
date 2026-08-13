import { Fragment, useLayoutEffect, useRef } from 'react';

import { attachmentUrl } from './attachments';
import { Icon, Logo } from './Icon';
import { Markdown } from './Markdown';
import type { Attachment, StoredMessage, TransientState } from './types';

function AttachmentView({ attachment }: { attachment: Attachment }) {
  const url = attachmentUrl(attachment);
  if (attachment.mime_type.startsWith('image/') && url) {
    return (
      <figure className="image-attachment">
        <img src={url} alt={attachment.filename} />
        <figcaption>{attachment.filename}</figcaption>
      </figure>
    );
  }
  return (
    <a className={`file-attachment${url ? '' : ' unavailable'}`} href={url || undefined} download={url ? attachment.filename : undefined}>
      <Icon name="file" /><span>{attachment.filename}</span>
    </a>
  );
}

function MessageView({ disabled, message, onChoice, onLink }: { disabled: boolean; message: StoredMessage; onChoice: (value: string, message: StoredMessage) => void; onLink: (href: string) => void }) {
  if (message.role === 'choice') {
    return (
      <section className="choice-card">
        <p>{message.question || 'Choose an option'}</p>
        {message.answered && <small className="choice-answer">Answered: {message.answered}</small>}
        <div className="choice-options">
          {(message.options || []).map((option) => (
            <button type="button" key={option.value} disabled={disabled || Boolean(message.answered)} onClick={() => onChoice(option.value, message)}>
              {option.label}
            </button>
          ))}
        </div>
      </section>
    );
  }
  return (
    <article className={`message-row ${message.role}`}>
      <div className="message-bubble">
        {message.text && (
          <div className="message-text">
            {message.role === 'assistant' ? <Markdown onLink={onLink}>{message.text}</Markdown> : message.text}
          </div>
        )}
        {(message.attachments || []).map((attachment, index) => (
          <AttachmentView attachment={attachment} key={`${attachment.filename}:${index}`} />
        ))}
        {Boolean(message.tool_calls?.length) && (
          <details className="tool-details">
            <summary>{message.tool_calls!.length} tool call{message.tool_calls!.length === 1 ? '' : 's'}</summary>
            <pre>{JSON.stringify(message.tool_calls, null, 2)}</pre>
          </details>
        )}
      </div>
    </article>
  );
}

interface Props {
  connectionState: 'connected' | 'offline' | 'authentication-required';
  disabled?: boolean;
  messages: StoredMessage[];
  newMessageKey?: string;
  transient: TransientState;
  onChoice: (value: string, message: StoredMessage) => void;
  onLink: (href: string) => void;
  onReadNewMessages: () => void;
  onReconnect: () => void;
}

export function MessageList({ connectionState, disabled = false, messages, newMessageKey, transient, onChoice, onLink, onReadNewMessages, onReconnect }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const followLatest = useRef(true);
  const userIsScrolling = useRef(false);

  useLayoutEffect(() => {
    if (followLatest.current && container.current) {
      container.current.scrollTop = container.current.scrollHeight;
    }
  }, [messages, transient, connectionState]);

  return (
    <div
      className="messages"
      ref={container}
      onWheel={() => { userIsScrolling.current = true; }}
      onTouchMove={() => { userIsScrolling.current = true; }}
      onScroll={(event) => {
        const node = event.currentTarget;
        followLatest.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
        const divider = node.querySelector<HTMLElement>('.new-messages-divider');
        if (userIsScrolling.current && divider && divider.getBoundingClientRect().top < node.getBoundingClientRect().bottom) {
          userIsScrolling.current = false;
          onReadNewMessages();
        }
      }}
    >
      {!messages.length ? (
        <section className="empty-chat">
          <span className="empty-mark"><Logo /></span>
          <h2>What can I help with?</h2>
          <p>Start a conversation with your personal agent.</p>
        </section>
      ) : messages.map((message) => (
        <Fragment key={message.key}>
          {message.key === newMessageKey && (
            <div className="new-messages-divider" role="separator" aria-label="New messages">
              <span>New messages</span>
            </div>
          )}
          <MessageView disabled={disabled} message={message} onChoice={onChoice} onLink={onLink} />
        </Fragment>
      ))}
      {connectionState === 'offline' && <div className="error-card">Ironpaw is down.</div>}
      {connectionState === 'authentication-required' && (
        <div className="error-card">
          Ironpaw needs to <button type="button" onClick={onReconnect}>reconnect</button>.
        </div>
      )}
      {transient.error && <div className="error-card">{transient.error}</div>}
      {transient.activity && (
        <div className="activity-row">
          <span className="typing-dots"><i /><i /><i /></span>
          <span>{transient.activity}</span>
        </div>
      )}
    </div>
  );
}
