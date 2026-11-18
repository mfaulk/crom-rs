# Model providers

This page covers provider profiles (which endpoint and credential an id
resolves to) and the shared error, retry, and timeout behaviour.

## Provider profiles

A provider profile maps a provider id (`--provider <id>`) to a wire
protocol, endpoint, API-key environment variable, default model, and
capability flags. The CLI (`crom run`/`chat`/...), the SDK
(`ProviderConfig::Profile`), and the eval harness (`crom eval run
--provider <id>`) all resolve profiles through the same function,
`crom_core::provider_profile::resolve_profile`, and build providers through
`crom_sdk::profile::build_profile_provider`, so one id means the same
endpoint and request shape everywhere.

### Built-in profiles

Defined in [`crates/crom-core/providers.toml`](../crates/crom-core/providers.toml)
and compiled into the binary.

| id          | kind          | base_url                                     | api_key_env            | default_model       | max_tokens_field        |
| ----------- | ------------- | -------------------------------------------- | ---------------------- | ------------------- | ----------------------- |
| `anthropic` | `anthropic`   | unset (`ANTHROPIC_BASE_URL`, else `https://api.anthropic.com`) | `ANTHROPIC_API_KEY` | `claude-sonnet-4-6` | n/a |
| `openai`    | `openai-chat` | `https://api.openai.com`                     | `OPENAI_API_KEY`       | `gpt-4.1`           | `max_completion_tokens` |
| `deepseek`  | `openai-chat` | `https://api.deepseek.com`                   | `DEEPSEEK_API_KEY`     | `deepseek-chat`     | `max_tokens`            |
| `gemini`    | `gemini`      | `https://generativelanguage.googleapis.com`  | `GEMINI_API_KEY`       | `gemini-2.5-pro`    | n/a                     |
| `local`     | `openai-chat` | `http://localhost:11434` (Ollama)            | `LOCAL_OPENAI_API_KEY` | none (pass `--model`) | `max_tokens`          |

`codex` and `fake` are not profiles: `codex` keeps its dedicated OpenAI
route, and `fake` is the offline echo provider. `vertex_ai`, `bedrock`, and
`foundry` are recognized but not implemented.

`base_url` excludes the API version: the `openai-chat` transport appends
`/v1/chat/completions` (and `/v1/models`).

### Settings

