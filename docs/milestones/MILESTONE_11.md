# Milestone 11 — Full Planning Mode + Plan/Act Workflow

**Theme:** Promote the M2 `PermissionMode::Plan` guardrail into a full
productized planning workflow. Users can ask the agent to draft a
structured plan, review and revise it, approve it, execute it, watch
step-by-step progress, detect deviations, and re-plan when execution
diverges — all without bypassing existing permissions, hooks, or
checkpoint rules.

## What's new

Core planning subsystem lives at
`crates/code-agent-core/src/plan/`:

| Module | Purpose |
|--------|---------|
| `artifact.rs` | Plan id generation, `PlanDocument`, `PlanStep`, status/kind/access/risk enums, `WorkspaceRelativePath` validator, `PlanApproval`, `PlanExecutionSummary`. |
| `options.rs` | `PlanningOptions`, `PlanningMode`, `PlanDeviationPolicy` with lenient parsers. |
| `store.rs` | `PlanStore` trait + `FsPlanStore` (atomic writes via `tempfile`, per-version snapshots, append-only `index.jsonl`, resilient listing). |
| `tools.rs` | `SubmitPlan` (planning-mode) and `PlanUpdate` (act-mode) internal `Tool` implementations + JSON schemas. |
| `deviation.rs` | `classify_tool_call` plan-scope classifier, `PlanDeviation`, severity defaults, secret redaction for plan metadata. |
| `scope.rs` | `PlanExecutionContext` summary used by the act driver. |
| `driver.rs` | `generate_plan` (planning loop) and `act_plan` (execute approved plan) helpers; `PlanScopeCallback` wraps the user's permission callback to detect and route deviations through a `PlanDeviationCallback`. |

`code-agent-core` re-exports the public surface and adds 16 new
`AgentEvent` variants:

```
PlanCreated, PlanRevisionCreated, PlanApprovalRequested,
PlanApproved, PlanRejected, PlanActStarted,
PlanStepStarted, PlanStepUpdated, PlanStepCompleted,
PlanDeviationDetected, PlanDeviationResolved,
PlanCompleted, PlanFailed, PlanNeedsReplan, PlanArchived
```

The CLI renders each one in plain and JSON modes.

## Plan store layout

```
.agent/plans/
  <plan-id>.json         # latest version, atomically replaced
  <plan-id>.v<n>.json    # historical snapshot for diff/history
  index.jsonl            # append-only audit log; corruption does not block listing
```

Listing rebuilds the directory from disk and skips files it can't
parse, so a single corrupted plan never hides healthy ones. Versioned
snapshots back `code-agent plans diff --from-version N --to-version M`.

## Planning options (`.agent/settings.json`)

```json
{
  "planning": {
    "enabled": true,
    "mode": "read_only_workspace",
    "require_approval_before_act": true,
    "enforce_plan_scope": true,
    "allow_read_only_tools_during_planning": true,
    "allow_external_tools_during_planning": false,
    "max_plan_steps": 20,
    "max_plan_revisions": 10,
    "max_plan_bytes": 131072,
    "default_act_permission_mode": "default",
    "deviation_policy": "ask",
    "persist_plans": true,
    "exploratory_reads_during_act": true,
    "strict_commands": true
  }
}
```

`PlanningSettingsConfig::apply` merges user/project settings into a
`PlanningOptions`. Invalid `mode` / `default_act_permission_mode` /
`deviation_policy` values produce a clear `SettingsError`
(`BadPlanningMode`, `BadDeviationPolicy`, …).

`PermissionMode::Plan` remains in M1's enum. The driver intentionally
does **not** flip the policy engine into `Plan` mode during plan
generation — that would deny the internal `SubmitPlan` tool. Instead
the driver restricts the tool registry to `SubmitPlan` plus the
read-only allow list, leaves the policy in `Default`, and adds a
planning-mode system prompt that bounds the model.

## CLI surface

New top-level commands:

