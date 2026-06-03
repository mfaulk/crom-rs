//! Resolve which credential a model call should use.
//!
//! The resolver considers four sources in an order determined by the
//! configured [`AuthPrecedenceMode`]:
//!
//! 1. **CLI explicit** — operator-supplied `--api-key` / `--account` flags.
//! 2. **Active stored account** — the account marked as active in the
//!    credential index.
//! 3. **Provider env var API key** — e.g. `ANTHROPIC_API_KEY`.
//! 4. **Stored account** — most-recently-added account for the provider.
//!
//! When two sources collide, the resolver returns a non-fatal warning so
//! the CLI/TUI/doctor can explain *why* the env var (say) overrode the
//! keychain entry. Tests can construct an [`AuthResolverInputs`] directly,
//! avoiding any I/O.

use std::collections::BTreeMap;

use crate::error::AuthError;
#[cfg(test)]
use crate::error::AuthErrorKind;
use crate::store::CredentialStore;
use crate::types::{
    AccountId, AuthPrecedenceMode, CredentialSource, ProviderId, ResolvedCredential, SecretString,
};

/// Static mapping from [`ProviderId`] to its API-key environment variable.
pub fn provider_env_var(provider: &ProviderId) -> Option<&'static str> {
    Some(match provider {
        ProviderId::Anthropic => "ANTHROPIC_API_KEY",
        ProviderId::OpenAi => "OPENAI_API_KEY",
        ProviderId::Codex => "OPENAI_API_KEY",
        ProviderId::Gemini => "GEMINI_API_KEY",
        ProviderId::Fake => "CODE_AGENT_FAKE_API_KEY",
        _ => return None,
    })
}

/// Inputs to [`resolve_credential`]. CLI is responsible for filling these
/// from flags/settings/env. Tests can construct one directly.
#[derive(Default)]
pub struct AuthResolverInputs<'a> {
    pub provider: Option<ProviderId>,
    pub mode: AuthPrecedenceMode,
    pub explicit_api_key: Option<SecretString>,
    pub explicit_account: Option<AccountId>,
    pub active_account: Option<&'a AccountId>,
    pub env: BTreeMap<String, String>,
    pub store: Option<&'a CredentialStore>,
}

/// Outcome of resolution: the credential plus zero-or-more advisory
/// warnings to surface in `auth status` / `doctor`.
#[derive(Debug)]
pub struct ResolvedAuth {
    pub credential: ResolvedCredential,
    pub warnings: Vec<String>,
}

/// Resolve a credential for `provider` per the configured precedence.
pub fn resolve_credential(inputs: AuthResolverInputs<'_>) -> Result<ResolvedAuth, AuthError> {
    let Some(provider) = inputs.provider.clone() else {
        return Ok(ResolvedAuth {
            credential: ResolvedCredential::None,
            warnings: vec![],
        });
    };

    let mut warnings = Vec::new();

    // Source A: explicit CLI api-key.
    let cli = inputs
        .explicit_api_key
        .as_ref()
        .map(|k| ResolvedCredential::ApiKey {
            provider: provider.clone(),
            source: CredentialSource::Cli,
            key: k.clone(),
        });

    // Look up a stored account's secret and turn it into a resolved
    // credential. A genuine backend `Err` (store unavailable / I/O) is
    // propagated; a true "no entry" stays `Ok(None)` and falls through.
    let resolve_stored = |aid: &AccountId| -> Result<Option<ResolvedCredential>, AuthError> {
        let Some(store) = inputs.store else {
            return Ok(None);
        };
        let Some(secret) = store.get_secret(&provider, aid)? else {
            return Ok(None);
        };
        let account = store.snapshot_index().get(&provider, aid).cloned();
        Ok(secret_to_credential(
            &provider,
            aid,
            &secret,
            account.as_ref(),
        ))
    };

    // Source B: explicit CLI account override (must be present in store).
    let cli_account = match inputs.explicit_account.as_ref() {
        Some(aid) => resolve_stored(aid)?,
        None => None,
    };

    // Source C: active stored account.
    let stored_active = match inputs.active_account {
        Some(aid) => resolve_stored(aid)?,
        None => None,
    };

    // Source D: env var API key.
    let env = provider_env_var(&provider).and_then(|name| {
        inputs.env.get(name).map(|val| ResolvedCredential::ApiKey {
            provider: provider.clone(),
            source: CredentialSource::EnvVar(name.to_string()),
            key: SecretString::new(val.clone()),
        })
    });

    // Source E: any stored account for provider, most recent first.
    let stored_first = match inputs.store {
        Some(store) => {
            let idx = store.snapshot_index();
            let mut accounts: Vec<_> = idx.list_for(&provider).into_iter().cloned().collect();
            accounts.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            let mut found = None;
            for acct in accounts {
                // Propagate genuine backend errors; skip accounts whose
                // secret is simply absent or non-credential.
                if let Some(secret) = store.get_secret(&provider, &acct.account_id)? {
                    if let Some(cred) =
                        secret_to_credential(&provider, &acct.account_id, &secret, Some(&acct))
                    {
                        found = Some(cred);
                        break;
                    }
                }
            }
            found
        }
        None => None,
    };

    let stored_any = stored_active.clone().or_else(|| stored_first.clone());

    // Generic conflict warning if both env var and stored credential exist.
    if env.is_some() && stored_any.is_some() {
        if let Some(name) = provider_env_var(&provider) {
            warnings.push(format!(
                "{name} environment variable will override stored {provider} account under env-first precedence"
            ));
        }
    }

    let chosen = match inputs.mode {
        AuthPrecedenceMode::ExplicitFirst => cli
            .or(cli_account)
            .or(stored_active.clone())
            .or(env)
            .or(stored_first),
        AuthPrecedenceMode::EnvFirst => env.or(cli).or(cli_account).or(stored_any),
        AuthPrecedenceMode::StoredFirst => stored_active.or(stored_first).or(cli).or(env),
    };

    match chosen {
        Some(c) => Ok(ResolvedAuth {
            credential: c,
            warnings,
        }),
        None => Err(AuthError::not_logged_in(provider.as_str())),
    }
}

