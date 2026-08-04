//! Append-only amendments to the goal `plan.md`.
//!
//! `plan.md` is frozen for a reason. It is what the skeptic panel judges
//! against, what `{PRIOR_GAPS}` anchors its "the bar does not rise between
//! rounds" promise to, and what the Exec/Audit checklist numbers its rows by.
//! A mid-run rewrite that renumbers or rewords a criterion invalidates all
//! three at once, and it does so silently: the checklist still looks complete,
//! but the `[x]` in row 2 was earned by a criterion that no longer exists.
//!
//! That is why the run cannot simply re-spawn the planner over the contract.
//! What it CAN do is amend the contract in the one direction that takes nothing
//! away:
//!
//! - **append** a criterion at `N+1` (existing rows keep their numbers, text,
//!   and marks);
//! - **add** a dependency edge (never remove one);
//! - **widen** a declared write scope (never narrow one).
//!
//! Every one of those makes the schedule strictly more constrained or the
//! contract strictly larger. No accepted criterion loses its audit mark, no
//! earned mark moves to a different criterion, and no pair of criteria that
//! used to be ordered becomes concurrent — which is the failure that costs a
//! lost edit rather than merely a slower run.
//!
//! The direction is enforced by the shape of [`PlanAmendment`], not by review:
//! a scope entry is a set of paths to UNION into the declared scope, so
//! "narrow this scope" cannot be expressed at all. What a producer cannot say,
//! it cannot get wrong.
//!
//! [`apply_amendment`] is the single choke point. It re-checks everything a
//! producer might have got wrong (unknown criterion numbers, an edge that would
//! close a cycle, a scope for a criterion that declared none) and drops those
//! changes individually rather than failing the whole amendment: this runs
//! unattended after a wave, and one bad row must not discard the repair for the
//! rest.

use crate::session::goal_acceptance_checklist::parse_dual_rows;
use crate::session::goal_criterion_graph::{
    CriterionGraph, CriterionNode, SECTION as DEPENDENCIES_SECTION, normalize_scope,
    parse_criterion_graph, parse_number, paths_overlap,
};
use crate::session::goal_plan_md::{
    header_level, is_any_header, is_section_header, is_separator_row, table_cells,
};
use crate::session::goal_planner::SpawnError;
use crate::session::goal_role_tools::RoleToolNames;
use std::path::Path;

const CRITERIA_SECTION: &str = "acceptance criteria";
const CHECKLIST_SECTION: &str = "acceptance checklist";

/// Most criteria one amendment may append.
///
/// An amendment fires from evidence gathered mid-run, and the producer that
/// appends criteria is a model. Without a cap, a planner that keeps finding
/// "one more thing" moves the finish line every round and the goal never
/// converges — the run would look busy forever. Three is enough to record a
/// missed integration step and small enough that a runaway producer is visible
/// in the rejection count instead of in the wall clock.
pub(crate) const MAX_APPENDED_CRITERIA: usize = 3;

/// Amendment-writer runs allowed per goal.
///
/// Two, not one, because the first run is often spent on a finding the panel
/// simply failed to cite and the amender correctly declines to act on; a goal
/// that hit its only run there would never get the append it needed. Two, not
/// more, because each run can add up to [`MAX_APPENDED_CRITERIA`] criteria that
/// must then pass audit on their own — an amender answering every rejection
/// with more contract would move the finish line faster than the implementer
/// can reach it.
pub(crate) const GOAL_AMENDER_MAX_RUNS: u32 = 2;

/// A criterion to append, with everything the scheduler needs to place it.
///
/// The scope and dependencies travel WITH the text for a scheduling reason. A
/// criterion appended without a declared scope reads as "may write anything",
/// which conflicts with every other criterion and forces it into a wave of its
/// own at the end of the goal. That is the correct conservative reading of an
/// undeclared scope, but it is the wrong answer for a criterion someone is
/// deliberately adding — so the producer is required to say where it writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NewCriterion {
    pub text: String,
    /// Paths this criterion may write. Empty is accepted and means the same
    /// thing it means everywhere else: serial against everything.
    pub write_scope: Vec<String>,
    /// Existing criteria that must be claimed first. Validated like any other
    /// edge — an unknown number or a cycle is dropped, not written.
    pub depends_on: Vec<u32>,
}

/// A change set that can only add to the contract.
///
/// Deliberately not "the new plan": a producer states what it wants ADDED, and
/// there is no field through which it could remove or rewrite anything. A
/// producer that works from a whole proposed plan (rather than from evidence)
/// is expected to diff its proposal into this shape and discard the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlanAmendment {
    /// Criteria to append after the existing ones, in order. They take the next
    /// free numbers and start with both marks unticked.
    pub appended: Vec<NewCriterion>,
    /// `(dependent, dependency)` edges to add to `## Criterion dependencies`.
    pub edges: Vec<(u32, u32)>,
    /// `(criterion, paths)` to union into a declared write scope.
    pub scopes: Vec<(u32, Vec<String>)>,
}

impl PlanAmendment {
    pub(crate) fn is_empty(&self) -> bool {
        self.appended.is_empty() && self.edges.is_empty() && self.scopes.is_empty()
    }
}

/// Why one requested change was not applied. Recorded per change so a producer
/// that is systematically wrong is diagnosable from telemetry rather than from
/// a plan that quietly did not move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The criterion number does not exist in the amended plan.
    UnknownCriterion { criterion: u32 },
    /// A criterion cannot depend on itself.
    SelfEdge { criterion: u32 },
    /// The edge is already declared; applying it would be a no-op.
    EdgeAlreadyDeclared { criterion: u32, depends_on: u32 },
    /// The edge would close a dependency cycle, which the scheduler resolves by
    /// discarding the whole graph and running serially. Keeping the plan
    /// schedulable is worth more than recording the edge.
    EdgeWouldCycle { criterion: u32, depends_on: u32 },
    /// The criterion declared no write scope, which means "may write anything"
    /// and makes it serial against everything. Filling it in from what one
    /// worker happened to touch would REMOVE that constraint — the one
    /// direction an amendment must never move.
    UndeclaredScope { criterion: u32 },
    /// Every path in the entry is already covered by the declared scope.
    ScopeAlreadyCovered { criterion: u32 },
    /// The plan has no `## Criterion dependencies` table to amend.
    NoDependencyTable,
    /// The numbered criteria and the checklist rows disagree, so an appended
    /// criterion could not be given a number that means the same thing in both.
    /// Edges and scopes are unaffected and still apply.
    ChecklistMismatch { criteria: usize, rows: usize },
    /// The appended text repeats a criterion the plan already carries.
    DuplicateCriterion,
    /// [`MAX_APPENDED_CRITERIA`] was already reached by this amendment.
    AppendCapReached,
}