Profiles are patched or added under the `providers` key of
`settings.json` (user `${CROM_CONFIG_HOME:-~/.config/crom}/settings.json`,
then project `.agent/settings.json`; project wins field-by-field). A project
file's `providers` key is ignored unless the workspace is trusted
(`crom trust`, see [Workspace trust](cli.md#workspace-trust)): a
`base_url` plus `api_key_env` would otherwise send a host credential to a
repository-chosen endpoint.

```json
{
  "providers": {
    "deepseek": { "base_url": "https://my-proxy.example.com" },
    "vllm": {
      "kind": "openai-chat",
      "base_url": "http://gpu-box:8000",
      "api_key_env": "VLLM_API_KEY",
      "default_model": "Qwen/Qwen2.5-Coder-32B-Instruct",
      "max_tokens_field": "max_tokens",
      "capabilities": { "tool_calls": true }
    }
  },
  "model_config": { "provider": "deepseek" }
}
```

| key                                  | type / values                                      |
| ------------------------------------ | -------------------------------------------------- |
| `providers.<id>.kind`                | `anthropic` \| `openai-chat` \| `gemini`; required for new ids |
| `providers.<id>.base_url`            | endpoint root, no `/v1`                            |
| `providers.<id>.api_key_env`         | name of the env var holding the key (validated as `[A-Za-z_][A-Za-z0-9_]*`) |
| `providers.<id>.default_model`       | model used when no model is configured             |
| `providers.<id>.max_tokens_field`    | `max_tokens` (default) \| `max_completion_tokens`  |
| `providers.<id>.capabilities.*`      | `tool_calls`, `parallel_tool_calls`, `streaming`, `reasoning_content` (bool); `context_window`, `max_output_tokens` (integer) |
| `model_config.provider`              | default provider id                                |
| `model_config.model`                 | default model id                                   |

Capabilities left unset are unknown and resolve to `None`. Nothing assumes
support for an unknown capability.

#### Provider and model precedence (CLI)

Provider: `--provider` > `.agent/active-provider.json` (`crom provider use`)
> `auth.active_provider` > `model_config.provider` > `anthropic`.

Model: `--model` > `settings.model` > `model_config.model` > the profile's
`default_model`. A profile without a `default_model` (e.g. `local`) requires
`--model`. Note that `settings.model` applies to whichever provider is
active; set `--model` when switching providers if `settings.model` is set.

### Credentials

Profiles never contain keys. A settings profile with an `api_key` field is
rejected at parse time, and `api_key_env` must be a variable name.

The CLI resolves the key with the usual precedence ladder (see
[`docs/sdk/providers-auth.md`](sdk/providers-auth.md)): active stored
account, then the profile's `api_key_env`, then any stored account for the
provider id.

Eligibility: these are API endpoints billed per token. A ChatGPT, Claude,
or Gemini consumer subscription does not grant API access, and crom does not
reuse consumer-app sessions.

### Run metadata

The endpoint host (`host[:port]`, no scheme, path, query, or userinfo) is
recorded, never the key or full URL:

- SDK: `SdkEvent::RunStarted.endpoint_host`, `AgentRuntime::endpoint_host()`.
- Eval: `provider.json` in each task's artifact directory
  (`provider`, `model`, `endpoint_host`).
- CLI: an `info`-level `provider profile resolved` tracing event.

### DeepSeek notes

- Base URL `https://api.deepseek.com`; key from `DEEPSEEK_API_KEY`.
- `deepseek-chat` supports function calling over the OpenAI Chat Completions
  schema and works with crom's tool loop.
- `reasoning_content` (returned by `deepseek-reasoner`) is dropped when
  parsing responses and is never sent back in later turns. DeepSeek's
  `deepseek-reasoner` docs state that echoing `reasoning_content` in input
  messages returns HTTP 400, so dropping it is the safe default for
  multi-turn tool loops. The reasoning text is not shown to the user.
  Thinking-mode tool loops on `deepseek-reasoner` are not verified.
- `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` are accepted in
  responses but not yet mapped into `TokenUsage` cache fields (tracked with
  the usage work in #280).

### Local OpenAI-compatible servers

The `local` profile targets Ollama (`http://localhost:11434`). For vLLM, set
`providers.local.base_url` to `http://localhost:8000`. Servers that ignore
auth still need `LOCAL_OPENAI_API_KEY` set to any placeholder.

```sh
LOCAL_OPENAI_API_KEY=unused crom run --provider local --model llama3.1 "..."
```

### Verification

- `crates/crom-cli/tests/provider_profile_cli.rs`: `crom run --provider
  deepseek --model deepseek-chat` completes a tool-using turn against a
  wiremock DeepSeek endpoint and the key never reaches output or files.
- `crates/crom-sdk/tests/provider_profile.rs`: SDK profile construction.
- `provider_route_tests::profile_resolves_identically_in_cli_sdk_and_eval`
  in `crates/crom-cli/src/main.rs`.
- `crates/crom-cli/tests/eval_live_cli.rs`: `crom eval run --provider
  <profile> --model <m>` resolves a task against a wiremock endpoint for
  each of `anthropic`, `openai`, `deepseek`, and `gemini`, and skips with
  `credentials_unavailable` (exit 3) when the key is missing.

Manual live check (opt-in, costs tokens):

```sh
DEEPSEEK_API_KEY=... crom run --provider deepseek "List the files in src/"
CROM_EVAL_LIVE=1 DEEPSEEK_API_KEY=... crom eval run --suite rust-small \
  --provider deepseek --model deepseek-chat
```

## Cost semantics

crom reports **nominal** cost: published standard-tier list price times the
token usage the provider reported. It is not the billed amount. It ignores
discounts, credits, batch/flex/off-peak rates, regional or data-residency
surcharges, long-context tiers, cache storage fees, and 1-hour cache-write
rates. Use it to compare runs, not to reconcile invoices.

- **Source of prices.** `crates/crom-core/pricing/prices.toml`, compiled
  in and parsed at runtime (`crom_core::PricingTable`). There are no prices
  in Rust source and no network fetches. Every entry has `source_url` and
  `retrieved_at`. The table `version` (the retrieval date) is recorded as
  `repro.pricing_source_version` in eval results and in `run.json`.
- **Matching.** `provider` is the provider profile id and `model` (or an
  entry in `aliases`) must equal the request model id exactly. A model
  without an entry has unknown cost (`no_pricing_entry`).
- **Fields.** USD per 1M tokens for `input` (uncached), `output` (including
  reasoning), and optionally `cache_write` and `cache_read`, matching
  `TokenUsage` normalization (see [events](sdk/events.md#token-usage)).
- **Unknown is never zero.** Cost is `null` if usage was not reported for
  any round trip, if input or output is unknown, or if an entry prices a
  cache quantity the provider did not report. Eval results give the reason
  in `cost_unknown_reason`: `no_pricing_entry`, `usage_unknown`,
  `input_tokens_unknown`, `output_tokens_unknown`, or
  `cache_tokens_unknown`.
- **Not priced on purpose.** Models whose price depends on prompt size
  (e.g. `gemini-2.5-pro` above 200k tokens) are left out, since a flat
  rate would misprice them. DeepSeek prices are peak-hour rates (off-peak
  is cheaper), so DeepSeek nominal cost is an upper bound. The DeepSeek
  pricing page no longer lists `deepseek-chat`/`deepseek-reasoner`, so the
  `deepseek` profile's default model is unpriced.
- **Overrides.** `CROM_PRICING_FILE=<path>` (read by
  `PricingTable::load_default`, which `crom eval run` uses) and
  `crom eval run --pricing-file <path>` merge a file in the same format
  over the built-in table. An override entry replaces any built-in entry
  for the same provider that shares a model name or alias. The recorded
  version becomes `<override version>+<built-in version>`.

### Budgets

`crom eval run --max-cost-usd <usd>` caps each task across all of its
attempts. A task's `limits.max_cost_usd` also applies, and the tighter cap
wins. `--max-run-cost-usd <usd>` caps the whole run, across repetitions.
Both are checked between model turns, so a task can overshoot its cap by at
most one turn. Caps fail closed: with a cap set, an unpriced model or
unreported usage stops the agent (`budget.usage_unknown`) or keeps further
tasks from starting. Remaining tasks are recorded as `skipped` with a
`budget:` reason. SDK hosts set `RunBudgets::max_cost_usd`,
`max_input_tokens`, and `max_output_tokens` and pass prices with
`AgentRuntimeBuilder::pricing` (see [SDK safety](sdk/safety.md)).

### Updating the pricing table

1. Open each `source_url` in `crates/crom-core/pricing/prices.toml` and
   read the standard (non-batch) per-1M-token prices.
2. Edit the prices. Set `retrieved_at` to today's date on every entry you
   checked. Add new models with their own `source_url`. Leave out any
   model whose price you cannot verify or that is tiered by prompt size.
3. Set the top-level `version` to the retrieval date (`YYYY-MM-DD`).
4. Run `cargo test -p crom-core pricing`. The tests check that the table
   parses, is in USD, has https sources, ISO dates, non-negative prices,
   and no duplicate names.
5. Commit as `chore(pricing): refresh prices (<date>)`. Results priced
   with different table versions are not comparable on cost.

### Compatibility matrix

How each built-in profile carries multi-turn tool use, as exercised by the
`provider-conformance` suite (see [eval.md](eval.md#provider-conformance-284)).

| | `anthropic` | `openai` | `deepseek` | `gemini` |
|---|---|---|---|---|
| Wire / transport | Messages, SSE | Chat Completions, non-streaming | Chat Completions, non-streaming | `generateContent`, non-streaming |
| Max-tokens field sent | `max_tokens` | `max_completion_tokens` | `max_tokens` | `maxOutputTokens` |
| Tool-call ids | Provider `toolu_…` ids round-tripped | Provider `call_…` ids round-tripped | Provider ids round-tripped | None on the wire; Crom synthesizes ids and maps results back by function name and order |
| Error tool result | `is_error: true` | Content prefixed `ERROR:` (no flag on the wire) | Same as `openai` | `functionResponse.response.isError: true` |
| Unparseable arguments | Reported to the tool as `_invalid_tool_input`; tool returns an error result | Passed as a JSON string; tool returns an error result | Same as `openai` | Arguments are always an object; schema errors return an error result |
| Parallel calls in one turn | Declared | Declared | Handled by the transport; not declared in the profile | Handled (same-name calls get distinct ids); not declared in the profile |
| Output-truncation stop reason | `max_tokens` | `length` | `length` | `MAX_TOKENS` |
| Offline conformance (synthetic transcripts) | 6/6 | 6/6 | 6/6 | 6/6 |
| Live conformance | Not yet run | Not yet run | Not yet run | Not yet run |

On a truncation stop reason the agent loop discards the turn's tool calls
(never executes them), keeps any text, and asks the model to continue. After
`MAX_OUTPUT_CONTINUATIONS` (2) consecutive truncated turns the run ends with
`ResultSubtype::ErrorOutputTruncated`, surfaced by the SDK as a `Failed` run
with diagnostic `model.output_truncated`.

## Errors, retries, timeouts

Applies to `crom-anthropic`, `crom-provider-openai`, and `crom-provider-gemini`.

### Error classification

Every provider maps failures to a typed `crom_core::ProviderError`
(`crates/crom-core/src/errors.rs`) via `ProviderError::from_status` for HTTP
statuses and a per-crate `reqwest::Error` classifier for transport failures.

| Cause | Variant | Retryable |
| --- | --- | --- |
| 401, 403, missing credential | `Auth(String)` | no |
| 402, or 429 with `insufficient_quota` in the body (account out of credits) | `Billing(String)` | no |
| 400, 404, 413, 422, other 4xx (context too long, bad param, unknown model) | `InvalidRequest { status, message }` | no |
| 408, client connect/read/total timeout | `Timeout(String)` | yes |
| 429 (rate limit) | `RateLimited { retry_after, message }` | yes |
| 5xx (including Anthropic 529) | `ServerError { status, message }` | yes |
| TCP/TLS connection failure | `Connect(String)` | yes |
| Other transport errors, unexpected status classes | `Http(String)` | no |
| Malformed body | `InvalidResponse(String)` | no |
| Mid-stream SSE failure (Anthropic) | `Stream(String)` | no (not retried by the provider) |

`ProviderError::kind()` returns a stable snake_case label (`rate_limited`,
`server_error`, ...) for events and reports. Status-derived messages have the
form `<provider>: status <code>: <body>`.

### Retry policy

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

### Timeouts

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

### Observing retries

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

### Tests

- `crates/crom-core/src/provider_retry.rs`: status mapping, backoff curve,
  jitter bounds, `Retry-After` handling, observer reporting (paused time).
- `crates/crom-{anthropic,provider-openai,provider-gemini}/tests/http_transport.rs`:
  wiremock tests mapping 401/403, 400, 408, 429 (+`Retry-After`), 5xx,
  connect errors, and read timeouts to typed variants, plus retry behaviour.
  Retry tests use a 1ms base delay or `Retry-After: 0`, so they do not sleep.
- `crates/crom-sdk/tests/headless_run.rs::provider_retry_is_emitted_as_sdk_event`.
