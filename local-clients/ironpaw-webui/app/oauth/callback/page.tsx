'use client';

import { useEffect, useState } from 'react';

import { OAuthCallbackPage } from '../../../src/OAuthCallback';
import type { OAuthCallback as OAuthCallbackResult } from '../../../src/client';

export default function OAuthCallback() {
  const [callback, setCallback] = useState<OAuthCallbackResult | null>();

  useEffect(() => {
    const query = new URLSearchParams(location.search);
    const state = query.get('state');
    const code = query.get('code');
    const error = query.get('error');
    setCallback(state && (code || error) ? {
      state,
      ...(code ? { code } : {}),
      ...(error ? { error } : {}),
      ...(query.get('error_description')
        ? { error_description: query.get('error_description')! }
        : {}),
    } : null);
  }, []);

  if (callback === undefined) return null;
  if (callback === null) return <main>Invalid OAuth callback.</main>;
  return <OAuthCallbackPage callback={callback} />;
}
