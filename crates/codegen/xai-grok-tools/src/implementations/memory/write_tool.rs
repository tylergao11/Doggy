//! `memory_write` tool — curated MEMORY.md entry mutations (add / replace / remove).
//!
//! Entries are blank-line-separated blocks. `replace` / `remove` locate an entry
//! by unique substring match on `old_text`. Capacity is enforced only on this
//! path (not on dream / `write_long_term`).
//!
//! Index refresh: MEMORY.md lives under the watched memory tree; the file
//! watcher marks it dirty and the next `memory_search` reindexes. No explicit
//! reindex on the success path.

use std::sync::Arc;

use super::types::MemoryWriteInput;
use crate::types::memory_backend::MemoryBackend;
use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};

/// Default curated MEMORY.md char limit per scope (global / workspace).
pub const DEFAULT_CURATED_CHAR_LIMIT: u64 = 2200;

/// Hard budget for the frozen system-prompt digest block (header + body).
pub const MEMORY_DIGEST_BUDGET: usize = 3200;

/// Zero-width / bidi control code points that must not enter curated memory.
fn contains_invisible_unicode(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            '\u{200B}' // ZERO WIDTH SPACE
                | '\u{200C}' // ZERO WIDTH NON-JOINER
                | '\u{200D}' // ZERO WIDTH JOINER
                | '\u{2060}' // WORD JOINER
                | '\u{FEFF}' // BOM / ZERO WIDTH NO-BREAK SPACE
                | '\u{202A}'..='\u{202E}' // bidi embeddings / overrides
                | '\u{2066}'..='\u{2069}' // bidi isolates
        )
    })
}

/// Lightweight entry safety: invisible Unicode or system-reminder injection.
pub(crate) fn content_safety_error(content: &str) -> Option<&'static str> {
    if contains_invisible_unicode(content) {
        return Some(
            "Content rejected: contains invisible Unicode (zero-width or bidirectional controls).",
        );
    }
    if content.to_ascii_lowercase().contains("<system-reminder") {
        return Some("Content rejected: must not contain `<system-reminder`.");
    }
    None
}

/// Split MEMORY.md into blank-line-separated entries (trim; drop empties).
pub(crate) fn parse_entries(content: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    for line in content.split('\n') {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                entries.push(current.trim().to_string());
            }
            current.clear();
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.trim().is_empty() {
        entries.push(current.trim().to_string());
    }
    entries
}

/// Join entries with a blank line between them (canonical MEMORY.md body).
pub(crate) fn join_entries(entries: &[String]) -> String {
    entries.join("\n\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocateError {
    None,
    Ambiguous(usize),
}

/// Locate a single entry whose text contains `old_text` as a substring.
pub(crate) fn locate_entry(entries: &[String], old_text: &str) -> Result<usize, LocateError> {
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.contains(old_text))
        .map(|(i, _)| i)
        .collect();
    match matches.len() {
        0 => Err(LocateError::None),
        1 => Ok(matches[0]),
        n => Err(LocateError::Ambiguous(n)),
    }
}

/// Result of applying an action in memory (before disk write / capacity gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyResult {
    /// New entry list ready to write.
    Ready {
        entries: Vec<String>,
        message: String,
        /// Chars contributed by the add/replace payload (for overflow text).
        action_chars: usize,
        action_label: &'static str,
    },
    /// Successful no-op (identical add).
    DuplicateNoop,
    /// Semantic / locate failure (no capacity issue).
    Error {
        message: String,
        entries: Vec<String>,
    },
}

