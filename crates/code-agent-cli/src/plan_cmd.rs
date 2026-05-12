//! CLI surface for the M11 planning subsystem.
//!
//! All commands below operate on the `<cwd>/.agent/plans/` plan store.
//! They never talk to the model on their own; plan generation and
//! execution still go through the normal agent run path (`code-agent plan
//! <prompt>` / `code-agent act <id>`). The handlers in this module are
//! the lightweight verbs the user reaches for between those runs:
//! `list`, `show`, `approve`, `reject`, `revise`, `diff`, `archive`,
//! `export`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use code_agent_core::{
    new_plan_id, render_plan_pretty, FsPlanStore, PlanApprovalRequest, PlanApprovalScope,
    PlanApprover, PlanCreatedBy, PlanDeviationPolicy, PlanDocument, PlanError, PlanId,
    PlanListFilter, PlanRejectRequest, PlanStatus, PlanStore, PlanningMode, PlanningOptions,
    Settings,
};
use serde_json::Value;
use time::OffsetDateTime;

/// Subcommand tree for `code-agent plans ...`.
#[derive(Subcommand, Debug)]
pub enum PlansCommand {
    /// List known plans (optionally filtered by status).
    List(ListArgs),
    /// Show a structured summary of one plan.
    Show(ShowArgs),
    /// Approve a plan version so it can be acted on.
    Approve(ApproveArgs),
    /// Reject a pending plan.
    Reject(RejectArgs),
    /// Apply revision instructions to the latest plan (manual or
    /// model-assisted depending on flags).
    Revise(ReviseArgs),
    /// Open a plan for manual editing in `$EDITOR` (or print the path).
    Edit(EditArgs),
    /// Diff two versions of a plan.
    Diff(DiffArgs),
    /// Archive a plan (set status to `archived`).
    Archive(ArchiveArgs),
    /// Export a plan as JSON or Markdown.
    Export(ExportArgs),
}

impl PlansCommand {
    /// The workspace directory the chosen subcommand was scoped to. Used
    /// by the top-level dispatcher so settings, plan store paths, and
    /// CLI arguments all agree on the same project root. Without this
    /// the dispatcher fell back to "." for settings while the plan store
    /// honored the user's `--cwd`, producing two divergent views of the
    /// same project.
    pub fn cwd(&self) -> &Path {
        match self {
            Self::List(a) => &a.common.cwd,
            Self::Show(a) => &a.common.cwd,
            Self::Approve(a) => &a.common.cwd,
            Self::Reject(a) => &a.common.cwd,
            Self::Revise(a) => &a.common.cwd,
            Self::Edit(a) => &a.common.cwd,
            Self::Diff(a) => &a.common.cwd,
            Self::Archive(a) => &a.common.cwd,
            Self::Export(a) => &a.common.cwd,
        }
    }
}

#[derive(Args, Debug)]
pub struct CommonArgs {
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Filter by status: draft|pending|approved|executing|completed|failed|needs-replan|all.
    #[arg(long)]
    pub status: Option<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    /// `pretty` | `json` | `markdown`. Default: pretty.
    #[arg(long, default_value = "pretty")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct ApproveArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    #[arg(long)]
    pub notes: Option<String>,
}

#[derive(Args, Debug)]
pub struct RejectArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Args, Debug)]
pub struct ReviseArgs {
    pub plan_id: String,
    /// Free-form revision instructions stored on the plan's metadata.
    pub instructions: String,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Args, Debug)]
pub struct EditArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    /// Just print the path that would be opened in $EDITOR.
    #[arg(long)]
    pub print_path: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    #[arg(long)]
    pub from_version: Option<u32>,
    #[arg(long)]
    pub to_version: Option<u32>,
}

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    pub plan_id: String,
    #[command(flatten)]
    pub common: CommonArgs,
    /// `json` | `markdown`. Default: json.
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Output path. Default: stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// Construct an effective [`PlanningOptions`] from CLI args + project
/// settings. CLI flags win over settings.
pub fn planning_options_from_settings(settings: &Settings) -> PlanningOptions {
    let mut opts = PlanningOptions::default();
    if let Some(p) = &settings.planning {
        let _ = p.apply(&mut opts);
    }
    opts
}

