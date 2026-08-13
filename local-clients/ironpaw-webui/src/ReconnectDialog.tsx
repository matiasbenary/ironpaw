import { useState } from 'react';

interface Props {
  initialAddress: string;
  onCancel: () => void;
  onConnect: (address: string, token: string) => Promise<void>;
}

export function ReconnectDialog({ initialAddress, onCancel, onConnect }: Props) {
  const [address, setAddress] = useState(initialAddress);
  const [token, setToken] = useState('');
  const [error, setError] = useState('');
  const [connecting, setConnecting] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setConnecting(true);
    setError('');
    try {
      await onConnect(address.trim(), token.trim());
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setConnecting(false);
    }
  }

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onCancel();
    }}>
      <form className="reconnect-dialog" role="dialog" aria-modal="true" aria-labelledby="reconnect-title" onSubmit={submit}>
        <h2 id="reconnect-title">Reconnect to Ironpaw</h2>
        <p>Ironpaw restarted. Enter the new bridge token to continue.</p>
        <label>
          Ironpaw address
          <input type="url" required value={address} onChange={(event) => setAddress(event.target.value)} />
        </label>
        <label>
          Bridge token
          <input type="password" required autoFocus value={token} onChange={(event) => setToken(event.target.value)} />
        </label>
        <div className="connection-error" role="alert">{error}</div>
        <div className="dialog-actions">
          <button type="button" onClick={onCancel}>Cancel</button>
          <button className="primary-button" type="submit" disabled={connecting}>
            {connecting ? 'Connecting…' : 'Reconnect'}
          </button>
        </div>
      </form>
    </div>
  );
}
