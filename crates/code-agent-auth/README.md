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

## Usage

Used by the `code-agent` CLI and the OpenAI/Gemini provider crates.
