use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use code_agent_anthropic::{AnthropicConfig, AnthropicProvider};
use code_agent_core::agent::{SessionSink, TelemetrySink};
use code_agent_core::{
    AgentClient, AgentEvent, AgentOptions, AgentTelemetryEvent, ApprovalMemoryMode, AuditLogHook,
    BlockDangerousBashHook, BlockEnvFileWritesHook, HookOutcomeKind, Hooks, MaxPatchSizeHook,
    ModelProvider, PermissionCallback, PermissionDecision, PermissionMode, PermissionRequest,
    ResultSubtype, SecretRedactionHook, SessionId, SessionStart, SettingSource, Settings,
    ToolRegistry, ToolRule,
};
use code_agent_session::{
    append_settings_snapshot, append_user_prompt, export_session, fork_session,
    replay_conversation, summarize_session, ExportFormat, FsSessionStore, SessionSinkAdapter,
    SessionStore,
};
use code_agent_tools::{register_builtin_with_limits, ToolLimits};
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

#[derive(Parser, Debug)]
#[command(name = "code-agent", version, about = "Headless coding agent (M2)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a single coding task end-to-end.
    Run(RunArgs),
    /// Resume a session by id, sending an additional prompt.
    Resume(ResumeArgs),
    /// Continue the latest session for cwd, sending an additional prompt.
    Continue(ContinueArgs),
    /// Fork a session by id into a new session and send a prompt.
    Fork(ForkArgs),
    /// Live interactive chat with multiple prompts in one process.
    Chat(ChatArgs),
    /// Manage sessions.
    #[command(subcommand)]
    Sessions(SessionsCommand),
}

#[derive(Parser, Debug)]
struct CommonArgs {
    /// Workspace directory; tools are sandboxed to this path.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Anthropic model id. When omitted, settings.model wins; otherwise
    /// falls back to the built-in default `claude-sonnet-4-6`.
    #[arg(long)]
    model: Option<String>,
    /// Comma-separated list of tools to expose by name.
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Vec<String>,
    /// Permission mode: default, acceptEdits, plan, dontAsk. When omitted,
    /// settings.permission_mode wins; otherwise falls back to `default`.
    #[arg(long)]
    permission_mode: Option<String>,
    /// Maximum agent turns before bailing out. When omitted, settings
    /// or the built-in default 8 wins.
    #[arg(long)]
    max_turns: Option<u32>,
    /// Settings sources to load: none|project|user|user-and-project.
    #[arg(long, default_value = "user-and-project")]
    settings: String,
    /// Path to an explicit settings file (overrides --settings).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Disable session writing.
    #[arg(long)]
    no_session: bool,
    /// Pretty | json log output.
    #[arg(long, default_value = "pretty")]
    log_format: String,
    /// Show usage summary at end of run.
    #[arg(long)]
    show_usage: bool,
    /// Quiet: suppress non-essential output.
    #[arg(long)]
    quiet: bool,
    /// Verbose: emit telemetry to stderr.
    #[arg(long)]
    verbose: bool,
    /// Allow rule overrides (repeatable). Format: `Tool` or `Bash:^cargo test$`.
    #[arg(long = "yes-to")]
    yes_to: Vec<String>,
    /// Disallow rule overrides (repeatable). Format: `Tool` or `Edit:**/.env*`.
    #[arg(long = "deny")]
    deny: Vec<String>,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Initial user prompt.
    prompt: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug)]
struct ResumeArgs {
    /// Session id to resume.
    session_id: String,
    /// New prompt to append to the session.
    prompt: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug)]
struct ContinueArgs {
    /// New prompt for the latest session.
    prompt: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug)]
struct ForkArgs {
    /// Source session id to fork.
    session_id: String,
    /// New prompt to send to the forked session.
    prompt: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug)]
struct ChatArgs {
    /// Initial prompt; if omitted, read from stdin.
    prompt: Option<String>,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// List sessions for a workspace.
    List {
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Show a structured summary for a session.
    Show {
        session_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Export a session as JSONL or JSON.
    Export {
        session_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long, default_value = "jsonl")]
        format: String,
    },
}

fn parse_permission_mode(s: &str) -> Result<PermissionMode> {
    PermissionMode::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --permission-mode: {s}"))
}

fn parse_setting_source(s: &str) -> Result<SettingSource> {
    SettingSource::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --settings: {s}"))
}

