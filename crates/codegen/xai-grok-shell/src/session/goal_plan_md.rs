//! Markdown primitives shared by every reader of the goal `plan.md`.
//!
//! The plan is a contract written by a model and read by several harness
//! components (acceptance checklist, next-step nudge, criterion graph). They
//! all need the same two operations — find a `## Section` and split a table
//! row — so those live here once instead of drifting per reader.

/// Whether `line` is a markdown header whose title matches `name`
/// (case-insensitive, any header level).
pub(crate) fn is_section_header(line: &str, name: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return false;
    }
    trimmed.trim_start_matches('#').trim().eq_ignore_ascii_case(name)
}

/// Number of leading `#` characters, i.e. the header depth.
pub(crate) fn header_level(line: &str) -> usize {
    line.trim_start().chars().take_while(|c| *c == '#').count()
}

pub(crate) fn is_any_header(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// Split a markdown table row into trimmed cells, dropping the empty strings
/// the leading and trailing pipes produce. Non-table lines yield no cells.
pub(crate) fn table_cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return Vec::new();
    }
    trimmed.trim_matches('|').split('|').map(str::trim).collect()
}

/// True when the row is a separator like `|---|:---:|---|`.
pub(crate) fn is_separator_row(cells: &[&str]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty() && s.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
        })
}

/// Lines belonging to the first `name` section, ending at the next header of
/// the same or shallower level. The header line itself is not included.
///
/// Only the FIRST matching section is returned: a plan that somehow carries
/// two `## Criterion dependencies` sections must not have its rows silently
/// concatenated into one oversized graph.
pub(crate) fn section_lines<'a>(body: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut level = None;
    for line in body.lines() {
        match level {
            None => {
                if is_section_header(line, name) {
                    level = Some(header_level(line));
                }
            }
            Some(open) => {
                if is_any_header(line) && header_level(line) <= open {
                    break;
                }
                out.push(line);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_header_matches_any_level_and_ignores_case() {
        assert!(is_section_header("## Acceptance criteria", "acceptance criteria"));
        assert!(is_section_header("#### ACCEPTANCE CRITERIA", "acceptance criteria"));
        assert!(!is_section_header("Acceptance criteria", "acceptance criteria"));
        assert!(!is_section_header("## Acceptance criteria extra", "acceptance criteria"));
    }

    #[test]
    fn table_cells_drops_pipe_padding_and_skips_prose() {
        assert_eq!(table_cells("| a | b |  c |"), vec!["a", "b", "c"]);
        assert!(table_cells("not a table").is_empty());
        assert!(is_separator_row(&table_cells("|---|:--:|---|")));
        assert!(!is_separator_row(&table_cells("| a | b |")));
    }

    #[test]
    fn section_lines_stops_at_same_or_shallower_header() {
        let body = "# Plan\n\n## A\nfirst\n### Deeper\nkept\n\n## B\nnope\n";
        assert_eq!(
            section_lines(body, "a"),
            vec!["first", "### Deeper", "kept", ""]
        );
        assert_eq!(section_lines(body, "b"), vec!["nope"]);
        assert!(section_lines(body, "missing").is_empty());
    }

    #[test]
    fn section_lines_returns_only_the_first_match() {
        let body = "## A\none\n\n## A\ntwo\n";
        assert_eq!(section_lines(body, "a"), vec!["one", ""]);
    }
}
