//! `code-agent login | auth | provider | model` commands and slash-command
//! handlers (M10).

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use code_agent_auth::{
    default_registry, provider_env_var, providers::FakeProviderState, resolve_credential,
    starter_registry, AuthContext, AuthErrorKind, AuthPrecedenceMode, AuthProviderRegistry,
    AuthResolverInputs, BillingMode, CredentialStore, CredentialStoreMode, LoginAlternative,
    LoginMethod, LoginMethodKind, LoginRequest, ModelAvailability, ProviderId, ResolvedCredential,
    SecretString, StoredAccount,
};

#[derive(Parser, Debug)]
pub struct LoginArgs {
    /// Provider name (anthropic | openai | codex | gemini | fake).
    /// Omit for the interactive selector.
    pub provider: Option<String>,

    /// Use API-key auth. Without a value, prompts on the terminal; with
    /// a value (`--api-key=sk-...`), uses the supplied key. Tests/scripts
    /// should prefer `--api-key-stdin`.
    #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub api_key: Option<String>,

    /// Use OAuth device-code flow.
    #[arg(long)]
    pub device: bool,

    /// Read API key from stdin instead of prompting (for scripts).
    #[arg(long)]
    pub api_key_stdin: bool,

    /// Workspace dir (used to resolve `.agent/auth.json`).
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Allow file credential store (overrides settings) for this run.
    #[arg(long)]
    pub allow_file_store: bool,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Show effective auth status across providers.
    Status(AuthCommonArgs),
    /// List all stored accounts.
    List(AuthCommonArgs),
    /// Set the active provider/account.
    Use {
        provider: String,
        account: Option<String>,
        #[command(flatten)]
        common: AuthCommonArgs,
    },
    /// Logout (delete stored credentials) for one or all providers.
    Logout {
        provider: Option<String>,
        #[command(flatten)]
        common: AuthCommonArgs,
    },
    /// Refresh OAuth tokens for a provider.
    Refresh {
        provider: Option<String>,
        #[command(flatten)]
        common: AuthCommonArgs,
    },
    /// Run auth-focused diagnostics.
    Doctor(AuthCommonArgs),
}

#[derive(Subcommand, Debug)]
pub enum ProviderCommand {
    /// List registered providers.
    List(AuthCommonArgs),
    /// Set the active provider.
    Use {
        provider: String,
        #[command(flatten)]
        common: AuthCommonArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModelCommand {
    /// List models from the model registry.
    List(AuthCommonArgs),
    /// Set the active provider/model.
    Use {
        /// Provider/model identifier (e.g. `anthropic/claude-sonnet-4-6`).
        spec: String,
        #[command(flatten)]
        common: AuthCommonArgs,
    },
    /// Show the active provider/model.
    Status(AuthCommonArgs),
}

#[derive(Args, Debug)]
pub struct AuthCommonArgs {
    /// Workspace dir.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
    /// Allow the file credential backend.
    #[arg(long)]
    pub allow_file_store: bool,
}

/// Resolve a credential store rooted at `cwd/.agent`.
pub fn build_store(cwd: &std::path::Path, allow_file_store: bool) -> Result<CredentialStore> {
    let index_path = cwd.join(".agent").join("auth.json");
    let store = CredentialStore::build(
        CredentialStoreMode::Auto,
        allow_file_store,
        Some(index_path),
    )
    .map_err(|e| anyhow!("build credential store: {}", e))?;
    Ok(store)
}

/// Build a registry, sharing one [`FakeProviderState`] so the login and
/// model commands can use the same fake setup.
pub fn build_registry() -> (AuthProviderRegistry, Arc<FakeProviderState>) {
    let state = Arc::new(FakeProviderState::new());
    // Pre-allow the demo API key so `code-agent login fake --api-key=demo`
    // succeeds out of the box.
    state.allow_api_key("demo");
    let reg = default_registry(state.clone());
    (reg, state)
}

fn resolve_provider(name: &str) -> ProviderId {
    ProviderId::parse(name)
}

fn read_api_key(args: &LoginArgs) -> Result<Option<SecretString>> {
    if args.api_key_stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!("--api-key-stdin: empty input"));
        }
        return Ok(Some(SecretString::new(trimmed)));
    }
    if let Some(value) = args.api_key.as_deref() {
        if !value.is_empty() {
            return Ok(Some(SecretString::new(value.to_string())));
        }
        // Bare `--api-key` flag: prompt or refuse non-interactively.
        if !std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "--api-key requires a terminal; use --api-key=<KEY> or --api-key-stdin"
            ));
        }
        // Print the prompt to stderr to match the previous behavior — CLI
        // prompts on stderr stay visible when stdout is piped or
        // redirected. `rpassword::read_password` only reads from the
        // tty; it does not also write the prompt, so do that ourselves.
        eprint!("Enter API key (input hidden): ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let raw = rpassword::read_password().map_err(|e| anyhow!("read API key: {e}"))?;
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!("API key empty"));
        }
        return Ok(Some(SecretString::new(trimmed)));
    }
    Ok(None)
}

