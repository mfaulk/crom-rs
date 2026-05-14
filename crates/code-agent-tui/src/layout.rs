//! Pure layout state: side panel, detail panel, modal stack, focus,
//! and a responsive `LayoutMode` derived from terminal dimensions.
//!
//! Phase B introduces the *shape* of the multi-pane TUI cockpit. The
//! actual content rendered for each [`SidePanelMode`] /
//! [`DetailPanelMode`] is a placeholder ("coming in Phase X") — real
//! content lands in the phase that owns the corresponding feature
//! (Plan dashboard in Phase E, inspectors in Phase F, etc.).
//!
//! Like [`crate::state`], everything here is synchronous and pure: the
//! reducer mutates `LayoutState` deterministically and the renderer
//! reads it. Snapshot tests cover the layout primitives directly.

/// Which logical pane currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusPane {
    #[default]
    Composer,
    Transcript,
    SidePanel,
    DetailPanel,
}

impl FocusPane {
    /// Order used for `Tab` / `Shift+Tab` cycling.
    pub fn cycle_order() -> [FocusPane; 4] {
        [
            FocusPane::Composer,
            FocusPane::Transcript,
            FocusPane::SidePanel,
            FocusPane::DetailPanel,
        ]
    }

    /// Return the next focus in cycle order, skipping panes that are not
    /// currently visible.
    pub fn next(self, side_visible: bool, detail_visible: bool) -> FocusPane {
        let order = Self::cycle_order();
        let idx = order.iter().position(|&p| p == self).unwrap_or(0);
        for step in 1..=order.len() {
            let candidate = order[(idx + step) % order.len()];
            if pane_is_focusable(candidate, side_visible, detail_visible) {
                return candidate;
            }
        }
        FocusPane::Composer
    }

    /// Return the previous focus in cycle order.
    pub fn prev(self, side_visible: bool, detail_visible: bool) -> FocusPane {
        let order = Self::cycle_order();
        let idx = order.iter().position(|&p| p == self).unwrap_or(0);
        for step in 1..=order.len() {
            // Step backwards through the cycle.
            let new = (idx + order.len() - step) % order.len();
            let candidate = order[new];
            if pane_is_focusable(candidate, side_visible, detail_visible) {
                return candidate;
            }
        }
        FocusPane::Composer
    }
}

fn pane_is_focusable(pane: FocusPane, side_visible: bool, detail_visible: bool) -> bool {
    match pane {
        FocusPane::Composer | FocusPane::Transcript => true,
        FocusPane::SidePanel => side_visible,
        FocusPane::DetailPanel => detail_visible,
    }
}

/// Which side-panel view is currently selected. The full list of modes
/// is fixed up front so later phases can fill content without changing
/// the reducer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidePanelMode {
    #[default]
    Plan,
    Changes,
    Files,
    Tools,
    Sessions,
    Diagnostics,
    Subagents,
    Mcp,
    Plugins,
    Skills,
    Usage,
    Auth,
    Model,
    Provider,
    /// M13 Phase H: scrollable view of the activity log (mirrors
    /// `TuiAppState::activity` with selection navigation).
    Activity,
}