fn parse_log_format(s: &str) -> Result<LogFormat> {
    match s.to_ascii_lowercase().as_str() {
        "pretty" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        other => Err(anyhow!("invalid --log-format: {other}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Pretty,
    Json,
}

fn parse_cli_rule(s: &str) -> Result<ToolRule> {
    if let Some((tool, rest)) = s.split_once(':') {
        if tool == "Bash" {
            return Ok(ToolRule::BashCommandRegex(rest.to_string()));
        }
        return Ok(ToolRule::FilePathGlob {
            tool: tool.to_string(),
            pattern: rest.to_string(),
        });
    }
    if s.contains('*') || s.contains('?') {
        Ok(ToolRule::ToolGlob(s.to_string()))
    } else {
        Ok(ToolRule::ExactTool(s.to_string()))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = match cli.command {
        Command::Run(args) => cmd_run(args).await,
        Command::Resume(args) => cmd_resume(args).await,
        Command::Continue(args) => cmd_continue(args).await,
        Command::Fork(args) => cmd_fork(args).await,
        Command::Chat(args) => cmd_chat(args).await,
        Command::Sessions(SessionsCommand::List { cwd }) => cmd_sessions_list(cwd).await,
        Command::Sessions(SessionsCommand::Show { session_id, cwd }) => {
            cmd_sessions_show(session_id, cwd).await
        }
        Command::Sessions(SessionsCommand::Export {
            session_id,
            cwd,
            format,
        }) => cmd_sessions_export(session_id, cwd, format).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

struct ResolvedRunConfig {
    cwd: PathBuf,
    options: AgentOptions,
    tools: ToolRegistry,
    hooks: Hooks,
    log_format: LogFormat,
    show_usage: bool,
    quiet: bool,
    verbose: bool,
    settings: Settings,
    settings_summary: String,
}

async fn resolve_common(
    common: &CommonArgs,
    prompt_for_callback: bool,
) -> Result<ResolvedRunConfig> {
    let cwd = common.cwd.canonicalize().context("--cwd")?;
    let source = parse_setting_source(&common.settings)?;
    let log_format = parse_log_format(&common.log_format)?;

    // Load file settings.
    let mut settings = if let Some(p) = &common.config {
        // Explicit settings file overrides everything.
        let bytes = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
        let file: code_agent_core::SettingsFile =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))?;
        let mut s = Settings {
            source: SettingSource::None,
            project_path: Some(p.clone()),
            ..Settings::default()
        };
        // Merge using a borrowed helper from the settings module: easiest is
        // to pass through SettingsFile via a temporary write/read; or just
        // deserialize directly. We re-implement the same merge logic here.
        if let Some(m) = file.model {
            s.model = Some(m);
        }
        if let Some(pm) = file.permission_mode {
            s.permission_mode = Some(
                PermissionMode::from_str_lenient(&pm)
                    .ok_or_else(|| anyhow!("invalid permission_mode in --config: {pm}"))?,
            );
        }
        if let Some(rules) = file.allowed_tools {
            s.allowed_tools = rules;
        }
        if let Some(rules) = file.disallowed_tools {
            s.disallowed_tools = rules;
        }
        if let Some(lim) = file.tool_limits {
            s.tool_limits = lim;
        }
        if let Some(h) = file.hooks {
            s.hooks = h;
        }
        if let Some(t) = file.max_turns {
            s.max_turns = Some(t);
        }
        if let Some(t) = file.max_tokens {
            s.max_tokens = Some(t);
        }
        if let Some(m) = file.project_instructions_mode {
            s.project_instructions_mode = m;
        }
        s
    } else {
        code_agent_core::settings::load(&cwd, source).context("load settings")?
    };

    // Apply CLI rule overrides.
    for rule_str in &common.yes_to {
        settings.allowed_tools.push(parse_cli_rule(rule_str)?);
    }
    for rule_str in &common.deny {
        settings.disallowed_tools.push(parse_cli_rule(rule_str)?);
    }

    // Build effective tool limits.
    let mut limits = ToolLimits::default();
    if let Some(v) = settings.tool_limits.max_read_bytes {
        limits.max_read_bytes = v;
    }
    if let Some(v) = settings.tool_limits.max_output_bytes {
        limits.max_output_bytes = v;
    }
    if let Some(v) = settings.tool_limits.bash_timeout_seconds {
        limits.bash_timeout = Duration::from_secs(v);
    }
    if let Some(v) = settings.tool_limits.max_glob_results {
        limits.max_glob_results = v;
    }
    if let Some(v) = settings.tool_limits.max_grep_results {
        limits.max_grep_results = v;
    }

    let mut tools = ToolRegistry::new();
    register_builtin_with_limits(&mut tools, limits);

    // Determine permission mode: explicit CLI flag wins over settings.
    let mode = if let Some(pm) = &common.permission_mode {
        parse_permission_mode(pm)?
    } else if let Some(pm) = settings.permission_mode {
        pm
    } else {
        PermissionMode::Default
    };

    // Determine model: explicit CLI flag wins over settings.
    let model = if let Some(m) = &common.model {
        m.clone()
    } else if let Some(m) = &settings.model {
        m.clone()
    } else {
        "claude-sonnet-4-6".to_string()
    };

    // Determine allowed tool names: CLI overrides settings if non-empty.
    //
    // In `dontAsk` mode the simple-name list acts as an implicit allowlist
    // (M1 contract). If the user did not explicitly set `--allowed-tools`,
    // we MUST NOT auto-fill the default 7 names there, because that would
    // silently widen permissions past whatever structured rules say. So in
    // dontAsk we only honor explicit `--allowed-tools` and leave the list
    // empty otherwise — the policy engine then evaluates structured rules
    // alone.
    let allowed_tools = if !common.allowed_tools.is_empty() {
        common.allowed_tools.clone()
    } else if matches!(mode, PermissionMode::DontAsk) {
        Vec::new()
    } else {
        let mut out: Vec<String> = vec![
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
            "Edit".into(),
            "Write".into(),
            "Bash".into(),
            "TodoWrite".into(),
        ];
        for r in &settings.allowed_tools {
            if let ToolRule::ExactTool(n) = r {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        out
    };

    // Project instructions.
    let mut instructions = load_agents_md(&cwd).await;
    if matches!(
        settings.project_instructions_mode,
        code_agent_core::ProjectInstructionsMode::AgentsAndClaudeMd
    ) {
        if let Ok(extra) = tokio::fs::read_to_string(cwd.join("CLAUDE.md")).await {
            let mut combined = instructions.unwrap_or_default();
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&extra);
            instructions = Some(combined);
        }
    }

    let max_turns = common.max_turns.or(settings.max_turns).unwrap_or(8);
    let max_tokens = settings.max_tokens.unwrap_or(4096);

    let permission_callback: Option<Arc<dyn PermissionCallback>> = if prompt_for_callback {
        Some(Arc::new(StdinApprover::new(
            mode,
            settings.disallowed_tools.clone(),
            settings.allowed_tools.clone(),
        )))
    } else {
        None
    };

    let options = AgentOptions {
        cwd: cwd.clone(),
        model: model.clone(),
        allowed_tools,
        allowed_tool_rules: settings.allowed_tools.clone(),
        disallowed_tool_rules: settings.disallowed_tools.clone(),
        permission_mode: mode,
        max_turns: Some(max_turns),
        session_id: None,
        permission_callback,
        system_prompt: None,
        project_instructions: instructions,
        max_tokens,
        approval_memory: ApprovalMemoryMode::Session,
    };

    // Build hooks from settings flags.
    let mut hooks = Hooks::new();
    if settings.hooks.block_env_file_writes.unwrap_or(true) {
        hooks = hooks
            .pre_tool_use("Edit|Write", Arc::new(BlockEnvFileWritesHook::default()))
            .expect("built-in pattern is always valid");
    }
    if settings.hooks.block_dangerous_bash.unwrap_or(true) {
        hooks = hooks
            .pre_tool_use("Bash", Arc::new(BlockDangerousBashHook::default()))
            .expect("built-in pattern is always valid");
    }
    if let Some(max_bytes) = settings.hooks.max_patch_size_bytes {
        hooks = hooks
            .pre_tool_use("Edit|Write", Arc::new(MaxPatchSizeHook::new(max_bytes)))
            .expect("built-in pattern is always valid");
    }
    // Audit-log every successful and failed tool call into an in-memory log
    // surfaced through `HookEvaluated` events.
    if settings.hooks.audit_log.unwrap_or(false) {
        let audit = Arc::new(AuditLogHook::default());
        hooks = hooks
            .post_tool_use("*", audit.clone())
            .expect("built-in pattern is always valid");
        hooks = hooks
            .post_tool_use_failure("*", audit)
            .expect("built-in pattern is always valid");
    }
    // Wire SecretRedactionHook so the toggle is honored. The hook itself
    // returns Continue and is informational; the real redaction is applied
    // by the audit-log hook to its own entries (see `redact_secrets_in_value`).
    if settings.hooks.secret_redaction.unwrap_or(true) {
        hooks = hooks
            .post_tool_use("*", Arc::new(SecretRedactionHook))
            .expect("built-in pattern is always valid");
    }

    let settings_summary = build_settings_summary(&settings, &model, mode);

    Ok(ResolvedRunConfig {
        cwd,
        options,
        tools,
        hooks,
        log_format,
        show_usage: common.show_usage,
        quiet: common.quiet,
        verbose: common.verbose,
        settings,
        settings_summary,
    })
}

fn build_settings_summary(s: &Settings, model: &str, mode: PermissionMode) -> String {
    serde_json::json!({
        "model": model,
        "permission_mode": format!("{:?}", mode),
        "allowed_tools_count": s.allowed_tools.len(),
        "disallowed_tools_count": s.disallowed_tools.len(),
        "settings_source": format!("{:?}", s.source),
        "project_path": s.project_path,
        "user_path": s.user_path,
    })
    .to_string()
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    run_one_shot(cfg, args.common.no_session, args.prompt, None, None).await
}

async fn cmd_resume(args: ResumeArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    let session_id = SessionId::from(args.session_id.clone());
    // Verify the session exists and is loadable BEFORE running, regardless
    // of `--no-session`. Even read-only resume requires the session file to
    // exist; otherwise we'd silently start fresh on a typo'd id, a real
    // resume-semantics regression.
    let store = FsSessionStore::new(cfg.cwd.clone());
    store
        .load(&session_id)
        .await
        .with_context(|| format!("session {} not found", session_id))?;
    run_one_shot(
        cfg,
        args.common.no_session,
        args.prompt,
        Some(session_id),
        None,
    )
    .await
}

async fn cmd_continue(args: ContinueArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cfg.cwd.clone()));
    let latest = store.latest_for_cwd(&cfg.cwd).await?;
    let session_id =
        latest.ok_or_else(|| anyhow!("no prior session found for cwd {}", cfg.cwd.display()))?;
    run_one_shot(
        cfg,
        args.common.no_session,
        args.prompt,
        Some(session_id),
        None,
    )
    .await
}

async fn cmd_fork(args: ForkArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cfg.cwd.clone()));
    let source = SessionId::from(args.session_id.clone());
    let new_id = fork_session(store.as_ref(), &cfg.cwd, &source).await?;
    if !cfg.quiet {
        println!("[fork] {} -> {}", source, new_id);
    }
    run_one_shot(
        cfg,
        args.common.no_session,
        args.prompt,
        Some(new_id),
        Some(source),
    )
    .await
}

async fn run_one_shot(
    cfg: ResolvedRunConfig,
    no_session: bool,
    prompt: String,
    forced_session_id: Option<SessionId>,
    fork_source: Option<SessionId>,
) -> Result<()> {
    let provider: Arc<dyn ModelProvider> = build_provider(&cfg.options.model)?;

    // `--no-session` disables session WRITES, but resume/continue/fork still
    // need to READ prior history to honor their semantics. When the caller
    // supplied an explicit session id (resume, continue, fork) we always open
    // the store for reading, even with --no-session.
    let read_store: Option<Arc<dyn SessionStore>> = forced_session_id
        .as_ref()
        .map(|_| Arc::new(FsSessionStore::new(cfg.cwd.clone())) as Arc<dyn SessionStore>);

    let was_forced = forced_session_id.is_some();
    let (session_id, session_sink, store) = if no_session {
        (forced_session_id.unwrap_or_default(), None, None)
    } else {
        let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cfg.cwd.clone()));
        let id = if let Some(id) = forced_session_id {
            id
        } else {
            store
                .create(&cfg.cwd)
                .await
                .context("failed to create session")?
        };
        let _ = fork_source;
        let _ =
            append_settings_snapshot(store.as_ref(), &cfg.cwd, &id, &cfg.settings_summary).await;
        let sink: Arc<dyn SessionSink> =
            Arc::new(SessionSinkAdapter::new(store.clone(), cfg.cwd.clone()));
        (id, Some(sink), Some(store))
    };

    let initial_history = if let Some(s) = store.as_ref().or(read_store.as_ref()) {
        // For resume/continue/fork (forced_session_id is Some) the session
        // was supposed to exist and load. Propagate the error so the user
        // sees a corrupted log instead of silently losing prior context
        // and appending new events on top of a broken history. For fresh
        // `run` (forced_session_id is None) the just-created file is
        // empty, so an unreadable load is unlikely but still surfaced.
        let records = if was_forced {
            s.load(&session_id)
                .await
                .with_context(|| format!("failed to load session {}", session_id))?
        } else {
            s.load(&session_id).await.unwrap_or_default()
        };
        let conv = replay_conversation(&records);
        drop_trailing_matching_user_prompt(conv.messages, &prompt)
    } else {
        Vec::new()
    };

    if let Some(s) = &store {
        let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &prompt).await;
    }

    let mut options = cfg.options.clone();
    options.session_id = Some(session_id.clone());

    let telemetry_sink: Option<Arc<dyn TelemetrySink>> =
        if cfg.verbose || matches!(cfg.log_format, LogFormat::Json) {
            Some(Arc::new(StderrTelemetrySink {
                json: matches!(cfg.log_format, LogFormat::Json),
            }))
        } else {
            None
        };

    let log_format = cfg.log_format;
    let show_usage = cfg.show_usage;
    let quiet = cfg.quiet;

    let mut builder = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(cfg.tools)
        .hooks(cfg.hooks)
        .initial_history({
            let _ = &store;
            initial_history
        });
    if let Some(sink) = session_sink {
        builder = builder.session_sink(sink);
    }
    if let Some(t) = telemetry_sink {
        builder = builder.telemetry_sink(t);
    }
    let agent = builder.build()?;

    let mut stream = agent.run(prompt);
    let exit_ok = drain_events_inline(&mut stream, log_format, show_usage, quiet).await;
    if exit_ok {
        Ok(())
    } else {
        Err(anyhow!("agent did not finish successfully"))
    }
}