/// Run the top-level `login` command. When `provider` is `None` we render
/// the provider selector and exit; the user runs again with the chosen
/// provider. (No interactive picker yet — this avoids a hidden terminal
/// dependency.)
pub async fn cmd_login(args: LoginArgs) -> Result<()> {
    let (registry, _state) = build_registry();
    let store = build_store(&args.cwd, args.allow_file_store)?;

    let Some(provider_name) = args.provider.clone() else {
        print_provider_selector(&registry);
        return Ok(());
    };
    let provider_id = resolve_provider(&provider_name);
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| anyhow!("unknown provider: {provider_name}"))?;

    let key = read_api_key(&args)?;
    let method = if args.device {
        LoginMethodKind::OAuthDeviceCode
    } else if args.api_key.is_some() || key.is_some() {
        LoginMethodKind::ApiKey
    } else {
        LoginMethodKind::OAuthBrowser
    };
    let req = LoginRequest {
        method: Some(method),
        api_key: key,
        params: Default::default(),
    };
    let ctx = AuthContext::from_process(true);
    let result = provider.login(req, ctx).await.map_err(|e| anyhow!(e))?;

    let provider_id = result.account.provider.clone();
    let account_id = result.account.account_id.clone();
    let display = result.account.display_label(true);
    store
        .put(result.account, &result.secret)
        .map_err(|e| anyhow!(e))?;
    store
        .set_active(&provider_id, account_id.clone())
        .map_err(|e| anyhow!(e))?;

    println!("✓ {} ({})", result.display_summary, display);
    println!("✓ Active provider set to {provider_id}");
    Ok(())
}

fn print_provider_selector(registry: &AuthProviderRegistry) {
    println!("Choose model access provider:\n");
    for (i, (_, provider)) in registry.iter().enumerate() {
        let id = provider.id();
        let name = provider.display_name();
        println!("  {}. {} ({})", i + 1, name, id);
        for method in provider.supported_login_methods() {
            match method {
                LoginMethod::ApiKey => println!("     • API key"),
                LoginMethod::OAuthBrowser => println!("     • OAuth (browser)"),
                LoginMethod::OAuthDeviceCode => println!("     • OAuth (device code)"),
                LoginMethod::ExternalCliManaged => {
                    println!("     • External CLI / sidecar (managed)")
                }
                LoginMethod::ServiceAccount => println!("     • Service account"),
                LoginMethod::Unsupported {
                    reason,
                    alternatives,
                } => {
                    println!("     • Unsupported: {}", reason);
                    for alt in alternatives {
                        print_alternative(&alt);
                    }
                }
            }
        }
    }
    println!("\nRun:");
    println!("  code-agent login <provider> --api-key");
    println!("  code-agent login <provider> --device");
}

fn print_alternative(alt: &LoginAlternative) {
    let mut line = format!("       → {}", alt.label);
    if let Some(hint) = &alt.command_hint {
        line.push_str(&format!("  ({})", hint));
    }
    println!("{line}");
}

