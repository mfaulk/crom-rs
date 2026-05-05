# checkpoint-demo

Walkthrough for M3 (Workspace Checkpointing + Rewind Alpha).

## Setup

```bash
cd examples/checkpoint-demo
cargo test       # one test fails on purpose
```

## Run the agent with checkpointing + validation

```bash
ANTHROPIC_API_KEY=... \
code-agent run \
  "Refactor parse_tokens to trim whitespace and add a test. It is okay if you make a mistake." \
  --checkpointing on \
  --permission-mode accept-edits \
  --validate "cargo test"
```

The CLI will print:

```
[checkpoint] created ckpt_… before user request
[tool] Edit src/lib.rs
[checkpoint] captured src/lib.rs before change
…
[validation] cargo test -> exit=Some(0) timed_out=false (…ms)
[validation] passed
```

If you also pass `--rewind-on-validation-failure`, the workspace is
restored to the start-of-run checkpoint when validation fails. The
conversation history is **not** rewound.

## Inspect and rewind by hand

```bash
code-agent checkpoints list
code-agent checkpoints diff <CHECKPOINT_ID>
code-agent rewind <CHECKPOINT_ID> --dry-run    # preview
code-agent rewind <CHECKPOINT_ID> --yes         # do it
```