fn drop_trailing_matching_user_prompt(
    mut history: Vec<code_agent_core::Message>,
    prompt: &str,
) -> Vec<code_agent_core::Message> {
    use code_agent_core::{ContentBlock, MessageRole};
    if let Some(last) = history.last() {
        if matches!(last.role, MessageRole::User) {
            if let Some(ContentBlock::Text { text }) = last.content.first() {
                if text == prompt && last.content.len() == 1 {
                    history.pop();
                }
            }
        }
    }
    history
}

async fn cmd_chat(args: ChatArgs) -> Result<()> {
    // Chat owns stdin for the user-input loop. A second stdin reader (the
    // one StdinApprover would spawn for approval prompts) would race on
    // FD 0 — Bash answers like `y` could be consumed as chat messages and
    // vice-versa. We disable the stdin approver in chat mode and require
    // the user to pre-allow tools via `--yes-to` or `.agent/settings.json`.
    // Unresolved tool requests are denied with a helpful message.
    let mut cfg = resolve_common(&args.common, false).await?;
    cfg.options.permission_callback = Some(Arc::new(ChatNoninteractiveApprover));
    let provider: Arc<dyn ModelProvider> = build_provider(&cfg.options.model)?;

    let store: Option<Arc<dyn SessionStore>> = if args.common.no_session {
        None
    } else {
        Some(Arc::new(FsSessionStore::new(cfg.cwd.clone())))
    };
    let session_id = if let Some(s) = &store {
        let id = s.create(&cfg.cwd).await?;
        let _ = append_settings_snapshot(s.as_ref(), &cfg.cwd, &id, &cfg.settings_summary).await;
        id
    } else {
        SessionId::new()
    };

    let session_sink: Option<Arc<dyn SessionSink>> = store
        .clone()
        .map(|s| Arc::new(SessionSinkAdapter::new(s, cfg.cwd.clone())) as Arc<dyn SessionSink>);

    let telemetry_sink: Option<Arc<dyn TelemetrySink>> =
        if cfg.verbose || matches!(cfg.log_format, LogFormat::Json) {
            Some(Arc::new(StderrTelemetrySink {
                json: matches!(cfg.log_format, LogFormat::Json),
            }))
        } else {
            None
        };

    // Tools.
    let mut tools = ToolRegistry::new();
    let mut limits = ToolLimits::default();
    if let Some(v) = cfg.settings.tool_limits.max_read_bytes {
        limits.max_read_bytes = v;
    }
    if let Some(v) = cfg.settings.tool_limits.max_output_bytes {
        limits.max_output_bytes = v;
    }
    if let Some(v) = cfg.settings.tool_limits.bash_timeout_seconds {
        limits.bash_timeout = Duration::from_secs(v);
    }
    if let Some(v) = cfg.settings.tool_limits.max_glob_results {
        limits.max_glob_results = v;
    }
    if let Some(v) = cfg.settings.tool_limits.max_grep_results {
        limits.max_grep_results = v;
    }
    register_builtin_with_limits(&mut tools, limits);

    // Hooks.
    let mut hooks = Hooks::new();
    if cfg.settings.hooks.block_env_file_writes.unwrap_or(true) {
        hooks = hooks
            .pre_tool_use("Edit|Write", Arc::new(BlockEnvFileWritesHook::default()))
            .expect("built-in pattern is always valid");
    }
    if cfg.settings.hooks.block_dangerous_bash.unwrap_or(true) {
        hooks = hooks
            .pre_tool_use("Bash", Arc::new(BlockDangerousBashHook::default()))
            .expect("built-in pattern is always valid");
    }
    if let Some(max_bytes) = cfg.settings.hooks.max_patch_size_bytes {
        hooks = hooks
            .pre_tool_use("Edit|Write", Arc::new(MaxPatchSizeHook::new(max_bytes)))
            .expect("built-in pattern is always valid");
    }
    if cfg.settings.hooks.audit_log.unwrap_or(false) {
        let audit = Arc::new(AuditLogHook::default());
        hooks = hooks
            .post_tool_use("*", audit.clone())
            .expect("built-in pattern is always valid");
        hooks = hooks
            .post_tool_use_failure("*", audit)
            .expect("built-in pattern is always valid");
    }
    if cfg.settings.hooks.secret_redaction.unwrap_or(true) {
        hooks = hooks
            .post_tool_use("*", Arc::new(SecretRedactionHook))
            .expect("built-in pattern is always valid");
    }

    let mut options = cfg.options.clone();
    options.session_id = Some(session_id.clone());

    let mut client_builder = AgentClient::builder()
        .cwd(cfg.cwd.clone())
        .model(cfg.options.model.clone())
        .permission_mode(cfg.options.permission_mode)
        .options(options)
        .provider(provider)
        .tools(tools)
        .hooks(hooks)
        .session_id(session_id.clone())
        .session_start(SessionStart::New);
    if let Some(s) = session_sink {
        client_builder = client_builder.session_sink(s);
    }
    if let Some(t) = telemetry_sink {
        client_builder = client_builder.telemetry_sink(t);
    }

    let mut client = client_builder.build().await?;

    if !cfg.quiet {
        println!("[chat] session {}", session_id);
        println!("[chat] enter prompts; Ctrl-D to exit, ':q' to quit, ':interrupt' to cancel current run.");
    }

    if let Some(initial) = args.prompt.clone() {
        if let Some(s) = &store {
            let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &initial).await;
        }
        client.send_user_message(initial).await?;
    }

    // Spawn a stdin reader.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(8);
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdin));
        while let Ok(Some(line)) = lines.next_line().await {
            if input_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    let mut current_text = String::new();
    let mut had_success = false;
    loop {
        tokio::select! {
            ev = client.next_event() => {
                let ev = match ev {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(_) => break,
                };
                if let AgentEvent::Result(r) = &ev {
                    had_success = matches!(r.subtype, ResultSubtype::Success);
                    if cfg.show_usage {
                        if let Some(u) = &r.usage {
                            let cost = r.total_cost_usd
                                .map(|c| format!(", est. cost=${:.4}", c))
                                .unwrap_or_default();
                            println!("[usage] turns={} input={} output={}{}", r.num_turns, u.input_tokens, u.output_tokens, cost);
                        }
                    }
                }
                render_event(&ev, cfg.log_format, &mut current_text, cfg.quiet);
            }
            line = input_rx.recv() => {
                match line {
                    Some(l) => {
                        let trimmed = l.trim();
                        if trimmed == ":q" || trimmed == ":quit" {
                            break;
                        }
                        if trimmed == ":interrupt" {
                            client.interrupt().await?;
                            continue;
                        }
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Some(s) = &store {
                            let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, trimmed).await;
                        }
                        client.send_user_message(trimmed.to_string()).await?;
                    }
                    None => break,
                }
            }
        }
    }

    let _ = client.shutdown().await;
    if had_success {
        Ok(())
    } else {
        // Treat normal chat exit (no Result) as success.
        Ok(())
    }
}

