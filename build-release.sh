#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RELEASE_DIR="${IRONPAW_RELEASE_DIR:-$REPO_DIR/release}"
CHANNELS_DIR="$RELEASE_DIR/channels"
CHANNEL_STORE_DIR="$RELEASE_DIR/channel-store"
TOOL_STORE_DIR="$RELEASE_DIR/tool-store"

mkdir -p "$CHANNELS_DIR"
mkdir -p "$TOOL_STORE_DIR"

echo "building ironpaw"
cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --release --locked
cp "$SCRIPT_DIR/target/release/ironpaw" "$RELEASE_DIR/ironpaw"
if command -v xattr >/dev/null 2>&1; then
  xattr -c "$RELEASE_DIR/ironpaw" || true
fi
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$RELEASE_DIR/ironpaw" >/dev/null 2>&1 || true
fi

echo "building ironpaw-tui"
cargo build --manifest-path "$SCRIPT_DIR/local-clients-src/ironpaw-tui/Cargo.toml" --release --locked
cp "$SCRIPT_DIR/local-clients-src/ironpaw-tui/target/release/ironpaw-tui" "$RELEASE_DIR/ironpaw-tui"
if command -v xattr >/dev/null 2>&1; then
  xattr -c "$RELEASE_DIR/ironpaw-tui" || true
fi
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$RELEASE_DIR/ironpaw-tui" >/dev/null 2>&1 || true
fi

build_wasm_packages() {
  local source_root="$1"
  local installed_dir="$2"
  local store_dir="$3"

  for source_dir in "$source_root"/*; do
    [ -f "$source_dir/Cargo.toml" ] || continue
    local package_name
    package_name="$(basename "$source_dir")"
    local crate_name
    crate_name="$(awk -F'"' '/^name[[:space:]]*=/ { print $2; exit }' "$source_dir/Cargo.toml")"
    [ -n "$crate_name" ] || { echo "missing package name in $source_dir/Cargo.toml" >&2; exit 1; }
    local artifact_name="${crate_name//-/_}.wasm"

    echo "building $package_name package"
    cargo build --manifest-path "$source_dir/Cargo.toml" --release --target wasm32-wasip2 --locked

    if compgen -G "$source_dir"/*.install.json >/dev/null; then
      local destination="$store_dir/$package_name"
      mkdir -p "$destination"
      cp "$source_dir/target/wasm32-wasip2/release/$artifact_name" "$destination/$package_name.wasm"
      for package_file in "$source_dir"/*.config.example.json "$source_dir"/*.install.json; do
        [ -f "$package_file" ] && cp "$package_file" "$destination/"
      done
    else
      cp "$source_dir/target/wasm32-wasip2/release/$artifact_name" "$installed_dir/$package_name.wasm"
      for config in "$source_dir"/*.config.json; do
        [ -f "$config" ] && cp "$config" "$installed_dir/"
      done
    fi
  done
}

build_wasm_packages "$SCRIPT_DIR/channels-src" "$CHANNELS_DIR" "$CHANNEL_STORE_DIR"
build_wasm_packages "$SCRIPT_DIR/tools-src" "$RELEASE_DIR/tools" "$TOOL_STORE_DIR"

for source_dir in "$SCRIPT_DIR/tools-src"/*; do
  [ -f "$source_dir/shell.json" ] || continue
  package_name="$(basename "$source_dir")"
  destination="$TOOL_STORE_DIR/$package_name"
  mkdir -p "$destination"
  cp "$source_dir/shell.json" "$destination/shell.json"
  [ ! -f "$source_dir/SKILL.md" ] || cp "$source_dir/SKILL.md" "$destination/SKILL.md"
done

mkdir -p "$RELEASE_DIR/tools"
cp "$TOOL_STORE_DIR/web-search/web-search.wasm" "$RELEASE_DIR/tools/web_search.wasm"
if [ ! -f "$RELEASE_DIR/tools/web_search.config.json" ]; then
  cp "$TOOL_STORE_DIR/web-search/web-search.config.example.json" \
    "$RELEASE_DIR/tools/web_search.config.json"
fi

echo "release assembled at $RELEASE_DIR"
