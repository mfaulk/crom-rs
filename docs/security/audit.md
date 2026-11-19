# Security audit events and secret redaction

M17.3 (#291). Every tool call the agent loop gates produces exactly one
`SecurityDecision` record, and every recorded event is secret-redacted before
it reaches a session file, the event stream, telemetry, or an eval artifact.

These records have no integrity protection and are best-effort. Crom Guard
keeps its own durable, hash-chained `guard-audit/v1` log of mediated
operations; see [docs/guard/audit.md](../guard/audit.md).

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
| `policy_source` | `PolicyDecisionSource` stage that decided (`hook`, `disallow_rule`, `permission_mode`, `allow_rule`, `approval_callback`, `approval_memory`, `external_effect_policy`, `default_deny`, `workspace_trust`); absent when the call never reached the policy engine or was refused below it |
| `rule` | Matching rule as `<list>[<index>]: <rule json>`, e.g. `disallowed_tools[0]: "Write"` |
| `decision` | `allow` / `deny` / `ask` |
| `enforcement` | `policy` (policy engine, tool allowlist, tool-catalog loading gate), `hook` (`PreToolUse` hook, or a `PostToolUse`/`PostToolUseFailure` hook `Deny`), `checkpoint` (checkpoint safety gate), `sandbox` (tool-level `PermissionDenied`: the Read/Write/Edit path sandbox, and a Bash call refused because the [OS sandbox](sandbox.md) is `required` but cannot be enforced) |
| `outcome` | `blocked`, `executed`, `failed` |
| `reason` | Redacted free text |

Emission points in `run_loop` (`crom-core/src/agent.rs`):

| Path | `enforcement` | `outcome` |
|---|---|---|
| Tool not loaded (tool search) / not in allowlist | `policy` | `blocked` |
| Policy engine deny (any stage) | `policy` or `hook` | `blocked` |
| Checkpoint `before_bash` / capture deny | `checkpoint` | `blocked` |
| Tool returns `ToolError::PermissionDenied` | `sandbox` | `blocked` |
| Tool ran, then a post-tool hook returned `Deny` | `hook` (`policy_source: hook`, `decision: deny`) | `blocked` |
| Tool ran | `policy` or `hook` | `executed` / `failed` (`is_error`) |

Policy engine stage order (`PolicyEngine::evaluate_tool`,
`crom-core/src/policy.rs`); the first stage that decides wins:

1. `PreToolUse` hooks — `Deny` returns here; `Allow` and `Ask` are recorded
   and applied below.
2. Disallow rules.
3. Approval memory, deny side (`DenyForSession`).
4. Plan mode (always denies).
5. MCP external-effect floor (`external_effect_policy`): for
   `PermissionKind::External` tools with no matching `allowed_tools` rule,
   `RequireExplicitAllowRule` denies and `DenyMutatingNameByDefault` denies
   write-like names. Nothing below — hook `Allow`, the `dontAsk` tool-name
   list (`AgentOptions::allowed_tools`), permission modes, approval memory,
   the approval callback — can override it (#343).
6. Hook `Allow`.
7. Permission mode (`dontAsk` rules and name list, `acceptEdits`,
   read-only auto-allow); skipped when a hook asked.
8. Allow rules; skipped when a hook asked.
9. Approval memory, allow side; skipped when a hook asked.
10. Approval callback.
11. Default deny.

Post-tool hook `Deny` is enforced, not advisory: the tool has already run,
so its effects stand, but the model receives an error result
(`Tool result blocked by post-tool hook: <reason>`) instead of the output.
`McpResultSizeLimitHook` uses this to withhold oversized MCP results.

CLI slash-command `` !`cmd` `` / `@file` expansions (#290) that the policy
engine denies record a blocked decision with `tool_use_id`
`slash-expansion-<run id>`. They are printed as JSON lines under
`--log-format json`; text mode shows only the expansion error.

Repo-local configuration ignored because the workspace is untrusted (#290,
see [threat model](threat-model.md) P9) records one blocked decision per
ignored key at CLI startup: `tool_name` `workspace_config`, `tool_use_id`
`workspace-trust-<run id>-<n>`, `resource` `<file>#<key>` (e.g.
`.agent/settings.json#allowed_tools`, `.agent/mcp.json#mcpServers.evil`),
`policy_source` `workspace_trust`, `decision` `deny`, `enforcement` `policy`,
`outcome` `blocked`. These are printed as JSON lines under
`--log-format json`; text mode prints a `[trust] ignored …` line on stderr.
They are emitted before the session starts, so `session_id` is a fresh
id (not the run's session) and they are not written to the session file.

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
