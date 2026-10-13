#!/usr/bin/env bash
# Plugin script invoked by an opt-in PostToolUse hook. The plugin system
# never executes this directly; only the hook system does, after the
# normal Bash permission flow.
echo "[rust-quality] format-check on ${CROM_PLUGIN_ROOT:-?}"