/// Apply add / replace / remove against the current entry list (no I/O).
pub(crate) fn apply_action(
    action: &str,
    entries: &[String],
    content: Option<&str>,
    old_text: Option<&str>,
) -> ApplyResult {
    match action {
        "add" => {
            let Some(content) = content.map(str::trim).filter(|s| !s.is_empty()) else {
                return ApplyResult::Error {
                    message: "content is required for action 'add'.".into(),
                    entries: entries.to_vec(),
                };
            };
            if entries.iter().any(|e| e == content) {
                return ApplyResult::DuplicateNoop;
            }
            let mut next = entries.to_vec();
            next.push(content.to_string());
            ApplyResult::Ready {
                entries: next,
                message: "Entry added.".into(),
                action_chars: content.len(),
                action_label: "add",
            }
        }
        "replace" => {
            let Some(old_text) = old_text.map(str::trim).filter(|s| !s.is_empty()) else {
                return ApplyResult::Error {
                    message: "old_text is required for action 'replace'.".into(),
                    entries: entries.to_vec(),
                };
            };
            let Some(content) = content.map(str::trim).filter(|s| !s.is_empty()) else {
                return ApplyResult::Error {
                    message: "content is required for action 'replace'.".into(),
                    entries: entries.to_vec(),
                };
            };
            match locate_entry(entries, old_text) {
                Ok(idx) => {
                    let mut next = entries.to_vec();
                    next[idx] = content.to_string();
                    ApplyResult::Ready {
                        entries: next,
                        message: "Entry replaced.".into(),
                        action_chars: content.len(),
                        action_label: "replace",
                    }
                }
                Err(LocateError::None) => ApplyResult::Error {
                    message: format!(
                        "No entry matched '{old_text}'. Provide a more specific unique substring; see current_entries."
                    ),
                    entries: entries.to_vec(),
                },
                Err(LocateError::Ambiguous(n)) => ApplyResult::Error {
                    message: format!(
                        "Ambiguous old_text '{old_text}' matched {n} entries. Provide a more specific unique substring; see current_entries."
                    ),
                    entries: entries.to_vec(),
                },
            }
        }
        "remove" => {
            let Some(old_text) = old_text.map(str::trim).filter(|s| !s.is_empty()) else {
                return ApplyResult::Error {
                    message: "old_text is required for action 'remove'.".into(),
                    entries: entries.to_vec(),
                };
            };
            match locate_entry(entries, old_text) {
                Ok(idx) => {
                    let mut next = entries.to_vec();
                    next.remove(idx);
                    ApplyResult::Ready {
                        entries: next,
                        message: "Entry removed.".into(),
                        action_chars: 0,
                        action_label: "remove",
                    }
                }
                Err(LocateError::None) => ApplyResult::Error {
                    message: format!(
                        "No entry matched '{old_text}'. Provide a more specific unique substring; see current_entries."
                    ),
                    entries: entries.to_vec(),
                },
                Err(LocateError::Ambiguous(n)) => ApplyResult::Error {
                    message: format!(
                        "Ambiguous old_text '{old_text}' matched {n} entries. Provide a more specific unique substring; see current_entries."
                    ),
                    entries: entries.to_vec(),
                },
            }
        }
        other => ApplyResult::Error {
            message: format!("Unknown action '{other}'. Use add, replace, or remove."),
            entries: entries.to_vec(),
        },
    }
}

/// Structured overflow error matching the P1 plan JSON shape.
pub(crate) fn overflow_json(
    used: usize,
    limit: u64,
    action: &str,
    n: usize,
    current_entries: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "success": false,
        "error": format!(
            "Memory at {used}/{limit} chars. This {action} ({n} chars) would exceed the limit. \
             Consolidate now: use 'replace' to merge overlapping entries into shorter ones, \
             or 'remove' stale entries (see current_entries), then retry — all in this turn."
        ),
        "current_entries": current_entries,
        "usage": format!("{used}/{limit}"),
    })
}

fn error_json(message: &str, current_entries: &[String]) -> serde_json::Value {
    serde_json::json!({
        "success": false,
        "error": message,
        "current_entries": current_entries,
    })
}

fn success_json(message: &str, entries: &[String], used: usize, limit: u64) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "message": message,
        "current_entries": entries,
        "usage": format!("{used}/{limit}"),
    })
}

/// Assemble a frozen system-prompt digest from workspace + global MEMORY.md.
///
/// Workspace entries first, then global. Total rendered length (header + body)
/// is capped at `budget` without cutting mid-entry. Returns `None` when both
/// sources yield no entries.
pub fn assemble_memory_digest(workspace_md: &str, global_md: &str, budget: usize) -> Option<String> {
    let mut entries = parse_entries(workspace_md);
    entries.extend(parse_entries(global_md));
    if entries.is_empty() {
        return None;
    }

    let full_body = join_entries(&entries);
    let used = full_body.chars().count();
    let limit = budget;
    let pct = if limit == 0 {
        0
    } else {
        ((used * 100) / limit).min(100)
    };
    let header = format!("MEMORY [{pct}% — {used}/{limit} chars]");

    // Fit whole entries under budget (header + "\n" + body).
    let mut kept: Vec<String> = Vec::new();
    for entry in &entries {
        let mut candidate = kept.clone();
        candidate.push(entry.clone());
        let body = join_entries(&candidate);
        let total = header.chars().count() + 1 + body.chars().count();
        if total > budget {
            break;
        }
        kept.push(entry.clone());
    }

    if kept.is_empty() {
        // Even the first entry alone exceeds budget — still emit header only
        // if it fits; otherwise omit digest entirely.
        if header.chars().count() <= budget {
            return Some(header);
        }
        return None;
    }

    Some(format!("{header}\n{}", join_entries(&kept)))
}

