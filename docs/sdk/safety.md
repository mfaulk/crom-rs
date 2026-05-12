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

## Threats addressed

- **Inadvertent mutation**: ReturnOnly + read-only side effects + no Bash.
- **Credential leakage**: `SecretString` redacts in `Debug`; the
  `SecretRedactor` runs on diagnostics and assistant text.
- **Tenant contamination**: no global state across runs; `SessionPolicy`
  defaults to no persistence.
- **Resource exhaustion**: `RunBudgets` caps tokens/turns/time/patch size.
- **Path traversal**: `WorkspacePath` rejects absolute paths, `..` and
  symlink escapes.
