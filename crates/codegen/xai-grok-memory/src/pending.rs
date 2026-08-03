//! Reflection operations staged for user approval.
//!
//! When `[memory.reflection] apply = "staged"`, the post-session reflection
//! writes its proposed edits here instead of touching `MEMORY.md`. The file is
//! append-only JSONL so a crash mid-write costs at most one operation, and so
//! two sessions ending at once cannot clobber each other's proposals.
//!
//! The TUI reads this file to drive the status-bar badge and the review list.
//! Nothing here blocks a turn: staging is fire-and-forget, and approval happens
//! whenever the user gets around to it.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::reflection::ReflectionOp;

/// File name inside the workspace memory directory.
pub const PENDING_FILE_NAME: &str = "reflection_pending.jsonl";

/// A staged operation together with the session that proposed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    /// Session that proposed the edit — shown in the review list so the user
    /// can tell a stale proposal from one they just watched happen.
    pub session_id: String,
    pub op: ReflectionOp,
}

/// Path to the pending file for a workspace memory directory.
pub fn pending_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PENDING_FILE_NAME)
}

/// Append proposed operations. Creates the parent directory as needed.
///
/// A no-op for an empty batch, so callers do not have to guard against
/// creating an empty file that would then read back as "0 pending".
pub fn append(path: &Path, session_id: &str, ops: &[ReflectionOp]) -> std::io::Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for op in ops {
        let line = serde_json::json!({
            "session_id": session_id,
            "action": op.action,
            "scope": op.scope,
            "content": op.content,
            "old_text": op.old_text,
        });
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// Read every staged operation, oldest first.
///
/// Tolerant by design: a missing file is empty, and an unparseable or
/// incomplete line is skipped rather than failing the whole read. A truncated
/// tail (killed mid-append) must not make the badge or the review list
/// unreachable.
pub fn read(path: &Path) -> Vec<PendingOp> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<PendingOp> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let action = value.get("action")?.as_str()?.to_owned();
    let scope = value.get("scope")?.as_str()?.to_owned();
    let string_field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Some(PendingOp {
        session_id: string_field("session_id").unwrap_or_default(),
        op: ReflectionOp {
            action,
            scope,
            content: string_field("content"),
            old_text: string_field("old_text"),
        },
    })
}

/// Number of staged operations. Reads the file — cheap enough for the badge
/// refresh points (session start, reflection completion, after a review) but
/// deliberately not called per frame.
pub fn count(path: &Path) -> usize {
    read(path).len()
}

