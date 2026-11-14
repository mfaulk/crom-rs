# Event stream

`AgentRuntime::run` returns an `AgentRun` whose `.events` is a stream of
`Result<SdkEvent, AgentSdkError>`. Drive the stream to drain the run; the
final result is available via `AgentRun::final_result`.

## Verbosity

`AgentRuntimeBuilder::verbosity(EventVerbosity)`:

- `Minimal` — start, progress, patch, result.
- `Standard` — adds assistant messages, tool calls, approvals, diagnostics.
- `Full` — wraps every internal `AgentEvent` as `SdkEvent::Internal`.

## Event shapes

`SdkEvent` is a tagged enum and `Serialize`/`Deserialize`. The major
variants:

- `RunStarted` / `RunCompleted` / `RunFailed` / `RunCancelled`
- `AssistantMessage`, `AssistantTextDelta`
- `ToolCallStarted`, `ToolCallCompleted`
- `ApprovalRequired`, `ApprovalResolved`
- `PatchProposed`, `FileGenerated`
- `ValidationStarted`, `ValidationCompleted`
- `Diagnostic`
- `ModelTurnCompleted` — one model round trip finished (see below).
- `PlanningStarted`, `PlanCreated`, `PlanApproved`, `ActStarted`
- `Internal` — opaque wrapper carrying serialized internal `AgentEvent`.

## Token usage

`ModelTurnCompleted` is emitted under `Standard` and `Full` once per model
round trip, including plan-then-act planning turns:

```json
{
  "type": "model_turn_completed",
  "run_id": "…",
  "turn": 2,
  "provider": "openai",
  "model": "gpt-4.1",
  "usage": {
    "input_tokens": 176,
    "output_tokens": 300,
    "cache_creation_input_tokens": null,
    "cache_read_input_tokens": 1024,
    "reasoning_tokens": 256
  },
  "latency_ms": 840
}
```

`turn` is 1-based and restarts per agent loop (planning and act phases are
numbered separately). `usage` is `null` when the provider reported no usage
for the turn, or the stream errored or was interrupted before completing.

`TokenUsage` fields are each `Option<u64>`; `null` means unreported, never
zero. Normalized across providers:

- `input_tokens` — prompt tokens **excluding** cache reads/writes.
- `cache_read_input_tokens` — Anthropic `cache_read_input_tokens`, OpenAI
  `prompt_tokens_details.cached_tokens`, DeepSeek `prompt_cache_hit_tokens`,
  Gemini `cachedContentTokenCount`.
- `cache_creation_input_tokens` — Anthropic only.
- `output_tokens` — all generated tokens **including** reasoning.
- `reasoning_tokens` — OpenAI/DeepSeek `completion_tokens_details.reasoning_tokens`,
  Gemini `thoughtsTokenCount`; `null` for Anthropic (not broken out).

Gemini omits zero counts (proto3 JSON), so missing counts inside a present
`usageMetadata` are read as `0`; a missing `usageMetadata` is `null` usage.

**Aggregation.** Run-level usage (`AgentRunResult.usage`,
`RunCompleted.usage`, core `AgentResult.usage`) is the strict sum over round
trips: `null` if the run made no round trips or **any** round trip's usage is
unknown; otherwise each field is summed and is `null` if any round trip left
it unreported. Plan-then-act run usage covers both phases. Cost estimates
(`total_cost_usd`) are `null` whenever a priced quantity is unknown.

## Metadata propagation

Every event carries the run id and (where applicable) the session id.
`RunStarted.metadata` carries the request metadata bundle (tenant, user,
request id, trace id, labels). Hosts use these to associate events with
upstream observability context.

## Sinks

Use `event_sink` to mirror events to your application. The runtime also
forwards events to the configured `AgentStorage` when a session policy
permits.
