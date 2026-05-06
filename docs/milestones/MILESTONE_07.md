# Milestone 7 — Skills + Slash Commands Alpha

**Theme:** Package repeatable project workflows as *Skills*
(`.agent/skills/<name>/SKILL.md`) and *slash commands*
(`.agent/commands/*.md`) so users can invoke them either explicitly or
through a built-in `Skill` tool. Adds manual `/compact` and a deterministic
older-turn summarizer.

## Skills

Filesystem layout:

```
.agent/skills/<name>/
  SKILL.md
  templates/...
  resources/...
```

`SKILL.md` is YAML frontmatter + Markdown body:

```yaml
---
name: release-pr
description: Create a release pull request summary from CHANGELOG.md and recent git history.
argument-hint: <version>
allowed-tools:
  - Read
  - Grep
  - Glob
  - Bash(git diff *)
  - Bash(git log *)
disallowed-tools:
  - Edit
  - Write
resources:
  - templates/pr-summary.md
---

You are helping the user write the release pull request summary...
```

A registry + filesystem loader discovers Skills with size caps, binary
detection, path-traversal rejection, and symlink-escape rejection.

## Built-in `Skill` tool

`PermissionKind::Orchestration` — not auto-approved by `acceptEdits`. The
agent loop refuses Skill-backed slash dispatch unless `Skill` is in
`--allowed-tools`.

Skill resources are loaded on demand through a safe loader (size caps +
sandbox + symlink check) and surfaced via a `SkillResourceLoaded` event.

## Slash commands

A parser + dispatcher routes `/<name>` to:

- **Built-in commands**: `/help`, `/context`, `/usage`, `/tools`,
  `/skills`, `/agents`, `/checkpoints`, `/compact`.
- **Skill-backed commands** — running a Skill by its slash name.
- **Custom files** in `.agent/commands/*.md`.

Custom command expansion supports:

- `$1` / `$ARGUMENTS` positional substitution.
- `@file` file references (routed through the existing `Read` tool with
  path sandbox + size caps).
- `` !`<bash>` `` bash expansion (routed through `BashTool` with timeout,
  output cap, and secret redaction). Substitutions inside bash blocks are
  POSIX single-quoted to defeat command injection.

`PreToolUse` hooks run before each expansion call. Captured output goes
through the same redaction pass as the live tool pipeline.

## Manual `/compact`

Deterministic summarizer that compacts older turns and preserves the most
recent N. Emits a `CompactBoundary` record in the session JSONL. In `chat`
mode `/compact` runs on the live conversation via `AgentClient::compact`.

## SystemInit + telemetry

`SystemInit` carries Skill and slash-command summaries (not full bodies).
New events:

- `SkillRegistryLoaded`, `SkillLoaded`, `SkillResourceLoaded`
- `SlashCommandStarted`, `SlashCommandExpanded`, `SlashCommandCompleted`,
  `SlashCommandFailed`
- `CompactBoundary`

Matching telemetry variants are emitted to the normal event stream.

## Settings

```json
{
  "skills": {
    "enabled": true,
    "project_path": ".agent/skills",
    "allow_filesystem_skills": true,
    "compat_claude_skills": false,
    "max_skill_md_bytes": 65536,
    "max_resource_bytes_per_skill": 1048576,
    "max_loaded_skills_per_run": 8,
    "load_resources_on_demand": true,
    "enforce_skill_allowed_tools": false
  },
  "slash_commands": {
    "enabled": true,
    "project_path": ".agent/commands",
    "compat_claude_commands": false,
    "enable_builtin_commands": true,
    "allow_bash_expansion": true,
    "allow_file_references": true,
    "max_expanded_prompt_bytes": 131072
  },
  "compaction": {
    "enabled": true,
    "preserve_recent_turns": 6,
    "max_summary_tokens": 4000
  }
}
```

## CLI surface

```sh
code-agent skills list --cwd .
code-agent skills show release-pr --cwd .
code-agent skills validate --cwd .
code-agent skills inspect release-pr --cwd .
code-agent skills init my-skill --cwd .

code-agent commands list --cwd .
code-agent commands show release-pr --cwd .
code-agent commands validate --cwd .
```

New flags on `run` / `chat`: `--skills`, `--commands`, `--no-skills`,
`--no-slash-commands`, `--compat-claude-skills`,
`--compat-claude-commands`. Slash dispatch is wired into both `run` and
`chat`.

## Safety

Reviewed in two `second-opinion` (codex + gemini) passes:

- POSIX single-quote escape for positional substitutions inside
  `` !`bash` `` blocks.
- Skill-backed slash dispatch refuses unless `Skill` is in
  `--allowed-tools`.
- CLI `--cwd` honored when resolving Skill / command paths.
- Custom command discovery canonicalizes each subdir before recursing
  (rejects symlink loops + escapes).
- All `String::truncate` sites switched to a UTF-8-safe helper.
- Expansion executor routes through `ReadTool` and `BashTool`, reusing the
  path sandbox, output cap, timeout, and secret redaction.
- `ResourceEscape` no longer masks `Io(NotFound)`.

## Demo

`examples/skills-demo/` — two Skills (`release-pr`, `rust-api-review`),
two custom commands (`release-pr`, `review-changes`), and 5 deterministic
offline tests.
