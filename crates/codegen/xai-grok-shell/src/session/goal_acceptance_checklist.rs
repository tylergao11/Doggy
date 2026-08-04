//! Dual-column acceptance checklist on the goal `plan.md`.
//!
//! Format (under `## Acceptance checklist`):
//!
//! ```markdown
//! | Exec | Audit | Criterion |
//! |------|-------|-----------|
//! | [ ] | [ ] | first criterion text |
//! | [x] | [ ] | second criterion text |
//! ```
//!
//! - **Exec** is toggled by the implementer as work lands.
//! - **Audit** is harness-owned: set `[x]` on verification Achieved, cleared
//!   on NotAchieved / fail-open.
//! - `update_goal(completed: true)` requires every Exec cell `[x]` before
//!   independent audit runs.
//! - Goal complete requires every Audit cell `[x]` after a successful audit.

use crate::session::goal_plan_md::{
    header_level, is_any_header, is_section_header, is_separator_row, table_cells,
};
use std::path::Path;

const SECTION: &str = "acceptance checklist";

/// One dual-mark row from the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DualRow {
    pub exec: bool,
    pub audit: bool,
    pub criterion: String,
}

/// Parse a table cell that holds a markdown checkbox.
fn parse_mark(cell: &str) -> Option<bool> {
    let t = cell.trim();
    match t {
        "[x]" | "[X]" => Some(true),
        "[ ]" => Some(false),
        _ => None,
    }
}

/// Parse dual-column rows from a full plan body.
pub(crate) fn parse_dual_rows(body: &str) -> Vec<DualRow> {
    let mut rows = Vec::new();
    let mut in_section = false;
    let mut section_level = 0usize;
    for line in body.lines() {
        if is_section_header(line, SECTION) {
            in_section = true;
            section_level = header_level(line);
            continue;
        }
        if in_section && is_any_header(line) && header_level(line) <= section_level {
            break;
        }
        if !in_section {
            continue;
        }
        let cells = table_cells(line);
        if cells.len() < 3 {
            continue;
        }
        if is_separator_row(&cells) {
            continue;
        }
        // Header row: Exec | Audit | Criterion
        if cells[0].eq_ignore_ascii_case("exec") && cells[1].eq_ignore_ascii_case("audit") {
            continue;
        }
        let Some(exec) = parse_mark(cells[0]) else {
            continue;
        };
        let Some(audit) = parse_mark(cells[1]) else {
            continue;
        };
        let criterion = cells[2..].join(" | ").trim().to_string();
        if criterion.is_empty() {
            continue;
        }
        rows.push(DualRow {
            exec,
            audit,
            criterion,
        });
    }
    rows
}

/// Whether the plan body contains a dual-column acceptance checklist section
/// with at least one data row.
pub(crate) fn has_dual_checklist(body: &str) -> bool {
    !parse_dual_rows(body).is_empty()
}

/// All Exec marks checked (and at least one row exists).
pub(crate) fn all_exec_checked(body: &str) -> bool {
    let rows = parse_dual_rows(body);
    !rows.is_empty() && rows.iter().all(|r| r.exec)
}

/// All Audit marks checked (and at least one row exists).
pub(crate) fn all_audit_checked(body: &str) -> bool {
    let rows = parse_dual_rows(body);
    !rows.is_empty() && rows.iter().all(|r| r.audit)
}

/// First criterion whose Exec mark is still unchecked.
pub(crate) fn first_unchecked_exec(body: &str) -> Option<String> {
    parse_dual_rows(body)
        .into_iter()
        .find(|r| !r.exec)
        .map(|r| r.criterion)
}

/// Read plan file (capped) and return first unchecked exec criterion.
pub(crate) fn first_unchecked_exec_from_path(path: &Path) -> Option<String> {
    let body = super::goal_next_step::read_capped(path)?;
    first_unchecked_exec(&body)
}