pub async fn cmd_auth(cmd: AuthCommand) -> Result<()> {
    match cmd {
        AuthCommand::Status(args) => cmd_auth_status(args).await,
        AuthCommand::List(args) => cmd_auth_list(args).await,
        AuthCommand::Use {
            provider,
            account,
            common,
        } => cmd_auth_use(provider, account, common).await,
        AuthCommand::Logout { provider, common } => cmd_auth_logout(provider, common).await,
        AuthCommand::Refresh { provider, common } => cmd_auth_refresh(provider, common).await,
        AuthCommand::Doctor(args) => cmd_auth_doctor(args).await,
    }
}

pub async fn cmd_auth_status(args: AuthCommonArgs) -> Result<()> {
    let store = build_store(&args.cwd, args.allow_file_store)?;
    let idx = store.snapshot_index();
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();

    let mut rows: Vec<StatusRow> = Vec::new();
    for provider_id in [
        ProviderId::Anthropic,
        ProviderId::OpenAi,
        ProviderId::Codex,
        ProviderId::Gemini,
        ProviderId::Fake,
    ] {
        let active = idx.active_for(&provider_id).cloned();
        let resolved = resolve_credential(AuthResolverInputs {
            provider: Some(provider_id.clone()),
            mode: AuthPrecedenceMode::ExplicitFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: active.as_ref(),
            env: env.clone(),
            store: Some(&store),
        });
        rows.push(StatusRow::from_resolved(
            &provider_id,
            active.as_ref(),
            &resolved,
            &idx.list_for(&provider_id),
        ));
    }

    if args.json {
        let json: Vec<_> = rows.iter().map(|r| r.to_json()).collect();
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("Provider    Method                   Account              Active");
        for row in &rows {
            println!(
                "{:<11} {:<24} {:<20} {}",
                row.provider,
                row.method,
                row.account,
                if row.active { "yes" } else { "no" }
            );
        }
        for row in &rows {
            for warn in &row.warnings {
                println!("! {warn}");
            }
        }
    }
    Ok(())
}

pub async fn cmd_auth_list(args: AuthCommonArgs) -> Result<()> {
    let store = build_store(&args.cwd, args.allow_file_store)?;
    let idx = store.snapshot_index();
    if args.json {
        // Project `active` from the index's authoritative active map so
        // the JSON output stays in sync with `set_active` (the stored
        // `active` bool inside each entry is not refreshed when the
        // active selection moves).
        let projected: Vec<serde_json::Value> = idx
            .accounts
            .iter()
            .map(|a| {
                let mut v = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
                let active = idx.active_for(&a.provider) == Some(&a.account_id);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("active".into(), serde_json::Value::Bool(active));
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&projected)?);
    } else if idx.accounts.is_empty() {
        println!("(no stored accounts)");
    } else {
        for a in &idx.accounts {
            let active = idx.active_for(&a.provider) == Some(&a.account_id);
            println!(
                "{}/{} method={} active={}",
                a.provider,
                a.account_id,
                a.login_method.as_str(),
                active
            );
        }
    }
    Ok(())
}

pub async fn cmd_auth_use(
    provider: String,
    account: Option<String>,
    common: AuthCommonArgs,
) -> Result<()> {
    let store = build_store(&common.cwd, common.allow_file_store)?;
    let provider_id = resolve_provider(&provider);
    let idx = store.snapshot_index();
    let chosen = match account {
        Some(a) => {
            let candidate = code_agent_auth::AccountId::new(a);
            // The active pointer must reference an account that is
            // actually in the index — writing an arbitrary id would
            // silently break the next `auth status` for this provider.
            if idx.get(&provider_id, &candidate).is_none() {
                let available: Vec<String> = idx
                    .list_for(&provider_id)
                    .iter()
                    .map(|a| a.account_id.to_string())
                    .collect();
                let listing = if available.is_empty() {
                    "(none stored)".to_string()
                } else {
                    available.join(", ")
                };
                return Err(anyhow!(
                    "no stored account {} for {}; available: {}",
                    candidate,
                    provider_id,
                    listing
                ));
            }
            candidate
        }
        None => idx
            .list_for(&provider_id)
            .first()
            .map(|a| a.account_id.clone())
            .ok_or_else(|| anyhow!("no stored accounts for {provider_id}"))?,
    };
    store
        .set_active(&provider_id, chosen.clone())
        .map_err(|e| anyhow!(e))?;
    println!("✓ Active account for {provider_id}: {chosen}");
    Ok(())
}

