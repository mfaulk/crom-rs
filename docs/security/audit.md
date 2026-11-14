# Security audit events and secret redaction

M17.3 (#291). Every tool call the agent loop gates produces exactly one
`SecurityDecision` record, and every recorded event is secret-redacted before
it reaches a session file, the event stream, telemetry, or an eval artifact.

## `SecurityDecision`

`AgentEvent::SecurityDecision` (`crom-core/src/events.rs`), surfaced to SDK
consumers as `SdkEvent::SecurityDecision` under **every** verbosity.

| Field | Meaning |
|---|---|
| `session_id`, `run_id`, `turn_index` | Where the call happened. `(run_id, tool_use_id)` is the correlation key. |
| `principal` | `{"kind":"session"}` or `{"kind":"subagent","agent":"<name>"}` |
| `tool_use_id`, `tool_name` | The model's tool call |
| `action_class` | `PermissionKind` (`ReadOnly`, `Mutating`, `Execution`, `External`, `Orchestration`); absent for unregistered tools |
| `resource` | Normalized target: workspace-relative (or normalized absolute) path, Bash command head (leading `NAME=value` assignments skipped), URL `host[:port]` without userinfo, `mcp:<server>`, `agent:<name>` |
| `policy_source` | `PolicyDecisionSource` stage that decided (`hook`, `disallow_rule`, `permission_mode`, `allow_rule`, `approval_callback`, `approval_memory`, `external_effect_policy`, `default_deny`); absent when the call never reached the policy engine or was refused below it |
| `rule` | Matching rule as `<list>[<index>]: <rule json>`, e.g. `disallowed_tools[0]: "Write"` |
| `decision` | `allow` / `deny` / `ask` |
| `enforcement` | `policy` (policy engine, tool allowlist, tool-catalog loading gate), `hook` (`PreToolUse` hook), `checkpoint` (checkpoint safety gate), `sandbox` (tool-level `PermissionDenied`, e.g. the Read/Write/Edit path sandbox; the #253 OS sandbox will report here) |
| `outcome` | `blocked`, `executed`, `failed` |
| `reason` | Redacted free text |

Emission points in `run_loop` (`crom-core/src/agent.rs`):

| Path | `enforcement` | `outcome` |
|---|---|---|
| Tool not loaded (tool search) / not in allowlist | `policy` | `blocked` |
| Policy engine deny (any stage) | `policy` or `hook` | `blocked` |
| Checkpoint `before_bash` / capture deny | `checkpoint` | `blocked` |
| Tool returns `ToolError::PermissionDenied` | `sandbox` | `blocked` |
| Tool ran | `policy` or `hook` | `executed` / `failed` (`is_error`) |

CLI slash-command `` !`cmd` `` / `@file` expansions (#290) that the policy
engine denies record a blocked decision with `tool_use_id`
`slash-expansion-<run id>`. They are printed as JSON lines under
`--log-format json`; text mode shows only the expansion error.

Text-mode CLI output does not print `SecurityDecision` (the `[policy]` and
`[denied]` lines cover it); the TUI shows a card for blocked decisions only.

## Audit coverage

Eval writes, per task:

- `security.jsonl` — every `SecurityDecision`.
- `security-summary.json` — `decisions`, `attempted`, `blocked`, `executed`,
  `failed`, `blocked_by_layer`, `tool_executions`,
  `executions_with_decision`, and `audit_coverage` =
  `executions_with_decision / tool_executions` (`null` when no tool ran).
  See `crom-eval/src/security.rs`.

## Secret redaction

Patterns (`crom-core/src/builtin_hooks.rs`, `redact_secrets_in_str`):
Anthropic/OpenAI-style `sk-…` keys, Google `AIza…`, AWS `AKIA…`/`ASIA…`,
GitHub `gh[pousr]_…` and `github_pat_…`, Slack `xox?-…`, `Bearer …`, PEM
private-key blocks, and the value of `NAME=value` / `NAME: value` where
`NAME` contains `API_KEY`, `SECRET`, `TOKEN`, or `PASSWORD` (8+ character
values; the name is kept).

Where it applies:

| Sink | Mechanism |
|---|---|
| Event stream and session JSONL | `emit` in `agent.rs` round-trips each `AgentEvent` through `redact_secrets_in_serializable` (string leaves only; numeric fields untouched). Always on. |
| `AssistantTextDelta` | Redacted per chunk. A secret split across chunks is only caught in the persisted `AssistantMessage`. |
| Telemetry sink | Same round trip in `emit_telemetry`. |
| `/compact` boundary record | Redacted before `SessionSink::record`. |
| Eval artifacts | Every file under `tasks/<id>/` except `workspace/`, plus the run-level `summary.*`, `run.json`, `repetitions.json`, `behavior-summary.json`, is written through `crom_eval::artifacts::write_redacted`. Patches that contained a secret are therefore not byte-exact. |
| SDK | `redact_event` additionally applies the configured `SecretRedactor` to `SecurityDecision.reason` and `.resource`. |

The model and tools always see real values: redaction applies to recorded
copies only. One consequence: `AgentClient` rebuilds multi-turn history from
the (redacted) event stream, so a secret from an earlier turn is
`[REDACTED]` in later turns of the same chat.

`hooks.secret_redaction` (default `true`) no longer gates event redaction,
which cannot be turned off. It still controls redaction of slash-expansion
output. `SecretRedactionHook` / `McpSecretRedactionHook` remain as
`Continue` hooks for configuration compatibility: hook outputs cannot rewrite
tool results.

## Not covered

- Secrets in formats the patterns do not recognize (e.g. arbitrary values of
  environment variables with non-secret-looking names).
- The run-level `index.jsonl` (appended by `crom_eval::index`), which holds
  ids, scores and metrics only.
- SIEM export and OpenTelemetry security semantic conventions (out of scope).
