//! Implement several acceptance criteria at once, one subagent each.
//!
//! Everything upstream of this module made concurrency *describable*: the
//! planner declares `depends_on` and `write_scope` per criterion, the graph
//! turns that into waves, the checklist tracks each criterion separately, and a
//! refuted criterion returns only itself (plus its dependents) to execution.
//! None of it made the goal actually run two things at the same time — a single
//! implementer still worked the ready set in whatever order it liked.
//!
//! This module spends that groundwork. For a wave with several ready criteria it
//! spawns one worker per criterion, each in its own git worktree, waits for all
//! of them, then lands their changes into the repo in criterion order.
//!
//! Three properties are load-bearing, and each one is a failure mode if lost:
//!
//! - **Isolation is physical, not advisory.** Workers get separate worktrees, so
//!   no worker can see or clobber another's edits while they work. `write_scope`
//!   is what keeps them from colliding at all; the worktree is what turns a
//!   worker that ignores its scope into a REPORTED merge conflict rather than a
//!   silently lost edit.
//! - **The plan is not in the repo.** `plan.md` lives under the session
//!   directory, so no worker can see or edit the acceptance checklist. Exec
//!   marks are set by the parent, after a merge succeeds — a worker cannot claim
//!   its own criterion done.
//! - **Landing is ordered and reported per criterion.** Merges are applied by
//!   criterion number so the result does not depend on which worker finished
//!   first, and a conflicted criterion stays open with its conflict recorded
//!   instead of being counted as delivered.
//!
//! Fan-out is off by default (`[goal] fanout_max = 1`). It also declines to run
//! when it cannot land the work — no git repo, or subagent worktree snapshotting
//! enabled (which deletes the worktree the merge would read). Declining means
//! the goal runs serially, which is correct and slower; the alternative would be
//! work that silently never reaches the repo.

use std::path::{Path, PathBuf};

use crate::session::goal_tracker::CriterionView;

/// Default `[goal] fanout_max`: fan-out disabled.
pub(crate) const GOAL_FANOUT_MAX_DEFAULT: u32 = 1;

/// Hard ceiling on concurrent criterion workers.
///
/// Each worker is a full session with its own model context and worktree, so
/// the cost is linear and the disk cost is paid up front. Past a handful the
/// wave is bounded by the merge and the review of it, not by the models.
pub(crate) const GOAL_FANOUT_MAX_CEILING: u32 = 8;

/// Fewest ready criteria worth fanning out.
///
/// One criterion is not a wave: spawning a worker for it would add a session, a
/// worktree, and a merge to work the goal session can do directly, with no
/// concurrency gained.
pub(crate) const GOAL_FANOUT_MIN_CRITERIA: usize = 2;

/// Why a wave was not fanned out. Recorded rather than silently skipped: a goal
/// that quietly runs serially when the user configured parallelism is a support
/// question with no evidence attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FanoutDeclined {
    /// `fanout_max <= 1`.
    Disabled,
    /// Fewer than [`GOAL_FANOUT_MIN_CRITERIA`] criteria are ready.
    NotEnoughReady { ready: usize },
    /// The workspace is not a git repo, so workers cannot get worktrees.
    NoRepo,
    /// Worktrees are snapshotted and deleted on completion, so there would be
    /// nothing left to merge from.
    WorktreesDisposed,
}

impl FanoutDeclined {
    pub(crate) fn as_const_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotEnoughReady { .. } => "not_enough_ready",
            Self::NoRepo => "no_repo",
            Self::WorktreesDisposed => "worktrees_disposed",
        }
    }
}