pub async fn cmd_auth_logout(provider: Option<String>, common: AuthCommonArgs) -> Result<()> {
    let store = build_store(&common.cwd, common.allow_file_store)?;
    let providers: Vec<ProviderId> = match provider {
        Some(p) => vec![resolve_provider(&p)],
        None => {
            let idx = store.snapshot_index();
            idx.accounts.iter().map(|a| a.provider.clone()).collect()
        }
    };
    let mut total = 0;
    for p in providers {
        let idx = store.snapshot_index();
        for acct in idx.list_for(&p) {
            if store.delete(&p, &acct.account_id).map_err(|e| anyhow!(e))? {
                println!("✓ logged out {}/{}", p, acct.account_id);
                total += 1;
            }
        }
    }
    if total == 0 {
        println!("(nothing to log out — no stored credentials)");
    }
    Ok(())
}

pub async fn cmd_auth_refresh(provider: Option<String>, common: AuthCommonArgs) -> Result<()> {
    let (registry, _state) = build_registry();
    let store = build_store(&common.cwd, common.allow_file_store)?;
    let idx = store.snapshot_index();
    let providers: Vec<ProviderId> = match provider {
        Some(p) => vec![resolve_provider(&p)],
        None => idx.accounts.iter().map(|a| a.provider.clone()).collect(),
    };
    for p in providers {
        let Some(provider) = registry.get(&p) else {
            println!("- {p}: not registered");
            continue;
        };
        for acct in idx.list_for(&p) {
            let res = provider
                .refresh(acct, AuthContext::from_process(false))
                .await;
            match res {
                Ok(r) if r.refreshed => {
                    store.put(r.account, &r.secret).map_err(|e| anyhow!(e))?;
                    println!("✓ refreshed {p}/{}", acct.account_id);
                }
                Ok(_) => {
                    println!("- {p}/{}: nothing to refresh", acct.account_id);
                }
                Err(e) => {
                    println!("✗ {p}/{}: {}", acct.account_id, e);
                }
            }
        }
    }
    Ok(())
}

pub async fn cmd_auth_doctor(args: AuthCommonArgs) -> Result<()> {
    // Wraps the structured doctor logic from m9. Implemented as a thin
    // duplicate so it can be tested independently from the workspace
    // doctor.
    let store = build_store(&args.cwd, args.allow_file_store)?;
    let idx = store.snapshot_index();
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let ci_marker = code_agent_auth::ci::detect_ci(&env);

    println!("Auth diagnostics");
    println!("  credential store: {}", store.backend.name());
    if let Some(marker) = ci_marker {
        println!(
            "  ! CI detected via {marker}; subscription auth blocked unless explicitly allowed"
        );
    } else {
        println!("  CI: not detected");
    }
    for provider_id in [
        ProviderId::Anthropic,
        ProviderId::OpenAi,
        ProviderId::Codex,
        ProviderId::Gemini,
        ProviderId::Fake,
    ] {
        let active = idx.active_for(&provider_id).cloned();
        let resolved = resolve_credential(AuthResolverInputs {
            provider: Some(provider_id.clone()),
            mode: AuthPrecedenceMode::ExplicitFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: active.as_ref(),
            env: env.clone(),
            store: Some(&store),
        });
        match resolved {
            Ok(r) => {
                println!(
                    "  ✓ {provider_id}: {} ({})",
                    summarize_resolved(&r.credential),
                    r.credential.source_label()
                );
                for w in r.warnings {
                    println!("    ! {w}");
                }
            }
            Err(e) if e.kind == AuthErrorKind::NotLoggedIn => {
                println!("  - {provider_id}: not logged in");
                if let Some(env) = provider_env_var(&provider_id) {
                    if std::env::var(env).is_ok() {
                        println!("    (env var {env} present but no stored account)");
                    }
                }
            }
            Err(e) => {
                println!("  ✗ {provider_id}: {e}");
            }
        }
    }
    Ok(())
}

