# Milestone 12 — Headless Coding Agent SDK + Embeddable Runtime

**Theme:** Turn the existing CLI/TUI-oriented coding agent into a stable,
embeddable Rust SDK that applications can use to generate code, produce
patches, apply changes, run validation, and stream progress events without
relying on terminal prompts, CLI flags, TUI state, global credentials, or
filesystem settings.

## What's new

A new top-level crate, `crates/code-agent-sdk`, exposes the agent as a
typed, async, headless library. The SDK is built on top of the existing
M1–M11 core; it adds public types, safety-first defaults, and an
embeddable runtime, but does not duplicate the agent loop.

| Module | Purpose |
|--------|---------|
| `runtime/` | `AgentRuntime` + `AgentRuntimeBuilder`, run dispatch, callback adapters, validation runner. |
| `workspace/` | `Workspace` trait, `WorkspacePath`, `FilesystemWorkspace`, `InMemoryWorkspace`, `ReadOnlyWorkspace`, `PatchOnlyWorkspace`, `OverlayWorkspace`. |
| `patch/` | `PatchSet`, `FilePatch`, `PatchSummary`, `CodeArtifacts`, `GeneratedFile`, `CommandSuggestion`, deterministic unified diffs. |
| `request.rs` | `AgentRequest`, `AgentMode`, `PatchPolicy`, `ApprovalPolicy`, `SessionPolicy`, `PlanPolicy`, `ValidationPolicy`, `RequestMetadata`. |
| `result.rs` | `AgentRunResult`, `AgentRunStatus`, `EventSummary`, `ValidationReport`. |
| `event/` | `SdkEvent` (typed event stream), `EventVerbosity`, `SdkEventMapper` from internal events. |
| `callback/` | `ApprovalCallback`, `CredentialResolver`, `EventSink`, `SecretRedactor`, `ToolHost`, `AgentHost`, `DenyAllApprovals` / `AllowAllApprovals` / `DefaultSecretRedactor` / `NoopSecretRedactor`. |
| `policy/` | `SideEffectPolicy`, `SideEffectLevel`, `ToolPolicy`, `BuiltinToolSet`, `InteractivityMode`, `ToolLoadingMode`. |
| `provider.rs` | `ProviderConfig`, `ProviderSelection`, `SecretString` re-export. |
| `storage/` | `AgentStorage` trait + `NoopStorage`, `InMemoryStorage`, `FileStorage` implementations. |
| `plan/` | Public `PlanRequest`/`PlanResult`/`ActRequest` mirrors of the M11 core API. |
| `budget.rs` | `RunBudgets`, `CancellationHandle`. |
| `error.rs` | `AgentSdkError` + `Diagnostic`/`DiagnosticSeverity`/`DiagnosticLocation`. |
| `fake_provider.rs` | `EchoFakeProvider` and `ScriptedFakeProvider` (feature `fake-provider`). |

## Public API shape

```rust
let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::anthropic_api_key(api_key))
    .workspace(WorkspaceSource::filesystem("./repo"))
    .build()
    .await?;

let result = runtime
    .run_once(
        AgentRequest::new("Fix the failing parser tests")
            .mode(AgentMode::GeneratePatch)
            .patch_policy(PatchPolicy::ReturnOnly)
            .max_turns(12),
    )
    .await?;
```

`run_once` returns a typed `AgentRunResult` with `summary`, `patch`,
`diagnostics`, `validations`, `plan`, `usage`, `events_summary`, and
`metadata`. `run` returns an `AgentRun` with an event stream and
cancellation handle for streaming consumers.

## Safety defaults

The SDK is configured defensively out of the box:

- `SideEffectPolicy::read_only()` — no Bash, no Edit/Write.
- `InteractivityMode::HeadlessDeny` — no approval callback consultation.
- `BuiltinToolSet::read_only_defaults()` — Read/Glob/Grep only.
- `PatchPolicy::ReturnOnly` — never mutate the workspace.
- `NoopStorage` — no session persistence by default.
- No global keychain or `.agent/` lookups.
- No MCP, plugin, skill, or subagent loading.
- Filesystem workspace + `ReturnOnly` snapshots files before the run and
  rolls back any disk mutations performed by built-in tools.