fn plan_store(cwd: &Path, settings: &Settings) -> Arc<dyn PlanStore> {
    Arc::new(FsPlanStore::new(
        cwd,
        planning_options_from_settings(settings),
    ))
}

/// Resolve a plan-id argument that may be `latest`, a short id prefix, or
/// a full id. Returns the matching plan id.
pub async fn resolve_plan_id(store: &Arc<dyn PlanStore>, raw: &str) -> Result<PlanId, PlanError> {
    if raw == "latest" {
        let listing = store.list(PlanListFilter::default()).await?;
        return listing
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| PlanError::NotFound("latest".into()));
    }
    // Try exact match first.
    let candidate = PlanId(raw.to_string());
    if candidate.is_valid_format() && store.load(&candidate).await.is_ok() {
        return Ok(candidate);
    }
    // Otherwise, prefix-match in the listing (helpful for short hex ids).
    let listing = store.list(PlanListFilter::default()).await?;
    let matches: Vec<_> = listing
        .iter()
        .filter(|s| s.id.as_str().starts_with(raw))
        .collect();
    match matches.len() {
        0 => Err(PlanError::NotFound(raw.to_string())),
        1 => Ok(matches[0].id.clone()),
        _ => Err(PlanError::Other(format!(
            "ambiguous plan id `{raw}`: {} matches",
            matches.len()
        ))),
    }
}

/// Dispatch a `plans` subcommand.
pub async fn cmd_plans(settings: &Settings, cmd: PlansCommand) -> Result<()> {
    match cmd {
        PlansCommand::List(a) => cmd_plans_list(settings, a).await,
        PlansCommand::Show(a) => cmd_plans_show(settings, a).await,
        PlansCommand::Approve(a) => cmd_plans_approve(settings, a).await,
        PlansCommand::Reject(a) => cmd_plans_reject(settings, a).await,
        PlansCommand::Revise(a) => cmd_plans_revise(settings, a).await,
        PlansCommand::Edit(a) => cmd_plans_edit(settings, a).await,
        PlansCommand::Diff(a) => cmd_plans_diff(settings, a).await,
        PlansCommand::Archive(a) => cmd_plans_archive(settings, a).await,
        PlansCommand::Export(a) => cmd_plans_export(settings, a).await,
    }
}

async fn cmd_plans_list(settings: &Settings, args: ListArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let status = match args.status.as_deref() {
        None | Some("all") | Some("") => None,
        Some(s) => {
            Some(PlanStatus::from_str_lenient(s).ok_or_else(|| anyhow!("unknown status: {s}"))?)
        }
    };
    let summaries = store
        .list(PlanListFilter {
            status,
            session_id: None,
            limit: None,
        })
        .await?;
    if args.json {
        let s = serde_json::to_string_pretty(&summaries)?;
        println!("{s}");
        return Ok(());
    }
    if summaries.is_empty() {
        println!("(no plans)");
        return Ok(());
    }
    for s in &summaries {
        println!(
            "{}  v{}  {:<12} steps={}  {}",
            s.id,
            s.version,
            s.status.label(),
            s.step_count,
            s.title
        );
    }
    Ok(())
}

async fn cmd_plans_show(settings: &Settings, args: ShowArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let plan = store.load(&id).await?;
    match args.format.as_str() {
        "json" => {
            let s = serde_json::to_string_pretty(&plan)?;
            println!("{s}");
        }
        "markdown" => println!("{}", render_plan_markdown(&plan)),
        _ => print!("{}", render_plan_pretty(&plan)),
    }
    Ok(())
}

async fn cmd_plans_approve(settings: &Settings, args: ApproveArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let plan = store.load(&id).await?;
    let approved = store
        .approve(PlanApprovalRequest {
            plan_id: id,
            approved_by: PlanApprover::CliFlag,
            approved_version: plan.version,
            scope: PlanApprovalScope::FullPlan,
            notes: args.notes,
        })
        .await?;
    println!("[plan] approved {} v{}", approved.id, approved.version);
    Ok(())
}

