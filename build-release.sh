#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RELEASE_DIR="${WARDEN_RELEASE_DIR:-$REPO_DIR/release}"
CHANNELS_DIR="$RELEASE_DIR/channels"

mkdir -p "$CHANNELS_DIR"

echo "building warden"
cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --release --locked
cp "$SCRIPT_DIR/target/release/warden" "$RELEASE_DIR/warden"
if command -v xattr >/dev/null 2>&1; then
  xattr -c "$RELEASE_DIR/warden" || true
fi
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$RELEASE_DIR/warden" >/dev/null 2>&1 || true
fi

echo "building warden-tui"
cargo build --manifest-path "$REPO_DIR/warden-tui/Cargo.toml" --release --locked
cp "$REPO_DIR/warden-tui/target/release/warden-tui" "$RELEASE_DIR/warden-tui"
if command -v xattr >/dev/null 2>&1; then
  xattr -c "$RELEASE_DIR/warden-tui" || true
fi
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$RELEASE_DIR/warden-tui" >/dev/null 2>&1 || true
fi

echo "building tui channel"
cargo build --manifest-path "$SCRIPT_DIR/channels-src/tui/Cargo.toml" --release --target wasm32-wasip2 --locked
cp "$SCRIPT_DIR/channels-src/tui/target/wasm32-wasip2/release/tui_channel.wasm" "$CHANNELS_DIR/tui.wasm"

echo "building telegram channel"
cargo build --manifest-path "$SCRIPT_DIR/channels-src/telegram/Cargo.toml" --release --target wasm32-wasip2 --locked
cp "$SCRIPT_DIR/channels-src/telegram/target/wasm32-wasip2/release/telegram_channel.wasm" "$CHANNELS_DIR/telegram.wasm"

echo "copying channel configs"
for config in "$SCRIPT_DIR"/channels-src/*/*.config.json "$SCRIPT_DIR"/channels-src/*/*.config.example.json; do
  if [ -f "$config" ]; then
    cp "$config" "$CHANNELS_DIR/"
  fi
done

echo "release assembled at $RELEASE_DIR"