async fn cmd_sessions_list(cwd: PathBuf) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let ids = store.list(&cwd).await?;
    if ids.is_empty() {
        println!("(no sessions in {})", cwd.display());
    } else {
        for id in ids {
            println!("{id}");
        }
    }
    Ok(())
}

async fn cmd_sessions_show(session_id: String, cwd: PathBuf) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let id = SessionId::from(session_id);
    let summary = summarize_session(&store, &id).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn cmd_sessions_export(session_id: String, cwd: PathBuf, format: String) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let id = SessionId::from(session_id);
    let fmt = ExportFormat::from_str_lenient(&format)
        .ok_or_else(|| anyhow!("invalid --format: {format}"))?;
    let s = export_session(&store, &id, fmt).await?;
    print!("{s}");
    Ok(())
}

fn build_provider(_model: &str) -> Result<Arc<dyn ModelProvider>> {
    let cfg =
        AnthropicConfig::from_env().context("ANTHROPIC_API_KEY must be set to call the model")?;
    let provider = AnthropicProvider::new(cfg)?;
    Ok(Arc::new(provider) as Arc<dyn ModelProvider>)
}

async fn load_agents_md(cwd: &Path) -> Option<String> {
    let p = cwd.join("AGENTS.md");
    tokio::fs::read_to_string(&p).await.ok()
}

