mod bilibili;

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::VideoCandidate;

pub use bilibili::BilibiliProvider;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub keyword: String,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("request rate limited: {0}")]
    RateLimited(String),
    #[error("risk control triggered: {0}")]
    RiskControl(String),
    #[error("authorization required: {0}")]
    Unauthorized(String),
    #[error("temporary provider failure: {0}")]
    Temporary(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("permanent provider failure: {0}")]
    Permanent(String),
    #[error("provider is in backoff: {0}")]
    Backoff(String),
}

pub type ProviderResult<T> = std::result::Result<T, ProviderError>;

#[async_trait]
pub trait VideoSearchProvider: Send + Sync {
    async fn search(&self, query: &SearchQuery) -> ProviderResult<Vec<VideoCandidate>>;
    async fn enrich(&self, candidate: &VideoCandidate) -> ProviderResult<VideoCandidate>;
}
