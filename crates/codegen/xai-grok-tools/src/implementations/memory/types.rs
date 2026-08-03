//! Input/output types for memory tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the `memory_search` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemorySearchInput {
    /// The search query string. Use specific technical terms rather than
    /// conversational language. Good: "authentication middleware patterns".
    /// Bad: "that thing we discussed about auth".
    pub query: String,
    /// Maximum number of results to return.
    ///
    /// When omitted the backend-configured value is used (typically 6 from
    /// `[memory.search].max_results`), so leaving this unset is preferred
    /// for normal queries.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Minimum relevance score threshold.
    ///
    /// When omitted the backend-configured value is used (typically 0.0 from
    /// `[memory.search].min_score`).
    #[serde(default)]
    pub min_score: Option<f64>,
}

/// Output schema for `memory_search` (used for JSON Schema generation only).
#[derive(Debug, JsonSchema)]
pub struct MemorySearchOutput {
    /// Formatted search results as markdown text.
    pub results: String,
}

/// Input for the `memory_get` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryGetInput {
    /// Path to the memory file to read.
    pub path: String,
    /// 0-based start line (default: beginning of file).
    #[serde(default)]
    pub from: Option<usize>,
    /// Maximum number of lines to return (default: all).
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Output schema for `memory_get` (used for JSON Schema generation only).
#[derive(Debug, JsonSchema)]
pub struct MemoryGetOutput {
    /// File content (optionally line-limited).
    pub content: String,
}

/// Input for the `memory_write` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteInput {
    /// Mutation action: `"add"`, `"replace"`, or `"remove"`.
    pub action: String,
    /// Target layer: `"workspace"` (default) for facts about this project,
    /// `"global"` for technical facts that hold across projects, or `"user"`
    /// for who the user is and how they want to be worked with.
    #[serde(default)]
    pub scope: Option<String>,
    /// Entry body for `add` / `replace`. Required for those actions.
    #[serde(default)]
    pub content: Option<String>,
    /// Unique substring locating the target entry for `replace` / `remove`.
    #[serde(default)]
    pub old_text: Option<String>,
}

/// Output schema for `memory_write` (used for JSON Schema generation only).
#[derive(Debug, JsonSchema)]
pub struct MemoryWriteOutput {
    /// JSON result text (success / overflow / error).
    pub result: String,
}