fn summarize_resolved(c: &ResolvedCredential) -> String {
    match c {
        ResolvedCredential::ApiKey { provider, .. } => format!("API key for {provider}"),
        ResolvedCredential::OAuthBearer {
            provider,
            account_id,
            ..
        } => format!("OAuth bearer for {provider}/{account_id}"),
        ResolvedCredential::ExternalManaged { provider, .. } => {
            format!("external sidecar for {provider}")
        }
        ResolvedCredential::None => "no credential".into(),
    }
}

pub async fn cmd_provider(cmd: ProviderCommand) -> Result<()> {
    match cmd {
        ProviderCommand::List(args) => cmd_provider_list(args).await,
        ProviderCommand::Use { provider, common } => cmd_provider_use(provider, common).await,
    }
}

pub async fn cmd_provider_list(args: AuthCommonArgs) -> Result<()> {
    let (registry, _) = build_registry();
    if args.json {
        let entries: Vec<_> = registry
            .iter()
            .map(|(_, p)| {
                serde_json::json!({
                    "id": p.id(),
                    "name": p.display_name(),
                    "methods": p
                        .supported_login_methods()
                        .iter()
                        .map(|m| format!("{:?}", m))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for (_, p) in registry.iter() {
            println!("- {} ({})", p.display_name(), p.id());
        }
    }
    Ok(())
}

pub async fn cmd_provider_use(provider: String, common: AuthCommonArgs) -> Result<()> {
    let provider_id = resolve_provider(&provider);
    // Don't silently re-write the credential index here; that path
    // produced a no-op when no account had ever been logged in. Instead
    // persist the operator's *intent* to a small project-local marker
    // file, so subsequent runs of `code-agent run` / status / TUI know
    // which provider to prefer regardless of whether a credential is
    // also stored. We mirror the pattern already used for `model use`.
    let dst = common.cwd.join(".agent").join("active-provider.json");
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({ "provider": provider_id });
    std::fs::write(&dst, serde_json::to_vec_pretty(&payload)?)?;
    // Best-effort: if the credential index already has an active
    // account for the chosen provider, keep it as the active selection
    // (idempotent re-write).
    if let Ok(store) = build_store(&common.cwd, common.allow_file_store) {
        let idx = store.snapshot_index();
        if let Some(active) = idx.active_for(&provider_id).cloned() {
            store.set_active(&provider_id, active)?;
        }
    }
    println!("✓ Active provider: {provider_id}");
    Ok(())
}

pub async fn cmd_model(cmd: ModelCommand) -> Result<()> {
    match cmd {
        ModelCommand::List(args) => cmd_model_list(args).await,
        ModelCommand::Use { spec, common } => cmd_model_use(spec, common).await,
        ModelCommand::Status(args) => cmd_model_status(args).await,
    }
}

pub async fn cmd_model_list(args: AuthCommonArgs) -> Result<()> {
    let registry = starter_registry();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&registry.models)?);
    } else {
        for m in &registry.models {
            let billing = match m.billing_mode {
                BillingMode::ApiUsageBased => "api",
                BillingMode::SubscriptionIncluded | BillingMode::SubscriptionCredits => {
                    "subscription"
                }
                BillingMode::Unknown => "?",
            };
            println!(
                "{}/{}  [{}]  {}  ({})",
                m.provider,
                m.model_id,
                billing,
                m.display_name,
                availability_label(&m.availability)
            );
        }
    }
    Ok(())
}

fn availability_label(a: &ModelAvailability) -> String {
    a.label()
}

pub async fn cmd_model_use(spec: String, common: AuthCommonArgs) -> Result<()> {
    let (provider, model) = spec
        .split_once('/')
        .ok_or_else(|| anyhow!("model spec must be in `provider/model` form"))?;
    let provider_id = resolve_provider(provider);
    let registry = starter_registry();
    let info = registry
        .find(&provider_id, model)
        .ok_or_else(|| anyhow!("unknown model: {spec}"))?;
    // Persist by writing to .agent/active-model.json (lightweight; the
    // host CLI honors --model on each invocation regardless).
    let dst = common.cwd.join(".agent").join("active-model.json");
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({
        "provider": info.provider,
        "model": info.model_id,
    });
    std::fs::write(&dst, serde_json::to_vec_pretty(&payload)?)?;
    println!("✓ Active model: {}/{}", info.provider, info.model_id);
    Ok(())
}

pub async fn cmd_model_status(args: AuthCommonArgs) -> Result<()> {
    let model_path = args.cwd.join(".agent").join("active-model.json");
    let provider_path = args.cwd.join(".agent").join("active-provider.json");
    let active_provider = if provider_path.exists() {
        let bytes = std::fs::read(&provider_path)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        v["provider"].as_str().map(|s| s.to_string())
    } else {
        None
    };
    if model_path.exists() {
        let bytes = std::fs::read(&model_path)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        println!(
            "Active model: {}/{}",
            v["provider"].as_str().unwrap_or("?"),
            v["model"].as_str().unwrap_or("?")
        );
    } else {
        println!("No active model selected. Use `code-agent model use <PROVIDER>/<MODEL>`.");
    }
    if let Some(p) = active_provider {
        println!("Active provider override: {p}");
    }
    Ok(())
}

#[derive(Clone)]
struct StatusRow {
    provider: String,
    method: String,
    account: String,
    active: bool,
    warnings: Vec<String>,
}

impl StatusRow {
    fn from_resolved(
        provider: &ProviderId,
        active: Option<&code_agent_auth::AccountId>,
        resolved: &Result<code_agent_auth::ResolvedAuth, code_agent_auth::AuthError>,
        accounts: &[&StoredAccount],
    ) -> Self {
        let (method, account, warnings) = match resolved {
            Ok(r) => {
                let warnings = r.warnings.clone();
                let (m, a) = match &r.credential {
                    ResolvedCredential::ApiKey { source, .. } => {
                        ("api-key", source.as_str().to_string())
                    }
                    ResolvedCredential::OAuthBearer { account_id, .. } => {
                        ("oauth-token", account_id.to_string())
                    }
                    ResolvedCredential::ExternalManaged { .. } => ("external", "managed".into()),
                    ResolvedCredential::None => ("-", "-".into()),
                };
                (m.to_string(), a, warnings)
            }
            Err(_) => ("-".to_string(), "-".to_string(), Vec::new()),
        };
        let display_account = match active {
            Some(a) => accounts
                .iter()
                .find(|s| &s.account_id == a)
                .map(|s| s.display_label(true))
                .unwrap_or_else(|| account.clone()),
            None => account,
        };
        Self {
            provider: provider.to_string(),
            method,
            account: display_account,
            active: active.is_some(),
            warnings,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "method": self.method,
            "account": self.account,
            "active": self.active,
            "warnings": self.warnings,
        })
    }
}

