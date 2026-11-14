#!/bin/sh
# Demo: a call in 10 minutes. Writes the board's events.json with a heads-up in 1 minute
# ("your call starts in 10 minutes") and the call itself 10 minutes from now, so the board
# rings twice. Run it with Ironpaw running; the board picks the list up within a minute of
# its next reply or reconnect, or in at most 10 minutes.
#   ./demo-call.sh [minutes until the call, default 10] [files dir, default ../../target/debug/files]
set -e
MIN=${1:-10}
FILES=${2:-"$(dirname "$0")/../../target/debug/files"}
# conversation key "local-client/esp32/esp32", hex-encoded (see message::scope_component)
DIR="$FILES/conversations/$(printf 'local-client/esp32/esp32' | od -An -tx1 | tr -d ' \n')"
mkdir -p "$DIR"
HEADS_UP=$(date -d "+1 min" +%Y-%m-%dT%H:%M)
CALL=$(date -d "+$MIN min" +%Y-%m-%dT%H:%M)
cat > "$DIR/events.json" <<EOF
[
  {"at": "$HEADS_UP", "text": "Heads up: your call starts in $MIN minutes. Grab water and your headphones."},
  {"at": "$CALL", "text": "Your call starts now. Good luck!", "sound": "ring.wav"}
]
EOF
echo "wrote $DIR/events.json:"
cat "$DIR/events.json"