Each protection is one builder method or one feature flag away from being
relaxed.

## CLI integration

The CLI grows one new subcommand whose entire implementation runs through
`AgentRuntime`:

```
code-agent headless-run "<prompt>" [--cwd .] [--provider anthropic|fake]
    [--api-key …] [--model …] [--max-turns N] [--deadline-secs N]
    [--apply-patch] [--allow-bash] [--auto-approve] [--plan-only]
```

`crates/code-agent-cli/src/sdk_cmd.rs` builds a `ProviderConfig` from CLI
flags, configures the runtime with conservative defaults, calls
`runtime.run_once`, and prints a summary plus any patch / diagnostics /
validations. Existing CLI subcommands (`run`, `chat`, `resume`,
`continue`, `fork`, …) continue to use `AgentClient` unchanged.

## Feature flags

```toml
default = ["filesystem-workspace", "in-memory-workspace", "anthropic", "planning"]
anthropic            = ["dep:code-agent-anthropic"]
openai               = ["dep:code-agent-provider-openai"]
gemini               = ["dep:code-agent-provider-gemini"]
fake-provider        = []
mcp                  = []
plugins              = []
skills               = []
subagents            = []
checkpointing        = ["dep:code-agent-checkpoint"]
planning             = []
filesystem-workspace = []
in-memory-workspace  = []
cli                  = []   # placeholders: SDK never depends on CLI/TUI
tui                  = []
```

`cargo tree -p code-agent-sdk` confirms the SDK has no dependency on
`code-agent-cli` or `code-agent-tui` under any feature combination.

## Examples and documentation

Reference examples live under `crates/code-agent-sdk/examples/` and run
with `cargo run --example <name> --features fake-provider -p
code-agent-sdk`:

- `simple_patch.rs`
- `in_memory_workspace.rs`
- `plan_then_act.rs`
- `web_service.rs`
- `custom_tool.rs`
- `approval_callback.rs`
- `cancel_run.rs`
- `noop_storage.rs`
- `fake_provider.rs`

A larger walkthrough lives in `examples/headless-sdk-demo/` with
subcommands `simple-patch`, `in-memory`, `plan-act`, `fake-provider`,
`approval-callback`, and `cancel-run`. The demo project carries a
fixture `examples/headless-sdk-demo/fixtures/simple-rust-project/`.

Hand-written docs are under `docs/sdk/`:

- `overview.md`
- `quickstart.md`
- `workspaces.md`
- `patch-only-mode.md`
- `plan-act.md`
- `approvals.md`
- `providers-auth.md`
- `custom-tools.md`
- `storage.md`
- `events.md`
- `safety.md`
- `migration-from-cli.md`

## Tests

| Test target | What it covers |
|-------------|----------------|
| `crates/code-agent-sdk/src/**` (`cargo test -p code-agent-sdk --lib`) | 48 unit tests: workspace path safety, in-memory workspace CRUD, read-only/patch-only/overlay wrappers, filesystem path guards, patch diff/summary, diagnostics, errors, storage round-trip, side-effect policy, request validation, fake provider behavior, redactor. |
| `crates/code-agent-sdk/tests/headless_run.rs` | 17 integration tests across run/run_once/plan/act: `RunStarted`/`RunCompleted` emission, cancellation, deadlines, budgets, `PatchPolicy::ReturnOnly` snapshot + rollback, approval callback wiring, in-memory storage recording, plan-only output, validation success and side-effect-policy denial, concurrent runs isolation. |
| `crates/code-agent-cli/src/sdk_cmd.rs` tests | 2 unit tests verify the CLI delegate to the SDK runtime and reject missing API keys. |

Total: 911 workspace tests with default features, 913 with
`--features fake-provider`, all green. Baseline before M12 was 844.

## Out of scope (deferred)

- Hosted cloud agent service
- OpenTelemetry export
- IDE extensions (VS Code, JetBrains)
- Web UI beyond the small SDK example
- Sandbox/container execution
- LSP code intelligence
- Remote plugin marketplace
- Long-term memory
- Multi-provider routing
- Billing integration
- Public package publishing automation