fn secret_to_credential(
    provider: &ProviderId,
    account_id: &AccountId,
    secret: &crate::types::StoredCredentialSecret,
    account: Option<&crate::types::StoredAccount>,
) -> Option<ResolvedCredential> {
    if let Some(api_key) = secret.api_key.as_ref() {
        return Some(ResolvedCredential::ApiKey {
            provider: provider.clone(),
            source: CredentialSource::Keychain,
            key: api_key.clone(),
        });
    }
    if let Some(at) = secret.access_token.as_ref() {
        return Some(ResolvedCredential::OAuthBearer {
            provider: provider.clone(),
            account_id: account_id.clone(),
            access_token: at.clone(),
            expires_at: account.and_then(|a| a.expires_at),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LoginMethodKind, StoredAccount, StoredCredentialSecret};
    use time::OffsetDateTime;

    fn store_with_account(
        provider: ProviderId,
        id: &str,
        key: &str,
    ) -> (CredentialStore, AccountId) {
        let store = CredentialStore::new_in_memory_for_tests();
        let aid = AccountId::new(id);
        let acct = StoredAccount {
            account_id: aid.clone(),
            provider: provider.clone(),
            login_method: LoginMethodKind::ApiKey,
            display_email: None,
            workspace_id: None,
            plan_type: None,
            created_at: OffsetDateTime::now_utc(),
            last_refresh_at: None,
            expires_at: None,
            scopes: vec![],
            active: true,
            metadata: serde_json::json!({}),
            needs_reauth: false,
        };
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new(key.into())),
            ..Default::default()
        };
        store.put(acct, &secret).unwrap();
        store.set_active(&provider, aid.clone()).unwrap();
        (store, aid)
    }

    #[test]
    // ExplicitFirst: --api-key wins over stored account and env var.
    fn explicit_cli_wins_in_explicit_first() {
        let (store, aid) = store_with_account(ProviderId::Anthropic, "k1", "stored-key");
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".into(), "env-key".into());
        let res = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Anthropic),
            mode: AuthPrecedenceMode::ExplicitFirst,
            explicit_api_key: Some(SecretString::new("cli-key".into())),
            explicit_account: None,
            active_account: Some(&aid),
            env,
            store: Some(&store),
        })
        .unwrap();
        match res.credential {
            ResolvedCredential::ApiKey { source, key, .. } => {
                assert!(matches!(source, CredentialSource::Cli));
                assert_eq!(key.expose(), "cli-key");
            }
            _ => panic!("expected ApiKey"),
        }
    }

    #[test]
    // EnvFirst: ANTHROPIC_API_KEY wins over stored.
    fn env_first_uses_env() {
        let (store, aid) = store_with_account(ProviderId::Anthropic, "k1", "stored-key");
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".into(), "env-key".into());
        let res = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Anthropic),
            mode: AuthPrecedenceMode::EnvFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: Some(&aid),
            env,
            store: Some(&store),
        })
        .unwrap();
        match res.credential {
            ResolvedCredential::ApiKey { source, key, .. } => {
                assert!(matches!(source, CredentialSource::EnvVar(_)));
                assert_eq!(key.expose(), "env-key");
            }
            _ => panic!("expected ApiKey"),
        }
    }

    #[test]
    // StoredFirst: stored account wins over env even when both present.
    fn stored_first_uses_stored() {
        let (store, aid) = store_with_account(ProviderId::OpenAi, "k1", "stored-key");
        let mut env = BTreeMap::new();
        env.insert("OPENAI_API_KEY".into(), "env-key".into());
        let res = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::OpenAi),
            mode: AuthPrecedenceMode::StoredFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: Some(&aid),
            env,
            store: Some(&store),
        })
        .unwrap();
        match res.credential {
            ResolvedCredential::ApiKey { source, key, .. } => {
                assert!(matches!(source, CredentialSource::Keychain));
                assert_eq!(key.expose(), "stored-key");
            }
            _ => panic!("expected ApiKey"),
        }
    }

    #[test]
    // No credentials at all returns NotLoggedIn.
    fn no_credentials_returns_not_logged_in() {
        let store = CredentialStore::new_in_memory_for_tests();
        let err = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Gemini),
            mode: AuthPrecedenceMode::ExplicitFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: None,
            env: BTreeMap::new(),
            store: Some(&store),
        })
        .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::NotLoggedIn);
    }

    #[test]
    // A stored OAuth account with an expiry produces an OAuthBearer that
    // carries that expiry rather than discarding it.
    fn oauth_credential_carries_stored_expiry() {
        let store = CredentialStore::new_in_memory_for_tests();
        let aid = AccountId::new("oauth-1");
        let expiry = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let acct = StoredAccount {
            account_id: aid.clone(),
            provider: ProviderId::Anthropic,
            login_method: LoginMethodKind::OAuthBrowser,
            display_email: None,
            workspace_id: None,
            plan_type: None,
            created_at: OffsetDateTime::now_utc(),
            last_refresh_at: None,
            expires_at: Some(expiry),
            scopes: vec![],
            active: true,
            metadata: serde_json::json!({}),
            needs_reauth: false,
        };
        let secret = StoredCredentialSecret {
            access_token: Some(SecretString::new("at-token".into())),
            ..Default::default()
        };
        store.put(acct, &secret).unwrap();
        store
            .set_active(&ProviderId::Anthropic, aid.clone())
            .unwrap();
        let res = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Anthropic),
            mode: AuthPrecedenceMode::StoredFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: Some(&aid),
            env: BTreeMap::new(),
            store: Some(&store),
        })
        .unwrap();
        match res.credential {
            ResolvedCredential::OAuthBearer {
                access_token,
                expires_at,
                ..
            } => {
                assert_eq!(access_token.expose(), "at-token");
                assert_eq!(expires_at, Some(expiry));
            }
            _ => panic!("expected OAuthBearer"),
        }
    }

    /// Backend whose `get` always fails with a hard backend error,
    /// simulating an unavailable store / I/O failure.
    struct FailingGetBackend;

    impl crate::store::SecretBackend for FailingGetBackend {
        fn name(&self) -> &'static str {
            "failing_get"
        }
        fn put(
            &self,
            _provider: &ProviderId,
            _account_id: &AccountId,
            _secret: &StoredCredentialSecret,
        ) -> Result<(), AuthError> {
            Ok(())
        }
        fn get(
            &self,
            _provider: &ProviderId,
            _account_id: &AccountId,
        ) -> Result<Option<StoredCredentialSecret>, AuthError> {
            Err(AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                "backend unavailable",
            ))
        }
        fn delete(&self, _provider: &ProviderId, _account_id: &AccountId) -> Result<(), AuthError> {
            Ok(())
        }
    }

    #[test]
    // A genuine backend error during lookup is surfaced to the caller
    // rather than collapsed into NotLoggedIn.
    fn backend_error_propagates_not_not_logged_in() {
        use std::sync::Arc;
        let aid = AccountId::new("k1");
        let acct = StoredAccount {
            account_id: aid.clone(),
            provider: ProviderId::Anthropic,
            login_method: LoginMethodKind::ApiKey,
            display_email: None,
            workspace_id: None,
            plan_type: None,
            created_at: OffsetDateTime::now_utc(),
            last_refresh_at: None,
            expires_at: None,
            scopes: vec![],
            active: true,
            metadata: serde_json::json!({}),
            needs_reauth: false,
        };
        // Seed the index so the active-account lookup hits the backend.
        let store = CredentialStore {
            mode: crate::types::CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: Arc::new(FailingGetBackend),
            index: Arc::new(std::sync::Mutex::new(crate::store::AccountIndex::default())),
            index_path: None,
        };
        store.index.lock().unwrap().upsert(acct);
        let err = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Anthropic),
            mode: AuthPrecedenceMode::StoredFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: Some(&aid),
            env: BTreeMap::new(),
            store: Some(&store),
        })
        .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
    }

    #[test]
    // Resolver surfaces a precedence-conflict warning when env var and
    // stored credential both exist.
    fn warns_on_env_vs_stored_conflict() {
        let (store, aid) = store_with_account(ProviderId::Anthropic, "k1", "stored-key");
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".into(), "env-key".into());
        let res = resolve_credential(AuthResolverInputs {
            provider: Some(ProviderId::Anthropic),
            mode: AuthPrecedenceMode::ExplicitFirst,
            explicit_api_key: None,
            explicit_account: None,
            active_account: Some(&aid),
            env,
            store: Some(&store),
        })
        .unwrap();
        assert!(!res.warnings.is_empty());
        assert!(res.warnings[0].contains("ANTHROPIC_API_KEY"));
    }
}