impl Rejection {
    /// Stable discriminator for telemetry.
    pub(crate) fn as_const_str(&self) -> &'static str {
        match self {
            Self::UnknownCriterion { .. } => "unknown_criterion",
            Self::SelfEdge { .. } => "self_edge",
            Self::EdgeAlreadyDeclared { .. } => "edge_already_declared",
            Self::EdgeWouldCycle { .. } => "edge_would_cycle",
            Self::UndeclaredScope { .. } => "undeclared_scope",
            Self::ScopeAlreadyCovered { .. } => "scope_already_covered",
            Self::NoDependencyTable => "no_dependency_table",
            Self::ChecklistMismatch { .. } => "checklist_mismatch",
            Self::DuplicateCriterion => "duplicate_criterion",
            Self::AppendCapReached => "append_cap_reached",
        }
    }
}

/// What an amendment actually changed, and what it refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AmendmentReport {
    /// Numbers assigned to the appended criteria.
    pub appended: Vec<u32>,
    /// Edges written to the dependency table.
    pub edges: Vec<(u32, u32)>,
    /// `(criterion, paths added)` for each widened scope.
    pub scopes: Vec<(u32, Vec<String>)>,
    pub rejected: Vec<Rejection>,
}

impl AmendmentReport {
    /// Whether anything reached the plan. A report with only rejections is the
    /// normal outcome of a wave whose scopes were already right.
    pub(crate) fn changed(&self) -> bool {
        !self.appended.is_empty() || !self.edges.is_empty() || !self.scopes.is_empty()
    }
}

/// Apply `amendment` to a plan body, dropping every change that would take
/// something away or leave the plan unschedulable.
///
/// Returns the new body and the report. The body is returned unchanged when
/// nothing survived the checks.
pub(crate) fn apply_amendment(body: &str, amendment: &PlanAmendment) -> (String, AmendmentReport) {
    let mut report = AmendmentReport::default();
    let rows = parse_dual_rows(body);
    let existing = rows.len() as u32;

    // The declared graph, NOT the scheduler's repaired one: `load_criterion_graph`
    // adds the edges that a scope overlap implies, and writing those back would
    // freeze a conservative fallback into the contract as if the planner had
    // meant it. An amendment records evidence, never the harness's own guess.
    let declared = parse_criterion_graph(body);

    let appended = plan_appends(body, &rows, &amendment.appended, &mut report);
    let total = existing + appended.len() as u32;

    // An appended criterion's own `depends_on` is just an edge whose dependent
    // happens to be new, so it goes through the same validation rather than a
    // parallel path that could accept a cycle the edge checker would refuse.
    let mut wanted_edges = amendment.edges.clone();
    for (i, c) in appended.iter().enumerate() {
        let number = existing + i as u32 + 1;
        for &d in &c.depends_on {
            wanted_edges.push((number, d));
        }
    }

    let (edges, scopes) = match declared.as_ref() {
        Some(graph) => {
            let edges = plan_edges(graph, total, &wanted_edges, &mut report);
            let scopes = plan_scopes(graph, &amendment.scopes, &mut report);
            (edges, scopes)
        }
        None => {
            if !wanted_edges.is_empty() || !amendment.scopes.is_empty() {
                report.rejected.push(Rejection::NoDependencyTable);
            }
            (Vec::new(), Vec::new())
        }
    };

    report.appended = (existing + 1..=existing + appended.len() as u32).collect();
    report.edges = edges.clone();
    report.scopes = scopes.clone();
    if !report.changed() {
        return (body.to_string(), report);
    }
    let out = rewrite(body, existing, &appended, &edges, &scopes);
    (out, report)
}

/// Apply an amendment to the plan file on disk, under the plan lock.
///
/// Takes the whole read-modify-write, like every other harness write to the
/// plan: a wave's workers finish at unpredictable times, and an amendment that
/// interleaved with an audit-mark rewrite would drop one of the two.
pub(crate) fn amend_plan_on_disk(
    path: &Path,
    amendment: &PlanAmendment,
) -> std::io::Result<AmendmentReport> {
    crate::session::goal_plan_write::with_plan_lock(path, || {
        let body = std::fs::read_to_string(path)?;
        let (updated, report) = apply_amendment(&body, amendment);
        if updated != body {
            std::fs::write(path, updated)?;
        }
        Ok(report)
    })
}

/// The amendment a finished wave justifies: record what each worker actually
/// wrote.
///
/// A merge conflict is the contract being wrong out loud — two criteria the
/// plan said were independent turned out to share a file. The honest repair is
/// not to guess a new decomposition but to write down the observed fact: this
/// criterion writes these paths. Once both sides declare the shared path, the
/// scheduler's own scope check orders them on the next round, so the collision
/// cannot repeat.
///
/// Only paths outside the declared scope are proposed; a criterion that stayed
/// inside its scope produces no amendment at all, which is the common case.
pub(crate) fn amendment_from_observed_writes(observed: &[(u32, Vec<String>)]) -> PlanAmendment {
    let mut scopes: Vec<(u32, Vec<String>)> = Vec::new();
    for (criterion, paths) in observed {
        let mut wanted: Vec<String> = Vec::new();
        for p in paths {
            let n = normalize_scope(p);
            if !n.is_empty() && !wanted.contains(&n) {
                wanted.push(n);
            }
        }
        if !wanted.is_empty() {
            scopes.push((*criterion, wanted));
        }
    }
    PlanAmendment {
        scopes,
        ..Default::default()
    }
}

// The producer that can append: the amendment writer.

const GOAL_AMENDER_SUBAGENT_TYPE: &str = crate::session::goal_planner::GOAL_ROLE_SUBAGENT_TYPE;

/// Description shown in the pager subagent strip and matched by the e2e
/// coordinator stub to tell amender spawns from the other roles.
pub(crate) const GOAL_AMENDER_SUBAGENT_DESCRIPTION: &str = "goal plan amender";

const GOAL_AMENDER_PROMPT_TEMPLATE: &str = include_str!("templates/goal_amender_prompt.md");

/// Cap on unattributed findings quoted into the amender prompt. The panel's
/// own per-refuter cap already bounds this; the second cap keeps a pathological
/// round from rendering a prompt out of proportion to the decision being made.
const MAX_QUOTED_FINDINGS: usize = 12;

/// Spawns the amendment writer. A trait so the runner can be tested without a
/// subagent coordinator, matching the planner and worker spawners.
#[allow(async_fn_in_trait)]
pub(crate) trait GoalAmenderSpawner {
    async fn spawn_amender(&self, id: &str, prompt: String) -> Result<String, SpawnError>;
}

pub(crate) struct GoalAmenderInputs<'a> {
    pub objective: &'a str,
    /// The current contract. The amender READS it (its numbers are what
    /// `depends_on` refers to) and never writes it — the only writer is
    /// [`apply_amendment`].
    pub plan_file: &'a Path,
    /// Where the amender writes its proposal JSON.
    pub amendment_file: &'a Path,
    /// The panel findings that matched no criterion — the entire reason to run.
    pub unattributed: &'a [String],
    pub tool_names: &'a RoleToolNames,
}

