'use client';

import { useEffect, useState } from 'react';

import type { OAuthCallback } from './client';

export const OAUTH_CALLBACK_CHANNEL = 'ironpaw-oauth-callback';

export function OAuthCallbackPage({ callback }: { callback: OAuthCallback }) {
  const [message, setMessage] = useState('Returning authorization to Ironpaw…');

  useEffect(() => {
    history.replaceState(null, '', '/oauth/callback');
    const channel = new BroadcastChannel(OAUTH_CALLBACK_CHANNEL);
    const send = window.setInterval(() => channel.postMessage(callback), 250);
    const timeout = window.setTimeout(() => {
      window.clearInterval(send);
      setMessage('Could not reach the original Ironpaw tab. Return there and try again.');
      channel.close();
    }, 10_000);

    channel.postMessage(callback);
    channel.onmessage = ({ data }: MessageEvent<{ state?: string; received?: boolean }>) => {
      if (data.state !== callback.state || !data.received) return;
      window.clearInterval(send);
      window.clearTimeout(timeout);
      setMessage('Authorization returned to Ironpaw. You can now close this window.');
      window.setTimeout(() => {
        channel.close();
        window.close();
      }, 100);
    };

    return () => {
      window.clearInterval(send);
      window.clearTimeout(timeout);
      channel.close();
    };
  }, [callback]);

  return <main>{message}</main>;
}
