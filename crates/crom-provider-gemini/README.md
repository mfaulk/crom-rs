# crom-provider-gemini

Google Gemini model provider for crom.

Implements `crom_core::ModelProvider` against Google AI Studio's
[`generateContent`](https://ai.google.dev/api/generate-content) API. A thin
adapter: it maps a `ModelRequest` to a `models/{model}:generateContent`
payload, makes the call, and normalizes the response into `ModelEvent`s.

## What's here

- The `ModelProvider` implementation.
- `GeminiTransport` — a pluggable HTTP transport (`post_generate`,
  `list_models`) so tests can mock responses.
- `GEMINI_API_KEY_HEADER` — the `x-goog-api-key` header. The provider sends
  the key as a header rather than a `?key=` URL parameter so it does not leak
  into logs, traces, or backtraces.

Credentials are passed in at call time as a `ResolvedCredential` (from
`crom-auth`).

## Usage

Enabled in `crom-sdk` via the `gemini` feature and selectable from the
`crom` CLI.
