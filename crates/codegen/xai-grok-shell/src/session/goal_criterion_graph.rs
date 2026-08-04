//! Static dependency + write-scope contract over the acceptance criteria.
//!
//! Format (under `## Criterion dependencies` on the goal `plan.md`):
//!
//! ```markdown
//! | # | Depends on | Write scope |
//! |---|------------|-------------|
//! | 1 | -          | src/parse/** |
//! | 2 | 1          | src/cli.rs   |
//! ```
//!
//! Two criteria may run at the same time only when neither depends on the
//! other AND their declared write scopes are disjoint. That makes this table
//! the input to fan-out: without it every criterion must run serially, because
//! two implementers editing the same file concurrently silently lose work.
//!
//! Everything here is a pure function over the plan text. The scheduling
//! decision lives with the caller; this module only says what the contract
//! permits.

use crate::session::goal_plan_md::{is_separator_row, section_lines, table_cells};

pub(crate) const SECTION: &str = "criterion dependencies";

/// One criterion's declared position in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CriterionNode {
    /// 1-based index into `## Acceptance criteria`.
    pub number: u32,
    /// Criteria that must be accepted before this one may start.
    pub depends_on: Vec<u32>,
    /// Paths/globs this criterion is allowed to write. Empty means
    /// "undeclared", which is treated as conflicting with everything.
    pub write_scope: Vec<String>,
}

/// The parsed contract for one goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CriterionGraph {
    pub nodes: Vec<CriterionNode>,
}

/// A reason the declared graph cannot be scheduled as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContractViolation {
    /// The table does not have exactly one row per acceptance criterion.
    RowCount { rows: usize, criteria: usize },
    /// A row's `#` is not the 1-based criterion index expected at that position.
    RowNumber { position: u32, found: u32 },
    /// `depends_on` names a criterion that does not exist.
    UnknownDependency { criterion: u32, depends_on: u32 },
    /// A criterion depends on itself.
    SelfDependency { criterion: u32 },
    /// Dependencies form a cycle, so no criterion in it can ever start.
    Cycle { members: Vec<u32> },
    /// Two independent criteria claim overlapping write scope.
    ScopeConflict { a: u32, b: u32, scope: String },
}

impl ContractViolation {
    /// One-line prose for the pause message the user reads.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::RowCount { rows, criteria } => format!(
                "`## {SECTION}` has {rows} row(s) but there are {criteria} acceptance criteria — \
                 write exactly one row per criterion"
            ),
            Self::RowNumber { position, found } => format!(
                "`## {SECTION}` row {position} is numbered {found} — rows must be numbered \
                 1..N in criterion order"
            ),
            Self::UnknownDependency {
                criterion,
                depends_on,
            } => format!(
                "criterion {criterion} depends on {depends_on}, which is not an acceptance criterion"
            ),
            Self::SelfDependency { criterion } => {
                format!("criterion {criterion} depends on itself")
            }
            Self::Cycle { members } => format!(
                "criteria {} form a dependency cycle and can never start",
                members
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::ScopeConflict { a, b, scope } => format!(
                "criteria {a} and {b} both write `{scope}` but neither depends on the other — \
                 give them disjoint write scopes or an explicit dependency"
            ),
        }
    }

    /// Whether the graph is unschedulable *as declared* with no safe reading.
    ///
    /// A [`Self::ScopeConflict`] has one: run the pair serially. Every other
    /// violation is a malformed contract the harness cannot repair by
    /// ordering, because it cannot know what the planner meant.
    pub(crate) fn is_structural(&self) -> bool {
        !matches!(self, Self::ScopeConflict { .. })
    }
}

impl CriterionGraph {
    /// The fully serial graph: criterion `i` depends on `i - 1`, no declared
    /// scopes. This is the fallback whenever a plan carries no dependency
    /// section, and it reproduces today's one-criterion-at-a-time behavior
    /// exactly — an absent contract must never be read as "safe to fan out".
    pub(crate) fn serial(criteria: usize) -> Self {
        Self {
            nodes: (1..=criteria as u32)
                .map(|number| CriterionNode {
                    number,
                    depends_on: if number > 1 { vec![number - 1] } else { Vec::new() },
                    write_scope: Vec::new(),
                })
                .collect(),
        }
    }

