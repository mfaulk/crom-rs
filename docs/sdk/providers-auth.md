# Providers and auth

The SDK accepts pre-resolved credentials. Applications never need to call
`/login` or rely on the OS keychain unless they opt in explicitly.

## Provider configuration

```rust,no_run
use code_agent_sdk::{ProviderConfig, SecretString};

let cfg = ProviderConfig::anthropic_api_key(SecretString::new(
    std::env::var("ANTHROPIC_API_KEY").unwrap(),
));
```

| Variant                 | Source of credentials                                                          |
| ----------------------- | ------------------------------------------------------------------------------ |
| `AnthropicApiKey`       | Direct `SecretString`. Default for the SDK.                                    |
| `OpenAiApiKey`          | Direct API key. Requires the `openai` feature and a transport via `External`.  |
| `GeminiApiKey`          | Direct API key. Same caveat as OpenAI.                                         |
| `OAuthCredential`       | Pre-resolved `StoredCredentialSecret`. (Reserved; wrap in `External` for now.) |
| `FromAuthStore`         | Look up via the M10 auth store. Explicit opt-in.                               |
| `External`              | Bring your own `Arc<dyn ModelProvider>` and a `ResolvedCredential`.            |
| `Fake`                  | Use a deterministic provider for tests.                                        |

## Why not auto-detect?

- Hosts share the SDK across tenants; one shouldn't pick up another's
  credentials by accident.
- Web services don't want to touch the OS keychain.
- CLI/TUI consumers still use the explicit auth store; the SDK is a
  different layer.

## Safety

- `ProviderConfig::AnthropicApiKey` is `Debug`-redacted; the raw key never
  appears in logs.
- `SecretString` zeroes its bytes on drop.
- Credentials are never serialized into events, diagnostics, or session
  storage.
