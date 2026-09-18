#!/bin/sh
# Linux-only: measure the daemon's peak RSS during a reverse grep over a large
# log file. Documents the < 15 MB RSS guarantee.
#
# Usage: measure-rss.sh <logfile> [binary]
#
# Generate a large fixture first:
#   scripts/gen-test-log.sh /tmp/big/catalina.out 2000000

set -eu

LOG="${1:?usage: measure-rss.sh <logfile> [binary]}"
BIN="${2:-/usr/local/bin/log-sidecar}"
PORT=9090
FILE="$(basename "$LOG")"

"$BIN" --root "$(dirname "$LOG")" --bind 127.0.0.1 --port "$PORT" &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT INT TERM

# Wait for the listener to come up.
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/api/files" >/dev/null 2>&1 && break
  sleep 0.1
done

tmp="${TMPDIR:-/tmp}/logsidecar-rss.$$"
: > "$tmp"

# Sample RSS every 100 ms while the grep runs.
(
  while kill -0 "$PID" 2>/dev/null; do
    rss=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
    [ -n "$rss" ] && echo "$rss" >> "$tmp"
    sleep 0.1
  done
) &

curl -sN "http://127.0.0.1:$PORT/api/grep?file=$FILE&query=ERROR&direction=reverse&limit=1000" >/dev/null

kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

peak=$(sort -n "$tmp" 2>/dev/null | tail -1)
rm -f "$tmp"
echo "peak RSS: ${peak:-0} KB"
