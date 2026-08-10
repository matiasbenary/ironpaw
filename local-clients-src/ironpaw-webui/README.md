# Ironpaw Web UI

A standalone Vite + React application. This directory can be copied and deployed to
any static web host.

```bash
npm install
npm run dev
```

Vite applies frontend edits immediately. The first page asks for the Ironpaw
address and bridge token. The client connects to `http://127.0.0.1:7878` by
default; use `?ironpaw=https://your-ironpaw-host` to select another address.

Create a deployable `dist/` directory with:

```bash
npm run build
```

The TUI and web UI both use Ironpaw's `/client` HTTP and SSE API. Conversations
are stored by the browser in IndexedDB for the current site origin.
