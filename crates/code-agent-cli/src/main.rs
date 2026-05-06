use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use code_agent_anthropic::{AnthropicConfig, AnthropicProvider};
use code_agent_checkpoint::{
    AgentCheckpointHandle, BashCheckpointPolicy, CheckpointFailureMode, CheckpointOptions,
    CheckpointStore, CreateCheckpointRequest, DiffCheckpointRequest, FileDiff, LocalCheckpointApi,
    LocalCheckpointStore, RewindRequest, ValidationOptions,
};
use code_agent_core::agent::{SessionSink, TelemetrySink};
use code_agent_core::SEARCH_TOOLS_NAME;
use code_agent_core::{
    is_valid_session_id_str, AgentClient, AgentEvent, AgentOptions, AgentTelemetryEvent,
    ApprovalMemoryMode, AuditLogHook, BlockDangerousBashHook, BlockEnvFileWritesHook,
    HookOutcomeKind, Hooks, MaxPatchSizeHook, ModelProvider, NativeToolSearchMode,
    PermissionCallback, PermissionDecision, PermissionMode, PermissionRequest, ResultSubtype,
    SearchToolsDefinitionTool, SecretRedactionHook, SessionId, SessionStart, SettingSource,
    Settings, ToolCatalog, ToolCatalogBuilder, ToolListFilter, ToolLoadingMode, ToolRegistry,
    ToolRule, ToolSearchOptions, ToolSearchQuery, ToolSearchStrategy, ToolSourceFilter,
};
use code_agent_core::{CheckpointApi, CheckpointHandle, RunId};
use code_agent_session::{
    append_settings_snapshot, append_user_prompt, export_session, fork_session,
    replay_conversation, summarize_session, ExportFormat, FsSessionStore, SessionPayload,
    SessionSinkAdapter, SessionStore,
};
use code_agent_tools::{register_builtin_with_limits, ToolLimits};
use futures::StreamExt;
use std::time::Duration as StdDuration;

type RunCheckpointSetup = (
    Option<Arc<dyn CheckpointHandle>>,
    Option<Arc<LocalCheckpointStore>>,
    Option<String>,
);

type ChatCheckpointSetup = (
    Option<Arc<LocalCheckpointStore>>,
    Option<Arc<code_agent_checkpoint::RotatingCheckpointHandle>>,
    Option<Arc<dyn CheckpointApi>>,
);
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
    /// Create a manual workspace checkpoint.
    Checkpoint(CheckpointCreateArgs),
    /// Manage workspace checkpoints.
    #[command(subcommand)]
    Checkpoints(CheckpointsCommand),
    /// Rewind workspace files to a checkpoint.
    Rewind(RewindArgs),
    /// Inspect and call configured MCP servers.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Inspect, search, and explain the tool catalog.
    #[command(subcommand)]
    Tools(ToolsCommand),
    /// Manage and run subagents.
    #[command(subcommand)]
    Agents(AgentsCommand),
    /// Manage Skills.
    #[command(subcommand)]
    Skills(SkillsCommand),
    /// Manage slash commands.
    #[command(subcommand)]
    Commands(CommandsCommand),
}

