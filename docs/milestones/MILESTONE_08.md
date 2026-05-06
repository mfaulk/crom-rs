# Milestone 8 — Plugin System Alpha

**Theme:** Package reusable agent behavior — Skills, slash commands,
subagents, hooks, MCP servers, settings defaults, and helper binaries —
into installable, versioned, namespaced bundles. M8 implements
**local-only** install / cache / load. Remote marketplaces, signing, and
dependency resolution are explicitly out of scope.

## Manifest

`<plugin>/.agent-plugin/plugin.json` (with opt-in compatibility for
`.claude-plugin/plugin.json`):

```json
{
  "name": "rust-quality",
  "version": "1.0.0",
  "description": "Rust code review skills, agents, hooks, and MCP tools.",
  "author": { "name": "Example Team", "email": "devtools@example.com" },
  "homepage": "https://example.com/rust-quality",
  "license": "MIT",
  "keywords": ["rust", "review", "quality"],
  "skills": "./skills/",
  "commands": "./commands/",
  "agents": "./agents/",
  "hooks": "./hooks/hooks.json",
  "mcpServers": "./.mcp.json",
  "settings": "./settings.json",
  "bin": "./bin/"
}
```

Strict identifier-safe `name` and `version` validation; relative paths
only; no traversal or symlink escape.

## Install scopes

| Scope | Layout |
|-------|--------|
| `user` | `~/.config/code-agent/plugins/<name>/<version>/` |
| `project` | `<cwd>/.agent/plugins/<name>/<version>/` |
| `local` | `<cwd>/.agent/plugins.local/<name>/<version>/` |

Installs are atomic — plugin contents are staged in a temp dir, then a
backup-rename promotes the staging dir into place, so a failure mid-copy
preserves any prior install of that name+version.

## Component namespacing

| Component | Namespaced as |
|-----------|---------------|
| Skills | `plugin:skill` |
| Slash commands | `/plugin:command` |
| Subagents | `plugin:agent` |
| MCP servers | `plugin_server` (sanitized for the registry) |

Settings defaults from plugins merge **below** user / project / local /
CLI layers — `settings.local` scalars never silently override user or
project security toggles. `null`-valued plugin defaults are treated as
unset and skipped.

## Variable substitution

Inside plugin manifests / configs:

- `${CODE_AGENT_PLUGIN_ROOT}` — resolves to the plugin's installed root.
- `${CODE_AGENT_PLUGIN_DATA}` — resolves to the plugin's persistent data
  directory.
- `${ENV_VAR}` — environment variable lookup.
- `${user_config.<key>}` — user-supplied configuration value.

## Bin PATH injection

**OFF by default.** When enabled per-plugin, the plugin's `bin/` directory
is added to `PATH` for the agent run. The current setting is surfaced in
`plugin show` output.

## Local marketplace

Workspaces can register a local marketplace JSON file (strict-mode
containment check). Marketplace entries can then be installed by token:

```json
{
  "name": "local-devtools",
  "owner": { "name": "DevTools Team" },
  "plugins": [
    {
      "name": "rust-quality",
      "source": "./plugins/rust-quality",
      "description": "Rust review skills and agents",
      "version": "1.0.0"
    }
  ]
}
```

## Telemetry + events

New plugin events emitted to the event stream and session JSONL:

- `PluginRegistryLoaded`
- `PluginLoaded` / `PluginDisabled` / `PluginErrored`
- `PluginComponentLoaded` (kind = skill/command/agent/hook/mcp_server/setting_default/bin)

`SystemInit` carries plugin summaries.

## CLI surface

```sh
code-agent plugin install <PATH-OR-MARKETPLACE-TOKEN> [--scope project|user|local] [--enable] [--dry-run] [--force]
code-agent plugin uninstall <NAME> [--scope ...] [--keep-data] [--yes]
code-agent plugin enable  <NAME> [--scope ...]
code-agent plugin disable <NAME> [--scope ...]
code-agent plugin list    [--all] [--json]
code-agent plugin show    <NAME> [--json]
code-agent plugin validate <PATH>
code-agent plugin reload
code-agent plugin init <NAME>
code-agent plugin prune

code-agent plugin marketplace add <PATH>
code-agent plugin marketplace list
code-agent plugin marketplace show <NAME>
```

New flags on existing commands: `--plugin-dir`, `--no-plugins`.

## Safety

Two pass-3 review findings closed:

- Plugin settings defaults treated correctly even after a round-trip
  through `SettingsFile` (both "absent" and "explicit `null`" are now
  treated as unset).
- `open_cached_install` re-validates `enabled_plugins[].name` and
  `[].version` from settings via `is_plugin_name_safe` /
  `is_plugin_version_safe` and asserts lexical containment under the
  cache root before any filesystem read.

## Demo

`examples/plugin-demo/` — `rust-quality` example plugin (Skills, custom
commands, subagents, hooks, MCP server, settings defaults, helper bin),
local `marketplace.json`, and `examples/plugin-demo.sh` which drives
validate → install → list → show → disable → enable → reload →
uninstall, all offline.

## Tests

12 plugin CLI integration tests (offline) plus ~30 unit tests across the
new modules. Existing M1–M7 tests untouched and passing.

## Out of scope

- Remote marketplaces, plugin signing, network installs.
- Dependency resolution between plugins.
