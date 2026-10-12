# Milestone 9 — Terminal Beta UX

**Theme:** Make the agent comfortable to drive from a terminal — a
ratatui-backed TUI, approval cards for every tool source, hunk-grouped
diffs, atomic run-locks with session recovery, first-run setup, and a
diagnostics surface (`doctor`) suitable for CI. M9 is local-only; remote
sync, multi-session UI, and shared dashboards remain out of scope.

## New crate: `crom-tui`

Renderer abstraction with two backends:

| Backend | Purpose |
|---------|---------|
| `fullscreen` | ratatui-driven TUI for interactive terminals |
| `plain` | line-mode fallback for non-TTY hosts and CI snapshots |

Modules:

- `renderer.rs` — backend trait + selection.
- `transcript.rs` — agent message / tool call rendering.
- `composer.rs` — input editor with configurable submit key
  (`enter` or `ctrl_enter`); both keys are wired so whichever the host
  can detect actually submits.
- `approval.rs` — approval cards for `Edit`, `Write`, `Bash`, `MCP`,
  `Agent`, `Skill`, and plugin-sourced tools.
- `diff_view.rs` — hunk-grouped unified-diff renderer; truncation cap
  preserves visibility of late-file edits.
- `statusline.rs` — width-aware truncation using `unicode-width`
  (CJK / emoji count as 2 columns).
- `fullscreen.rs` / `plain.rs` — concrete backends.

Tests use ratatui's `TestBackend` for deterministic snapshot coverage.

## Run locks + session recovery

`crom-session` gains `recovery.rs`:

- Locks acquired atomically via `OpenOptions::create_new` — no
  TOCTOU window between probe and write.
- `detect_state` classifies a session without loading the full JSONL:
  `tail_probe` reads ~64 KB from `SeekFrom::End` to find a terminal
  `Result` event.
- New `Interrupted` classification covers sessions that have records
  but no `Result` and no live lock.
- `sessions-ext recover` appends a `SessionRecovered` audit marker
  to honor the recovery contract.
- Stale-lock age is configurable via
  `recovery.stale_lock_after_seconds` (no more hardcoded 3600s).
- `collect_clean_candidates` skips files with unreadable mtime instead
  of treating them as ancient and promoting them onto the delete list.
- `parse_age_spec` uses `checked_mul` so absurd inputs like
  `9999999999999d` produce a clear overflow error.

## Permission decision: `RequestRevision`

Reviewers can steer the agent instead of just denying — a new
`PermissionDecision::RequestRevision { feedback }` variant feeds the
reviewer's note back into the loop as actionable context.

## CLI surface

```sh
crom init                             # first-run scaffold
crom doctor [--json] [--fix] [--yes]  # diagnostics; --fix only creates missing .agent/ dirs
crom changes [summary|diff|...]       # workspace inspection over checkpoint history
crom sessions-ext recent              # honors recovery.stale_lock_after_seconds
crom sessions-ext recover <ID>        # appends SessionRecovered marker
crom paths                            # resolved config / data / cache locations
crom status                           # current session + lock state
crom config [...]                     # inspect merged settings layers
crom statusline [--max-width N]       # render status-line preview
crom tui [--renderer fullscreen|plain] [--snapshot]
crom completions <shell>              # shell completion script
```

`doctor --fix` requires `--yes` (or interactive confirmation) before
writing anything. JSON output is redacted and CI-suitable.

## Settings

```json
{
  "ui": {
    "renderer": "fullscreen",
    "status_line": { "template": "...", "max_width": 120 },
    "composer":    { "submit_key": "enter" },
    "notifications": { "enabled": true }
  },
  "recovery": {
    "stale_lock_after_seconds": 3600,
    "auto_recover": false
  }
}
```

Plugin defaults overlay explicitly does **not** apply `ui` or
`recovery` — plugins must not silently flip the renderer or submit key.
Round-trip through `SettingsFile` preserves user-set ui/recovery values.

## Safety

Two `second-opinion` (Codex + Gemini) passes resolved:

- TOCTOU on lock acquisition — replaced probe-then-write with atomic
  `create_new`.
- JSONL load amplification in `detect_state` — replaced full-file
  scan with bounded `tail_probe`.
- Diff truncation cap hiding late-file edits — switched to hunk-grouped
  rendering that keeps every hunk's header visible under the cap.
- Plugin-default data loss for ui/recovery — overlay now scoped to the
  fields plugins are allowed to seed.
- `ChangesCommand::Summary` byte deltas were always 0 — now derived
  from the unified-diff content via `count_unified_diff_bytes`.
- `block_on(detect_state(...))` calls in async paths replaced with
  `.await` so the Tokio worker pool is not blocked.
- Status-line cap of 0 returns empty; cap of 1 returns just an
  ellipsis — both honor the `max_width` contract exactly.

## Demo

`examples/beta-demo/` — offline scaffold with one intentionally failing
test (`parse_header_rejects_empty_name`) so the agent has something to
fix end-to-end. Exercises `doctor`, `init`, and `tui --renderer plain
--snapshot` without a live Anthropic key.

## Tests

533 tests pass workspace-wide. New regressions cover atomic locks,
late-file diff visibility, `statusline-set` safety, doctor confirmation,
orphan lock cleanup, `CtrlEnter` submit path, `parse_age_spec` overflow,
`Interrupted` classification, large-file `tail_probe`, wide CJK
status-line truncation, and zero-width statusline cap.
`cargo fmt --all --check` and `cargo clippy --workspace --all-targets`
are clean.

## Out of scope

- Remote session sync, multi-session UI, shared dashboards.
- Mouse / image protocol support in the fullscreen renderer.
- Auto-recovery of interrupted sessions (gated behind
  `recovery.auto_recover`, default off).