async fn cmd_plans_reject(settings: &Settings, args: RejectArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let rejected = store
        .reject(PlanRejectRequest {
            plan_id: id,
            reason: args.reason,
        })
        .await?;
    println!("[plan] rejected {} v{}", rejected.id, rejected.version);
    Ok(())
}

async fn cmd_plans_revise(settings: &Settings, args: ReviseArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let mut plan = store.load(&id).await?;
    plan.version = plan
        .version
        .checked_add(1)
        .ok_or_else(|| anyhow!("plan version overflow"))?;
    plan.status = PlanStatus::PendingApproval;
    plan.approval = None;
    plan.created_by = PlanCreatedBy::RevisedByUser;
    let map = plan.metadata.as_object_mut();
    match map {
        Some(m) => {
            m.insert(
                "revision_instructions".into(),
                Value::String(args.instructions),
            );
        }
        None => {
            plan.metadata = serde_json::json!({
                "revision_instructions": args.instructions
            });
        }
    }
    plan.updated_at = OffsetDateTime::now_utc();
    store.save_revision(plan.clone()).await?;
    println!("[plan] revised {} -> v{}", plan.id, plan.version);
    Ok(())
}

async fn cmd_plans_edit(settings: &Settings, args: EditArgs) -> Result<()> {
    // Use the concrete `FsPlanStore` here (not the trait object returned
    // by `plan_store`) so we can call `save_revision_atomic`, which
    // holds the per-plan lock across load + validate + save and closes
    // the TOCTOU between `validate_edited_plan` and the persisted save.
    let store = Arc::new(FsPlanStore::new(
        &args.common.cwd,
        planning_options_from_settings(settings),
    ));
    let trait_store: Arc<dyn PlanStore> = store.clone();
    let id = resolve_plan_id(&trait_store, &args.plan_id).await?;
    let plan = store.load(&id).await?;
    let live_path = args
        .common
        .cwd
        .join(".agent")
        .join("plans")
        .join(format!("{}.json", plan.id));
    if args.print_path || std::env::var_os("EDITOR").is_none() {
        println!("{}", live_path.display());
        return Ok(());
    }
    let editor = std::env::var("EDITOR").context("$EDITOR not set")?;

    // Edit a temporary copy, NOT the live plan file. If the user saves
    // invalid JSON or aborts, the on-disk plan store is untouched. The
    // tempfile lives in the same directory so atomic-rename semantics
    // apply when we eventually persist via `save_revision`.
    let plans_dir = args.common.cwd.join(".agent").join("plans");
    std::fs::create_dir_all(&plans_dir)
        .with_context(|| format!("create {}", plans_dir.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("{}.edit-", plan.id))
        .suffix(".json")
        .tempfile_in(&plans_dir)
        .with_context(|| format!("create tempfile in {}", plans_dir.display()))?;
    let original_bytes =
        std::fs::read(&live_path).with_context(|| format!("read {}", live_path.display()))?;
    use std::io::Write as _;
    tmp.write_all(&original_bytes)
        .context("seed tempfile from live plan")?;
    tmp.flush().ok();
    let tmp_path = tmp.path().to_path_buf();

    let status = std::process::Command::new(&editor)
        .arg(&tmp_path)
        .status()
        .context("failed to launch $EDITOR")?;
    if !status.success() {
        return Err(anyhow!(
            "editor exited with {:?}; live plan untouched",
            status.code()
        ));
    }
    let edited_bytes = std::fs::read(&tmp_path).context("re-reading edited plan")?;
    let edited: PlanDocument = serde_json::from_slice(&edited_bytes)
        .context("edited plan is not valid JSON; live plan untouched")?;
    if edited.id != plan.id {
        return Err(anyhow!(
            "plan id may not be changed during edit; live plan untouched"
        ));
    }
    // Drop the tempfile guard explicitly so it's removed before we persist.
    drop(tmp);
    let (saved_id, saved_version) = (edited.id.clone(), edited.version);
    // Atomic load+validate+save inside the per-plan lock. Without this,
    // a concurrent writer could mutate the on-disk plan between the
    // validator's read and our save, letting the edit silently overwrite
    // a state the user never reviewed.
    store
        .save_revision_atomic(edited, |current, edited| {
            validate_edited_plan(current, edited).map_err(|e| PlanError::Invalid(format!("{e:#}")))
        })
        .await
        .context("rejected manual edit; live plan untouched")?;
    println!("[plan] saved manual edit {} v{}", saved_id, saved_version);
    Ok(())
}

/// Reject manual-edit changes that bypass plan-store invariants:
///
/// - `version` may not be decremented.
/// - `status` must be preserved by an edit. Status transitions belong to
///   approve/reject/run flows.
/// - `approval` may not be introduced or mutated by an edit. Only
///   `plans approve` may attach an approval block.
///
/// Non-mutating edits (steps, summary, etc.) are allowed; `save_revision`
/// still performs its own approval-invalidation pass on body drift.
pub fn validate_edited_plan(original: &PlanDocument, edited: &PlanDocument) -> Result<()> {
    if edited.version < original.version {
        return Err(anyhow!(
            "field `version`: cannot downgrade plan version (was {}, edited to {}); \
             revisions must increment, not decrement",
            original.version,
            edited.version
        ));
    }
    if edited.status != original.status {
        return Err(anyhow!(
            "field `status`: cannot change plan status via edit \
             (was {:?}, edited to {:?}); use `plans approve`, `plans reject`, or \
             `act` to transition status",
            original.status,
            edited.status
        ));
    }
    // `approval` may stay non-null only if it is byte-for-byte identical
    // to the original. Comparing serialized form sidesteps having to add
    // `PartialEq` to `PlanApproval`/`OffsetDateTime`.
    let approval_changed = match (&original.approval, &edited.approval) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            let a_json = serde_json::to_value(a).unwrap_or(Value::Null);
            let b_json = serde_json::to_value(b).unwrap_or(Value::Null);
            a_json != b_json
        }
        // Either side flipped between None/Some — that's an approval mutation.
        _ => true,
    };
    if approval_changed {
        return Err(anyhow!(
            "field `approval`: cannot set or mutate approval via edit; \
             use `plans approve` instead"
        ));
    }
    Ok(())
}