/// The criteria a wave should run in parallel, or why it should not.
///
/// Ready means all four of:
///
/// - **Not yet claimed** (`Exec` unticked). A criterion whose implementation
///   already landed is not work, even though its audit is still pending — and
///   since the audit only runs once every criterion is claimed, treating it as
///   work would re-dispatch a worker for it on every single round.
/// - **Not accepted** (`Audit` unticked).
/// - **Not deferred.**
/// - **Every declared dependency claimed** (`Exec` ticked).
///
/// That last rule cannot wait for the dependency's *audit*. The panel only runs
/// once every criterion is claimed, so gating dependents on acceptance would
/// deadlock the goal: criterion 2 would wait for criterion 1's audit, which
/// waits for criterion 2 to be claimed. Building on claimed-but-unaudited work
/// is safe because a worker's checkout replicates the parent's working tree
/// (`PreserveWorkingTree`), so the dependency's code is really there, and
/// because a later refutation of criterion 1 also strips the audit marks of
/// everything built on it — see
/// [`with_dependents`](crate::session::goal_criterion_graph::with_dependents).
///
/// Note what the first rule excludes: a REFUTED criterion keeps its `Exec` tick,
/// so it is not given to a worker. That is deliberate. A rejection is prose
/// about the deliverable as a whole, and acting on it needs the goal's context —
/// which the coordinating session has and a single-criterion worker does not.
/// Workers write first drafts; fixes stay with the session.
pub(crate) fn plan_wave(
    criteria: &[CriterionView],
    fanout_max: u32,
    repo_available: bool,
    worktrees_disposed: bool,
) -> Result<Vec<CriterionView>, FanoutDeclined> {
    if fanout_max <= 1 {
        return Err(FanoutDeclined::Disabled);
    }
    if !repo_available {
        return Err(FanoutDeclined::NoRepo);
    }
    if worktrees_disposed {
        return Err(FanoutDeclined::WorktreesDisposed);
    }
    let claimed = |n: u32| {
        criteria
            .iter()
            .any(|c| c.number == n && (c.exec || c.audit))
    };
    let ready: Vec<CriterionView> = criteria
        .iter()
        .filter(|c| !c.exec && !c.audit && !c.deferred && c.depends_on.iter().copied().all(claimed))
        .take(fanout_max as usize)
        .cloned()
        .collect();
    if ready.len() < GOAL_FANOUT_MIN_CRITERIA {
        return Err(FanoutDeclined::NotEnoughReady { ready: ready.len() });
    }
    Ok(ready)
}

/// What one worker produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerOutcome {
    pub criterion: u32,
    /// The worker's final message, for the parent's next-round context.
    pub summary: String,
    /// Worktree to merge from. `None` when the worker failed before writing
    /// anything, or when its worktree did not survive.
    pub worktree: Option<PathBuf>,
    /// Set when the worker itself failed (transport, runtime, cancellation).
    pub error: Option<String>,
}

/// What landed, per criterion, after merging a wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    /// Every changed file landed; the criterion may be marked Exec-complete.
    Landed { criterion: u32, files: usize },
    /// Some files could not be merged. The criterion stays open and the
    /// conflicting paths are reported so the next round knows where to look.
    Conflicted {
        criterion: u32,
        conflicts: Vec<String>,
    },
    /// Nothing to merge, or the merge could not run.
    Skipped { criterion: u32, why: String },
}

impl MergeOutcome {
    pub(crate) fn criterion(&self) -> u32 {
        match self {
            Self::Landed { criterion, .. }
            | Self::Conflicted { criterion, .. }
            | Self::Skipped { criterion, .. } => *criterion,
        }
    }

    /// Only a clean landing earns an Exec mark. A conflicted or skipped
    /// criterion has no deliverable in the repo, and marking it would put the
    /// goal one audit away from claiming work that is not there.
    pub(crate) fn landed(&self) -> bool {
        matches!(self, Self::Landed { .. })
    }
}

/// The prompt for one criterion worker.
///
/// Deliberately narrow. The worker is told its one criterion, the files it owns,
/// and that it cannot see or update the goal contract. Anything broader invites
/// it to "help" with a neighbouring criterion, which is precisely the collision
/// the worktrees are protecting against — and unlike a file collision, an
/// unrequested design change in another criterion's area merges cleanly and is
/// found much later.
pub(crate) fn worker_prompt(objective: &str, criterion: &CriterionView) -> String {
    let scope = if criterion.write_scope.is_empty() {
        "Not declared — stay as close as possible to the files this criterion is about.".to_string()
    } else {
        criterion.write_scope.join(", ")
    };
    format!(
        "You are implementing ONE acceptance criterion of a larger goal, in your own \
         isolated worktree, at the same time as other agents implementing the other \
         criteria.\n\n\
         Overall objective (context only — do NOT try to satisfy all of it):\n{objective}\n\n\
         Your criterion (#{number}) — this and only this is your job:\n{text}\n\n\
         Files you may write: {scope}\n\n\
         Rules:\n\
         - Write ONLY inside the files above. Another agent is editing the rest right \
           now, in its own checkout, and it cannot see your edits: touching its files \
           produces a merge conflict that throws away one of the two rounds.\n\
         - Do not refactor, reformat, or 'improve' anything your criterion does not require.\n\
         - You cannot see or update the goal's acceptance checklist, and you must not try \
           to declare the goal complete. Your work is verified independently after it is \
           merged.\n\
         - Finish by stating, in a few lines, what you changed and how the criterion is \
           observable in the code you wrote. That text is the only thing the coordinating \
           agent will see.\n",
        number = criterion.number,
        text = criterion.text.trim(),
    )
}

