import { useState } from 'react';

import { Logo } from './Icon';

interface Credentials {
  address: string;
  token: string;
}

interface Props {
  initialAddress: string;
  onConnect: (credentials: Credentials) => Promise<void>;
}

export function ConnectionScreen({ initialAddress, onConnect }: Props) {
  const [address, setAddress] = useState(initialAddress);
  const [token, setToken] = useState('');
  const [error, setError] = useState('');
  const [connecting, setConnecting] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setConnecting(true);
    setError('');
    try {
      await onConnect({ address: address.trim(), token: token.trim() });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setConnecting(false);
    }
  }

  return (
    <main className="connection-screen">
      <form className="connection-card" onSubmit={submit}>
        <Logo className="connection-logo" />
        <h1>Connect to Ironpaw</h1>
        <p>Enter the address and bridge token printed by Ironpaw.</p>
        <label>
          Ironpaw address
          <input type="url" required value={address} onChange={(event) => setAddress(event.target.value)} />
        </label>
        <label>
          Bridge token
          <input type="password" autoComplete="current-password" required autoFocus value={token} onChange={(event) => setToken(event.target.value)} />
        </label>
        <div className="connection-error" role="alert">{error}</div>
        <button type="submit" disabled={connecting}>{connecting ? 'Connecting…' : 'Open Ironpaw'}</button>
      </form>
    </main>
  );
}
