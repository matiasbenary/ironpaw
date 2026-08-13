import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

export function stripMessageIdPrefix(text: string): string {
  return text.replace(/^\[message_id:[A-Za-z0-9_-]{1,128}\]\s*/, '');
}

export function Markdown({ children, onLink }: { children: string; onLink?: (href: string) => void }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
        a: ({ children, href, node: _node, ...props }) => (
          <a {...props} href={href} target="_blank" rel="noreferrer" onClick={() => href && onLink?.(href)}>
            {children}
          </a>
        ),
      }}
    >
      {stripMessageIdPrefix(children)}
    </ReactMarkdown>
  );
}