    /// Criteria grouped into waves: every member of a wave may run in
    /// parallel, and a wave starts only once all earlier waves are accepted.
    ///
    /// Returns `None` when the dependencies contain a cycle, since no layering
    /// exists. Callers should [`Self::validate`] first; this is the scheduling
    /// view, not the diagnostic one.
    pub(crate) fn parallel_waves(&self) -> Option<Vec<Vec<u32>>> {
        let mut remaining: Vec<&CriterionNode> = self.nodes.iter().collect();
        let mut done: Vec<u32> = Vec::new();
        let mut waves: Vec<Vec<u32>> = Vec::new();
        while !remaining.is_empty() {
            let (ready, blocked): (Vec<_>, Vec<_>) = remaining
                .into_iter()
                .partition(|n| n.depends_on.iter().all(|d| done.contains(d)));
            if ready.is_empty() {
                return None; // cycle
            }
            let wave: Vec<u32> = ready.iter().map(|n| n.number).collect();
            done.extend(wave.iter().copied());
            waves.push(wave);
            remaining = blocked;
        }
        Some(waves)
    }

    /// Whether `a` and `b` are ordered by dependencies, in either direction,
    /// transitively. Unordered pairs are the ones that can race.
    fn ordered(&self, a: u32, b: u32) -> bool {
        self.reaches(a, b) || self.reaches(b, a)
    }

    /// Whether `from` transitively depends on `to`.
    fn reaches(&self, from: u32, to: u32) -> bool {
        let mut stack = vec![from];
        let mut seen: Vec<u32> = Vec::new();
        while let Some(cur) = stack.pop() {
            if seen.contains(&cur) {
                continue;
            }
            seen.push(cur);
            let Some(node) = self.nodes.iter().find(|n| n.number == cur) else {
                continue;
            };
            for &dep in &node.depends_on {
                if dep == to {
                    return true;
                }
                stack.push(dep);
            }
        }
        false
    }

    /// Every way the declared graph fails to be a schedulable contract over
    /// `criteria` acceptance criteria. An empty vec means it is safe to
    /// schedule by [`Self::parallel_waves`].
    pub(crate) fn validate(&self, criteria: usize) -> Vec<ContractViolation> {
        let mut out = Vec::new();
        if self.nodes.len() != criteria {
            out.push(ContractViolation::RowCount {
                rows: self.nodes.len(),
                criteria,
            });
        }
        for (idx, node) in self.nodes.iter().enumerate() {
            let position = idx as u32 + 1;
            if node.number != position {
                out.push(ContractViolation::RowNumber {
                    position,
                    found: node.number,
                });
            }
            for &dep in &node.depends_on {
                if dep == node.number {
                    out.push(ContractViolation::SelfDependency {
                        criterion: node.number,
                    });
                } else if !self.nodes.iter().any(|n| n.number == dep) {
                    out.push(ContractViolation::UnknownDependency {
                        criterion: node.number,
                        depends_on: dep,
                    });
                }
            }
        }
        // Numbering must be sound before reachability means anything: a graph
        // with duplicate or out-of-range numbers would make `reaches` answer
        // about the wrong nodes, so report only what is already known.
        if !out.is_empty() {
            return out;
        }
        if self.parallel_waves().is_none() {
            out.push(ContractViolation::Cycle {
                members: self.cycle_members(),
            });
            return out;
        }
        for (i, a) in self.nodes.iter().enumerate() {
            for b in self.nodes.iter().skip(i + 1) {
                if self.ordered(a.number, b.number) {
                    continue;
                }
                if let Some(scope) = overlapping_scope(&a.write_scope, &b.write_scope) {
                    out.push(ContractViolation::ScopeConflict {
                        a: a.number,
                        b: b.number,
                        scope,
                    });
                }
            }
        }
        out
    }