/// Merge one worker's worktree into the repo.
///
/// `Merge` mode rather than `Overwrite`: another criterion's worker may have
/// legitimately touched a shared file first, and overwriting would delete its
/// work while reporting success.
pub(crate) async fn merge_worker(session_id: &str, outcome: &WorkerOutcome) -> MergeOutcome {
    use xai_grok_workspace::worktree::{ApplyMode, ApplyWorktreeRequest, ApplyWorktreeResponse};
    let criterion = outcome.criterion;
    if let Some(err) = &outcome.error {
        return MergeOutcome::Skipped {
            criterion,
            why: format!("worker failed: {err}"),
        };
    }
    let Some(worktree) = &outcome.worktree else {
        return MergeOutcome::Skipped {
            criterion,
            why: "worker left no worktree to merge".to_string(),
        };
    };
    let req = ApplyWorktreeRequest {
        session_id: session_id.to_string(),
        worktree_path: worktree.to_string_lossy().to_string(),
        mode: ApplyMode::Merge,
    };
    match xai_grok_workspace::worktree::apply_worktree(&req).await {
        Ok(ApplyWorktreeResponse::Success { files, .. }) => MergeOutcome::Landed {
            criterion,
            files: files.len(),
        },
        Ok(ApplyWorktreeResponse::Conflicts { conflicts, .. }) => MergeOutcome::Conflicted {
            criterion,
            conflicts: conflicts.into_iter().map(|c| c.path).collect(),
        },
        Err(e) => MergeOutcome::Skipped {
            criterion,
            why: format!("merge failed: {e}"),
        },
    }
}

/// Merge a whole wave, lowest criterion number first.
///
/// Order matters for reproducibility: with two workers touching one file, the
/// merged result must not depend on which model happened to finish first.
pub(crate) async fn merge_wave(session_id: &str, workers: &[WorkerOutcome]) -> Vec<MergeOutcome> {
    let mut ordered: Vec<&WorkerOutcome> = workers.iter().collect();
    ordered.sort_by_key(|w| w.criterion);
    let mut out = Vec::with_capacity(ordered.len());
    for w in ordered {
        out.push(merge_worker(session_id, w).await);
    }
    out
}

/// The block handed to the goal session after a wave, replacing the ready-wave
/// block for that round.
///
/// States outcomes per criterion, including the failures. A summary that only
/// listed successes would leave the coordinating model to infer the rest from
/// silence, and it would infer that the work is done.
pub(crate) fn render_wave_report(workers: &[WorkerOutcome], merges: &[MergeOutcome]) -> String {
    let mut out = String::from(
        "Parallel implementation round finished. Each criterion below was implemented by a \
         separate agent in its own worktree; results are already merged into the working \
         tree.\n",
    );
    for m in merges {
        let n = m.criterion();
        let summary = workers
            .iter()
            .find(|w| w.criterion == n)
            .map(|w| one_line(&w.summary))
            .unwrap_or_default();
        match m {
            MergeOutcome::Landed { files, .. } => {
                out.push_str(&format!(
                    "- criterion {n}: merged ({files} file(s)). {summary}\n"
                ));
            }
            MergeOutcome::Conflicted { conflicts, .. } => {
                out.push_str(&format!(
                    "- criterion {n}: NOT merged — conflicts in {}. Its work is still in the \
                     worker's worktree only; redo it here or resolve the conflict. {summary}\n",
                    conflicts.join(", "),
                ));
            }
            MergeOutcome::Skipped { why, .. } => {
                out.push_str(&format!(
                    "- criterion {n}: nothing landed — {why}. {summary}\n"
                ));
            }
        }
    }
    let landed: Vec<u32> = merges
        .iter()
        .filter(|m| m.landed())
        .map(MergeOutcome::criterion)
        .collect();
    let open: Vec<u32> = merges
        .iter()
        .filter(|m| !m.landed())
        .map(MergeOutcome::criterion)
        .collect();
    if !landed.is_empty() {
        out.push_str(&format!(
            "Exec marks for criterion(s) {} are set; they still need independent audit.\n",
            numbers(&landed),
        ));
    }
    if !open.is_empty() {
        out.push_str(&format!(
            "Criterion(s) {} did not land and remain your work.\n",
            numbers(&open),
        ));
    }
    out.push_str(
        "Review the merged result as a whole before requesting verification: the parts were \
         written independently and may not fit together.\n\n",
    );
    out
}

