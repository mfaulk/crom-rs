# Milestone 10 — Provider Login + Subscription/Auth Gateway Alpha

**Theme:** Move provider credentials out of `ANTHROPIC_API_KEY`-only
territory. Add a single login surface that covers Anthropic, OpenAI,
Codex, Gemini, and a fully offline fake provider, with explicit
visible-but-unsupported branches for the subscription paths we are not
authorized to ship as defaults. Credentials live in a separate store
that defaults to the OS keychain (or a clearly opt-in file backend) and
are resolved against a deterministic `auth precedence` ladder. M10 is
local-only; remote brokering, hosted accounts, and team sharing remain
out of scope.

## New crates

| Crate | Purpose |
|-------|---------|
| `code-agent-auth` | Auth types, providers, OAuth (PKCE + device-code), credential store, model registry, CI policy. |
| `code-agent-provider-openai` | OpenAI Chat Completions adapter. Accepts a `ResolvedCredential` at call time; transport is pluggable (test mock or `reqwest`). |
| `code-agent-provider-gemini` | Google AI Studio `generateContent` adapter with the same call-time-credential pattern. |

`code-agent-auth` is dependency-light on purpose: only the workspace's
standard async + serde stack plus `sha2`/`hex` (for PKCE and the
redacted account-id hash). No HTTP client is bundled — providers and
the OAuth flows accept an `oauth::HttpTransport` so the host crate can
back it with `reqwest`, a fake transport, or anything else.

## Auth surface

Core types (in `code-agent-auth`):

- `ProviderId` — `Anthropic | OpenAi | Codex | Gemini | VertexAi |
  Bedrock | Foundry | Fake | Custom(String)`. Serializes as a plain
  lowercase string so events / settings stay readable.
- `LoginMethod` — `ApiKey | OAuthBrowser | OAuthDeviceCode |
  ExternalCliManaged | ServiceAccount | Unsupported { reason,
  alternatives }`. `Unsupported` is a *first-class* outcome, not an
  error; the selector renders it with concrete alternatives.
- `AuthProvider` trait — `id`, `display_name`,
  `supported_login_methods`, async `login | refresh | logout | status`.
- `AuthProviderRegistry` — keyed by `ProviderId`; built-in registry has
  Anthropic, OpenAI, Codex (default mode `Unsupported`), Gemini, and
  Fake.
- `ResolvedCredential` — `ApiKey | OAuthBearer | ExternalManaged |
  None`. Custom `Debug` impl redacts the secret bytes.
- `SecretString` — minimal in-tree wrapper that hides values from
  `Debug` / `Display` and best-effort zeroizes on `Drop`.

OAuth framework (`oauth/`):

- `pkce.rs` — `generate_verifier`, `generate_state`, `s256_challenge`.
  Verified against the RFC 7636 §4.4 test vector. No real RNG dep —
  bytes come from `SystemTime` × `pid` × per-call counter through SHA-256,
  documented as sufficient for the threat the PKCE/state pair defends
  against (cross-site code injection).
- `browser.rs` — `OAuthClientConfig`, `RedirectMode::{Loopback,
  ManualCode}`, `OAuthLoginSession`, `LoopbackCallback`.
  `start_oauth_browser_login` + `finish_oauth_browser_login` are split
  so tests can drive the happy path with deterministic state. Loopback
  binds `127.0.0.1:0` (or a preferred port), the success page contains
  no secrets, and the verifier is wrapped in `SecretString` for the
  duration of the flow.
- `device.rs` — RFC 8628. `request_device_code` + `device_code_poll`.
  Maps `authorization_pending`, `slow_down`, `expired_token`,
  `access_denied` to stable `AuthErrorKind` values; `slow_down` extends
  the next interval.
- `token.rs` — shared `TokenResponse`, `TokenError`, and a small
  `TokenForm` URL-encoder.

Credential store (`store/`):

- `SecretBackend` trait + three backends: `FakeKeychainBackend` (in-memory,
  used by tests + demo), `FileCredentialBackend` (opt-in;
  `0700`/`0600` on Unix; account-id-hashed filenames so the directory
  listing reveals nothing), `SystemKeychainBackend` (stub today —
  reports unavailable so a future `keyring`-backed feature can drop in
  without API churn).