    /// Criteria that can never start, i.e. those left over once every
    /// schedulable node is peeled off. Reported as the cycle for diagnostics.
    fn cycle_members(&self) -> Vec<u32> {
        let mut done: Vec<u32> = Vec::new();
        loop {
            let ready: Vec<u32> = self
                .nodes
                .iter()
                .filter(|n| !done.contains(&n.number))
                .filter(|n| n.depends_on.iter().all(|d| done.contains(d)))
                .map(|n| n.number)
                .collect();
            if ready.is_empty() {
                break;
            }
            done.extend(ready);
        }
        self.nodes
            .iter()
            .map(|n| n.number)
            .filter(|n| !done.contains(n))
            .collect()
    }

    /// Add the implicit ordering a [`ContractViolation::ScopeConflict`]
    /// implies: the later criterion waits for the earlier one. This trades
    /// parallelism for safety and is what lets a sloppy contract keep running
    /// instead of pausing the goal.
    pub(crate) fn serialize_conflicts(&mut self, criteria: usize) {
        // Re-validating after each edit keeps this honest: adding an edge can
        // resolve several conflicts at once, and must not add an edge that
        // would create a cycle (impossible here, since edges always point
        // from the higher number to the lower one).
        loop {
            let conflicts: Vec<(u32, u32)> = self
                .validate(criteria)
                .into_iter()
                .filter_map(|v| match v {
                    ContractViolation::ScopeConflict { a, b, .. } => Some((a, b)),
                    _ => None,
                })
                .collect();
            let Some(&(a, b)) = conflicts.first() else {
                return;
            };
            let (earlier, later) = if a < b { (a, b) } else { (b, a) };
            let Some(node) = self.nodes.iter_mut().find(|n| n.number == later) else {
                return;
            };
            if node.depends_on.contains(&earlier) {
                return; // no progress possible; stop rather than spin
            }
            node.depends_on.push(earlier);
        }
    }
}

/// Parse the `## Criterion dependencies` table, or `None` when the section is
/// absent or has no data rows. `None` is not an error — callers fall back to
/// [`CriterionGraph::serial`].
pub(crate) fn parse_criterion_graph(body: &str) -> Option<CriterionGraph> {
    let mut nodes = Vec::new();
    for line in section_lines(body, SECTION) {
        let cells = table_cells(line);
        if cells.len() < 3 || is_separator_row(&cells) {
            continue;
        }
        // Skips the header row `| # | Depends on | Write scope |`, whose first
        // cell carries no digits.
        let Some(number) = parse_number(cells[0]) else {
            continue;
        };
        nodes.push(CriterionNode {
            number,
            depends_on: parse_list(cells[1])
                .iter()
                .filter_map(|s| parse_number(s))
                .collect(),
            write_scope: parse_list(cells[2]),
        });
    }
    (!nodes.is_empty()).then_some(CriterionGraph { nodes })
}

/// The graph to schedule `criteria` acceptance criteria by, from a plan body.
///
/// Always returns something safe to run, because a plan can degrade after the
/// planner validated it (a user or implementer edits `plan.md` mid-goal) and a
/// scheduler must not have to choose between panicking and racing:
///
/// - no section, or a graph that is structurally unschedulable → fully serial,
///   i.e. exactly the pre-fan-out behavior;
/// - overlapping write scopes → the pair is ordered by criterion number, so
///   the conflict costs parallelism instead of correctness.
pub(crate) fn load_criterion_graph(body: &str, criteria: usize) -> CriterionGraph {
    let mut graph = match parse_criterion_graph(body) {
        Some(g) if g.validate(criteria).iter().all(|v| !v.is_structural()) => g,
        _ => CriterionGraph::serial(criteria),
    };
    graph.serialize_conflicts(criteria);
    graph
}

