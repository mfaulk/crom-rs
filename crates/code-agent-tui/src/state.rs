//! Pure, deterministic state machine that drives the live TUI.
//!
//! `TuiAppState` owns the transcript, composer state, activity log, run
//! status, and status-line context. It is mutated only through
//! [`TuiAppState::apply`], which consumes a [`TuiInbound`] event and
//! returns a list of side-effect intents ([`TuiAction`]) for the host
//! event loop to perform. All terminal I/O, runtime calls, and async
//! work live in `app.rs`; this module remains synchronous and pure so
//! it can be exercised by unit tests with no terminal or runtime
//! involvement.

use std::cell::RefCell;
use std::collections::HashSet;

use code_agent_core::plan::PlanDocument;
use code_agent_core::StatusLineContext;
use code_agent_sdk::patch::PatchSet;
use code_agent_sdk::SdkEvent;
use tracing::debug;

use crate::completion::{
    context_token_candidates, detect_trigger, slash_candidates, CompletionKind, CompletionPopup,
    SLASH_COMMANDS,
};
use crate::composer::{ComposerState, KeyAction, KeyEvent};
use crate::event_adapter::map_sdk_event;
use crate::inspectors::Inspectors;
use crate::layout::{
    DetailPanelMode, DetailPanelState, FocusPane, LayoutState, SidePanelMode, TuiModal,
};
use crate::transcript::EventCard;

/// Modal-id prefix used by Phase E to mark the approval modal that
/// hovers over the workbench. Lets the modal-key handler distinguish
/// it from generic Custom modals.
pub const APPROVAL_MODAL_ID: &str = "phase-e-approval";

/// Modal-id prefix used by Phase E to mark the plan-approval modal.
pub const PLAN_APPROVAL_MODAL_ID: &str = "phase-e-plan-approval";

/// Max bytes accepted in a single paste before the reducer truncates
/// and surfaces a warning. Phase D ceiling; later phases can wire
/// `ui.composer.large_paste_warning_bytes` through `TuiAppState`.
pub const PASTE_HARD_LIMIT: usize = 64 * 1024;

/// Threshold at which a paste produces a warning entry in the
/// activity log. Pastes between this and [`PASTE_HARD_LIMIT`] are
/// inserted in full but logged.
pub const PASTE_WARN_BYTES: usize = 32 * 1024;

/// Maximum number of transcript cards retained in state. Bounded so a
/// long-running session doesn't grow unbounded memory; later phases will
/// virtualize scrollback over a larger backing store.
pub const TRANSCRIPT_CAP: usize = 1024;

/// Maximum number of activity-log entries retained.
pub const ACTIVITY_CAP: usize = 16;

/// Side-effect intents emitted by the reducer for the host loop to handle.
///
/// All UI-internal mutations (panel toggling, focus cycling, modal stack)
/// happen inside the reducer and produce no `TuiAction`. The host loop
/// only needs to react to actions that cross the runtime boundary or end
/// the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    /// Submit a new prompt to the runtime. The host should construct an
    /// `AgentRequest`, call `runtime.run(...)`, and feed the resulting
    /// event stream back into the reducer.
    SubmitPrompt(String),
    /// Cancel the active run, if any. No-op when no run is in flight.
    CancelRun,
    /// Exit the TUI cleanly.
    Exit,
    /// M13 Phase E: forward an approval decision to the host so it can
    /// resolve the SDK's `ApprovalCallback` callback. The host is
    /// responsible for matching this back to the pending
    /// `ApprovalRequired` request id; the reducer does not own that
    /// channel.
    ResolveApproval(ApprovalDecisionAction),
    // #27: the former `ApprovePlan` / `RejectPlan` / `RevisePlan`
    // variants were removed. The SDK has no host-callable plan-decision
    // surface yet, so those actions only fed no-op handlers in app.rs
    // while the plan auto-approved internally — pressing "reject" still
    // let the run proceed. To stop the UI lying, the plan-approval modal
    // is now a dismissible informational notice that binds no decision
    // keys. Wiring a real host-side PlanDecisionCallback (issue #27
    // option 1) is the tracked follow-up that would reintroduce a
    // decision action with actual enforcement.
}

/// Reducer-side mirror of the SDK's `ApprovalDecision`. We don't depend
/// on the SDK type directly here so the reducer stays
/// transport-agnostic; the host translates this to whatever its
/// `ApprovalCallback` expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecisionAction {
    Allow,
    AllowForSession,
    Deny { reason: String },
    RequestRevision,
}

/// Events the reducer consumes.
///
/// `Sdk` boxes its payload because `SdkEvent` is much larger than every
/// other variant (clippy `large_enum_variant`), and inbound events are
/// allocated per delivery anyway.
#[derive(Debug, Clone)]
pub enum TuiInbound {
    /// A typed SDK event from the active run.
    Sdk(Box<SdkEvent>),
    /// The active run's event stream encountered a fatal error.
    RunStreamError(String),
    /// The active run's event stream finished. Resets run status if a
    /// terminal SDK event has not already been observed.
    RunStreamEnded,
    /// A key press from the terminal, already decoded to the composer's
    /// abstract `KeyEvent` enum.
    Key(KeyEvent),
    /// A whole-paste payload. Phase D delivers bracketed-paste data as
    /// a single event so the reducer can compute size, warn on large
    /// pastes, and insert the text in one go instead of per-char.
    Paste(String),
    /// Terminal was resized to the given dimensions.
    Resize { width: u16, height: u16 },
}

impl TuiInbound {
    /// Convenience: wrap an `SdkEvent` into a boxed `Sdk` variant.
    pub fn sdk(event: SdkEvent) -> Self {
        Self::Sdk(Box::new(event))
    }
}

/// Run lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
}

/// Top-level TUI state. Phase B added `layout` for the multi-pane
/// cockpit; Phase C adds transcript navigation + search state.
/// Later phases extend without breaking this surface.
///
/// `#[non_exhaustive]` reserves the right to add private fields in
/// future phases without forcing every literal construction to update.
/// Internal code in this crate constructs the type via [`Self::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TuiAppState {
    pub composer: ComposerState,
    pub transcript: Vec<EventCard>,
    pub activity: Vec<String>,
    pub status_template: String,
    pub status_ctx: StatusLineContext,
    pub run_status: RunStatus,
    pub last_error: Option<String>,
    pub size: (u16, u16),
    pub should_quit: bool,
    pub layout: LayoutState,
    /// Monotonic id assigned to each card as it enters state. Used to
    /// give cards a stable identity that survives filtering / scrolling.
    pub next_card_id: u64,
    /// Id of the currently-selected transcript card, if any. `None`
    /// means no selection; navigation keys initialize selection to the
    /// most recent visible card.
    pub selected_card_id: Option<u64>,
    /// Ids of cards the user has explicitly expanded (e/Enter). Cards
    /// with empty `expanded_body` ignore the expansion bit so the
    /// renderer doesn't paint phantom whitespace.
    pub expanded_card_ids: HashSet<u64>,
    /// Active substring filter for the transcript. `None` means no
    /// filter; the empty string is normalized to `None` on commit.
    pub search_query: Option<String>,
    /// Input buffer for the in-progress search modal (filled when the
    /// SearchTranscript modal is open).
    pub search_input: String,
    /// Cached visible indices for the active `search_query`. Recomputed
    /// only when the query or transcript content changes, so the
    /// renderer's per-frame call to `visible_card_indices` is cheap
    /// even for large transcripts with case-insensitive substring
    /// matching.
    ///
    /// `RefCell` so `visible_card_indices(&self)` can memoize on a cache
    /// miss (#227): every `push_transcript` invalidates the cache, and
    /// without lazy repopulation the renderer would re-run the O(n)
    /// substring scan on every frame while streaming under an active
    /// filter. The cache is always either `None` (invalidated) or a
    /// value computed against the current transcript + query, so a lazy
    /// fill is always correct.
    visible_cache: RefCell<Option<Vec<usize>>>,
    /// Active completion popup, if any. Phase D autocomplete state:
    /// rebuilt by `refresh_composer_popup` after every key that
    /// mutates the composer buffer.
    pub composer_popup: Option<CompletionPopup>,
    /// Phase E: the most recently created plan document, if any. The
    /// plan dashboard renders this when the side panel is in `Plan`
    /// mode. Cleared when a new plan supersedes it.
    pub current_plan: Option<PlanDocument>,
    /// Phase E: pending approval request (tool name + summary) pulled
    /// from an `SdkEvent::ApprovalRequired` event. The detail panel's
    /// `Approval` mode and the approval modal render this. Cleared
    /// when the corresponding `ApprovalResolved` event fires.
    pub pending_approval: Option<PendingApproval>,
    /// #80: snapshot of the detail-panel mode/visibility taken when
    /// `open_approval_modal` overrides the panel to show the approval.
    /// Restored when the approval resolves so the prior content (e.g. a
    /// diff) is not lost. `None` when no approval is overriding the panel.
    pub prior_detail_panel: Option<DetailPanelState>,
    /// Phase E: most recent proposed patch (the diff workbench's
    /// payload). Populated from `SdkEvent::PatchProposed`.
    pub last_patch: Option<PatchSet>,
    /// Phase F: snapshot of host registries (tools / MCP / subagents /
    /// skills / plugins) for the side-panel inspectors. The reducer
    /// only reads from this; the host populates it before
    /// constructing `TuiAppState`. Defaults to empty.
    pub inspectors: Inspectors,
    /// Phase F: per-inspector selection index. Each field tracks the
    /// row a user has selected in that panel; `None` means
    /// uninitialized (first nav key seeds it to 0). Indices may point
    /// at a stale row if the host hot-reloads the snapshot; the
    /// renderer clamps to the visible range.
    pub inspector_selection: InspectorSelection,
    /// Phase G: bounded ring buffer of [`DiagnosticEntry`] records
    /// pulled from `SdkEvent::Diagnostic` events. Renders into the
    /// Diagnostics side panel and is also surfaced in the activity
    /// log.
    pub diagnostics: Vec<DiagnosticEntry>,
    /// Phase G: selection cursor for the Diagnostics panel. Same
    /// `None`-initialized-then-seeds-to-0 semantics as the inspector
    /// cursors.
    pub diagnostic_selection: Option<usize>,
    /// Phase H: selection cursor for the Activity panel.
    pub activity_selection: Option<usize>,
    /// Phase H: state carried by the command palette modal.
    /// `None` when the palette is closed.
    pub palette: Option<PaletteState>,
}

/// Phase H: state carried by the command palette modal. The reducer
/// rebuilds the filtered candidate list on demand from `input`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaletteState {
    /// Current input buffer (the substring filter).
    pub input: String,
    /// Highlighted row in the filtered candidate list.
    pub selected: usize,
}

/// Phase G: TUI-local mirror of [`code_agent_sdk::error::Diagnostic`].
/// Stays plain-string so the renderer doesn't depend on the SDK enum
/// shape; the event-adapter step does the translation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticEntry {
    /// Severity label: `info`, `warning`, `error`.
    pub severity: String,
    /// Stable diagnostic code (e.g. `tool/exec/timeout`).
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional hint produced by the SDK to nudge the user toward a fix.
    pub suggestion: Option<String>,
}

/// Maximum number of diagnostics retained in state. Past this point
/// older entries drop on push.
pub const DIAGNOSTIC_CAP: usize = 64;

/// Phase G: snake-case label for a [`code_agent_sdk::error::DiagnosticSeverity`].
fn diagnostic_severity_label(s: code_agent_sdk::error::DiagnosticSeverity) -> &'static str {
    use code_agent_sdk::error::DiagnosticSeverity::*;
    match s {
        Info => "info",
        Warning => "warning",
        Error => "error",
    }
}

/// Phase I: short tag for an [`ApprovalDecisionAction`] suitable
/// for telemetry. We never expose the `Deny { reason }` string in
/// logs to preserve the same privacy posture as `submit_prompt`'s
/// `prompt_bytes`.
fn decision_kind_label(d: &ApprovalDecisionAction) -> &'static str {
    match d {
        ApprovalDecisionAction::Allow => "allow",
        ApprovalDecisionAction::AllowForSession => "allow_for_session",
        ApprovalDecisionAction::Deny { .. } => "deny",
        ApprovalDecisionAction::RequestRevision => "request_revision",
    }
}

/// Per-inspector selection cursor. Kept as a small named struct
/// rather than five loose fields so reducer code can pattern-match
/// against the active side-panel mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSelection {
    pub tools: Option<usize>,
    pub mcp: Option<usize>,
    pub subagents: Option<usize>,
    pub skills: Option<usize>,
    pub plugins: Option<usize>,
    /// Phase G additions.
    pub sessions: Option<usize>,
    pub accounts: Option<usize>,
    pub models: Option<usize>,
    pub providers: Option<usize>,
}

/// Phase E: the minimal pending-approval record the TUI needs to
/// render the approval modal + detail-panel card. We keep this
/// reducer-local so it doesn't depend on the SDK event types
/// directly (the reducer is otherwise SDK-event agnostic outside the
/// `event_adapter`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingApproval {
    pub tool_name: String,
    pub summary: String,
}

impl TuiAppState {
    /// Construct a new state from a composer state and an initial status
    /// context.
    pub fn new(
        composer: ComposerState,
        status_template: String,
        status_ctx: StatusLineContext,
    ) -> Self {
        let mut layout = LayoutState::default();
        layout.recompute_for_size(80, 24);
        Self {
            composer,
            transcript: Vec::new(),
            activity: Vec::new(),
            status_template,
            status_ctx,
            run_status: RunStatus::Idle,
            last_error: None,
            size: (80, 24),
            should_quit: false,
            layout,
            next_card_id: 0,
            selected_card_id: None,
            expanded_card_ids: HashSet::new(),
            search_query: None,
            search_input: String::new(),
            visible_cache: RefCell::new(None),
            composer_popup: None,
            current_plan: None,
            pending_approval: None,
            prior_detail_panel: None,
            last_patch: None,
            inspectors: Inspectors::empty(),
            inspector_selection: InspectorSelection::default(),
            diagnostics: Vec::new(),
            diagnostic_selection: None,
            activity_selection: None,
            palette: None,
        }
    }

    /// Replace the inspector snapshot. Convenience for callers that
    /// pre-build the registries (e.g. CLI startup) and want to hand
    /// them to a freshly-constructed `TuiAppState`. Returns `self` so
    /// it composes with [`Self::with_initial_side_panel`].
    pub fn with_inspectors(mut self, inspectors: Inspectors) -> Self {
        self.inspectors = inspectors;
        self
    }

    /// Return the inspector list length for the active side panel
    /// mode, or `None` if the mode is not an inspector mode. Helper
    /// for tests and the renderer. Phase G adds the Sessions / Auth /
    /// Model / Provider / Diagnostics panels.
    pub fn inspector_list_len(&self, mode: SidePanelMode) -> Option<usize> {
        match mode {
            SidePanelMode::Tools => Some(self.inspectors.tools.len()),
            SidePanelMode::Mcp => Some(self.inspectors.mcp.len()),
            SidePanelMode::Subagents => Some(self.inspectors.subagents.len()),
            SidePanelMode::Skills => Some(self.inspectors.skills.len()),
            SidePanelMode::Plugins => Some(self.inspectors.plugins.len()),
            SidePanelMode::Sessions => Some(self.inspectors.sessions.len()),
            SidePanelMode::Auth => Some(self.inspectors.accounts.len()),
            SidePanelMode::Model => Some(self.inspectors.models.len()),
            SidePanelMode::Provider => Some(self.inspectors.providers.len()),
            SidePanelMode::Diagnostics => Some(self.diagnostics.len()),
            SidePanelMode::Activity => Some(self.activity.len()),
            _ => None,
        }
    }