#[derive(Subcommand, Debug)]
enum SkillsCommand {
    /// List discovered Skills.
    List(SkillsCommonArgs),
    /// Show a Skill (frontmatter + body preview).
    Show {
        skill_name: String,
        #[command(flatten)]
        common: SkillsCommonArgs,
    },
    /// Validate every discovered Skill.
    Validate(SkillsCommonArgs),
    /// Inspect a Skill's resources, sizes, and tool rules.
    Inspect {
        skill_name: String,
        #[command(flatten)]
        common: SkillsCommonArgs,
    },
    /// Create a skeleton Skill directory.
    Init {
        name: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
}

#[derive(Parser, Debug, Clone)]
struct SkillsCommonArgs {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Path to the Skills directory (default `.agent/skills`).
    #[arg(long)]
    skills: Option<PathBuf>,
    /// Enable `.claude/skills` compatibility loading.
    #[arg(long)]
    compat_claude_skills: bool,
}

#[derive(Subcommand, Debug)]
enum CommandsCommand {
    /// List slash commands.
    List(CommandsCommonArgs),
    /// Show a slash command (source + body preview).
    Show {
        command_name: String,
        #[command(flatten)]
        common: CommandsCommonArgs,
    },
    /// Validate every custom command file.
    Validate(CommandsCommonArgs),
}

#[derive(Parser, Debug, Clone)]
struct CommandsCommonArgs {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Path to the custom commands directory (default `.agent/commands`).
    #[arg(long)]
    commands: Option<PathBuf>,
    /// Path to the Skills directory (so Skill-backed commands appear).
    #[arg(long)]
    skills: Option<PathBuf>,
    /// Disable built-in commands in the listing.
    #[arg(long)]
    no_builtins: bool,
}

#[derive(Subcommand, Debug)]
enum AgentsCommand {
    /// List registered subagents.
    List(AgentsCommonArgs),
    /// Show a subagent definition (parsed frontmatter + prompt preview).
    Show {
        agent_name: String,
        #[command(flatten)]
        common: AgentsCommonArgs,
    },
    /// Validate filesystem and built-in subagent definitions.
    Validate(AgentsCommonArgs),
    /// Run a subagent directly (debugging).
    Run {
        agent_name: String,
        task: String,
        #[command(flatten)]
        common: AgentsCommonArgs,
        #[arg(long)]
        max_turns: Option<u32>,
        #[arg(long)]
        show_events: bool,
    },
}

#[derive(Parser, Debug, Clone)]
struct AgentsCommonArgs {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Path to subagent definitions directory (default `.agent/agents`).
    #[arg(long)]
    agents: Option<PathBuf>,
    /// Settings sources to load: none|project|user|user-and-project.
    #[arg(long, default_value = "user-and-project")]
    settings: String,
}

#[derive(Subcommand, Debug)]
enum ToolsCommand {
    /// List tools in the catalog.
    List(ToolsListArgs),
    /// Search tool metadata.
    Search(ToolsSearchArgs),
    /// Show one tool descriptor as JSON.
    Show(ToolsShowArgs),
    /// Show tool catalog and budget stats.
    Stats(ToolsCommonArgs),
    /// Explain the loading-mode decision.
    ExplainLoading(ToolsCommonArgs),
}

#[derive(Parser, Debug, Clone)]
struct ToolsCommonArgs {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    #[arg(long)]
    mcp_timeout: Option<u64>,
    #[arg(long)]
    mcp_strict: bool,
    #[arg(long = "mcp-server")]
    mcp_servers: Vec<String>,
    #[arg(long, default_value = "user-and-project")]
    settings: String,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Vec<String>,
    #[arg(long = "deny")]
    deny: Vec<String>,
    /// Permission mode override (default | accept-edits | dontAsk | plan).
    /// Falls back to `permissionMode` from settings, then `default`.
    #[arg(long)]
    permission_mode: Option<String>,
    #[arg(long)]
    tool_loading: Option<String>,
    #[arg(long)]
    tool_search_strategy: Option<String>,
    #[arg(long)]
    native_tool_search: Option<String>,
    #[arg(long)]
    tool_search_threshold: Option<u8>,
    #[arg(long)]
    max_tool_search_results: Option<usize>,
    #[arg(long)]
    max_loaded_tools: Option<usize>,
    #[arg(long)]
    max_loaded_tool_tokens: Option<usize>,
    #[arg(long, value_delimiter = ',')]
    always_load: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    defer_tools: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    disable_tools: Vec<String>,
}

#[derive(Parser, Debug)]
struct ToolsListArgs {
    #[arg(long)]
    loaded: bool,
    #[arg(long)]
    deferred: bool,
    #[arg(long)]
    disabled: bool,
    #[arg(long)]
    all: bool,
    #[command(flatten)]
    common: ToolsCommonArgs,
}

#[derive(Parser, Debug)]
struct ToolsSearchArgs {
    query: String,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    strategy: Option<String>,
    #[arg(long)]
    source: Option<String>,
    #[command(flatten)]
    common: ToolsCommonArgs,
}

#[derive(Parser, Debug)]
struct ToolsShowArgs {
    tool: String,
    #[command(flatten)]
    common: ToolsCommonArgs,
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    /// List configured MCP servers.
    List(McpCommonArgs),
    /// Connect and show MCP server status.
    Status(McpCommonArgs),
    /// List tools exposed by one MCP server.
    Tools {
        server: String,
        #[command(flatten)]
        common: McpCommonArgs,
    },
    /// Call an MCP tool directly.
    Call {
        server: String,
        tool: String,
        #[arg(long, default_value = "{}")]
        args_json: String,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        common: McpCommonArgs,
    },
    /// List resources exposed by one MCP server.
    Resources {
        server: String,
        #[command(flatten)]
        common: McpCommonArgs,
    },
    /// Read a resource from one MCP server.
    #[command(subcommand)]
    Resource(McpResourceCommand),
    /// List prompts exposed by one MCP server.
    Prompts {
        server: String,
        #[command(flatten)]
        common: McpCommonArgs,
    },
}

#[derive(Subcommand, Debug)]
enum McpResourceCommand {
    Read {
        server: String,
        uri: String,
        #[command(flatten)]
        common: McpCommonArgs,
    },
}

#[derive(Parser, Debug, Clone)]
struct McpCommonArgs {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    #[arg(long)]
    mcp_timeout: Option<u64>,
    #[arg(long)]
    mcp_strict: bool,
    #[arg(long = "mcp-server")]
    mcp_servers: Vec<String>,
}

#[derive(Parser, Debug)]
struct CheckpointCreateArgs {
    /// Optional description for the checkpoint.
    description: Option<String>,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Session id to attach the checkpoint to. If omitted, the latest
    /// session for the cwd is used.
    #[arg(long)]
    session: Option<String>,
}

#[derive(Subcommand, Debug)]
enum CheckpointsCommand {
    /// List checkpoints for a session.
    List {
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Session id. If omitted, the latest session for the cwd is used.
        #[arg(long)]
        session: Option<String>,
    },
    /// Show a checkpoint's metadata.
    Show {
        checkpoint_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        session: Option<String>,
    },
    /// Diff a checkpoint against the current workspace.
    Diff {
        checkpoint_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Parser, Debug)]
struct RewindArgs {
    /// Checkpoint id to rewind to.
    checkpoint_id: String,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Skip the confirmation prompt.
    #[arg(long)]
    yes: bool,
    /// Print actions without modifying files.
    #[arg(long)]
    dry_run: bool,
    /// Allow rewind even if checkpoint cwd does not match current cwd.
    #[arg(long)]
    force_cwd: bool,
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
    /// Checkpointing: on|off. Defaults to settings.checkpointing.enabled,
    /// then on for mutating CLI commands.
    #[arg(long)]
    checkpointing: Option<String>,
    /// Validation command(s) to run after the agent completes (repeatable).
    #[arg(long = "validate")]
    validate: Vec<String>,
    /// Per-validation-command timeout, in seconds.
    #[arg(long)]
    validation_timeout: Option<u64>,
    /// Automatically rewind to the run's checkpoint when validation fails.
    #[arg(long)]
    rewind_on_validation_failure: bool,
    /// Load MCP server configuration from this file.
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// Enable or disable MCP: true|false.
    #[arg(long)]
    mcp_enabled: Option<String>,
    /// Fail the run if any configured MCP server fails to connect.
    #[arg(long)]
    mcp_strict: bool,
    /// Override MCP startup and request timeout, in seconds.
    #[arg(long)]
    mcp_timeout: Option<u64>,
    /// Limit MCP loading to this server name (repeatable).
    #[arg(long = "mcp-server")]
    mcp_servers: Vec<String>,
    /// Tool loading mode: eager|search|auto.
    #[arg(long)]
    tool_loading: Option<String>,
    /// Tool search strategy: regex|bm25|hybrid.
    #[arg(long)]
    tool_search_strategy: Option<String>,
    /// Native provider tool search: off|anthropic-regex|anthropic-bm25|auto.
    #[arg(long)]
    native_tool_search: Option<String>,
    /// Auto-loading activation threshold as percent of context.
    #[arg(long)]
    tool_search_threshold: Option<u8>,
    /// Maximum SearchTools results.
    #[arg(long)]
    max_tool_search_results: Option<usize>,
    /// Maximum loaded tools in search mode.
    #[arg(long)]
    max_loaded_tools: Option<usize>,
    /// Maximum estimated loaded tool-definition tokens in search mode.
    #[arg(long)]
    max_loaded_tool_tokens: Option<usize>,
    /// Comma-separated tool names or globs to always load.
    #[arg(long, value_delimiter = ',')]
    always_load: Vec<String>,
    /// Comma-separated tool names or globs to defer.
    #[arg(long, value_delimiter = ',')]
    defer_tools: Vec<String>,
    /// Comma-separated tool names or globs to disable.
    #[arg(long, value_delimiter = ',')]
    disable_tools: Vec<String>,
    /// Path to subagent definitions directory (default `.agent/agents`).
    #[arg(long)]
    agents: Option<PathBuf>,
    /// Disable filesystem subagent loading entirely.
    #[arg(long)]
    no_agents: bool,
    /// Cap on concurrent in-flight subagents (default 3).
    #[arg(long)]
    max_concurrent_subagents: Option<usize>,
    /// Per-subagent timeout in seconds (default 300).
    #[arg(long)]
    subagent_timeout: Option<u64>,
    /// When set, child events are interleaved into the parent stream.
    #[arg(long)]
    show_subagent_events: bool,
    /// Path to Skills directory (default `.agent/skills`).
    #[arg(long)]
    skills: Option<PathBuf>,
    /// Disable Skills entirely.
    #[arg(long)]
    no_skills: bool,
    /// Path to custom commands directory (default `.agent/commands`).
    #[arg(long)]
    commands: Option<PathBuf>,
    /// Disable slash command dispatch.
    #[arg(long)]
    no_slash_commands: bool,
    /// Enable `.claude/skills` compatibility loading.
    #[arg(long)]
    compat_claude_skills: bool,
    /// Enable `.claude/commands` compatibility loading.
    #[arg(long)]
    compat_claude_commands: bool,
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
    /// List child (subagent) sessions for a parent session.
    Children {
        session_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Show a child (subagent) session's records.
    ShowChild {
        subagent_session_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
    /// Export a child (subagent) session as JSONL or JSON.
    ExportChild {
        subagent_session_id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long, default_value = "jsonl")]
        format: String,
    },
}

fn parse_permission_mode(s: &str) -> Result<PermissionMode> {
    PermissionMode::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --permission-mode: {s}"))
}

/// Reject session ids that contain path separators, `..`, or other
/// non-`[A-Za-z0-9_-]` characters. Defends against path traversal via the
/// user-supplied `--session-id` (it ultimately becomes a file name under
/// `<cwd>/.agent/sessions/`).
fn parse_session_id(s: &str) -> Result<SessionId> {
    if !is_valid_session_id_str(s) {
        return Err(anyhow!(
            "invalid session id: must match [A-Za-z0-9_-]+ and contain no path separators"
        ));
    }
    Ok(SessionId::from(s.to_string()))
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

fn parse_bool_flag(name: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(anyhow!("invalid {name}: {other}")),
    }
}

fn parse_tool_loading_mode(
    value: &str,
    threshold_percent: u8,
    min_tool_count: usize,
) -> Result<ToolLoadingMode> {
    ToolLoadingMode::from_str_lenient(value, threshold_percent, min_tool_count)
        .ok_or_else(|| anyhow!("invalid --tool-loading: {value}"))
}

fn parse_tool_search_strategy(value: &str) -> Result<ToolSearchStrategy> {
    ToolSearchStrategy::from_str_lenient(value)
        .ok_or_else(|| anyhow!("invalid --tool-search-strategy: {value}"))
}

fn parse_native_tool_search(value: &str) -> Result<NativeToolSearchMode> {
    NativeToolSearchMode::from_str_lenient(value)
        .ok_or_else(|| anyhow!("invalid --native-tool-search: {value}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Pretty,
    Json,
}

fn parse_cli_rule(s: &str) -> Result<ToolRule> {
    if let Some((tool, rest)) = s.split_once(':') {
        if tool == "Bash" {
            return ToolRule::try_bash_command_regex(rest)
                .map_err(|e| anyhow!("--yes-to/--deny rule {s:?}: {e}"));
        }
        return ToolRule::try_file_path_glob(tool, rest)
            .map_err(|e| anyhow!("--yes-to/--deny rule {s:?}: {e}"));
    }
    if s.contains('*') || s.contains('?') {
        ToolRule::try_tool_glob(s).map_err(|e| anyhow!("--yes-to/--deny rule {s:?}: {e}"))
    } else {
        Ok(ToolRule::ExactTool(s.to_string()))
    }
}

#[cfg(test)]
mod parse_cli_rule_tests {
    use super::*;

    #[test]
    // Malformed Bash command regex from the CLI must error, not silently produce a fail-open rule.
    fn rejects_malformed_bash_regex() {
        let err = parse_cli_rule("Bash:[").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("command_regex") && msg.contains("\"[\""),
            "expected error mentioning the bad regex; got: {msg}"
        );
    }

    #[test]
    // Malformed file path glob from the CLI must error.
    fn rejects_malformed_path_glob() {
        let err = parse_cli_rule("Edit:[").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("path_glob") && msg.contains("\"[\""),
            "expected error mentioning the bad path_glob; got: {msg}"
        );
    }

    #[test]
    // Malformed bare tool-glob string must error.
    fn rejects_malformed_tool_glob() {
        let err = parse_cli_rule("[*").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tool glob") && msg.contains("\"[*\""),
            "expected error mentioning the bad tool glob; got: {msg}"
        );
    }

    #[test]
    // Valid simple inputs still parse to the expected ToolRule shapes.
    fn parses_valid_inputs() {
        assert!(matches!(
            parse_cli_rule("Read").unwrap(),
            ToolRule::ExactTool(ref n) if n == "Read"
        ));
        assert!(matches!(
            parse_cli_rule("Edit*").unwrap(),
            ToolRule::ToolGlob(ref g) if g == "Edit*"
        ));
        assert!(matches!(
            parse_cli_rule("Bash:^cargo (test|check)").unwrap(),
            ToolRule::BashCommandRegex(_)
        ));
        assert!(matches!(
            parse_cli_rule("Edit:**/.env*").unwrap(),
            ToolRule::FilePathGlob { .. }
        ));
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
        Command::Sessions(SessionsCommand::Children { session_id, cwd }) => {
            cmd_sessions_children(session_id, cwd).await
        }
        Command::Sessions(SessionsCommand::ShowChild {
            subagent_session_id,
            cwd,
        }) => cmd_sessions_show_child(subagent_session_id, cwd).await,
        Command::Sessions(SessionsCommand::ExportChild {
            subagent_session_id,
            cwd,
            format,
        }) => cmd_sessions_export_child(subagent_session_id, cwd, format).await,
        Command::Agents(AgentsCommand::List(args)) => cmd_agents_list(args).await,
        Command::Agents(AgentsCommand::Show { agent_name, common }) => {
            cmd_agents_show(agent_name, common).await
        }
        Command::Agents(AgentsCommand::Validate(args)) => cmd_agents_validate(args).await,
        Command::Agents(AgentsCommand::Run {
            agent_name,
            task,
            common,
            max_turns,
            show_events,
        }) => cmd_agents_run(agent_name, task, common, max_turns, show_events).await,
        Command::Checkpoint(args) => cmd_checkpoint_create(args).await,
        Command::Checkpoints(CheckpointsCommand::List { cwd, session }) => {
            cmd_checkpoints_list(cwd, session).await
        }
        Command::Checkpoints(CheckpointsCommand::Show {
            checkpoint_id,
            cwd,
            session,
        }) => cmd_checkpoints_show(checkpoint_id, cwd, session).await,
        Command::Checkpoints(CheckpointsCommand::Diff {
            checkpoint_id,
            cwd,
            session,
        }) => cmd_checkpoints_diff(checkpoint_id, cwd, session).await,
        Command::Rewind(args) => cmd_rewind(args).await,
        Command::Mcp(args) => cmd_mcp(args).await,
        Command::Tools(args) => cmd_tools(args).await,
        Command::Skills(SkillsCommand::List(args)) => cmd_skills_list(args).await,
        Command::Skills(SkillsCommand::Show { skill_name, common }) => {
            cmd_skills_show(skill_name, common).await
        }
        Command::Skills(SkillsCommand::Validate(args)) => cmd_skills_validate(args).await,
        Command::Skills(SkillsCommand::Inspect { skill_name, common }) => {
            cmd_skills_inspect(skill_name, common).await
        }
        Command::Skills(SkillsCommand::Init { name, cwd }) => cmd_skills_init(name, cwd).await,
        Command::Commands(CommandsCommand::List(args)) => cmd_commands_list(args).await,
        Command::Commands(CommandsCommand::Show {
            command_name,
            common,
        }) => cmd_commands_show(command_name, common).await,
        Command::Commands(CommandsCommand::Validate(args)) => cmd_commands_validate(args).await,
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
    checkpointing: CheckpointOptions,
    validation: ValidationOptions,
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
        if let Some(mcp) = file.mcp {
            s.mcp = Some(mcp);
        }
        if let Some(tool_search) = file.tool_search {
            s.tool_search = tool_search;
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
    for tool_name in &common.allowed_tools {
        if tool_name.starts_with("mcp__") {
            settings.allowed_tools.push(parse_cli_rule(tool_name)?);
        }
    }

    let mcp_enabled_override = common
        .mcp_enabled
        .as_deref()
        .map(|s| parse_bool_flag("--mcp-enabled", s))
        .transpose()?;
    let mcp_timeout = common.mcp_timeout.map(Duration::from_secs);
    let mut mcp_options = code_agent_mcp::load_mcp_options(
        &cwd,
        settings.mcp.as_ref(),
        common.mcp_config.as_deref(),
        mcp_enabled_override,
        common.mcp_strict.then_some(true),
        mcp_timeout,
    )
    .context("load MCP config")?;
    if !common.mcp_servers.is_empty() {
        mcp_options
            .servers
            .retain(|name, _| common.mcp_servers.iter().any(|wanted| wanted == name));
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
            SEARCH_TOOLS_NAME.into(),
        ];
        for r in &settings.allowed_tools {
            if let Some(pattern) = registry_filter_pattern_for_rule(r) {
                if !out.contains(&pattern) {
                    out.push(pattern);
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
    let tool_search = tool_search_options_from_common(&settings, common)?;

    // M6: load subagent registry and runtime options.
    let (agent_registry, agent_runtime_options) =
        load_agent_registry_and_runtime(&cwd, &settings, common)?;

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
        mcp: mcp_options.clone(),
        tool_search,
        agents: agent_registry.clone(),
        agent_runtime: agent_runtime_options.clone(),
        skills: load_skills_for_run(&cwd, common, &settings)?,
        skill_options: skill_options_for_run(common, &settings),
        slash_commands: load_slash_registry_for_run(
            &cwd,
            common,
            &settings,
            &load_skills_for_run(&cwd, common, &settings)?,
        )?,
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

    let settings_summary = build_settings_summary(&settings, &model, mode, &mcp_options);
    let (checkpointing, validation) = resolve_checkpoint_and_validation(common, &settings)?;

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
        checkpointing,
        validation,
    })
}

fn resolve_checkpoint_and_validation(
    common: &CommonArgs,
    settings: &Settings,
) -> Result<(CheckpointOptions, ValidationOptions)> {
    // Default: enabled when settings says so, otherwise on for CLI.
    let cli_override: Option<bool> = match common.checkpointing.as_deref() {
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Some(true),
            "off" | "false" | "no" | "0" => Some(false),
            other => return Err(anyhow!("invalid --checkpointing: {other}")),
        },
        None => None,
    };
    let enabled = cli_override
        .or(settings.checkpointing.enabled)
        .unwrap_or(true);

    let failure_mode = settings
        .checkpointing
        .failure_mode
        .as_deref()
        .map(|s| {
            CheckpointFailureMode::from_str_lenient(s)
                .ok_or_else(|| anyhow!("invalid checkpointing.failure_mode: {s}"))
        })
        .transpose()?
        .unwrap_or_default();

    let bash_policy = settings
        .checkpointing
        .bash_policy
        .as_deref()
        .map(|s| {
            BashCheckpointPolicy::from_str_lenient(s)
                .ok_or_else(|| anyhow!("invalid checkpointing.bash_policy: {s}"))
        })
        .transpose()?
        .unwrap_or_default();

    let mut limits = code_agent_checkpoint::CheckpointLimits::default();
    if let Some(v) = settings.checkpointing.max_checkpoint_bytes_per_file {
        limits.max_checkpoint_bytes_per_file = v;
    }
    if let Some(v) = settings
        .checkpointing
        .max_total_checkpoint_bytes_per_session
    {
        limits.max_total_checkpoint_bytes_per_session = v;
    }
    if let Some(r) = &settings.checkpointing.retention {
        if let Some(v) = r.max_checkpoints_per_session {
            limits.max_checkpoints_per_session = v;
        }
        if let Some(v) = r.max_age_days {
            limits.max_age_days = Some(v);
        }
    }
    if let Some(v) = settings.checkpointing.max_diff_bytes {
        limits.max_diff_bytes = v;
    }

    let opts = CheckpointOptions {
        enabled,
        failure_mode,
        create_on_user_message: settings
            .checkpointing
            .create_on_user_message
            .unwrap_or(true),
        limits,
        bash_policy,
    };

    // Validation: CLI flags override settings.
    let cmds = if !common.validate.is_empty() {
        common.validate.clone()
    } else {
        settings.validation.commands.clone().unwrap_or_default()
    };
    let timeout_secs = common
        .validation_timeout
        .or(settings.validation.timeout_seconds)
        .unwrap_or(60);
    let rewind_on_failure = common.rewind_on_validation_failure
        || settings.validation.rewind_on_failure.unwrap_or(false);
    let validation = ValidationOptions {
        commands: cmds,
        timeout: StdDuration::from_secs(timeout_secs),
        rewind_on_failure,
    };

    Ok((opts, validation))
}

fn registry_filter_pattern_for_rule(rule: &ToolRule) -> Option<String> {
    match rule {
        ToolRule::ExactTool(name) => Some(name.clone()),
        ToolRule::ToolGlob(pattern) => Some(pattern.clone()),
        ToolRule::McpServerGlob { server, tool_glob } => {
            Some(format!("mcp__{}__{}", server, tool_glob))
        }
        ToolRule::BashCommandRegex(_) | ToolRule::FilePathGlob { .. } => None,
        // M6: an Agent rule grants/denies the built-in `Agent` tool. The
        // registry filter just needs to keep the tool name available; the
        // policy engine matches `agent` field against the glob.
        ToolRule::AgentNameGlob { .. } => Some("Agent".into()),
    }
}

fn load_agent_registry_and_runtime(
    cwd: &Path,
    settings: &Settings,
    common: &CommonArgs,
) -> Result<(
    code_agent_core::AgentRegistry,
    code_agent_core::AgentRuntimeOptions,
)> {
    let mut registry = code_agent_core::AgentRegistry::new();
    let mut runtime = code_agent_core::AgentRuntimeOptions::default();

    // Apply settings (project + user) first.
    if let Some(s) = &settings.agents {
        if let Some(v) = s.enabled {
            runtime.enabled = v;
        }
        if let Some(v) = s.max_concurrent_subagents {
            runtime.max_concurrent_subagents = v.max(1);
        }
        if let Some(v) = s.subagent_timeout_seconds {
            runtime.subagent_timeout = std::time::Duration::from_secs(v);
        }
        if let Some(v) = s.persist_subagent_transcripts {
            runtime.persist_subagent_transcripts = v;
        }
        if let Some(v) = s.event_visibility.as_deref() {
            runtime.event_visibility =
                code_agent_core::SubagentEventVisibility::from_str_lenient(v)
                    .ok_or_else(|| anyhow!("invalid agents.event_visibility: {v:?}"))?;
        }
        if let Some(v) = s.checkpoint_policy.as_deref() {
            runtime.checkpoint_policy =
                code_agent_core::SubagentCheckpointPolicy::from_str_lenient(v)
                    .ok_or_else(|| anyhow!("invalid agents.checkpoint_policy: {v:?}"))?;
        }
        let mut policy = runtime.context_policy.clone();
        if let Some(v) = s.inherit_project_instructions {
            policy.inherit_project_instructions = v;
        }
        if let Some(v) = s.inherit_settings {
            policy.inherit_settings = v;
        }
        if let Some(v) = s.inherit_hooks {
            policy.inherit_hooks = v;
        }
        if let Some(v) = s.inherit_permissions {
            policy.inherit_permissions = v;
        }
        if let Some(v) = s.inherit_mcp_servers {
            policy.inherit_mcp_servers = v;
        }
        if let Some(v) = s.inherit_tool_search_state {
            policy.inherit_tool_search_state = v;
        }
        if let Some(v) = s.inherit_approval_memory {
            policy.inherit_approval_memory = v;
        }
        runtime.context_policy = policy;
    }

    // CLI overrides.
    if let Some(v) = common.max_concurrent_subagents {
        runtime.max_concurrent_subagents = v.max(1);
    }
    if let Some(v) = common.subagent_timeout {
        runtime.subagent_timeout = std::time::Duration::from_secs(v);
    }
    if common.show_subagent_events {
        runtime.event_visibility = code_agent_core::SubagentEventVisibility::StreamChildEvents;
    }

    if common.no_agents {
        runtime.enabled = false;
        return Ok((registry, runtime));
    }

    // Optional built-in starter set, if requested in settings.
    let allow_filesystem = settings
        .agents
        .as_ref()
        .and_then(|s| s.allow_filesystem_agents)
        .unwrap_or(true);

    if let Some(builtin_names) = settings.agents.as_ref().and_then(|s| s.builtins.as_ref()) {
        let starters = code_agent_core::builtin_starter_definitions();
        for name in builtin_names {
            if let Some(d) = starters.iter().find(|d| &d.name == name) {
                registry.register_builtin(d.clone()).ok();
            }
        }
    }

    // Filesystem definitions.
    if allow_filesystem {
        let dir = if let Some(p) = &common.agents {
            p.clone()
        } else if let Some(p) = settings.agents.as_ref().and_then(|s| s.path.clone()) {
            cwd.join(p)
        } else {
            code_agent_core::filesystem_agents_dir(cwd)
        };
        let (defs, errors) = code_agent_core::load_filesystem_agents(&dir);
        for d in defs {
            if let Err(e) = registry.register_filesystem(d) {
                eprintln!("[agents] {e}");
            }
        }
        for e in errors {
            eprintln!("[agents] {e}");
        }
    }

    // If the operator hasn't pinned `agents.enabled` either way and the
    // registry has at least one agent, default to enabled. We MUST NOT
    // overwrite an explicit `false` from settings — that's how the
    // operator turns subagents off without deleting their definitions.
    let explicit_enabled = settings.agents.as_ref().and_then(|s| s.enabled).is_some();
    if !registry.is_empty() && !runtime.enabled && !explicit_enabled {
        runtime.enabled = true;
    }
    Ok((registry, runtime))
}

fn tool_search_options_from_common(
    settings: &Settings,
    common: &CommonArgs,
) -> Result<ToolSearchOptions> {
    let mut options = ToolSearchOptions::default();
    options
        .apply_settings(&settings.tool_search)
        .map_err(|e| anyhow!("{e}"))?;
    apply_tool_search_cli_overrides(
        &mut options,
        common.tool_loading.as_deref(),
        common.tool_search_strategy.as_deref(),
        common.native_tool_search.as_deref(),
        common.tool_search_threshold,
        common.max_tool_search_results,
        common.max_loaded_tools,
        common.max_loaded_tool_tokens,
        &common.always_load,
        &common.defer_tools,
        &common.disable_tools,
    )?;
    Ok(options)
}

#[allow(clippy::too_many_arguments)]
fn apply_tool_search_cli_overrides(
    options: &mut ToolSearchOptions,
    loading_mode: Option<&str>,
    strategy: Option<&str>,
    native: Option<&str>,
    threshold: Option<u8>,
    max_results: Option<usize>,
    max_loaded_tools: Option<usize>,
    max_loaded_tool_tokens: Option<usize>,
    always_load: &[String],
    defer_tools: &[String],
    disable_tools: &[String],
) -> Result<()> {
    if let Some(strategy) = strategy {
        options.strategy = parse_tool_search_strategy(strategy)?;
    }
    if let Some(native) = native {
        options.native_provider_mode = parse_native_tool_search(native)?;
    }
    if let Some(max_results) = max_results {
        options.max_results = max_results.max(1);
    }
    if let Some(max_loaded_tools) = max_loaded_tools {
        options.max_loaded_tools = max_loaded_tools.max(1);
    }
    if let Some(max_loaded_tool_tokens) = max_loaded_tool_tokens {
        options.max_loaded_tool_tokens = max_loaded_tool_tokens.max(1);
    }
    if !always_load.is_empty() {
        options.always_loaded = always_load.to_vec();
        if !options
            .always_loaded
            .iter()
            .any(|name| name == SEARCH_TOOLS_NAME)
        {
            options.always_loaded.push(SEARCH_TOOLS_NAME.into());
        }
    }
    if !defer_tools.is_empty() {
        options.deferred = defer_tools.to_vec();
    }
    if !disable_tools.is_empty() {
        options.disabled = disable_tools.to_vec();
    }
    if let Some(mode) = loading_mode {
        options.mode = parse_tool_loading_mode(
            mode,
            threshold.unwrap_or(options.activation_threshold_percent()),
            options.min_tool_count(),
        )?;
    } else if let Some(threshold) = threshold {
        options.mode = ToolLoadingMode::Auto {
            threshold_percent_of_context: threshold,
            min_tool_count: options.min_tool_count(),
        };
    }
    Ok(())
}

fn build_settings_summary(
    s: &Settings,
    model: &str,
    mode: PermissionMode,
    mcp: &code_agent_mcp::McpOptions,
) -> String {
    serde_json::json!({
        "model": model,
        "permission_mode": format!("{:?}", mode),
        "allowed_tools_count": s.allowed_tools.len(),
        "disallowed_tools_count": s.disallowed_tools.len(),
        "mcp_settings_present": s.mcp.is_some(),
        "mcp": mcp.redacted_summary(),
        "tool_search": s.tool_search,
        "settings_source": format!("{:?}", s.source),
        "project_path": s.project_path,
        "user_path": s.user_path,
    })
    .to_string()
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    let prompt = match maybe_handle_slash_command(&cfg, &args.prompt).await? {
        SlashOutcome::HandledLocally => return Ok(()),
        SlashOutcome::Rewrite(p) => p,
        SlashOutcome::Passthrough => args.prompt.clone(),
    };
    run_one_shot(cfg, args.common.no_session, prompt, None, None).await
}

/// Result of attempting to dispatch a slash command from a CLI prompt.
enum SlashOutcome {
    /// The slash command was handled entirely locally (e.g. `/help`,
    /// `/skills`); no model turn is required.
    HandledLocally,
    /// The slash command was resolved to a Skill or custom command and
    /// the synthetic prompt should replace the original.
    Rewrite(String),
    /// Not a slash command — keep the prompt as-is for the model.
    Passthrough,
}

async fn maybe_handle_slash_command(cfg: &ResolvedRunConfig, raw: &str) -> Result<SlashOutcome> {
    let parsed = match code_agent_core::parse_slash_command(raw) {
        Ok(p) => p,
        Err(code_agent_core::SlashCommandError::NotASlashCommand) => {
            return Ok(SlashOutcome::Passthrough)
        }
        Err(e) => return Err(anyhow!("slash parse: {e}")),
    };
    let dispatch = match cfg.options.slash_commands.dispatch(parsed.clone()) {
        Ok(d) => d,
        Err(e) => return Err(anyhow!("slash dispatch: {e}")),
    };
    use code_agent_core::SlashCommandHandler;
    match dispatch.handler {
        SlashCommandHandler::BuiltIn(b) => {
            handle_builtin_slash(b, cfg).await?;
            Ok(SlashOutcome::HandledLocally)
        }
        SlashCommandHandler::Skill(name) => {
            // Refuse skill-backed dispatch when the parent has not been
            // granted the `Skill` tool. The model-facing `Skill` tool is
            // gated on `allowed_tools`, and the slash path must respect
            // the same gate so the CLI can't silently load instructions
            // that the agent loop would refuse to expose.
            if !skill_tool_is_allowed(&cfg.options) {
                return Err(anyhow!(
                    "skill `/{name}` requires the `Skill` tool to be in --allowed-tools \
                     (or matched by an allow rule)"
                ));
            }
            let state = code_agent_core::LoadedSkillsState::new();
            let result = code_agent_core::SkillTool::load(
                &cfg.options.skills,
                &cfg.options.skill_options,
                &state,
                &name,
            )
            .map_err(|e| anyhow!("skill {name}: {e}"))?;
            let def = cfg
                .options
                .skills
                .get(&name)
                .ok_or_else(|| anyhow!("skill {name} not registered"))?;
            let synthetic = code_agent_core::render_synthetic_skill_prompt(&parsed, def, &result);
            Ok(SlashOutcome::Rewrite(synthetic))
        }
        SlashCommandHandler::CustomFile(path) => {
            let def = cfg
                .options
                .slash_commands
                .custom(&parsed.full_name())
                .cloned()
                .ok_or_else(|| anyhow!("custom command not found at {}", path.display()))?;
            let executor = make_cli_expansion_executor(cfg)?;
            let opts = slash_expansion_options_from_settings(&cfg.settings);
            let result =
                code_agent_core::expand_custom_command(&parsed, &def, &opts, executor.as_ref())
                    .await
                    .map_err(|e| anyhow!("expansion failed: {e}"))?;
            Ok(SlashOutcome::Rewrite(result.expanded_prompt))
        }
    }
}

/// Result of dispatching a slash command in `chat`. The chat loop owns an
/// AgentClient instance, so built-in commands like `/compact` can act on
/// the live conversation directly instead of just emitting an info line
/// like the one-shot `run` path does.
enum ChatSlashOutcome {
    HandledLocally,
    Rewrite(String),
    Passthrough,
}

async fn maybe_handle_slash_command_for_chat(
    cfg: &ResolvedRunConfig,
    client: &mut AgentClient,
    raw: &str,
) -> Result<ChatSlashOutcome> {
    let parsed = match code_agent_core::parse_slash_command(raw) {
        Ok(p) => p,
        Err(code_agent_core::SlashCommandError::NotASlashCommand) => {
            return Ok(ChatSlashOutcome::Passthrough)
        }
        Err(e) => return Err(anyhow!("slash parse: {e}")),
    };
    let dispatch = match cfg.options.slash_commands.dispatch(parsed.clone()) {
        Ok(d) => d,
        Err(e) => return Err(anyhow!("slash dispatch: {e}")),
    };
    use code_agent_core::SlashCommandHandler;
    match dispatch.handler {
        SlashCommandHandler::BuiltIn(b) => {
            handle_builtin_slash_for_chat(b, cfg, client).await?;
            Ok(ChatSlashOutcome::HandledLocally)
        }
        SlashCommandHandler::Skill(name) => {
            if !skill_tool_is_allowed(&cfg.options) {
                return Err(anyhow!(
                    "skill `/{name}` requires the `Skill` tool to be in --allowed-tools"
                ));
            }
            let state = code_agent_core::LoadedSkillsState::new();
            let result = code_agent_core::SkillTool::load(
                &cfg.options.skills,
                &cfg.options.skill_options,
                &state,
                &name,
            )
            .map_err(|e| anyhow!("skill {name}: {e}"))?;
            let def = cfg
                .options
                .skills
                .get(&name)
                .ok_or_else(|| anyhow!("skill {name} not registered"))?;
            let synthetic = code_agent_core::render_synthetic_skill_prompt(&parsed, def, &result);
            Ok(ChatSlashOutcome::Rewrite(synthetic))
        }
        SlashCommandHandler::CustomFile(path) => {
            let def = cfg
                .options
                .slash_commands
                .custom(&parsed.full_name())
                .cloned()
                .ok_or_else(|| anyhow!("custom command not found at {}", path.display()))?;
            let executor = make_cli_expansion_executor(cfg)?;
            let opts = slash_expansion_options_from_settings(&cfg.settings);
            let result =
                code_agent_core::expand_custom_command(&parsed, &def, &opts, executor.as_ref())
                    .await
                    .map_err(|e| anyhow!("expansion failed: {e}"))?;
            Ok(ChatSlashOutcome::Rewrite(result.expanded_prompt))
        }
    }
}

async fn handle_builtin_slash_for_chat(
    b: code_agent_core::BuiltInSlashCommand,
    cfg: &ResolvedRunConfig,
    client: &mut AgentClient,
) -> Result<()> {
    use code_agent_core::BuiltInSlashCommand as B;
    match b {
        B::Compact => {
            let compact_cfg = compaction_options_from_settings(&cfg.settings);
            if !compact_cfg.enabled {
                println!("[compact] disabled by configuration");
                return Ok(());
            }
            match client.compact(compact_cfg).await {
                Ok(b) => {
                    println!(
                        "[compact] {} -> {} messages ({} preserved)",
                        b.pre_messages, b.post_messages, b.preserved_recent_turns
                    );
                }
                Err(e) => println!("[compact-error] {e}"),
            }
        }
        other => handle_builtin_slash(other, cfg).await?,
    }
    Ok(())
}

fn compaction_options_from_settings(settings: &Settings) -> code_agent_core::CompactionOptions {
    let cfg = settings.compaction.clone().unwrap_or_default();
    let mut opts = code_agent_core::CompactionOptions::default();
    if let Some(v) = cfg.enabled {
        opts.enabled = v;
    }
    if let Some(v) = cfg.preserve_recent_turns {
        opts.preserve_recent_turns = v;
    }
    if let Some(v) = cfg.max_summary_tokens {
        opts.max_summary_tokens = v;
    }
    opts
}

/// True iff the parent has been granted the `Skill` tool, either by name
/// in `--allowed-tools` or by a structured allow rule.
fn skill_tool_is_allowed(options: &AgentOptions) -> bool {
    options
        .allowed_tools
        .iter()
        .any(|n| n == code_agent_core::SKILL_TOOL_NAME)
        || options.allowed_tool_rules.iter().any(|r| {
            matches!(
                r,
                code_agent_core::ToolRule::ExactTool(n) if n == code_agent_core::SKILL_TOOL_NAME
            )
        })
}

/// Build [`SlashExpansionOptions`] from the merged settings. CLI overrides
/// land via `--no-slash-commands` (handled earlier) and the
/// `slash_commands.allow_*` / `max_*` knobs in `.agent/settings.json`.
fn slash_expansion_options_from_settings(
    settings: &Settings,
) -> code_agent_core::SlashExpansionOptions {
    let cfg = settings.slash_commands.clone().unwrap_or_default();
    let mut opts = code_agent_core::SlashExpansionOptions::default();
    if let Some(v) = cfg.allow_bash_expansion {
        opts.allow_bash_expansion = v;
    }
    if let Some(v) = cfg.allow_file_references {
        opts.allow_file_references = v;
    }
    if let Some(v) = cfg.max_expanded_prompt_bytes {
        opts.max_expanded_prompt_bytes = v;
    }
    if let Some(v) = cfg.max_expansion_output_bytes {
        opts.max_expansion_output_bytes = v;
    }
    opts
}

async fn handle_builtin_slash(
    b: code_agent_core::BuiltInSlashCommand,
    cfg: &ResolvedRunConfig,
) -> Result<()> {
    use code_agent_core::BuiltInSlashCommand as B;
    match b {
        B::Help => {
            println!("Available slash commands:");
            for s in cfg.options.slash_commands.list() {
                let n = match &s.namespace {
                    Some(ns) => format!("/{ns}:{}", s.name),
                    None => format!("/{}", s.name),
                };
                println!("  {:<20} {}", n, s.description);
            }
        }
        B::Context => {
            println!("cwd:          {}", cfg.options.cwd.display());
            println!("model:        {}", cfg.options.model);
            println!("permission:   {:?}", cfg.options.permission_mode);
            println!("max_turns:    {}", cfg.options.max_turns.unwrap_or(8));
        }
        B::Usage => {
            println!("(usage stats are emitted at the end of each run)");
        }
        B::Tools => {
            println!("Allowed tools: {:?}", cfg.options.allowed_tools);
        }
        B::Skills => {
            for s in cfg.options.skills.list() {
                println!("  {} — {}", s.name, s.description);
            }
        }
        B::Agents => {
            for s in cfg.options.agents.list() {
                println!("  {} — {}", s.name, s.description);
            }
        }
        B::Checkpoints => {
            println!(
                "(use `code-agent checkpoints list --cwd {}` to inspect checkpoints)",
                cfg.options.cwd.display()
            );
        }
        B::Compact => {
            println!(
                "(in one-shot mode there's no live conversation to compact; \
                 use `/compact` from `code-agent chat`)"
            );
        }
    }
    Ok(())
}

fn make_cli_expansion_executor(
    cfg: &ResolvedRunConfig,
) -> Result<Arc<dyn code_agent_core::slash::expansion::SlashExpansionExecutor>> {
    // Build the ReadTool and BashTool with the same limits the agent loop
    // uses. Routing expansion through these tools means slash `@file` and
    // `!`bash`` reuse the existing path sandbox, output cap, timeout, and
    // secret-redaction logic — there is no shadow code path for the
    // riskiest expansion surface to bypass.
    let limits = std::sync::Arc::new(tool_limits_from_settings(&cfg.settings));
    Ok(Arc::new(CliExpansionExecutor {
        cwd: cfg.options.cwd.clone(),
        session_id: cfg.options.session_id.clone().unwrap_or_default(),
        read: std::sync::Arc::new(code_agent_tools::ReadTool::new(limits.clone())),
        bash: std::sync::Arc::new(code_agent_tools::BashTool::new(limits)),
        hooks: cfg.hooks.clone(),
        secret_redaction: cfg.settings.hooks.secret_redaction.unwrap_or(true),
    }))
}

fn tool_limits_from_settings(settings: &Settings) -> ToolLimits {
    let mut limits = ToolLimits::default();
    if let Some(v) = settings.tool_limits.max_read_bytes {
        limits.max_read_bytes = v;
    }
    if let Some(v) = settings.tool_limits.max_output_bytes {
        limits.max_output_bytes = v;
    }
    if let Some(v) = settings.tool_limits.bash_timeout_seconds {
        limits.bash_timeout = std::time::Duration::from_secs(v);
    }
    if let Some(v) = settings.tool_limits.max_glob_results {
        limits.max_glob_results = v;
    }
    if let Some(v) = settings.tool_limits.max_grep_results {
        limits.max_grep_results = v;
    }
    limits
}

struct CliExpansionExecutor {
    cwd: PathBuf,
    session_id: SessionId,
    read: std::sync::Arc<code_agent_tools::ReadTool>,
    bash: std::sync::Arc<code_agent_tools::BashTool>,
    hooks: Hooks,
    secret_redaction: bool,
}

impl CliExpansionExecutor {
    /// Run the configured PreToolUse hooks. Returns an error if any hook
    /// denies, so denials surface through the same `SlashCommandFailed`
    /// path as other expansion failures.
    async fn check_pre_tool_use(
        &self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<(), code_agent_core::SlashCommandError> {
        use code_agent_core::HookEventName;
        let hook_input = code_agent_core::HookInput {
            event: HookEventName::PreToolUse,
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            run_id: code_agent_core::RunId::new(),
            turn_index: 0,
            tool_name: Some(tool_name.to_string()),
            tool_input: Some(input),
            tool_result: None,
            prompt: None,
            stop_reason: None,
            metadata: serde_json::Value::Null,
        };
        let records = self.hooks.dispatch(&hook_input).await;
        let (decision, _) = code_agent_core::aggregate(&records);
        if let code_agent_core::HookAggregateDecision::Deny { reason } = decision {
            return Err(code_agent_core::SlashCommandError::ExpansionFailed(
                format!("pre-tool-use hook denied: {reason}"),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl code_agent_core::slash::expansion::SlashExpansionExecutor for CliExpansionExecutor {
    async fn read_file(
        &self,
        path: &str,
    ) -> std::result::Result<String, code_agent_core::SlashCommandError> {
        let input = serde_json::json!({"file_path": path});
        self.check_pre_tool_use("Read", input.clone()).await?;
        let ctx =
            code_agent_core::ToolContext::new_minimal(self.cwd.clone(), self.session_id.clone());
        use code_agent_core::Tool;
        let result = self
            .read
            .execute(input, ctx)
            .await
            .map_err(|e| code_agent_core::SlashCommandError::ExpansionFailed(e.to_string()))?;
        if result.is_error {
            return Err(code_agent_core::SlashCommandError::ExpansionFailed(
                result.flatten_text(),
            ));
        }
        Ok(maybe_redact(result.flatten_text(), self.secret_redaction))
    }

    async fn run_bash(
        &self,
        command: &str,
    ) -> std::result::Result<String, code_agent_core::SlashCommandError> {
        let input = serde_json::json!({"command": command});
        self.check_pre_tool_use("Bash", input.clone()).await?;
        let ctx =
            code_agent_core::ToolContext::new_minimal(self.cwd.clone(), self.session_id.clone());
        use code_agent_core::Tool;
        let result = self
            .bash
            .execute(input, ctx)
            .await
            .map_err(|e| code_agent_core::SlashCommandError::ExpansionFailed(e.to_string()))?;
        if result.is_error {
            return Err(code_agent_core::SlashCommandError::ExpansionFailed(
                result.flatten_text(),
            ));
        }
        Ok(maybe_redact(result.flatten_text(), self.secret_redaction))
    }
}

/// Apply the project's secret-redaction pass to a captured string,
/// matching the hook applied to live tool results.
fn maybe_redact(text: String, enabled: bool) -> String {
    if !enabled {
        return text;
    }
    let v = serde_json::Value::String(text);
    let redacted = code_agent_core::redact_secrets_in_value(&v);
    redacted.as_str().map(str::to_string).unwrap_or_default()
}

async fn cmd_resume(args: ResumeArgs) -> Result<()> {
    let cfg = resolve_common(&args.common, true).await?;
    let session_id = parse_session_id(&args.session_id)?;
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
    let source = parse_session_id(&args.session_id)?;
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
    let validation = cfg.validation.clone();
    let checkpointing = cfg.checkpointing.clone();
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

    let (initial_history, preloaded_discovered_tools) =
        if let Some(s) = store.as_ref().or(read_store.as_ref()) {
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
            let discovered = discovered_tools_from_records(&records);
            let conv = replay_conversation(&records);
            (
                drop_trailing_matching_user_prompt(conv.messages, &prompt),
                discovered,
            )
        } else {
            (Vec::new(), Vec::new())
        };

    if let Some(s) = &store {
        let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &prompt).await;
    }

    let mut options = cfg.options.clone();
    options.session_id = Some(session_id.clone());
    for tool_name in preloaded_discovered_tools {
        if !options
            .tool_search
            .preloaded_discovered_tools
            .contains(&tool_name)
        {
            options
                .tool_search
                .preloaded_discovered_tools
                .push(tool_name);
        }
    }

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

    // Set up checkpointing. The store is created when checkpointing is on.
    let (checkpoint_handle, checkpoint_store_for_validation, run_checkpoint_id): RunCheckpointSetup =
        if checkpointing.enabled && checkpointing.create_on_user_message {
        let store = Arc::new(LocalCheckpointStore::with_limits(
            cfg.cwd.clone(),
            checkpointing.limits.clone(),
        ));
        let run_id = RunId::new();
        let git_state = code_agent_checkpoint::probe_git_state(&cfg.cwd).await;
        let checkpoint_id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                cwd: cfg.cwd.clone(),
                description: Some("user message".to_string()),
                user_message_id: None,
                git_state: git_state.clone(),
            })
            .await
            .map_err(|e| anyhow!("failed to create checkpoint: {e}"))?;
        if !quiet {
            println!("[checkpoint] created {} before user request", checkpoint_id);
        }
        let handle: Arc<dyn CheckpointHandle> = Arc::new(AgentCheckpointHandle::new(
            store.clone(),
            session_id.clone(),
            run_id,
            cfg.cwd.clone(),
            checkpoint_id.clone(),
            checkpointing.clone(),
        ));
        (Some(handle), Some(store), Some(checkpoint_id.to_string()))
    } else {
        (None, None, None)
    };

    let mut builder = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(cfg.tools)
        .hooks(cfg.hooks)
        .initial_history({
            let _ = &store;
            initial_history
        });
    if !no_session {
        let fs_store = Arc::new(FsSessionStore::new(cfg.cwd.clone()));
        let factory = Arc::new(code_agent_session::FsChildSessionSinkFactory::new(
            fs_store,
            cfg.cwd.clone(),
        ));
        builder = builder.child_session_sink_factory(factory);
    }
    if let Some(sink) = session_sink {
        builder = builder.session_sink(sink);
    }
    if let Some(t) = telemetry_sink {
        builder = builder.telemetry_sink(t);
    }
    if let Some(h) = checkpoint_handle {
        builder = builder.checkpoint_handle(h);
    }
    let agent = builder.build()?;

    let mut stream = agent.run(prompt);
    let exit_ok = drain_events_inline(&mut stream, log_format, show_usage, quiet).await;

    // Post-run validation gates. When validation is configured, its
    // outcome must affect the CLI exit code so `--validate "cargo test"`
    // is meaningful in CI.
    let mut validation_passed = true;
    if validation.is_enabled() {
        let report = code_agent_checkpoint::validation::run_validation(
            &cfg.cwd,
            &validation.commands,
            validation.timeout,
        )
        .await;
        validation_passed = report.passed;
        if !quiet {
            for r in &report.results {
                println!(
                    "[validation] {} -> exit={:?} timed_out={} ({}ms)",
                    r.command, r.exit_code, r.timed_out, r.duration_ms
                );
            }
            if report.passed {
                println!("[validation] passed");
            } else {
                println!("[validation] failed");
            }
        }
        if !report.passed && validation.rewind_on_failure {
            if let (Some(store), Some(id_str)) =
                (&checkpoint_store_for_validation, &run_checkpoint_id)
            {
                let id = code_agent_checkpoint::CheckpointId::from(id_str.clone());
                if !quiet {
                    println!("[rewind] validation failed; rewinding to {}", id);
                }
                match store
                    .rewind(RewindRequest {
                        checkpoint_id: id.clone(),
                        cwd: cfg.cwd.clone(),
                        force_cwd: false,
                        dry_run: false,
                    })
                    .await
                {
                    Ok(report) => {
                        if !quiet {
                            for p in &report.restored_files {
                                println!("[rewind] restored {}", p);
                            }
                            for p in &report.recreated_files {
                                println!("[rewind] recreated {}", p);
                            }
                            for p in &report.deleted_files {
                                println!("[rewind] deleted {}", p);
                            }
                            for s in &report.skipped_files {
                                println!("[rewind] skipped {} ({})", s.path, s.reason);
                            }
                            println!("[rewind] complete");
                            println!("[note] conversation history was not rewound");
                        }
                    }
                    Err(e) => {
                        eprintln!("[rewind] failed: {}", e);
                    }
                }
            }
        }
    }

    if !exit_ok {
        return Err(anyhow!("agent did not finish successfully"));
    }
    if !validation_passed {
        return Err(anyhow!("validation failed"));
    }
    Ok(())
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

fn discovered_tools_from_records(records: &[code_agent_session::SessionRecord]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for record in records {
        if let SessionPayload::Event(AgentEvent::ToolLoaded(loaded)) = &record.payload {
            names.insert(loaded.tool_name.clone());
        }
    }
    names.into_iter().collect()
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

    // Optional checkpointing for chat: a rotating handle whose active id is
    // refreshed before each user prompt below.
    let (checkpoint_store, rotating_handle, checkpoint_api): ChatCheckpointSetup =
        if cfg.checkpointing.enabled && cfg.checkpointing.create_on_user_message {
            let store = Arc::new(LocalCheckpointStore::with_limits(
                cfg.cwd.clone(),
                cfg.checkpointing.limits.clone(),
            ));
            let initial_run = RunId::new();
            let initial_id = store
                .create_checkpoint(CreateCheckpointRequest {
                    session_id: session_id.clone(),
                    run_id: initial_run.clone(),
                    cwd: cfg.cwd.clone(),
                    description: Some("chat session start".to_string()),
                    user_message_id: None,
                    git_state: code_agent_checkpoint::probe_git_state(&cfg.cwd).await,
                })
                .await
                .map_err(|e| anyhow!("create checkpoint: {e}"))?;
            if !cfg.quiet {
                println!("[checkpoint] created {} (chat start)", initial_id);
            }
            let rot = Arc::new(code_agent_checkpoint::RotatingCheckpointHandle::new(
                store.clone(),
                session_id.clone(),
                cfg.cwd.clone(),
                initial_id,
                initial_run,
                cfg.checkpointing.clone(),
            ));
            let api: Arc<dyn CheckpointApi> = Arc::new(LocalCheckpointApi::new(
                store.clone(),
                session_id.clone(),
                cfg.cwd.clone(),
            ));
            (Some(store), Some(rot), Some(api))
        } else {
            (None, None, None)
        };

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
    // Honor --no-session: when the user explicitly opts out of session
    // persistence, also opt out of child-session persistence so the child
    // transcripts don't show up on disk surprisingly.
    if !args.common.no_session {
        let fs_store = Arc::new(FsSessionStore::new(cfg.cwd.clone()));
        let factory = Arc::new(code_agent_session::FsChildSessionSinkFactory::new(
            fs_store,
            cfg.cwd.clone(),
        ));
        client_builder = client_builder.child_session_sink_factory(factory);
    }
    if let Some(t) = telemetry_sink {
        client_builder = client_builder.telemetry_sink(t);
    }
    if let Some(h) = rotating_handle.clone() {
        let h: Arc<dyn CheckpointHandle> = h;
        client_builder = client_builder.checkpoint_handle(h);
    }
    if let Some(api) = checkpoint_api {
        client_builder = client_builder.checkpoint_api(api);
    }

    let mut client = client_builder.build().await?;
    let _ = checkpoint_store;

    if !cfg.quiet {
        println!("[chat] session {}", session_id);
        println!("[chat] enter prompts; Ctrl-D to exit, ':q' to quit, ':interrupt' to cancel current run.");
    }

    if let Some(initial) = args.prompt.clone() {
        match maybe_handle_slash_command_for_chat(&cfg, &mut client, &initial).await? {
            ChatSlashOutcome::HandledLocally => {}
            ChatSlashOutcome::Rewrite(prompt) => {
                if let Some(s) = &store {
                    let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &prompt).await;
                }
                rotate_checkpoint(rotating_handle.as_ref(), &cfg.cwd, &session_id, cfg.quiet).await;
                client.send_user_message(prompt).await?;
            }
            ChatSlashOutcome::Passthrough => {
                if let Some(s) = &store {
                    let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &initial).await;
                }
                rotate_checkpoint(rotating_handle.as_ref(), &cfg.cwd, &session_id, cfg.quiet).await;
                client.send_user_message(initial).await?;
            }
        }
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
                        // Dispatch slash commands at the prompt boundary so
                        // built-ins like `/help` and `/compact` work in chat
                        // and Skill / custom-command rewrites flow through
                        // the same path as one-shot `run`.
                        match maybe_handle_slash_command_for_chat(&cfg, &mut client, trimmed)
                            .await
                        {
                            Ok(ChatSlashOutcome::HandledLocally) => continue,
                            Ok(ChatSlashOutcome::Rewrite(prompt)) => {
                                if let Some(s) = &store {
                                    let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, &prompt).await;
                                }
                                rotate_checkpoint(rotating_handle.as_ref(), &cfg.cwd, &session_id, cfg.quiet).await;
                                client.send_user_message(prompt).await?;
                            }
                            Ok(ChatSlashOutcome::Passthrough) => {
                                if let Some(s) = &store {
                                    let _ = append_user_prompt(s.as_ref(), &cfg.cwd, &session_id, trimmed).await;
                                }
                                rotate_checkpoint(rotating_handle.as_ref(), &cfg.cwd, &session_id, cfg.quiet).await;
                                client.send_user_message(trimmed.to_string()).await?;
                            }
                            Err(e) => {
                                eprintln!("[slash-error] {e}");
                            }
                        }
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

async fn cmd_mcp(args: McpCommand) -> Result<()> {
    match args {
        McpCommand::List(common) => {
            let options = resolve_mcp_cli_options(&common).await?;
            for (name, server) in &options.servers {
                println!("{}\t{}", name, code_agent_core::mcp::transport_kind(server));
            }
            Ok(())
        }
        McpCommand::Status(common) => {
            let options = resolve_mcp_cli_options(&common).await?;
            let mut registry = ToolRegistry::new();
            let report = code_agent_core::register_mcp_tools(&mut registry, &options).await;
            println!("SERVER\tTRANSPORT\tSTATUS\tTOOLS\tVERSION");
            for status in report.statuses {
                println!(
                    "{}\t{}\t{:?}\t{}\t{}",
                    status.name,
                    status.transport,
                    status.status,
                    status.tool_count,
                    status.protocol_version.unwrap_or_else(|| "-".into())
                );
                if let Some(error) = status.error {
                    println!("[mcp] {} failed: {}", status.name, error);
                }
            }
            Ok(())
        }
        McpCommand::Tools { server, common } => {
            let options = resolve_mcp_cli_options(&common).await?;
            let mut client = connect_single_mcp_server(&options, &server).await?;
            let tools = client.list_tools().await.map_err(|e| anyhow!("{e}"))?;
            for tool in tools {
                println!(
                    "{}\t{}",
                    tool.name,
                    tool.description.unwrap_or_default().replace('\n', " ")
                );
            }
            let _ = client.shutdown().await;
            Ok(())
        }
        McpCommand::Call {
            server,
            tool,
            args_json,
            yes,
            common,
        } => {
            if !yes {
                return Err(anyhow!(
                    "direct MCP calls require --yes because effects are external and not checkpointed"
                ));
            }
            let options = resolve_mcp_cli_options(&common).await?;
            let args: serde_json::Value = serde_json::from_str(&args_json)
                .with_context(|| format!("parse --args-json for {server}.{tool}"))?;
            let mut client = connect_single_mcp_server(&options, &server).await?;
            println!("[mcp] {}.{}", server, tool);
            println!(
                "[warning] MCP tool effects are external and are not covered by workspace checkpointing."
            );
            let result = client
                .call_tool(&tool, args)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let (text, metadata, _, _) = code_agent_mcp::mcp_result_to_text_and_metadata(
                &result,
                options.max_tool_result_bytes,
            )
            .map_err(|e| anyhow!("{e}"))?;
            if let Some(structured) = metadata
                .get("mcp")
                .and_then(|m| m.get("structured_content"))
                .filter(|v| !v.is_null())
            {
                println!("{}", serde_json::to_string_pretty(structured)?);
            } else {
                println!("{text}");
            }
            let _ = client.shutdown().await;
            Ok(())
        }
        McpCommand::Resources { server, common } => {
            let options = resolve_mcp_cli_options(&common).await?;
            let mut client = connect_single_mcp_server(&options, &server).await?;
            let resources = client.list_resources().await.map_err(|e| anyhow!("{e}"))?;
            for resource in resources {
                println!(
                    "{}\t{}\t{}",
                    resource.uri,
                    resource.name.unwrap_or_default(),
                    resource.mime_type.unwrap_or_default()
                );
            }
            let _ = client.shutdown().await;
            Ok(())
        }
        McpCommand::Resource(McpResourceCommand::Read {
            server,
            uri,
            common,
        }) => {
            let options = resolve_mcp_cli_options(&common).await?;
            let mut client = connect_single_mcp_server(&options, &server).await?;
            let contents = client
                .read_resource(&uri)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let mut rendered = serde_json::to_string_pretty(&contents)?;
            if rendered.len() > options.max_tool_result_bytes {
                rendered.truncate(options.max_tool_result_bytes);
                rendered.push_str("\n[MCP resource output truncated]");
            }
            println!("{rendered}");
            let _ = client.shutdown().await;
            Ok(())
        }
        McpCommand::Prompts { server, common } => {
            let options = resolve_mcp_cli_options(&common).await?;
            let mut client = connect_single_mcp_server(&options, &server).await?;
            let prompts = client.list_prompts().await.map_err(|e| anyhow!("{e}"))?;
            for prompt in prompts {
                println!(
                    "{}\t{}\targs={}",
                    prompt.name,
                    prompt.description.unwrap_or_default().replace('\n', " "),
                    prompt.arguments.len()
                );
            }
            let _ = client.shutdown().await;
            Ok(())
        }
    }
}

struct ToolsCatalogContext {
    catalog: ToolCatalog,
    state: code_agent_core::LoadedToolState,
    options: ToolSearchOptions,
    settings: Settings,
    permission_mode: PermissionMode,
    /// Effective simple-name allowlist (mirrors `AgentOptions::allowed_tools`).
    /// Empty means "no simple-name allowlist" — registry was not narrowed.
    allowed_tool_names: Vec<String>,
    mcp_external_effect_policy: code_agent_mcp::McpExternalEffectPolicy,
}

async fn cmd_tools(args: ToolsCommand) -> Result<()> {
    match args {
        ToolsCommand::List(args) => {
            let ctx = build_tools_catalog(&args.common).await?;
            let filter = if args.loaded {
                ToolListFilter::Loaded
            } else if args.deferred {
                ToolListFilter::Deferred
            } else if args.disabled {
                ToolListFilter::Disabled
            } else {
                let _ = args.all;
                ToolListFilter::All
            };
            let previews = ctx.catalog.list_previews(filter, &ctx.state, &ctx.options);
            println!("NAME\tSOURCE\tCATEGORY\tRISK\tPERMISSION\tDESCRIPTION");
            for p in previews {
                println!(
                    "{}\t{}\t{}\t{:?}\t{:?}\t{}",
                    p.name,
                    p.source.label(),
                    p.category.unwrap_or_else(|| "-".into()),
                    p.risk_profile,
                    p.permission_kind,
                    p.description.replace('\n', " ")
                );
            }
            Ok(())
        }
        ToolsCommand::Search(args) => {
            let mut ctx = build_tools_catalog(&args.common).await?;
            if let Some(strategy) = args.strategy.as_deref() {
                ctx.options.strategy = parse_tool_search_strategy(strategy)?;
            }
            let source_filter = args
                .source
                .as_deref()
                .map(ToolSourceFilter::parse)
                .transpose()
                .map_err(|e| anyhow!("{e}"))?;
            let visibility = ctx.options.visibility_context(
                ctx.permission_mode,
                ctx.settings.allowed_tools.clone(),
                ctx.settings.disallowed_tools.clone(),
                ctx.allowed_tool_names.clone(),
                ctx.mcp_external_effect_policy,
            );
            let result = ctx.catalog.search(
                ToolSearchQuery {
                    query: args.query,
                    limit: args.limit.unwrap_or(ctx.options.max_results),
                    source_filter,
                    include_risky: true,
                },
                ctx.options.strategy,
                &visibility,
            )?;
            println!("NAME\tSCORE\tVISIBILITY\tSOURCE\tREASON\tDESCRIPTION");
            for m in result.matches {
                println!(
                    "{}\t{:.2}\t{:?}\t{}\t{}\t{}",
                    m.tool_name,
                    m.score,
                    m.visibility,
                    m.descriptor_preview.source.label(),
                    m.reason,
                    m.descriptor_preview.description.replace('\n', " ")
                );
            }
            Ok(())
        }
        ToolsCommand::Show(args) => {
            let ctx = build_tools_catalog(&args.common).await?;
            let descriptor = ctx
                .catalog
                .get(&args.tool)
                .ok_or_else(|| anyhow!("unknown tool {}", args.tool))?;
            println!("{}", serde_json::to_string_pretty(descriptor)?);
            Ok(())
        }
        ToolsCommand::Stats(common) => {
            let ctx = build_tools_catalog(&common).await?;
            let decision = ctx.catalog.resolve_loading_mode(&ctx.options);
            let loaded = ctx.catalog.loaded_tool_names(&ctx.state, &ctx.options);
            let deferred = ctx.catalog.deferred_tool_names(&ctx.state, &ctx.options);
            let loaded_tokens = ctx.catalog.estimated_tokens_for(&loaded);
            println!("CATALOG");
            println!("  total tools:        {}", ctx.catalog.len());
            println!(
                "  eager tools:        {}",
                ctx.catalog
                    .descriptors()
                    .filter(|d| d.load_policy == code_agent_core::ToolLoadPolicy::Always)
                    .count()
            );
            println!(
                "  deferred tools:     {}",
                ctx.catalog
                    .descriptors()
                    .filter(|d| d.load_policy == code_agent_core::ToolLoadPolicy::Deferred)
                    .count()
            );
            println!(
                "  disabled tools:     {}",
                ctx.catalog
                    .descriptors()
                    .filter(|d| d.load_policy == code_agent_core::ToolLoadPolicy::Disabled)
                    .count()
            );
            println!(
                "  estimated tokens:   {}",
                ctx.catalog.estimated_total_tokens()
            );
            println!("  loaded tokens:      {}", loaded_tokens);
            println!();
            println!("SOURCES");
            let mut sources = std::collections::BTreeMap::<String, usize>::new();
            for descriptor in ctx.catalog.descriptors() {
                *sources.entry(descriptor.source.label()).or_default() += 1;
            }
            for (source, count) in sources {
                println!("  {source}:              {count}");
            }
            println!();
            println!("SEARCH");
            println!("  mode:               {}", ctx.options.mode);
            println!("  resolved mode:      {}", decision.resolved_mode);
            println!("  strategy:           {}", ctx.options.strategy);
            println!("  max results:        {}", ctx.options.max_results);
            println!("  loaded tools:       {}", loaded.len());
            println!("  deferred tools:     {}", deferred.len());
            Ok(())
        }
        ToolsCommand::ExplainLoading(common) => {
            let ctx = build_tools_catalog(&common).await?;
            let decision = ctx.catalog.resolve_loading_mode(&ctx.options);
            println!("TOOL LOADING DECISION");
            println!("  mode: {}", ctx.options.mode);
            println!("  resolved: {}", decision.resolved_mode);
            println!("  reason: {}", decision.reason);
            println!(
                "  total estimated tokens: {}",
                ctx.catalog.estimated_total_tokens()
            );
            println!(
                "  threshold: {}% of context",
                ctx.options.activation_threshold_percent()
            );
            println!();
            println!("ALWAYS LOADED");
            for descriptor in ctx
                .catalog
                .descriptors()
                .filter(|d| d.load_policy == code_agent_core::ToolLoadPolicy::Always)
            {
                println!("  {}", descriptor.name);
            }
            println!();
            println!("DEFERRED");
            let deferred = ctx
                .catalog
                .descriptors()
                .filter(|d| d.load_policy == code_agent_core::ToolLoadPolicy::Deferred)
                .count();
            println!("  {} tools", deferred);
            Ok(())
        }
    }
}

async fn build_tools_catalog(common: &ToolsCommonArgs) -> Result<ToolsCatalogContext> {
    let cwd = common.cwd.canonicalize().context("--cwd")?;
    let source = parse_setting_source(&common.settings)?;
    let mut settings = if let Some(path) = &common.config {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let file: code_agent_core::SettingsFile =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        let mut s = Settings {
            source: SettingSource::None,
            project_path: Some(path.clone()),
            ..Settings::default()
        };
        if let Some(rules) = file.allowed_tools {
            s.allowed_tools = rules;
        }
        if let Some(rules) = file.disallowed_tools {
            s.disallowed_tools = rules;
        }
        if let Some(mcp) = file.mcp {
            s.mcp = Some(mcp);
        }
        if let Some(tool_search) = file.tool_search {
            s.tool_search = tool_search;
        }
        s
    } else {
        code_agent_core::settings::load(&cwd, source).context("load settings")?
    };
    for allowed in &common.allowed_tools {
        settings.allowed_tools.push(parse_cli_rule(allowed)?);
    }
    for denied in &common.deny {
        settings.disallowed_tools.push(parse_cli_rule(denied)?);
    }
    let mut options = ToolSearchOptions::default();
    options
        .apply_settings(&settings.tool_search)
        .map_err(|e| anyhow!("{e}"))?;
    apply_tool_search_cli_overrides(
        &mut options,
        common.tool_loading.as_deref(),
        common.tool_search_strategy.as_deref(),
        common.native_tool_search.as_deref(),
        common.tool_search_threshold,
        common.max_tool_search_results,
        common.max_loaded_tools,
        common.max_loaded_tool_tokens,
        &common.always_load,
        &common.defer_tools,
        &common.disable_tools,
    )?;

    let mcp_timeout = common.mcp_timeout.map(Duration::from_secs);
    let mut mcp_options = code_agent_mcp::load_mcp_options(
        &cwd,
        settings.mcp.as_ref(),
        common.mcp_config.as_deref(),
        common.mcp_config.as_ref().map(|_| true),
        common.mcp_strict.then_some(true),
        mcp_timeout,
    )
    .context("load MCP config")?;
    if !common.mcp_servers.is_empty() {
        mcp_options
            .servers
            .retain(|name, _| common.mcp_servers.iter().any(|wanted| wanted == name));
    }

    let mut registry = ToolRegistry::new();
    register_builtin_with_limits(&mut registry, ToolLimits::default());
    registry.register(Arc::new(SearchToolsDefinitionTool::new(
        options.max_results,
    )));
    let report = code_agent_core::register_mcp_tools(&mut registry, &mcp_options).await;
    if mcp_options.strict
        && report
            .statuses
            .iter()
            .any(|s| matches!(s.status, code_agent_mcp::McpConnectionStatus::Failed))
    {
        return Err(anyhow!("strict MCP mode: one or more MCP servers failed"));
    }

    // Resolve permission mode: CLI flag wins over settings, then defaults.
    let permission_mode = if let Some(pm) = &common.permission_mode {
        parse_permission_mode(pm)?
    } else if let Some(pm) = settings.permission_mode {
        pm
    } else {
        PermissionMode::Default
    };

    // Mirror agent.rs registry filtering so `tools stats` / `explain-loading`
    // / `search` operate on the same effective tool set as the running
    // agent. An empty allowlist means "allow all" (matches
    // `AgentOptions::allowed_tools.is_empty()` semantics in agent.rs).
    let allowed_tool_names: Vec<String> = if !common.allowed_tools.is_empty() {
        common.allowed_tools.clone()
    } else {
        let mut out: Vec<String> = Vec::new();
        for r in &settings.allowed_tools {
            if let Some(pattern) = registry_filter_pattern_for_rule(r) {
                if !out.contains(&pattern) {
                    out.push(pattern);
                }
            }
        }
        out
    };
    let filtered_registry = if allowed_tool_names.is_empty() {
        registry.clone()
    } else {
        registry.filter(&allowed_tool_names)
    };

    let mut builder = ToolCatalogBuilder::new(options.clone()).ingest_registry(&filtered_registry);
    for status in &report.statuses {
        builder = builder.mcp_transport(status.name.clone(), status.transport);
    }
    let catalog = builder.build();
    let state = catalog.initial_loaded_state(&options);
    let mcp_external_effect_policy = mcp_options.external_effect_policy;
    Ok(ToolsCatalogContext {
        catalog,
        state,
        options,
        settings,
        permission_mode,
        allowed_tool_names,
        mcp_external_effect_policy,
    })
}

async fn resolve_mcp_cli_options(common: &McpCommonArgs) -> Result<code_agent_mcp::McpOptions> {
    let cwd = common.cwd.canonicalize().context("--cwd")?;
    let mut options = code_agent_mcp::load_mcp_options(
        &cwd,
        None,
        common.mcp_config.as_deref(),
        Some(true),
        common.mcp_strict.then_some(true),
        common.mcp_timeout.map(Duration::from_secs),
    )
    .context("load MCP config")?;
    if !common.mcp_servers.is_empty() {
        options
            .servers
            .retain(|name, _| common.mcp_servers.iter().any(|wanted| wanted == name));
    }
    Ok(options)
}

async fn connect_single_mcp_server(
    options: &code_agent_mcp::McpOptions,
    server: &str,
) -> Result<code_agent_mcp::McpClient> {
    let config = options
        .servers
        .get(server)
        .ok_or_else(|| anyhow!("unknown MCP server {server}"))?;
    let mut client = code_agent_mcp::McpClient::connect(
        server,
        config,
        options.limits(),
        options.secret_values(),
    )
    .await
    .map_err(|e| anyhow!("{e}"))?;
    client.initialize().await.map_err(|e| anyhow!("{e}"))?;
    Ok(client)
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
    let id = parse_session_id(&session_id)?;
    let summary = summarize_session(&store, &id).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn cmd_sessions_export(session_id: String, cwd: PathBuf, format: String) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let id = parse_session_id(&session_id)?;
    let fmt = ExportFormat::from_str_lenient(&format)
        .ok_or_else(|| anyhow!("invalid --format: {format}"))?;
    let s = export_session(&store, &id, fmt).await?;
    print!("{s}");
    Ok(())
}

async fn cmd_sessions_children(session_id: String, cwd: PathBuf) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let id = parse_session_id(&session_id)?;
    let children = store.list_children(&id).await?;
    if children.is_empty() {
        println!("(no child sessions for {})", id);
    } else {
        for c in children {
            println!("{c}");
        }
    }
    Ok(())
}

async fn cmd_sessions_show_child(child_id: String, cwd: PathBuf) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let child = parse_session_id(&child_id)?;
    let parent = store
        .find_parent_for_child(&child)
        .await?
        .ok_or_else(|| anyhow!("no parent session found for {child_id}"))?;
    let records = store.load_child(&parent, &child).await?;
    println!(
        "(parent={parent}, child={child}, records={})",
        records.len()
    );
    for r in &records {
        println!("{}", serde_json::to_string(r)?);
    }
    Ok(())
}

async fn cmd_sessions_export_child(child_id: String, cwd: PathBuf, format: String) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = FsSessionStore::new(cwd.clone());
    let child = parse_session_id(&child_id)?;
    let parent = store
        .find_parent_for_child(&child)
        .await?
        .ok_or_else(|| anyhow!("no parent session found for {child_id}"))?;
    let records = store.load_child(&parent, &child).await?;
    let fmt = ExportFormat::from_str_lenient(&format)
        .ok_or_else(|| anyhow!("invalid --format: {format}"))?;
    match fmt {
        ExportFormat::Jsonl => {
            for r in records {
                println!("{}", serde_json::to_string(&r)?);
            }
        }
        ExportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&records)?);
        }
    }
    Ok(())
}