- `CredentialStore` — combined account index + secret backend, with
  modes `auto | keychain | file`. Default `auto` prefers the keychain;
  if none is available it falls back to the file store *only* when
  `allow_file_credential_store` is set, otherwise it surfaces a noop
  backend that errors loudly on the first write.
- Account metadata (`StoredAccount`) is kept in
  `.agent/auth.json` — never the secret bytes.

Auth precedence (`precedence.rs`):

- `AuthPrecedenceMode::ExplicitFirst` (default), `EnvFirst`,
  `StoredFirst`. Resolution order is documented in the module docstring;
  conflicts (e.g. env var present *and* stored credential) come back as
  non-fatal warnings so `auth status` / `doctor` can explain them.

CI policy (`ci.rs`):

- Detects `CI`, `GITHUB_ACTIONS`, `BUILDKITE`, `GITLAB_CI`, `CIRCLECI`,
  `TRAVIS`, `JENKINS_URL`, `TF_BUILD` and ignores `0`/`false` values.
- `enforce_ci_policy` returns `Allowed | BlockedSubscription |
  AllowedWithWarning`. Subscription auth in CI is blocked unless
  `auth.allow_subscription_auth_in_ci = true`.

Provider matrix (default build):

| Provider  | API key | OAuth browser | OAuth device | Subscription |
|-----------|:-------:|:-------------:|:------------:|:------------:|
| Anthropic | ✓       | —             | —            | **Unsupported** (Claude OAuth is not authorized for third-party clients) |
| OpenAI    | ✓       | —             | —            | n/a |
| Codex     | ✓       | only with `UserSuppliedOAuth` | only with `UserSuppliedOAuth` | **Feature-gated**; defaults to `Unsupported` |
| Gemini    | ✓       | —             | —            | **Unsupported** (Gemini CLI OAuth is not authorized for third-party use) |
| Fake      | ✓       | ✓             | ✓            | ✓ (offline) |

`CodexAuthProvider::new(CodexAuthMode::ChatGptManagedSidecar |
UserSuppliedOAuth | OpenAiApiKey)` switches between the four documented
modes; the default constructor returns `Unsupported`. No first-party
client IDs are embedded.

## Model registry

`code_agent_auth::starter_registry()` ships one representative model per
provider with `BillingMode` annotated:

| Provider/Model | Billing | Availability |
|----------------|---------|--------------|
| `anthropic/claude-sonnet-4-6` | `ApiUsageBased` | Requires API key |
| `openai/gpt-4.1`              | `ApiUsageBased` | Requires API key |
| `codex/gpt-codex`             | `SubscriptionIncluded` | Requires subscription |
| `gemini/gemini-1.5-pro`       | `ApiUsageBased` | Requires API key |
| `fake/fake-coding-model`      | `ApiUsageBased` | Available |

Subscription-mode rows are tagged so `model list` / status output can
suppress the misleading local dollar estimate.

## Model providers (call-time credentials)

Both new model providers take a closure that returns the current
`ResolvedCredential` so the host re-resolves auth per turn (env vs.
stored vs. CLI override) without the provider having to know how the
credential store works:

```rust
let provider = OpenAiProvider::new(
    OpenAiConfig::default(),
    Arc::new(MyTransport::new(http_client)),
    Arc::new(move || resolve_credential(...).unwrap().credential),
);
```

Both providers map `code_agent_core::ModelRequest` to the upstream
schema, normalize streaming/tool-call output back into `ModelEvent`s,
and reject missing credentials via `ProviderError::Auth` before
touching the transport.

## CLI surface

```sh
code-agent login                                # provider selector
code-agent login fake --api-key=demo            # explicit value
code-agent login anthropic --api-key            # prompts on TTY
code-agent login openai  --api-key-stdin        # script-friendly
code-agent login codex   --device               # OAuth device-code (when enabled)

code-agent auth status        [--allow-file-store] [--json]
code-agent auth list          [--allow-file-store] [--json]
code-agent auth use   <PROVIDER> [ACCOUNT]
code-agent auth logout [PROVIDER]
code-agent auth refresh [PROVIDER]
code-agent auth doctor

code-agent provider list
code-agent provider use <PROVIDER>
code-agent model list
code-agent model use <PROVIDER>/<MODEL>
code-agent model status

code-agent logout                               # alias for `auth logout`
```

