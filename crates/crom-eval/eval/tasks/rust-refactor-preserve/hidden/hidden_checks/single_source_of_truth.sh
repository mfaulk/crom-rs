#!/bin/sh
# Structural goal of the refactor: at most one PI literal and at most one
# negative-radius comparison remain in src/lib.rs (behavior is covered by the
# hidden tests).
pi=$(grep -c '3\.141' src/lib.rs)
neg=$(grep -c '< 0\.0' src/lib.rs)
echo "PI literals: $pi, negative-radius checks: $neg"
[ "$pi" -le 1 ] && [ "$neg" -le 1 ]