async fn cmd_agents_list(common: AgentsCommonArgs) -> Result<()> {
    let registry = load_agent_registry_for_cli(&common)?;
    if registry.is_empty() {
        println!("(no subagents registered)");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<10} DESCRIPTION", "NAME", "TURNS", "TOOLS");
    for s in registry.list() {
        let tools = s
            .tool_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let turns = s
            .max_turns
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:<8} {:<10} {}",
            s.name,
            turns,
            tools,
            truncate(&s.description, 80)
        );
    }
    Ok(())
}

async fn cmd_agents_show(agent_name: String, common: AgentsCommonArgs) -> Result<()> {
    let registry = load_agent_registry_for_cli(&common)?;
    let def = registry
        .get(&agent_name)
        .ok_or_else(|| anyhow!("unknown agent {agent_name:?}"))?;
    println!("name:              {}", def.name);
    println!("description:       {}", def.description);
    if let Some(model) = &def.model {
        println!("model:             {model}");
    }
    println!("max_turns:         {}", def.effective_max_turns());
    if let Some(mode) = def.permission_mode {
        println!("permission_mode:   {:?}", mode);
    }
    if let Some(tools) = &def.tools {
        println!("tools:             {} rule(s)", tools.len());
        for r in tools {
            println!(
                "  - {}",
                serde_json::to_string(r).unwrap_or_else(|_| "?".into())
            );
        }
    } else {
        println!("tools:             (inherit parent allow set)");
    }
    if !def.disallowed_tools.is_empty() {
        println!("disallowed_tools:  {} rule(s)", def.disallowed_tools.len());
        for r in &def.disallowed_tools {
            println!(
                "  - {}",
                serde_json::to_string(r).unwrap_or_else(|_| "?".into())
            );
        }
    }
    if let Some(servers) = &def.mcp_servers {
        println!("mcp_servers:       {}", servers.join(", "));
    }
    println!();
    println!("--- prompt ---");
    let preview: String = def.prompt.chars().take(2048).collect();
    println!("{preview}");
    if def.prompt.len() > preview.len() {
        println!("…[truncated]");
    }
    Ok(())
}