    /// Return the selected row index for the active side panel mode,
    /// clamped to a valid index when the snapshot is non-empty.
    /// Returns `None` when the snapshot is empty or the mode is not
    /// an inspector mode.
    pub fn inspector_selected_index(&self, mode: SidePanelMode) -> Option<usize> {
        let len = self.inspector_list_len(mode)?;
        if len == 0 {
            return None;
        }
        let raw = match mode {
            SidePanelMode::Tools => self.inspector_selection.tools,
            SidePanelMode::Mcp => self.inspector_selection.mcp,
            SidePanelMode::Subagents => self.inspector_selection.subagents,
            SidePanelMode::Skills => self.inspector_selection.skills,
            SidePanelMode::Plugins => self.inspector_selection.plugins,
            SidePanelMode::Sessions => self.inspector_selection.sessions,
            SidePanelMode::Auth => self.inspector_selection.accounts,
            SidePanelMode::Model => self.inspector_selection.models,
            SidePanelMode::Provider => self.inspector_selection.providers,
            SidePanelMode::Diagnostics => self.diagnostic_selection,
            SidePanelMode::Activity => self.activity_selection,
            _ => return None,
        }
        .unwrap_or(0);
        Some(raw.min(len - 1))
    }

    /// Mutate the selection cursor for the given inspector mode.
    /// Saturates at the list bounds; no-op when the mode is not an
    /// inspector mode or when the list is empty.
    fn move_inspector_selection(&mut self, mode: SidePanelMode, delta: isize) {
        let Some(len) = self.inspector_list_len(mode) else {
            return;
        };
        if len == 0 {
            return;
        }
        let cur = self.inspector_selected_index(mode).unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        self.set_inspector_selection(mode, Some(next));
    }

    fn set_inspector_selection(&mut self, mode: SidePanelMode, value: Option<usize>) {
        match mode {
            SidePanelMode::Tools => self.inspector_selection.tools = value,
            SidePanelMode::Mcp => self.inspector_selection.mcp = value,
            SidePanelMode::Subagents => self.inspector_selection.subagents = value,
            SidePanelMode::Skills => self.inspector_selection.skills = value,
            SidePanelMode::Plugins => self.inspector_selection.plugins = value,
            SidePanelMode::Sessions => self.inspector_selection.sessions = value,
            SidePanelMode::Auth => self.inspector_selection.accounts = value,
            SidePanelMode::Model => self.inspector_selection.models = value,
            SidePanelMode::Provider => self.inspector_selection.providers = value,
            SidePanelMode::Diagnostics => self.diagnostic_selection = value,
            SidePanelMode::Activity => self.activity_selection = value,
            _ => {}
        }
    }

    /// Phase G: synthesize the tool-approval modal directly. Used by
    /// both the `SdkEvent::ApprovalRequired` event arm and the
    /// approval-bridge path so a TUI host that receives the callback
    /// before the matching event (or where the event is suppressed)
    /// still renders the modal and stores `pending_approval`.
    pub fn open_approval_modal(&mut self, tool_name: String, summary: String) {
        // #177: the approval modal can be opened from two uncoordinated
        // paths (the live bridge handler in app.rs and the
        // `SdkEvent::ApprovalRequired` reducer arm). Strip any existing
        // approval modal before pushing so the two paths can never stack
        // duplicates regardless of arrival order — there is always exactly
        // one approval modal on the stack.
        let _ = self.layout.remove_custom_modals_by_id(APPROVAL_MODAL_ID);
        self.pending_approval = Some(PendingApproval {
            tool_name: tool_name.clone(),
            summary: summary.clone(),
        });
        // #80: snapshot the prior detail-panel state before overriding it
        // so `ApprovalResolved` can restore it. Only snapshot the first
        // time so a second `open_approval_modal` (e.g. duplicate path)
        // doesn't overwrite the snapshot with the Approval override.
        if self.prior_detail_panel.is_none() {
            self.prior_detail_panel = Some(self.layout.detail_panel.clone());
        }
        self.layout.detail_panel.mode = DetailPanelMode::Approval;
        self.layout.detail_panel.visible = true;
        self.layout.push_modal(TuiModal::Custom {
            id: APPROVAL_MODAL_ID.to_string(),
            title: format!("Approve {tool_name}?"),
            body: vec![
                summary,
                String::new(),
                "  [y] yes  [s] yes-for-session  [n] no/Esc  [r] request-revision".to_string(),
            ],
        });
    }

    /// Phase G: append a diagnostic to the bounded ring buffer. Drops
    /// the oldest entry once [`DIAGNOSTIC_CAP`] is exceeded. Selection
    /// indices for the Diagnostics panel are unaffected; the renderer
    /// clamps to the new bounds. Only `warning` / `error` severities
    /// mirror to the activity log — `info` would churn the 16-entry
    /// activity panel without adding signal, since the
    /// `event_adapter` already produces a transcript card for every
    /// diagnostic.
    pub fn push_diagnostic(&mut self, entry: DiagnosticEntry) {
        if matches!(entry.severity.as_str(), "warning" | "error") {
            self.push_activity(format!(
                "{}: {} — {}",
                entry.severity, entry.code, entry.message
            ));
        }
        self.diagnostics.push(entry);
        if self.diagnostics.len() > DIAGNOSTIC_CAP {
            let excess = self.diagnostics.len() - DIAGNOSTIC_CAP;
            self.diagnostics.drain(..excess);
        }
    }

    /// Return the indices into `self.transcript` for the cards that
    /// pass the current search filter. With no filter active, returns
    /// `0..transcript.len()`.
    ///
    /// Uses an internal cache so a repeated call in the same draw
    /// cycle is O(visible). The cache is invalidated by
    /// `push_transcript` and by any mutation of `search_query` that
    /// goes through the reducer's modal path.
    ///
    /// #227: on a cache miss this recomputes *and stores* the result via
    /// interior mutability, so the per-frame renderer call stays O(1)
    /// while the agent streams cards under an active filter — previously
    /// each `push_transcript` invalidated the cache and every frame
    /// re-ran the full O(n) substring scan.
    pub fn visible_card_indices(&self) -> Vec<usize> {
        if let Some(cached) = self.visible_cache.borrow().as_ref() {
            return cached.clone();
        }
        let computed = self.compute_visible_indices();
        *self.visible_cache.borrow_mut() = Some(computed.clone());
        computed
    }

    fn compute_visible_indices(&self) -> Vec<usize> {
        match &self.search_query {
            Some(q) if !q.is_empty() => self
                .transcript
                .iter()
                .enumerate()
                .filter_map(|(i, c)| if c.matches_search(q) { Some(i) } else { None })
                .collect(),
            _ => (0..self.transcript.len()).collect(),
        }
    }

    fn invalidate_search_cache(&mut self) {
        *self.visible_cache.borrow_mut() = None;
    }

    fn refresh_search_cache(&mut self) {
        let computed = self.compute_visible_indices();
        *self.visible_cache.borrow_mut() = Some(computed);
    }

    /// Is the given card expanded? An expanded card with empty
    /// `expanded_body` still reports `true` (caller can short-circuit).
    pub fn is_card_expanded(&self, id: u64) -> bool {
        self.expanded_card_ids.contains(&id)
    }

    /// Replace the initial side panel mode + visibility. Convenience for
    /// callers that want to honor `ui.default_side_panel` /
    /// `ui.show_side_panel` settings without poking the layout field
    /// directly.
    pub fn with_initial_side_panel(mut self, mode: SidePanelMode, visible: bool) -> Self {
        self.layout.side_panel.mode = mode;
        self.layout.side_panel.visible = visible;
        self
    }

    /// Apply an inbound event. Returns the list of host-side actions that
    /// the event loop should execute (e.g. submit a prompt to the runtime,
    /// cancel the active run, exit cleanly).
    ///
    /// The reducer is total: it never panics, never blocks, and the same
    /// (state, event) input always produces the same (state', actions)
    /// output — so it is straightforward to snapshot-test.
    pub fn apply(&mut self, event: TuiInbound) -> Vec<TuiAction> {
        match event {
            TuiInbound::Sdk(ev) => {
                self.apply_sdk_event(ev.as_ref());
                Vec::new()
            }
            TuiInbound::RunStreamError(message) => {
                self.last_error = Some(message.clone());
                self.run_status = RunStatus::Idle;
                self.push_transcript(EventCard {
                    kind: "run_failed",
                    headline: "✗ run stream error".to_string(),
                    body: vec![message],
                    ..Default::default()
                });
                Vec::new()
            }
            TuiInbound::RunStreamEnded => {
                if matches!(self.run_status, RunStatus::Running) {
                    self.run_status = RunStatus::Idle;
                }
                Vec::new()
            }
            TuiInbound::Key(k) => self.apply_key(k),
            TuiInbound::Paste(data) => self.apply_paste(data),
            TuiInbound::Resize { width, height } => {
                self.size = (width, height);
                self.layout.recompute_for_size(width, height);
                // Responsive shrink may hide the panel that currently
                // holds focus. Bring focus back to the composer so the
                // user always has a visible cursor.
                self.clamp_focus();
                Vec::new()
            }
        }
    }

    /// Insert a clipboard / bracketed-paste payload into the composer.
    /// Emits an activity warning when the paste exceeds
    /// [`PASTE_WARN_BYTES`] and truncates when above
    /// [`PASTE_HARD_LIMIT`].
    fn apply_paste(&mut self, mut data: String) -> Vec<TuiAction> {
        // Enforce the paste size cap *first*, before routing the payload into
        // any buffer. The #228 modal routing below previously returned before
        // this logic, so a bracketed paste into Search/CommandPalette could
        // append an arbitrarily large clipboard payload to
        // `search_input`/`palette.input` — which is re-rendered and filtered
        // every frame and can stall the TUI. Capping up front covers the
        // composer and both modals uniformly.
        let original_len = data.len();
        if original_len > PASTE_HARD_LIMIT {
            // Truncate at a char boundary to keep the buffer valid UTF-8.
            let mut cut = PASTE_HARD_LIMIT;
            while cut > 0 && !data.is_char_boundary(cut) {
                cut -= 1;
            }
            data.truncate(cut);
            self.push_activity(format!(
                "paste truncated: {original_len} bytes > {PASTE_HARD_LIMIT} cap"
            ));
        } else if original_len > PASTE_WARN_BYTES {
            self.push_activity(format!(
                "large paste: {original_len} bytes (over {PASTE_WARN_BYTES} warn threshold)"
            ));
        }

        // #228: route bracketed-paste into the active text-input modal's
        // buffer instead of discarding it. The Search and CommandPalette
        // modals accept typed characters, so a paste into them should
        // fill the same buffer the keystrokes do. Strip embedded newlines
        // — these are single-line inputs and a literal `\n` would corrupt
        // the query / filter. Non-text modals leave the paste a no-op.
        if let Some(modal) = self.layout.current_modal() {
            match modal {
                TuiModal::SearchTranscript => {
                    self.search_input
                        .extend(data.chars().filter(|c| *c != '\n' && *c != '\r'));
                    return Vec::new();
                }
                TuiModal::CommandPalette => {
                    if let Some(p) = self.palette.as_mut() {
                        p.input
                            .extend(data.chars().filter(|c| *c != '\n' && *c != '\r'));
                        p.selected = 0;
                    }
                    return Vec::new();
                }
                _ => return Vec::new(),
            }
        }
        // No modal: only the composer accepts pastes. If the focus is on
        // another pane (e.g. the transcript), pasting is a no-op.
        if !matches!(self.layout.focus, FocusPane::Composer) {
            return Vec::new();
        }
        // Insert at the cursor.
        let pos = self.composer.cursor.min(self.composer.buffer.len());
        self.composer.buffer.insert_str(pos, &data);
        self.composer.cursor = pos + data.len();
        self.refresh_composer_popup();
        Vec::new()
    }

    /// Rebuild [`Self::composer_popup`] from the current composer
    /// buffer + cursor. Pure: only the buffer + cursor are inspected.
    /// File completions are intentionally NOT fetched here — the
    /// reducer must not touch the filesystem. The host (`app.rs`)
    /// can populate file candidates later if it wants to support
    /// `@<path>` completions in the live shell; Phase D ships
    /// slash + context-token completions only.
    pub fn refresh_composer_popup(&mut self) {
        let trigger = detect_trigger(&self.composer.buffer, self.composer.cursor);
        self.composer_popup = match trigger {
            Some(t) => {
                let candidates = match t.kind {
                    CompletionKind::Slash => slash_candidates(&t.token, SLASH_COMMANDS, 12),
                    CompletionKind::ContextToken => context_token_candidates(&t.token, 12),
                    // File candidates require the workspace root, which
                    // lives in the host's runtime context; Phase D
                    // leaves these to be populated by the host. We
                    // still surface the popup with an empty list to
                    // signal intent so later phases can plug in real
                    // file completion.
                    CompletionKind::File => Vec::new(),
                };
                if candidates.is_empty() {
                    None
                } else {
                    Some(CompletionPopup::new(
                        t.kind,
                        candidates,
                        t.token_start,
                        t.token_end,
                    ))
                }
            }
            None => None,
        };
    }

    /// Accept the currently-selected completion candidate. Splices the
    /// candidate's `insert_text` into the composer buffer at the
    /// trigger range, moves the cursor to the end of the inserted
    /// text, and closes the popup.
    pub fn accept_completion(&mut self) {
        let Some(popup) = self.composer_popup.take() else {
            return;
        };
        let Some(candidate) = popup.selected_candidate().cloned() else {
            return;
        };
        let start = popup.token_start.min(self.composer.buffer.len());
        let end = popup.token_end.min(self.composer.buffer.len()).max(start);
        self.composer
            .buffer
            .replace_range(start..end, &candidate.insert_text);
        self.composer.cursor = start + candidate.insert_text.len();
    }