impl SidePanelMode {
    /// Stable short label used by the status line and pane headers.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Changes => "Changes",
            Self::Files => "Files",
            Self::Tools => "Tools",
            Self::Sessions => "Sessions",
            Self::Diagnostics => "Diagnostics",
            Self::Subagents => "Agents",
            Self::Mcp => "MCP",
            Self::Plugins => "Plugins",
            Self::Skills => "Skills",
            Self::Usage => "Usage",
            Self::Auth => "Auth",
            Self::Model => "Model",
            Self::Provider => "Provider",
            Self::Activity => "Activity",
        }
    }

    /// Settings-config string used in `ui.default_side_panel`.
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Changes => "changes",
            Self::Files => "files",
            Self::Tools => "tools",
            Self::Sessions => "sessions",
            Self::Diagnostics => "diagnostics",
            Self::Subagents => "subagents",
            Self::Mcp => "mcp",
            Self::Plugins => "plugins",
            Self::Skills => "skills",
            Self::Usage => "usage",
            Self::Auth => "auth",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Activity => "activity",
        }
    }

    /// Parse the settings-config string. Returns `None` for unknown values.
    pub fn from_config_key(s: &str) -> Option<Self> {
        Some(match s {
            "plan" => Self::Plan,
            "changes" => Self::Changes,
            "files" => Self::Files,
            "tools" => Self::Tools,
            "sessions" => Self::Sessions,
            "diagnostics" => Self::Diagnostics,
            "subagents" | "agents" => Self::Subagents,
            "mcp" => Self::Mcp,
            "plugins" => Self::Plugins,
            "skills" => Self::Skills,
            "usage" => Self::Usage,
            "auth" => Self::Auth,
            "model" => Self::Model,
            "provider" => Self::Provider,
            "activity" => Self::Activity,
            _ => return None,
        })
    }
}

/// Which detail-panel view is currently selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailPanelMode {
    #[default]
    None,
    EventDetails,
    Diff,
    Approval,
    PlanDetails,
    FilePreview,
    ToolDetails,
    Diagnostics,
    SessionDetails,
    RawJson,
}

impl DetailPanelMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Details",
            Self::EventDetails => "Event",
            Self::Diff => "Diff",
            Self::Approval => "Approval",
            Self::PlanDetails => "Plan",
            Self::FilePreview => "File",
            Self::ToolDetails => "Tool",
            Self::Diagnostics => "Diagnostics",
            Self::SessionDetails => "Session",
            Self::RawJson => "Raw JSON",
        }
    }
}

/// Side-panel state. `visible` controls whether the pane is included in
/// the layout at all; `mode` selects which view is rendered.
#[derive(Debug, Clone)]
pub struct SidePanelState {
    pub mode: SidePanelMode,
    pub visible: bool,
}

impl Default for SidePanelState {
    fn default() -> Self {
        Self {
            mode: SidePanelMode::default(),
            visible: true,
        }
    }
}

/// Detail-panel state.
#[derive(Debug, Clone)]
pub struct DetailPanelState {
    pub mode: DetailPanelMode,
    pub visible: bool,
}

impl Default for DetailPanelState {
    fn default() -> Self {
        Self {
            mode: DetailPanelMode::None,
            visible: false,
        }
    }
}

/// A modal overlay. The variants name the modal class; content for
/// non-trivial modals (command palette, prompt history) lands in the
/// phase that owns that feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiModal {
    Help,
    CommandPalette,
    SearchTranscript,
    PromptHistory,
    /// Open-ended modal with a title and pre-formatted body lines.
    /// Lets future phases push ad-hoc modals (e.g. deviation,
    /// approval-expired, recovery) without inventing new variants
    /// before they're needed.
    Custom {
        id: String,
        title: String,
        body: Vec<String>,
    },
}

impl TuiModal {
    pub fn title(&self) -> &str {
        match self {
            Self::Help => "Help",
            Self::CommandPalette => "Command palette",
            Self::SearchTranscript => "Search transcript",
            Self::PromptHistory => "Prompt history",
            Self::Custom { title, .. } => title,
        }
    }
}

/// Responsive layout class derived from terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Status + transcript + side + detail + composer + footer.
    #[default]
    Full,
    /// Status + transcript + composer + footer (no side/detail panels).
    Compact,
    /// Transcript + composer only. Used when even the status line
    /// would steal too much vertical space.
    Tiny,
}

/// Width / height thresholds that decide [`LayoutMode`]. Centralized
/// so tests and renderer agree.
pub const FULL_LAYOUT_MIN_WIDTH: u16 = 100;
pub const FULL_LAYOUT_MIN_HEIGHT: u16 = 18;
pub const COMPACT_LAYOUT_MIN_WIDTH: u16 = 60;
pub const COMPACT_LAYOUT_MIN_HEIGHT: u16 = 10;

