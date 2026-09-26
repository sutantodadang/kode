/// Result of scoring context candidates against the task.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankOutcome {
    /// One score per candidate, same order, higher = more relevant.
    Scored(Vec<f32>),
    Skipped(String),
    Failed(String),
}

#[async_trait::async_trait]
pub trait ContextReranker: Send + Sync {
    async fn rerank(&self, query: &str, docs: &[String]) -> RerankOutcome;
}