/// Expand a set of refuted criteria to include everything built on top of them.
///
/// The audit ratchet has to give ground here. Criterion 3 was verified against a
/// deliverable where criterion 1 held; if criterion 1 is refuted and 1's work is
/// redone, 3's verification was performed against a state that no longer exists.
/// Keeping 3's audit mark would hand out credit no skeptic gave for the code that
/// actually ships — and it fails silently, because the checklist looks complete.
///
/// Transitive, since a dependent's dependents are equally invalidated. The result
/// is sorted and deduplicated, and always contains the input.
pub(crate) fn with_dependents(
    criteria: &[crate::session::goal_tracker::CriterionView],
    refuted: &[u32],
) -> Vec<u32> {
    let mut invalid: Vec<u32> = refuted.to_vec();
    // Each pass can only add criteria, and there are finitely many, so this
    // terminates even if the dependency data contains a cycle (the harness
    // discards unschedulable graphs, but this must not depend on that).
    loop {
        let before = invalid.len();
        for c in criteria {
            if !invalid.contains(&c.number)
                && c.depends_on.iter().any(|d| invalid.contains(d))
            {
                invalid.push(c.number);
            }
        }
        if invalid.len() == before {
            break;
        }
    }
    invalid.sort_unstable();
    invalid.dedup();
    invalid
}

/// Project the plan into the per-criterion rows the UI renders.
///
/// Joins the three things a reader needs to understand progress and can only
/// get from three different places: the dual checklist (what is done), the
/// dependency table (what waits for what), and run state (what was given up
/// on). Returns an empty vec for a plan with no acceptance checklist, which is
/// the honest answer — there is nothing to show yet.
pub(crate) fn build_criteria_view(
    body: &str,
    deferred: &[crate::session::goal_tracker::DeferredCriterion],
) -> Vec<crate::session::goal_tracker::CriterionView> {
    let rows = crate::session::goal_acceptance_checklist::parse_dual_rows(body);
    if rows.is_empty() {
        return Vec::new();
    }
    let graph = load_criterion_graph(body, rows.len());
    // `None` waves mean the graph could not be scheduled, so the harness runs
    // serially — the UI says "unknown" rather than inventing a wave number that
    // would imply parallelism the run will not have.
    let waves = graph.parallel_waves();
    let wave_of = |n: u32| -> Option<u32> {
        waves.as_ref().and_then(|ws| {
            ws.iter()
                .position(|w| w.contains(&n))
                .map(|i| i as u32)
        })
    };
    // An unattributed deferral blocks the whole goal, so it marks every row:
    // showing some criteria as still live would promise work that will not run.
    let all_deferred = deferred.iter().any(|d| d.criterion.is_none());
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let number = i as u32 + 1;
            let node = graph.nodes.iter().find(|n| n.number == number);
            crate::session::goal_tracker::CriterionView {
                number,
                text: row.criterion.clone(),
                exec: row.exec,
                audit: row.audit,
                depends_on: node.map(|n| n.depends_on.clone()).unwrap_or_default(),
                write_scope: node.map(|n| n.write_scope.clone()).unwrap_or_default(),
                wave: wave_of(number),
                deferred: all_deferred
                    || deferred.iter().any(|d| d.criterion == Some(number)),
            }
        })
        .collect()
}

/// A criterion index from a cell like `2`, `#2`, or `criterion 2`.
pub(crate) fn parse_number(cell: &str) -> Option<u32> {
    let digits: String = cell
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|n| *n > 0)
}

/// Split a cell into entries on commas/semicolons/whitespace, dropping the
/// several ways a model writes "nothing here".
pub(crate) fn parse_list(cell: &str) -> Vec<String> {
    let t = cell.trim().trim_matches('`');
    if matches!(
        t.to_ascii_lowercase().as_str(),
        "" | "-" | "—" | "–" | "none" | "n/a" | "na" | "nothing" | "*none*"
    ) {
        return Vec::new();
    }
    t.split([',', ';'])
        .flat_map(str::split_whitespace)
        .map(|s| s.trim().trim_matches('`').trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The first scope entry that both sides could write, if any.
///
/// Deliberately conservative — an undeclared scope, or a pattern this cannot
/// reason about, counts as overlapping. A false overlap only costs
/// parallelism; a missed overlap costs a lost edit.
fn overlapping_scope(a: &[String], b: &[String]) -> Option<String> {
    if a.is_empty() {
        return Some("<undeclared>".to_string());
    }
    if b.is_empty() {
        return Some("<undeclared>".to_string());
    }
    for x in a {
        for y in b {
            if paths_overlap(x, y) {
                return Some(x.clone());
            }
        }
    }
    None
}

/// Whether two path patterns can name the same file.
pub(crate) fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_scope(a), normalize_scope(b));
    // Compare only the literal prefix before any wildcard: `src/a/**` and
    // `src/a/b.rs` overlap, and a pattern we cannot expand degrades to its
    // directory, which is the safe direction.
    let la = literal_prefix(&a);
    let lb = literal_prefix(&b);
    prefix_at_boundary(&la, &lb) || prefix_at_boundary(&lb, &la)
}

