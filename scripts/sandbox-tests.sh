#!/usr/bin/env bash
# Run the crom-tools OS sandbox suites, including the #[ignore]d P6 containment
# tests (fork loop, fork bomb, memory), inside a cgroup scope whose limits do
# not depend on the code under test. If the sandbox is broken, the outer scope
# turns a fork bomb into a failed test instead of a downed host (#253).
#
# Usage: scripts/sandbox-tests.sh [extra libtest args...]
# Env:   SANDBOX_TESTS_TASKS_MAX (default 1024), SANDBOX_TESTS_MEMORY_MAX (default 8G)
set -euo pipefail

tasks_max="${SANDBOX_TESTS_TASKS_MAX:-1024}"
memory_max="${SANDBOX_TESTS_MEMORY_MAX:-8G}"
tests=(--test sandbox --test sandbox_env)

if [[ "$(uname -s)" != Linux ]]; then
  exec cargo test --locked -p crom-tools "${tests[@]}" -- "$@"
fi

if ! command -v systemd-run >/dev/null || ! systemd-run --user --scope --quiet true 2>/dev/null; then
  echo "error: no systemd user session for an outer cgroup scope;" >&2
  echo "       refusing to run the containment tests without one." >&2
  exit 2
fi

# Compile outside the scope: a parallel build legitimately needs more tasks.
cargo test --locked -p crom-tools "${tests[@]}" --no-run

# One test at a time: RLIMIT_NPROC is per uid and the sandbox sets it to the
# uid's current task count plus max_processes, so processes from a concurrent
# test exiting mid-call would loosen the bound the P6 tests assert.
exec systemd-run --user --scope --quiet \
  -p "TasksMax=${tasks_max}" -p "MemoryMax=${memory_max}" -p MemorySwapMax=0 \
  cargo test --locked -p crom-tools "${tests[@]}" -- --include-ignored --test-threads=1 "$@"
