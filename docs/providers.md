# Model providers: errors, retries, timeouts

Applies to `crom-anthropic`, `crom-provider-openai`, and `crom-provider-gemini`.

## Error classification

Every provider maps failures to a typed `crom_core::ProviderError`
(`crates/crom-core/src/errors.rs`) via `ProviderError::from_status` for HTTP
statuses and a per-crate `reqwest::Error` classifier for transport failures.

| Cause | Variant | Retryable |
| --- | --- | --- |
| 401, 403, missing credential | `Auth(String)` | no |
| 400, 404, 413, 422, other 4xx (context too long, bad param, unknown model) | `InvalidRequest { status, message }` | no |
| 408, client connect/read/total timeout | `Timeout(String)` | yes |
| 429 | `RateLimited { retry_after, message }` | yes |
| 5xx (including Anthropic 529) | `ServerError { status, message }` | yes |
| TCP/TLS connection failure | `Connect(String)` | yes |
| Other transport errors, unexpected status classes | `Http(String)` | no |
| Malformed body | `InvalidResponse(String)` | no |
| Mid-stream SSE failure (Anthropic) | `Stream(String)` | no (not retried by the provider) |

`ProviderError::kind()` returns a stable snake_case label (`rate_limited`,
`server_error`, ...) for events and reports. Status-derived messages have the
form `<provider>: status <code>: <body>`.

## Retry policy

`crom_core::provider_retry` holds the shared policy; all three providers call
`retry_provider_call(provider, &policy, op)`.

- Retries only errors where `is_retryable()` is true. Total attempts =
  `1 + max_retries`.
- Backoff before retry *n* is `base_delay * 2^(n-1)`, capped at 30s, with
  equal jitter (uniform in `[d/2, d]`).
- A `Retry-After` hint (delta-seconds; OpenAI's `retry-after-ms` is preferred
  when present) replaces the backoff. A hint above `max_retry_after` (60s)
  surfaces `RateLimited` immediately instead of sleeping. HTTP-date
  `Retry-After` values are ignored.
- Each retry is logged at `warn` (`retrying provider call`) and reported to
  the observer installed with `with_retry_observer`.

| Provider | `max_retries` default | Base delay | Configurable via |
| --- | --- | --- | --- |
| Anthropic | 2 | 250ms | `AnthropicConfig.max_retries` |
| OpenAI | 3 | 250ms | `OpenAiConfig.{max_retries, retry_base_delay}` |
| Gemini | 3 | 250ms | `GeminiConfig.{max_retries, retry_base_delay}` |

Retries re-send the full POST. Anthropic retries only before the SSE stream
starts; a failure after the first byte surfaces as `Stream`.

## Timeouts

| Provider | Connect | Idle (read) | Total |
| --- | --- | --- | --- |
| Anthropic (streaming) | `connect_timeout` 30s | `read_timeout` 120s between SSE chunks | none (would truncate long streams) |
| OpenAI (non-streaming) | `connect_timeout` 30s | `read_timeout` 300s | `request_timeout` 600s |
| Gemini (non-streaming) | `connect_timeout` 30s | `read_timeout` 300s | `request_timeout` 600s |

OpenAI and Gemini are non-streaming, so the idle timeout also bounds the wait
for response headers while the model generates. The total timeout is a
backstop sized for slow reasoning models. All three timeouts surface as
`ProviderError::Timeout` and are retried.

Per-profile timeout settings are not yet exposed in `settings.json`; the CLI
and SDK use the defaults above.

## Observing retries

The SDK wraps the runtime provider for each run (`RetryObservingProvider` in
`crates/crom-sdk/src/runtime/retry.rs`) and emits
`SdkEvent::ProviderRetry` (`type: "provider_retry"`) before each retry, at
`Standard` and `Full` verbosity:

```json
{"type":"provider_retry","run_id":"…","provider":"openai","attempt":1,
 "max_retries":3,"delay_ms":250,"error_kind":"rate_limited",
 "message":"rate limited: openai: status 429: …"}
```

`message` is truncated to 512 bytes and passed through the SDK secret
redactor. Count `provider_retry` events per run to measure retry volume.
Plan-only generation does not emit retry events; plan-then-act emits act-phase
retries ahead of the replayed act events.

Outside the SDK, wrap any provider call in
`crom_core::with_retry_observer(observer, fut)` to receive
`crom_core::ProviderRetry` values. The observer is a tokio task-local, so it
must wrap the future that calls `stream_turn` inside any spawned task.

## Tests

- `crates/crom-core/src/provider_retry.rs`: status mapping, backoff curve,
  jitter bounds, `Retry-After` handling, observer reporting (paused time).
- `crates/crom-{anthropic,provider-openai,provider-gemini}/tests/http_transport.rs`:
  wiremock tests mapping 401/403, 400, 408, 429 (+`Retry-After`), 5xx,
  connect errors, and read timeouts to typed variants, plus retry behaviour.
  Retry tests use a 1ms base delay or `Retry-After: 0`, so they do not sleep.
- `crates/crom-sdk/tests/headless_run.rs::provider_retry_is_emitted_as_sdk_event`.