/// What one amendment-writer run concluded.
///
/// [`Nothing`](GoalAmenderOutcome::Nothing) and
/// [`FailedOpen`](GoalAmenderOutcome::FailedOpen) both leave the plan
/// untouched, but they are not the same fact and telemetry must not merge
/// them: the first says the contract was already complete, the second says
/// nobody checked.
#[derive(Debug)]
pub(crate) enum GoalAmenderOutcome {
    /// Criteria to append. Not yet checked against the plan — that is
    /// [`apply_amendment`]'s job.
    Proposed(PlanAmendment),
    /// The amender ran and found nothing the contract was missing.
    Nothing,
    /// The run could not be completed. Already logged.
    FailedOpen,
}

/// Ask a subagent what the contract is missing, and return only what it may
/// legally propose.
///
/// Fail-OPEN in every direction: a spawn error and a missing or unparseable
/// proposal all end as [`GoalAmenderOutcome::FailedOpen`], and the round then
/// proceeds exactly as it does today. That is the right default because this
/// producer is an improvement to the contract, not a gate on it — a goal must
/// never stall because the amender was unavailable.
pub(crate) async fn run_goal_amender<S: GoalAmenderSpawner + ?Sized>(
    spawner: &S,
    inputs: GoalAmenderInputs<'_>,
) -> GoalAmenderOutcome {
    if inputs.unattributed.is_empty() {
        return GoalAmenderOutcome::Nothing;
    }
    // A stale proposal from an earlier round must not be read back as this
    // round's answer, so the file is removed before the spawn rather than
    // trusted to be overwritten.
    let _ = std::fs::remove_file(inputs.amendment_file);
    if let Some(parent) = inputs.amendment_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let prompt = render_amender_prompt(&inputs);
    let spawn_id = uuid::Uuid::now_v7().to_string();
    let response = match spawner.spawn_amender(&spawn_id, prompt).await {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %e, "goal amender: spawn failed; failing open");
            return GoalAmenderOutcome::FailedOpen;
        }
    };
    // The terminal token is advisory: the proposal file is the artifact, and a
    // model that wrote good JSON but a chatty final message should not lose it.
    if !crate::session::goal_planner::parse_terminal_response(&response) {
        tracing::debug!("goal amender: non-`Done` terminal response; reading the file anyway");
    }

    let raw = match std::fs::read_to_string(inputs.amendment_file) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %inputs.amendment_file.display(),
                "goal amender: no proposal file; failing open",
            );
            return GoalAmenderOutcome::FailedOpen;
        }
    };
    match parse_amendment_proposal(&raw) {
        Some(a) if !a.appended.is_empty() => GoalAmenderOutcome::Proposed(a),
        Some(_) => GoalAmenderOutcome::Nothing,
        None => {
            tracing::warn!("goal amender: unparseable proposal; failing open");
            GoalAmenderOutcome::FailedOpen
        }
    }
}

fn render_amender_prompt(inputs: &GoalAmenderInputs<'_>) -> String {
    let with_paths = GOAL_AMENDER_PROMPT_TEMPLATE
        .replace("{PLAN_FILE}", &inputs.plan_file.to_string_lossy())
        .replace("{AMENDMENT_FILE}", &inputs.amendment_file.to_string_lossy());
    let mut prompt = inputs.tool_names.apply(&with_paths);
    prompt.push_str("\n\nOBJECTIVE:\n");
    prompt.push_str(inputs.objective);
    prompt.push_str("\n\nUNATTRIBUTED FINDINGS:\n");
    for f in inputs.unattributed.iter().take(MAX_QUOTED_FINDINGS) {
        prompt.push_str("- ");
        prompt.push_str(f.trim());
        prompt.push('\n');
    }
    prompt
}

/// The amender's proposal, as it arrives on disk.
#[derive(serde::Deserialize)]
struct AmendmentProposal {
    #[serde(default)]
    criteria: Vec<ProposedCriterion>,
}

#[derive(serde::Deserialize)]
struct ProposedCriterion {
    #[serde(default)]
    text: String,
    #[serde(default)]
    write_scope: Vec<String>,
    /// Untyped on purpose: a model that writes `["2"]` instead of `[2]` has
    /// still said something usable, and rejecting the whole proposal over the
    /// JSON type of one dependency would throw away the criteria with it.
    #[serde(default)]
    depends_on: Vec<serde_json::Value>,
}

/// Parse a proposal into an append-only amendment, tolerating the prose and
/// code fences a model tends to wrap JSON in.
fn parse_amendment_proposal(raw: &str) -> Option<PlanAmendment> {
    let proposal: AmendmentProposal = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => {
            let start = raw.find('{')?;
            let end = raw.rfind('}')?;
            serde_json::from_str(raw.get(start..=end)?).ok()?
        }
    };
    let appended: Vec<NewCriterion> = proposal
        .criteria
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| NewCriterion {
            text: c.text.trim().to_string(),
            write_scope: c.write_scope,
            depends_on: c.depends_on.iter().filter_map(lenient_number).collect(),
        })
        .collect();
    Some(PlanAmendment {
        appended,
        ..Default::default()
    })
}

/// A criterion number from a JSON number or a string carrying one.
fn lenient_number(v: &serde_json::Value) -> Option<u32> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(s) => parse_number(s),
        _ => None,
    }
}

/// Production spawner: one inherit-path spawn on the session model and harness.
///
/// No per-role model override, unlike the planner and skeptics. Those roles are
/// configurable because their JUDGEMENT is the product; the amender's output is
/// three fields that [`apply_amendment`] re-checks anyway, so a model dial here
/// would be configuration surface without a decision behind it.
pub(crate) struct ChannelAmenderSpawner {
    pub(crate) event_tx: tokio::sync::mpsc::UnboundedSender<
        xai_grok_tools::implementations::grok_build::task::types::SubagentEvent,
    >,
    pub(crate) parent_session_id: String,
    pub(crate) parent_prompt_id: Option<String>,
    pub(crate) cwd: Option<String>,
    /// Trace-artifact sink + resolved `task` tool name; `None` disables
    /// recording. See [`crate::session::goal_classifier::record_subagent_trace`].
    pub(crate) trace_sink: Option<(xai_chat_state::ChatStateHandle, String)>,
}

impl GoalAmenderSpawner for ChannelAmenderSpawner {
    async fn spawn_amender(&self, id: &str, prompt: String) -> Result<String, SpawnError> {
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentEvent, SubagentRequest, SubagentRuntimeOverrides,
        };
        let trace_prompt = self.trace_sink.as_ref().map(|_| prompt.clone());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: id.to_string(),
            prompt,
            description: GOAL_AMENDER_SUBAGENT_DESCRIPTION.to_string(),
            subagent_type: GOAL_AMENDER_SUBAGENT_TYPE.to_string(),
            parent_session_id: self.parent_session_id.clone(),
            parent_prompt_id: self.parent_prompt_id.clone(),
            resume_from: None,
            cwd: self.cwd.clone(),
            runtime_overrides: SubagentRuntimeOverrides::default(),
            run_in_background: false,
            // Harness-internal: never surface to the model's idle reminder.
            surface_completion: false,
            fork_context: false,
            result_tx,
        };
        if self
            .event_tx
            .send(SubagentEvent::Spawn(Box::new(request)))
            .is_err()
        {
            return Err(SpawnError::Transport(
                "subagent coordinator channel closed".to_string(),
            ));
        }
        let result = result_rx
            .await
            .map_err(|_| SpawnError::Transport("subagent result channel dropped".to_string()))?;
        let outcome = if result.success {
            Ok(result.output.to_string())
        } else {
            Err(SpawnError::Runtime {
                message: result.error.unwrap_or_else(|| "unknown error".to_string()),
                cancelled: result.cancelled,
            })
        };
        crate::session::goal_classifier::record_subagent_trace(
            self.trace_sink.as_ref(),
            id,
            GOAL_AMENDER_SUBAGENT_TYPE,
            GOAL_AMENDER_SUBAGENT_DESCRIPTION,
            trace_prompt.as_deref(),
            match &outcome {
                Ok(text) => text,
                Err(SpawnError::Runtime { message, .. }) => message,
                Err(SpawnError::Transport(detail)) => detail,
            },
        );
        outcome
    }
}

