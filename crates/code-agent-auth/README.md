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

The file-backed credential store serializes its index read-modify-write
(`put` / `delete` / `set_active`) both in-process (a `Mutex`) and across
processes (an exclusive `flock`, via `fs4`, on a sibling `<index>.lock`
file). While holding the cross-process lock it re-reads the on-disk index
so concurrent `code-agent` processes (e.g. the CLI and the TUI) cannot
clobber each other's updates. The secret write keeps its atomic temp-file
+ rename and `0o600` permissions.

## Usage

Used by the `code-agent` CLI and the OpenAI/Gemini provider crates.
