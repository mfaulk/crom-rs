# Safety defaults

The SDK's defaults are designed to be *embeddable*: an unconfigured runtime
should never make a destructive choice on the host's behalf.

| Default                                         | Opt-in API                                              |
| ----------------------------------------------- | ------------------------------------------------------- |
| `SideEffectPolicy::read_only()`                 | `.side_effect_policy(SideEffectPolicy::*)`              |
| `InteractivityMode::HeadlessDeny`               | `.interactivity(InteractivityMode::*)`                  |
| `BuiltinToolSet::read_only_defaults()`          | `.tool_policy(ToolPolicy { enable_builtin_tools, .. })` |
| No approval callback (Deny)                     | `.approval_callback(Arc::new(...))`                     |
| No filesystem mutation (`PatchPolicy::ReturnOnly`) | `.patch_policy(PatchPolicy::ApplyToWorkspace)`       |
| No Bash                                         | Allow command execution + register `BashTool`           |
| No plugin loading                               | Feature flag `plugins` + opt-in registration            |
| No MCP                                          | Feature flag `mcp` + explicit registration              |
| No global credential lookup                     | `ProviderConfig::FromAuthStore`                         |
| `NoopStorage`                                   | `.storage(Arc::new(...))`                               |
| No external network beyond the model            | Model provider call only                                |

Threat model, trust boundaries, and required OS-enforcement properties:
[`docs/security/threat-model.md`](../security/threat-model.md). The SDK
controls below are policy-level. For OS enforcement of built-in `Bash` calls on
Linux, set `ToolPolicy { sandbox: crom_core::SandboxConfig::required(), .. }`
(default `off`); a command the host cannot sandbox is refused with
`ToolError::PermissionDenied`. See
[`docs/security/sandbox.md`](../security/sandbox.md).

## Defense in depth

Every layer enforces:

1. `SideEffectPolicy` blocks the broad category (writes, exec, network).
2. `ToolPolicy.enable_builtin_tools` controls which built-ins register.
3. `AgentRequest.approval_policy` chooses how the policy engine decides.
4. `WorkspaceCapabilities` lets the runtime reject operations the workspace
   itself does not support.
5. Per-tool `PermissionKind` gates approval prompts.
6. `RunBudgets` caps turns, tool calls, duration, files changed, patch
   size, cumulative input/output tokens, and nominal cost — independent of
   the model. Token and cost caps are checked after every model turn and
   stop the run with a `budget.max_input_tokens_exceeded`,
   `budget.max_output_tokens_exceeded`, or `budget.max_cost_exceeded`
   diagnostic. They fail closed: if usage is unreported, or a cost cap has
   no price (`AgentRuntimeBuilder::pricing`), the run stops with
   `budget.usage_unknown`. Plan modes do not meter usage, so a request
   that sets a token or cost cap with `PlanOnly`/`PlanThenAct` is rejected
   with `budget.unsupported_mode`. Cost is nominal list price, not billed
   spend ([cost semantics](../providers.md#cost-semantics)).

Built-in file and shell tools are bound to the workspace, never to the
host process cwd: `ReturnOnly`/`RejectMutations` run them in a private
scratch copy, apply policies use the real root, and a rootless workspace
under an apply policy gets none (see [Workspaces](workspaces.md)).

## Repository-local configuration

The SDK never reads a workspace's `.agent/settings.json` or `.agent/mcp.json`;
every policy, tool and MCP setting comes from the embedder's builder calls
(`sdk_ignores_repo_local_agent_config` in `crom-sdk/tests/headless_run.rs`).
`AgentClientBuilder::mcp_config_file` is operator-sourced and keeps `${VAR}`
interpolation.

The CLI does read them, under a persisted workspace trust decision (#290):
untrusted project settings may only tighten policy, repo MCP config is not
loaded, and repo-sourced MCP config never gets `${VAR}` interpolation. See
[Workspace trust](../cli.md#workspace-trust). Embedders that want the
same behaviour can load settings with `crom_core::settings::load` (which
consults the trust store) or `load_with_trust`, and MCP options with
`crom_mcp::load_mcp_options(.., workspace_trusted)`; ignored entries are in
`Settings::untrusted_denials` and `McpOptions::repo_config_denials`.

## Threats addressed

- **Inadvertent mutation**: ReturnOnly + read-only side effects + no Bash.
  Under `ReturnOnly` and `RejectMutations` the tools never see the
  caller's root.
- **Credential leakage**: `SecretString` redacts in `Debug`; the
  `SecretRedactor` runs on diagnostics and assistant text.
- **Tenant contamination**: no global state across runs; `SessionPolicy`
  defaults to no persistence.
- **Resource exhaustion**: `RunBudgets` caps tokens/turns/time/patch size.
- **Path traversal**: `WorkspacePath` rejects absolute paths, `..` and
  symlink escapes.
- **Unapproved MCP effects**: the MCP external-effect policy (default
  `RequireExplicitAllowRule`) is a floor in the policy engine. A
  `PreToolUse` hook `Allow`, the `dontAsk` tool-name list, permission modes
  and approval memory cannot run an MCP tool that no explicit allow rule
  names; the denial is audited with `policy_source: external_effect_policy`.
  A `PostToolUse` hook `Deny` (e.g. `McpResultSizeLimitHook`) withholds
  the result from the model but cannot undo an effect the tool already had.