/// Gate for `update_goal(completed: true)`: every Exec mark must be checked.
///
/// Returns `Ok(())` when the dual checklist exists and all Exec cells are `[x]`.
/// Returns `Err(detail)` with a model-facing reason otherwise.
pub(crate) fn require_exec_complete(path: &Path) -> Result<(), String> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) => {
            return Err(format!(
                "Cannot read goal plan at {}: {e}. Finish planning before marking complete.",
                path.display()
            ));
        }
    };
    let rows = parse_dual_rows(&body);
    if rows.is_empty() {
        return Err(
            "Plan is missing a dual-column `## Acceptance checklist` (Exec | Audit | Criterion). \
             Re-run planning or add one row per acceptance criterion before \
             `update_goal(completed: true)`."
                .to_string(),
        );
    }
    if let Some(pending) = rows.iter().find(|r| !r.exec) {
        return Err(format!(
            "Execution column incomplete — check off every Exec mark in \
             `## Acceptance checklist` before requesting audit. First open: {}",
            pending.criterion
        ));
    }
    Ok(())
}

/// Rewrite every Audit mark in the dual checklist to `checked`.
pub(crate) fn set_all_audit_marks(path: &Path, checked: bool) -> std::io::Result<()> {
    crate::session::goal_plan_write::with_plan_lock(path, || {
        let body = std::fs::read_to_string(path)?;
        let updated = rewrite_audit_marks(&body, checked);
        if updated != body {
            std::fs::write(path, updated)?;
        }
        Ok(())
    })
}

/// Rewrite the Audit mark of only the listed criteria (1-based row numbers,
/// matching the numbered `## Acceptance criteria`), leaving every other row
/// untouched.
///
/// This is what makes a rejection local: a verdict that refutes criterion 2
/// returns only criterion 2 to execution, so work already accepted elsewhere
/// is not re-audited. Out-of-range numbers are ignored rather than an error —
/// they come from model output, and a bad index must not abort the rewrite of
/// the valid ones. Callers with no attribution at all must use
/// [`set_all_audit_marks`]; passing an empty slice here is a no-op, never a
/// silent full clear.
pub(crate) fn set_audit_marks_for_criteria(
    path: &Path,
    criteria: &[u32],
    checked: bool,
) -> std::io::Result<()> {
    if criteria.is_empty() {
        return Ok(());
    }
    crate::session::goal_plan_write::with_plan_lock(path, || {
        let body = std::fs::read_to_string(path)?;
        let updated = rewrite_audit_marks_for_criteria(&body, criteria, checked);
        if updated != body {
            std::fs::write(path, updated)?;
        }
        Ok(())
    })
}

/// Set the Exec mark of only the listed criteria.
///
/// The harness writes this column exactly once: when a criterion implemented by
/// its own worker has been **merged into the repo**. Everywhere else the Exec
/// column belongs to the implementer, which is why this takes a criterion list
/// and never a "set all" — the harness has no basis to claim work it did not
/// land.
pub(crate) fn set_exec_marks_for_criteria(
    path: &Path,
    criteria: &[u32],
    checked: bool,
) -> std::io::Result<()> {
    if criteria.is_empty() {
        return Ok(());
    }
    crate::session::goal_plan_write::with_plan_lock(path, || {
        let body = std::fs::read_to_string(path)?;
        let updated = rewrite_marks_where(&body, Column::Exec, checked, |row| {
            criteria.contains(&row)
        });
        if updated != body {
            std::fs::write(path, updated)?;
        }
        Ok(())
    })
}

fn mark_glyph(checked: bool) -> &'static str {
    if checked { "[x]" } else { "[ ]" }
}

/// Which checklist column a rewrite targets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Exec,
    Audit,
}

/// Rewrite Audit column cells inside the Acceptance checklist section.
pub(crate) fn rewrite_audit_marks(body: &str, checked: bool) -> String {
    rewrite_audit_marks_where(body, checked, |_| true)
}

/// Rewrite the Audit cells of the criteria whose 1-based row numbers appear in
/// `criteria`. Row numbers follow checklist order, which is the same order the
/// numbered `## Acceptance criteria` list uses, so a skeptic's `criterion: 2`
/// lands on the second row.
pub(crate) fn rewrite_audit_marks_for_criteria(
    body: &str,
    criteria: &[u32],
    checked: bool,
) -> String {
    rewrite_audit_marks_where(body, checked, |row| criteria.contains(&row))
}