impl LayoutMode {
    pub fn from_size(width: u16, height: u16) -> Self {
        if width >= FULL_LAYOUT_MIN_WIDTH && height >= FULL_LAYOUT_MIN_HEIGHT {
            Self::Full
        } else if width >= COMPACT_LAYOUT_MIN_WIDTH && height >= COMPACT_LAYOUT_MIN_HEIGHT {
            Self::Compact
        } else {
            Self::Tiny
        }
    }
}

/// Top-level layout state. Owned by [`crate::state::TuiAppState`] and
/// mutated by the reducer.
#[derive(Debug, Clone, Default)]
pub struct LayoutState {
    pub mode: LayoutMode,
    pub focus: FocusPane,
    pub side_panel: SidePanelState,
    pub detail_panel: DetailPanelState,
    pub modal_stack: Vec<TuiModal>,
    pub show_help_footer: bool,
}

impl LayoutState {
    /// Recompute the responsive [`LayoutMode`] for the given terminal
    /// dimensions. Other fields are preserved so the user's panel
    /// selection persists across a resize.
    pub fn recompute_for_size(&mut self, width: u16, height: u16) {
        self.mode = LayoutMode::from_size(width, height);
    }

    /// Is the side panel laid out (i.e. visible AND not suppressed by
    /// the current responsive mode)?
    pub fn side_panel_rendered(&self) -> bool {
        self.side_panel.visible && matches!(self.mode, LayoutMode::Full)
    }

    /// Is the detail panel laid out?
    pub fn detail_panel_rendered(&self) -> bool {
        self.detail_panel.visible && matches!(self.mode, LayoutMode::Full)
    }

    /// Push a modal onto the stack. The topmost modal is rendered.
    pub fn push_modal(&mut self, modal: TuiModal) {
        self.modal_stack.push(modal);
    }

    /// Pop the topmost modal. Returns `None` if the stack is empty.
    pub fn pop_modal(&mut self) -> Option<TuiModal> {
        self.modal_stack.pop()
    }

    /// Top-of-stack modal, if any.
    pub fn current_modal(&self) -> Option<&TuiModal> {
        self.modal_stack.last()
    }

