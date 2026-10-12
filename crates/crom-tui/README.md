# crom-tui

Terminal UI for crom.

Renders the interactive terminal experience: status line, transcript cards,
approval prompts, diffs, plan panels, and a live event-loop application shell
driven by a `crom-sdk` runtime.

## Architecture

- **Rendering primitives** — deterministic `Fullscreen` and `Plain`
  renderers, plus `statusline`, `transcript`, `approval`, `diff_view`,
  `plan_view`, `composer`, and `completion` components.
- **App shell** — `TuiApp` runs the live loop: keyboard, resize, and
  `SdkEvent` streams feed a pure reducer (`TuiAppState`) whose actions
  (`TuiAction`) are executed by the host loop. `drive_single_prompt` runs a
  one-shot prompt.
- **Approval bridge** — `ApprovalBridge` connects interactive approval prompts
  to the runtime's permission callbacks.

Heavy terminal I/O is kept out of unit tests: ratatui's `TestBackend` backs
snapshot tests and the reducer is exercised directly, so behavior is verified
without a real TTY.

## Usage

Consumed by the `crom` CLI to provide its interactive mode.