async fn cmd_agents_validate(common: AgentsCommonArgs) -> Result<()> {
    let registry = load_agent_registry_for_cli(&common)?;
    if registry.is_empty() {
        println!("(no subagents registered — nothing to validate)");
        return Ok(());
    }
    // Build a tool registry the same way the runtime does so the
    // available-tool set tracks the real binary instead of a hardcoded
    // list. We cannot enumerate MCP tools without connecting to servers,
    // but glob rules and MCP rules are not flagged as "unknown" in the
    // registry validator (only `ExactTool` references are checked).
    let mut tools = code_agent_core::ToolRegistry::new();
    code_agent_tools::register_builtin_with_limits(
        &mut tools,
        code_agent_tools::ToolLimits::default(),
    );
    let mut available: std::collections::BTreeSet<String> = tools.names().into_iter().collect();
    available.insert("SearchTools".into());
    registry
        .validate_against_tools(&available)
        .map_err(|e| anyhow!("{e}"))?;
    println!("ok ({} agents)", registry.len());
    for s in registry.list() {
        println!("  {} - {}", s.name, truncate(&s.description, 80));
    }
    Ok(())
}

async fn cmd_agents_run(
    agent_name: String,
    task: String,
    common: AgentsCommonArgs,
    max_turns: Option<u32>,
    show_events: bool,
) -> Result<()> {
    let registry = load_agent_registry_for_cli(&common)?;
    let def = registry
        .get(&agent_name)
        .ok_or_else(|| anyhow!("unknown agent {agent_name:?}"))?;
    println!(
        "[agent] {} (model={}, tools={})",
        def.name,
        def.model.as_deref().unwrap_or("(inherit)"),
        def.tools
            .as_ref()
            .map(|t| t.len().to_string())
            .unwrap_or_else(|| "(inherit)".into())
    );
    println!("[agent] task: {}", truncate(&task, 200));
    if max_turns.is_some() || show_events {
        println!(
            "[agent] (max_turns={:?}, show_events={})",
            max_turns, show_events
        );
    }
    println!(
        "[agent] direct subagent execution requires a configured ModelProvider; \
         use `code-agent run` with `--allowed-tools Agent,...` and the parent agent will \
         delegate to this subagent through the Agent tool."
    );
    Ok(())
}