async fn cmd_plans_diff(settings: &Settings, args: DiffArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let plan = store.load(&id).await?;
    let to_v = args.to_version.unwrap_or(plan.version);
    let from_v = args
        .from_version
        .unwrap_or_else(|| to_v.saturating_sub(1).max(1));
    let dir = args.common.cwd.join(".agent").join("plans");
    // Prefer the write-once `.approved.json` snapshot for either side when
    // it exists. That preserves the body the reviewer approved even after
    // the act driver has overwritten `.v<n>.json` with post-execution state.
    let pick_snapshot = |v: u32| -> std::path::PathBuf {
        let approved = dir.join(format!("{}.v{}.approved.json", plan.id, v));
        if approved.exists() {
            approved
        } else {
            dir.join(format!("{}.v{}.json", plan.id, v))
        }
    };
    let from_path = pick_snapshot(from_v);
    let to_path = pick_snapshot(to_v);
    let from_bytes = std::fs::read_to_string(&from_path)
        .with_context(|| format!("read {}", from_path.display()))?;
    let to_bytes =
        std::fs::read_to_string(&to_path).with_context(|| format!("read {}", to_path.display()))?;
    let diff = similar::TextDiff::from_lines(&from_bytes, &to_bytes);
    let mut printed = 0u32;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Equal => continue,
            similar::ChangeTag::Delete => print!("- {}", change),
            similar::ChangeTag::Insert => print!("+ {}", change),
        }
        printed += 1;
    }
    if printed == 0 {
        println!("(no differences between v{from_v} and v{to_v})");
    }
    Ok(())
}