// Change planning — each returns only the changes that survived the checks.

fn plan_appends(
    body: &str,
    rows: &[crate::session::goal_acceptance_checklist::DualRow],
    wanted: &[NewCriterion],
    report: &mut AmendmentReport,
) -> Vec<NewCriterion> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let numbered =
        crate::session::goal_acceptance_checklist::extract_numbered_acceptance_criteria(body);
    // An appended criterion is only meaningful if its number means the same
    // thing in the numbered list, the checklist, and the dependency table. When
    // the first two already disagree, appending would deepen the confusion.
    if numbered.len() != rows.len() {
        report.rejected.push(Rejection::ChecklistMismatch {
            criteria: numbered.len(),
            rows: rows.len(),
        });
        return Vec::new();
    }
    let mut out: Vec<NewCriterion> = Vec::new();
    for candidate in wanted {
        let text = candidate.text.trim();
        if text.is_empty() {
            continue;
        }
        if out.len() >= MAX_APPENDED_CRITERIA {
            report.rejected.push(Rejection::AppendCapReached);
            break;
        }
        let dup = rows
            .iter()
            .any(|r| r.criterion.trim().eq_ignore_ascii_case(text))
            || out.iter().any(|c| c.text.eq_ignore_ascii_case(text));
        if dup {
            report.rejected.push(Rejection::DuplicateCriterion);
            continue;
        }
        let mut write_scope: Vec<String> = Vec::new();
        for p in &candidate.write_scope {
            let p = normalize_scope(p);
            if !p.is_empty() && !write_scope.contains(&p) {
                write_scope.push(p);
            }
        }
        out.push(NewCriterion {
            text: text.to_string(),
            write_scope,
            depends_on: candidate.depends_on.clone(),
        });
    }
    out
}

fn plan_edges(
    declared: &CriterionGraph,
    total: u32,
    wanted: &[(u32, u32)],
    report: &mut AmendmentReport,
) -> Vec<(u32, u32)> {
    let mut candidate = declared.clone();
    // Appended criteria are not in the declared table yet, but an edge may
    // reference them, so give the cycle check a node for each.
    for n in 1..=total {
        if !candidate.nodes.iter().any(|node| node.number == n) {
            candidate.nodes.push(CriterionNode {
                number: n,
                depends_on: Vec::new(),
                write_scope: Vec::new(),
            });
        }
    }
    candidate.nodes.sort_by_key(|n| n.number);
    let mut applied = Vec::new();
    for &(criterion, depends_on) in wanted {
        if criterion == depends_on {
            report.rejected.push(Rejection::SelfEdge { criterion });
            continue;
        }
        let unknown = [criterion, depends_on]
            .into_iter()
            .find(|n| *n == 0 || *n > total);
        if let Some(criterion) = unknown {
            report
                .rejected
                .push(Rejection::UnknownCriterion { criterion });
            continue;
        }
        let Some(node) = candidate.nodes.iter_mut().find(|n| n.number == criterion) else {
            report
                .rejected
                .push(Rejection::UnknownCriterion { criterion });
            continue;
        };
        if node.depends_on.contains(&depends_on) {
            report.rejected.push(Rejection::EdgeAlreadyDeclared {
                criterion,
                depends_on,
            });
            continue;
        }
        node.depends_on.push(depends_on);
        // Checked after each edge, not once at the end: two edges can be
        // individually fine and jointly cyclic, and the one to blame is the
        // second one.
        if candidate.parallel_waves().is_none() {
            let node = candidate
                .nodes
                .iter_mut()
                .find(|n| n.number == criterion)
                .expect("node found immediately above");
            node.depends_on.retain(|d| *d != depends_on);
            report.rejected.push(Rejection::EdgeWouldCycle {
                criterion,
                depends_on,
            });
            continue;
        }
        applied.push((criterion, depends_on));
    }
    applied
}

fn plan_scopes(
    declared: &CriterionGraph,
    wanted: &[(u32, Vec<String>)],
    report: &mut AmendmentReport,
) -> Vec<(u32, Vec<String>)> {
    let mut applied: Vec<(u32, Vec<String>)> = Vec::new();
    for (criterion, paths) in wanted {
        let Some(node) = declared.nodes.iter().find(|n| n.number == *criterion) else {
            report.rejected.push(Rejection::UnknownCriterion {
                criterion: *criterion,
            });
            continue;
        };
        if node.write_scope.is_empty() {
            report.rejected.push(Rejection::UndeclaredScope {
                criterion: *criterion,
            });
            continue;
        }
        let mut have = node.write_scope.clone();
        let mut added: Vec<String> = Vec::new();
        for p in paths {
            let p = normalize_scope(p);
            if p.is_empty() {
                continue;
            }
            // Conservative coverage test: an entry that could name this path
            // already constrains it. Erring towards "covered" only means the
            // scope stays as it was, which loses nothing.
            if have.iter().any(|s| paths_overlap(s, &p)) {
                continue;
            }
            have.push(p.clone());
            added.push(p);
        }
        if added.is_empty() {
            report.rejected.push(Rejection::ScopeAlreadyCovered {
                criterion: *criterion,
            });
            continue;
        }
        applied.push((*criterion, added));
    }
    applied
}

// Markdown surgery

