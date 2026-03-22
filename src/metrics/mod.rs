//! Usage metrics and token tracking for MCP responses.
//!
//! Tracks token counts to optimize response sizes and measure efficiency.

pub mod tokens;
pub mod usage;

#[doc(inline)]
pub use tokens::{
    TokenCounter, TruncationResult, count_tokens, truncate_to_tokens, truncate_with_continuation,
    truncate_with_ellipsis,
};
#[doc(inline)]
pub use usage::{ToolUsage, UsageMetrics};