async fn cmd_plans_archive(settings: &Settings, args: ArchiveArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    store.archive(&id).await?;
    println!("[plan] archived {}", id);
    Ok(())
}

async fn cmd_plans_export(settings: &Settings, args: ExportArgs) -> Result<()> {
    let store = plan_store(&args.common.cwd, settings);
    let id = resolve_plan_id(&store, &args.plan_id).await?;
    let plan = store.load(&id).await?;
    let body = match args.format.as_str() {
        "markdown" | "md" => render_plan_markdown(&plan),
        _ => serde_json::to_string_pretty(&plan)?,
    };
    match args.output {
        Some(p) => std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?,
        None => print!("{body}"),
    }
    Ok(())
}

/// Render a plan as a Markdown document, suitable for export.
pub fn render_plan_markdown(plan: &PlanDocument) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {}\n\n", plan.title));
    s.push_str(&format!(
        "- **Plan id:** `{}`\n- **Version:** {}\n- **Status:** {}\n",
        plan.id,
        plan.version,
        plan.status.label()
    ));
    if let Some(m) = &plan.model {
        s.push_str(&format!("- **Model:** {m}\n"));
    }
    s.push('\n');
    if !plan.goal.is_empty() {
        s.push_str("## Goal\n\n");
        s.push_str(&plan.goal);
        s.push_str("\n\n");
    }
    if !plan.summary.is_empty() {
        s.push_str("## Summary\n\n");
        s.push_str(&plan.summary);
        s.push_str("\n\n");
    }
    s.push_str("## Steps\n\n");
    for (i, step) in plan.steps.iter().enumerate() {
        s.push_str(&format!(
            "{}. **{}** — {} ({:?}, risk={:?})\n",
            i + 1,
            step.title,
            step.description,
            step.kind,
            step.risk
        ));
        for f in &step.expected_files {
            s.push_str(&format!(
                "   - file: `{}` ({:?})\n",
                f.path.as_str(),
                f.access
            ));
        }
        for c in &step.expected_commands {
            s.push_str(&format!("   - run: `{}`\n", c.command));
        }
    }
    if !plan.validations.is_empty() {
        s.push_str("\n## Validation\n\n");
        for v in &plan.validations {
            match &v.command {
                Some(cmd) => s.push_str(&format!("- `{}` — {}\n", cmd, v.description)),
                None => s.push_str(&format!("- {}\n", v.description)),
            }
        }
    }
    if !plan.risks.is_empty() {
        s.push_str("\n## Risks\n\n");
        for r in &plan.risks {
            s.push_str(&format!("- **{:?}:** {}\n", r.level, r.description));
        }
    }
    s
}

/// Stable summary for `code-agent status` and the TUI.
pub async fn latest_plan_summary(cwd: &Path, settings: &Settings) -> Result<Vec<String>> {
    let store = plan_store(cwd, settings);
    let listing = store
        .list(PlanListFilter {
            status: None,
            session_id: None,
            limit: Some(5),
        })
        .await?;
    let mut out = Vec::new();
    out.push("Plans".to_string());
    if listing.is_empty() {
        out.push("  (no plans yet — try `code-agent plan <prompt>`)".into());
    } else {
        for s in &listing {
            out.push(format!(
                "  {}  v{}  {:<12} {} ({} steps)",
                s.id,
                s.version,
                s.status.label(),
                s.title,
                s.step_count
            ));
        }
    }
    Ok(out)
}