async fn drain_events_inline(
    stream: &mut (dyn futures::Stream<Item = AgentEvent> + Unpin),
    log_format: LogFormat,
    show_usage: bool,
    quiet: bool,
) -> bool {
    let mut current_text = String::new();
    let mut had_success = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::Result(r) = &ev {
            had_success = matches!(r.subtype, ResultSubtype::Success);
            if show_usage {
                if let Some(u) = &r.usage {
                    let cost = r
                        .total_cost_usd
                        .map(|c| format!(", est. cost=${:.4}", c))
                        .unwrap_or_default();
                    println!(
                        "[usage] turns={} input={} output={}{}",
                        r.num_turns, u.input_tokens, u.output_tokens, cost
                    );
                }
            }
        }
        render_event(&ev, log_format, &mut current_text, quiet);
    }
    had_success
}

fn render_event(ev: &AgentEvent, format: LogFormat, current_text: &mut String, quiet: bool) {
    if matches!(format, LogFormat::Json) {
        if let Ok(s) = serde_json::to_string(ev) {
            println!("{s}");
        }
        return;
    }
    match ev {
        AgentEvent::SystemInit(init) => {
            if !quiet {
                println!("[init] session {}", init.session_id);
            }
        }
        AgentEvent::AssistantTextDelta(t) => {
            print!("{t}");
            let _ = std::io::stdout().flush();
            current_text.push_str(t);
        }
        AgentEvent::AssistantMessage(_) => {
            if !current_text.is_empty() {
                println!();
                current_text.clear();
            }
        }
        AgentEvent::ToolCallStarted(s) => {
            if !quiet {
                println!("[tool] {}: {}", s.call.name, summarize_input(&s.call.input));
            }
        }
        AgentEvent::ToolCallCompleted(c) => {
            if !quiet {
                let tag = if c.is_error {
                    "tool-error"
                } else {
                    "tool-result"
                };
                println!("[{tag}] {} ({}ms)", c.tool_name, c.duration_ms);
            }
        }
        AgentEvent::ToolCallDenied(d) => {
            println!("[denied] {}: {}", d.tool_name, d.reason);
        }
        AgentEvent::UserToolResult(_) => {}
        AgentEvent::HookEvaluated(h) => {
            if !quiet
                && (matches!(
                    h.outcome,
                    HookOutcomeKind::Deny | HookOutcomeKind::AddContext
                ))
            {
                let tn = h.tool_name.clone().unwrap_or_default();
                println!(
                    "[hook] {:?} {} {:?}: {}",
                    h.event,
                    tn,
                    h.outcome,
                    h.reason.clone().unwrap_or_default()
                );
            }
        }
        AgentEvent::PolicyDecision(d) => {
            if !quiet {
                println!(
                    "[policy] {} {:?} {:?}{}",
                    d.tool_name,
                    d.outcome,
                    d.source,
                    d.reason
                        .as_ref()
                        .map(|r| format!(": {}", r))
                        .unwrap_or_default(),
                );
            }
        }
        AgentEvent::TodoUpdate(u) => {
            print!("{}", u.list.render());
            let _ = std::io::stdout().flush();
        }
        AgentEvent::Interrupted(_) => {
            println!("[interrupted]");
        }
        AgentEvent::Result(r) => {
            println!(
                "[result] {} (turns={}, subtype={:?})",
                r.result.as_deref().unwrap_or(""),
                r.num_turns,
                r.subtype
            );
        }
    }
}

