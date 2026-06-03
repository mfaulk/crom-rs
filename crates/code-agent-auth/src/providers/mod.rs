//! Built-in [`crate::AuthProvider`] implementations.

pub mod anthropic;
pub mod codex;
pub mod fake;
pub mod gemini;
pub mod openai;

pub use anthropic::AnthropicAuthProvider;
pub use codex::CodexAuthProvider;
pub use fake::{FakeAuthProvider, FakeProviderState};
pub use gemini::GeminiAuthProvider;
pub use openai::OpenAiAuthProvider;
