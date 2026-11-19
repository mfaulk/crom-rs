#!/usr/bin/env bash
# Guard framework-independence check (ADR 0002, MILESTONE_18 §10.4).
# crom-guard and crom-guard-server may not depend, directly or transitively,
# on any crom-* crate other than crom-guard. Normal and build dependencies
# both count: build scripts run as code too. Dev-dependencies are exempt.
set -euo pipefail

status=0
for pkg in crom-guard crom-guard-server; do
  deps=$(cargo tree --locked -p "$pkg" -e normal,build --prefix none --format '{p}' \
    | awk '{print $1}' | sort -u)
  bad=$(grep -E '^crom-' <<<"$deps" | grep -vxE "crom-guard|$pkg" || true)
  if [[ -n "$bad" ]]; then
    echo "error: $pkg depends on forbidden crom-* crates:" >&2
    sed 's/^/  /' <<<"$bad" >&2
    status=1
  else
    echo "ok: $pkg depends on no crom-* crate other than crom-guard"
  fi
done
exit "$status"