fn summarize_input(v: &serde_json::Value) -> String {
    if let Some(p) = v.get("file_path").and_then(|s| s.as_str()) {
        return p.to_string();
    }
    if let Some(p) = v.get("pattern").and_then(|s| s.as_str()) {
        return p.to_string();
    }
    if let Some(c) = v.get("command").and_then(|s| s.as_str()) {
        return c.to_string();
    }
    v.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BashRisk {
    Low,
    Medium,
    High,
    Blocked,
}

fn classify_bash_risk(cmd: &str, disallowed: &[ToolRule]) -> BashRisk {
    for r in disallowed {
        if r.matches("Bash", &serde_json::json!({"command": cmd})) {
            return BashRisk::Blocked;
        }
    }
    let c = cmd.trim();
    let lc = c.to_ascii_lowercase();
    if lc.starts_with("sudo")
        || lc.contains(" sudo ")
        || lc.contains("rm -rf")
        || lc.contains("chmod -R 777")
        || lc.contains("chown ")
        || (lc.contains("curl") && lc.contains("| sh"))
        || (lc.contains("wget") && lc.contains("| sh"))
    {
        return BashRisk::High;
    }
    let low_starts = [
        "cargo test",
        "cargo check",
        "cargo fmt",
        "cargo clippy",
        "cargo build",
        "npm test",
        "pnpm test",
        "yarn test",
        "pytest",
        "go test",
    ];
    for s in &low_starts {
        if c.starts_with(s) {
            return BashRisk::Low;
        }
    }
    BashRisk::Medium
}

/// Permission callback that prompts the user on stdin with the rich
/// approval menu.
/// Permission callback used by `cmd_chat`. Chat owns stdin, so a stdin-
/// reading approver would race the chat input loop. Instead, this callback
/// always denies and points the user at `--yes-to` / `.agent/settings.json`.
/// Read-only tools and explicitly-allowed rules never reach this callback
/// because the policy engine resolves them upstream.
struct ChatNoninteractiveApprover;

#[async_trait]
impl PermissionCallback for ChatNoninteractiveApprover {
    async fn approve(&self, request: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: format!(
                "interactive approval is disabled in chat mode; allow `{}` via --yes-to=<rule> or .agent/settings.json (allowed_tools)",
                request.tool_name
            ),
        }
    }
}

