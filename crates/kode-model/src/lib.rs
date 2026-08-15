pub mod catalog;
pub mod codex;
mod error;
mod mock;
mod openai;
pub mod opencode;
mod sse;
mod stream;
mod types;

pub use codex::{CodexAuth, CodexModel};
pub use error::{ModelError, Result};
pub use mock::MockModel;
pub use openai::{OpenAiModel, OpenAiOptions};
pub use stream::{ResponseAccumulator, collect_response};
pub use types::{
    FinishReason, Message, ModelCapabilities, ModelRequest, ModelResponse, StreamEvent, ToolCall,
    ToolSpec, Usage,
};

pub type ModelStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>;

#[async_trait::async_trait]
pub trait Model: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream>;
    fn capabilities(&self) -> ModelCapabilities;
}