/// Resolve and load the Skills registry for a `run`/`chat` session.
fn load_skills_for_run(
    cwd: &Path,
    common: &CommonArgs,
    settings: &Settings,
) -> Result<code_agent_core::SkillRegistry> {
    if common.no_skills {
        return Ok(code_agent_core::SkillRegistry::new());
    }
    let cfg = settings.skills.clone().unwrap_or_default();
    if matches!(cfg.enabled, Some(false)) {
        return Ok(code_agent_core::SkillRegistry::new());
    }
    let project_dir = if let Some(p) = &common.skills {
        resolve_against_cwd(cwd, p)
    } else if let Some(p) = &cfg.project_path {
        resolve_against_cwd(cwd, p)
    } else {
        code_agent_core::filesystem_skills_dir(cwd)
    };
    let mut paths = vec![code_agent_core::SkillDiscoveryPath {
        source: code_agent_core::SkillSource::Project,
        path: project_dir,
    }];
    if let Some(user) = cfg.user_path.clone() {
        // User paths can be absolute or `~`-prefixed; treat relative as
        // relative to cwd to match project paths.
        paths.push(code_agent_core::SkillDiscoveryPath {
            source: code_agent_core::SkillSource::User,
            path: resolve_against_cwd(cwd, &user),
        });
    }
    let compat = common.compat_claude_skills || cfg.compat_claude_skills.unwrap_or(false);
    if compat {
        paths.push(code_agent_core::SkillDiscoveryPath {
            source: code_agent_core::SkillSource::CompatibilityClaude,
            path: code_agent_core::compat_claude_skills_dir(cwd),
        });
    }
    let opts = skill_options_for_run(common, settings);
    code_agent_core::SkillRegistry::discover(&paths, &opts)
        .map_err(|e| anyhow!("skills discover: {e}"))
}

