# crom-anthropic

Anthropic Messages API provider for crom.

Implements `crom_core::ModelProvider` against Anthropic's public
[Messages API](https://docs.anthropic.com/en/api/messages). It sends
conversations and tool definitions, then parses the streaming Server-Sent
Events response into normalized `ModelEvent`s.

## What's here

- `AnthropicProvider` — the `ModelProvider` implementation.
- `AnthropicConfig` — API key, base URL, connect/read timeouts, and retry
  count. `AnthropicConfig::from_env()` reads `ANTHROPIC_API_KEY`.

The `connect_timeout` bounds connection setup only; `read_timeout` bounds the
gap between successive bytes, so a stalled stream is detected without
truncating a healthy long-running response.

## Usage

Construct a provider and hand it to the core agent runtime (directly or via
`crom-sdk`'s `anthropic` feature). Used by the `crom` CLI as the
default provider.
