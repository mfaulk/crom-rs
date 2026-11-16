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
   size — independent of the model.

Built-in file and shell tools are bound to the workspace, never to the
host process cwd: `ReturnOnly`/`RejectMutations` run them in a private
scratch copy, apply policies use the real root, and a rootless workspace
under an apply policy gets none (see [Workspaces](workspaces.md)).

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
