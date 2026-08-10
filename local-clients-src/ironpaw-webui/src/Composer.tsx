import { useRef, useState } from 'react';

import { Icon } from './Icon';

export function Composer({ onSend }: { onSend: (text: string, files: File[]) => Promise<boolean> }) {
  const [text, setText] = useState('');
  const [files, setFiles] = useState<File[]>([]);
  const [sending, setSending] = useState(false);
  const form = useRef<HTMLFormElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (sending || (!text.trim() && !files.length)) return;
    setSending(true);
    if (await onSend(text, files)) {
      setText('');
      setFiles([]);
      if (fileInput.current) fileInput.current.value = '';
      if (textarea.current) textarea.current.style.height = 'auto';
    }
    setSending(false);
  }

  return (
    <form className="composer-wrap" ref={form} onSubmit={submit}>
      <div className="pending-files">
        {files.map((file) => <span className="pending-file" key={`${file.name}:${file.lastModified}`}>{file.name}</span>)}
      </div>
      <div className="composer">
        <button className="attach-button" type="button" aria-label="Attach file" onClick={() => fileInput.current?.click()}>
          <Icon name="attach" />
        </button>
        <input ref={fileInput} type="file" multiple hidden onChange={(event) => setFiles(Array.from(event.target.files || []))} />
        <textarea
          ref={textarea}
          rows={1}
          placeholder="Message Ironpaw"
          aria-label="Message"
          value={text}
          onChange={(event) => {
            setText(event.target.value);
            event.target.style.height = 'auto';
            event.target.style.height = `${Math.min(event.target.scrollHeight, 160)}px`;
          }}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              form.current?.requestSubmit();
            }
          }}
        />
        <button className="send-button" type="submit" aria-label="Send" disabled={sending}>
          <Icon name="send" />
        </button>
      </div>
      <p className="composer-hint">Enter to send · Shift + Enter for a new line</p>
    </form>
  );
}
