#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/tui_channel.wasm tui.wasm