fn skill_options_for_run(
    common: &CommonArgs,
    settings: &Settings,
) -> code_agent_core::SkillDiscoveryOptions {
    let mut opts = code_agent_core::SkillDiscoveryOptions::default();
    let cfg = settings.skills.clone().unwrap_or_default();
    if let Some(v) = cfg.allow_filesystem_skills {
        opts.allow_filesystem_skills = v;
    }
    opts.compat_claude_skills =
        common.compat_claude_skills || cfg.compat_claude_skills.unwrap_or(false);
    if let Some(v) = cfg.max_skill_md_bytes {
        opts.max_skill_md_bytes = v;
    }
    if let Some(v) = cfg.max_resource_bytes_per_skill {
        opts.max_resource_bytes_per_skill = v;
    }
    if let Some(v) = cfg.max_loaded_skills_per_run {
        opts.max_loaded_skills_per_run = v;
    }
    if let Some(v) = cfg.load_resources_on_demand {
        opts.load_resources_on_demand = v;
    }
    if let Some(v) = cfg.enforce_skill_allowed_tools {
        opts.enforce_skill_allowed_tools = v;
    }
    opts
}

fn load_slash_registry_for_run(
    cwd: &Path,
    common: &CommonArgs,
    settings: &Settings,
    skills: &code_agent_core::SkillRegistry,
) -> Result<code_agent_core::SlashCommandRegistry> {
    if common.no_slash_commands {
        return Ok(code_agent_core::SlashCommandRegistry::new(false));
    }
    let cfg = settings.slash_commands.clone().unwrap_or_default();
    if matches!(cfg.enabled, Some(false)) {
        return Ok(code_agent_core::SlashCommandRegistry::new(false));
    }
    let builtins = cfg.enable_builtin_commands.unwrap_or(true);
    let dir = if let Some(p) = &common.commands {
        resolve_against_cwd(cwd, p)
    } else if let Some(p) = &cfg.project_path {
        resolve_against_cwd(cwd, p)
    } else {
        code_agent_core::custom_commands_dir(cwd)
    };
    let (mut defs, errors) = code_agent_core::load_custom_commands(&dir);
    for e in errors {
        eprintln!("[commands] {e}");
    }
    // M7: optionally also load user-level and `.claude/commands/`
    // compatibility command files.
    if let Some(user) = cfg.user_path.clone() {
        let resolved = resolve_against_cwd(cwd, &user);
        let (mut user_defs, user_errs) = code_agent_core::load_custom_commands(&resolved);
        for e in user_errs {
            eprintln!("[commands:user] {e}");
        }
        defs.append(&mut user_defs);
    }
    let compat_commands =
        common.compat_claude_commands || cfg.compat_claude_commands.unwrap_or(false);
    if compat_commands {
        let claude = cwd.join(".claude").join("commands");
        let (mut compat_defs, compat_errs) = code_agent_core::load_custom_commands(&claude);
        for e in compat_errs {
            eprintln!("[commands:compat] {e}");
        }
        defs.append(&mut compat_defs);
    }
    let reg = code_agent_core::SlashCommandRegistry::new(builtins)
        .with_custom(defs)
        .with_skills(skills);
    Ok(reg)
}

