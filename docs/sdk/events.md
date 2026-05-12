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
- `PlanningStarted`, `PlanCreated`, `PlanApproved`, `ActStarted`
- `Internal` — opaque wrapper carrying serialized internal `AgentEvent`.

## Metadata propagation

Every event carries the run id and (where applicable) the session id.
`RunStarted.metadata` carries the request metadata bundle (tenant, user,
request id, trace id, labels). Hosts use these to associate events with
upstream observability context.

## Sinks

Use `event_sink` to mirror events to your application. The runtime also
forwards events to the configured `AgentStorage` when a session policy
permits.