/// Slash-command handler: parses `args` (everything after `/login `) and
/// runs the equivalent of `code-agent login`. Returns a string suitable
/// for displaying to the user.
///
/// `allow_file_store` mirrors `auth.allow_file_credential_store` from
/// the loaded `Settings`. The slash flow must honor it — earlier
/// revisions hard-coded `true` and were flagged by code review for
/// silently bypassing the user's opt-in.
pub async fn handle_login_slash(
    args: &str,
    cwd: &std::path::Path,
    allow_file_store: bool,
) -> Result<String> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let mut provider_name: Option<String> = None;
    let mut device = false;
    let mut want_api_key = false;
    let mut explicit_key: Option<String> = None;
    for p in parts {
        match p {
            "--api-key" => want_api_key = true,
            "--device" => device = true,
            "--api-key-stdin" => {} // not used in slash mode
            "status" => {
                return format_login_status(cwd, true, allow_file_store).await;
            }
            "logout" => {
                return format_logout(cwd, true, allow_file_store).await;
            }
            other if other.starts_with("--api-key=") => {
                explicit_key = Some(other.trim_start_matches("--api-key=").to_string());
                want_api_key = true;
            }
            other if !other.starts_with("--") && provider_name.is_none() => {
                provider_name = Some(other.to_string());
            }
            _ => {}
        }
    }
    // Run with optional explicit key (skip stdin/tty paths).
    let (registry, _state) = build_registry();
    let store = build_store(cwd, allow_file_store)?;
    let Some(provider_name) = provider_name else {
        let mut buf = String::new();
        use std::fmt::Write;
        writeln!(buf, "Choose model access provider:")?;
        for (_, p) in registry.iter() {
            writeln!(buf, "  - {} ({})", p.display_name(), p.id())?;
        }
        writeln!(buf, "Then run e.g. /login anthropic --api-key=<key>")?;
        return Ok(buf);
    };
    let provider_id = resolve_provider(&provider_name);
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| anyhow!("unknown provider: {provider_name}"))?;

    let key = explicit_key.map(SecretString::new);
    let req = LoginRequest {
        method: Some(if device {
            LoginMethodKind::OAuthDeviceCode
        } else if key.is_some() || want_api_key {
            LoginMethodKind::ApiKey
        } else {
            LoginMethodKind::OAuthBrowser
        }),
        api_key: key,
        params: Default::default(),
    };
    let result = provider
        .login(req, AuthContext::from_process(false))
        .await
        .map_err(|e| anyhow!(e))?;
    let display = result.account.display_label(true);
    let provider_id = result.account.provider.clone();
    let account_id = result.account.account_id.clone();
    store.put(result.account, &result.secret)?;
    store.set_active(&provider_id, account_id.clone())?;
    Ok(format!(
        "✓ {} ({})\n✓ Active provider: {provider_id}\n",
        result.display_summary, display
    ))
}

