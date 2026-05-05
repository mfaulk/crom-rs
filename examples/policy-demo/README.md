# policy-demo

A tiny Rust crate used as the M2 demo. It includes:

- One failing test (`tests/parse_test.rs::handles_quoted_values`) that the
  agent should fix.
- A `.env` file the agent **must not modify**.
- An `AGENTS.md` describing rules.
- A `.agent/settings.json` that:
  - Disallows `Edit`/`Write`/`Read` of any `.env*` file.
  - Disallows dangerous Bash patterns (`rm -rf`, `sudo`, `curl|sh`).
  - Whitelists safe `cargo` commands.
  - Enables the `block_env_file_writes`, `block_dangerous_bash`,
    `audit_log`, and `secret_redaction` hooks.
  - Sets `max_patch_size_bytes` so accidental large overwrites are blocked.

## Run

From this directory:

```sh
ANTHROPIC_API_KEY=… code-agent chat \
  --cwd . \
  --permission-mode default \
  --show-usage
```

A typical session:

1. User: "Run tests and fix the failing bug. Do not modify .env."
2. Agent asks approval to run `cargo test`. User accepts.
3. Agent reads `tests/parse_test.rs` and `src/lib.rs`.
4. If the agent attempts `Edit .env`, both the policy disallow rule and
   the `BlockEnvFileWritesHook` deny it.
5. Agent fixes `parse_kv` to strip surrounding quotes.
6. Agent reruns `cargo test`. Tests pass.

## Run the tests directly

```sh
cargo test
```

The `handles_quoted_values` test currently fails until `parse_kv` is
patched.