    fn apply_key(&mut self, k: KeyEvent) -> Vec<TuiAction> {
        // Completion popup keys take priority over everything except
        // the global modal/layout dispatch — but only when the
        // composer pane is the focused pane. If focus has moved
        // (Tab cycled to transcript), the popup is treated as stale
        // and Tab/Up/Down/Esc fall through to pane-specific behavior.
        if self.composer_popup.is_some()
            && self.layout.current_modal().is_none()
            && matches!(self.layout.focus, FocusPane::Composer)
        {
            match k {
                KeyEvent::Tab => {
                    self.accept_completion();
                    return Vec::new();
                }
                KeyEvent::Up => {
                    if let Some(p) = self.composer_popup.as_mut() {
                        p.prev();
                    }
                    return Vec::new();
                }
                KeyEvent::Down => {
                    if let Some(p) = self.composer_popup.as_mut() {
                        p.next();
                    }
                    return Vec::new();
                }
                KeyEvent::Esc => {
                    self.composer_popup = None;
                    return Vec::new();
                }
                _ => {}
            }
        }
        // Layout-priority keys intercepted before the composer. These
        // are global navigation that does not depend on which pane has
        // focus, so the composer never sees them. Cycling focus away
        // from the composer also closes any stale completion popup
        // so navigation keys in the new pane aren't ambiguous.
        //
        // Phase H: when the command palette is the active modal,
        // Tab is a palette-accept key (matches the composer popup's
        // Tab-to-accept convention). Route modal keys *before* the
        // global Tab cycle for that case so the cycle doesn't fire
        // behind the modal.
        //
        // #27 (Codex P2): the plan-approval notice promises "any key"
        // dismisses it, so Tab/ShiftTab must reach the modal handler
        // (which dismisses) rather than being eaten by the global focus
        // cycle behind the modal. Treat it like the palette: skip the
        // global Tab cycle while that notice is the active modal.
        let modal_owns_tab = match self.layout.current_modal() {
            Some(TuiModal::CommandPalette) => true,
            Some(TuiModal::Custom { id, .. }) => id == PLAN_APPROVAL_MODAL_ID,
            _ => false,
        };
        if !modal_owns_tab {
            match k {
                KeyEvent::Tab => {
                    self.layout.focus = self.layout.focus.next(
                        self.layout.side_panel_rendered(),
                        self.layout.detail_panel_rendered(),
                    );
                    if !matches!(self.layout.focus, FocusPane::Composer) {
                        self.composer_popup = None;
                    }
                    return Vec::new();
                }
                KeyEvent::ShiftTab => {
                    self.layout.focus = self.layout.focus.prev(
                        self.layout.side_panel_rendered(),
                        self.layout.detail_panel_rendered(),
                    );
                    if !matches!(self.layout.focus, FocusPane::Composer) {
                        self.composer_popup = None;
                    }
                    return Vec::new();
                }
                _ => {}
            }
        }
        // When a modal is open, route keys to the modal-specific
        // handler. The SearchTranscript modal accumulates an input
        // buffer; other modals swallow most input. Esc always pops
        // the modal; Ctrl+C always reaches the run-cancel path.
        if let Some(modal) = self.layout.current_modal().cloned() {
            return self.apply_modal_key(modal, k);
        }
        // Transcript-focused navigation. While the transcript pane is
        // focused, the composer is intentionally read-only: typing
        // characters does not modify its buffer, and unmapped keys are
        // swallowed. Only global exits (Ctrl+C / Ctrl+D) escape this
        // mode without first cycling focus back to the composer.
        if matches!(self.layout.focus, FocusPane::Transcript) {
            match k {
                KeyEvent::CtrlC => {
                    return match self.run_status {
                        RunStatus::Running => vec![TuiAction::CancelRun],
                        RunStatus::Idle => vec![TuiAction::Exit],
                    };
                }
                KeyEvent::CtrlD => return vec![TuiAction::Exit],
                _ => {}
            }
            if let Some(actions) = self.apply_transcript_nav_key(k) {
                return actions;
            }
            // Any other key (including unmapped chars and Up/Down when
            // visible_card_indices is empty) is swallowed — the
            // composer never sees it. Without this guard, typing
            // "agent" while focused on the transcript would write "at"
            // into the composer and randomly trigger `g`/`e`/`n`.
            return Vec::new();
        }
        // Phase F: when the side panel is focused on an inspector
        // mode, j/k/Up/Down/g/G drive selection through the inspector
        // rows. Other keys are swallowed so a user navigating the
        // inspector doesn't accidentally type into the composer.
        //
        // Non-inspector side-panel modes (Plan, Changes, Files,
        // Sessions, etc.) intentionally fall through to the composer
        // so users can still type slash commands while the side panel
        // is the focused pane — matches pre-Phase-F behavior.
        if matches!(self.layout.focus, FocusPane::SidePanel)
            && self
                .inspector_list_len(self.layout.side_panel.mode)
                .is_some()
        {
            match k {
                KeyEvent::CtrlC => {
                    return match self.run_status {
                        RunStatus::Running => vec![TuiAction::CancelRun],
                        RunStatus::Idle => vec![TuiAction::Exit],
                    };
                }
                KeyEvent::CtrlD => return vec![TuiAction::Exit],
                _ => {}
            }
            if let Some(actions) = self.apply_inspector_nav_key(k) {
                return actions;
            }
            return Vec::new();
        }
        let action = self.composer.handle_key(k);
        // Anything that may have mutated the composer buffer requires
        // a popup refresh. Submit (which clears the buffer) closes the
        // popup; chars / backspace / paste may open or update it.
        self.refresh_composer_popup();
        match action {
            KeyAction::Submit(prompt) => {
                let trimmed = prompt.trim();
                if trimmed.is_empty() {
                    return Vec::new();
                }
                // `//foo` is the literal-prompt escape: collapse the
                // leading double-slash into a single `/` and submit
                // `/foo` to the runtime. Lets users send absolute
                // paths or anything that starts with `/` without
                // colliding with the slash-command space.
                if let Some(rest) = trimmed.strip_prefix("//") {
                    let with_one_slash = format!("/{rest}");
                    return self.submit_prompt_action(&with_one_slash);
                }
                // Slash commands route into the layout state machine
                // instead of the runtime. Unknown commands are NOT
                // fall-through-submitted: that would turn a typo
                // (`/halp`) into a silently-sent prompt, which is worse
                // than a clear diagnostic.
                if let Some(stripped) = trimmed.strip_prefix('/') {
                    self.apply_slash_command(stripped);
                    return Vec::new();
                }
                self.submit_prompt_action(trimmed)
            }
            KeyAction::Interrupt => match self.run_status {
                RunStatus::Running => vec![TuiAction::CancelRun],
                RunStatus::Idle => vec![TuiAction::Exit],
            },
            KeyAction::Exit => vec![TuiAction::Exit],
            KeyAction::None | KeyAction::HistorySearch => Vec::new(),
        }
    }