#[derive(Debug, Default)]
pub struct MemoryWriteImpl;

impl crate::types::tool_metadata::ToolMetadata for MemoryWriteImpl {
    fn kind(&self) -> ToolKind {
        ToolKind::MemoryWrite
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Write curated long-term memory (MEMORY.md). Actions: add (append entry), \
         replace (unique substring match → whole-entry swap), remove (unique substring match). \
         Scope defaults to workspace; use global for cross-project notes. \
         Entries are blank-line-separated. Identical add is a no-op. \
         Capacity is limited — on overflow, consolidate with replace/remove then retry in this turn."
    }
}

impl xai_tool_runtime::Tool for MemoryWriteImpl {
    type Args = MemoryWriteInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("memory_write").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "memory_write",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: MemoryWriteInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let Some(memory) = resources
            .lock()
            .await
            .get::<Arc<dyn MemoryBackend>>()
            .cloned()
        else {
            return Ok(ToolOutput::Text(
                "Memory is not enabled. Use --experimental-memory to enable.".into(),
            ));
        };

        let scope = input
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace");
        if scope != "workspace" && scope != "global" {
            return Ok(ToolOutput::Text(
                error_json(
                    &format!("Invalid scope '{scope}'. Use 'workspace' or 'global'."),
                    &[],
                )
                .to_string()
                .into(),
            ));
        }

        let action = input.action.trim().to_ascii_lowercase();

        // Safety checks on content that would be written.
        if matches!(action.as_str(), "add" | "replace") {
            if let Some(ref c) = input.content
                && let Some(err) = content_safety_error(c)
            {
                return Ok(ToolOutput::Text(error_json(err, &[]).to_string().into()));
            }
        }

        // Ephemeral workspace cannot persist workspace MEMORY.md.
        if scope == "workspace" && memory.is_ephemeral_workspace() {
            return Ok(ToolOutput::Text(
                error_json(
                    "Workspace memory writes are skipped for ephemeral (temp-dir) workspaces. \
                     Use scope 'global', or run from a non-temporary working directory.",
                    &[],
                )
                .to_string()
                .into(),
            ));
        }

        tracing::info!(
            target: crate::types::memory_backend::MEMORY_LOG_TARGET,
            action = %action,
            scope,
            "MEMORY_WRITE: invoked"
        );

