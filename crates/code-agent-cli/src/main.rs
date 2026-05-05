use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use code_agent_anthropic::{AnthropicConfig, AnthropicProvider};
use code_agent_core::agent::SessionSink;
use code_agent_core::{
    AgentBuilder, AgentEvent, AgentOptions, ModelProvider, PermissionCallback, PermissionDecision,
    PermissionMode, PermissionRequest, ResultSubtype, SessionId, ToolRegistry,
};
use code_agent_session::{
    append_user_prompt, replay_conversation, FsSessionStore, SessionSinkAdapter, SessionStore,
};
use code_agent_tools::register_builtin;
use futures::StreamExt;

#[derive(Parser, Debug)]
#[command(name = "code-agent", version, about = "Headless coding agent (M1)")]
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
    /// Manage sessions.
    #[command(subcommand)]
    Sessions(SessionsCommand),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Initial user prompt.
    prompt: String,
    /// Workspace directory; tools are sandboxed to this path.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Anthropic model id.
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,
    /// Comma-separated list of tools to expose.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "Read,Glob,Grep,Edit,Bash"
    )]
    allowed_tools: Vec<String>,
    /// Permission mode: default, acceptEdits, plan, dontAsk.
    #[arg(long, default_value = "default")]
    permission_mode: String,
    /// Maximum agent turns before bailing out.
    #[arg(long, default_value_t = 8)]
    max_turns: u32,
    /// Disable session writing.
    #[arg(long)]
    no_session: bool,
}

#[derive(Parser, Debug)]
struct ResumeArgs {
    /// Session id to resume.
    session_id: String,
    /// New prompt to append to the session.
    prompt: String,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "Read,Glob,Grep,Edit,Bash"
    )]
    allowed_tools: Vec<String>,
    #[arg(long, default_value = "default")]
    permission_mode: String,
    #[arg(long, default_value_t = 8)]
    max_turns: u32,
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// List sessions for a workspace.
    List {
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
    },
}

fn parse_permission_mode(s: &str) -> Result<PermissionMode> {
    PermissionMode::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --permission-mode: {s}"))
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
        Command::Sessions(SessionsCommand::List { cwd }) => cmd_sessions_list(cwd).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    let mode = parse_permission_mode(&args.permission_mode)?;
    let cwd = args.cwd.canonicalize().context("--cwd")?;
    let project_instructions = load_agents_md(&cwd).await;

    let provider: Arc<dyn ModelProvider> = build_provider(&args.model)?;
    let mut tools = ToolRegistry::new();
    register_builtin(&mut tools);

    // Create the session up front when persistence is enabled and reuse the
    // returned id everywhere. Generating a fresh id here while also calling
    // `store.create()` would leak an empty orphan session file into
    // `.agent/sessions/` on every run.
    let (session_id, session_sink, store) = if args.no_session {
        (SessionId::new(), None, None)
    } else {
        let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
        let id = store
            .create(&cwd)
            .await
            .context("failed to create session")?;
        let sink: Arc<dyn SessionSink> =
            Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));
        (id, Some(sink), Some(store))
    };
    if let Some(s) = &store {
        let _ = append_user_prompt(s.as_ref(), &cwd, &session_id, &args.prompt).await;
    }

    let options = AgentOptions {
        cwd: cwd.clone(),
        model: args.model.clone(),
        allowed_tools: args.allowed_tools.clone(),
        permission_mode: mode,
        max_turns: Some(args.max_turns),
        session_id: Some(session_id.clone()),
        permission_callback: Some(Arc::new(StdinApprover::new(mode))),
        system_prompt: None,
        project_instructions,
        max_tokens: 4096,
    };

    let mut builder = AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools);
    if let Some(sink) = session_sink {
        builder = builder.session_sink(sink);
    }
    let agent = builder.build()?;
    let mut stream = agent.run(args.prompt);

    let exit_ok = drain_events(&mut stream).await;
    if exit_ok {
        Ok(())
    } else {
        Err(anyhow!("agent did not finish successfully"))
    }
}

