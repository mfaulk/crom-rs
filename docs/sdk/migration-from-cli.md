# Migrating from the CLI

The CLI (`code-agent`) still drives the existing M1–M11 flows directly
against `AgentClient`. The SDK adds a path that bypasses the CLI layer
entirely:

- The new `code-agent headless-run "<prompt>"` subcommand delegates to
  `AgentRuntime` for a single coding task.
- Library consumers should use `code-agent-sdk` directly; the CLI is a
  thin wrapper, not a required layer.

## Mapping CLI flags to SDK builder

| CLI flag                          | SDK builder/request                                    |
| --------------------------------- | ------------------------------------------------------ |
| `--cwd <dir>`                     | `WorkspaceSource::filesystem(dir)`                     |
| `--model <id>`                    | `AgentRuntimeBuilder::model(id)` / `AgentRequest::model(id)` |
| `--max-turns N`                   | `AgentRequest::max_turns(N)`                           |
| `--apply-patch`                   | `PatchPolicy::ApplyAndReturn` + `AgentMode::ModifyWorkspace` |
| `--allow-bash`                    | `SideEffectPolicy::command_execution()`                |
| `--plan-only`                     | `AgentMode::PlanOnly`                                  |
| `--api-key`/`ANTHROPIC_API_KEY`   | `ProviderConfig::anthropic_api_key(SecretString::new(key))` |

## What the SDK doesn't expose (yet)

Hosts that need the following stay on the CLI/`AgentClient` for now:

- Plugin marketplaces and `code-agent plugin` commands.
- Interactive TUI sessions and approval flows.
- The `/login` browser flow.
- Skills loading from a filesystem skills directory.
- MCP server orchestration with strict mode and inspection.

These are not architectural limits — the SDK can opt into each via a
feature flag — they are simply out of scope for M12.

## Trade-offs

- **Conservatism**: SDK defaults deny more by default than the CLI. Web
  services should keep them. Power users in a CLI may want to widen via
  `tool_policy`/`side_effect_policy`.
- **No global state**: each run is isolated. Cross-run state (approval
  memory, plugin registries) must be wired explicitly.
- **No `.agent/` reads by default**: the SDK does not look at workspace
  settings files. Hosts that want those can load them via
  `code_agent_core::settings::load` and feed the relevant pieces in.
