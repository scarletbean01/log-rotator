#!/bin/sh
# Generate a synthetic Tomcat-style log file.
# Usage: gen-test-log.sh <outfile> <lines>
set -eu

outfile="$1"
lines="$2"

: > "$outfile"

i=1
while [ "$i" -le "$lines" ]; do
  # Every 100th line is ERROR, every 250th is WARN, otherwise INFO.
  if [ $((i % 250)) -eq 0 ]; then
    level="WARN"
  elif [ $((i % 100)) -eq 0 ]; then
    level="ERROR"
  else
    level="INFO"
  fi
  printf '2026-09-17 10:00:%06d %-5s org.apache.catalina.core message %d\n' "$i" "$level" "$i" >> "$outfile"
  i=$((i + 1))
done