```
code-agent plan <PROMPT> [--mode no-tools|read-only|read-only-external]
                         [--max-steps N] [--provider X] [--model Y]
code-agent act <PLAN_ID|latest> [--permission-mode MODE]
                                [--strict-plan] [--no-enforce-plan-scope]
code-agent replan [PLAN_ID|latest] [INSTRUCTIONS]
code-agent plans list   [--status all|draft|pending|approved|...]
code-agent plans show   <PLAN_ID> [--format pretty|json|markdown]
code-agent plans approve <PLAN_ID> [--notes ...]
code-agent plans reject  <PLAN_ID> [--reason ...]
code-agent plans revise  <PLAN_ID> <INSTRUCTIONS>
code-agent plans edit    <PLAN_ID>          # $EDITOR or --print-path
code-agent plans diff    <PLAN_ID> [--from-version N --to-version M]
code-agent plans archive <PLAN_ID>
code-agent plans export  <PLAN_ID> [--format json|markdown --output PATH]
```

`code-agent run --require-plan` refuses to run unless an approved
plan already exists in the plan store. Useful for automation that
wants planning to be mandatory.

The CLI plan handlers (`crates/code-agent-cli/src/plan_cmd.rs`) reuse
`resolve_common` so plan/act runs pick up the same provider, MCP
configuration, tool registry, and validation settings as `run`.

## Slash commands

Nine new built-in commands live in
`crates/code-agent-core/src/slash/commands.rs`:

```
/plan <prompt>
/plans
/approve-plan [PLAN_ID|latest]
/reject-plan  [PLAN_ID|latest]
/revise-plan  <instructions>
/edit-plan
/plan-diff
/act          [PLAN_ID|latest]
/replan       [instructions]
```

These resolve via the existing `SlashCommandRegistry` and run inside
both `code-agent run` and `code-agent chat`. Per the registry's
ordering (custom commands → built-ins → skills), plugin slash commands
cannot shadow `/plan`, `/act`, or `/replan` unless they're installed
as project-level custom commands.

## SubmitPlan / PlanUpdate

`SubmitPlan` (planning) and `PlanUpdate` (act) are internal `Tool`
implementations. They are registered onto a *cloned* tool registry by
the planning/act drivers, so they never leak into the normal CLI tool
catalog and cannot be invoked through MCP, plugins, or
`tools call`. Inputs are validated:

- `SubmitPlan` rejects empty titles, missing steps, step counts above
  `max_plan_steps`, invalid file paths (`../` traversal, absolute
  paths, drive letters), invalid command regexes, and unknown step
  kinds. The result is persisted to the plan store with status
  `PendingApproval`. Plan metadata is run through
  `redact_secrets_in_value` before write.
- `PlanUpdate` rejects unknown step statuses. The act driver consumes
  the resulting `PlanUpdateHandle` snapshot and applies updates to the
  plan document at run end.

If the model returns prose instead of calling `SubmitPlan`, the driver
falls back to `parse_submit_plan_input`, which captures the prose,
marks the plan with `unstructured_warning`, and surfaces a warning in
the CLI output.

## Plan scope enforcement

`PlanScopeCallback` wraps the user's permission callback in act mode.
For every tool call:

1. The internal `PlanUpdate` (and `SubmitPlan` during replan) is
   in-scope unconditionally.
2. `Read | Glob | Grep` are in-scope when `exploratory_reads_during_act`
   is on.
3. `Edit | Write` match against `PlanFileRef.access`: `Edit` covers
   `Write` and `Create`; `Write` covers `Create`; `Read` only
   satisfies `Read`.
4. `Bash` matches exact command or `command_regex`.
5. Other tools (including MCP) must be named somewhere in the plan
   (`requested_tools` or step `expected_tools`); otherwise they're
   `OutOfScope`.

Out-of-scope calls become `PlanDeviation` records and are routed
through the configured `deviation_policy`:

| Policy | Behavior |
|--------|----------|
| `deny` | Permission callback returns `Deny`. |
| `ask` | `PlanDeviationCallback::resolve` chooses (approve once / update / replan / deny). |
| `require_replan` | Permission callback returns `Deny` and execution stops. |
| `allow_with_warning` | Falls through to the inner callback; deviation is still recorded. |

