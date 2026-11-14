#!/bin/sh
# Mutation oracle for the agent-written regression test: it must pass on the
# workspace and fail once `slugify` is reverted to the original buggy body.
set -u
name=collapses_repeated_separators

out=$(cargo test --all-targets --no-fail-fast "$name" 2>&1)
if ! printf '%s\n' "$out" | grep -Eq "^test .*$name \.\.\. ok$"; then
    printf '%s\n' "$out"
    echo "FAIL: test '$name' is missing or does not pass on the workspace" >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp -R Cargo.toml src "$tmp/"
if [ -d tests ]; then cp -R tests "$tmp/"; fi
if [ -f Cargo.lock ]; then cp Cargo.lock "$tmp/"; fi

if ! awk '
/^pub fn slugify\(/ {
    found = 1; skip = 1
    print "pub fn slugify(input: &str) -> String {"
    print "    let mut out = String::new();"
    print "    for c in input.chars() {"
    print "        if c.is_ascii_alphanumeric() {"
    print "            out.push(c.to_ascii_lowercase());"
    print "        } else {"
    print "            out.push(\x27-\x27);"
    print "        }"
    print "    }"
    print "    out.trim_matches(\x27-\x27).to_string()"
    print "}"
    next
}
skip && /^}/ { skip = 0; next }
skip { next }
{ print }
END { if (!found) exit 3 }
' src/lib.rs >"$tmp/src/lib.rs"; then
    echo "FAIL: could not locate 'pub fn slugify(' in src/lib.rs to mutate" >&2
    exit 1
fi

mout=$(cd "$tmp" && CARGO_TARGET_DIR="$tmp/target" cargo test --all-targets --no-fail-fast "$name" 2>&1)
if printf '%s\n' "$mout" | grep -Eq "^test .*$name \.\.\. FAILED$"; then
    echo "ok: '$name' detects the reverted bug"
    exit 0
fi
printf '%s\n' "$mout"
echo "FAIL: '$name' still passes against the original buggy slugify" >&2
exit 1