struct StdinApprover {
    interactive: bool,
    mode: PermissionMode,
    disallowed: Vec<ToolRule>,
    allowed: Vec<ToolRule>,
    last_input_seen: TokioMutex<()>,
}

impl StdinApprover {
    fn new(mode: PermissionMode, disallowed: Vec<ToolRule>, allowed: Vec<ToolRule>) -> Self {
        Self {
            interactive: std::io::stdin().is_terminal(),
            mode,
            disallowed,
            allowed,
            last_input_seen: TokioMutex::new(()),
        }
    }

    fn render_prompt(&self, request: &PermissionRequest) -> String {
        let mut s = String::new();
        s.push_str(&format!("\n[approval] mode={:?}\n", self.mode));
        s.push_str(&format!("  Tool: {}\n", request.tool_name));
        match request.tool_name.as_str() {
            "Bash" => {
                let cmd = request
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let to = request
                    .input
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                let risk = classify_bash_risk(cmd, &self.disallowed);
                s.push_str(&format!("  Command: {}\n", cmd));
                s.push_str(&format!("  Timeout: {}s\n", to));
                s.push_str(&format!("  Risk: {:?}\n", risk));
                s.push_str(&format!("  Reason: {}\n", request.summary));
            }
            "Edit" | "Write" => {
                let path = request
                    .input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let exists = std::path::Path::new(path).exists();
                s.push_str(&format!("  File: {}\n", path));
                s.push_str(&format!(
                    "  Action: {}\n",
                    if exists { "modify" } else { "create" }
                ));
                if request.tool_name == "Edit" {
                    let old = request
                        .input
                        .get("old_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let new = request
                        .input
                        .get("new_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    s.push_str(&render_diff_preview(old, new));
                    s.push_str(&format!("  Size: -{} +{}\n", old.len(), new.len()));
                } else {
                    let content = request
                        .input
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    s.push_str(&format!("  Bytes: {}\n", content.len()));
                    let preview: String = content.chars().take(160).collect();
                    s.push_str(&format!("  Preview: {}\n", preview.replace('\n', " ")));
                }
            }
            _ => {
                s.push_str(&format!("  Summary: {}\n", request.summary));
            }
        }
        s.push_str("    [y] allow once\n");
        s.push_str("    [s] allow for session\n");
        s.push_str("    [a] allow for project\n");
        s.push_str("    [n] deny once\n");
        s.push_str("    [d] deny for session\n");
        s.push_str("    [v] view full input\n");
        s.push_str("  approve> ");
        s
    }

    fn ask_user(prompt: &str, full_input: &str) -> String {
        loop {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            let answer = buf.trim().to_ascii_lowercase();
            if answer == "v" {
                println!("---\n{}\n---", full_input);
                continue;
            }
            return answer;
        }
    }
}

#[async_trait]
impl PermissionCallback for StdinApprover {
    async fn approve(&self, request: PermissionRequest) -> PermissionDecision {
        if !self.interactive {
            return PermissionDecision::Deny {
                reason: format!(
                    "non-interactive session; tool {} requires approval (mode={:?})",
                    request.tool_name, self.mode
                ),
            };
        }
        let _g = self.last_input_seen.lock().await;
        let prompt = self.render_prompt(&request);
        // Show matched policy/hook information if we have any.
        let mut prompt = prompt;
        for r in &self.allowed {
            if r.matches(&request.tool_name, &request.input) {
                prompt.insert_str(0, "\n[matched allow rule]\n");
                break;
            }
        }
        let full_input =
            serde_json::to_string_pretty(&request.input).unwrap_or_else(|_| String::new());
        let answer = tokio::task::spawn_blocking(move || Self::ask_user(&prompt, &full_input))
            .await
            .unwrap_or_default();
        match answer.as_str() {
            "y" | "yes" => PermissionDecision::AllowOnce,
            "s" => PermissionDecision::AllowForSession,
            "a" => PermissionDecision::AllowForProject,
            "n" | "" => PermissionDecision::Deny {
                reason: "user denied once".into(),
            },
            "d" => PermissionDecision::DenyForSession {
                reason: "user denied for session".into(),
            },
            other => PermissionDecision::Deny {
                reason: format!("unknown choice `{other}`; treated as deny"),
            },
        }
    }
}

fn render_diff_preview(old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut s = String::new();
    s.push_str("  Diff:\n");
    let mut shown = 0usize;
    for change in diff.iter_all_changes() {
        let sign: &str = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => continue,
        };
        s.push_str("    ");
        s.push_str(sign);
        let value: &str = change.value();
        s.push_str(value);
        if !value.ends_with('\n') {
            s.push('\n');
        }
        shown += 1;
        if shown > 20 {
            s.push_str("    ... (diff truncated)\n");
            break;
        }
    }
    s
}

struct StderrTelemetrySink {
    json: bool,
}

#[async_trait]
impl TelemetrySink for StderrTelemetrySink {
    async fn record(&self, event: &AgentTelemetryEvent) {
        if self.json {
            if let Ok(s) = serde_json::to_string(event) {
                eprintln!("{s}");
            }
        } else {
            eprintln!("[telemetry] {:?}", event);
        }
    }
}
