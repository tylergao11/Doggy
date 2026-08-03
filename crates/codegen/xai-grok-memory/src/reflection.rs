//! Post-session reflection — distill a finished conversation into curated
//! `MEMORY.md` edits.
//!
//! This is the pure half: prompt, response parsing, and applying operations
//! against in-memory entry lists. The session actor owns the model call and
//! the disk writes (see `memory_dream.rs`).
//!
//! Operations reuse the `memory_write` primitives, so reflection is subject
//! to the same entry semantics, safety checks, and capacity limit as an
//! agent-issued write.

use xai_grok_tools::implementations::memory::{
    apply_action, content_safety_error, join_entries, parse_entries, ApplyResult,
};

const LOG: &str = xai_grok_telemetry::memory_log::TARGET;

pub const REFLECTION_SYSTEM_PROMPT: &str = "\
You are the curator of a coding agent's long-term memory. A session just ended. \
Decide what — if anything — is worth remembering across future sessions.

You will receive the recent conversation, then the current curated memory entries \
with their character usage.

Record only durable facts:
- Preferences the user stated or corrected (\"always use X\", \"never do Y\")
- Project invariants: architecture decisions, conventions, non-obvious constraints
- Environment facts that cost time to rediscover (paths, commands, versions, gotchas)

Never record:
- Task progress, what was done this session, or what to do next
- Anything already implied by the code, or discoverable in seconds
- Restatements of entries already present
- Speculation, or the user's mood

Rules:
- Each entry is one self-contained fact, at most 240 characters, written so a \
future session with no context understands it.
- Prefer `replace` over `add` when an existing entry covers the same topic — \
merge instead of accumulating near-duplicates.
- Use `remove` for entries the session proved wrong or obsolete.
- Pick the scope by lifetime, not by topic: `user` for who the user is and how \
they want to be worked with (stable across every project); `global` for technical \
facts true across all projects; `workspace` for this project only. Default to \
`workspace`.
- Capacity is finite. If memory is near full, consolidate rather than append.
- Most sessions warrant no change. When in doubt, record nothing.

Respond with a JSON array of operations and nothing else — no prose, no code fences:
[{\"action\":\"add\",\"scope\":\"workspace\",\"content\":\"...\"},
 {\"action\":\"replace\",\"scope\":\"global\",\"old_text\":\"unique substring of the entry\",\"content\":\"...\"},
 {\"action\":\"remove\",\"scope\":\"workspace\",\"old_text\":\"unique substring of the entry\"}]

`old_text` must be a substring matching exactly one existing entry. \
If nothing is worth recording, respond with [].";

/// One reflection operation, mirroring the `memory_write` tool arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionOp {
    pub action: String,
    pub scope: String,
    pub content: Option<String>,
    pub old_text: Option<String>,
}

/// Why an operation was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedOp {
    pub op: ReflectionOp,
    pub reason: String,
}

/// Outcome of applying a batch of operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReflectionApplyReport {
    /// New user-profile body, `None` when unchanged.
    pub user_body: Option<String>,
    /// New workspace body, `None` when unchanged.
    pub workspace_body: Option<String>,
    /// New global body, `None` when unchanged.
    pub global_body: Option<String>,
    pub applied: usize,
    pub skipped: Vec<SkippedOp>,
}

impl ReflectionApplyReport {
    pub fn changed(&self) -> bool {
        self.user_body.is_some() || self.workspace_body.is_some() || self.global_body.is_some()
    }
}

/// Render the curated-memory context appended as the final user message.
pub fn build_curated_context(
    user_md: &str,
    workspace_md: &str,
    global_md: &str,
    limit: u64,
) -> String {
    let user = parse_entries(user_md);
    let ws = parse_entries(workspace_md);
    let gl = parse_entries(global_md);

    let mut out = String::from(
        "The session above has ended. Here is the current curated memory.\n\n",
    );
    for (label, entries) in [("user", &user), ("workspace", &ws), ("global", &gl)] {
        let used = join_entries(entries).chars().count();
        out.push_str(&format!(
            "## {label} memory [{used}/{limit} chars]\n\n"
        ));
        if entries.is_empty() {
            out.push_str("(empty)\n\n");
        } else {
            for entry in entries.iter() {
                out.push_str("- ");
                out.push_str(entry);
                out.push('\n');
            }
            out.push('\n');
        }
    }
    out.push_str("Respond with the JSON array of operations, or [] for no change.");
    out
}

/// Extract the first balanced top-level JSON array from a model response.
///
/// Models wrap arrays in prose or code fences often enough that plain
/// `serde_json::from_str` on the whole response is not usable.
fn extract_json_array(response: &str) -> Option<&str> {
    let bytes = response.as_bytes();
    let start = response.find('[')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&response[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse the model response into at most `max_ops` well-formed operations.
///
/// Malformed individual entries are dropped; only a response with no
/// extractable array at all is an error.
pub fn parse_reflection_ops(
    response: &str,
    max_ops: usize,
) -> Result<Vec<ReflectionOp>, String> {
    let array = extract_json_array(response).ok_or_else(|| {
        format!(
            "no JSON array in response (first 120 chars: {:?})",
            response.chars().take(120).collect::<String>()
        )
    })?;
    let parsed: serde_json::Value =
        serde_json::from_str(array).map_err(|e| format!("invalid JSON array: {e}"))?;
    let items = parsed
        .as_array()
        .ok_or_else(|| "parsed value is not an array".to_string())?;

    let mut ops = Vec::new();
    for item in items {
        let Some(action) = field(item, "action").map(|a| a.to_ascii_lowercase()) else {
            continue;
        };
        if !matches!(action.as_str(), "add" | "replace" | "remove") {
            tracing::debug!(target: LOG, %action, "REFLECTION_PARSE: unknown action, dropped");
            continue;
        }
        let scope = field(item, "scope")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| crate::storage::MemoryScope::Workspace.as_str().to_owned());
        if crate::storage::MemoryScope::parse(&scope).is_none() {
            tracing::debug!(target: LOG, %scope, "REFLECTION_PARSE: unknown scope, dropped");
            continue;
        }
        ops.push(ReflectionOp {
            action,
            scope,
            content: field(item, "content"),
            old_text: field(item, "old_text"),
        });
        if ops.len() == max_ops {
            break;
        }
    }
    Ok(ops)
}

/// Apply operations against the three curated bodies.
///
/// Each operation is independent: one failing (unsafe content, ambiguous
/// `old_text`, capacity overflow) is recorded in `skipped` and the rest still
/// apply. Bodies are only returned for scopes that actually changed. The
/// capacity `limit` applies per scope, since each is a separate file.
///
/// When `workspace_writable` is false (ephemeral workspace) workspace-scoped
/// operations are skipped; user and global ones still apply.
pub fn apply_reflection_ops(
    ops: &[ReflectionOp],
    user_md: &str,
    workspace_md: &str,
    global_md: &str,
    limit: u64,
    workspace_writable: bool,
) -> ReflectionApplyReport {
    use crate::storage::MemoryScope;

    let mut user = parse_entries(user_md);
    let mut ws = parse_entries(workspace_md);
    let mut gl = parse_entries(global_md);
    let mut user_changed = false;
    let mut ws_changed = false;
    let mut gl_changed = false;
    let mut report = ReflectionApplyReport::default();

    for op in ops {
        // Rejected rather than defaulted: silently filing a typo'd scope under
        // one of the real layers writes to a file the model did not ask for.
        let Some(scope) = MemoryScope::parse(&op.scope) else {
            report.skipped.push(SkippedOp {
                op: op.clone(),
                reason: format!("unknown scope '{}'", op.scope),
            });
            continue;
        };
        if scope == MemoryScope::Workspace && !workspace_writable {
            report.skipped.push(SkippedOp {
                op: op.clone(),
                reason: "ephemeral workspace is not writable".into(),
            });
            continue;
        }
        if let Some(content) = op.content.as_deref()
            && let Some(err) = content_safety_error(content)
        {
            report.skipped.push(SkippedOp {
                op: op.clone(),
                reason: err.to_owned(),
            });
            continue;
        }

        let entries = match scope {
            MemoryScope::User => &mut user,
            MemoryScope::Workspace => &mut ws,
            MemoryScope::Global => &mut gl,
        };
        match apply_action(
            &op.action,
            entries,
            op.content.as_deref(),
            op.old_text.as_deref(),
        ) {
            ApplyResult::DuplicateNoop => {
                report.skipped.push(SkippedOp {
                    op: op.clone(),
                    reason: "duplicate entry".into(),
                });
            }
            ApplyResult::Error { message, .. } => {
                report.skipped.push(SkippedOp {
                    op: op.clone(),
                    reason: message,
                });
            }
            ApplyResult::Ready {
                entries: next,
                action_label,
                ..
            } => {
                let used = join_entries(&next).chars().count() as u64;
                if matches!(action_label, "add" | "replace") && used > limit {
                    report.skipped.push(SkippedOp {
                        op: op.clone(),
                        reason: format!("would exceed capacity ({used}/{limit} chars)"),
                    });
                    continue;
                }
                *entries = next;
                match scope {
                    MemoryScope::User => user_changed = true,
                    MemoryScope::Workspace => ws_changed = true,
                    MemoryScope::Global => gl_changed = true,
                }
                report.applied += 1;
            }
        }
    }

    if user_changed {
        report.user_body = Some(join_entries(&user));
    }
    if ws_changed {
        report.workspace_body = Some(join_entries(&ws));
    }
    if gl_changed {
        report.global_body = Some(join_entries(&gl));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(action: &str, scope: &str, content: Option<&str>, old: Option<&str>) -> ReflectionOp {
        ReflectionOp {
            action: action.into(),
            scope: scope.into(),
            content: content.map(str::to_owned),
            old_text: old.map(str::to_owned),
        }
    }

    #[test]
    fn system_prompt_states_the_contract() {
        assert!(REFLECTION_SYSTEM_PROMPT.contains("JSON array"));
        assert!(REFLECTION_SYSTEM_PROMPT.contains("old_text"));
        assert!(REFLECTION_SYSTEM_PROMPT.contains("240 characters"));
        assert!(REFLECTION_SYSTEM_PROMPT.contains("record nothing"));
    }

    #[test]
    fn context_lists_entries_and_usage() {
        let ctx = build_curated_context("answers in Chinese", "prefer tabs\n\nuse UTC", "", 2200);
        assert!(ctx.contains("## user memory [18/2200 chars]"), "{ctx}");
        assert!(ctx.contains("- answers in Chinese"), "{ctx}");
        assert!(ctx.contains("## workspace memory [20/2200 chars]"), "{ctx}");
        assert!(ctx.contains("- prefer tabs"), "{ctx}");
        assert!(ctx.contains("## global memory [0/2200 chars]"), "{ctx}");
        assert!(ctx.contains("(empty)"), "{ctx}");
    }

    #[test]
    fn parse_plain_array() {
        let ops = parse_reflection_ops(
            r#"[{"action":"add","scope":"global","content":"user prefers pnpm"}]"#,
            4,
        )
        .unwrap();
        assert_eq!(ops, vec![op("add", "global", Some("user prefers pnpm"), None)]);
    }

    #[test]
    fn parse_array_wrapped_in_prose_and_fences() {
        let resp = "Here is what I'd record:\n```json\n[{\"action\":\"remove\",\
                    \"scope\":\"workspace\",\"old_text\":\"stale\"}]\n```\nDone.";
        let ops = parse_reflection_ops(resp, 4).unwrap();
        assert_eq!(ops, vec![op("remove", "workspace", None, Some("stale"))]);
    }

    #[test]
    fn parse_nested_array_inside_string_is_balanced() {
        let resp = r#"[{"action":"add","scope":"workspace","content":"use [brackets] freely"}]"#;
        let ops = parse_reflection_ops(resp, 4).unwrap();
        assert_eq!(ops[0].content.as_deref(), Some("use [brackets] freely"));
    }

    #[test]
    fn parse_empty_array_is_ok() {
        assert!(parse_reflection_ops("[]", 4).unwrap().is_empty());
    }

    #[test]
    fn parse_non_json_is_error_not_panic() {
        assert!(parse_reflection_ops("I don't think anything is worth saving.", 4).is_err());
        assert!(parse_reflection_ops("", 4).is_err());
    }

    #[test]
    fn parse_drops_malformed_items_keeps_valid() {
        let resp = r#"[{"action":"frobnicate","content":"x"},
                       {"scope":"workspace"},
                       {"action":"add","scope":"martian","content":"y"},
                       {"action":"add","content":"defaults to workspace"}]"#;
        let ops = parse_reflection_ops(resp, 8).unwrap();
        assert_eq!(ops, vec![op("add", "workspace", Some("defaults to workspace"), None)]);
    }

    /// The system prompt offers three scopes, so the parser must accept all
    /// three — dropping `user` here would silently discard profile edits.
    #[test]
    fn parse_accepts_every_documented_scope() {
        let resp = r#"[{"action":"add","scope":"user","content":"a"},
                       {"action":"add","scope":"workspace","content":"b"},
                       {"action":"add","scope":"global","content":"c"}]"#;
        let ops = parse_reflection_ops(resp, 8).unwrap();
        let scopes: Vec<&str> = ops.iter().map(|o| o.scope.as_str()).collect();
        assert_eq!(scopes, ["user", "workspace", "global"]);
    }

    #[test]
    fn parse_truncates_to_max_ops() {
        let resp = r#"[{"action":"add","content":"a"},{"action":"add","content":"b"},
                       {"action":"add","content":"c"}]"#;
        assert_eq!(parse_reflection_ops(resp, 2).unwrap().len(), 2);
    }

    #[test]
    fn apply_add_and_replace_across_scopes() {
        let ops = vec![
            op("add", "workspace", Some("build with cargo xtask"), None),
            op("replace", "global", Some("prefer pnpm"), Some("npm")),
        ];
        let report = apply_reflection_ops(&ops, "", "existing note", "use npm", 2200, true);
        assert_eq!(report.applied, 2);
        assert_eq!(
            report.workspace_body.as_deref(),
            Some("existing note\n\nbuild with cargo xtask")
        );
        assert_eq!(report.global_body.as_deref(), Some("prefer pnpm"));
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn apply_skips_overflow_without_touching_body() {
        let big = "x".repeat(200);
        let ops = vec![
            op("add", "workspace", Some(&big), None),
            op("add", "workspace", Some("small"), None),
        ];
        let report = apply_reflection_ops(&ops, "", "seed", "", 100, true);
        assert_eq!(report.applied, 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("capacity"));
        let body = report.workspace_body.unwrap();
        assert!(!body.contains(&big));
        assert!(body.contains("small"));
    }

    #[test]
    fn apply_skips_ambiguous_and_missing_old_text() {
        let ops = vec![
            op("replace", "workspace", Some("z"), Some("foo")),
            op("remove", "workspace", None, Some("nonexistent")),
        ];
        let report = apply_reflection_ops(&ops, "", "foo bar\n\nfoo baz", "", 2200, true);
        assert_eq!(report.applied, 0);
        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped[0].reason.contains("Ambiguous"));
        assert!(report.skipped[1].reason.contains("No entry matched"));
        assert!(!report.changed());
    }

    #[test]
    fn apply_rejects_unsafe_content() {
        let ops = vec![op("add", "workspace", Some("sneaky\u{200B}entry"), None)];
        let report = apply_reflection_ops(&ops, "", "", "", 2200, true);
        assert_eq!(report.applied, 0);
        assert!(report.skipped[0].reason.contains("invisible Unicode"));
    }

    #[test]
    fn apply_ephemeral_skips_workspace_keeps_global() {
        let ops = vec![
            op("add", "workspace", Some("ws note"), None),
            op("add", "global", Some("gl note"), None),
        ];
        let report = apply_reflection_ops(&ops, "", "", "", 2200, false);
        assert_eq!(report.applied, 1);
        assert!(report.workspace_body.is_none());
        assert_eq!(report.global_body.as_deref(), Some("gl note"));
        assert!(report.skipped[0].reason.contains("ephemeral"));
    }

    #[test]
    fn apply_no_ops_leaves_bodies_untouched() {
        let report = apply_reflection_ops(&[], "seed", "seed", "seed", 2200, true);
        assert!(!report.changed());
        assert_eq!(report.applied, 0);
    }

    #[test]
    fn apply_routes_user_scope_to_its_own_body() {
        let ops = vec![
            op("add", "user", Some("answers in Chinese"), None),
            op("add", "workspace", Some("build with cargo xtask"), None),
        ];
        let report = apply_reflection_ops(&ops, "", "", "", 2200, true);
        assert_eq!(report.applied, 2);
        assert_eq!(report.user_body.as_deref(), Some("answers in Chinese"));
        assert_eq!(
            report.workspace_body.as_deref(),
            Some("build with cargo xtask")
        );
        assert!(report.global_body.is_none());
    }

    /// The profile survives an ephemeral workspace: it lives in the global
    /// directory, so only workspace-scoped ops are unwritable.
    #[test]
    fn apply_ephemeral_still_writes_the_user_profile() {
        let ops = vec![
            op("add", "user", Some("prefers terse replies"), None),
            op("add", "workspace", Some("ws note"), None),
        ];
        let report = apply_reflection_ops(&ops, "", "", "", 2200, false);
        assert_eq!(report.applied, 1);
        assert_eq!(report.user_body.as_deref(), Some("prefers terse replies"));
        assert!(report.workspace_body.is_none());
    }

    /// Each layer is a separate file, so capacity is counted per layer — a full
    /// workspace must not block a profile write.
    #[test]
    fn apply_budgets_capacity_per_scope() {
        let ops = vec![op("add", "user", Some("short"), None)];
        let near_full = "w".repeat(2190);
        let report = apply_reflection_ops(&ops, "", &near_full, "", 2200, true);
        assert_eq!(report.applied, 1);
        assert!(report.user_body.as_deref().unwrap().contains("short"));
    }

    /// A scope the model invented must be reported, not silently filed under
    /// whichever layer happens to be the fallback.
    #[test]
    fn apply_rejects_an_unknown_scope() {
        let ops = vec![op("add", "profile", Some("stray"), None)];
        let report = apply_reflection_ops(&ops, "", "", "", 2200, true);
        assert_eq!(report.applied, 0);
        assert!(!report.changed());
        assert!(report.skipped[0].reason.contains("unknown scope"), "{:?}", report.skipped);
    }
}
