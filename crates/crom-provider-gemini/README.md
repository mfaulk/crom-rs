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

## `http` feature (default)

The default-on `http` feature adds `ReqwestGeminiTransport`, a `reqwest`-backed transport,
and a one-call constructor:

```rust,ignore
let provider = GeminiProvider::from_config(GeminiConfig::default(), resolved_credential)?;
```

`GeminiConfig` defaults: `connect_timeout` 30s, `read_timeout` (idle) 300s,
`request_timeout` (total) 600s, `max_retries` 3, `retry_base_delay` 250ms.
Failures map to typed `ProviderError` variants (`Auth`, `InvalidRequest`,
`RateLimited { retry_after }`, `Timeout`, `Connect`, `ServerError`). 408, 429,
5xx, connect errors, and timeouts are retried through the shared core policy
with jittered exponential backoff, honouring `Retry-After`. See
[`docs/providers.md`](../../docs/providers.md).

Build with `default-features = false` to drop the reqwest transport and
supply your own `GeminiTransport` via `GeminiProvider::new`.

## Usage

Enabled in `crom-sdk` via the `gemini` feature (on by default) as
`ProviderConfig::gemini_api_key`, and selectable from the `crom` CLI; both
go through `from_config`.

HTTP-level tests (`tests/http_transport.rs`) run the reqwest transport
against a `wiremock` server: success, status classification (401/403, 400,
408, 429 + `Retry-After`, 5xx), connect errors, read/total timeouts, and
retries (reported through `with_retry_observer`).
