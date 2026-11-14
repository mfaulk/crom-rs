# crom-provider-openai

OpenAI model provider for crom.

Implements `crom_core::ModelProvider` against OpenAI's
[Chat Completions](https://platform.openai.com/docs/api-reference/chat) API. A
thin adapter: it maps a `ModelRequest` to a `chat.completions` body, makes the
call, and normalizes the response into `ModelEvent`s.

## What's here

- The `ModelProvider` implementation.
- `OpenAiTransport` — a pluggable HTTP transport (`post_chat`, `list_models`)
  so tests can mock responses without real network calls.

Credentials are passed in at call time as a `ResolvedCredential` (from
`crom-auth`), keeping credential storage out of the request path.

## `http` feature (default)

The default-on `http` feature adds `ReqwestOpenAiTransport`, a `reqwest`-backed transport,
and a one-call constructor:

```rust,ignore
let provider = OpenAiProvider::from_config(OpenAiConfig::default(), resolved_credential)?;
```

`OpenAiConfig` defaults: `connect_timeout` 30s, `read_timeout` (idle) 300s,
`request_timeout` (total) 600s, `max_retries` 3, `retry_base_delay` 250ms.
Failures map to typed `ProviderError` variants (`Auth`, `InvalidRequest`,
`RateLimited { retry_after }`, `Timeout`, `Connect`, `ServerError`). 408, 429,
5xx, connect errors, and timeouts are retried through the shared core policy
with jittered exponential backoff, honouring `retry-after-ms` / `Retry-After`.
See [`docs/providers.md`](../../docs/providers.md).

Build with `default-features = false` to drop the reqwest transport and
supply your own `OpenAiTransport` via `OpenAiProvider::new`.

## Usage

Enabled in `crom-sdk` via the `openai` feature (on by default) as
`ProviderConfig::openai_api_key`, and selectable from the `crom` CLI; both
go through `from_config`.

HTTP-level tests (`tests/http_transport.rs`) run the reqwest transport
against a `wiremock` server: success, status classification (401/403, 400,
408, 429 + `Retry-After`, 5xx), connect errors, read/total timeouts, and
retries (reported through `with_retry_observer`).
