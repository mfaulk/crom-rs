# code-agent-auth

Provider login, credential storage, and OAuth for code-agent.

Handles authenticating to model providers, storing credentials securely,
resolving which credential to use at call time, and tracking available models.

## What's here

- **Login** — `AuthProvider` implementations for Anthropic, OpenAI, Codex, and
  Gemini (`AnthropicAuthProvider`, …), plus a `FakeAuthProvider` for tests.
- **OAuth** — browser (PKCE) and device-code flows. SHA-256 backs PKCE and the
  redacted account-id hash.
- **Storage** — `CredentialStore` over pluggable backends: OS keychain,
  `FileCredentialBackend`, and a fake for tests.
- **Resolution** — `resolve_credential` applies precedence across environment
  variables and stored credentials (`AuthResolverInputs`, `ResolvedAuth`).
- **Model registry** — `ModelRegistry` / `ModelInfo` describing available
  models and their availability.

## Design

Dependency-light: no HTTP client is bundled. OAuth flows and providers accept
an `oauth::HttpTransport` the host crate supplies (e.g. backed by `reqwest`),
keeping this crate easy to test and embed.

The file-backed credential store serializes its write paths
(`put` / `delete` / `set_active`) with a single, consistent lock order:
the cross-process advisory `flock` (via `fs4`, on a sibling
`<index>.lock` file) is acquired FIRST and held across the whole critical
section, and the in-process `Mutex` guarding the in-memory index snapshot
is taken only afterwards, briefly. Using the same `flock` → `Mutex` order
on every write path avoids the inverted-ordering deadlock that an
in-process `Mutex` → `flock` order on one path would cause. While holding
the `flock` the store re-reads the on-disk index and performs both the
backend secret mutation and the matching index update under that lock, so
concurrent `code-agent` processes (e.g. the CLI and the TUI) can neither
clobber each other's updates nor leave the index pointing at a secret a
concurrent writer just removed. The secret write keeps its atomic
temp-file + rename and `0o600` permissions.

## Usage

Used by the `code-agent` CLI and the OpenAI/Gemini provider crates.
