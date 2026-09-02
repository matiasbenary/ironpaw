# Ironpaw Web UI

The local client and OAuth broker for Ironpaw, built with Next.js.

```bash
npm install
cp .env.example .env.local
npm run dev
```

The first page asks for the Ironpaw address and connection token. It connects to
`http://127.0.0.1:7878` by default; use
`?ironpaw=https://your-ironpaw-host` to select another address.

## Google OAuth

Create a Google OAuth **Web application** and register:

```text
http://localhost:3000/oauth/callback
https://ironpaw.app/oauth/callback
```

The Google client secret and grant-encryption key are server-only environment
variables. Ironpaw stores an encrypted grant and calls this server when it needs
a short-lived access token; Google API requests still go directly from Ironpaw.

During development, compile and run Ironpaw with the explicit local-development
feature so it may contact the HTTP callback and broker on loopback:

```bash
cargo run --features local-development
```

Without that feature, Ironpaw requires HTTPS and defaults to `ironpaw.app`.
Production instances may override those defaults with:

```text
IRONPAW_OAUTH_CALLBACK_URL=https://ironpaw.app/oauth/callback
IRONPAW_OAUTH_BROKER_URL=https://ironpaw.app
```

The TUI and Web UI both use Ironpaw's `/client` HTTP and SSE API. Conversations
are stored by the browser in IndexedDB for the current site origin.