async fn cmd_resume(args: ResumeArgs) -> Result<()> {
    let mode = parse_permission_mode(&args.permission_mode)?;
    let cwd = args.cwd.canonicalize().context("--cwd")?;
    let session_id = SessionId::from(args.session_id.clone());
    let project_instructions = load_agents_md(&cwd).await;

    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let records = store
        .load(&session_id)
        .await
        .context("failed to load session")?;
    let conv = replay_conversation(&records);

    let provider: Arc<dyn ModelProvider> = build_provider(&args.model)?;
    let mut tools = ToolRegistry::new();
    register_builtin(&mut tools);

    let _ = append_user_prompt(store.as_ref(), &cwd, &session_id, &args.prompt).await;
    let sink: Arc<dyn SessionSink> = Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));

    let options = AgentOptions {
        cwd: cwd.clone(),
        model: args.model.clone(),
        allowed_tools: args.allowed_tools.clone(),
        permission_mode: mode,
        max_turns: Some(args.max_turns),
        session_id: Some(session_id.clone()),
        permission_callback: Some(Arc::new(StdinApprover::new(mode))),
        system_prompt: None,
        project_instructions,
        max_tokens: 4096,
    };

    let agent = AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .session_sink(sink)
        .initial_history(conv.messages)
        .build()?;
    let mut stream = agent.run(args.prompt);
    let exit_ok = drain_events(&mut stream).await;
    if exit_ok {
        Ok(())
    } else {
        Err(anyhow!("agent did not finish successfully"))
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

async fn drain_events(stream: &mut (dyn futures::Stream<Item = AgentEvent> + Unpin)) -> bool {
    let mut current_text = String::new();
    let mut had_success = false;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::SystemInit(init) => {
                println!("[init] session {}", init.session_id);
            }
            AgentEvent::AssistantTextDelta(t) => {
                print!("{t}");
                let _ = std::io::stdout().flush();
                current_text.push_str(&t);
            }
            AgentEvent::AssistantMessage(_) => {
                if !current_text.is_empty() {
                    println!();
                    current_text.clear();
                }
            }
            AgentEvent::ToolCallStarted(s) => {
                println!("[tool] {}: {}", s.call.name, summarize_input(&s.call.input));
            }
            AgentEvent::ToolCallCompleted(c) => {
                let tag = if c.is_error {
                    "tool-error"
                } else {
                    "tool-result"
                };
                println!("[{tag}] {} ({}ms)", c.tool_name, c.duration_ms);
            }
            AgentEvent::ToolCallDenied(d) => {
                println!("[denied] {}: {}", d.tool_name, d.reason);
            }
            AgentEvent::UserToolResult(_) => {
                // Already summarized via ToolCallCompleted.
            }
            AgentEvent::Result(r) => {
                println!(
                    "[result] {} (turns={}, subtype={:?})",
                    r.result.as_deref().unwrap_or(""),
                    r.num_turns,
                    r.subtype
                );
                had_success = matches!(r.subtype, ResultSubtype::Success);
            }
        }
    }
    had_success
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

/// Permission callback that prompts the user on stdin. Default is deny unless
/// the user types "y" or "yes". Skipped for AcceptEdits + ReadOnly cases by
/// the agent loop already; this only fires when approval is genuinely needed.
struct StdinApprover {
    interactive: bool,
    mode: PermissionMode,
}

impl StdinApprover {
    fn new(mode: PermissionMode) -> Self {
        Self {
            interactive: std::io::stdin().is_terminal(),
            mode,
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
        let mode = self.mode;
        let summary = request.summary;
        let tool = request.tool_name;
        let answer = tokio::task::spawn_blocking(move || {
            println!("\n[approval] mode={mode:?} tool={tool}");
            println!("  {summary}");
            print!("  approve? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
            buf.trim().to_ascii_lowercase()
        })
        .await
        .unwrap_or_default();
        if matches!(answer.as_str(), "y" | "yes") {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: "user denied".into(),
            }
        }
    }
}
