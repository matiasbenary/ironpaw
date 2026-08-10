import { useLayoutEffect, useRef } from 'react';

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

function MessageView({ message, onChoice }: { message: StoredMessage; onChoice: (value: string, message: StoredMessage) => void }) {
  if (message.role === 'choice') {
    return (
      <section className="choice-card">
        <p>{message.question || 'Choose an option'}</p>
        {message.answered && <small className="choice-answer">Answered: {message.answered}</small>}
        <div className="choice-options">
          {(message.options || []).map((option) => (
            <button type="button" key={option.value} disabled={Boolean(message.answered)} onClick={() => onChoice(option.value, message)}>
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
            {message.role === 'assistant' ? <Markdown>{message.text}</Markdown> : message.text}
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
  messages: StoredMessage[];
  transient: TransientState;
  onChoice: (value: string, message: StoredMessage) => void;
}

export function MessageList({ messages, transient, onChoice }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const followLatest = useRef(true);

  useLayoutEffect(() => {
    if (followLatest.current && container.current) {
      container.current.scrollTop = container.current.scrollHeight;
    }
  }, [messages, transient]);

  return (
    <div
      className="messages"
      ref={container}
      onScroll={(event) => {
        const node = event.currentTarget;
        followLatest.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
      }}
    >
      {!messages.length ? (
        <section className="empty-chat">
          <span className="empty-mark"><Logo /></span>
          <h2>What can I help with?</h2>
          <p>Start a conversation with your personal agent.</p>
        </section>
      ) : messages.map((message) => (
        <MessageView message={message} onChoice={onChoice} key={message.key} />
      ))}
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