`--api-key` accepts an inline value (`--api-key=sk-...`) or, when bare,
prompts on a real TTY. Non-interactive shells must use
`--api-key=<KEY>` or `--api-key-stdin` — bare `--api-key` exits
nonzero rather than hanging on stdin.

`auth status` renders a table with a row per provider + the resolved
credential source, and prints precedence warnings (e.g.
`ANTHROPIC_API_KEY environment variable will override stored anthropic
account under env-first precedence`).

## Slash commands

`BuiltInSlashCommand` gains `Login`, `Logout`, `Auth`, `Provider`,
`Model`. The TUI and `code-agent chat` dispatch each to the same
handlers as the CLI subcommands. `/login` with no provider renders the
provider selector; `/login fake --api-key=demo` runs the offline path.

## Status-line variables

`{provider}`, `{account}`, `{auth_method}`, `{plan}`, `{quota}`. The
default status-line context is populated from `.agent/auth.json` (no
secrets), with the account redacted when one is configured. `{quota}`
is left empty rather than fabricated when the provider does not expose
the data.

## Doctor

`code-agent doctor` gains an **Auth** section that reports:

- Credential-store backend in use.
- CI detection (and a warning when subscription auth is selected).
- Per-provider resolution state (logged in, env override warning,
  not-logged-in with the right `/login` hint).

Doctor never opts into the file backend on its own — building the
auth-checks store passes `allow_file_store=false` so a `--fix --yes`
path that depends on `.agent/` not yet existing keeps working.

## Settings

```json
{
  "auth": {
    "credential_store": "auto",
    "active_provider": "anthropic",
    "active_account": null,
    "auth_precedence": "explicit_first",
    "allow_file_credential_store": false,
    "refresh_before_expiry_seconds": 300,
    "allow_subscription_auth_in_ci": false,
    "providers": {
      "anthropic": { "enabled": true, "allowed_methods": ["api_key"] },
      "openai":    { "enabled": true, "allowed_methods": ["api_key"] },
      "codex":     { "enabled": true, "allowed_methods": ["chatgpt_managed", "api_key", "user_supplied_oauth"] },
      "gemini":    { "enabled": true, "allowed_methods": ["api_key"] }
    }
  },
  "model_config": {
    "provider": "anthropic",
    "model": "claude-sonnet-4-6",
    "fallbacks": [
      { "provider": "openai", "model": "gpt-4.1" },
      { "provider": "gemini", "model": "gemini-1.5-pro" }
    ]
  }
}
```

Tokens are never written to settings JSON. The settings merge layer
treats `auth` and `model_config` as scalar overrides (per-key replace)
to match how plugins and CLI flags interact.

## Events / telemetry

`AgentEvent` gains `AuthProviderRegistryLoaded`, `LoginStarted`,
`LoginCompleted`, `LoginFailed`, `LogoutCompleted`, `CredentialStored`,
`CredentialRefreshed`, `CredentialRefreshFailed`,
`AuthProviderSelected`, `ModelSelected`, `ModelListLoaded`,
`AuthStatusChecked`. All event payloads carry the redacted account
hash (`acct_<12 hex chars>`) — never the raw account-id, email, or
secret bytes.

The auth crate also exposes its own `events` module with the same
structures keyed off the provider's native types; the CLI maps those
into the central `AgentEvent` shapes when it forwards.

## Demo

`examples/auth-demo/` — offline runner with `.agent/settings.json`
preconfigured for the fake provider. `bash run.sh` exercises:

1. `provider list` — registered providers + support matrix.
2. `login` (no args) — selector that explicitly explains the
   subscription paths that are unsupported and why.
3. `login fake --api-key=demo --allow-file-store` — store a fake key.
4. `auth status` / `model list` / `model use` / `auth logout`.