/// Drop every staged operation by removing the file.
pub fn clear(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Rewrite the file with `keep` as its full contents.
///
/// Used after a partial review: the approved/discarded entries are dropped and
/// the rest stay staged. Writing an empty list removes the file so `count`
/// reports zero without a stray empty file lingering.
pub fn rewrite(path: &Path, keep: &[PendingOp]) -> std::io::Result<()> {
    if keep.is_empty() {
        return clear(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for entry in keep {
        let line = serde_json::json!({
            "session_id": entry.session_id,
            "action": entry.op.action,
            "scope": entry.op.scope,
            "content": entry.op.content,
            "old_text": entry.op.old_text,
        });
        body.push_str(&line.to_string());
        body.push('\n');
    }
    std::fs::write(path, body)
}

/// One-line summary of a staged operation, for the review list.
///
/// Entries are markdown and routinely multi-line, so the body is flattened and
/// clipped — the list is for deciding, not for reading the full text.
pub fn describe(index: usize, entry: &PendingOp) -> String {
    let body = entry
        .op
        .content
        .as_deref()
        .or(entry.op.old_text.as_deref())
        .unwrap_or("");
    let flattened = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped = clip(&flattened, 72);
    format!(
        "  {index}. {} [{}] {clipped}",
        entry.op.action, entry.op.scope
    )
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

/// What resolving the queue did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveOutcome {
    /// Operations that landed in a curated file.
    pub applied: usize,
    /// Operations dropped at the user's request, without being applied.
    pub discarded: usize,
    /// Why individual operations did not land (unsafe content, ambiguous
    /// `old_text`, capacity overflow).
    pub skipped: Vec<String>,
    /// Operations still queued afterwards. Always zero today — resolving is
    /// all-or-nothing — but reported so the caller drives its indicator off
    /// the real number rather than assuming.
    pub remaining: usize,
}

impl ResolveOutcome {
    /// Human-readable result for the review surface.
    ///
    /// `approved` distinguishes the two callers, which an all-zero outcome
    /// cannot: approving and discarding an empty queue look identical.
    pub fn summary(&self, approved: bool) -> String {
        if !approved {
            return match self.discarded {
                0 => "No memory edits were waiting for approval.".to_owned(),
                n => format!("Discarded {n} staged memory edit(s)."),
            };
        }
        if self.applied == 0 && self.skipped.is_empty() {
            return "No memory edits were waiting for approval.".to_owned();
        }
        let mut summary = format!("Applied {} memory edit(s).", self.applied);
        if !self.skipped.is_empty() {
            summary.push_str(&format!(
                "\nSkipped {}:\n  {}",
                self.skipped.len(),
                self.skipped.join("\n  ")
            ));
        }
        summary
    }
}

/// Apply every staged operation to the curated files, then empty the queue.
///
/// Skipped operations are reported, not re-queued: an operation the apply
/// rejects will be rejected identically next time, so leaving it staged would
/// pin the indicator on forever with no way for the user to clear it.
///
/// The queue is emptied only after the curated writes succeed, so a failed
/// write leaves the proposals recoverable.
pub fn approve_all(
    storage: &crate::storage::MemoryStorage,
    limit: u64,
) -> std::io::Result<ResolveOutcome> {
    let path = storage.pending_file();
    let entries = read(&path);
    if entries.is_empty() {
        return Ok(ResolveOutcome::default());
    }

    let ops: Vec<ReflectionOp> = entries.into_iter().map(|e| e.op).collect();
    let user_md = std::fs::read_to_string(storage.user_memory_file()).unwrap_or_default();
    let workspace_md = std::fs::read_to_string(storage.workspace_memory_file()).unwrap_or_default();
    let global_md = std::fs::read_to_string(storage.global_memory_file()).unwrap_or_default();
    let report = crate::reflection::apply_reflection_ops(
        &ops,
        &user_md,
        &workspace_md,
        &global_md,
        limit,
        !storage.is_ephemeral(),
    );

    for (body, scope) in [
        (report.user_body.as_deref(), crate::storage::MemoryScope::User),
        (
            report.workspace_body.as_deref(),
            crate::storage::MemoryScope::Workspace,
        ),
        (
            report.global_body.as_deref(),
            crate::storage::MemoryScope::Global,
        ),
    ] {
        if let Some(body) = body {
            storage.write_long_term(scope, body)?;
        }
    }
    clear(&path)?;

    Ok(ResolveOutcome {
        applied: report.applied,
        discarded: 0,
        skipped: report
            .skipped
            .iter()
            .map(|s| format!("{} [{}]: {}", s.op.action, s.op.scope, s.reason))
            .collect(),
        remaining: count(&path),
    })
}

/// Drop every staged operation without touching the curated files.
pub fn discard_all(storage: &crate::storage::MemoryStorage) -> std::io::Result<ResolveOutcome> {
    let path = storage.pending_file();
    let discarded = count(&path);
    clear(&path)?;
    Ok(ResolveOutcome {
        applied: 0,
        discarded,
        skipped: Vec::new(),
        remaining: count(&path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(action: &str, scope: &str, content: Option<&str>, old: Option<&str>) -> ReflectionOp {
        ReflectionOp {
            action: action.to_owned(),
            scope: scope.to_owned(),
            content: content.map(str::to_owned),
            old_text: old.map(str::to_owned),
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());

        let ops = vec![
            op("add", "workspace", Some("prefers tabs"), None),
            op("remove", "global", None, Some("old fact")),
        ];
        append(&path, "sess-1", &ops).unwrap();

        let read_back = read(&path);
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].session_id, "sess-1");
        assert_eq!(read_back[0].op, ops[0]);
        assert_eq!(read_back[1].op, ops[1]);
    }

    #[test]
    fn append_accumulates_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());

        append(&path, "a", &[op("add", "workspace", Some("one"), None)]).unwrap();
        append(&path, "b", &[op("add", "global", Some("two"), None)]).unwrap();

        let all = read(&path);
        assert_eq!(all.len(), 2, "second session must not clobber the first");
        assert_eq!(all[0].session_id, "a");
        assert_eq!(all[1].session_id, "b");
    }

    #[test]
    fn append_empty_batch_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        append(&path, "sess", &[]).unwrap();
        assert!(!path.exists());
        assert_eq!(count(&path), 0);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        assert!(read(&path).is_empty());
        assert_eq!(count(&path), 0);
    }

    /// A process killed mid-append leaves a truncated last line. That must cost
    /// only the partial entry, not the whole queue.
    #[test]
    fn truncated_tail_does_not_hide_earlier_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        append(&path, "s", &[op("add", "workspace", Some("kept"), None)]).unwrap();

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{{\"action\":\"add\",\"sco").unwrap();
        drop(file);

        let all = read(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].op.content.as_deref(), Some("kept"));
    }

    /// Lines that parse as JSON but lack the required fields are not ops.
    #[test]
    fn lines_missing_required_fields_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        std::fs::write(
            &path,
            "{\"scope\":\"workspace\"}\n\
             {\"action\":\"add\"}\n\
             \n\
             {\"action\":\"add\",\"scope\":\"global\",\"content\":\"real\"}\n",
        )
        .unwrap();

        let all = read(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].op.content.as_deref(), Some("real"));
        assert_eq!(all[0].session_id, "", "absent session_id defaults to empty");
    }

    #[test]
    fn clear_removes_the_queue_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        append(&path, "s", &[op("add", "workspace", Some("x"), None)]).unwrap();

        clear(&path).unwrap();
        assert_eq!(count(&path), 0);
        clear(&path).unwrap();
    }

    #[test]
    fn rewrite_keeps_only_the_survivors() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        append(
            &path,
            "s",
            &[
                op("add", "workspace", Some("first"), None),
                op("add", "workspace", Some("second"), None),
                op("add", "global", Some("third"), None),
            ],
        )
        .unwrap();

        let mut all = read(&path);
        all.remove(1);
        rewrite(&path, &all).unwrap();

        let after = read(&path);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].op.content.as_deref(), Some("first"));
        assert_eq!(after[1].op.content.as_deref(), Some("third"));
    }

    #[test]
    fn rewrite_with_nothing_left_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        append(&path, "s", &[op("add", "workspace", Some("x"), None)]).unwrap();

        rewrite(&path, &[]).unwrap();
        assert!(!path.exists());
        assert_eq!(count(&path), 0);
    }

    // ── describe ───────────────────────────────────────────────────────

    #[test]
    fn describe_flattens_and_clips_multiline_bodies() {
        let entry = PendingOp {
            session_id: "s".into(),
            op: op(
                "add",
                "workspace",
                Some(&format!("## Head\n\n{}", "word ".repeat(40))),
                None,
            ),
        };
        let line = describe(1, &entry);
        assert!(!line.contains('\n'), "review list stays one row per entry");
        assert!(line.contains("1. add [workspace]"));
        assert!(line.ends_with('\u{2026}'), "long bodies are clipped");
    }

    #[test]
    fn describe_falls_back_to_old_text_for_removals() {
        let entry = PendingOp {
            session_id: "s".into(),
            op: op("remove", "global", None, Some("stale fact")),
        };
        assert!(describe(2, &entry).contains("stale fact"));
    }

    // ── approve / discard ──────────────────────────────────────────────

    fn storage_with_dirs() -> (tempfile::TempDir, crate::storage::MemoryStorage) {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let storage = crate::storage::MemoryStorage::with_paths(global, workspace);
        (dir, storage)
    }

    #[test]
    fn approve_writes_entries_and_empties_the_queue() {
        let (_dir, storage) = storage_with_dirs();
        append(
            &storage.pending_file(),
            "s",
            &[
                op("add", "workspace", Some("uses pnpm"), None),
                op("add", "global", Some("prefers UTC"), None),
            ],
        )
        .unwrap();

        let outcome = approve_all(&storage, 2200).unwrap();
        assert_eq!(outcome.applied, 2);
        assert_eq!(outcome.remaining, 0);
        assert!(outcome.skipped.is_empty());

        let ws = std::fs::read_to_string(storage.workspace_memory_file()).unwrap();
        let gl = std::fs::read_to_string(storage.global_memory_file()).unwrap();
        assert!(ws.contains("uses pnpm"));
        assert!(gl.contains("prefers UTC"));
        assert_eq!(count(&storage.pending_file()), 0);
    }

    /// An op the apply rejects must not stay queued — otherwise the indicator
    /// would be pinned on with nothing the user could do about it.
    #[test]
    fn approve_reports_rejects_without_requeueing_them() {
        let (_dir, storage) = storage_with_dirs();
        append(
            &storage.pending_file(),
            "s",
            &[
                op("add", "workspace", Some("real entry"), None),
                // No entry matches, so the replace cannot be located.
                op("replace", "workspace", Some("new"), Some("absent text")),
            ],
        )
        .unwrap();

        let outcome = approve_all(&storage, 2200).unwrap();
        assert_eq!(outcome.applied, 1);
        assert_eq!(outcome.skipped.len(), 1);
        assert!(outcome.skipped[0].contains("replace"));
        assert_eq!(
            count(&storage.pending_file()),
            0,
            "queue is emptied even when some ops were rejected"
        );
    }

    #[test]
    fn approve_on_an_empty_queue_is_a_no_op() {
        let (_dir, storage) = storage_with_dirs();
        let outcome = approve_all(&storage, 2200).unwrap();
        assert_eq!(outcome, ResolveOutcome::default());
        assert!(!storage.workspace_memory_file().exists());
    }

    #[test]
    fn summary_distinguishes_an_empty_approve_from_an_empty_discard() {
        let empty = ResolveOutcome::default();
        assert_eq!(empty.summary(true), empty.summary(false));

        let discarded = ResolveOutcome {
            discarded: 2,
            ..Default::default()
        };
        assert!(discarded.summary(false).contains("Discarded 2"));

        let applied = ResolveOutcome {
            applied: 3,
            skipped: vec!["replace [global]: no match".into()],
            ..Default::default()
        };
        let text = applied.summary(true);
        assert!(text.contains("Applied 3"));
        assert!(text.contains("Skipped 1"));
        assert!(text.contains("no match"));
    }

    #[test]
    fn discard_drops_the_queue_and_leaves_memory_untouched() {
        let (_dir, storage) = storage_with_dirs();
        storage
            .write_long_term(crate::storage::MemoryScope::Workspace, "existing entry")
            .unwrap();
        append(
            &storage.pending_file(),
            "s",
            &[op("add", "workspace", Some("unwanted"), None)],
        )
        .unwrap();

        let outcome = discard_all(&storage).unwrap();
        assert_eq!(outcome.discarded, 1);
        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.remaining, 0);

        let ws = std::fs::read_to_string(storage.workspace_memory_file()).unwrap();
        assert!(ws.contains("existing entry"));
        assert!(!ws.contains("unwanted"));
    }

    /// Entries with embedded newlines must survive the JSONL round trip —
    /// memory entries are multi-line markdown more often than not.
    #[test]
    fn multiline_content_survives_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_file(dir.path());
        let body = "## Heading\n\nline one\nline two";
        append(&path, "s", &[op("add", "workspace", Some(body), None)]).unwrap();

        let all = read(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].op.content.as_deref(), Some(body));
    }
}