        let existing = memory.read_curated_memory(scope).map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("memory_write").expect("valid"),
                format!("memory write failed to read: {e}"),
            )
        })?;
        let entries = parse_entries(&existing);
        let limit = memory.curated_char_limit();

        let applied = apply_action(
            &action,
            &entries,
            input.content.as_deref(),
            input.old_text.as_deref(),
        );

        match applied {
            ApplyResult::DuplicateNoop => {
                let used = join_entries(&entries).chars().count();
                Ok(ToolOutput::Text(
                    success_json("duplicate, not added", &entries, used, limit)
                        .to_string()
                        .into(),
                ))
            }
            ApplyResult::Error { message, entries } => Ok(ToolOutput::Text(
                error_json(&message, &entries).to_string().into(),
            )),
            ApplyResult::Ready {
                entries: next,
                message,
                action_chars,
                action_label,
            } => {
                let body = join_entries(&next);
                let new_used = body.chars().count();

                // Capacity gate only for add/replace (remove always shrinks).
                if matches!(action_label, "add" | "replace") && new_used as u64 > limit {
                    let used = join_entries(&entries).chars().count();
                    return Ok(ToolOutput::Text(
                        overflow_json(used, limit, action_label, action_chars, &entries)
                            .to_string()
                            .into(),
                    ));
                }

                let outcome = memory.write_curated_memory(scope, &body).map_err(|e| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("memory_write").expect("valid"),
                        format!("memory write failed: {e}"),
                    )
                })?;
                if outcome.skipped_ephemeral {
                    return Ok(ToolOutput::Text(
                        error_json(
                            "Workspace memory write was skipped (ephemeral workspace). \
                             Nothing was written.",
                            &entries,
                        )
                        .to_string()
                        .into(),
                    ));
                }

                // Index refresh relies on the memory file watcher (see module docs).
                Ok(ToolOutput::Text(
                    success_json(&message, &next, new_used, limit)
                        .to_string()
                        .into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_blank_line_entries() {
        let entries = parse_entries("alpha\n\nbeta line\nstill beta\n\ngamma");
        assert_eq!(entries, vec!["alpha", "beta line\nstill beta", "gamma"]);
    }

    #[test]
    fn parse_empty_and_whitespace() {
        assert!(parse_entries("").is_empty());
        assert!(parse_entries("  \n\n  ").is_empty());
    }

    #[test]
    fn join_round_trip() {
        let entries = vec!["a".into(), "b\nc".into()];
        assert_eq!(join_entries(&entries), "a\n\nb\nc");
    }

    #[test]
    fn add_appends() {
        let entries = vec!["one".into()];
        match apply_action("add", &entries, Some("two"), None) {
            ApplyResult::Ready { entries: next, .. } => {
                assert_eq!(next, vec!["one", "two"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn add_identical_is_noop() {
        let entries = vec!["same".into()];
        assert_eq!(
            apply_action("add", &entries, Some("same"), None),
            ApplyResult::DuplicateNoop
        );
    }

    #[test]
    fn replace_unique_substring() {
        let entries = vec!["prefer tabs".into(), "use UTC".into()];
        match apply_action("replace", &entries, Some("prefer spaces"), Some("tabs")) {
            ApplyResult::Ready { entries: next, .. } => {
                assert_eq!(next, vec!["prefer spaces", "use UTC"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn remove_unique_substring() {
        let entries = vec!["prefer tabs".into(), "use UTC".into()];
        match apply_action("remove", &entries, None, Some("UTC")) {
            ApplyResult::Ready { entries: next, .. } => {
                assert_eq!(next, vec!["prefer tabs"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn locate_zero_hits_errors() {
        let entries = vec!["alpha".into()];
        match apply_action("remove", &entries, None, Some("zzz")) {
            ApplyResult::Error {
                message,
                entries: e,
            } => {
                assert!(message.contains("No entry matched"), "{message}");
                assert_eq!(e, entries);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn locate_multi_hits_errors() {
        let entries = vec!["foo bar".into(), "foo baz".into()];
        match apply_action("replace", &entries, Some("x"), Some("foo")) {
            ApplyResult::Error {
                message,
                entries: e,
            } => {
                assert!(message.contains("Ambiguous"), "{message}");
                assert_eq!(e, entries);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn safety_rejects_zero_width() {
        assert!(content_safety_error("hello\u{200B}world").is_some());
    }

    #[test]
    fn safety_rejects_system_reminder() {
        assert!(content_safety_error("x <system-reminder> y").is_some());
        assert!(content_safety_error("x <SYSTEM-REMINDER> y").is_some());
    }

    #[test]
    fn overflow_json_shape() {
        let v = overflow_json(2000, 2200, "add", 500, &["a".into()]);
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("2000/2200"));
        assert!(v["error"].as_str().unwrap().contains("This add (500 chars)"));
        assert_eq!(v["usage"], "2000/2200");
        assert_eq!(v["current_entries"][0], "a");
    }

    #[test]
    fn digest_none_when_empty() {
        assert!(assemble_memory_digest("", "", MEMORY_DIGEST_BUDGET).is_none());
        assert!(assemble_memory_digest("  \n\n", "", MEMORY_DIGEST_BUDGET).is_none());
    }

    #[test]
    fn digest_workspace_first_and_header() {
        let d = assemble_memory_digest("ws-entry", "global-entry", MEMORY_DIGEST_BUDGET).unwrap();
        assert!(d.starts_with("MEMORY ["), "{d}");
        assert!(d.contains("% — "), "{d}");
        assert!(d.contains(" chars]"), "{d}");
        let body = d.split_once('\n').unwrap().1;
        assert!(body.starts_with("ws-entry"), "{d}");
        assert!(body.contains("global-entry"), "{d}");
    }

    #[test]
    fn digest_respects_budget_no_half_entry() {
        let ws = "short\n\n";
        let long = "x".repeat(200);
        let d = assemble_memory_digest(ws, &long, 40).unwrap();
        assert!(d.len() <= 40, "len={}", d.len());
        assert!(d.contains("short"), "{d}");
        if d.contains('x') {
            assert!(d.contains(&long) || !d.contains('x'));
        } else {
            assert!(!d.contains(&long[..10.min(long.len())]));
        }
    }

    #[test]
    fn digest_over_budget_truncates_entries() {
        let e1 = "a".repeat(100);
        let e2 = "b".repeat(100);
        let e3 = "c".repeat(100);
        let body = join_entries(&[e1.clone(), e2.clone(), e3.clone()]);
        let d = assemble_memory_digest(&body, "", 150).unwrap();
        assert!(d.chars().count() <= 150, "len={}", d.chars().count());
        assert!(d.contains(&e1));
        assert!(!d.contains(&e3) || d.chars().count() <= 150);
    }
}