Existing M1–M9 demos continue to work unchanged.

## Safety

- No token logging (events redact to the hashed account id).
- No tokens persisted in settings JSON.
- File credential backend is opt-in. On Unix the secrets dir is `0700`
  and per-account files are `0600`; filenames are the redacted hash so a
  directory listing reveals nothing.
- OAuth state is validated before the token exchange runs; mismatches
  short-circuit before any HTTP write.
- Loopback callback page contains no secrets.
- PKCE verifier wrapped in `SecretString` and dropped at flow end.
- PKCE verifier and CSRF `state` are drawn from the OS CSPRNG
  (`rand::rngs::OsRng`). An earlier revision derived these from
  `SystemTime + pid + counter` and was rejected by code review for
  using a predictable generator on security tokens.
- No first-party client IDs hard-coded as defaults; Codex subscription
  modes other than the documented `OpenAiApiKey` path are
  feature-gated and surface a clear `Unsupported` status by default.
- Subscription auth is blocked in CI unless `auth.allow_subscription_auth_in_ci = true`.
- Doctor never opts into the file backend; it cannot side-effect-create
  `.agent/secrets/`.
- Slash auth helpers (`/login`, `/auth`, `/logout`) honor the
  configured `auth.allow_file_credential_store` instead of forcibly
  enabling the file backend. The slash flow surfaces a clear error if
  no backend is available.
- `CredentialStore::delete` propagates backend failures rather than
  swallowing them — logout cannot claim success while leaving the
  secret intact in the keychain.

## Runtime provider selection

`build_provider` honors `--provider <name>` (CLI), then
`.agent/active-provider.json` (written by `code-agent provider use`),
then `auth.active_provider` from settings, then the built-in default
`anthropic`. The OpenAI/Gemini providers are wired to a `reqwest`-backed
transport and resolve credentials through the same auth precedence
ladder used by `auth status`. `--provider fake` runs an in-process
ModelProvider for offline testing — no network required.

## Schema correctness

- OpenAI Chat Completions: tool calls live on `assistant.tool_calls`
  (`{id, type:"function", function:{name, arguments: <json string>}}`)
  and tool results are top-level `role: "tool"` messages with
  `tool_call_id`. Earlier revisions emitted Anthropic-style nested
  content blocks and would have been rejected with HTTP 400 on any
  tool turn.
- Gemini: `FunctionResponse.name` carries the original function name.
  The synthetic `gemini-<name>` id used in `ToolCall.id` is stripped
  before the response is sent back, so Gemini round-trips correctly
  across turns.

## Tests

639 tests pass workspace-wide. New coverage includes:

- Auth crate (66 tests): every provider, the credential store
  (round-trip, file-perm enforcement, opt-in gating, idempotent
  logout), PKCE (RFC 7636 vector), CSRF state, browser-flow happy path
  (with state validation), state mismatch, browser-open fallback,
  authorization-endpoint error propagation, device-code pending →
  slow_down → token, expired/denied/timeout handling, auth precedence
  for all three modes + the env/stored conflict warning, CI policy
  decision matrix, model-registry shape, and event redaction.
- OpenAI provider (4 tests): chat-completion streaming, model listing,
  missing-credential auth error, tool-call argument parsing.
- Gemini provider (4 tests): same shape against the
  `generateContent` schema (system_instruction, function-call/response,
  models listing).
- CLI (5 tests): `/login fake --api-key`, no-provider selector,
  `/login status`, `/logout` idempotency, Codex subscription
  unsupported branch.
- `EnvGuard` in the `m9` doctor tests now holds a process-global
  mutex so two tests touching `ANTHROPIC_API_KEY` cannot interleave.

`cargo fmt --all`, `cargo test --workspace`, and `cargo clippy
--workspace --all-targets` are clean.

## Out of scope

- A `keyring`-crate-backed real keychain (the `SystemKeychainBackend`
  shape is in place; the integration is left for a follow-up).
- Hosted/team account brokers, marketplace billing integration.
- Browser-cookie / external-CLI auth-cache scraping.
- Telemetry upload / OpenTelemetry export.
- Auto-purchasing quota or anything else that could spend money.
