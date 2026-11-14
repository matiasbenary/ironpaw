#!/usr/bin/env bash
# Starts the Ironpaw backend (local-development) and the web UI. Ctrl+C stops both.
set -euo pipefail
cd "$(dirname "$0")"

webui=local-clients/ironpaw-webui
[ -f "$webui/.env.local" ] || cp "$webui/.env.example" "$webui/.env.local"
[ -d "$webui/node_modules" ] || npm --prefix "$webui" install

trap 'kill 0' EXIT
grep -q '^WHISPER_URL=' .env 2>/dev/null && python3 whisper-server.py &
cargo run --features local-development &
npm --prefix "$webui" run dev &
wait
