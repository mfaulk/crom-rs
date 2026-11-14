# crom-anthropic

Anthropic Messages API provider for crom.

Implements `crom_core::ModelProvider` against Anthropic's public
[Messages API](https://docs.anthropic.com/en/api/messages). It sends
conversations and tool definitions, then parses the streaming Server-Sent
Events response into normalized `ModelEvent`s.

## What's here

- `AnthropicProvider` — the `ModelProvider` implementation.
- `AnthropicConfig` — API key, base URL, connect/read timeouts, and retry
  count. `AnthropicConfig::from_env()` reads `ANTHROPIC_API_KEY`.

The `connect_timeout` bounds connection setup only; `read_timeout` bounds the
gap between successive bytes, so a stalled stream is detected without
truncating a healthy long-running response.

Failures before the stream starts map to typed `ProviderError` variants
(`Auth`, `InvalidRequest`, `RateLimited { retry_after }`, `Timeout`,
`Connect`, `ServerError`) and transient ones are retried (`max_retries`,
default 2) through the shared core policy with jittered backoff and
`Retry-After` support. See [`docs/providers.md`](../../docs/providers.md).

`tests/http_transport.rs` exercises the reqwest client against `wiremock`:
status classification, connect errors, read timeouts, and an observed retry.

## Usage

Construct a provider and hand it to the core agent runtime (directly or via
`crom-sdk`'s `anthropic` feature). Used by the `crom` CLI as the
default provider.