fn numbers(ns: &[u32]) -> String {
    ns.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

/// Collapse a worker's final message to one line for the report.
const WORKER_SUMMARY_MAX: usize = 240;

fn one_line(s: &str) -> String {
    let flat: String = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= WORKER_SUMMARY_MAX {
        return flat;
    }
    let kept: String = flat.chars().take(WORKER_SUMMARY_MAX - 1).collect();
    format!("{kept}…")
}

/// Whether `cwd` sits in a git repo, which is what worker worktrees are cut
/// from.
pub(crate) fn repo_available(cwd: &Path) -> bool {
    xai_grok_workspace::session::git::find_git_root_from_path(cwd).is_ok()
}

/// Criterion workers spawn on the same fixed `general-purpose` subagent type as
/// every other goal role — see [`GOAL_ROLE_SUBAGENT_TYPE`]. It is the type that
/// resolves to a capable toolset on whichever harness the session runs; a
/// role-specific type string would not resolve to an agent definition at all.
/// Roles are told apart by their description, not their subagent type.
///
/// [`GOAL_ROLE_SUBAGENT_TYPE`]: crate::session::goal_planner::GOAL_ROLE_SUBAGENT_TYPE
pub(crate) use crate::session::goal_planner::GOAL_ROLE_SUBAGENT_TYPE as GOAL_WORKER_SUBAGENT_TYPE;

/// Description shown for a worker in the subagent list.
const GOAL_WORKER_DESCRIPTION: &str = "Goal criterion implementer";

/// Spawns criterion workers. A trait so the wave driver can be tested without a
/// subagent coordinator, matching the planner and verifier spawners.
#[allow(async_fn_in_trait)]
pub(crate) trait GoalWorkerSpawner {
    /// Run one worker to completion and report what it produced. Never fails
    /// the wave: a worker error becomes a [`WorkerOutcome`] with `error` set, so
    /// one dead worker cannot discard its siblings' merged work.
    async fn spawn_worker(&self, id: &str, criterion: u32, prompt: String) -> WorkerOutcome;
}

/// Production spawner over the subagent coordinator channel.
pub(crate) struct ChannelWorkerSpawner {
    pub(crate) event_tx: tokio::sync::mpsc::UnboundedSender<
        xai_grok_tools::implementations::grok_build::task::types::SubagentEvent,
    >,
    pub(crate) parent_session_id: String,
    pub(crate) parent_prompt_id: Option<String>,
    pub(crate) cwd: Option<String>,
    /// Model + harness override for the worker role, if the deployment pins one.
    pub(crate) role_model: Option<String>,
    pub(crate) role_agent_type: Option<String>,
}

impl GoalWorkerSpawner for ChannelWorkerSpawner {
    async fn spawn_worker(&self, id: &str, criterion: u32, prompt: String) -> WorkerOutcome {
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentEvent, SubagentRequest, SubagentRuntimeOverrides,
        };
        let failed = |error: String| WorkerOutcome {
            criterion,
            summary: String::new(),
            worktree: None,
            error: Some(error),
        };
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: id.to_string(),
            prompt,
            description: format!("{GOAL_WORKER_DESCRIPTION} (criterion {criterion})"),
            subagent_type: GOAL_WORKER_SUBAGENT_TYPE.to_string(),
            parent_session_id: self.parent_session_id.clone(),
            parent_prompt_id: self.parent_prompt_id.clone(),
            resume_from: None,
            cwd: self.cwd.clone(),
            runtime_overrides: SubagentRuntimeOverrides {
                model: self.role_model.clone(),
                harness_agent_type: self.role_agent_type.clone(),
                // The whole design rests on this: without a worktree, two
                // workers edit the same checkout and the wave corrupts itself.
                isolation: Some(xai_tool_types::SubagentIsolationMode::Worktree),
                ..Default::default()
            },
            run_in_background: false,
            // Harness-internal: the coordinating model is told about the wave by
            // the report, not by an idle-reminder notification per worker.
            surface_completion: false,
            // A worker must not inherit the parent's belief about progress; it
            // gets exactly its criterion. Forking would also copy a transcript
            // about the OTHER criteria, which is what makes workers wander.
            fork_context: false,
            result_tx,
        };
        if self
            .event_tx
            .send(SubagentEvent::Spawn(Box::new(request)))
            .is_err()
        {
            return failed("subagent coordinator channel closed".to_string());
        }
        let Ok(result) = result_rx.await else {
            return failed("subagent result channel dropped".to_string());
        };
        if !result.success {
            let mut out = failed(result.error.unwrap_or_else(|| {
                if result.cancelled {
                    "cancelled".to_string()
                } else {
                    "unknown error".to_string()
                }
            }));
            // A failed worker may still have written something useful before it
            // died; keep the path so the merge can decide, and the summary so
            // the report can say what it was doing.
            out.worktree = result.worktree_path.as_deref().map(PathBuf::from);
            out.summary = result.output.to_string();
            out
        } else {
            WorkerOutcome {
                criterion,
                summary: result.output.to_string(),
                worktree: result.worktree_path.as_deref().map(PathBuf::from),
                error: None,
            }
        }
    }
}