/// Doctor check: plan store writability and stale executing plans.
pub async fn plan_store_doctor(
    cwd: &Path,
    settings: &Settings,
) -> Vec<(String, bool, String, Option<String>)> {
    let store = plan_store(cwd, settings);
    let mut checks = Vec::new();
    let writable = store.writable().await;
    checks.push((
        "planning.store_writable".to_string(),
        writable,
        if writable {
            "plan store writable".into()
        } else {
            "plan store not writable".into()
        },
        if writable {
            None
        } else {
            Some(format!(
                "ensure {} is writable",
                cwd.join(".agent").join("plans").display()
            ))
        },
    ));
    if let Ok(listing) = store.list(PlanListFilter::default()).await {
        let stale: Vec<_> = listing
            .iter()
            .filter(|s| s.status == PlanStatus::Executing)
            .collect();
        checks.push((
            "planning.stale_executing".to_string(),
            stale.is_empty(),
            if stale.is_empty() {
                "no stale executing plans".into()
            } else {
                format!("{} executing plan(s) (possibly interrupted)", stale.len())
            },
            if stale.is_empty() {
                None
            } else {
                Some(format!(
                    "inspect with `code-agent plans show {}`",
                    stale[0].id
                ))
            },
        ));
    }
    checks
}

/// Build a printable mini-summary of how scope enforcement maps to the
/// active plan, for `/plan show` and `--strict-plan` debugging.
pub fn render_scope_summary(plan: &PlanDocument, opts: &PlanningOptions) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("Plan id:        {}", plan.id));
    out.push(format!("Plan version:   {}", plan.version));
    out.push(format!("Plan status:    {}", plan.status.label()));
    out.push(format!(
        "Enforce scope:  {}",
        if opts.enforce_plan_scope { "yes" } else { "no" }
    ));
    out.push(format!(
        "Deviation:      {}",
        match opts.deviation_policy {
            PlanDeviationPolicy::Deny => "deny",
            PlanDeviationPolicy::Ask => "ask",
            PlanDeviationPolicy::RequireReplan => "require_replan",
            PlanDeviationPolicy::AllowWithWarning => "allow_with_warning",
        }
    ));
    out.push(format!(
        "Planning mode:  {}",
        match opts.planning_mode {
            PlanningMode::NoTools => "no_tools",
            PlanningMode::ReadOnlyWorkspace => "read_only_workspace",
            PlanningMode::ReadOnlyWithApprovedExternal => "read_only_with_approved_external",
        }
    ));
    out
}

/// Convenience: produce a brand-new plan id for the planner driver.
pub fn fresh_plan_id() -> PlanId {
    new_plan_id()
}

#[derive(Args, Debug, Clone)]
pub struct PlanArgs {
    /// The user request to draft a plan for.
    pub prompt: String,
    /// Plan generation mode override: no-tools|read-only|read-only-external.
    #[arg(long)]
    pub mode: Option<String>,
    /// Cap the number of steps the planner may produce.
    #[arg(long)]
    pub max_steps: Option<usize>,
    /// Override the provider used for planning.
    #[arg(long)]
    pub provider: Option<String>,
    /// Override the model used for planning.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ActArgs {
    /// Plan id or `latest`.
    pub plan_id: String,
    /// Permission mode for the act run.
    #[arg(long)]
    pub permission_mode: Option<String>,
    /// Reject any deviation, regardless of policy.
    #[arg(long)]
    pub strict_plan: bool,
    /// Don't enforce plan scope (still records deviations).
    #[arg(long)]
    pub no_enforce_plan_scope: bool,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ReplanArgs {
    /// Plan id or `latest`. May be omitted for the most-recent plan.
    pub plan_id: Option<String>,
    /// Free-form instructions for the re-planner.
    pub instructions: Option<String>,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
}

/// Apply CLI overrides to the planning options block.
pub fn apply_plan_args_to_options(args: &PlanArgs, opts: &mut PlanningOptions) -> Result<()> {
    if let Some(s) = &args.mode {
        opts.planning_mode =
            PlanningMode::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --mode: {s}"))?;
    }
    if let Some(n) = args.max_steps {
        opts.max_plan_steps = n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::PlanApproval;
    use time::OffsetDateTime;

    fn base_plan() -> PlanDocument {
        let mut p = PlanDocument::new_draft(new_plan_id(), "T", "u");
        p.version = 3;
        p.status = PlanStatus::PendingApproval;
        p
    }

    #[test]
    // validate_edited_plan rejects a version downgrade and names the
    // offending field in the error message. Pins #79: pre-fix the
    // editor flow handed the lowered version straight to
    // `save_revision`, which happily wrote it back.
    fn edit_rejects_version_downgrade() {
        let original = base_plan();
        let mut edited = original.clone();
        edited.version = 2;
        let err = validate_edited_plan(&original, &edited).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`version`"),
            "error should name the version field, got: {msg}"
        );
    }

    #[test]
    // validate_edited_plan accepts a same-version edit, including body
    // changes (steps, summary). `save_revision` still applies its own
    // approval-invalidation pass on body drift.
    fn edit_allows_same_version_body_change() {
        let original = base_plan();
        let mut edited = original.clone();
        edited.summary = "new summary".into();
        validate_edited_plan(&original, &edited).expect("body-only edit must succeed");
    }

    #[test]
    // validate_edited_plan accepts a version increment, which is how a
    // legitimate revision (created by the act driver or `plans revise`)
    // would appear when the user edits the latest snapshot.
    fn edit_allows_version_increment() {
        let original = base_plan();
        let mut edited = original.clone();
        edited.version = original.version + 1;
        validate_edited_plan(&original, &edited).expect("version increment must succeed");
    }

    #[test]
    // validate_edited_plan rejects a direct status flip to `Approved`
    // and names the offending field. Status transitions must go through
    // approve/reject/run flows.
    fn edit_rejects_status_change() {
        let original = base_plan();
        let mut edited = original.clone();
        edited.status = PlanStatus::Approved;
        let err = validate_edited_plan(&original, &edited).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`status`"),
            "error should name the status field, got: {msg}"
        );
    }