    /// Route a key to the topmost modal. Most modals swallow input
    /// other than Esc (close) and Ctrl+C (cancel run). The
    /// SearchTranscript modal additionally accumulates printable
    /// characters into `self.search_input` and commits the query on
    /// Enter.
    fn apply_modal_key(&mut self, modal: TuiModal, k: KeyEvent) -> Vec<TuiAction> {
        match (modal, k) {
            // Phase G: Esc on the tool-approval modal must also
            // resolve the SDK callback with a Deny — silently popping
            // the modal would strand the SDK awaiting a decision that
            // never comes. Match this branch before the generic Esc
            // arm.
            (TuiModal::Custom { id, .. }, KeyEvent::Esc) if id == APPROVAL_MODAL_ID => {
                let _ = self.layout.remove_custom_modals_by_id(APPROVAL_MODAL_ID);
                let decision = ApprovalDecisionAction::Deny {
                    reason: "dismissed without decision".to_string(),
                };
                debug!(
                    target: "code_agent_tui",
                    decision_kind = decision_kind_label(&decision),
                    "approval_decision"
                );
                vec![TuiAction::ResolveApproval(decision)]
            }
            // #27: the plan-approval modal is a dismissible informational
            // notice whose copy promises "Press Esc (or any key) to
            // dismiss". Route *every* key kind (Enter, Tab, arrows,
            // Backspace, Esc, chars, …) to the dismiss handler so the
            // claim is literally true — previously only `Char(_)` was
            // routed here and other decoded keys fell through to the
            // generic no-op arm, doing nothing. Match before the generic
            // Esc arm so Esc still dismisses it via the same path.
            //
            // EXCEPTION: Ctrl+C must NOT be swallowed by this catch-all.
            // It is the global cancel/exit gesture and must keep working
            // even while the notice is open; excluding it here lets it
            // fall through to the `(_, KeyEvent::CtrlC)` arm below, which
            // cancels a running run (or exits when idle). Without this
            // guard the catch-all (placed before the CtrlC arm) would
            // merely dismiss the modal and drop the cancel — a regression.
            (TuiModal::Custom { id, .. }, k)
                if id == PLAN_APPROVAL_MODAL_ID && k != KeyEvent::CtrlC =>
            {
                self.handle_plan_approval_key()
            }
            (closed, KeyEvent::Esc) => {
                let _ = self.layout.pop_modal();
                // #153: only clear the state that belongs to the modal
                // being closed. The old catch-all wiped both
                // `search_input` and `palette` on *any* Esc, so dismissing
                // an unrelated (possibly layered) modal — Help,
                // PromptHistory, a Custom modal — would silently discard a
                // half-typed search query or palette filter underneath it.
                match closed {
                    TuiModal::SearchTranscript => self.search_input.clear(),
                    TuiModal::CommandPalette => self.palette = None,
                    _ => {}
                }
                Vec::new()
            }
            (_, KeyEvent::CtrlC) => match self.run_status {
                RunStatus::Running => vec![TuiAction::CancelRun],
                RunStatus::Idle => Vec::new(),
            },
            (TuiModal::SearchTranscript, KeyEvent::Char(c)) => {
                self.search_input.push(c);
                Vec::new()
            }
            (TuiModal::SearchTranscript, KeyEvent::Backspace) => {
                self.search_input.pop();
                Vec::new()
            }
            (TuiModal::SearchTranscript, KeyEvent::Enter) => {
                let q = self.search_input.trim().to_string();
                self.search_query = if q.is_empty() { None } else { Some(q) };
                self.search_input.clear();
                let _ = self.layout.pop_modal();
                // Search filter changed; recompute the cached visible
                // set eagerly so the per-frame `visible_card_indices`
                // call is cheap even for long transcripts.
                self.refresh_search_cache();
                // Jump selection to the first visible match.
                let first = self
                    .visible_card_indices()
                    .first()
                    .and_then(|i| self.transcript.get(*i).map(|c| c.id));
                if first.is_some() {
                    self.selected_card_id = first;
                }
                Vec::new()
            }
            // M13 Phase E: tool-approval modal keys.
            (TuiModal::Custom { id, .. }, KeyEvent::Char(c)) if id == APPROVAL_MODAL_ID => {
                self.handle_approval_key(c)
            }
            // M13 Phase H: command palette interactivity.
            (TuiModal::CommandPalette, KeyEvent::Char(c)) => {
                if let Some(p) = self.palette.as_mut() {
                    p.input.push(c);
                    p.selected = 0;
                }
                Vec::new()
            }
            (TuiModal::CommandPalette, KeyEvent::Backspace) => {
                if let Some(p) = self.palette.as_mut() {
                    p.input.pop();
                    p.selected = 0;
                }
                Vec::new()
            }
            (TuiModal::CommandPalette, KeyEvent::Up) => {
                if let Some(p) = self.palette.as_mut() {
                    p.selected = p.selected.saturating_sub(1);
                }
                Vec::new()
            }
            (TuiModal::CommandPalette, KeyEvent::Down) => {
                let len = self.palette_candidates().len();
                if let Some(p) = self.palette.as_mut() {
                    if len > 0 {
                        p.selected = (p.selected + 1).min(len - 1);
                    }
                }
                Vec::new()
            }
            // Phase H: both Enter and Tab accept the highlighted
            // palette entry — Tab matches the composer popup's
            // accept-key convention so users have one muscle memory.
            (TuiModal::CommandPalette, KeyEvent::Enter)
            | (TuiModal::CommandPalette, KeyEvent::Tab) => {
                let candidates = self.palette_candidates();
                let picked = self
                    .palette
                    .as_ref()
                    .and_then(|p| candidates.get(p.selected).copied())
                    .map(str::to_string);
                self.close_palette();
                if let Some(cmd) = picked {
                    self.apply_slash_command(&cmd);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Phase H: open the command palette modal *and* seed the
    /// `palette` state in one step. The invariant is "palette modal
    /// open ⇔ `state.palette` is Some" — callers should prefer this
    /// helper over raw `push_modal(CommandPalette)`.
    pub fn open_palette(&mut self) {
        self.palette = Some(PaletteState::default());
        self.layout.push_modal(TuiModal::CommandPalette);
        debug!(target: "code_agent_tui", "palette_open");
    }

    /// Phase H: close the command palette modal *and* clear the
    /// `palette` state. Counterpart to [`Self::open_palette`].
    pub fn close_palette(&mut self) {
        // Remove just the palette modal if it sits on the stack; we
        // intentionally don't pop blindly because another modal may
        // have layered on top.
        self.layout
            .modal_stack
            .retain(|m| !matches!(m, TuiModal::CommandPalette));
        self.palette = None;
    }

    /// Phase H: filter the canonical slash-command list by the
    /// palette's current input substring (case-insensitive).
    /// Returns an empty list when the palette is closed.
    pub fn palette_candidates(&self) -> Vec<&'static str> {
        let input = match &self.palette {
            Some(p) => p.input.trim().to_ascii_lowercase(),
            None => return Vec::new(),
        };
        if input.is_empty() {
            return SLASH_COMMANDS.to_vec();
        }
        SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|c| c.to_ascii_lowercase().contains(&input))
            .collect()
    }

    fn handle_approval_key(&mut self, c: char) -> Vec<TuiAction> {
        let decision = match c.to_ascii_lowercase() {
            'y' => ApprovalDecisionAction::Allow,
            's' => ApprovalDecisionAction::AllowForSession,
            'n' => ApprovalDecisionAction::Deny {
                reason: "denied by user".to_string(),
            },
            // Phase G: `r` (request revision) was listed in the
            // Phase E modal body, but the SDK has no first-class
            // revision variant. Until that lands, `r` maps to a
            // descriptive Deny rather than promising a flow we can't
            // honor. The modal copy was also dropped to avoid
            // misleading users.
            'r' => ApprovalDecisionAction::Deny {
                reason: "user requested revision".to_string(),
            },
            _ => return Vec::new(),
        };
        // Pop the modal so the user gets immediate visual feedback.
        // Crucially, we do NOT clear `pending_approval` here — that's
        // the job of `SdkEvent::ApprovalResolved`. Clearing early
        // would flash the empty-approval placeholder while the SDK is
        // still processing the decision, and could strand the user if
        // the callback round-trips slowly.
        let _ = self.layout.remove_custom_modals_by_id(APPROVAL_MODAL_ID);
        // Phase I review fix: log only the kind (allow / deny /
        // session-allow / revision) so an attacker-controlled or
        // surprising Deny reason can't leak into logs.
        debug!(
            target: "code_agent_tui",
            decision_kind = decision_kind_label(&decision),
            "approval_decision"
        );
        vec![TuiAction::ResolveApproval(decision)]
    }

    fn handle_plan_approval_key(&mut self) -> Vec<TuiAction> {
        // #27: the plan-approval modal is an INFORMATIONAL notice only.
        // The SDK auto-approves plans internally and exposes no
        // host-callable decision surface yet, so any key that claimed to
        // approve/reject/revise would lie — the run proceeds regardless.
        // We therefore bind no decision keys: ANY key (Esc, Enter, Tab,
        // arrows, chars, …) simply dismisses the notice — matching the
        // "Press Esc (or any key) to dismiss" copy — and NO decision
        // action is emitted. Wiring a real PlanDecisionCallback that can
        // actually gate execution is the tracked follow-up (issue #27
        // option 1).
        let _ = self
            .layout
            .remove_custom_modals_by_id(PLAN_APPROVAL_MODAL_ID);
        Vec::new()
    }

    /// Transcript-focused navigation. Returns `Some(actions)` when the
    /// key was handled (so the caller should NOT also forward it to
    /// the composer), `None` otherwise.
    fn apply_transcript_nav_key(&mut self, k: KeyEvent) -> Option<Vec<TuiAction>> {
        let visible = self.visible_card_indices();
        if visible.is_empty() {
            return None;
        }
        let current = self
            .selected_card_id
            .and_then(|id| visible.iter().position(|i| self.transcript[*i].id == id))
            .unwrap_or_else(|| visible.len() - 1);
        match k {
            KeyEvent::Char('j') | KeyEvent::Down => {
                let new = (current + 1).min(visible.len() - 1);
                self.selected_card_id = Some(self.transcript[visible[new]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('k') | KeyEvent::Up => {
                let new = current.saturating_sub(1);
                self.selected_card_id = Some(self.transcript[visible[new]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('g') => {
                self.selected_card_id = Some(self.transcript[visible[0]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('G') => {
                self.selected_card_id = Some(self.transcript[visible[visible.len() - 1]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('e') | KeyEvent::Enter => {
                // Resolve to the currently-visible cursor in case
                // `selected_card_id` points at a card that aged out of
                // the ring buffer. Toggling a vanished id silently
                // mutates state that's not rendered.
                let cursor_id = self.transcript[visible[current]].id;
                self.selected_card_id = Some(cursor_id);
                if self.expanded_card_ids.contains(&cursor_id) {
                    self.expanded_card_ids.remove(&cursor_id);
                } else {
                    self.expanded_card_ids.insert(cursor_id);
                }
                Some(Vec::new())
            }
            KeyEvent::Char('n') => {
                // Next search match. Without an active query this is a
                // no-op (the visible set is everything).
                if self.search_query.is_some() {
                    let new = (current + 1).min(visible.len() - 1);
                    self.selected_card_id = Some(self.transcript[visible[new]].id);
                }
                Some(Vec::new())
            }
            KeyEvent::Char('N') => {
                if self.search_query.is_some() {
                    let new = current.saturating_sub(1);
                    self.selected_card_id = Some(self.transcript[visible[new]].id);
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Phase F: side-panel inspector navigation. Returns `Some(actions)`
    /// when the key was handled (so the caller should NOT also forward
    /// it to the composer), `None` otherwise. Only fires while
    /// `FocusPane::SidePanel` is the focus and the panel mode is one
    /// of the inspector modes.
    fn apply_inspector_nav_key(&mut self, k: KeyEvent) -> Option<Vec<TuiAction>> {
        let mode = self.layout.side_panel.mode;
        let len = self.inspector_list_len(mode)?;
        if len == 0 {
            // No rows to navigate. Swallow nav keys silently so they
            // don't fall through to a composer write.
            return matches!(
                k,
                KeyEvent::Char('j')
                    | KeyEvent::Char('k')
                    | KeyEvent::Char('g')
                    | KeyEvent::Char('G')
                    | KeyEvent::Up
                    | KeyEvent::Down
            )
            .then(Vec::new);
        }
        match k {
            KeyEvent::Char('j') | KeyEvent::Down => {
                self.move_inspector_selection(mode, 1);
                Some(Vec::new())
            }
            KeyEvent::Char('k') | KeyEvent::Up => {
                self.move_inspector_selection(mode, -1);
                Some(Vec::new())
            }
            KeyEvent::Char('g') => {
                self.set_inspector_selection(mode, Some(0));
                Some(Vec::new())
            }
            KeyEvent::Char('G') => {
                self.set_inspector_selection(mode, Some(len - 1));
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Common path for submitting a literal prompt (used both by
    /// vanilla input and the `//` escape). Enforces the
    /// "no submit while run is in progress" guard.
    fn submit_prompt_action(&mut self, prompt: &str) -> Vec<TuiAction> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Vec::new();
        }
        if matches!(self.run_status, RunStatus::Running) {
            self.push_activity("ignored prompt: run already in progress".to_string());
            return Vec::new();
        }
        let prompt = prompt.to_string();
        self.push_transcript(EventCard {
            kind: "user",
            headline: "You:".to_string(),
            body: prompt.lines().map(String::from).collect(),
            ..Default::default()
        });
        self.run_status = RunStatus::Running;
        self.last_error = None;
        debug!(target: "code_agent_tui", prompt_bytes = prompt.len(), "submit_prompt");
        vec![TuiAction::SubmitPrompt(prompt)]
    }

    fn apply_slash_command(&mut self, cmd: &str) {
        // Allow `/cmd args...` by splitting off the first whitespace
        // run. Phase B doesn't act on args yet; Phase H will use them
        // for palette / search inputs.
        let (cmd_name, _args) = cmd
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((cmd.trim(), ""));
        // Side-panel switches. Accept any of the 14 canonical config
        // keys plus `agents` (alias for `subagents`) — kept in sync
        // with `SidePanelMode::from_config_key`.
        if let Some(mode) = SidePanelMode::from_config_key(cmd_name) {
            self.layout.side_panel.mode = mode;
            self.layout.side_panel.visible = true;
            // Only move focus to the side panel if it will actually be
            // rendered: in Compact / Tiny layouts the panel is hidden,
            // and stranding focus on an invisible pane creates a
            // no-visible-focus state.
            if self.layout.side_panel_rendered() {
                self.layout.focus = FocusPane::SidePanel;
            }
            self.push_activity(format!("side panel → {}", mode.label()));
            // M13 Phase I: telemetry for panel switches. Stays at
            // `debug` so a default `RUST_LOG=info` keeps the TUI
            // silent unless the operator opts in.
            debug!(target: "code_agent_tui", panel = %mode.label(), "side_panel_switch");
            self.clamp_focus();
            return;
        }
        match cmd_name {
            "help" | "keys" => {
                self.layout.push_modal(TuiModal::Help);
            }
            "palette" => {
                self.open_palette();
            }
            "search" => {
                self.layout.push_modal(TuiModal::SearchTranscript);
            }
            "history" => {
                self.layout.push_modal(TuiModal::PromptHistory);
            }
            "toggle-side" | "side" => {
                self.layout.side_panel.visible = !self.layout.side_panel.visible;
            }
            "toggle-detail" | "detail" => {
                self.layout.detail_panel.visible = !self.layout.detail_panel.visible;
            }
            "" => {
                // Bare `/` — no-op, harmless.
            }
            other => {
                self.push_activity(format!("unknown slash command: /{other}"));
            }
        }
        // Whatever changed above, make sure focus still points at a
        // pane that is rendered. Otherwise the user has no visible
        // cursor until they press Tab.
        self.clamp_focus();
    }

    /// If `layout.focus` points at a pane that is not currently
    /// rendered (because it was hidden or because responsive mode
    /// suppresses it), reset focus to the composer. Cheap to call
    /// after any panel-state or layout-mode mutation.
    fn clamp_focus(&mut self) {
        let side_ok = self.layout.side_panel_rendered();
        let detail_ok = self.layout.detail_panel_rendered();
        match self.layout.focus {
            FocusPane::SidePanel if !side_ok => {
                self.layout.focus = FocusPane::Composer;
            }
            FocusPane::DetailPanel if !detail_ok => {
                self.layout.focus = FocusPane::Composer;
            }
            _ => {}
        }
    }

    /// Public helper: directly set the side-panel mode (e.g. from a
    /// command palette entry in a future phase, or from CLI args).
    pub fn set_side_panel(&mut self, mode: SidePanelMode, visible: bool) {
        self.layout.side_panel.mode = mode;
        self.layout.side_panel.visible = visible;
    }

    /// Public helper: directly set the detail-panel mode.
    pub fn set_detail_panel(&mut self, mode: DetailPanelMode, visible: bool) {
        self.layout.detail_panel.mode = mode;
        self.layout.detail_panel.visible = visible;
    }

    /// Public helper: open a modal explicitly.
    pub fn push_modal(&mut self, modal: TuiModal) {
        self.layout.push_modal(modal);
    }

    /// Public helper: close the topmost modal.
    pub fn pop_modal(&mut self) -> Option<TuiModal> {
        self.layout.pop_modal()
    }

    fn apply_sdk_event(&mut self, ev: &SdkEvent) {
        let outcome = map_sdk_event(ev);
        if let Some(delta) = outcome.coalesce_assistant_delta {
            self.coalesce_assistant_delta(&delta);
        }
        if let Some(card) = outcome.card {
            self.push_transcript(card);
        }
        if let Some(label) = outcome.activity {
            self.push_activity(label);
        }
        if let Some(err) = outcome.error {
            self.last_error = Some(err);
        }
        if let Some(success) = outcome.run_terminal {
            self.run_status = RunStatus::Idle;
            self.status_ctx
                .set("run_status", if success { "ok" } else { "failed" });
        } else if matches!(ev, SdkEvent::RunStarted(_)) {
            self.run_status = RunStatus::Running;
            self.status_ctx.set("run_status", "running");
        }
        // M13 Phase E: pull plan / approval / patch payloads out of
        // the SDK event stream into reducer state so the workbench
        // panels can render them.
        self.apply_phase_e_workbench_event(ev);
    }

    fn apply_phase_e_workbench_event(&mut self, ev: &SdkEvent) {
        match ev {
            SdkEvent::PlanCreated(p) => {
                self.current_plan = Some(p.plan.clone());
                // #27: route the plan dashboard into view and surface a
                // purely INFORMATIONAL notice. The SDK auto-approves the
                // plan internally and exposes no host-callable decision
                // surface, so the TUI cannot block, reject, or revise a
                // plan yet. The notice therefore advertises NO
                // approve/reject/revise keys — earlier copy showed
                // `[a] approve* [r] reject* [u] revise*`, which implied
                // the keystrokes gated execution even though the handlers
                // were no-ops and the run proceeded regardless. The notice
                // is dismissed with Esc (or any key). Each body line is
                // kept under 80 columns so the modal renders cleanly on a
                // default terminal.
                //
                // TODO(#27): wire a real PlanDecisionCallback through
                // AgentRuntime (issue #27 option 1) so an operator's
                // approve/reject/revise can actually gate execution. That
                // change spans sdk + core and is intentionally out of scope
                // for this crate-exclusive fix; until it lands the modal
                // must stay informational and bind no decision keys.
                self.layout.side_panel.mode = SidePanelMode::Plan;
                self.layout.side_panel.visible = true;
                self.layout.push_modal(TuiModal::Custom {
                    id: PLAN_APPROVAL_MODAL_ID.to_string(),
                    title: "Plan created (informational)".to_string(),
                    body: vec![
                        format!("Plan: {}", p.plan.title.clone()),
                        String::new(),
                        "  The plan was created and the run will proceed.".to_string(),
                        "  Approval gating is not yet wired to the TUI, so".to_string(),
                        "  this plan cannot be blocked or revised from here.".to_string(),
                        String::new(),
                        "  Press Esc (or any key) to dismiss this notice.".to_string(),
                    ],
                });
            }
            SdkEvent::PlanApproved(_) => {
                // Remove the plan-approval modal wherever it sits in
                // the stack — a layered approval modal could have been
                // pushed on top in the meantime, and a top-only pop
                // would strand the plan-approval modal indefinitely.
                let _ = self
                    .layout
                    .remove_custom_modals_by_id(PLAN_APPROVAL_MODAL_ID);
            }
            SdkEvent::ApprovalRequired(a) => {
                // #177: the approval modal can be opened from two
                // uncoordinated paths — the live bridge handler
                // (`recv_bridge` in app.rs, which opens the modal AND
                // stashes the SDK resolver) and this reducer arm (which
                // also serves reducer-only hosts/tests that have no
                // bridge). When a bridge is wired, both fire for the same
                // request and we would push a second, resolver-less modal
                // on top of the bridge's. Guard against that: if an
                // approval modal is already on the stack, only refresh the
                // activity log and leave the existing modal (with its live
                // resolver) untouched. The bridge is the single source of
                // truth when present; this arm stays self-sufficient when
                // it is not.
                let already_open =
                    self.layout.modal_stack.iter().any(
                        |m| matches!(m, TuiModal::Custom { id, .. } if id == APPROVAL_MODAL_ID),
                    );
                if already_open {
                    self.push_activity(format!(
                        "approval required: {} (modal already open)",
                        a.tool_name
                    ));
                } else {
                    self.open_approval_modal(a.tool_name.clone(), a.summary.clone());
                }
            }
            SdkEvent::ApprovalResolved(_) => {
                self.pending_approval = None;
                let _ = self.layout.remove_custom_modals_by_id(APPROVAL_MODAL_ID);
                // #80: restore the detail panel to whatever it showed
                // before the approval overrode it (e.g. a diff), instead
                // of leaving it pinned to `Approval` with empty content.
                // If the panel is no longer in Approval mode the user
                // navigated away, so leave their choice intact.
                if let Some(prior) = self.prior_detail_panel.take() {
                    if self.layout.detail_panel.mode == DetailPanelMode::Approval {
                        self.layout.detail_panel = prior;
                    }
                }
            }
            SdkEvent::PatchProposed(p) => {
                self.last_patch = Some(p.patch.clone());
                self.layout.detail_panel.mode = DetailPanelMode::Diff;
                self.layout.detail_panel.visible = true;
            }
            // M13 Phase G: pull diagnostics into the bounded ring
            // buffer that backs the Diagnostics side panel. The
            // event-adapter mirror still produces an EventCard for
            // the main transcript so this is purely additive.
            SdkEvent::Diagnostic(d) => {
                self.push_diagnostic(DiagnosticEntry {
                    severity: diagnostic_severity_label(d.diagnostic.severity).to_string(),
                    code: d.diagnostic.code.clone(),
                    message: d.diagnostic.message.clone(),
                    suggestion: d.diagnostic.suggestion.clone(),
                });
            }
            _ => {}
        }
    }

    fn coalesce_assistant_delta(&mut self, delta: &str) {
        // Streaming deltas are coalesced into the most recent assistant
        // card so the transcript reflects a single message rather than a
        // wall of fragments. Embedded newlines must split into new body
        // entries — otherwise a single body line could contain a literal
        // `\n` and the transcript pane would silently lose the break.
        // If the last card isn't an assistant card, start a fresh one.
        let mut pieces = delta.split('\n');
        let first = pieces.next().unwrap_or("");
        let rest: Vec<&str> = pieces.collect();
        if let Some(last) = self.transcript.last_mut() {
            if last.kind == "assistant" || last.kind == "assistant_delta" {
                if last.body.is_empty() {
                    last.body.push(first.to_string());
                } else {
                    let n = last.body.len();
                    last.body[n - 1].push_str(first);
                }
                for line in rest {
                    last.body.push(line.to_string());
                }
                return;
            }
        }
        let mut body: Vec<String> = Vec::with_capacity(1 + rest.len());
        body.push(first.to_string());
        for line in rest {
            body.push(line.to_string());
        }
        self.push_transcript(EventCard {
            kind: "assistant_delta",
            headline: "Agent:".to_string(),
            body,
            ..Default::default()
        });
    }

    fn push_transcript(&mut self, mut card: EventCard) {
        // Stamp the card with a stable id before it enters state. The
        // id is used by selection / expansion / search-result sets so
        // they survive the transcript ring buffer's eviction of older
        // cards.
        card.id = self.next_card_id;
        self.next_card_id = self.next_card_id.wrapping_add(1);
        self.transcript.push(card);
        // Bounded ring buffer; oldest cards drop first. As we evict,
        // drop their ids from the expansion set and clear the
        // selection cursor if it points at an evicted card. Without
        // this, `expanded_card_ids` would grow unboundedly across a
        // long session.
        if self.transcript.len() > TRANSCRIPT_CAP {
            let excess = self.transcript.len() - TRANSCRIPT_CAP;
            for evicted in self.transcript.drain(..excess) {
                self.expanded_card_ids.remove(&evicted.id);
                if self.selected_card_id == Some(evicted.id) {
                    self.selected_card_id = None;
                }
            }
        }
        // Search results may have changed; invalidate cache.
        self.invalidate_search_cache();
    }

    /// Append a one-line entry to the bounded activity log. The log
    /// is shown in the bottom-status area of the renderer. Public so
    /// the host loop (`app.rs`) can record action-handling
    /// diagnostics (Phase E: approval / plan decisions).
    pub fn push_activity(&mut self, label: String) {
        self.activity.push(label);
        if self.activity.len() > ACTIVITY_CAP {
            let excess = self.activity.len() - ACTIVITY_CAP;
            self.activity.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::ComposerConfig;
    use code_agent_sdk::event::{
        AssistantTextDelta, RunCompletedEvent, RunFailedEvent, RunStarted, ToolCallStartedEvent,
    };
    use code_agent_sdk::request::RequestMetadata;
    use code_agent_sdk::result::{AgentRunStatus, EventSummary};
    use code_agent_sdk::{ProviderId, RunId};

    fn fresh_state() -> TuiAppState {
        TuiAppState::new(
            ComposerState::new(ComposerConfig::default()),
            "{project}".to_string(),
            StatusLineContext::default(),
        )
    }

    fn rid() -> RunId {
        RunId::new()
    }

    #[test]
    // Typing characters then pressing Enter emits a SubmitPrompt action and
    // marks the run as in-progress, with a "You:" card in the transcript.
    fn submit_prompt_emits_action_and_run_started() {
        let mut s = fresh_state();
        for c in "hi".chars() {
            assert!(s.apply(TuiInbound::Key(KeyEvent::Char(c))).is_empty());
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(actions, vec![TuiAction::SubmitPrompt("hi".to_string())]);
        assert_eq!(s.run_status, RunStatus::Running);
        assert_eq!(s.transcript.last().unwrap().kind, "user");
    }

    #[test]
    // RunStarted updates run_status and pushes a "run started" card.
    fn run_started_updates_state() {
        let mut s = fresh_state();
        let ev = SdkEvent::RunStarted(RunStarted {
            run_id: rid(),
            session_id: None,
            provider: ProviderId::Fake,
            model: "fake-model".to_string(),
            metadata: RequestMetadata::default(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Running);
        assert!(s.transcript.iter().any(|c| c.kind == "run_started"));
    }

    #[test]
    // RunCompleted moves run_status back to Idle and clears nothing else.
    fn run_completed_moves_to_idle() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        let ev = SdkEvent::RunCompleted(RunCompletedEvent {
            run_id: rid(),
            status: AgentRunStatus::Completed,
            summary: "done".to_string(),
            usage: None,
            events_summary: EventSummary::default(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert!(s.last_error.is_none());
    }

    #[test]
    // RunFailed records the error message and returns to Idle.
    fn run_failed_records_error_and_idles() {
        let mut s = fresh_state();
        let ev = SdkEvent::RunFailed(RunFailedEvent {
            run_id: rid(),
            status: AgentRunStatus::Failed,
            message: "boom".to_string(),
            diagnostics: Vec::new(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert_eq!(s.last_error.as_deref(), Some("boom"));
    }

    #[test]
    // Successive AssistantTextDelta events accumulate into a single card.
    fn assistant_deltas_coalesce() {
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "hel".to_string(),
            },
        )));
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "lo".to_string(),
            },
        )));
        let last = s.transcript.last().unwrap();
        assert_eq!(last.kind, "assistant_delta");
        assert_eq!(last.body, vec!["hello".to_string()]);
    }

    #[test]
    // Tool calls show up in the activity log; capacity is enforced.
    fn tool_call_pushes_activity_with_cap() {
        let mut s = fresh_state();
        for i in 0..(ACTIVITY_CAP + 4) {
            s.apply(TuiInbound::sdk(SdkEvent::ToolCallStarted(
                ToolCallStartedEvent {
                    run_id: rid(),
                    tool_name: format!("tool_{i}"),
                    tool_use_id: i.to_string(),
                    input: serde_json::Value::Null,
                },
            )));
        }
        assert_eq!(s.activity.len(), ACTIVITY_CAP);
        assert!(s
            .activity
            .last()
            .unwrap()
            .contains(&format!("tool_{}", ACTIVITY_CAP + 3)));
    }

    #[test]
    // Ctrl+C while idle exits; Ctrl+C while running cancels.
    fn ctrl_c_exits_or_cancels() {
        let mut s = fresh_state();
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::Exit]
        );
        s.run_status = RunStatus::Running;
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::CancelRun]
        );
    }

    #[test]
    // RunStreamError records the message, idles the run, and adds a card.
    fn stream_error_records_and_idles() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        s.apply(TuiInbound::RunStreamError("disconnected".to_string()));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert_eq!(s.last_error.as_deref(), Some("disconnected"));
        assert!(s.transcript.last().unwrap().kind == "run_failed");
    }

    #[test]
    // Submitting a prompt while a run is already in progress is rejected
    // before any state mutation, so no orphan "You:" card is left for a
    // prompt that never runs. The user gets an activity entry instead.
    fn submit_while_running_is_rejected() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        for c in "second".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert!(s.transcript.iter().all(|c| c.kind != "user"));
        assert!(s
            .activity
            .iter()
            .any(|a| a.contains("run already in progress")));
    }

    #[test]
    // AssistantTextDelta events that contain a newline split into a new
    // body line so the transcript pane never silently swallows a `\n`.
    fn delta_with_newline_splits_into_lines() {
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "first\nsecond".to_string(),
            },
        )));
        let last = s.transcript.last().unwrap();
        assert_eq!(last.kind, "assistant_delta");
        assert_eq!(
            last.body,
            vec!["first".to_string(), "second".to_string()],
            "delta with `\\n` should split into separate body lines"
        );
    }

    #[test]
    // Resize updates the cached terminal size.
    fn resize_updates_size() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 132,
            height: 50,
        });
        assert_eq!(s.size, (132, 50));
    }

    #[test]
    // Tab cycles focus forward through visible panes; Shift+Tab cycles
    // backward. The composer never sees these keys.
    fn tab_and_shift_tab_cycle_focus() {
        let mut s = fresh_state();
        s.layout.side_panel.visible = true;
        s.layout.detail_panel.visible = true;
        // Recompute mode for a wide-enough size so panels are rendered.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        assert_eq!(s.layout.focus, FocusPane::Composer);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Transcript);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::DetailPanel);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Composer);
        s.apply(TuiInbound::Key(KeyEvent::ShiftTab));
        assert_eq!(s.layout.focus, FocusPane::DetailPanel);
    }

    #[test]
    // Esc with a modal on top of the stack pops it; without a modal,
    // Esc is a no-op (the composer ignores Esc by design).
    fn esc_pops_modal_when_open() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        assert!(s.layout.current_modal().is_some());
        let actions = s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(actions.is_empty());
        assert!(s.layout.current_modal().is_none());
        // Without a modal Esc is harmless.
        let _ = s.apply(TuiInbound::Key(KeyEvent::Esc));
    }

    #[test]
    // A `/files` slash command switches the side panel mode and never
    // submits a prompt to the runtime. On a Full-sized terminal the
    // side panel is rendered, so focus moves to it.
    fn slash_command_routes_panel_switch() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        for c in "/files".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty(), "slash must not emit SubmitPrompt");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
        assert!(s.layout.side_panel.visible);
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
    }

    #[test]
    // /help opens the help modal.
    fn slash_help_opens_help_modal() {
        let mut s = fresh_state();
        for c in "/help".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(matches!(s.layout.current_modal(), Some(TuiModal::Help)));
    }

    #[test]
    // Unknown slash commands surface as an activity entry but never
    // submit a prompt.
    fn unknown_slash_is_diagnostic_not_prompt() {
        let mut s = fresh_state();
        for c in "/nope".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert!(s
            .activity
            .iter()
            .any(|a| a.contains("unknown slash command")));
    }

    #[test]
    // Resize recomputes the layout mode but preserves the user's
    // panel selections.
    fn resize_recomputes_layout_mode() {
        let mut s = fresh_state();
        s.layout.side_panel.mode = SidePanelMode::Changes;
        s.apply(TuiInbound::Resize {
            width: 40,
            height: 6,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Tiny);
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Changes);
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Full);
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Changes);
    }

    #[test]
    // A `//path/...` prompt is the literal-prompt escape: the leading
    // pair of slashes is collapsed to one and submitted unchanged.
    fn double_slash_escapes_to_prompt() {
        let mut s = fresh_state();
        for c in "//var/log/syslog is broken".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(
            actions,
            vec![TuiAction::SubmitPrompt(
                "/var/log/syslog is broken".to_string()
            )]
        );
    }

    #[test]
    // Slash commands accept trailing arguments (Phase H will use them).
    // The command name still resolves and the activity log records it.
    fn slash_command_accepts_arguments() {
        let mut s = fresh_state();
        for c in "/files src/main.rs".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
    }

    #[test]
    // While a modal is open, the composer must not see Enter / Char
    // events. Esc still pops the modal; Ctrl+C cancels a running run.
    fn modal_blocks_input_to_composer() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        for c in "this should be ignored".chars() {
            let actions = s.apply(TuiInbound::Key(KeyEvent::Char(c)));
            assert!(actions.is_empty());
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty(), "Enter must not submit while modal open");
        assert!(
            s.composer.buffer.is_empty(),
            "composer should not have absorbed keys"
        );
        // Esc still pops the modal.
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.layout.current_modal().is_none());
    }

    #[test]
    // Ctrl+C with a modal open and a run in flight still cancels the
    // run rather than getting swallowed.
    fn ctrl_c_through_modal_cancels_run() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        s.layout.push_modal(TuiModal::Help);
        let actions = s.apply(TuiInbound::Key(KeyEvent::CtrlC));
        assert_eq!(actions, vec![TuiAction::CancelRun]);
    }

    #[test]
    // Toggling a panel off while it has focus snaps focus back to the
    // composer so the user always has a visible cursor.
    fn hide_panel_clamps_focus() {
        let mut s = fresh_state();
        // Reach Full mode so panel is rendered.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus the side panel.
        s.apply(TuiInbound::Key(KeyEvent::Tab)); // Composer → Transcript
        s.apply(TuiInbound::Key(KeyEvent::Tab)); // Transcript → SidePanel
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // /toggle-side hides it.
        for c in "/toggle-side".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(!s.layout.side_panel.visible);
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    #[test]
    // On a Compact terminal where the side panel is not rendered,
    // `/files` keeps focus on the composer because moving it to an
    // invisible pane would leave the user with no visible cursor.
    fn slash_switch_on_compact_keeps_focus_on_composer() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 70,
            height: 16,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Compact);
        for c in "/files".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
        // Mode is still Compact → side panel not rendered → focus
        // stays on the composer.
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    #[test]
    // Resizing down to Tiny while focus is on a panel clamps focus.
    fn resize_to_tiny_clamps_focus() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        s.apply(TuiInbound::Resize {
            width: 40,
            height: 6,
        });
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    fn push_user_card(s: &mut TuiAppState, text: &str) {
        s.push_transcript(EventCard {
            kind: "user",
            headline: text.to_string(),
            body: vec![text.to_string()],
            ..Default::default()
        });
    }

    #[test]
    // Each push_transcript assigns a fresh monotonic id; cards retain
    // their ids through subsequent pushes and eviction.
    fn push_transcript_assigns_monotonic_ids() {
        let mut s = fresh_state();
        push_user_card(&mut s, "one");
        push_user_card(&mut s, "two");
        push_user_card(&mut s, "three");
        let ids: Vec<u64> = s.transcript.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(s.next_card_id, 3);
    }

    #[test]
    // With transcript focused, j/k navigate selection through visible
    // cards and saturate at the ends.
    fn transcript_nav_j_k_moves_selection() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        push_user_card(&mut s, "a");
        push_user_card(&mut s, "b");
        push_user_card(&mut s, "c");
        // First j with no selection: starts from last card (most recent).
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert_eq!(s.selected_card_id, Some(2));
        // k moves up to b.
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(1));
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(0));
        // Saturates at top.
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(0));
        // g/G jump to top/bottom.
        s.apply(TuiInbound::Key(KeyEvent::Char('G')));
        assert_eq!(s.selected_card_id, Some(2));
        s.apply(TuiInbound::Key(KeyEvent::Char('g')));
        assert_eq!(s.selected_card_id, Some(0));
    }

    #[test]
    // Transcript navigation keys are ignored when focus is on the
    // composer (default). Typing `j` adds it to the composer buffer.
    fn transcript_nav_inert_when_composer_focused() {
        let mut s = fresh_state();
        push_user_card(&mut s, "a");
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        // Composer absorbed the keystroke.
        assert_eq!(s.composer.buffer, "j");
        assert!(s.selected_card_id.is_none());
    }

    #[test]
    // `e` and Enter toggle expansion of the selected card; cards
    // without expanded_body still record the bit so the renderer
    // can decide what to do.
    fn e_toggles_expansion() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        push_user_card(&mut s, "a");
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert!(!s.is_card_expanded(0));
        s.apply(TuiInbound::Key(KeyEvent::Char('e')));
        assert!(s.is_card_expanded(0));
        // Enter also toggles when transcript-focused.
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(!s.is_card_expanded(0));
    }

    #[test]
    // `/search foo` opens the SearchTranscript modal; typing `foo` +
    // Enter commits the query. Visible card list shrinks to matches.
    fn search_modal_filters_transcript() {
        let mut s = fresh_state();
        push_user_card(&mut s, "alpha");
        push_user_card(&mut s, "beta");
        push_user_card(&mut s, "alphabet");
        // Open the modal via slash command.
        for c in "/search".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::SearchTranscript)
        ));
        // Type query.
        for c in "alpha".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert_eq!(s.search_input, "alpha");
        // Commit.
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(s.search_query.as_deref(), Some("alpha"));
        assert!(s.layout.current_modal().is_none());
        let visible = s.visible_card_indices();
        // alpha + alphabet match; beta does not.
        assert_eq!(visible.len(), 2);
        // Selection jumps to first match.
        assert!(s.selected_card_id.is_some());
    }

    #[test]
    // Backspace removes the last char from the search input; Esc
    // cancels the modal and clears the input without committing.
    fn search_modal_backspace_and_cancel() {
        let mut s = fresh_state();
        for c in "/search".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        for c in "wrong".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Backspace));
        s.apply(TuiInbound::Key(KeyEvent::Backspace));
        assert_eq!(s.search_input, "wro");
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.layout.current_modal().is_none());
        assert!(s.search_input.is_empty());
        assert!(s.search_query.is_none());
    }

    #[test]
    // n / N navigate forward/backward through search matches when
    // transcript is focused. Without an active query they're no-ops.
    fn n_and_capital_n_navigate_matches() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "alpha");
        push_user_card(&mut s, "beta");
        push_user_card(&mut s, "alphabet");
        s.search_query = Some("alpha".to_string());
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // First n from no selection → most recent match (alphabet, id 2).
        s.apply(TuiInbound::Key(KeyEvent::Char('n')));
        assert_eq!(s.selected_card_id, Some(2));
        s.apply(TuiInbound::Key(KeyEvent::Char('N')));
        assert_eq!(s.selected_card_id, Some(0));
    }

    #[test]
    // Typing characters while the transcript pane is focused is
    // swallowed — they must not leak into the composer buffer.
    fn transcript_focused_swallows_unmapped_chars() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "anchor");
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Transcript);
        for c in "agent".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert!(
            s.composer.buffer.is_empty(),
            "composer absorbed transcript-focused keystrokes: {:?}",
            s.composer.buffer
        );
    }

    #[test]
    // With the transcript pane focused and an empty visible set
    // (e.g. zero matches), Up/Down still must not leak to composer.
    fn transcript_focused_empty_visible_swallows_arrows() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus transcript with nothing in it.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Down));
        s.apply(TuiInbound::Key(KeyEvent::Up));
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert!(s.composer.buffer.is_empty());
    }

    #[test]
    // Ctrl+C and Ctrl+D still escape from transcript focus.
    fn transcript_focused_ctrl_c_and_d_still_global() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // Ctrl+D from transcript focus with empty composer exits.
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlD)),
            vec![TuiAction::Exit]
        );
        // Ctrl+C with a running run cancels rather than exiting.
        s.run_status = RunStatus::Running;
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::CancelRun]
        );
    }

    #[test]
    // Evicting old cards drops their ids from expanded_card_ids and
    // clears the selection cursor if it pointed at one of them.
    fn ring_buffer_eviction_cleans_orphan_ids() {
        let mut s = fresh_state();
        // Fill the transcript right up to its cap and expand the
        // first three cards.
        for i in 0..TRANSCRIPT_CAP {
            push_user_card(&mut s, &format!("c{i}"));
            if i < 3 {
                s.expanded_card_ids.insert(i as u64);
            }
        }
        // Point selection at the card that is about to be evicted.
        s.selected_card_id = Some(0);
        // Push one more card — id 0 is now drained.
        push_user_card(&mut s, "tail");
        assert!(!s.expanded_card_ids.contains(&0));
        assert!(s.selected_card_id.is_none());
        // Push two more to drain ids 1 and 2 → their expansion bits
        // also clear.
        push_user_card(&mut s, "tail2");
        push_user_card(&mut s, "tail3");
        assert!(!s.expanded_card_ids.contains(&1));
        assert!(!s.expanded_card_ids.contains(&2));
    }

    #[test]
    // Pressing `e` while selection points at an evicted card
    // resolves to the visible cursor and toggles its expansion
    // instead.
    fn expand_resolves_evicted_selection_to_cursor() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "a");
        push_user_card(&mut s, "b");
        // Pretend selection points at a card that no longer exists.
        s.selected_card_id = Some(99);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Char('e')));
        // Selection now points at the visible cursor (most recent),
        // and that card is expanded.
        let new_selected = s.selected_card_id.unwrap();
        assert!(s.is_card_expanded(new_selected));
        // The vanished id was not toggled.
        assert!(!s.is_card_expanded(99));
    }

    #[test]
    // Typing `/pl` opens a Slash popup with `plan` / `plugins` / etc.
    // among the candidates. Tab accepts the highlighted candidate
    // and replaces the trigger range in the composer buffer.
    fn slash_popup_opens_and_accepts() {
        let mut s = fresh_state();
        for c in "/pl".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let popup = s.composer_popup.as_ref().expect("popup should be open");
        assert_eq!(popup.kind, crate::completion::CompletionKind::Slash);
        assert!(!popup.candidates.is_empty(), "expected candidates");
        // Tab accepts. The first candidate (`/plan`) replaces the
        // trigger range; the popup closes.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert!(s.composer_popup.is_none(), "popup should close on accept");
        assert!(
            s.composer.buffer.starts_with("/p"),
            "buffer should contain inserted candidate: {:?}",
            s.composer.buffer
        );
    }

    #[test]
    // Typing `@cha` opens a ContextToken popup including `@changed`.
    // Esc closes the popup without altering the buffer.
    fn at_context_token_popup_opens_and_closes_on_esc() {
        let mut s = fresh_state();
        for c in "@cha".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let popup = s.composer_popup.as_ref().expect("popup should be open");
        assert_eq!(popup.kind, crate::completion::CompletionKind::ContextToken);
        assert!(popup.candidates.iter().any(|c| c.display == "@changed"));
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.composer_popup.is_none());
        assert_eq!(s.composer.buffer, "@cha");
    }

    #[test]
    // Up/Down navigate the popup selection without touching the buffer.
    fn popup_up_down_navigates_selection() {
        let mut s = fresh_state();
        for c in "/p".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let first = s.composer_popup.as_ref().unwrap().selected;
        s.apply(TuiInbound::Key(KeyEvent::Down));
        let next = s.composer_popup.as_ref().unwrap().selected;
        assert_ne!(first, next);
        s.apply(TuiInbound::Key(KeyEvent::Up));
        let back = s.composer_popup.as_ref().unwrap().selected;
        assert_eq!(back, first);
    }

    #[test]
    // A small paste is inserted at the cursor and produces no warning.
    fn small_paste_inserts_without_warning() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Paste("hello world".to_string()));
        assert_eq!(s.composer.buffer, "hello world");
        assert_eq!(s.composer.cursor, "hello world".len());
        assert!(s.activity.iter().all(|a| !a.contains("paste")));
    }

    #[test]
    // A paste over PASTE_WARN_BYTES surfaces an activity entry.
    fn large_paste_logs_warning() {
        let mut s = fresh_state();
        let data: String = "x".repeat(PASTE_WARN_BYTES + 100);
        s.apply(TuiInbound::Paste(data));
        assert!(s.activity.iter().any(|a| a.contains("large paste")));
    }

    #[test]
    // A paste over PASTE_HARD_LIMIT is truncated to the cap.
    fn huge_paste_is_truncated() {
        let mut s = fresh_state();
        let data: String = "x".repeat(PASTE_HARD_LIMIT * 2);
        s.apply(TuiInbound::Paste(data));
        assert_eq!(s.composer.buffer.len(), PASTE_HARD_LIMIT);
        assert!(s.activity.iter().any(|a| a.contains("paste truncated")));
    }

    #[test]
    // Accepting a mid-word completion replaces both the prefix and
    // the suffix of the token under the cursor, not just the part
    // before the cursor. Without this fix `/pl|an` + Tab would
    // produce `/planan` (suffix duplication).
    fn accept_completion_replaces_whole_token() {
        let mut s = fresh_state();
        for c in "/plugins".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        // Move the cursor back four characters so it sits between
        // `/pl` and `ugins` — the popup's selected candidate is
        // `/plan` (alphabetically first /pl* match).
        s.composer.cursor = 3;
        s.refresh_composer_popup();
        let popup = s.composer_popup.as_ref().expect("popup");
        assert_eq!(popup.kind, crate::completion::CompletionKind::Slash);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // Suffix `ugins` must be replaced, not duplicated.
        assert!(
            !s.composer.buffer.contains("ugins"),
            "suffix duplication: {:?}",
            s.composer.buffer
        );
        // The accepted candidate (whatever the popup picked) is in
        // the buffer.
        assert!(s.composer.buffer.starts_with('/'));
    }

    #[test]
    // Cycling focus away from the composer closes the completion
    // popup, so popup keys (Tab/Up/Down/Esc) don't hijack the new
    // pane. Tab itself is consumed by the popup-accept path when
    // the popup is open, so the cycle is exercised by ShiftTab,
    // which falls through to the layout dispatch.
    fn focus_cycle_clears_completion_popup() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        for c in "/p".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert!(s.composer_popup.is_some());
        s.apply(TuiInbound::Key(KeyEvent::ShiftTab));
        assert!(
            !matches!(s.layout.focus, FocusPane::Composer),
            "focus should have moved off composer"
        );
        assert!(
            s.composer_popup.is_none(),
            "popup should close when focus leaves composer"
        );
    }

    #[test]
    // Paste is ignored while a modal is open so the underlying
    // composer never gets corrupted by a clipboard payload that was
    // meant for the modal.
    fn paste_under_modal_is_swallowed() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        s.apply(TuiInbound::Paste("should be ignored".to_string()));
        assert!(s.composer.buffer.is_empty());
    }

    #[test]
    // ApprovalRequired event populates pending_approval, opens the
    // approval modal, and routes the detail panel to Approval mode.
    fn approval_event_opens_modal_and_detail_panel() {
        use code_agent_sdk::event::ApprovalRequiredEvent;
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalRequired(
            ApprovalRequiredEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                summary: "cargo test".to_string(),
            },
        )));
        let pending = s.pending_approval.as_ref().unwrap();
        assert_eq!(pending.tool_name, "Bash");
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::Custom { .. })
        ));
        assert_eq!(s.layout.detail_panel.mode, DetailPanelMode::Approval);
    }

    #[test]
    // `y` in the approval modal emits ResolveApproval(Allow) and
    // pops the modal. `pending_approval` is intentionally kept
    // until `SdkEvent::ApprovalResolved` arrives so the detail
    // panel doesn't flash an empty state.
    fn approval_y_key_emits_allow_and_keeps_pending() {
        use code_agent_sdk::event::ApprovalRequiredEvent;
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalRequired(
            ApprovalRequiredEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                summary: "cargo test".to_string(),
            },
        )));
        let actions = s.apply(TuiInbound::Key(KeyEvent::Char('y')));
        assert_eq!(
            actions,
            vec![TuiAction::ResolveApproval(ApprovalDecisionAction::Allow)]
        );
        assert!(
            s.pending_approval.is_some(),
            "pending_approval is held until ApprovalResolved"
        );
        assert!(s.layout.current_modal().is_none());
    }

    #[test]
    // ApprovalResolved (delayed, possibly with another modal layered
    // on top) cleans up the approval modal from anywhere in the
    // stack and clears pending_approval.
    fn approval_resolved_strips_modal_from_stack() {
        use code_agent_sdk::event::{
            ApprovalDecisionWire, ApprovalRequiredEvent, ApprovalResolvedEvent,
        };
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalRequired(
            ApprovalRequiredEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                summary: "cargo test".to_string(),
            },
        )));
        // Layer a Help modal on top so the approval modal is NOT
        // at the top of the stack.
        s.layout.push_modal(TuiModal::Help);
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalResolved(
            ApprovalResolvedEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                decision: ApprovalDecisionWire::Allow,
            },
        )));
        assert!(s.pending_approval.is_none());
        // Approval modal removed; Help modal still on top.
        assert!(matches!(s.layout.current_modal(), Some(TuiModal::Help)));
        assert_eq!(s.layout.modal_stack.len(), 1);
    }

    #[test]
    // #80: resolving an approval restores the detail panel to its prior
    // mode/visibility (a Diff here) instead of leaving it pinned to
    // Approval with empty content.
    fn approval_resolved_restores_prior_detail_panel() {
        use code_agent_sdk::event::{ApprovalDecisionWire, ApprovalResolvedEvent};
        let mut s = fresh_state();
        // The detail panel is showing a diff before the approval arrives.
        s.set_detail_panel(DetailPanelMode::Diff, true);
        s.open_approval_modal("Bash".to_string(), "cargo test".to_string());
        // Sanity: the approval override took effect.
        assert_eq!(s.layout.detail_panel.mode, DetailPanelMode::Approval);
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalResolved(
            ApprovalResolvedEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                decision: ApprovalDecisionWire::Allow,
            },
        )));
        // Prior diff view is restored, not left on empty Approval mode.
        assert_eq!(s.layout.detail_panel.mode, DetailPanelMode::Diff);
        assert!(s.layout.detail_panel.visible);
        assert!(s.prior_detail_panel.is_none());
    }

    // #177: the approval modal can be opened from two uncoordinated
    // paths — the bridge handler (which opens the modal directly) and
    // the `SdkEvent::ApprovalRequired` reducer arm. Whatever order they
    // arrive in, the stack must end up with exactly one approval modal so
    // the decision can never go to the wrong (resolver-less) prompt.
    fn count_approval_modals(s: &TuiAppState) -> usize {
        s.layout
            .modal_stack
            .iter()
            .filter(|m| matches!(m, TuiModal::Custom { id, .. } if id == APPROVAL_MODAL_ID))
            .count()
    }

    #[test]
    // Bridge-first then event: the bridge opens the modal (modelled here
    // by a direct `open_approval_modal`), then the matching SDK event
    // arrives. The reducer must NOT stack a second modal.
    fn approval_bridge_then_event_keeps_single_modal() {
        use code_agent_sdk::event::ApprovalRequiredEvent;
        let mut s = fresh_state();
        // Bridge path.
        s.open_approval_modal("Bash".to_string(), "rm -rf target".to_string());
        assert_eq!(count_approval_modals(&s), 1);
        // SDK event for the same approval arrives afterwards.
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalRequired(
            ApprovalRequiredEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                summary: "rm -rf target".to_string(),
            },
        )));
        assert_eq!(
            count_approval_modals(&s),
            1,
            "reducer must not stack a duplicate approval modal"
        );
    }

    #[test]
    // Event-first then bridge: the SDK event opens a (resolver-less)
    // modal, then the bridge opens its own. `open_approval_modal` is
    // idempotent on the stack, so there is still exactly one modal.
    fn approval_event_then_bridge_keeps_single_modal() {
        use code_agent_sdk::event::ApprovalRequiredEvent;
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::ApprovalRequired(
            ApprovalRequiredEvent {
                run_id: rid(),
                tool_name: "Bash".to_string(),
                summary: "rm -rf target".to_string(),
            },
        )));
        assert_eq!(count_approval_modals(&s), 1);
        // Bridge path opens its modal for the same approval.
        s.open_approval_modal("Bash".to_string(), "rm -rf target".to_string());
        assert_eq!(
            count_approval_modals(&s),
            1,
            "open_approval_modal must replace, not stack, the existing modal"
        );
    }

    #[test]
    // PatchProposed populates last_patch and routes the detail
    // panel to Diff mode.
    fn patch_event_routes_to_diff_panel() {
        use code_agent_sdk::event::PatchProposedEvent;
        use code_agent_sdk::patch::PatchSet;
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::PatchProposed(
            PatchProposedEvent {
                run_id: rid(),
                patch: PatchSet::default(),
            },
        )));
        assert!(s.last_patch.is_some());
        assert_eq!(s.layout.detail_panel.mode, DetailPanelMode::Diff);
    }

    #[test]
    // Phase F: with_inspectors replaces the snapshot and the
    // per-mode length getters reflect the new data.
    fn with_inspectors_replaces_snapshot() {
        use crate::inspectors::{InspectorPlugin, InspectorTool, Inspectors};
        let s = fresh_state().with_inspectors(Inspectors {
            tools: vec![InspectorTool {
                name: "Bash".to_string(),
                description: "run a shell command".to_string(),
                permission: "execute_bash".to_string(),
            }],
            plugins: vec![InspectorPlugin {
                name: "x".to_string(),
                version: Some("0.1".to_string()),
                enabled: true,
                scope: "user".to_string(),
                status: "loaded".to_string(),
                component_total: 3,
            }],
            ..Inspectors::default()
        });
        assert_eq!(s.inspector_list_len(SidePanelMode::Tools), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Plugins), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Mcp), Some(0));
        // Non-inspector modes report None so the renderer can short-circuit.
        assert_eq!(s.inspector_list_len(SidePanelMode::Plan), None);
    }

    #[test]
    // Phase F: j/Down/k/Up walk inspector selection inside the
    // focused side-panel when the panel mode is an inspector mode.
    fn inspector_j_k_navigates_selection() {
        use crate::inspectors::{InspectorTool, Inspectors};
        let tools = vec![
            InspectorTool {
                name: "Bash".into(),
                description: "shell".into(),
                permission: "execute_bash".into(),
            },
            InspectorTool {
                name: "Read".into(),
                description: "read a file".into(),
                permission: "read_only".into(),
            },
            InspectorTool {
                name: "Write".into(),
                description: "write a file".into(),
                permission: "write_fs".into(),
            },
        ];
        let mut s = fresh_state().with_inspectors(Inspectors {
            tools,
            ..Inspectors::default()
        });
        // Need Full layout for the side panel to render, otherwise
        // apply_slash_command keeps focus on the composer.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Switch to Tools panel and focus it.
        s.apply_slash_command("tools");
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // Initial state: no selection key has been pressed yet.
        assert!(s.inspector_selection.tools.is_none());
        // First `j` initializes selection at 0 then moves to 1.
        // The reducer seeds the starting cursor at 0, so a single
        // `j` moves to index 1. Two `j`s pin at 2 (last index).
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(1));
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        s.apply(TuiInbound::Key(KeyEvent::Char('j'))); // saturate
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(2));
        // k moves up; saturates at 0.
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(1));
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(0));
        // G jumps to last; g jumps to first.
        s.apply(TuiInbound::Key(KeyEvent::Char('G')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(2));
        s.apply(TuiInbound::Key(KeyEvent::Char('g')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(0));
    }

    #[test]
    // Phase F: when the inspector list is empty, navigation keys are
    // swallowed silently and never escape into the composer buffer.
    fn inspector_nav_on_empty_list_is_inert() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply_slash_command("mcp");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Mcp);
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // 'j' on an empty MCP list does not move selection or fall
        // through to the composer.
        let actions = s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert!(actions.is_empty());
        assert!(s.composer.buffer.is_empty());
        assert_eq!(s.inspector_selected_index(SidePanelMode::Mcp), None);
    }

    #[test]
    // Phase F: switching panels preserves the per-mode selection so a
    // user can flick between Tools and Skills and find their cursor
    // where they left it.
    fn inspector_selection_is_per_mode() {
        use crate::inspectors::{InspectorSkill, InspectorTool, Inspectors};
        let mut s = fresh_state().with_inspectors(Inspectors {
            tools: vec![
                InspectorTool {
                    name: "a".into(),
                    ..Default::default()
                },
                InspectorTool {
                    name: "b".into(),
                    ..Default::default()
                },
            ],
            skills: vec![
                InspectorSkill {
                    name: "x".into(),
                    ..Default::default()
                },
                InspectorSkill {
                    name: "y".into(),
                    ..Default::default()
                },
                InspectorSkill {
                    name: "z".into(),
                    ..Default::default()
                },
            ],
            ..Inspectors::default()
        });
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply_slash_command("tools");
        s.apply(TuiInbound::Key(KeyEvent::Char('G')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(1));
        s.apply_slash_command("skills");
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert_eq!(s.inspector_selected_index(SidePanelMode::Skills), Some(1));
        // Switch back: Tools selection survived.
        s.apply_slash_command("tools");
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(1));
    }

    #[test]
    // Phase F: non-nav keys in an inspector-focused side panel are
    // also swallowed (don't escape into the composer). Locking in
    // the behavior so a typo against a stale or empty inspector
    // doesn't accidentally compose a prompt.
    fn inspector_nonnav_key_does_not_modify_composer() {
        use crate::inspectors::{InspectorTool, Inspectors};
        let mut s = fresh_state().with_inspectors(Inspectors {
            tools: vec![InspectorTool {
                name: "Bash".into(),
                ..Default::default()
            }],
            ..Inspectors::default()
        });
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply_slash_command("tools");
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // 'x' isn't a nav key; with the side panel focused on an
        // inspector mode it must be swallowed, not appended to the
        // composer.
        let before = s.composer.buffer.clone();
        let actions = s.apply(TuiInbound::Key(KeyEvent::Char('x')));
        assert!(actions.is_empty());
        assert_eq!(s.composer.buffer, before, "composer must not change");
    }

    #[test]
    // Phase F: when a stale snapshot has a selection index that is now
    // out of bounds (e.g. a hot reload trimmed the list), the
    // `inspector_selected_index` clamp returns the last valid index
    // rather than panicking on the renderer.
    fn inspector_selection_clamps_to_visible_range() {
        use crate::inspectors::{InspectorTool, Inspectors};
        let mut s = fresh_state().with_inspectors(Inspectors {
            tools: vec![InspectorTool {
                name: "only".into(),
                ..Default::default()
            }],
            ..Inspectors::default()
        });
        s.inspector_selection.tools = Some(42);
        assert_eq!(s.inspector_selected_index(SidePanelMode::Tools), Some(0));
    }

    #[test]
    // Phase G fix: Esc on the tool-approval modal must resolve the
    // SDK callback with a Deny, not silently dismiss. The reducer
    // emits a ResolveApproval(Deny) action so app.rs can send it
    // through the bridge resolver and prevent SDK hang.
    fn approval_modal_esc_emits_deny_action() {
        let mut s = fresh_state();
        s.open_approval_modal("Bash".to_string(), "rm -rf target".to_string());
        assert!(s.pending_approval.is_some());
        let actions = s.apply(TuiInbound::Key(KeyEvent::Esc));
        match actions.first() {
            Some(TuiAction::ResolveApproval(ApprovalDecisionAction::Deny { reason })) => {
                assert!(reason.contains("dismissed"));
            }
            other => panic!("expected ResolveApproval(Deny), got {other:?}"),
        }
        // Modal popped, but pending_approval is preserved until the
        // SDK confirms via ApprovalResolved (matches Phase E
        // semantics).
        assert!(s.layout.current_modal().is_none());
        assert!(s.pending_approval.is_some());
    }

    #[test]
    // Phase G: open_approval_modal sets pending_approval and pushes
    // the modal whether or not an SdkEvent::ApprovalRequired ever
    // arrives. Used by the approval-bridge path so a callback-only
    // SDK still surfaces a prompt.
    fn open_approval_modal_directly() {
        let mut s = fresh_state();
        s.open_approval_modal("Bash".to_string(), "echo hi".to_string());
        assert_eq!(
            s.pending_approval.as_ref().unwrap().tool_name,
            "Bash".to_string()
        );
        match s.layout.current_modal() {
            Some(TuiModal::Custom { id, title, .. }) => {
                assert_eq!(id, APPROVAL_MODAL_ID);
                assert!(title.contains("Bash"));
            }
            _ => panic!("expected approval modal on top"),
        }
    }

    #[test]
    // Phase G: info-severity diagnostics are NOT mirrored to the
    // activity log (avoid churning the 16-entry panel with low-signal
    // entries). Warning / error still mirror.
    fn info_diagnostic_skips_activity_mirror() {
        let mut s = fresh_state();
        s.push_diagnostic(DiagnosticEntry {
            severity: "info".into(),
            code: "ok".into(),
            message: "nothing to see here".into(),
            suggestion: None,
        });
        assert_eq!(s.diagnostics.len(), 1);
        // No mirror for info.
        assert!(s
            .activity
            .iter()
            .all(|a| !a.contains("nothing to see here")));
        // Warning DOES mirror.
        s.push_diagnostic(DiagnosticEntry {
            severity: "warning".into(),
            code: "x".into(),
            message: "look here".into(),
            suggestion: None,
        });
        assert!(s.activity.iter().any(|a| a.contains("look here")));
    }

    #[test]
    // Phase G: SdkEvent::Diagnostic feeds the diagnostics ring buffer
    // and mirrors a one-line summary to the activity log.
    fn sdk_diagnostic_pushes_into_ring_buffer() {
        use code_agent_sdk::error::{Diagnostic, DiagnosticSeverity};
        use code_agent_sdk::event::DiagnosticEvent;
        let mut s = fresh_state();
        let diag = Diagnostic::new(
            "tool/exec/timeout",
            DiagnosticSeverity::Warning,
            "bash command exceeded budget",
        )
        .with_suggestion("raise tool_limits.bash_timeout_seconds");
        s.apply(TuiInbound::sdk(SdkEvent::Diagnostic(DiagnosticEvent {
            run_id: rid(),
            diagnostic: diag,
        })));
        assert_eq!(s.diagnostics.len(), 1);
        assert_eq!(s.diagnostics[0].severity, "warning");
        assert_eq!(s.diagnostics[0].code, "tool/exec/timeout");
        assert!(s.diagnostics[0]
            .suggestion
            .as_deref()
            .map(|s| s.contains("bash_timeout_seconds"))
            .unwrap_or(false));
        // The activity log also receives a short mirror.
        assert!(s
            .activity
            .last()
            .map(|a| a.contains("tool/exec/timeout"))
            .unwrap_or(false));
    }

    #[test]
    // Phase G: diagnostics ring buffer caps at DIAGNOSTIC_CAP; oldest
    // entries drop on overflow.
    fn diagnostics_ring_buffer_is_bounded() {
        let mut s = fresh_state();
        for i in 0..(DIAGNOSTIC_CAP + 5) {
            s.push_diagnostic(DiagnosticEntry {
                severity: "info".into(),
                code: format!("c{i}"),
                message: format!("m{i}"),
                suggestion: None,
            });
        }
        assert_eq!(s.diagnostics.len(), DIAGNOSTIC_CAP);
        // Oldest 5 dropped; first remaining entry is index 5.
        assert_eq!(s.diagnostics.first().unwrap().code, "c5");
    }

    #[test]
    // Phase G: inspector_list_len now reports for sessions, accounts,
    // models, providers, and diagnostics modes.
    fn phase_g_inspector_modes_report_lengths() {
        use crate::inspectors::{
            InspectorAccount, InspectorModel, InspectorProvider, InspectorSession, Inspectors,
        };
        let mut s = fresh_state().with_inspectors(Inspectors {
            sessions: vec![InspectorSession {
                id: "s1".into(),
                ..Default::default()
            }],
            accounts: vec![InspectorAccount {
                provider: "anthropic".into(),
                ..Default::default()
            }],
            models: vec![InspectorModel {
                provider: "anthropic".into(),
                model_id: "claude".into(),
                ..Default::default()
            }],
            providers: vec![InspectorProvider {
                provider: "anthropic".into(),
                ..Default::default()
            }],
            ..Inspectors::default()
        });
        s.push_diagnostic(DiagnosticEntry {
            severity: "info".into(),
            code: "x".into(),
            ..Default::default()
        });
        assert_eq!(s.inspector_list_len(SidePanelMode::Sessions), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Auth), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Model), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Provider), Some(1));
        assert_eq!(s.inspector_list_len(SidePanelMode::Diagnostics), Some(1));
    }

    #[test]
    // Phase H: /palette opens the palette modal and seeds an empty
    // PaletteState.
    fn palette_slash_opens_modal_and_seeds_state() {
        let mut s = fresh_state();
        s.apply_slash_command("palette");
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::CommandPalette)
        ));
        assert!(s.palette.is_some());
        assert!(s.palette.as_ref().unwrap().input.is_empty());
    }

    #[test]
    // Phase H: typing into the palette filters the candidate list
    // case-insensitively; arrow keys move selection; Enter executes
    // the highlighted command (closing the modal).
    fn palette_filter_navigate_and_enter() {
        let mut s = fresh_state();
        s.apply_slash_command("palette");
        // Type "AC" — should filter to commands containing "ac"
        // case-insensitively (e.g. `activity`).
        s.apply(TuiInbound::Key(KeyEvent::Char('A')));
        s.apply(TuiInbound::Key(KeyEvent::Char('c')));
        let candidates = s.palette_candidates();
        assert!(
            candidates.contains(&"activity"),
            "expected `activity` in candidates: {candidates:?}"
        );
        // Navigate to last and execute — `Enter` closes the palette
        // and runs the picked slash command.
        let target_idx = candidates.iter().position(|c| *c == "activity").unwrap();
        for _ in 0..target_idx {
            s.apply(TuiInbound::Key(KeyEvent::Down));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(s.layout.current_modal().is_none(), "palette should close");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Activity);
    }

    #[test]
    // Phase H: Backspace deletes from input and resets selection.
    // Esc clears palette state so a re-open starts blank.
    fn palette_backspace_and_esc_reset() {
        let mut s = fresh_state();
        s.apply_slash_command("palette");
        for c in "help".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert_eq!(s.palette.as_ref().unwrap().input, "help");
        s.apply(TuiInbound::Key(KeyEvent::Backspace));
        assert_eq!(s.palette.as_ref().unwrap().input, "hel");
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.palette.is_none());
        assert!(s.layout.current_modal().is_none());
    }

    #[test]
    // Phase H: the Activity side panel is an inspector mode with
    // selection driven by inspector_list_len + selected_index over
    // the bounded activity log.
    fn activity_panel_is_inspector_mode() {
        let mut s = fresh_state();
        s.push_activity("alpha".to_string());
        s.push_activity("beta".to_string());
        assert_eq!(s.inspector_list_len(SidePanelMode::Activity), Some(2));
        // Resize to Full so /activity actually focuses the panel.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply_slash_command("activity");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Activity);
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // Switching panels itself appends an activity entry, so the
        // log has 3 rows now ("alpha" / "beta" / the side-panel
        // log line). 'G' jumps to the last index.
        let last = s.activity.len() - 1;
        s.apply(TuiInbound::Key(KeyEvent::Char('G')));
        assert_eq!(
            s.inspector_selected_index(SidePanelMode::Activity),
            Some(last)
        );
    }

    #[test]
    // Phase H review-fix: Tab in the open palette accepts the
    // highlighted entry rather than cycling pane focus.
    fn palette_tab_accepts_like_enter() {
        let mut s = fresh_state();
        s.open_palette();
        // Type `tools` to filter to a single candidate.
        for c in "tools".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert!(s.palette.is_none(), "palette should close on Tab accept");
        assert!(s.layout.current_modal().is_none());
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Tools);
    }

    #[test]
    // Phase H review-fix: open_palette / close_palette maintain the
    // "modal open ⇔ palette state Some" invariant. close_palette
    // removes only the CommandPalette modal from the stack, even
    // when other modals are layered on top.
    fn open_close_palette_maintain_invariant() {
        let mut s = fresh_state();
        s.open_palette();
        assert!(s.palette.is_some());
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::CommandPalette)
        ));
        // Layer a Help modal on top.
        s.layout.push_modal(TuiModal::Help);
        s.close_palette();
        assert!(s.palette.is_none());
        // Help modal survives.
        assert!(matches!(s.layout.current_modal(), Some(TuiModal::Help)));
    }

    #[test]
    // Phase I: telemetry events fire as `debug!` records on the
    // `code_agent_tui` target. We can't capture them without a
    // tracing subscriber here, so as a stand-in we just verify the
    // surrounding side-effects (activity entry, panel switch) still
    // happen — i.e., the tracing calls don't perturb reducer
    // behavior.
    fn telemetry_does_not_perturb_state() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply_slash_command("tools");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Tools);
        assert!(s
            .activity
            .last()
            .map(|a| a.contains("side panel"))
            .unwrap_or(false));
    }

    #[test]
    // Phase I (review fix): real tracing-subscriber capture asserts
    // the telemetry contract — event names and the absence of
    // sensitive payload. Without this test, removing the target or
    // renaming an event would silently break a downstream
    // subscriber.
    fn telemetry_emits_events_with_expected_shape() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct CapturedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for CapturedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for CapturedWriter {
            type Writer = CapturedWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::DEBUG)
            .with_target(true)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut s = fresh_state();
            s.apply(TuiInbound::Resize {
                width: 120,
                height: 30,
            });
            // Trigger panel-switch, palette-open, submit, approval.
            s.apply_slash_command("tools");
            s.open_palette();
            // Close the palette before submitting so chars hit the
            // composer instead of the palette input.
            s.close_palette();
            // After /tools, focus is on the side panel; move it back
            // to the composer so character input lands in the
            // composer buffer rather than being swallowed by the
            // inspector-nav swallow.
            s.layout.focus = FocusPane::Composer;
            for c in "hi".chars() {
                s.apply(TuiInbound::Key(KeyEvent::Char(c)));
            }
            s.apply(TuiInbound::Key(KeyEvent::Enter));
            // Approval flow.
            s.open_approval_modal("Bash".to_string(), "summary".to_string());
            // Press 'y' to allow.
            s.apply(TuiInbound::Key(KeyEvent::Char('y')));
        });
        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("code_agent_tui"), "{captured}");
        assert!(captured.contains("side_panel_switch"), "{captured}");
        assert!(captured.contains("palette_open"), "{captured}");
        assert!(captured.contains("submit_prompt"), "{captured}");
        assert!(captured.contains("approval_decision"), "{captured}");
        assert!(captured.contains("decision_kind=\"allow\""), "{captured}");
        // Privacy: the raw prompt text must not appear; only the byte count.
        assert!(!captured.contains("prompt=\"hi\""), "{captured}");
        // And the Deny reason text from a denial path must not leak.
        assert!(
            !captured.contains("dismissed without decision"),
            "{captured}"
        );
    }

    #[test]
    // Transcript length is bounded to TRANSCRIPT_CAP.
    fn transcript_is_bounded() {
        let mut s = fresh_state();
        for i in 0..(TRANSCRIPT_CAP + 10) {
            s.push_transcript(EventCard {
                kind: "test",
                headline: i.to_string(),
                body: Vec::new(),
                ..Default::default()
            });
        }
        assert_eq!(s.transcript.len(), TRANSCRIPT_CAP);
        // Oldest cards (0..10) were dropped.
        assert_eq!(s.transcript.first().unwrap().headline, "10");
    }

    // #27: the modal pushed on `SdkEvent::PlanCreated` must be a purely
    // informational notice. It must NOT advertise approve/reject/revise
    // keys (the handlers were no-ops while the SDK auto-approved the plan
    // internally, so those affordances lied about gating execution), and
    // it must state that approval gating is not yet wired and the run
    // will proceed. Pinning this prevents a silent regression to the old
    // lying copy.
    #[test]
    fn plan_created_modal_is_informational_notice() {
        use code_agent_core::plan::{new_plan_id, PlanDocument};
        use code_agent_sdk::event::PlanCreated;

        let mut s = fresh_state();
        let plan = PlanDocument::new_draft(new_plan_id(), "demo", "u");
        s.apply(TuiInbound::sdk(SdkEvent::PlanCreated(PlanCreated {
            run_id: rid(),
            plan,
        })));

        let modal = s
            .layout
            .current_modal()
            .expect("plan-created notice should be open");
        let body = match modal {
            TuiModal::Custom { id, body, .. } => {
                assert_eq!(id, PLAN_APPROVAL_MODAL_ID);
                body.clone()
            }
            other => panic!("expected Custom modal, got {other:?}"),
        };
        // No decision-key affordance may be advertised: the old copy
        // showed `[a] approve* [r] reject* [u] revise*` (preview-only),
        // which implied the keystrokes gated execution.
        assert!(
            !body.iter().any(|l| l.contains("[a]")
                || l.contains("[r]")
                || l.contains("[u]")
                || l.to_lowercase().contains("approve")
                || l.to_lowercase().contains("preview")),
            "notice must not advertise decision keys, got: {body:?}"
        );
        assert!(
            body.iter().any(|l| l.contains("not yet wired")),
            "notice must say gating is not wired, got: {body:?}"
        );
        assert!(
            body.iter().any(|l| l.contains("will proceed")),
            "notice must say the run proceeds, got: {body:?}"
        );
        // Each body line must fit a default 80-col terminal so the
        // modal does not wrap or truncate (review feedback).
        for line in &body {
            assert!(
                line.chars().count() <= 80,
                "body line exceeds 80 columns ({} chars): {line}",
                line.chars().count()
            );
        }
    }

    // #27 (was #226): pressing a key on the plan-created notice must
    // dismiss it without emitting any action, regardless of whether a
    // `current_plan` is set. The notice is informational only — no key
    // implies a decision was enforced.
    #[test]
    fn plan_created_notice_key_dismisses_without_action_no_plan() {
        for key in ['a', 'r', 'u'] {
            let mut s = fresh_state();
            // Push the notice without seeding `current_plan`.
            s.layout.push_modal(TuiModal::Custom {
                id: PLAN_APPROVAL_MODAL_ID.to_string(),
                title: "Plan created (informational)".to_string(),
                body: vec!["The plan was created and the run will proceed.".to_string()],
            });
            assert!(s.current_plan.is_none());
            let actions = s.apply(TuiInbound::Key(KeyEvent::Char(key)));
            // Informational notice → never any action, modal must be gone.
            assert!(
                actions.is_empty(),
                "informational notice must yield no action for {key}, got: {actions:?}"
            );
            assert!(
                s.layout.current_modal().is_none(),
                "notice must close for key {key}"
            );
        }
    }

    // #27: with a plan present, a/r/u must STILL emit no decision action
    // (the run auto-proceeds; the keys cannot block it). This pins the
    // honest behavior so the modal can't silently regress to emitting an
    // Approve/Reject/Revise that lies about gating execution.
    #[test]
    fn plan_created_notice_key_emits_no_decision_with_plan() {
        use code_agent_core::plan::{new_plan_id, PlanDocument};
        use code_agent_sdk::event::PlanCreated;

        let mut s = fresh_state();
        let plan = PlanDocument::new_draft(new_plan_id(), "demo", "u");
        s.apply(TuiInbound::sdk(SdkEvent::PlanCreated(PlanCreated {
            run_id: rid(),
            plan,
        })));
        assert!(s.current_plan.is_some());
        let actions = s.apply(TuiInbound::Key(KeyEvent::Char('a')));
        assert!(
            actions.is_empty(),
            "notice must emit no decision action, got: {actions:?}"
        );
        assert!(s.layout.current_modal().is_none());
    }

    // #27 (Codex P2): the notice copy promises "Press Esc (or any key) to
    // dismiss this notice", so EVERY key kind — not just `Char(_)` — must
    // dismiss it. Previously only chars were routed to the dismiss
    // handler; Enter / Tab / arrows / Backspace fell through to the
    // generic no-op modal arm and did nothing, making "any key"
    // inaccurate. Pin that all non-char keys (and Esc) now dismiss the
    // notice with no action so the copy stays honest.
    #[test]
    fn plan_created_notice_any_key_dismisses_without_action() {
        let keys = [
            KeyEvent::Enter,
            KeyEvent::Tab,
            KeyEvent::Up,
            KeyEvent::Down,
            KeyEvent::Backspace,
            KeyEvent::Esc,
        ];
        for key in keys {
            let mut s = fresh_state();
            s.layout.push_modal(TuiModal::Custom {
                id: PLAN_APPROVAL_MODAL_ID.to_string(),
                title: "Plan created (informational)".to_string(),
                body: vec!["The plan was created and the run will proceed.".to_string()],
            });
            let actions = s.apply(TuiInbound::Key(key));
            assert!(
                actions.is_empty(),
                "informational notice must yield no action for {key:?}, got: {actions:?}"
            );
            assert!(
                s.layout.current_modal().is_none(),
                "notice must close for non-char key {key:?}"
            );
        }
    }

    // Regression (Gemini review): Ctrl+C must NOT be swallowed by the
    // plan-notice "any key dismisses" catch-all. The catch-all sits before
    // the global `(_, KeyEvent::CtrlC)` modal arm, so without an explicit
    // exclusion it would intercept Ctrl+C and merely dismiss the notice,
    // silently dropping the cancel gesture. Pin that while the notice is
    // open: Ctrl+C while a run is in progress cancels the run (the global
    // modal-CtrlC behavior), exactly as it does without the notice open. A
    // non-Ctrl+C key (Enter) still dismisses the notice.
    #[test]
    fn plan_created_notice_ctrl_c_cancels_not_dismiss() {
        // Running: Ctrl+C must cancel the run, not silently dismiss.
        // This is the regression: the catch-all would have intercepted
        // Ctrl+C and dismissed the modal, dropping the CancelRun.
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        s.layout.push_modal(TuiModal::Custom {
            id: PLAN_APPROVAL_MODAL_ID.to_string(),
            title: "Plan created (informational)".to_string(),
            body: vec!["The plan was created and the run will proceed.".to_string()],
        });
        let actions = s.apply(TuiInbound::Key(KeyEvent::CtrlC));
        assert_eq!(
            actions,
            vec![TuiAction::CancelRun],
            "Ctrl+C over the plan notice while running must cancel, not dismiss"
        );

        // Idle: Ctrl+C falls through to the global modal-CtrlC arm, which
        // is a deliberate no-op while a modal is open (it does not exit out
        // from under a modal). The point of this assertion is the inverse of
        // the regression: Ctrl+C is NOT treated as an "any key" dismiss — it
        // yields no action and the notice stays open rather than being
        // silently popped by the catch-all.
        let mut s = fresh_state();
        assert_eq!(s.run_status, RunStatus::Idle);
        s.layout.push_modal(TuiModal::Custom {
            id: PLAN_APPROVAL_MODAL_ID.to_string(),
            title: "Plan created (informational)".to_string(),
            body: vec!["The plan was created and the run will proceed.".to_string()],
        });
        let actions = s.apply(TuiInbound::Key(KeyEvent::CtrlC));
        assert!(
            actions.is_empty(),
            "Ctrl+C over an idle plan notice must yield no action, got: {actions:?}"
        );
        assert!(
            s.layout.current_modal().is_some(),
            "Ctrl+C must not be swallowed by the dismiss catch-all (notice should remain)"
        );

        // Sanity: a non-Ctrl+C key still dismisses the notice with no action.
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Custom {
            id: PLAN_APPROVAL_MODAL_ID.to_string(),
            title: "Plan created (informational)".to_string(),
            body: vec!["The plan was created and the run will proceed.".to_string()],
        });
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(
            actions.is_empty(),
            "Enter must dismiss the notice with no action, got: {actions:?}"
        );
        assert!(
            s.layout.current_modal().is_none(),
            "Enter must close the plan notice"
        );
    }

    // #27 (Codex P2): guard that the dismiss copy and the routing agree.
    // The notice must advertise "any key" dismissal — if a future change
    // narrows routing back to chars only, the copy here should be tightened
    // in lockstep (and this assertion updated).
    #[test]
    fn plan_created_notice_copy_promises_any_key() {
        use code_agent_core::plan::{new_plan_id, PlanDocument};
        use code_agent_sdk::event::PlanCreated;

        let mut s = fresh_state();
        let plan = PlanDocument::new_draft(new_plan_id(), "demo", "u");
        s.apply(TuiInbound::sdk(SdkEvent::PlanCreated(PlanCreated {
            run_id: rid(),
            plan,
        })));
        let body = match s.layout.current_modal().expect("notice open") {
            TuiModal::Custom { body, .. } => body.clone(),
            other => panic!("expected Custom modal, got {other:?}"),
        };
        assert!(
            body.iter()
                .any(|l| l.contains("Esc") && l.contains("any key")),
            "notice must promise Esc-or-any-key dismissal, got: {body:?}"
        );
    }

    // #227: under an active search filter, the visibility cache must
    // memoize on a miss so a second `visible_card_indices()` call between
    // transcript mutations is served from the cache (interior mutability).
    #[test]
    fn visible_cache_memoizes_under_active_filter() {
        let mut s = fresh_state();
        push_user_card(&mut s, "alpha");
        push_user_card(&mut s, "beta");
        s.search_query = Some("alpha".to_string());
        // First call after invalidation computes + stores.
        let first = s.visible_card_indices();
        assert!(s.visible_cache.borrow().is_some(), "cache must repopulate");
        // Second call returns the same memoized result.
        let second = s.visible_card_indices();
        assert_eq!(first, second);
        // A new transcript card invalidates the cache (next call recomputes).
        push_user_card(&mut s, "alphabet");
        assert!(
            s.visible_cache.borrow().is_none(),
            "push_transcript must invalidate the cache"
        );
        // Recompute reflects the new card and re-memoizes.
        let third = s.visible_card_indices();
        assert_eq!(third.len(), 2, "alpha + alphabet match");
        assert!(s.visible_cache.borrow().is_some());
    }

    // #228: pasting into an open SearchTranscript modal fills the search
    // input buffer instead of being discarded.
    #[test]
    fn paste_routes_into_search_modal_input() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::SearchTranscript);
        s.apply(TuiInbound::Paste("hello\nworld".to_string()));
        // Newlines stripped; single-line buffer filled.
        assert_eq!(s.search_input, "helloworld");
    }

    // #228: pasting into an open CommandPalette modal fills the palette
    // input buffer.
    #[test]
    fn paste_routes_into_palette_input() {
        let mut s = fresh_state();
        s.open_palette();
        s.apply(TuiInbound::Paste("hel\rlo".to_string()));
        assert_eq!(s.palette.as_ref().unwrap().input, "hello");
    }

    // A huge paste routed into a text-input modal is capped by
    // PASTE_HARD_LIMIT, just like the composer — the #228 modal routing must
    // not bypass the size guard (an unbounded search_input stalls rendering).
    #[test]
    fn huge_paste_into_search_modal_is_capped() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::SearchTranscript);
        let data: String = "x".repeat(PASTE_HARD_LIMIT * 2);
        s.apply(TuiInbound::Paste(data));
        assert_eq!(s.search_input.len(), PASTE_HARD_LIMIT);
    }

    // #228: non-text modals still drop pastes (no panic, no state change).
    #[test]
    fn paste_into_non_text_modal_is_noop() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        s.apply(TuiInbound::Paste("ignored".to_string()));
        assert!(s.search_input.is_empty());
        assert!(s.composer.buffer.is_empty());
    }

    // #153: Esc closing an unrelated modal must not wipe an underlying
    // search query input / palette state. The catch-all previously
    // cleared both on any modal Esc.
    #[test]
    fn esc_on_unrelated_modal_preserves_palette_state() {
        let mut s = fresh_state();
        s.open_palette();
        if let Some(p) = s.palette.as_mut() {
            p.input.push_str("foo");
        }
        // Layer an unrelated modal on top of the palette.
        s.layout.push_modal(TuiModal::Help);
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        // Help popped; palette state underneath is untouched.
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::CommandPalette)
        ));
        assert_eq!(
            s.palette.as_ref().map(|p| p.input.as_str()),
            Some("foo"),
            "palette input must survive Esc on an unrelated modal"
        );
    }

    // #153: Esc on the CommandPalette itself still clears palette state.
    #[test]
    fn esc_on_palette_clears_palette_state() {
        let mut s = fresh_state();
        s.open_palette();
        if let Some(p) = s.palette.as_mut() {
            p.input.push_str("foo");
        }
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.layout.current_modal().is_none());
        assert!(s.palette.is_none());
    }
}