/// Run every criterion in `wave` concurrently and return their outcomes.
///
/// The workers are awaited together — that concurrency is the entire point, and
/// awaiting them in sequence would produce identical results at N times the
/// wall-clock cost, which is exactly the bug this module exists to fix.
pub(crate) async fn run_wave<S: GoalWorkerSpawner>(
    spawner: &S,
    objective: &str,
    wave: &[CriterionView],
) -> Vec<WorkerOutcome> {
    let spawns = wave.iter().map(|c| {
        let id = uuid::Uuid::now_v7().to_string();
        let prompt = worker_prompt(objective, c);
        async move { spawner.spawn_worker(&id, c.number, prompt).await }
    });
    futures::future::join_all(spawns).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(number: u32, audit: bool, depends_on: Vec<u32>) -> CriterionView {
        CriterionView {
            number,
            text: format!("criterion {number}"),
            exec: audit,
            audit,
            depends_on,
            write_scope: vec![format!("src/c{number}.rs")],
            wave: None,
            deferred: false,
        }
    }

    #[test]
    fn fanout_is_off_unless_configured() {
        let criteria = vec![view(1, false, vec![]), view(2, false, vec![])];
        assert_eq!(
            plan_wave(&criteria, 1, true, false),
            Err(FanoutDeclined::Disabled),
        );
    }

    #[test]
    fn a_wave_takes_only_dependency_free_criteria() {
        let criteria = vec![
            view(1, false, vec![]),
            view(2, false, vec![]),
            view(3, false, vec![1]),
        ];
        let wave = plan_wave(&criteria, 4, true, false).expect("two are ready");
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![1, 2],
            "criterion 3 waits on 1, which nobody has implemented yet"
        );
    }

    #[test]
    fn a_claimed_dependency_releases_its_dependents() {
        let mut criteria = vec![
            view(1, false, vec![]),
            view(2, false, vec![1]),
            view(3, false, vec![1]),
        ];
        assert_eq!(
            plan_wave(&criteria, 4, true, false),
            Err(FanoutDeclined::NotEnoughReady { ready: 1 }),
            "only criterion 1 can start; 2 and 3 extend work that does not exist yet"
        );
        criteria[0].exec = true;
        let wave = plan_wave(&criteria, 4, true, false).expect("1 claimed, 2 and 3 released");
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![2, 3],
            "waiting for criterion 1's AUDIT would deadlock: the panel does not run \
             until 2 and 3 are claimed too"
        );
        // Acceptance releases them just the same, and 1 stays out of the wave.
        criteria[0].audit = true;
        let wave = plan_wave(&criteria, 4, true, false).unwrap();
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn one_ready_criterion_is_not_worth_a_worker() {
        let criteria = vec![view(1, true, vec![]), view(2, false, vec![])];
        assert_eq!(
            plan_wave(&criteria, 4, true, false),
            Err(FanoutDeclined::NotEnoughReady { ready: 1 }),
        );
    }

    #[test]
    fn the_wave_respects_the_configured_cap() {
        let criteria: Vec<CriterionView> = (1..=6).map(|n| view(n, false, vec![])).collect();
        let wave = plan_wave(&criteria, 3, true, false).unwrap();
        assert_eq!(wave.len(), 3);
    }

    #[test]
    fn deferred_criteria_are_never_given_to_a_worker() {
        let mut criteria = vec![
            view(1, false, vec![]),
            view(2, false, vec![]),
            view(3, false, vec![]),
        ];
        criteria[1].deferred = true;
        let wave = plan_wave(&criteria, 4, true, false).unwrap();
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn fanout_declines_when_the_work_could_not_be_landed() {
        let criteria = vec![view(1, false, vec![]), view(2, false, vec![])];
        assert_eq!(
            plan_wave(&criteria, 4, false, false),
            Err(FanoutDeclined::NoRepo),
            "no repo means no worktrees to write in"
        );
        assert_eq!(
            plan_wave(&criteria, 4, true, true),
            Err(FanoutDeclined::WorktreesDisposed),
            "a disposed worktree is a wave whose work silently never reaches the repo"
        );
    }

    #[test]
    fn only_a_clean_landing_earns_an_exec_mark() {
        assert!(
            MergeOutcome::Landed {
                criterion: 1,
                files: 2
            }
            .landed()
        );
        assert!(
            !MergeOutcome::Conflicted {
                criterion: 1,
                conflicts: vec!["src/a.rs".into()]
            }
            .landed(),
            "a conflicted criterion has no deliverable in the repo"
        );
        assert!(
            !MergeOutcome::Skipped {
                criterion: 1,
                why: "worker failed".into()
            }
            .landed()
        );
    }

    #[test]
    fn the_worker_prompt_names_one_criterion_and_its_files() {
        let c = view(2, false, vec![]);
        let p = worker_prompt("build the whole app", &c);
        assert!(p.contains("#2"), "{p}");
        assert!(p.contains("src/c2.rs"), "{p}");
        assert!(
            p.contains("do NOT try to satisfy all of it"),
            "the objective is context, not the worker's job: {p}"
        );
        assert!(
            p.contains("must not try to declare the goal complete"),
            "a worker seeing one criterion cannot judge the goal: {p}"
        );
    }

    #[test]
    fn an_undeclared_write_scope_is_stated_as_such() {
        let mut c = view(1, false, vec![]);
        c.write_scope.clear();
        let p = worker_prompt("obj", &c);
        assert!(p.contains("Not declared"), "{p}");
    }

    #[test]
    fn the_report_states_failures_as_plainly_as_successes() {
        let workers = vec![
            WorkerOutcome {
                criterion: 1,
                summary: "added the parser".into(),
                worktree: None,
                error: None,
            },
            WorkerOutcome {
                criterion: 2,
                summary: "wired the cli".into(),
                worktree: None,
                error: None,
            },
            WorkerOutcome {
                criterion: 3,
                summary: String::new(),
                worktree: None,
                error: Some("cancelled".into()),
            },
        ];
        let merges = vec![
            MergeOutcome::Landed {
                criterion: 1,
                files: 3,
            },
            MergeOutcome::Conflicted {
                criterion: 2,
                conflicts: vec!["src/cli.rs".into()],
            },
            MergeOutcome::Skipped {
                criterion: 3,
                why: "worker failed: cancelled".into(),
            },
        ];
        let report = render_wave_report(&workers, &merges);
        assert!(
            report.contains("criterion 1: merged (3 file(s))"),
            "{report}"
        );
        assert!(report.contains("added the parser"), "{report}");
        assert!(
            report.contains("criterion 2: NOT merged — conflicts in src/cli.rs"),
            "{report}"
        );
        assert!(report.contains("criterion 3: nothing landed"), "{report}");
        assert!(
            report.contains("Exec marks for criterion(s) 1 are set"),
            "only the landed criterion earns a mark: {report}"
        );
        assert!(
            report.contains("Criterion(s) 2, 3 did not land"),
            "the coordinator must not infer the failures from silence: {report}"
        );
    }

    #[test]
    fn a_long_worker_summary_is_truncated_not_dropped() {
        let workers = vec![WorkerOutcome {
            criterion: 1,
            summary: "x".repeat(400),
            worktree: None,
            error: None,
        }];
        let merges = vec![MergeOutcome::Landed {
            criterion: 1,
            files: 1,
        }];
        let report = render_wave_report(&workers, &merges);
        assert!(report.contains('…'), "{report}");
        assert!(report.lines().count() < 10, "still a compact report");
    }

    #[tokio::test]
    async fn a_failed_worker_is_not_merged() {
        let outcome = WorkerOutcome {
            criterion: 1,
            summary: String::new(),
            worktree: Some(PathBuf::from("/nonexistent")),
            error: Some("boom".into()),
        };
        // Returns without touching the filesystem: the error short-circuits.
        assert_eq!(
            merge_worker("s", &outcome).await,
            MergeOutcome::Skipped {
                criterion: 1,
                why: "worker failed: boom".into()
            },
        );
    }

    #[tokio::test]
    async fn a_worker_without_a_worktree_is_reported_not_assumed_clean() {
        let outcome = WorkerOutcome {
            criterion: 4,
            summary: "did stuff".into(),
            worktree: None,
            error: None,
        };
        assert!(matches!(
            merge_worker("s", &outcome).await,
            MergeOutcome::Skipped { criterion: 4, .. }
        ));
    }

    #[test]
    fn a_criterion_already_implemented_is_not_dispatched_again() {
        // Exec ticked, audit still pending — the state every criterion sits in
        // between its wave and the panel's verdict. Re-dispatching it would
        // rewrite landed work on every round until the audit finally runs.
        let mut criteria = vec![
            view(1, false, vec![]),
            view(2, false, vec![]),
            view(3, false, vec![]),
        ];
        criteria[0].exec = true;
        let wave = plan_wave(&criteria, 8, true, false).unwrap();
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn a_refuted_criterion_goes_back_to_the_session_not_to_a_worker() {
        // Rejection clears Audit and leaves Exec: the fix needs the rejection's
        // context, which only the coordinating session has.
        let mut criteria = vec![view(1, true, vec![]), view(2, true, vec![])];
        for c in &mut criteria {
            c.audit = false; // refuted; Exec stays ticked
        }
        assert_eq!(
            plan_wave(&criteria, 8, true, false),
            Err(FanoutDeclined::NotEnoughReady { ready: 0 }),
        );
    }

    #[test]
    fn criteria_that_share_files_are_never_put_in_one_wave() {
        // End-to-end from plan text: the graph loader serializes overlapping
        // write scopes, so the pair arrives here already ordered and cannot be
        // co-scheduled. This is the property that makes fan-out safe on a plan
        // whose scopes were written carelessly — the whole chain has to hold,
        // not just `plan_wave`.
        let body = "## Acceptance criteria\n\
            1. parser\n\
            2. cli\n\
            3. docs\n\n\
            ## Acceptance checklist\n\
            | Exec | Audit | Criterion |\n\
            |---|---|---|\n\
            | [ ] | [ ] | parser |\n\
            | [ ] | [ ] | cli |\n\
            | [ ] | [ ] | docs |\n\n\
            ## Criterion dependencies\n\
            | # | Depends on | Write scope |\n\
            |---|---|---|\n\
            | 1 | - | src/app/mod.rs |\n\
            | 2 | - | src/app/** |\n\
            | 3 | - | docs/x.md |\n";
        let criteria = crate::session::goal_criterion_graph::build_criteria_view(body, &[]);
        let wave = plan_wave(&criteria, 8, true, false).expect("1 and 3 are disjoint");
        assert_eq!(
            wave.iter().map(|c| c.number).collect::<Vec<_>>(),
            vec![1, 3],
            "criterion 2 shares src/app with criterion 1, so it must wait"
        );
    }

    struct StubSpawner {
        /// Criterion numbers in the order their worker STARTED, to prove the
        /// wave overlaps instead of running one worker at a time.
        started: std::sync::Arc<parking_lot::Mutex<Vec<u32>>>,
        /// Workers still to start before any is allowed to finish.
        gate: std::sync::Arc<tokio::sync::Barrier>,
    }

    impl GoalWorkerSpawner for StubSpawner {
        async fn spawn_worker(&self, _id: &str, criterion: u32, prompt: String) -> WorkerOutcome {
            self.started.lock().push(criterion);
            // Every worker must be inside this call before any returns; a
            // sequential driver would deadlock here rather than pass.
            self.gate.wait().await;
            WorkerOutcome {
                criterion,
                summary: format!("worked {criterion}, prompt {} bytes", prompt.len()),
                worktree: None,
                error: None,
            }
        }
    }

    #[tokio::test]
    async fn a_wave_runs_its_workers_concurrently() {
        let wave: Vec<CriterionView> = (1..=3).map(|n| view(n, false, vec![])).collect();
        let spawner = StubSpawner {
            started: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            gate: std::sync::Arc::new(tokio::sync::Barrier::new(wave.len())),
        };
        let outcomes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_wave(&spawner, "objective", &wave),
        )
        .await
        .expect("all three workers must be in flight together; a serial driver deadlocks");
        assert_eq!(outcomes.len(), 3);
        assert_eq!(spawner.started.lock().len(), 3);
        assert!(outcomes.iter().all(|o| o.error.is_none()), "{outcomes:?}");
        // Each worker got its own criterion's prompt, not a shared one.
        for o in &outcomes {
            assert!(
                o.summary.starts_with(&format!("worked {}", o.criterion)),
                "{o:?}"
            );
        }
    }

    // ── Merge against a real repo ─────────────────────────────────────
    //
    // `merge_worker` rests on one assumption that no unit test can check: that
    // `apply_worktree` in Merge mode lands a worker's files in the parent repo
    // and reports a collision as `Conflicts` rather than an error. If that is
    // wrong, the harness ticks Exec for work that never arrived — the one
    // failure this whole module is built to avoid — so it is worth the git.

    fn init_repo(path: &std::path::Path) {
        crate::test_support::ensure_hermetic_git_on_path();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t.com"],
            vec!["config", "user.name", "T"],
        ] {
            std::process::Command::new("git")
                .current_dir(path)
                .args(&args)
                .output()
                .unwrap();
        }
    }

    fn commit_all(path: &std::path::Path, msg: &str) {
        for args in [vec!["add", "."], vec!["commit", "-m", msg]] {
            std::process::Command::new("git")
                .current_dir(path)
                .args(&args)
                .output()
                .unwrap();
        }
    }

    /// A linked worktree of `repo`, standing in for a criterion worker's
    /// checkout.
    fn add_worktree(repo: &std::path::Path, name: &str) -> PathBuf {
        let dest = repo.join(format!("../{name}"));
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(["worktree", "add", "-f", &dest.to_string_lossy(), "HEAD"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        dest.canonicalize().unwrap()
    }

    fn worker_at(criterion: u32, worktree: PathBuf) -> WorkerOutcome {
        WorkerOutcome {
            criterion,
            summary: format!("criterion {criterion} done"),
            worktree: Some(worktree),
            error: None,
        }
    }

    #[tokio::test]
    async fn disjoint_workers_both_land_in_the_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("a.rs"), "a0\n").unwrap();
        std::fs::write(repo.join("b.rs"), "b0\n").unwrap();
        commit_all(&repo, "init");

        let w1 = add_worktree(&repo, "w1");
        let w2 = add_worktree(&repo, "w2");
        std::fs::write(w1.join("a.rs"), "a1\n").unwrap();
        std::fs::write(w2.join("b.rs"), "b1\n").unwrap();

        let merges = merge_wave("s", &[worker_at(1, w1.clone()), worker_at(2, w2.clone())]).await;
        assert!(
            merges.iter().all(MergeOutcome::landed),
            "each worker touched only its own file: {merges:?}"
        );
        assert_eq!(std::fs::read_to_string(repo.join("a.rs")).unwrap(), "a1\n");
        assert_eq!(std::fs::read_to_string(repo.join("b.rs")).unwrap(), "b1\n");
    }

    #[tokio::test]
    async fn a_collision_is_reported_and_leaves_the_first_writer_intact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("shared.rs"), "base\n").unwrap();
        commit_all(&repo, "init");

        let w1 = add_worktree(&repo, "c1");
        let w2 = add_worktree(&repo, "c2");
        std::fs::write(w1.join("shared.rs"), "from criterion 1\n").unwrap();
        std::fs::write(w2.join("shared.rs"), "from criterion 2\n").unwrap();

        // Criterion 2 merges into a tree criterion 1 already changed.
        let merges = merge_wave("s", &[worker_at(2, w2), worker_at(1, w1)]).await;
        assert_eq!(
            merges
                .iter()
                .map(MergeOutcome::criterion)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "criterion order, not finishing order"
        );
        assert!(merges[0].landed(), "the lower criterion lands: {merges:?}");
        match &merges[1] {
            MergeOutcome::Conflicted { conflicts, .. } => {
                assert!(
                    conflicts.iter().any(|p| p.contains("shared.rs")),
                    "{conflicts:?}"
                );
            }
            other => panic!("a second writer to one file must be reported, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(repo.join("shared.rs")).unwrap(),
            "from criterion 1\n",
            "a conflict must not overwrite the work that did land"
        );
        // And the report must not let the coordinator believe 2 is done.
        let report = render_wave_report(&[], &merges);
        assert!(
            report.contains("Exec marks for criterion(s) 1 are set"),
            "{report}"
        );
        assert!(report.contains("Criterion(s) 2 did not land"), "{report}");
    }

    #[tokio::test]
    async fn a_worker_that_changed_nothing_is_not_a_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("a.rs"), "a0\n").unwrap();
        commit_all(&repo, "init");
        let idle = add_worktree(&repo, "idle");

        // Empty diff is `Success` with no files: the criterion is claimed, and
        // the audit is what decides whether "no change" was the right answer.
        assert!(
            merge_worker("s", &worker_at(1, idle)).await.landed(),
            "an empty worker diff is a merge with nothing in it, not an error"
        );
    }

    #[tokio::test]
    async fn a_wave_merges_in_criterion_order() {
        // Ordering is what makes the merged result reproducible; the outcomes
        // here all short-circuit, so this asserts the order, not the git work.
        let workers = vec![
            WorkerOutcome {
                criterion: 3,
                summary: String::new(),
                worktree: None,
                error: Some("e".into()),
            },
            WorkerOutcome {
                criterion: 1,
                summary: String::new(),
                worktree: None,
                error: Some("e".into()),
            },
            WorkerOutcome {
                criterion: 2,
                summary: String::new(),
                worktree: None,
                error: Some("e".into()),
            },
        ];
        let merges = merge_wave("s", &workers).await;
        assert_eq!(
            merges
                .iter()
                .map(MergeOutcome::criterion)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
        );
    }
}
