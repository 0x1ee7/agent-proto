#!/bin/sh
# Operator console for the demo stack. The console connects as the bus identity "tui".
# --root lists the directories the agents work in: the console refuses to load its operator key
# when the key path lies inside one of them (the default root is the current directory, which is
# "/" under `docker compose exec`).
exec a2a-tui --state-dir /state --seed-file /state/creds/tui.nk --url nats://127.0.0.1:4222 \
  --root /state/ws --root /home/a2a/repo "$@"
