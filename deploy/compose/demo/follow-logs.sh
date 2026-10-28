#!/bin/sh
# Streams the whole system to stdout, so `docker compose logs -f` shows what happens:
#   [bus] [supervisor] [omp] [claude] [omp-model] [claude-model]  component logs under <state>/logs
#   [events]                                                      agent presence, task states, artifacts, approvals and audit records read from the bus
# The component logs come from `tail -F` (new files are picked up within a second, rotated files
# are followed by name). The event tail is `a2a-tui --follow`; it needs the network
# namespace of the supervisor container (bus on 127.0.0.1) and retries until the bus is up.
#
# Usage: follow-logs [state-dir]
state="${1:-/state}"
trap 'kill 0' TERM INT

(
  while :; do
    if [ -f "$state/creds/tui.nk" ]; then
      a2a-tui --follow --seed-file "$state/creds/tui.nk" --url nats://127.0.0.1:4222 2>&1 \
        | sed -u 's/^/[events] /'
    fi
    sleep 2
  done
) &

seen=" "
while :; do
  for file in "$state"/logs/*.log "$state"/logs/*.jsonl; do
    [ -f "$file" ] || continue
    case "$seen" in *" $file "*) continue ;; esac
    seen="$seen$file "
    name=$(basename "$file")
    name="${name%.*}"
    ( tail -n +1 -F "$file" 2>/dev/null | sed -u "s/^/[$name] /" ) &
  done
  sleep 1
done
