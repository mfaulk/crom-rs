# Providers and auth

The SDK accepts pre-resolved credentials. Applications never need to call
`/login` or rely on the OS keychain unless they opt in explicitly.

## Provider configuration

```rust,no_run
use crom_sdk::{ProviderConfig, SecretString};

let cfg = ProviderConfig::anthropic_api_key(SecretString::new(
    std::env::var("ANTHROPIC_API_KEY").unwrap(),
));
```

| Variant                 | Source of credentials                                                          |
| ----------------------- | ------------------------------------------------------------------------------ |
| `AnthropicApiKey`       | Direct `SecretString`. Default model `claude-sonnet-4-6`. `anthropic` feature. |
| `OpenAiApiKey`          | Direct API key. Default model `gpt-4.1`. `openai` feature (default on).        |
| `GeminiApiKey`          | Direct API key. Default model `gemini-1.5-pro`. `gemini` feature (default on). |
| `OAuthCredential`       | Pre-resolved `StoredCredentialSecret`. (Reserved; wrap in `External` for now.) |
| `FromAuthStore`         | Reserved; rejected with `provider.auth_store_disabled`. Use an API-key variant. |
| `External`              | Bring your own `Arc<dyn ModelProvider>` and a `ResolvedCredential`.            |
| `Fake`                  | Use a deterministic provider for tests.                                        |

`OpenAiApiKey` and `GeminiApiKey` call `OpenAiProvider::from_config` /
`GeminiProvider::from_config` from the provider crates, the same constructors
the `crom` CLI uses. `base_url: Some(..)` overrides the API endpoint (useful
for proxies and mock servers). OpenAI and Gemini use 30s connect / 300s idle /
600s total timeouts and retry transient failures (408, 429, 5xx, connect
errors, timeouts) up to 3 times; Anthropic retries up to 2 times. Each retry
emits `SdkEvent::ProviderRetry`; see [`../providers.md`](../providers.md). A
disabled provider feature fails with `provider.feature_disabled`.

## Why not auto-detect?

- Hosts share the SDK across tenants; one shouldn't pick up another's
  credentials by accident.
- Web services don't want to touch the OS keychain.
- CLI/TUI consumers still use the explicit auth store; the SDK is a
  different layer.

## Safety

- The `*ApiKey` variants of `ProviderConfig` are `Debug`-redacted; the raw key never
  appears in logs.
- `SecretString` zeroes its bytes on drop.
- Credentials are never serialized into events, diagnostics, or session
  storage.