Plan approval **never** bypasses:

- project `disallowed_tools` rules,
- hooks (PreToolUse, etc.),
- MCP external-effect policy,
- checkpoint failure rules.

## Doctor / status / status line

`code-agent doctor` gains three new checks under "Planning":

- `planning.store_writable`
- `planning.malformed` (counts plan files that fail to parse)
- `planning.stale_executing` (plans stuck in `Executing`)

`code-agent status` shows the latest plan id, version, status, and
the active enforcement mode + deviation policy.

The status-line template registers four new variables:
`{plan_id}`, `{plan_status}`, `{plan_step}`, `{plan_progress}` — for
example:

```
code-agent · my-crate · plan_01J approved · step 3/5 · ctx 42%
```

These read the most recently updated plan file under `.agent/plans/`
synchronously (no nested tokio runtime), so they work from any context.

## TUI

`crates/code-agent-tui/src/plan_view.rs` adds four deterministic
renderers used by both Plain and Fullscreen renderers:

- `format_plan_panel` — title/status/steps/validation/risks.
- `format_plan_approval_modal` — headline summary, mutating files,
  external (MCP) tools, risky steps, validation, action menu.
- `format_deviation_modal` — requested tool, approved scope, current
  policy, action menu.
- `format_plan_diff` — title/goal/per-step delta between two versions.

Each is exercised by a snapshot test.

## Demo: `examples/planning-demo`

A standalone crate (excluded from the workspace via `Cargo.toml`'s
`workspace.exclude`) with a planted bug in `subtract` and a failing
test `subtract_works`. Used to validate the full plan → approve →
act → re-plan loop offline.

```bash
cd examples/planning-demo
code-agent plan "Fix the failing subtract test"
code-agent plans show latest
code-agent plans approve latest
code-agent act latest
```

Settings enable planning, checkpointing, and `cargo test` as the
validation command. The demo never requires a real model account — it
ships the planted bug and the offline tests as the proof surface.

## Tests

20 unit tests inside the plan module and 11 integration tests in
`crates/code-agent-core/tests/planning.rs` cover:

- plan id format + path-traversal rejection,
- plan round-trip (serde, version increment, approval invalidation
  on revision),
- plan store atomic writes, listing past corrupted files, oversized
  plan rejection,
- `SubmitPlan` happy path + path-traversal rejection,
- unstructured-prose fallback,
- `PlanUpdate` step tracking,
- scope classifier (in-scope edit, out-of-scope edit, bash regex,
  exploratory reads, `PlanUpdate` always in-scope),
- end-to-end act run with scripted `ModelProvider`,
- act refusal on unapproved plans,
- act failure propagation,
- deviation policy `Deny` enforcement via wrapped callback,
- approval invalidation after revision,
- replan child plan linkage,
- plan listing surface.

Plus the TUI snapshot tests in `plan_view.rs` (panel, approval modal,
deviation modal). Total workspace count: **654 tests** (up from 623).

## Safety notes

- Plan generation never mutates the workspace: tool registry is
  restricted to `SubmitPlan` + read-only tools; policy stays at
  `Default`.
- Approval is keyed to plan version; any revision that bumps the
  version automatically resets `status` to `PendingApproval` and
  clears `PlanApproval`.
- Plan metadata is redacted before persistence
  (`redact_secrets_in_value`).
- The act driver loads the plan **once**, records the approved
  version, and refuses to run if `plan.version != approval.version`.
- Plan approval does not silently approve future tool calls; every
  call still goes through hooks, disallow rules, and the wrapped
  callback.
- `--require-plan` is enforced before the agent loop starts so
  automation can guarantee planning is mandatory.

## Out of scope (intentionally)

- Automatic long-term memory.
- Remote/cloud planning service.
- Collaborative multi-user approval.
- IDE / web UI integration.
- OpenTelemetry export.
- Model-provider routing or learned plan templates.
- Unattended plan execution in CI under subscription auth.
