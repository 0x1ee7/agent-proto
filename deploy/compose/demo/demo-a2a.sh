#!/bin/sh
# Operator CLI for the demo stack. The CLI connects as the same identity as the console ("tui"),
# so its seed is creds/tui.nk and not the default creds/cli.nk.
export A2A_CONFIG=/etc/a2ad/a2a-cli.toml
export A2A_NKEY_SEED_FILE=/state/creds/tui.nk
export A2AB_STATE_DIR=/state
exec a2a "$@"
