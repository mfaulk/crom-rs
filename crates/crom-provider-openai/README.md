# crom-provider-openai

OpenAI model provider for crom.

Implements `crom_core::ModelProvider` against OpenAI's
[Chat Completions](https://platform.openai.com/docs/api-reference/chat) API. A
thin adapter: it maps a `ModelRequest` to a `chat.completions` body, makes the
call, and normalizes the response into `ModelEvent`s.

## What's here

- The `ModelProvider` implementation.
- `OpenAiTransport` — a pluggable HTTP transport (`post_chat`, `list_models`)
  so tests can mock responses without real network calls.

Credentials are passed in at call time as a `ResolvedCredential` (from
`crom-auth`), keeping credential storage out of the request path.

## Usage

Enabled in `crom-sdk` via the `openai` feature and selectable from the
`crom` CLI.