    #[test]
    // validate_edited_plan rejects attaching an `approval` block via
    // edit even when the user keeps version and status fixed. Approval
    // is the integrity-critical record `plans approve` is supposed to
    // own.
    fn edit_rejects_introducing_approval() {
        let original = base_plan();
        let mut edited = original.clone();
        edited.approval = Some(PlanApproval {
            approved_at: OffsetDateTime::now_utc(),
            approved_by: PlanApprover::CliFlag,
            approved_version: original.version,
            notes: Some("forged".into()),
            scope: PlanApprovalScope::FullPlan,
        });
        let err = validate_edited_plan(&original, &edited).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`approval`"),
            "error should name the approval field, got: {msg}"
        );
    }

    #[test]
    // validate_edited_plan rejects mutating an existing `approval`
    // block — e.g. changing `notes` or `approved_by` — without going
    // through the approve flow.
    fn edit_rejects_mutating_existing_approval() {
        let mut original = base_plan();
        original.status = PlanStatus::Approved;
        let now = OffsetDateTime::now_utc();
        original.approval = Some(PlanApproval {
            approved_at: now,
            approved_by: PlanApprover::CliFlag,
            approved_version: original.version,
            notes: Some("real".into()),
            scope: PlanApprovalScope::FullPlan,
        });
        let mut edited = original.clone();
        edited.approval = Some(PlanApproval {
            approved_at: now,
            approved_by: PlanApprover::CliFlag,
            approved_version: original.version,
            notes: Some("tampered".into()),
            scope: PlanApprovalScope::FullPlan,
        });
        let err = validate_edited_plan(&original, &edited).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`approval`"),
            "error should name the approval field, got: {msg}"
        );
    }

    #[test]
    // validate_edited_plan accepts a preserved approval block (same
    // bytes round-tripped through serde) so the no-op case is allowed.
    fn edit_allows_preserved_approval() {
        let mut original = base_plan();
        original.status = PlanStatus::Approved;
        original.approval = Some(PlanApproval {
            approved_at: OffsetDateTime::now_utc(),
            approved_by: PlanApprover::CliFlag,
            approved_version: original.version,
            notes: Some("real".into()),
            scope: PlanApprovalScope::FullPlan,
        });
        // Round-trip through JSON to mimic the editor save path.
        let json = serde_json::to_string(&original).unwrap();
        let edited: PlanDocument = serde_json::from_str(&json).unwrap();
        validate_edited_plan(&original, &edited).expect("identical approval must be allowed");
    }
}