/// Resolve a configured path against `cwd`. If `p` is absolute, return it
/// unchanged; if relative, join onto `cwd`. Mirrors the agents-path
/// handling so `--cwd examples/skills-demo` works regardless of where the
/// CLI was invoked from.
fn resolve_against_cwd(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Resolve the Skills directory and load the registry.
fn load_skills_for_cli(common: &SkillsCommonArgs) -> Result<code_agent_core::SkillRegistry> {
    let cwd = common
        .cwd
        .canonicalize()
        .with_context(|| format!("--cwd {}", common.cwd.display()))?;
    let dir = if let Some(p) = &common.skills {
        resolve_against_cwd(&cwd, p)
    } else {
        code_agent_core::filesystem_skills_dir(&cwd)
    };
    let mut paths = vec![code_agent_core::SkillDiscoveryPath {
        source: code_agent_core::SkillSource::Project,
        path: dir,
    }];
    if common.compat_claude_skills {
        paths.push(code_agent_core::SkillDiscoveryPath {
            source: code_agent_core::SkillSource::CompatibilityClaude,
            path: code_agent_core::compat_claude_skills_dir(&cwd),
        });
    }
    let opts = code_agent_core::SkillDiscoveryOptions {
        compat_claude_skills: common.compat_claude_skills,
        ..code_agent_core::SkillDiscoveryOptions::default()
    };
    code_agent_core::SkillRegistry::discover(&paths, &opts)
        .map_err(|e| anyhow!("skills discover: {e}"))
}

async fn cmd_skills_list(common: SkillsCommonArgs) -> Result<()> {
    let reg = load_skills_for_cli(&common)?;
    if reg.is_empty() {
        println!("(no skills registered)");
        return Ok(());
    }
    println!(
        "{:<24} {:<8} {:<10} DESCRIPTION",
        "NAME", "SOURCE", "RESOURCES"
    );
    for s in reg.list() {
        println!(
            "{:<24} {:<8} {:<10} {}",
            s.name,
            s.source.as_str(),
            s.resource_count,
            truncate(&s.description, 80)
        );
    }
    Ok(())
}

async fn cmd_skills_show(name: String, common: SkillsCommonArgs) -> Result<()> {
    let reg = load_skills_for_cli(&common)?;
    let def = reg
        .get(&name)
        .ok_or_else(|| anyhow!("unknown skill {name:?}"))?;
    println!("name:              {}", def.name);
    println!("description:       {}", def.description);
    println!("source:            {}", def.source.as_str());
    if let Some(hint) = &def.argument_hint {
        println!("argument-hint:     {hint}");
    }
    if !def.allowed_tools.is_empty() {
        println!("allowed-tools:     {} rule(s)", def.allowed_tools.len());
    }
    if !def.disallowed_tools.is_empty() {
        println!("disallowed-tools:  {} rule(s)", def.disallowed_tools.len());
    }
    if !def.resources.is_empty() {
        println!("resources:");
        for r in &def.resources {
            println!("  - {r}");
        }
    }
    println!();
    println!("--- instructions ---");
    let preview: String = def.instructions.chars().take(2048).collect();
    println!("{preview}");
    if def.instructions.len() > preview.len() {
        println!("…[truncated]");
    }
    Ok(())
}

async fn cmd_skills_validate(common: SkillsCommonArgs) -> Result<()> {
    let reg = load_skills_for_cli(&common)?;
    if reg.is_empty() {
        println!("(no skills to validate)");
        return Ok(());
    }
    let mut bad = 0usize;
    for (name, def) in reg.iter() {
        match def.validate_structural() {
            Ok(()) => println!("ok    {name}"),
            Err(e) => {
                println!("error {name}: {e}");
                bad += 1;
            }
        }
    }
    if bad > 0 {
        Err(anyhow!("{bad} skill(s) failed validation"))
    } else {
        Ok(())
    }
}

async fn cmd_skills_inspect(name: String, common: SkillsCommonArgs) -> Result<()> {
    let reg = load_skills_for_cli(&common)?;
    let def = reg
        .get(&name)
        .ok_or_else(|| anyhow!("unknown skill {name:?}"))?;
    println!("name:        {}", def.name);
    println!("source:      {}", def.source.as_str());
    println!("root:        {}", def.root_dir.display());
    let opts = code_agent_core::SkillDiscoveryOptions::default();
    let state = code_agent_core::LoadedSkillsState::new();
    let result = code_agent_core::SkillTool::load(&reg, &opts, &state, &name)
        .map_err(|e| anyhow!("inspect {name}: {e}"))?;
    println!("instructions bytes: {}", result.instructions.len());
    println!("declared resources: {}", def.resources.len());
    let mut total: u64 = 0;
    for r in &result.resources {
        let truncated = if r.truncated { " (truncated)" } else { "" };
        println!(
            "  {:<48} {:>10}  loaded={}{}",
            r.path, r.size_bytes, r.loaded, truncated
        );
        total += r.size_bytes;
    }
    println!("total resource bytes: {total}");
    if !def.allowed_tools.is_empty() {
        println!("allowed-tools: {} rule(s)", def.allowed_tools.len());
    }
    if !def.disallowed_tools.is_empty() {
        println!("disallowed-tools: {} rule(s)", def.disallowed_tools.len());
    }
    Ok(())
}

async fn cmd_skills_init(name: String, cwd: PathBuf) -> Result<()> {
    if !code_agent_core::is_identifier_safe_skill_name(&name) {
        return Err(anyhow!("skill name {name:?} must match [a-z0-9_-]{{1,64}}"));
    }
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("--cwd {}", cwd.display()))?;
    let dir = code_agent_core::filesystem_skills_dir(&cwd).join(&name);
    if dir.exists() {
        return Err(anyhow!("{} already exists", dir.display()));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let body = format!(
        "---\nname: {name}\ndescription: TODO — describe this skill\nallowed-tools:\n  - Read\n  - Grep\n  - Glob\n---\n\n# {name}\n\nReplace this body with the procedure the model should follow when this\nSkill is loaded.\n"
    );
    let path = dir.join("SKILL.md");
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    println!("created {}", path.display());
    Ok(())
}

fn load_commands_for_cli(
    common: &CommandsCommonArgs,
) -> Result<code_agent_core::SlashCommandRegistry> {
    let cwd = common
        .cwd
        .canonicalize()
        .with_context(|| format!("--cwd {}", common.cwd.display()))?;
    let dir = if let Some(p) = &common.commands {
        resolve_against_cwd(&cwd, p)
    } else {
        code_agent_core::custom_commands_dir(&cwd)
    };
    let (defs, errors) = code_agent_core::load_custom_commands(&dir);
    for e in errors {
        eprintln!("[commands] {e}");
    }
    let mut reg = code_agent_core::SlashCommandRegistry::new(!common.no_builtins).with_custom(defs);
    let skills_dir = if let Some(p) = &common.skills {
        resolve_against_cwd(&cwd, p)
    } else {
        code_agent_core::filesystem_skills_dir(&cwd)
    };
    let (skill_defs, _skill_errors) = code_agent_core::load_filesystem_skills(
        &skills_dir,
        code_agent_core::SkillSource::Project,
        code_agent_core::DEFAULT_MAX_SKILL_MD_BYTES,
    );
    let mut sk = code_agent_core::SkillRegistry::new();
    for d in skill_defs {
        let _ = sk.register(d);
    }
    reg = reg.with_skills(&sk);
    Ok(reg)
}

async fn cmd_commands_list(common: CommandsCommonArgs) -> Result<()> {
    let reg = load_commands_for_cli(&common)?;
    let cmds = reg.list();
    if cmds.is_empty() {
        println!("(no commands)");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<8} DESCRIPTION", "NAME", "SOURCE", "ARGS");
    for s in cmds {
        let full = match &s.namespace {
            Some(ns) => format!("{}:{}", ns, s.name),
            None => s.name.clone(),
        };
        println!(
            "{:<24} {:<8} {:<8} {}",
            full,
            s.source.label(),
            s.argument_hint.clone().unwrap_or_default(),
            truncate(&s.description, 80)
        );
    }
    Ok(())
}

async fn cmd_commands_show(name: String, common: CommandsCommonArgs) -> Result<()> {
    let reg = load_commands_for_cli(&common)?;
    if let Some(def) = reg.custom(&name) {
        println!("name:          {}", def.full_name());
        println!("source:        custom file");
        println!("path:          {}", def.source_path.display());
        println!("description:   {}", def.description);
        if let Some(h) = &def.argument_hint {
            println!("argument-hint: {h}");
        }
        if let Some(m) = &def.model {
            println!("model:         {m}");
        }
        if !def.allowed_tools.is_empty() {
            println!("allowed-tools: {} rule(s)", def.allowed_tools.len());
        }
        if !def.disallowed_tools.is_empty() {
            println!("disallowed:    {} rule(s)", def.disallowed_tools.len());
        }
        println!();
        println!("--- body ---");
        let preview: String = def.body.chars().take(2048).collect();
        println!("{preview}");
        if def.body.len() > preview.len() {
            println!("…[truncated]");
        }
        return Ok(());
    }
    if let Some(b) = code_agent_core::BuiltInSlashCommand::from_name(&name) {
        println!("name:        /{}", b.name());
        println!("source:      builtin");
        println!("description: {}", b.description());
        return Ok(());
    }
    if reg.list().iter().any(|s| s.name == name) {
        println!("name:        /{name}");
        println!("source:      skill-backed");
        return Ok(());
    }
    Err(anyhow!("unknown command {name:?}"))
}

async fn cmd_commands_validate(common: CommandsCommonArgs) -> Result<()> {
    let cwd = common
        .cwd
        .canonicalize()
        .with_context(|| format!("--cwd {}", common.cwd.display()))?;
    let dir = if let Some(p) = &common.commands {
        p.clone()
    } else {
        code_agent_core::custom_commands_dir(&cwd)
    };
    let (defs, errors) = code_agent_core::load_custom_commands(&dir);
    let bad = errors.len();
    for e in &errors {
        eprintln!("error: {e}");
    }
    for def in &defs {
        println!("ok    {}", def.full_name());
    }
    if bad > 0 {
        Err(anyhow!("{bad} command file(s) failed validation"))
    } else {
        Ok(())
    }
}

fn load_agent_registry_for_cli(
    common: &AgentsCommonArgs,
) -> Result<code_agent_core::AgentRegistry> {
    let cwd = common
        .cwd
        .canonicalize()
        .with_context(|| format!("--cwd {}", common.cwd.display()))?;
    let mut registry = code_agent_core::AgentRegistry::new();
    let dir = if let Some(p) = &common.agents {
        p.clone()
    } else {
        code_agent_core::filesystem_agents_dir(&cwd)
    };
    let (defs, errors) = code_agent_core::load_filesystem_agents(&dir);
    for d in defs {
        if let Err(e) = registry.register_filesystem(d) {
            eprintln!("[agents] {e}");
        }
    }
    for e in errors {
        eprintln!("[agents] {e}");
    }
    let _ = common.settings;
    Ok(registry)
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

async fn rotate_checkpoint(
    handle: Option<&Arc<code_agent_checkpoint::RotatingCheckpointHandle>>,
    cwd: &Path,
    session_id: &SessionId,
    quiet: bool,
) {
    let h = match handle {
        Some(h) => h,
        None => return,
    };
    let run_id = RunId::new();
    let res = h
        .store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            cwd: cwd.to_path_buf(),
            description: Some("user message".into()),
            user_message_id: None,
            git_state: code_agent_checkpoint::probe_git_state(cwd).await,
        })
        .await;
    match res {
        Ok(id) => {
            if !quiet {
                println!("[checkpoint] created {} before user request", id);
            }
            h.set_active(id, run_id).await;
        }
        Err(e) => {
            tracing::warn!("failed to create per-prompt checkpoint: {e}");
        }
    }
}

async fn resolve_session_for_checkpoints(
    cwd: &Path,
    explicit: Option<String>,
) -> Result<SessionId> {
    if let Some(s) = explicit {
        return parse_session_id(&s);
    }
    let store = FsSessionStore::new(cwd.to_path_buf());
    store
        .latest_for_cwd(cwd)
        .await?
        .ok_or_else(|| anyhow!("no sessions found for cwd; pass --session"))
}

async fn cmd_checkpoint_create(args: CheckpointCreateArgs) -> Result<()> {
    let cwd = args.cwd.canonicalize().context("--cwd")?;
    let session_id = resolve_session_for_checkpoints(&cwd, args.session).await?;
    let store = Arc::new(LocalCheckpointStore::new(cwd.clone()));
    let api = LocalCheckpointApi::new(store, session_id, cwd);
    let id = api
        .create_checkpoint(args.description)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("[checkpoint] created {}", id);
    Ok(())
}

async fn cmd_checkpoints_list(cwd: PathBuf, session: Option<String>) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let session_id = resolve_session_for_checkpoints(&cwd, session).await?;
    let store = LocalCheckpointStore::new(cwd.clone());
    let list = store
        .list_checkpoints(&session_id)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if list.is_empty() {
        println!("(no checkpoints for session {session_id})");
    } else {
        for s in list {
            let desc = s.description.clone().unwrap_or_default();
            println!(
                "{}\t{}\tfiles={}\t{}",
                s.checkpoint_id, s.created_at, s.files_tracked_count, desc
            );
        }
    }
    Ok(())
}

async fn cmd_checkpoints_show(
    checkpoint_id: String,
    cwd: PathBuf,
    session: Option<String>,
) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = LocalCheckpointStore::new(cwd.clone());
    let id = code_agent_checkpoint::CheckpointId::from(checkpoint_id);
    let session_id = if let Some(s) = session {
        parse_session_id(&s)?
    } else {
        store
            .find_session_for_checkpoint(&id)
            .await
            .map_err(|e| anyhow!("{e}"))?
    };
    let meta = store
        .get_checkpoint(&session_id, &id)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}