/// Splice the surviving changes into the plan body.
///
/// Existing lines are copied verbatim except for the dependency rows whose
/// cells grow. Nothing in `## Acceptance checklist` is ever rewritten — the
/// Exec and Audit columns are the run's memory of what was earned, and an
/// amendment has no business touching them.
fn rewrite(
    body: &str,
    existing: u32,
    appended: &[NewCriterion],
    edges: &[(u32, u32)],
    scopes: &[(u32, Vec<String>)],
) -> String {
    let last_criterion =
        last_line_in_section(body, CRITERIA_SECTION, |l| numbered_item(l).is_some());
    let last_checklist = last_line_in_section(body, CHECKLIST_SECTION, is_checklist_row);
    let last_dependency = last_line_in_section(body, DEPENDENCIES_SECTION, is_dependency_row);

    let mut out = String::with_capacity(body.len() + 256);
    for (idx, line) in body.lines().enumerate() {
        let in_dependencies = last_dependency.is_some_and(|_| true) && is_dependency_row(line);
        if in_dependencies
            && let Some(number) = table_cells(line).first().and_then(|c| parse_number(c))
            && let Some(rewritten) = rewrite_dependency_row(line, number, edges, scopes)
        {
            out.push_str(&rewritten);
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
        if Some(idx) == last_criterion {
            for (i, c) in appended.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", existing as usize + i + 1, c.text));
            }
        }
        if Some(idx) == last_checklist {
            for c in appended {
                out.push_str(&format!("| [ ] | [ ] | {} |\n", escape_cell(&c.text)));
            }
        }
        if Some(idx) == last_dependency {
            for (i, c) in appended.iter().enumerate() {
                let number = existing + i as u32 + 1;
                // An appended criterion has no row to rewrite, so its edges and
                // its declared scope are written straight into the row that
                // creates it. Only edges that survived validation appear here.
                let deps: Vec<String> = edges
                    .iter()
                    .filter(|(dependent, _)| *dependent == number)
                    .map(|(_, d)| d.to_string())
                    .collect();
                out.push_str(&format!(
                    "| {number} | {} | {} |\n",
                    append_cell("-", &deps),
                    append_cell("-", &c.write_scope),
                ));
            }
        }
    }
    if !body.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Rewrite one dependency row's `Depends on` / `Write scope` cells, or `None`
/// when this row is not touched by the amendment.
fn rewrite_dependency_row(
    line: &str,
    number: u32,
    edges: &[(u32, u32)],
    scopes: &[(u32, Vec<String>)],
) -> Option<String> {
    let new_edges: Vec<u32> = edges
        .iter()
        .filter(|(c, _)| *c == number)
        .map(|(_, d)| *d)
        .collect();
    let new_scope: Vec<String> = scopes
        .iter()
        .filter(|(c, _)| *c == number)
        .flat_map(|(_, p)| p.clone())
        .collect();
    if new_edges.is_empty() && new_scope.is_empty() {
        return None;
    }
    let cells = table_cells(line);
    let depends = cells.get(1).copied().unwrap_or("-");
    let scope = cells.get(2).copied().unwrap_or("-");
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    Some(format!(
        "{indent}| {number} | {} | {} |",
        append_cell(
            depends,
            &new_edges.iter().map(u32::to_string).collect::<Vec<_>>()
        ),
        append_cell(scope, &new_scope),
    ))
}

/// Append entries to a comma-separated table cell, preserving what is there.
///
/// A cell holding one of the several ways a model writes "nothing" is replaced
/// outright; anything else is extended, because the existing text is the
/// planner's declaration and an amendment adds to it rather than restating it.
fn append_cell(current: &str, add: &[String]) -> String {
    if add.is_empty() {
        return current.trim().to_string();
    }
    let existing = crate::session::goal_criterion_graph::parse_list(current);
    let mut parts = existing;
    for a in add {
        if !parts.iter().any(|p| p == a) {
            parts.push(a.clone());
        }
    }
    if parts.is_empty() {
        return "-".to_string();
    }
    parts.join(", ")
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// Index of the last line inside `section` for which `pred` holds.
fn last_line_in_section(body: &str, section: &str, pred: impl Fn(&str) -> bool) -> Option<usize> {
    let mut level = None;
    let mut found = None;
    for (idx, line) in body.lines().enumerate() {
        match level {
            None => {
                if is_section_header(line, section) {
                    level = Some(header_level(line));
                }
            }
            Some(open) => {
                if is_any_header(line) && header_level(line) <= open {
                    break;
                }
                if pred(line) {
                    found = Some(idx);
                }
            }
        }
    }
    found
}

/// The text of a `1. item` / `1) item` line.
fn numbered_item(line: &str) -> Option<&str> {
    let t = line.trim();
    let i = t.find(['.', ')'])?;
    let (num, rest) = t.split_at(i);
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = rest[1..].trim();
    (!rest.is_empty()).then_some(rest)
}

fn is_checklist_row(line: &str) -> bool {
    let cells = table_cells(line);
    cells.len() >= 3
        && !is_separator_row(&cells)
        && matches!(cells[0].trim(), "[x]" | "[X]" | "[ ]")
        && matches!(cells[1].trim(), "[x]" | "[X]" | "[ ]")
}

fn is_dependency_row(line: &str) -> bool {
    let cells = table_cells(line);
    cells.len() >= 3 && !is_separator_row(&cells) && parse_number(cells[0]).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "\
# Plan: demo

## Acceptance criteria
1. first thing works
2. second thing works

## Acceptance checklist

| Exec | Audit | Criterion |
|------|-------|-----------|
| [x] | [x] | first thing works |
| [ ] | [ ] | second thing works |

## Criterion dependencies

| # | Depends on | Write scope |
|---|------------|-------------|
| 1 | - | src/a.rs |
| 2 | - | src/b.rs |

## Non-goals
- polish
";

    /// A criterion appended with no scope declared: serial against everything.
    fn unscoped(text: &str) -> NewCriterion {
        NewCriterion {
            text: text.to_string(),
            ..Default::default()
        }
    }

    fn scoped(text: &str, write_scope: &[&str], depends_on: &[u32]) -> NewCriterion {
        NewCriterion {
            text: text.to_string(),
            write_scope: write_scope.iter().map(|s| s.to_string()).collect(),
            depends_on: depends_on.to_vec(),
        }
    }

    fn scope_of(body: &str, number: u32) -> Vec<String> {
        parse_criterion_graph(body)
            .expect("plan must carry a dependency table")
            .nodes
            .into_iter()
            .find(|n| n.number == number)
            .expect("criterion must have a row")
            .write_scope
    }

    fn depends_of(body: &str, number: u32) -> Vec<u32> {
        parse_criterion_graph(body)
            .expect("plan must carry a dependency table")
            .nodes
            .into_iter()
            .find(|n| n.number == number)
            .expect("criterion must have a row")
            .depends_on
    }

    #[test]
    fn widening_a_scope_keeps_what_was_already_declared() {
        let amendment = PlanAmendment {
            scopes: vec![(2, vec!["src/shared.rs".into()])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(
            scope_of(&out, 2),
            vec!["src/b.rs".to_string(), "src/shared.rs".to_string()],
            "an amendment adds to the planner's declaration, never replaces it"
        );
        assert_eq!(report.scopes, vec![(2, vec!["src/shared.rs".to_string()])]);
        assert_eq!(scope_of(&out, 1), vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn earned_marks_and_criterion_text_survive_every_amendment() {
        // The whole point of append-only: whatever else changes, row 1 keeps
        // the audit mark it earned and row 2 keeps its number.
        let amendment = PlanAmendment {
            appended: vec![unscoped("third thing works")],
            edges: vec![(2, 1)],
            scopes: vec![(2, vec!["src/shared.rs".into()])],
        };
        let (out, _) = apply_amendment(PLAN, &amendment);
        let rows = parse_dual_rows(&out);
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].exec && rows[0].audit,
            "row 1 keeps its audit credit"
        );
        assert_eq!(rows[0].criterion, "first thing works");
        assert_eq!(rows[1].criterion, "second thing works");
        assert!(!rows[1].exec && !rows[1].audit);
        assert_eq!(rows[2].criterion, "third thing works");
        assert!(
            !rows[2].exec && !rows[2].audit,
            "an appended criterion starts unearned"
        );
    }

    #[test]
    fn an_appended_criterion_lands_in_all_three_places() {
        let amendment = PlanAmendment {
            appended: vec![unscoped("third thing works")],
            ..Default::default()
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(report.appended, vec![3]);
        assert!(
            out.contains("3. third thing works"),
            "numbered list:\n{out}"
        );
        assert!(
            out.contains("| [ ] | [ ] | third thing works |"),
            "checklist:\n{out}"
        );
        assert_eq!(
            depends_of(&out, 3),
            Vec::<u32>::new(),
            "a new criterion gets a dependency row so the table still has one per criterion"
        );
        assert_eq!(
            crate::session::goal_criterion_graph::load_criterion_graph(&out, 3)
                .validate(3)
                .len(),
            0,
            "the amended plan must still be a schedulable contract:\n{out}"
        );
    }

    #[test]
    fn an_appended_criterion_declares_its_own_scope_and_can_share_a_wave() {
        // Without a scope of its own a new criterion reads as "may write
        // anything", conflicts with every existing one, and is forced into a
        // wave by itself at the end — which silently costs the parallelism the
        // whole orchestration layer exists for.
        let amendment = PlanAmendment {
            appended: vec![scoped("third thing works", &["src/c.rs"], &[])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(report.appended, vec![3]);
        assert_eq!(scope_of(&out, 3), vec!["src/c.rs".to_string()]);
        assert_eq!(
            crate::session::goal_criterion_graph::load_criterion_graph(&out, 3).parallel_waves(),
            Some(vec![vec![1, 2, 3]]),
            "a scoped, dependency-free new criterion runs alongside the others:\n{out}"
        );

        // And a declared dependency still places it after what it builds on.
        let (out, _) = apply_amendment(
            PLAN,
            &PlanAmendment {
                appended: vec![scoped("third thing works", &["src/c.rs"], &[2])],
                ..Default::default()
            },
        );
        assert_eq!(depends_of(&out, 3), vec![2]);
        assert_eq!(
            crate::session::goal_criterion_graph::load_criterion_graph(&out, 3).parallel_waves(),
            Some(vec![vec![1, 2], vec![3]]),
        );
    }

    #[test]
    fn an_appended_criterion_cannot_smuggle_in_a_cycle_through_its_dependencies() {
        // `depends_on` on a new criterion goes through the same edge validation
        // as any other edge; an unknown target is dropped, not written.
        let amendment = PlanAmendment {
            appended: vec![scoped("third thing works", &["src/c.rs"], &[9])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(report.appended, vec![3], "the criterion itself still lands");
        assert!(
            report
                .rejected
                .contains(&Rejection::UnknownCriterion { criterion: 9 })
        );
        assert_eq!(depends_of(&out, 3), Vec::<u32>::new());
    }

    #[test]
    fn an_edge_is_added_and_never_replaces_the_declared_ones() {
        let plan = PLAN.replace("| 2 | - | src/b.rs |", "| 2 | 1 | src/b.rs |");
        let amendment = PlanAmendment {
            appended: vec![unscoped("third thing works")],
            edges: vec![(3, 2)],
            ..Default::default()
        };
        let (out, report) = apply_amendment(&plan, &amendment);
        assert_eq!(report.edges, vec![(3, 2)]);
        assert_eq!(depends_of(&out, 2), vec![1], "existing edges are untouched");
        assert_eq!(depends_of(&out, 3), vec![2]);
    }

    #[test]
    fn an_edge_that_would_close_a_cycle_is_dropped_not_written() {
        // A cyclic table is discarded wholesale by the scheduler, which would
        // turn one bad edge into a fully serial goal.
        let plan = PLAN.replace("| 2 | - | src/b.rs |", "| 2 | 1 | src/b.rs |");
        let amendment = PlanAmendment {
            edges: vec![(1, 2)],
            ..Default::default()
        };
        let (out, report) = apply_amendment(&plan, &amendment);
        assert_eq!(
            report.rejected,
            vec![Rejection::EdgeWouldCycle {
                criterion: 1,
                depends_on: 2
            }]
        );
        assert!(!report.changed());
        assert_eq!(
            out, plan,
            "a rejected amendment leaves the file byte-identical"
        );
    }

    #[test]
    fn a_scope_the_planner_left_open_is_never_filled_in() {
        // An empty scope means "may write anything", which makes the criterion
        // serial against every other one. Replacing it with the files one
        // worker happened to touch would hand back concurrency the planner did
        // not grant — the only direction an amendment must not move.
        let plan = PLAN.replace("| 2 | - | src/b.rs |", "| 2 | - | - |");
        let amendment = PlanAmendment {
            scopes: vec![(2, vec!["src/b.rs".into()])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(&plan, &amendment);
        assert_eq!(
            report.rejected,
            vec![Rejection::UndeclaredScope { criterion: 2 }]
        );
        assert_eq!(out, plan);
    }

    #[test]
    fn a_path_the_declared_scope_already_covers_is_not_restated() {
        let amendment = PlanAmendment {
            scopes: vec![(1, vec!["src/a.rs".into()])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(
            report.rejected,
            vec![Rejection::ScopeAlreadyCovered { criterion: 1 }]
        );
        assert_eq!(out, PLAN);

        // Same for a path inside a declared glob.
        let plan = PLAN.replace("| 1 | - | src/a.rs |", "| 1 | - | src/app/** |");
        let (_, report) = apply_amendment(
            &plan,
            &PlanAmendment {
                scopes: vec![(1, vec!["src/app/deep/x.rs".into()])],
                ..Default::default()
            },
        );
        assert_eq!(
            report.rejected,
            vec![Rejection::ScopeAlreadyCovered { criterion: 1 }]
        );
    }

    #[test]
    fn changes_are_dropped_one_at_a_time_not_as_a_batch() {
        // This runs unattended after a wave: one unusable entry must not
        // discard the repair the rest of the wave earned.
        let amendment = PlanAmendment {
            appended: vec![
                unscoped("second thing works"),
                unscoped("third thing works"),
            ],
            edges: vec![(2, 2), (9, 1)],
            scopes: vec![
                (7, vec!["src/x.rs".into()]),
                (2, vec!["src/shared.rs".into()]),
            ],
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(
            report.appended,
            vec![3],
            "the duplicate is dropped, the new one lands"
        );
        assert_eq!(report.scopes, vec![(2, vec!["src/shared.rs".to_string()])]);
        assert!(report.edges.is_empty());
        assert!(report.rejected.contains(&Rejection::DuplicateCriterion));
        assert!(
            report
                .rejected
                .contains(&Rejection::SelfEdge { criterion: 2 })
        );
        assert!(
            report
                .rejected
                .contains(&Rejection::UnknownCriterion { criterion: 9 })
        );
        assert!(
            report
                .rejected
                .contains(&Rejection::UnknownCriterion { criterion: 7 })
        );
        assert!(out.contains("3. third thing works"));
    }

    #[test]
    fn appending_stops_at_the_cap() {
        let amendment = PlanAmendment {
            appended: (0..MAX_APPENDED_CRITERIA + 2)
                .map(|i| unscoped(&format!("extra criterion {i}")))
                .collect(),
            ..Default::default()
        };
        let (_, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(report.appended.len(), MAX_APPENDED_CRITERIA);
        assert!(report.rejected.contains(&Rejection::AppendCapReached));
    }

    #[test]
    fn a_plan_whose_list_and_checklist_disagree_accepts_no_appends() {
        // Numbering an appended criterion needs the two to agree on what
        // number the last one has.
        let plan = PLAN.replace("2. second thing works\n", "");
        let amendment = PlanAmendment {
            appended: vec![unscoped("third thing works")],
            scopes: vec![(2, vec!["src/shared.rs".into()])],
            ..Default::default()
        };
        let (out, report) = apply_amendment(&plan, &amendment);
        assert!(report.appended.is_empty());
        assert!(matches!(
            report.rejected.first(),
            Some(Rejection::ChecklistMismatch { .. })
        ));
        assert_eq!(
            report.scopes,
            vec![(2, vec!["src/shared.rs".to_string()])],
            "a scope repair does not depend on the numbering being sound"
        );
        assert!(out.contains("src/shared.rs"));
    }

    #[test]
    fn a_plan_with_no_dependency_table_rejects_edges_and_scopes() {
        let plan = "# Plan\n\n## Acceptance criteria\n1. only\n\n## Acceptance checklist\n\
                    | Exec | Audit | Criterion |\n|---|---|---|\n| [ ] | [ ] | only |\n";
        let (out, report) = apply_amendment(
            plan,
            &PlanAmendment {
                edges: vec![(1, 2)],
                scopes: vec![(1, vec!["src/a.rs".into()])],
                ..Default::default()
            },
        );
        assert_eq!(report.rejected, vec![Rejection::NoDependencyTable]);
        assert_eq!(out, plan);
    }

    #[test]
    fn observed_writes_become_a_scope_only_amendment() {
        let amendment = amendment_from_observed_writes(&[
            (1, vec!["src\\a.rs".into()]),
            (2, vec!["src/shared.rs".into(), "src/shared.rs".into()]),
        ]);
        assert!(
            amendment.appended.is_empty() && amendment.edges.is_empty(),
            "evidence about who wrote what says nothing about new criteria"
        );
        assert_eq!(
            amendment.scopes,
            vec![
                (1, vec!["src/a.rs".to_string()]),
                (2, vec!["src/shared.rs".to_string()])
            ],
            "paths are normalized and deduplicated before they reach the plan"
        );
    }

    #[test]
    fn a_collision_recorded_once_orders_the_pair_from_then_on() {
        // The end-to-end reason this module exists: criteria 1 and 2 both wrote
        // `src/shared.rs` in the same wave, so the next schedule must not run
        // them together.
        let before = crate::session::goal_criterion_graph::load_criterion_graph(PLAN, 2);
        assert_eq!(
            before.parallel_waves(),
            Some(vec![vec![1, 2]]),
            "precondition: the declared plan lets them race"
        );
        let amendment = amendment_from_observed_writes(&[
            (1, vec!["src/shared.rs".into()]),
            (2, vec!["src/shared.rs".into()]),
        ]);
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert!(report.changed());
        let after = crate::session::goal_criterion_graph::load_criterion_graph(&out, 2);
        assert_eq!(
            after.parallel_waves(),
            Some(vec![vec![1], vec![2]]),
            "the shared file is now declared on both sides, so the scheduler \
             serializes them:\n{out}"
        );
    }

    #[test]
    fn amending_on_disk_writes_only_when_something_survived() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        std::fs::write(&path, PLAN).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        let report = amend_plan_on_disk(
            &path,
            &PlanAmendment {
                scopes: vec![(1, vec!["src/a.rs".into()])],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!report.changed());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

        let report = amend_plan_on_disk(
            &path,
            &PlanAmendment {
                scopes: vec![(1, vec!["src/shared.rs".into()])],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(report.changed());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("src/shared.rs")
        );
    }

    // Amender: proposal parse + fail-open runner.

    #[test]
    fn a_proposal_parses_even_when_wrapped_in_prose_and_stringy_numbers() {
        // Models wrap JSON in fences and write `"2"` instead of `2`. Both must
        // still become a criterion; rejecting the whole proposal over either
        // would throw away the only signal that closes the loop.
        let raw = r#"
Here is the amendment:

```json
{
  "criteria": [
    {
      "text": "third thing works",
      "write_scope": ["src/c.rs"],
      "depends_on": ["2", 1]
    }
  ]
}
```
"#;
        let amendment = parse_amendment_proposal(raw).expect("proposal must parse");
        assert_eq!(amendment.appended.len(), 1);
        assert_eq!(amendment.appended[0].text, "third thing works");
        assert_eq!(amendment.appended[0].write_scope, vec!["src/c.rs"]);
        assert_eq!(amendment.appended[0].depends_on, vec![2, 1]);
    }

    #[test]
    fn an_empty_proposal_is_a_valid_answer() {
        let amendment = parse_amendment_proposal(r#"{ "criteria": [] }"#).unwrap();
        assert!(amendment.appended.is_empty());
    }

    #[test]
    fn a_garbage_proposal_is_rejected_not_panicked() {
        assert!(parse_amendment_proposal("not json at all").is_none());
        assert!(parse_amendment_proposal("{").is_none());
    }

    /// A spawner that writes `body` to the proposal file (when given one) and
    /// returns `response`, standing in for the subagent.
    struct MockAmender {
        amendment_file: std::path::PathBuf,
        body: Option<&'static str>,
        response: Result<String, SpawnError>,
    }

    impl MockAmender {
        fn writing(amendment_file: &Path, body: &'static str) -> Self {
            Self {
                amendment_file: amendment_file.to_path_buf(),
                body: Some(body),
                response: Ok("Done".into()),
            }
        }
    }

    impl GoalAmenderSpawner for MockAmender {
        async fn spawn_amender(&self, _id: &str, _prompt: String) -> Result<String, SpawnError> {
            if let Some(body) = self.body {
                std::fs::write(&self.amendment_file, body).unwrap();
            }
            match &self.response {
                Ok(text) => Ok(text.clone()),
                Err(SpawnError::Transport(d)) => Err(SpawnError::Transport(d.clone())),
                Err(SpawnError::Runtime { message, cancelled }) => Err(SpawnError::Runtime {
                    message: message.clone(),
                    cancelled: *cancelled,
                }),
            }
        }
    }

    /// A spawn failure must leave the plan untouched and report FailedOpen,
    /// never invent a criterion to keep the loop moving.
    #[tokio::test]
    async fn a_spawn_failure_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let amendment_file = dir.path().join("amendment.json");
        std::fs::write(&plan, PLAN).unwrap();
        let tools = RoleToolNames::inherit_defaults();
        let outcome = run_goal_amender(
            &MockAmender {
                amendment_file: amendment_file.clone(),
                body: None,
                response: Err(SpawnError::Transport("gone".into())),
            },
            GoalAmenderInputs {
                objective: "demo",
                plan_file: &plan,
                amendment_file: &amendment_file,
                unattributed: &["gap · somewhere — missing".into()],
                tool_names: &tools,
            },
        )
        .await;
        assert!(matches!(outcome, GoalAmenderOutcome::FailedOpen));
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), PLAN);
    }

    /// A proposal left behind by an earlier round must never be read back as
    /// this round's answer — that would append a criterion nobody proposed
    /// against evidence that no longer exists.
    #[tokio::test]
    async fn a_stale_proposal_is_not_read_back_as_this_rounds_answer() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let amendment_file = dir.path().join("amendment.json");
        std::fs::write(&plan, PLAN).unwrap();
        std::fs::write(
            &amendment_file,
            r#"{ "criteria": [{ "text": "stale thing", "write_scope": [] }] }"#,
        )
        .unwrap();

        let tools = RoleToolNames::inherit_defaults();
        let outcome = run_goal_amender(
            // Succeeds but writes nothing: the only file present is the stale one.
            &MockAmender {
                amendment_file: amendment_file.clone(),
                body: None,
                response: Ok("Done".into()),
            },
            GoalAmenderInputs {
                objective: "demo",
                plan_file: &plan,
                amendment_file: &amendment_file,
                unattributed: &["gap · somewhere — missing".into()],
                tool_names: &tools,
            },
        )
        .await;
        assert!(
            matches!(outcome, GoalAmenderOutcome::FailedOpen),
            "the stale file must have been deleted before the spawn",
        );
    }

    /// The whole loop in one place: an unattributed finding becomes a criterion
    /// that the scheduler can actually run — with a scope, in a wave, and
    /// without disturbing what the existing criteria already earned.
    #[tokio::test]
    async fn an_unattributed_finding_becomes_a_criterion_the_next_wave_can_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let amendment_file = dir.path().join("amendment.json");
        std::fs::write(&plan, PLAN).unwrap();

        let tools = RoleToolNames::inherit_defaults();
        let outcome = run_goal_amender(
            &MockAmender::writing(
                &amendment_file,
                r#"{ "criteria": [
                    { "text": "third thing works", "write_scope": ["src/c.rs"], "depends_on": [] }
                ] }"#,
            ),
            GoalAmenderInputs {
                objective: "demo",
                plan_file: &plan,
                amendment_file: &amendment_file,
                unattributed: &["gap · src/c.rs — nothing covers the third thing".into()],
                tool_names: &tools,
            },
        )
        .await;
        let GoalAmenderOutcome::Proposed(amendment) = outcome else {
            panic!("expected a proposal, got {outcome:?}");
        };

        let report = amend_plan_on_disk(&plan, &amendment).unwrap();
        assert_eq!(report.appended, vec![3]);
        let out = std::fs::read_to_string(&plan).unwrap();

        // The contract grew, and grew in a schedulable shape.
        assert!(out.contains("3. third thing works"));
        assert_eq!(scope_of(&out, 3), vec!["src/c.rs".to_string()]);
        assert_eq!(
            crate::session::goal_criterion_graph::load_criterion_graph(&out, 3).parallel_waves(),
            Some(vec![vec![1, 2, 3]]),
            "the new criterion runs in the next wave, not alone at the end:\n{out}"
        );

        // And nothing that was already earned moved.
        let rows = parse_dual_rows(&out);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].criterion.trim(), "first thing works");
        assert!(rows[0].exec && rows[0].audit, "earned marks must survive");
        assert!(
            !rows[2].exec && !rows[2].audit,
            "an appended criterion starts unclaimed",
        );
    }

    /// The proposal FILE is the artifact, not the chat response: a model that
    /// wrote good JSON but a chatty closing message must not lose it.
    #[tokio::test]
    async fn a_proposal_on_disk_survives_a_chatty_terminal_response() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let amendment_file = dir.path().join("amendment.json");
        std::fs::write(&plan, PLAN).unwrap();

        let tools = RoleToolNames::inherit_defaults();
        let outcome = run_goal_amender(
            &MockAmender {
                amendment_file: amendment_file.clone(),
                body: Some(
                    r#"{ "criteria": [{ "text": "third thing works", "write_scope": ["src/c.rs"], "depends_on": [2] }] }"#,
                ),
                response: Ok("Sure, I wrote the amendment. Done-ish.".into()),
            },
            GoalAmenderInputs {
                objective: "demo",
                plan_file: &plan,
                amendment_file: &amendment_file,
                unattributed: &["gap · somewhere — missing".into()],
                tool_names: &tools,
            },
        )
        .await;
        let GoalAmenderOutcome::Proposed(amendment) = outcome else {
            panic!("expected a proposal, got {outcome:?}");
        };
        let (out, report) = apply_amendment(PLAN, &amendment);
        assert_eq!(report.appended, vec![3]);
        assert_eq!(scope_of(&out, 3), vec!["src/c.rs".to_string()]);
        assert_eq!(depends_of(&out, 3), vec![2]);
    }

    /// An amender that proposes nothing is the normal answer when the auditor
    /// simply failed to cite a criterion that already covers the gap.
    #[tokio::test]
    async fn proposing_nothing_is_reported_as_nothing_not_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let amendment_file = dir.path().join("amendment.json");
        std::fs::write(&plan, PLAN).unwrap();

        let tools = RoleToolNames::inherit_defaults();
        let outcome = run_goal_amender(
            &MockAmender::writing(&amendment_file, r#"{ "criteria": [] }"#),
            GoalAmenderInputs {
                objective: "demo",
                plan_file: &plan,
                amendment_file: &amendment_file,
                unattributed: &["gap · somewhere — already covered".into()],
                tool_names: &tools,
            },
        )
        .await;
        assert!(matches!(outcome, GoalAmenderOutcome::Nothing));
    }

    #[test]
    fn the_amender_prompt_names_the_unattributed_findings_and_no_tool_placeholders() {
        let tools = RoleToolNames::inherit_defaults();
        let plan = std::path::Path::new("/tmp/plan.md");
        let amendment = std::path::Path::new("/tmp/amendment.json");
        let prompt = render_amender_prompt(&GoalAmenderInputs {
            objective: "ship the demo",
            plan_file: plan,
            amendment_file: amendment,
            unattributed: &["gap · foo — bar".into(), "bug · baz — qux".into()],
            tool_names: &tools,
        });
        assert!(prompt.contains("ship the demo"));
        assert!(prompt.contains("gap · foo — bar"));
        assert!(prompt.contains("bug · baz — qux"));
        assert!(prompt.contains("/tmp/plan.md"));
        assert!(prompt.contains("/tmp/amendment.json"));
        crate::session::goal_role_tools::tests::assert_no_tool_placeholders(&prompt);
    }
}