pub(crate) fn normalize_scope(s: &str) -> String {
    let s = s.trim().replace('\\', "/");
    let s = s.trim_start_matches("./").trim_start_matches('/');
    s.trim_end_matches('/').to_string()
}

/// The pattern up to the last separator before its first wildcard.
fn literal_prefix(s: &str) -> String {
    match s.find(['*', '?', '[']) {
        None => s.to_string(),
        Some(i) => {
            let head = &s[..i];
            match head.rfind('/') {
                Some(j) => head[..j].to_string(),
                None => String::new(),
            }
        }
    }
}

/// Whether `parent` contains `child`, comparing whole path segments so that
/// `src/app` does not appear to contain `src/appendix.rs`.
fn prefix_at_boundary(parent: &str, child: &str) -> bool {
    // An empty literal prefix came from a leading wildcard (`**/x.rs`), which
    // can match anywhere in the tree.
    if parent.is_empty() {
        return true;
    }
    if parent == child {
        return true;
    }
    child
        .strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod ratchet_tests {
    use super::*;
    use crate::session::goal_tracker::CriterionView;

    fn view(number: u32, depends_on: Vec<u32>) -> CriterionView {
        CriterionView {
            number,
            text: String::new(),
            exec: true,
            audit: true,
            depends_on,
            write_scope: Vec::new(),
            wave: None,
            deferred: false,
        }
    }

    #[test]
    fn refuting_a_criterion_invalidates_the_chain_built_on_it() {
        // 1 ← 2 ← 3, and 4 stands alone.
        let criteria = vec![
            view(1, vec![]),
            view(2, vec![1]),
            view(3, vec![2]),
            view(4, vec![]),
        ];
        assert_eq!(
            with_dependents(&criteria, &[1]),
            vec![1, 2, 3],
            "3 was verified against a deliverable where 1 held; redoing 1 \
             invalidates it transitively"
        );
        assert_eq!(
            with_dependents(&criteria, &[4]),
            vec![4],
            "an independent criterion must not drag anything else back to execution"
        );
    }

    #[test]
    fn a_criterion_with_several_dependencies_falls_with_any_of_them() {
        let criteria = vec![view(1, vec![]), view(2, vec![]), view(3, vec![1, 2])];
        assert_eq!(with_dependents(&criteria, &[2]), vec![2, 3]);
    }

    #[test]
    fn a_dependency_cycle_cannot_hang_the_expansion() {
        // The harness discards unschedulable graphs, but this must terminate
        // regardless of what the plan declared.
        let criteria = vec![view(1, vec![2]), view(2, vec![1])];
        assert_eq!(with_dependents(&criteria, &[1]), vec![1, 2]);
    }

    #[test]
    fn no_criteria_returns_the_input_unchanged() {
        assert_eq!(with_dependents(&[], &[2, 1]), vec![1, 2]);
    }
}

#[cfg(test)]
mod criteria_view_tests {
    use super::*;
    use crate::session::goal_tracker::DeferredCriterion;

    const PLAN: &str = "\
# Plan

## Acceptance criteria
1. first
2. second

## Acceptance checklist
| Exec | Audit | Criterion |
|------|-------|-----------|
| [x] | [x] | first |
| [x] | [ ] | second |

## Criterion dependencies
| # | Depends on | Write scope |
|---|------------|-------------|
| 1 | - | src/a.rs |
| 2 | 1 | src/b.rs |
";

    #[test]
    fn joins_checklist_progress_dependencies_and_scopes() {
        let view = build_criteria_view(PLAN, &[]);
        assert_eq!(view.len(), 2);
        assert!(view[0].audit && view[0].exec);
        assert!(view[1].exec && !view[1].audit);
        assert_eq!(view[1].depends_on, vec![1]);
        assert_eq!(view[1].write_scope, vec!["src/b.rs".to_string()]);
        assert_eq!(
            (view[0].wave, view[1].wave),
            (Some(0), Some(1)),
            "a declared dependency puts the dependent in a later wave"
        );
    }

    #[test]
    fn an_unattributed_deferral_marks_every_criterion() {
        let deferred = vec![DeferredCriterion {
            criterion: None,
            reason: "environment".into(),
        }];
        let view = build_criteria_view(PLAN, &deferred);
        assert!(
            view.iter().all(|c| c.deferred),
            "a goal-wide blocker stops everything; showing some rows as live \
             would promise work that will not run"
        );
    }

    #[test]
    fn a_scoped_deferral_marks_only_that_criterion() {
        let deferred = vec![DeferredCriterion {
            criterion: Some(2),
            reason: "kept failing".into(),
        }];
        let view = build_criteria_view(PLAN, &deferred);
        assert_eq!(
            view.iter().map(|c| c.deferred).collect::<Vec<_>>(),
            vec![false, true]
        );
    }

    #[test]
    fn a_plan_with_no_checklist_projects_nothing() {
        assert!(build_criteria_view("# Plan\n\n## Acceptance criteria\n1. x\n", &[]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
# Plan

## Criterion dependencies

| # | Depends on | Write scope |
|---|------------|-------------|
| 1 | -          | src/parse/** |
| 2 | 1          | src/cli.rs |
| 3 | 1          | docs/guide.md |

## Non-goals
- x
";

    fn node(number: u32, depends_on: &[u32], write_scope: &[&str]) -> CriterionNode {
        CriterionNode {
            number,
            depends_on: depends_on.to_vec(),
            write_scope: write_scope.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_rows_and_ignores_header_and_separator() {
        let g = parse_criterion_graph(TABLE).expect("table must parse");
        assert_eq!(
            g.nodes,
            vec![
                node(1, &[], &["src/parse/**"]),
                node(2, &[1], &["src/cli.rs"]),
                node(3, &[1], &["docs/guide.md"]),
            ]
        );
        assert!(g.validate(3).is_empty(), "{:?}", g.validate(3));
    }

    #[test]
    fn absent_section_parses_as_none_not_an_empty_graph() {
        // `None` must be distinguishable from "declared zero criteria": the
        // caller turns it into a serial graph, and an empty graph would look
        // like a goal with no criteria at all.
        assert!(parse_criterion_graph("# Plan\n\n## Non-goals\n- x\n").is_none());
        let header_only =
            "## Criterion dependencies\n\n| # | Depends on | Write scope |\n|---|---|---|\n";
        assert!(parse_criterion_graph(header_only).is_none());
    }

    #[test]
    fn parses_the_shapes_models_actually_write() {
        let body = "## Criterion dependencies\n\
            | # | Depends on | Write scope |\n\
            |---|---|---|\n\
            | 1 | none | `src/a.rs`, `src/b.rs` |\n\
            | #2 | criterion 1; 3 | src/c.rs |\n\
            | 3 | N/A | src/d.rs |\n";
        let g = parse_criterion_graph(body).expect("must parse");
        assert_eq!(g.nodes[0].write_scope, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(g.nodes[1].number, 2);
        assert_eq!(g.nodes[1].depends_on, vec![1, 3]);
        assert!(g.nodes[2].depends_on.is_empty());
    }

    #[test]
    fn serial_graph_is_a_single_chain_with_one_criterion_per_wave() {
        let g = CriterionGraph::serial(3);
        assert_eq!(g.nodes[0].depends_on, Vec::<u32>::new());
        assert_eq!(g.nodes[2].depends_on, vec![2]);
        assert!(g.validate(3).is_empty());
        assert_eq!(
            g.parallel_waves(),
            Some(vec![vec![1], vec![2], vec![3]]),
            "an absent contract must not permit any fan-out"
        );
        assert_eq!(CriterionGraph::serial(0).parallel_waves(), Some(vec![]));
    }

    #[test]
    fn independent_criteria_share_a_wave() {
        let g = parse_criterion_graph(TABLE).unwrap();
        assert_eq!(g.parallel_waves(), Some(vec![vec![1], vec![2, 3]]));
    }

    #[test]
    fn cycle_is_reported_and_has_no_waves() {
        let g = CriterionGraph {
            nodes: vec![node(1, &[2], &["a.rs"]), node(2, &[1], &["b.rs"])],
        };
        assert_eq!(g.parallel_waves(), None);
        assert_eq!(
            g.validate(2),
            vec![ContractViolation::Cycle {
                members: vec![1, 2]
            }]
        );
    }

    #[test]
    fn unknown_self_and_miscount_are_structural() {
        let g = CriterionGraph {
            nodes: vec![node(1, &[9], &["a.rs"]), node(2, &[2], &["b.rs"])],
        };
        let v = g.validate(3);
        assert!(v.contains(&ContractViolation::RowCount {
            rows: 2,
            criteria: 3
        }));
        assert!(v.contains(&ContractViolation::UnknownDependency {
            criterion: 1,
            depends_on: 9
        }));
        assert!(v.contains(&ContractViolation::SelfDependency { criterion: 2 }));
        assert!(v.iter().all(ContractViolation::is_structural));
    }

    #[test]
    fn misnumbered_rows_are_reported_before_reachability_is_trusted() {
        let g = CriterionGraph {
            nodes: vec![node(1, &[], &["a.rs"]), node(3, &[], &["b.rs"])],
        };
        assert_eq!(
            g.validate(2),
            vec![ContractViolation::RowNumber {
                position: 2,
                found: 3
            }]
        );
    }

    #[test]
    fn overlapping_scope_without_an_edge_is_a_conflict() {
        let g = CriterionGraph {
            nodes: vec![
                node(1, &[], &["src/app/mod.rs"]),
                node(2, &[], &["src/app/**"]),
            ],
        };
        assert_eq!(
            g.validate(2),
            vec![ContractViolation::ScopeConflict {
                a: 1,
                b: 2,
                scope: "src/app/mod.rs".into()
            }]
        );
        assert!(!g.validate(2)[0].is_structural(), "serializing is a safe fix");
    }

    #[test]
    fn a_dependency_edge_licenses_a_shared_scope() {
        // Ordered criteria never run at the same time, so sharing a file is
        // fine — this is the normal "extend what criterion 1 built" shape.
        let g = CriterionGraph {
            nodes: vec![node(1, &[], &["src/a.rs"]), node(2, &[1], &["src/a.rs"])],
        };
        assert!(g.validate(2).is_empty());
    }

    #[test]
    fn transitive_ordering_also_licenses_a_shared_scope() {
        let g = CriterionGraph {
            nodes: vec![
                node(1, &[], &["src/a.rs"]),
                node(2, &[1], &["src/b.rs"]),
                node(3, &[2], &["src/a.rs"]),
            ],
        };
        assert!(g.validate(3).is_empty(), "{:?}", g.validate(3));
    }

    #[test]
    fn undeclared_scope_conflicts_with_everything() {
        let g = CriterionGraph {
            nodes: vec![node(1, &[], &[]), node(2, &[], &["src/b.rs"])],
        };
        assert_eq!(
            g.validate(2),
            vec![ContractViolation::ScopeConflict {
                a: 1,
                b: 2,
                scope: "<undeclared>".into()
            }]
        );
    }

    #[test]
    fn scope_comparison_respects_path_segments() {
        assert!(paths_overlap("src/app", "src/app/mod.rs"));
        assert!(paths_overlap("src/app/**", "src/app/deep/x.rs"));
        assert!(paths_overlap("./src/a.rs", "src\\a.rs"));
        assert!(paths_overlap("**/x.rs", "anywhere/y.rs"), "leading wildcard is unbounded");
        assert!(!paths_overlap("src/app", "src/appendix.rs"));
        assert!(!paths_overlap("src/a.rs", "src/b.rs"));
        assert!(!paths_overlap("src/a/**", "src/b/**"));
    }

    #[test]
    fn serialize_conflicts_orders_the_pair_and_clears_the_violation() {
        let mut g = CriterionGraph {
            nodes: vec![
                node(1, &[], &["src/app/mod.rs"]),
                node(2, &[], &["src/app/**"]),
                node(3, &[], &["docs/x.md"]),
            ],
        };
        g.serialize_conflicts(3);
        assert!(g.validate(3).is_empty(), "{:?}", g.validate(3));
        assert_eq!(g.nodes[1].depends_on, vec![1], "later waits for earlier");
        assert_eq!(
            g.parallel_waves(),
            Some(vec![vec![1, 3], vec![2]]),
            "the non-conflicting criterion keeps its parallelism"
        );
    }

    #[test]
    fn load_falls_back_to_serial_for_absent_and_broken_contracts() {
        // No section at all: the common case for every plan written before
        // this contract existed.
        assert_eq!(
            load_criterion_graph("# Plan\n", 2).parallel_waves(),
            Some(vec![vec![1], vec![2]])
        );
        // A cycle hand-edited into the plan after the planner passed. Running
        // serially is wrong about the planner's intent but cannot lose work;
        // honoring the cycle would deadlock.
        let cyclic = "## Criterion dependencies\n\
            | # | Depends on | Write scope |\n\
            |---|---|---|\n\
            | 1 | 2 | src/a.rs |\n\
            | 2 | 1 | src/b.rs |\n";
        assert_eq!(
            load_criterion_graph(cyclic, 2).parallel_waves(),
            Some(vec![vec![1], vec![2]])
        );
        // A row count that disagrees with the criteria: the graph cannot be
        // mapped onto the criteria, so it is not trusted at all.
        let short = "## Criterion dependencies\n\
            | # | Depends on | Write scope |\n\
            |---|---|---|\n\
            | 1 | - | src/a.rs |\n";
        assert_eq!(load_criterion_graph(short, 3).nodes.len(), 3);
    }

    #[test]
    fn load_returns_a_graph_that_always_validates_clean() {
        // The scheduler's contract: whatever the plan says, what it gets back
        // is schedulable. A regression here would surface as a race, not a
        // test failure, so assert it directly.
        for body in [
            "# Plan\n",
            TABLE,
            "## Criterion dependencies\n| # | Depends on | Write scope |\n|---|---|---|\n\
             | 1 | - | src/app/** |\n| 2 | - | src/app/mod.rs |\n",
            "## Criterion dependencies\n| # | Depends on | Write scope |\n|---|---|---|\n\
             | 1 | - | - |\n| 2 | - | - |\n| 3 | - | - |\n",
        ] {
            let criteria = 3;
            let g = load_criterion_graph(body, criteria);
            assert_eq!(g.nodes.len(), criteria, "body: {body}");
            assert!(g.validate(criteria).is_empty(), "body: {body}");
            assert!(g.parallel_waves().is_some(), "body: {body}");
        }
    }

    #[test]
    fn serialize_conflicts_collapses_an_all_undeclared_contract_to_serial() {
        // The worst realistic case: a planner that declares no scopes at all
        // must end up fully serial, i.e. exactly today's behavior.
        let mut g = CriterionGraph {
            nodes: vec![node(1, &[], &[]), node(2, &[], &[]), node(3, &[], &[])],
        };
        g.serialize_conflicts(3);
        assert!(g.validate(3).is_empty(), "{:?}", g.validate(3));
        assert_eq!(g.parallel_waves(), Some(vec![vec![1], vec![2], vec![3]]));
    }
}
