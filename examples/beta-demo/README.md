# beta-demo

Offline demo crate for M9 beta UX testing.

## Try it

```sh
cd examples/beta-demo

# 1. Diagnose
crom doctor

# 2. Render a status-line preview without a TTY
crom tui --renderer plain --snapshot

# 3. Open the TUI in a real terminal
crom tui

# 4. Inside the TUI, ask:
#      fix the failing tests
#    Then try the slash commands:
#      /changes
#      /diff
#      /status
#      /doctor
#      /sessions
```

The included `src/lib.rs` has one failing test (`parse_header_rejects_empty_name`)
to give the agent something to fix end-to-end.

## What this demo exercises

- first-run setup (already done — `.agent/settings.json` and `AGENTS.md` shipped)
- fullscreen renderer with plain fallback
- status line with default template
- approval flow (Edit/Bash) with checkpoint coverage notes
- `/changes` and `/diff` with checkpoint history
- session recovery if you Ctrl+C in the middle
- `crom doctor` for diagnostics