pub async fn format_login_status(
    cwd: &std::path::Path,
    _redact: bool,
    allow_file_store: bool,
) -> Result<String> {
    let store = build_store(cwd, allow_file_store)?;
    let idx = store.snapshot_index();
    use std::fmt::Write;
    let mut out = String::new();
    if idx.accounts.is_empty() {
        writeln!(out, "No stored credentials. Use /login.")?;
    }
    for a in &idx.accounts {
        let active = idx.active_for(&a.provider) == Some(&a.account_id);
        writeln!(
            out,
            "{}/{} method={} active={}",
            a.provider,
            a.display_label(true),
            a.login_method.as_str(),
            active
        )?;
    }
    Ok(out)
}

pub async fn format_logout(
    cwd: &std::path::Path,
    _redact: bool,
    allow_file_store: bool,
) -> Result<String> {
    let store = build_store(cwd, allow_file_store)?;
    let idx = store.snapshot_index();
    use std::fmt::Write;
    let mut out = String::new();
    let mut count = 0;
    for a in &idx.accounts {
        if store.delete(&a.provider, &a.account_id)? {
            writeln!(out, "✓ logged out {}/{}", a.provider, a.account_id)?;
            count += 1;
        }
    }
    if count == 0 {
        writeln!(out, "(no stored credentials to remove)")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    // /login fake --api-key=demo stores a credential and marks the fake provider active.
    async fn slash_login_fake_api_key_stores_credential() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = handle_login_slash("fake --api-key=demo", dir.path(), true)
            .await
            .unwrap();
        assert!(out.contains("Active provider: fake"));
        let store = build_store(dir.path(), true).unwrap();
        let idx = store.snapshot_index();
        assert_eq!(idx.list_for(&ProviderId::Fake).len(), 1);
        assert!(idx.active_for(&ProviderId::Fake).is_some());
    }

    #[tokio::test]
    // /login (no provider) renders the provider selector text instead of erroring.
    async fn slash_login_no_provider_shows_selector() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = handle_login_slash("", dir.path(), true).await.unwrap();
        assert!(out.contains("Choose model access provider"));
        assert!(out.contains("Anthropic"));
        assert!(out.contains("Codex"));
    }

    #[tokio::test]
    // /login status shows stored credentials in a redacted form.
    async fn slash_login_status_lists_stored_accounts() {
        let dir = tempfile::TempDir::new().unwrap();
        let _ = handle_login_slash("fake --api-key=demo", dir.path(), true)
            .await
            .unwrap();
        let out = format_login_status(dir.path(), true, true).await.unwrap();
        assert!(out.contains("fake"));
        assert!(out.contains("api_key"));
    }

    #[tokio::test]
    // /logout removes the stored credential and is idempotent.
    async fn slash_logout_removes_stored_credential() {
        let dir = tempfile::TempDir::new().unwrap();
        let _ = handle_login_slash("fake --api-key=demo", dir.path(), true)
            .await
            .unwrap();
        let out1 = format_logout(dir.path(), true, true).await.unwrap();
        assert!(out1.contains("logged out"));
        let out2 = format_logout(dir.path(), true, true).await.unwrap();
        assert!(out2.contains("no stored credentials"));
    }

    #[tokio::test]
    // /login codex (no method) hits the unsupported subscription branch with a clear message.
    async fn slash_login_codex_subscription_unsupported() {
        let dir = tempfile::TempDir::new().unwrap();
        let res = handle_login_slash("codex --device", dir.path(), true).await;
        match res {
            Err(e) => {
                assert!(e.to_string().contains("not enabled") || e.to_string().contains("Codex"))
            }
            Ok(s) => panic!("expected error, got: {s}"),
        }
    }

    #[tokio::test]
    // `code-agent provider use <provider>` writes a project-local
    // marker even when no credential is yet stored. Regression test
    // for the second-opinion review finding that the previous impl was
    // a silent no-op.
    async fn provider_use_persists_active_provider_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let common = AuthCommonArgs {
            cwd: dir.path().to_path_buf(),
            json: false,
            allow_file_store: true,
        };
        cmd_provider_use("openai".into(), common).await.unwrap();
        let marker = dir.path().join(".agent/active-provider.json");
        assert!(marker.exists(), "provider marker should exist");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(v["provider"].as_str(), Some("openai"));
    }

    #[tokio::test]
    // `auth use <provider> <account>` rejects an account id that is not
    // in the credential index, instead of writing a dangling active
    // pointer that would break the next `auth status` call.
    async fn auth_use_rejects_unknown_account() {
        let dir = tempfile::TempDir::new().unwrap();
        let _ = handle_login_slash("fake --api-key=demo", dir.path(), true)
            .await
            .unwrap();
        let common = AuthCommonArgs {
            cwd: dir.path().to_path_buf(),
            json: false,
            allow_file_store: true,
        };
        let err = cmd_auth_use("fake".into(), Some("does-not-exist".into()), common)
            .await
            .expect_err("expected unknown-account error");
        let msg = err.to_string();
        assert!(msg.contains("does-not-exist"), "msg = {msg}");
        assert!(msg.contains("available"), "msg = {msg}");
    }

    #[tokio::test]
    // `auth use <provider> <account>` accepts an account id that is
    // present in the credential index and writes the active pointer.
    async fn auth_use_accepts_known_account() {
        let dir = tempfile::TempDir::new().unwrap();
        let _ = handle_login_slash("fake --api-key=demo", dir.path(), true)
            .await
            .unwrap();
        let store = build_store(dir.path(), true).unwrap();
        let stored = store
            .snapshot_index()
            .list_for(&ProviderId::Fake)
            .first()
            .map(|a| a.account_id.clone())
            .unwrap();
        let common = AuthCommonArgs {
            cwd: dir.path().to_path_buf(),
            json: false,
            allow_file_store: true,
        };
        cmd_auth_use("fake".into(), Some(stored.to_string()), common)
            .await
            .unwrap();
        let reloaded = build_store(dir.path(), true).unwrap();
        assert_eq!(
            reloaded.snapshot_index().active_for(&ProviderId::Fake),
            Some(&stored)
        );
    }

    #[tokio::test]
    // When allow_file_store=false and no keychain is available, the
    // slash login flow surfaces the credential-store-unavailable error
    // rather than silently writing to .agent/secrets.
    async fn slash_login_respects_disabled_file_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let res = handle_login_slash("fake --api-key=demo", dir.path(), false).await;
        let err = res.expect_err("expected credential-store error");
        assert!(
            err.to_string().contains("credential store"),
            "unexpected error: {err}"
        );
        // No .agent/secrets/ should have been created.
        assert!(!dir.path().join(".agent/secrets").exists());
    }
}