async fn cmd_checkpoints_diff(
    checkpoint_id: String,
    cwd: PathBuf,
    session: Option<String>,
) -> Result<()> {
    let cwd = cwd.canonicalize().context("--cwd")?;
    let store = LocalCheckpointStore::new(cwd.clone());
    let id = code_agent_checkpoint::CheckpointId::from(checkpoint_id.clone());
    let session_id = if let Some(s) = session {
        parse_session_id(&s)?
    } else {
        store
            .find_session_for_checkpoint(&id)
            .await
            .map_err(|e| anyhow!("{e}"))?
    };
    let diff = store
        .diff_checkpoint(DiffCheckpointRequest {
            checkpoint_id: id.clone(),
            session_id,
            cwd,
            max_bytes: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("[checkpoint] diff {}", id);
    for fd in &diff.file_diffs {
        match fd {
            FileDiff::Modified {
                path, unified_diff, ..
            } => {
                print!("{unified_diff}");
                if !unified_diff.ends_with('\n') {
                    println!();
                }
                let _ = path;
            }
            FileDiff::Created {
                path,
                current_content_preview,
                ..
            } => {
                println!("+++ created {}\n{}", path, current_content_preview);
            }
            FileDiff::Deleted {
                path,
                original_content_preview,
                ..
            } => {
                println!("--- deleted {}\n{}", path, original_content_preview);
            }
            FileDiff::Binary { path, change_kind } => {
                println!("[binary {:?}] {}", change_kind, path);
            }
            FileDiff::Redacted { path, change_kind } => {
                println!("[redacted {:?}] {}", change_kind, path);
            }
        }
    }
    if diff.truncated {
        println!("(diff output truncated)");
    }
    Ok(())
}

async fn cmd_rewind(args: RewindArgs) -> Result<()> {
    let cwd = args.cwd.canonicalize().context("--cwd")?;
    let store = LocalCheckpointStore::new(cwd.clone());
    let id = code_agent_checkpoint::CheckpointId::from(args.checkpoint_id.clone());

    if !args.yes && !args.dry_run {
        eprintln!(
            "About to rewind workspace files at {} to {}.",
            cwd.display(),
            id
        );
        eprintln!("Conversation history will not be rewound.");
        eprint!("Proceed? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        let answer = buf.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("[rewind] cancelled");
            return Ok(());
        }
    }

    if args.dry_run {
        println!("[rewind] dry run for checkpoint {}", id);
    } else {
        println!("[rewind] restoring workspace to {}", id);
    }

    let report = store
        .rewind(RewindRequest {
            checkpoint_id: id.clone(),
            cwd: cwd.clone(),
            force_cwd: args.force_cwd,
            dry_run: args.dry_run,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if args.dry_run {
        for p in &report.restored_files {
            println!("would restore: {}", p);
        }
        for p in &report.recreated_files {
            println!("would recreate: {}", p);
        }
        for p in &report.deleted_files {
            println!("would delete: {}", p);
        }
        for s in &report.skipped_files {
            println!("would skip: {} ({})", s.path, s.reason);
        }
        println!("conversation history will not be rewound");
    } else {
        for p in &report.restored_files {
            println!("[rewind] restored {}", p);
        }
        for p in &report.recreated_files {
            println!("[rewind] recreated {}", p);
        }
        for p in &report.deleted_files {
            println!("[rewind] deleted {}", p);
        }
        for s in &report.skipped_files {
            println!("[rewind] skipped {} ({})", s.path, s.reason);
        }
        println!("[rewind] complete");
        println!("[note] conversation history was not rewound");

        // Append a session record noting the rewind into the checkpoint's
        // OWNER session (not `latest_for_cwd`, which can resolve to an
        // unrelated session if a newer one was started since). Falls back
        // to silent no-op if the checkpoint can't be located — at this
        // point the rewind already succeeded on disk and the note is
        // best-effort context for the next agent turn.
        if let Ok(owner_session) = store.find_session_for_checkpoint(&id).await {
            let session_store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
            let note = format!(
                "rewind to {} restored={} deleted={} recreated={} skipped={}",
                id,
                report.restored_files.len(),
                report.deleted_files.len(),
                report.recreated_files.len(),
                report.skipped_files.len(),
            );
            let _ = code_agent_session::append_user_prompt(
                session_store.as_ref(),
                &cwd,
                &owner_session,
                &format!("[system note] {note}; conversation history was not rewound"),
            )
            .await;
        }
    }
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
        AgentEvent::CheckpointEvent(p) => {
            if !quiet {
                render_checkpoint_event(p);
            }
        }
        AgentEvent::McpServerConnecting(e) => {
            if !quiet {
                println!("[mcp] connecting {} via {}", e.server_name, e.transport);
            }
        }
        AgentEvent::McpServerConnected(e) => {
            if !quiet {
                println!(
                    "[mcp] {} connected, {} tools discovered",
                    e.server_name, e.tool_count
                );
            }
        }
        AgentEvent::McpServerFailed(e) => {
            println!("[mcp] {} failed: {}", e.server_name, e.error);
        }
        AgentEvent::McpToolsDiscovered(_) => {}
        AgentEvent::McpToolCallStarted(e) => {
            if !quiet {
                println!("[mcp-tool] {}.{}", e.server_name, e.remote_tool_name);
            }
        }
        AgentEvent::McpToolCallCompleted(e) => {
            if !quiet {
                println!(
                    "[mcp-result] {} ({} bytes, {}ms)",
                    e.agent_tool_name, e.result_bytes, e.duration_ms
                );
            }
        }
        AgentEvent::McpToolCallFailed(e) => {
            println!("[mcp-error] {}: {}", e.agent_tool_name, e.error);
        }
        AgentEvent::McpServerLog(e) => {
            if !quiet {
                println!("[mcp-log] {} {:?}: {}", e.server_name, e.stream, e.message);
            }
        }
        AgentEvent::ToolCatalogBuilt(e) => {
            if !quiet {
                println!(
                    "[tools] catalog built: {} tools, {} eager, {} deferred, {} disabled (~{} tokens)",
                    e.total_tools,
                    e.eager_tools,
                    e.deferred_tools,
                    e.disabled_tools,
                    e.estimated_total_tokens
                );
            }
        }
        AgentEvent::ToolLoadingModeResolved(e) => {
            if !quiet {
                println!(
                    "[tools] loading mode: {} -> {} ({})",
                    e.configured_mode, e.resolved_mode, e.reason
                );
            }
        }
        AgentEvent::ToolContextBudgetUpdated(e) => {
            if !quiet {
                println!(
                    "[tools] loaded this turn: {} tools, ~{} tokens; deferred: {} tools, ~{} tokens",
                    e.loaded_tool_count,
                    e.estimated_loaded_tool_tokens,
                    e.deferred_tool_count,
                    e.estimated_deferred_tool_tokens
                );
            }
        }
        AgentEvent::ToolSearchStarted(e) => {
            if !quiet {
                println!("[tool-search] query={:?}", e.query);
            }
        }
        AgentEvent::ToolSearchCompleted(e) => {
            if !quiet {
                let loaded: Vec<String> = e
                    .matches
                    .iter()
                    .filter(|m| m.loaded)
                    .map(|m| m.tool_name.clone())
                    .collect();
                if loaded.is_empty() {
                    println!("[tool-search] no tools loaded");
                } else {
                    println!("[tool-search] loaded:");
                    for name in loaded {
                        println!("  {}", name);
                    }
                }
            }
        }
        AgentEvent::ToolSearchFailed(e) => {
            println!("[tool-search-error] {}: {}", e.query, e.error);
        }
        AgentEvent::ToolLoaded(_) | AgentEvent::ToolDeferred(_) | AgentEvent::ToolDisabled(_) => {}
        AgentEvent::AgentRegistryLoaded(e) => {
            if !quiet {
                let names: Vec<String> = e.agents.iter().map(|a| a.name.clone()).collect();
                println!(
                    "[agents] loaded {} subagents: {}",
                    e.agents.len(),
                    names.join(", ")
                );
            }
        }
        AgentEvent::SubagentStart(e) => {
            if !quiet {
                println!("[agent] starting {}", e.agent_name);
            }
        }
        AgentEvent::SubagentEvent(e) => {
            if !quiet {
                println!("[agent:{}] {:?}", e.agent_name, e.event.as_ref());
            }
        }
        AgentEvent::SubagentStop(e) => {
            if !quiet {
                let preview: String = e
                    .result_preview
                    .lines()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "[agent-result] {} ({:?}, {}ms): {}",
                    e.agent_name, e.status, e.duration_ms, preview
                );
            }
        }
        AgentEvent::SubagentFailed(e) => {
            println!("[agent-error] {} failed: {}", e.agent_name, e.error);
        }
        AgentEvent::SubagentCancelled(e) => {
            println!("[agent-cancelled] {}: {}", e.agent_name, e.reason);
        }
        AgentEvent::SubagentTimedOut(e) => {
            println!(
                "[agent-error] {} timed out after {}s",
                e.agent_name,
                e.timeout_ms / 1000
            );
        }
        AgentEvent::SkillRegistryLoaded(s) => {
            if !quiet {
                let names: Vec<&str> = s.skills.iter().map(|x| x.name.as_str()).collect();
                println!(
                    "[skills] discovered {} skills: {}",
                    s.skills.len(),
                    names.join(", ")
                );
            }
        }
        AgentEvent::SkillLoaded(s) => {
            if !quiet {
                println!(
                    "[skill] loaded {} ({} resources)",
                    s.name, s.loaded_resource_count
                );
            }
        }
        AgentEvent::SkillResourceLoaded(r) => {
            if !quiet {
                println!(
                    "[skill] resource {}/{} ({} bytes{})",
                    r.skill_name,
                    r.path,
                    r.size_bytes,
                    if r.truncated { ", truncated" } else { "" }
                );
            }
        }
        AgentEvent::SkillInvocationFailed(e) => {
            println!("[skill-error] {}: {}", e.name, e.reason);
        }
        AgentEvent::SlashCommandRegistryLoaded(s) => {
            if !quiet {
                println!("[slash] {} commands loaded", s.commands.len());
            }
        }
        AgentEvent::SlashCommandStarted(s) => {
            if !quiet {
                println!("[slash] /{} ({})", s.command, s.source.label());
            }
        }
        AgentEvent::SlashCommandExpanded(e) => {
            if !quiet {
                println!(
                    "[slash] expanded {} file ref(s) and {} bash command(s)",
                    e.file_refs.len(),
                    e.bash_commands.len()
                );
            }
        }
        AgentEvent::SlashCommandCompleted(c) => {
            if !quiet {
                println!("[slash] /{} done ({}ms)", c.command, c.duration_ms);
            }
        }
        AgentEvent::SlashCommandFailed(f) => {
            println!("[slash-error] /{}: {}", f.command, f.reason);
        }
        AgentEvent::CompactBoundary(b) => {
            println!(
                "[compact] {} -> {} messages ({} preserved)",
                b.pre_messages, b.post_messages, b.preserved_recent_turns
            );
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

fn render_checkpoint_event(p: &code_agent_core::CheckpointEventPayload) {
    match p.event.as_str() {
        "created" => {
            let id = p
                .data
                .get("checkpoint_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let desc = p.data.get("description").and_then(|v| v.as_str());
            match desc {
                Some(d) => println!("[checkpoint] created {} ({})", id, d),
                None => println!("[checkpoint] created {}", id),
            }
        }
        "snapshot_captured" => {
            let path = p.data.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let kind = p
                .data
                .get("capture")
                .and_then(|v| v.get("kind").and_then(|s| s.as_str()))
                .unwrap_or("?");
            match kind {
                "missing_before_change" => {
                    println!("[checkpoint] tracked missing file {}", path);
                }
                "skipped" => {
                    let reason = p
                        .data
                        .get("capture")
                        .and_then(|v| v.get("reason").and_then(|s| s.as_str()))
                        .unwrap_or("");
                    println!("[checkpoint] skipped {} ({})", path, reason);
                }
                _ => println!("[checkpoint] captured {} before change", path),
            }
        }
        "capture_failed" => {
            let reason = p.data.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            println!("[checkpoint] capture failed: {}", reason);
        }
        "bash_warning" => {
            let msg = p.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
            println!("[warning] {}", msg);
        }
        "rewind_started" => {
            let id = p
                .data
                .get("checkpoint_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[rewind] restoring workspace to {}", id);
        }
        "file_rewound" => {
            let path = p.data.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let action = p.data.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            match action {
                "restored" => println!("[rewind] restored {}", path),
                "recreated" => println!("[rewind] recreated {}", path),
                "deleted" => println!("[rewind] deleted {}", path),
                _ => println!("[rewind] {} {}", action, path),
            }
        }
        "rewind_completed" => {
            println!("[rewind] complete");
        }
        "rewind_failed" => {
            let reason = p.data.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            println!("[rewind] failed: {}", reason);
        }
        "validation_started" => {
            let cmds = p
                .data
                .get("commands")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!("[validation] running {}", cmds);
        }
        "validation_completed" => {
            let passed = p
                .data
                .get("passed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if passed {
                println!("[validation] passed");
            } else {
                println!("[validation] failed");
            }
        }
        other => {
            println!("[checkpoint] {}", other);
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
        if let Some(parts) = code_agent_mcp::parse_agent_tool_name(&request.tool_name) {
            s.push_str(&format!("  Server: {}\n", parts.server_name));
            s.push_str(&format!("  Remote tool: {}\n", parts.remote_tool_name));
            if let Some(transport) = request
                .metadata
                .get("mcp")
                .and_then(|m| m.get("transport"))
                .and_then(|v| v.as_str())
            {
                s.push_str(&format!("  Transport: {}\n", transport));
            }
            s.push_str(&format!("  Arguments: {}\n", request.input));
            s.push_str("  External access: yes\n");
            s.push_str("  Checkpoint coverage: not applicable\n");
            if code_agent_mcp::mutating_tool_name(&parts.remote_tool_name) {
                s.push_str("  Warning: This MCP tool may modify external state. Workspace rewind cannot undo it.\n");
            } else {
                s.push_str("  Warning: MCP tool effects are external and are not covered by workspace checkpointing.\n");
            }
            s.push_str("    [y] allow once\n");
            s.push_str("    [s] allow for session\n");
            s.push_str("    [a] allow for project\n");
            s.push_str("    [n] deny once\n");
            s.push_str("    [d] deny for session\n");
            s.push_str("    [v] view full input\n");
            s.push_str("  approve> ");
            return s;
        }
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