fn rewrite_audit_marks_where(
    body: &str,
    checked: bool,
    should_rewrite: impl FnMut(u32) -> bool,
) -> String {
    rewrite_marks_where(body, Column::Audit, checked, should_rewrite)
}

fn rewrite_marks_where(
    body: &str,
    column: Column,
    checked: bool,
    mut should_rewrite: impl FnMut(u32) -> bool,
) -> String {
    let glyph = mark_glyph(checked);
    let mut row_number = 0u32;
    let mut out = String::with_capacity(body.len() + 16);
    let mut in_section = false;
    let mut section_level = 0usize;
    for line in body.lines() {
        if is_section_header(line, SECTION) {
            in_section = true;
            section_level = header_level(line);
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_section && is_any_header(line) && header_level(line) <= section_level {
            in_section = false;
        }
        if in_section {
            let cells = table_cells(line);
            if cells.len() >= 3
                && !is_separator_row(&cells)
                && !(cells[0].eq_ignore_ascii_case("exec")
                    && cells[1].eq_ignore_ascii_case("audit"))
                && parse_mark(cells[0]).is_some()
                && parse_mark(cells[1]).is_some()
            {
                row_number += 1;
                if should_rewrite(row_number) {
                    let criterion = cells[2..].join(" | ");
                    let (exec, audit) = match column {
                        Column::Exec => (glyph, cells[1].trim()),
                        Column::Audit => (cells[0].trim(), glyph),
                    };
                    out.push_str(&format!("| {exec} | {audit} | {} |\n", criterion.trim()));
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    // Preserve lack of trailing newline only if original lacked one.
    if !body.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Extract numbered items under `## Acceptance criteria` (lines like `1. foo`).
pub(crate) fn extract_numbered_acceptance_criteria(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_section = false;
    let mut section_level = 0usize;
    for line in body.lines() {
        if is_section_header(line, "acceptance criteria") {
            in_section = true;
            section_level = header_level(line);
            continue;
        }
        if in_section && is_any_header(line) && header_level(line) <= section_level {
            break;
        }
        if !in_section {
            continue;
        }
        let t = line.trim();
        // `1. text` or `1) text`
        let rest = t
            .find('.')
            .or_else(|| t.find(')'))
            .and_then(|i| {
                let (num, rest) = t.split_at(i);
                if num.chars().all(|c| c.is_ascii_digit()) && !num.is_empty() {
                    Some(rest[1..].trim())
                } else {
                    None
                }
            });
        if let Some(text) = rest
            && !text.is_empty()
        {
            items.push(text.to_string());
        }
    }
    items
}

/// Ensure `## Acceptance checklist` exists with one dual-mark row per
/// numbered acceptance criterion. Idempotent when the section already has rows.
pub(crate) fn ensure_dual_checklist_section(body: &str) -> String {
    if has_dual_checklist(body) {
        return body.to_string();
    }
    let criteria = extract_numbered_acceptance_criteria(body);
    if criteria.is_empty() {
        return body.to_string();
    }
    let mut table = String::from(
        "\n## Acceptance checklist\n\n\
         | Exec | Audit | Criterion |\n\
         |------|-------|----------|\n",
    );
    for c in &criteria {
        let safe = c.replace('|', "\\|");
        table.push_str(&format!("| [ ] | [ ] | {safe} |\n"));
    }
    // Prefer insert before Verification plan / Non-goals / Task checklist.
    for marker in [
        "## Verification plan",
        "## Non-goals",
        "## Assumed scope",
        "## Implementation approach",
        "## Task checklist",
        "## Risks",
    ] {
        if let Some(idx) = body.find(marker) {
            let mut out = String::with_capacity(body.len() + table.len());
            out.push_str(&body[..idx]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&table);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&body[idx..]);
            return out;
        }
    }
    let mut out = body.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&table);
    out
}

/// Apply [`ensure_dual_checklist_section`] to the plan file on disk.
pub(crate) fn ensure_dual_checklist_on_disk(path: &Path) -> std::io::Result<bool> {
    crate::session::goal_plan_write::with_plan_lock(path, || {
        let body = std::fs::read_to_string(path)?;
        let updated = ensure_dual_checklist_section(&body);
        if updated != body {
            std::fs::write(path, updated)?;
            Ok(true)
        } else {
            Ok(false)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Plan: demo

## Acceptance criteria
1. first thing works
2. second thing works

## Acceptance checklist

| Exec | Audit | Criterion |
|------|-------|-----------|
| [ ] | [ ] | first thing works |
| [x] | [ ] | second thing works |

## Non-goals
- polish
"#;

    #[test]
    fn parses_dual_rows() {
        let rows = parse_dual_rows(SAMPLE);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].exec);
        assert!(!rows[0].audit);
        assert_eq!(rows[0].criterion, "first thing works");
        assert!(rows[1].exec);
        assert!(!rows[1].audit);
    }

    #[test]
    fn first_unchecked_exec_is_first_open_row() {
        assert_eq!(
            first_unchecked_exec(SAMPLE).as_deref(),
            Some("first thing works")
        );
        let all_exec = SAMPLE.replace("| [ ] | [ ] | first", "| [x] | [ ] | first");
        assert!(first_unchecked_exec(&all_exec).is_none());
        assert!(all_exec_checked(&all_exec));
    }

    #[test]
    fn rewrite_audit_marks_sets_both_rows() {
        let out = rewrite_audit_marks(SAMPLE, true);
        let rows = parse_dual_rows(&out);
        assert!(rows.iter().all(|r| r.audit));
        assert!(!rows[0].exec); // exec unchanged
        assert!(rows[1].exec);
        let cleared = rewrite_audit_marks(&out, false);
        assert!(parse_dual_rows(&cleared).iter().all(|r| !r.audit));
    }

    #[test]
    fn rewrite_audit_marks_for_criteria_clears_only_the_refuted_row() {
        let all_audited = rewrite_audit_marks(SAMPLE, true);
        let out = rewrite_audit_marks_for_criteria(&all_audited, &[2], false);
        let rows = parse_dual_rows(&out);
        // Criterion 1 keeps the audit credit it earned; only 2 goes back.
        assert!(rows[0].audit);
        assert!(!rows[1].audit);
        // Exec marks are untouched by an audit rewrite.
        assert!(!rows[0].exec);
        assert!(rows[1].exec);
    }

    #[test]
    fn rewrite_audit_marks_for_criteria_ignores_out_of_range_and_empty() {
        let all_audited = rewrite_audit_marks(SAMPLE, true);
        // A model-supplied row number past the end must not touch other rows
        // and must not panic.
        let out = rewrite_audit_marks_for_criteria(&all_audited, &[7], false);
        assert!(parse_dual_rows(&out).iter().all(|r| r.audit));
        // An empty list is a no-op, never a full clear.
        let empty = rewrite_audit_marks_for_criteria(&all_audited, &[], false);
        assert_eq!(empty, all_audited);
    }

    #[test]
    fn set_audit_marks_for_criteria_writes_only_the_named_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        std::fs::write(&path, rewrite_audit_marks(SAMPLE, true)).unwrap();
        set_audit_marks_for_criteria(&path, &[1], false).unwrap();
        let rows = parse_dual_rows(&std::fs::read_to_string(&path).unwrap());
        assert!(!rows[0].audit);
        assert!(rows[1].audit);
    }

    #[test]
    fn ensure_dual_from_numbered_criteria() {
        let bare = "# Plan\n\n## Acceptance criteria\n1. alpha\n2. beta\n\n## Non-goals\n- x\n";
        let out = ensure_dual_checklist_section(bare);
        assert!(has_dual_checklist(&out));
        let rows = parse_dual_rows(&out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].criterion, "alpha");
        assert_eq!(rows[1].criterion, "beta");
        // Idempotent.
        assert_eq!(ensure_dual_checklist_section(&out), out);
    }

    #[test]
    fn require_exec_complete_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        std::fs::write(&path, SAMPLE).unwrap();
        let err = require_exec_complete(&path).unwrap_err();
        assert!(err.contains("Execution column incomplete"));
        let done = SAMPLE
            .replace("| [ ] | [ ] | first", "| [x] | [ ] | first");
        std::fs::write(&path, done).unwrap();
        assert!(require_exec_complete(&path).is_ok());
    }
}
