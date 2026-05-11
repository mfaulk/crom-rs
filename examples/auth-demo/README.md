# auth-demo

Offline demonstration of the M10 provider login + credential store flow.

The demo exercises the **fake** provider end-to-end: an in-memory OAuth
authorization server, a fake API-key check, an in-memory model registry,
and the CLI commands that drive them. No network access is required.

## Quick start

From the repository root, after `cargo build`:

    cd examples/auth-demo
    bash run.sh

The script runs (in order):

1. `code-agent provider list` — registered providers and their support matrix.
2. `code-agent login` (no args) — provider selector, including the
   intentionally **unsupported** Claude / Gemini subscription paths.
3. `code-agent login fake --api-key=demo --allow-file-store` — store a
   fake API key.
4. `code-agent auth status --allow-file-store` — show the resulting
   active provider and account.
5. `code-agent model list` — unified model registry.
6. `code-agent model use fake/fake-coding-model` — pick the demo model.
7. `code-agent auth logout fake --allow-file-store` — delete the stored
   credential.
8. `code-agent auth status --allow-file-store` — show the empty status.

## Notes

- `--allow-file-store` opts into the on-disk credential backend rooted at
  `.agent/secrets/`. Without it the OS keychain is preferred — and on
  systems without one available, writes fail loudly so secrets never
  leak to disk implicitly.
- The fake provider lives entirely in-process. There is no network call;
  in particular, the fake OAuth/device flows are exercised by the
  `code-agent-auth` integration tests rather than this script (which
  focuses on the user-facing CLI).
- Subscription-auth modes (Claude, Gemini, Codex) are surfaced as
  visible-but-unsupported. The selector explains why and points at the
  supported alternative.

## Caveats

- This demo is intended as a documentation aid, not a benchmark or
  production starter. Do not point it at real provider credentials.