    /// Remove every `TuiModal::Custom` whose `id` matches `target_id`,
    /// regardless of stack position. Phase E uses this so a delayed
    /// `ApprovalResolved` / `PlanApproved` clears its modal even when
    /// the user has layered another modal on top in the meantime.
    /// Returns the number of modals removed.
    pub fn remove_custom_modals_by_id(&mut self, target_id: &str) -> usize {
        let before = self.modal_stack.len();
        self.modal_stack
            .retain(|m| !matches!(m, TuiModal::Custom { id, .. } if id == target_id));
        before - self.modal_stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // LayoutMode is full at standard terminal sizes, compacts when
    // either dimension is too small, and degrades to Tiny on very
    // small terminals.
    fn layout_mode_responsive_thresholds() {
        assert_eq!(LayoutMode::from_size(120, 40), LayoutMode::Full);
        assert_eq!(LayoutMode::from_size(80, 24), LayoutMode::Compact);
        // Below the compact-width threshold but above the compact-height
        // threshold → Tiny, because both dimensions must clear the bar.
        assert_eq!(LayoutMode::from_size(40, 10), LayoutMode::Tiny);
        // At-or-above both compact thresholds is Compact.
        assert_eq!(LayoutMode::from_size(60, 10), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_size(40, 6), LayoutMode::Tiny);
        assert_eq!(LayoutMode::from_size(20, 4), LayoutMode::Tiny);
        // Just below the full thresholds drops to Compact, not Tiny.
        assert_eq!(LayoutMode::from_size(99, 24), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_size(120, 17), LayoutMode::Compact);
    }

    #[test]
    // Recomputing on resize updates the mode without touching focus or
    // side panel selection.
    fn recompute_preserves_other_fields() {
        let mut s = LayoutState {
            focus: FocusPane::SidePanel,
            side_panel: SidePanelState {
                mode: SidePanelMode::Changes,
                visible: true,
            },
            ..LayoutState::default()
        };
        s.recompute_for_size(40, 6);
        assert_eq!(s.mode, LayoutMode::Tiny);
        assert_eq!(s.focus, FocusPane::SidePanel);
        assert_eq!(s.side_panel.mode, SidePanelMode::Changes);
    }

    #[test]
    // FocusPane::next skips invisible panes and wraps.
    fn focus_next_skips_invisible_panes() {
        assert_eq!(
            FocusPane::Composer.next(false, false),
            FocusPane::Transcript
        );
        assert_eq!(
            FocusPane::Transcript.next(false, false),
            FocusPane::Composer
        );
        assert_eq!(FocusPane::Composer.next(true, false), FocusPane::Transcript);
        assert_eq!(
            FocusPane::Transcript.next(true, false),
            FocusPane::SidePanel
        );
        assert_eq!(FocusPane::SidePanel.next(true, false), FocusPane::Composer);
        assert_eq!(
            FocusPane::SidePanel.next(true, true),
            FocusPane::DetailPanel
        );
    }

    #[test]
    // FocusPane::prev cycles backwards, also skipping invisible panes.
    fn focus_prev_skips_invisible_panes() {
        assert_eq!(
            FocusPane::Composer.prev(false, false),
            FocusPane::Transcript
        );
        assert_eq!(FocusPane::Composer.prev(true, true), FocusPane::DetailPanel);
        assert_eq!(
            FocusPane::DetailPanel.prev(true, true),
            FocusPane::SidePanel
        );
        assert_eq!(FocusPane::Transcript.prev(true, true), FocusPane::Composer);
    }

    #[test]
    // SidePanelMode round-trips through its config key.
    fn side_panel_mode_config_key_roundtrips() {
        for mode in [
            SidePanelMode::Plan,
            SidePanelMode::Changes,
            SidePanelMode::Files,
            SidePanelMode::Tools,
            SidePanelMode::Sessions,
            SidePanelMode::Diagnostics,
            SidePanelMode::Subagents,
            SidePanelMode::Mcp,
            SidePanelMode::Plugins,
            SidePanelMode::Skills,
            SidePanelMode::Usage,
            SidePanelMode::Auth,
            SidePanelMode::Model,
            SidePanelMode::Provider,
        ] {
            let key = mode.config_key();
            assert_eq!(SidePanelMode::from_config_key(key), Some(mode));
        }
        assert_eq!(
            SidePanelMode::from_config_key("agents"),
            Some(SidePanelMode::Subagents)
        );
        assert_eq!(SidePanelMode::from_config_key("unknown"), None);
    }

    #[test]
    // Modal stack push/pop is LIFO; current_modal reflects the top.
    fn modal_stack_is_lifo() {
        let mut s = LayoutState::default();
        assert!(s.current_modal().is_none());
        s.push_modal(TuiModal::Help);
        s.push_modal(TuiModal::CommandPalette);
        assert!(matches!(s.current_modal(), Some(TuiModal::CommandPalette)));
        assert!(matches!(s.pop_modal(), Some(TuiModal::CommandPalette)));
        assert!(matches!(s.current_modal(), Some(TuiModal::Help)));
        assert!(matches!(s.pop_modal(), Some(TuiModal::Help)));
        assert!(s.pop_modal().is_none());
    }

    #[test]
    // The side/detail panels are only laid out under Full mode, even
    // if `visible` is true.
    fn responsive_mode_suppresses_panels() {
        let mut s = LayoutState::default();
        s.side_panel.visible = true;
        s.detail_panel.visible = true;
        s.mode = LayoutMode::Full;
        assert!(s.side_panel_rendered());
        assert!(s.detail_panel_rendered());
        s.mode = LayoutMode::Compact;
        assert!(!s.side_panel_rendered());
        assert!(!s.detail_panel_rendered());
        s.mode = LayoutMode::Tiny;
        assert!(!s.side_panel_rendered());
        assert!(!s.detail_panel_rendered());
    }
}
