//! Goal-harness support for `SessionActor`: reminder/directive templates,
//! the `update_goal` drain guards and acks, goal wrappers, and goal token
//! accounting. Complements the `goal` sibling (orchestration flows).

use super::*;

/// Number of consecutive non-completing goal-mode turns before the goal
/// auto-pauses with `GoalPauseReason::BackOff`. See `handle_turn_end`.
/// Compile-time constant for v1; remote tunability is a deferred follow-up.
pub(super) const GOAL_CONTINUATION_BACKOFF_THRESHOLD: u32 = 3;

/// Hard ceiling on Doggy rounds within a single prompt.
///
/// The stall measure in `xai_doggy_orchestrator::progress` is the real guard
/// and catches the common case — rounds that repeat verbatim — in six. This is
/// the backstop for the case it cannot see: a run that oscillates between two
/// or more distinguishable states forever, and a goal with no parseable
/// acceptance criteria, where a cut-off has nothing to defer against and so
/// cannot end the run on its own.
///
/// Sized to be unreachable by real work rather than to be tight. A goal that
/// needs a hundred model rounds in one prompt is not going to be rescued by the
/// hundred-and-first, and an unattended overnight run must have *some* number
/// it cannot exceed.
pub(super) const DOGGY_MAX_ROUNDS_PER_PROMPT: u32 = 100;

/// How `maybe_run_goal_planner` settled, after retries and the harness's own
/// fallback plan. Only the last two arms leave the goal without a contract,
/// and neither is something another spawn could fix.
enum PlannerResolution {
    /// A plan is on disk — written by the planner subagent or by the harness.
    Planned(std::path::PathBuf),
    /// The user cancelled the planner spawn.
    UserAborted,
    /// The plan path cannot be written at all.
    Unwritable,
}

impl DrainSource {
    /// Consume the source, returning `(input, Option<ack_tx>)`.
    /// `None` means the ack was already resolved.
    pub(super) fn into_parts(
        self,
    ) -> (
        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput,
        Option<
            tokio::sync::oneshot::Sender<
                xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
            >,
        >,
    ) {
        match self {
            Self::Pending(i) => (i, None),
            Self::Channel((i, ack)) => (i, Some(ack)),
        }
    }
}

/// Send an `UpdateGoalAck` only if the ack channel is live (channel
/// source). No-op for `Pending` source — its ack was already resolved.
pub(super) fn try_send_ack(
    ack_tx: Option<
        tokio::sync::oneshot::Sender<
            xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
        >,
    >,
    ack: xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
) {
    if let Some(tx) = ack_tx {
        send_ack(tx, ack);
    }
}

/// Resolved policy for the goal-achievement classifier. Single-shot
/// snapshot per drain so the disabled / enabled / cap-reached branches
/// see consistent values even if the underlying flag flips mid-loop.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GoalClassifierPolicy {
    /// Master kill-switch (env > local > remote > default(false)).
    pub enabled: bool,
    /// Maximum per-goal classifier runs before the goal auto-pauses.
    pub max_runs: u32,
}

impl NotAchievedSyntheticReason {
    /// Per-variant Markdown body for the synthetic details file. The
    /// exhaustive match forces a new variant to supply its own prose.
    pub(super) fn details_body(self, attempt: u32, max_runs: u32) -> String {
        match self {
            Self::ConcurrentInFlight => format!(
                "# Goal classifier — Not Achieved (synthetic)\n\n\
                 **Attempt:** {attempt} of {max_runs}\n\
                 **Reason:** ConcurrentInFlight\n\n\
                 The harness short-circuited this `update_goal(completed: true)` call \
                 because a classifier was already in flight for the previous completion \
                 attempt. The second attempt was accounted as Not Achieved (counting \
                 toward `classifier_max_runs`) to bound model retry spam.\n\n\
                 No sampler call was made; no token cost was incurred for this attempt.\n",
            ),
        }
    }

    /// One-line gaps gist inlined into the rejection nudge for the
    /// synthetic (no-sampler) `NotAchieved` path. Mirrors the bullet
    /// shape of `goal_classifier::build_gaps_summary` so the nudge
    /// reads uniformly regardless of which path produced it.
    pub(super) fn gaps_summary(self) -> String {
        match self {
            Self::ConcurrentInFlight => "- a verification was already running for the previous \
                 completion attempt; this duplicate `completed: true` was rejected without \
                 re-running the panel"
                .to_string(),
        }
    }
}

/// Maximum number of deferred `completed: true` commands buffered in
/// `pending_classifier_completions` between turn-ends. A pathological
/// model that emits many mid-turn completions otherwise grows the
/// queue unboundedly within a turn. On overflow the oldest entry is
/// dropped and a `FailClosed { PendingQueueFull }` event is emitted
/// so the cap is observable in telemetry.
pub(super) const GOAL_CLASSIFIER_PENDING_QUEUE_CAP: usize = 4;

/// Scope-guard that clears `goal_classifier_in_flight` on drop —
/// fires on normal return AND on unwind (panic) so a panic mid-fire
/// can never wedge the flag `true` for the lifetime of the actor.
/// The drain body acquires the guard immediately after a successful
/// `compare_exchange` on the flag and holds it through every `.await`
/// of the fire branch.
pub(super) struct InFlightGuard<'a> {
    pub(super) flag: &'a std::sync::atomic::AtomicBool,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Disarm-able tracker scope-guard: runs `on_drop` on scope exit AND when a
/// turn cancel drops an in-flight future mid-await (same drop-safety
/// contract as [`InFlightGuard`]), so partial verification / strategist
/// state can never outlive its run.
///
/// No `GoalUpdated` from `Drop`: the Ctrl+C cancel path emits its own
/// auto-pause update strictly after the turn future is dropped, so cleared
/// state reaches the pager on that emit. `Drop` re-locks the non-reentrant
/// tracker mutex — never let the guard drop while holding that lock.
pub(super) struct TrackerDropGuard<'a, F: FnOnce(&mut crate::session::goal_tracker::GoalTracker)> {
    tracker: &'a parking_lot::Mutex<crate::session::goal_tracker::GoalTracker>,
    on_drop: Option<F>,
}

impl<'a, F: FnOnce(&mut crate::session::goal_tracker::GoalTracker)> TrackerDropGuard<'a, F> {
    pub(super) fn new(
        tracker: &'a parking_lot::Mutex<crate::session::goal_tracker::GoalTracker>,
        on_drop: F,
    ) -> Self {
        Self {
            tracker,
            on_drop: Some(on_drop),
        }
    }

    /// Cancel the drop action — the guarded state was applied normally.
    pub(super) fn disarm(&mut self) {
        self.on_drop = None;
    }
}

impl<F: FnOnce(&mut crate::session::goal_tracker::GoalTracker)> Drop for TrackerDropGuard<'_, F> {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f(&mut self.tracker.lock());
        }
    }
}

/// Send an `UpdateGoalAck` on the tool's oneshot. Logs at `debug` if
/// the receiver was dropped (benign — tool future aborted).
pub(super) fn send_ack(
    ack_tx: tokio::sync::oneshot::Sender<
        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
    >,
    ack: xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
) {
    if ack_tx.send(ack).is_err() {
        tracing::debug!("update_goal ack receiver dropped before harness could respond");
    }
}

/// Result of [`SessionActor::resume_goal()`] (`/goal resume`).
pub(super) enum GoalResumeOutcome {
    /// Resume/nudge succeeded: run an inference turn seeded with `reminder`
    /// as the turn content; `user_msg` is the slash-command output.
    Inference { reminder: String, user_msg: String },
    /// Terminal/no-op (`AlreadyComplete` | `BudgetLimited` | `NoGoal` |
    /// missing goal state): print this message and end the turn.
    Message(String),
}

/// Result of deferring a blocker: what to tell the model, and whether the run
/// ended because nothing reachable was left.
pub(super) struct DeferralOutcome {
    pub run_ended: bool,
    pub ack: String,
}

/// Goal-only `<task_completion_discipline>` (Rules 1–4); `{TODO_TOOL}` from [`GoalToolNames`].
/// Template must end with `\n` so `{DISCIPLINE_BLOCK}TRACKING:` glues correctly.
pub(super) fn render_goal_task_discipline(names: &GoalToolNames) -> String {
    GOAL_TASK_DISCIPLINE_TEMPLATE.replace("{TODO_TOOL}", &names.todo)
}

/// Render the plan-aware reminder block. `Plan: <abs path>` renders
/// on its own column-0 line — a single line-delimited pointer the
/// model and any downstream consumer (debug log scraper, support
/// tooling) can extract reliably, so keep the format stable.
pub(super) fn render_goal_plan_block(plan_path: &std::path::Path, names: &GoalToolNames) -> String {
    debug_assert!(
        !plan_path.as_os_str().is_empty(),
        "render_goal_plan_block requires a non-empty plan_path; an \
         empty path renders a dangling `Plan:` line that the model \
         cannot follow",
    );
    // Column-0 single-line `Plan: <abs>` contract — see fn docs.
    GOAL_PLAN_BLOCK_TEMPLATE
        .replace("{PLAN_PATH}", &plan_path.display().to_string())
        .replace("{TODO_TOOL}", &names.todo)
}

/// Plan path for the goal-mode reminder, or `None` on the legacy path.
/// `Some` only when the planner is enabled (`GROK_GOAL_PLANNER`) and a plan
/// exists; disabled ⇒ `None` ⇒ legacy block (no dangling `Plan:` line).
/// All three render sites (`setup_goal`, `resume_goal`, continuation nudge)
/// route through this helper so the gate can't drift. Borrows, no alloc.
pub(super) fn goal_reminder_plan_path(
    planner_enabled: bool,
    orchestration: &crate::session::goal_tracker::GoalOrchestration,
) -> Option<&std::path::Path> {
    planner_enabled
        .then_some(orchestration.plan_file.as_deref())
        .flatten()
}

/// Worker rounds a refuted goal may run without re-firing verification
/// before the continuation directive escalates to a forceful "re-verify
/// now" block. A refuted weak model can otherwise churn indefinitely —
/// verification only fires when it calls `update_goal(completed: true)`.
/// Override with `GROK_GOAL_REVERIFY_AFTER` (floored at 1).
pub(crate) const GOAL_REVERIFY_AFTER_DEFAULT: u32 = 8;

/// Stable substring present in every rendered continuation directive
/// (from [`GOAL_CONTINUATION_DIRECTIVE_TEMPLATE`]) and in no other
/// reminder. Used to find and drop the prior turn's directive from
/// history so only the latest copy persists.
pub(super) const GOAL_CONTINUATION_SENTINEL: &str =
    "Goal NOT complete — continue working. Next step:";

/// Bail-specific preface substituted into the `{bail_preface}` slot of
/// [`GOAL_CONTINUATION_DIRECTIVE_TEMPLATE`] when the turn-final text
/// matched a [`goal_stop_detector`](super::goal_stop_detector) pattern
/// while pending todos remained. Names the apparent stop and the
/// outstanding work, then lets the unchanged generic body carry the
/// next-step / token / Rule-1/Rule-4 content. The generic flavor
/// substitutes the empty string for this slot.
pub(super) const GOAL_CONTINUATION_BAIL_PREFACE: &str = "You appear to be stopping or handing off, but the goal is NOT complete \
     and todos remain. Do not end the turn here — keep working.\n\n";

/// Render the shared goal-rules template with tool names and
/// site-specific blocks substituted in.
///
/// The template is the slim current form: verification is owned by
/// the harness (the adversarial skeptic panel in `goal_classifier.rs`),
/// so this body only carries TRACKING / WORKING / VERIFY / TEST
/// guidance and the `{GOAL_TOOL}` completion contract. No per-goal
/// verdict-file path is substituted — `{VERIFIER_ID}` no longer
/// appears in the template; `verifier_id` continues to anchor
/// harness-owned skeptic verdict files inside `goal_classifier.rs`.
///
/// `block_recap` and `goal_state` are inserted verbatim — pass an
/// empty string to omit either section. The trailing newline of the
/// template is preserved so callers can append their closing
/// directive ("Start now." / "Continue working now.") and the
/// closing `</system-reminder>` tag without extra glue.
///
/// `plan_path` `Some` folds the plan-aware preamble into the same block as
/// the discipline; `None` renders the no-plan block byte-for-byte unchanged.
///
/// Legacy artifacts (the deleted COMPLETION AUDIT / canonical
/// verifier blocks / `{VERIFIER_ID}` placeholder) are pinned absent
/// by `goal_rules_template_drops_all_legacy_verifier_artifacts`.
pub(super) fn render_goal_rules(
    objective: &str,
    names: &GoalToolNames,
    block_recap: &str,
    goal_state: &str,
    plan_path: Option<&std::path::Path>,
    scratch_dir: &str,
    scratch_ready: bool,
) -> String {
    let discipline = render_goal_task_discipline(names);
    let plan_block = match plan_path {
        Some(path) => render_goal_plan_block(path, names),
        None => String::new(),
    };
    GOAL_RULES_TEMPLATE
        .replace("{OBJECTIVE}", objective)
        .replace("{GOAL_TOOL}", &names.goal)
        .replace("{TASK_TOOL}", &names.task)
        .replace("{TODO_TOOL}", &names.todo)
        .replace("{PLAN_BLOCK}", &plan_block)
        .replace("{BLOCK_RECAP}", block_recap)
        .replace("{DISCIPLINE_BLOCK}", &discipline)
        .replace("{GOAL_STATE}", goal_state)
        // Ordered after `{OBJECTIVE}`, consistent with `{GOAL_TOOL}` etc.: a
        // literal `{SCRATCH_DIR}` in the objective WOULD be expanded here
        // (harmless, astronomically unlikely). The `{SCRATCH}` placeholder the
        // text references is a different token, left unreplaced.
        .replace("{SCRATCH_DIR}", scratch_dir)
        // Only claim the dir exists when the harness actually created it.
        .replace(
            "{SCRATCH_STATUS}",
            if scratch_ready {
                "The dir has been created for you."
            } else {
                "Create it with `mkdir -p` if it does not already exist."
            },
        )
}

/// Assemble a goal auto-pause message: a `headline` summarizing why the
/// goal paused, the grouped-by-classification blocker `pause_summary`
/// (already sanitized upstream), and the `details_path` pointer. The
/// summary block is dropped when empty so a degenerate pause stays
/// well-formed.
pub(super) fn format_goal_pause_message(
    headline: &str,
    pause_summary: &str,
    details_path: &str,
) -> String {
    // An empty `details_path` means the harness has no artifact to point at
    // (e.g. the synthetic-details write was skipped on a squatted scratch
    // root); omit the "See …" pointer rather than dangle a bare "See".
    let has_path = !details_path.trim().is_empty();
    match (pause_summary.trim().is_empty(), has_path) {
        (true, true) => format!("{headline} See {details_path}"),
        (true, false) => headline.to_string(),
        (false, true) => format!("{headline}\n{pause_summary}\nSee {details_path}"),
        (false, false) => format!("{headline}\n{pause_summary}"),
    }
}

/// Make `{` / `}` inert in a model-controlled directive-slot value: a
/// smuggled `{goal_tool}` etc. would otherwise be re-expanded by a
/// later `.replace` pass and spoof a harness slot. A zero-width space
/// inside each brace keeps legitimate braces (code spans, JSON)
/// visually intact while no `{placeholder}` token can match; borrowed
/// pass-through when brace-free, so clean slots cost no allocation.
fn neutralize_directive_braces(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(['{', '}']) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '{' => out.push_str("{\u{200b}"),
            '}' => out.push_str("\u{200b}}"),
            _ => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Full model-slot sanitization: brace neutralization
/// (anti-`{placeholder}` spoofing) plus reminder-tag neutralization
/// (anti-`</system-reminder>` frame escape). Applied uniformly at the
/// renderer so no slot's defense depends on its producer remembering an
/// axis; re-application to producer-neutralized values is a no-op, and
/// the tag pass only allocates when a frame-tag fragment is present.
fn neutralize_directive_slot(text: &str) -> std::borrow::Cow<'_, str> {
    use crate::session::goal_classifier::neutralize_reminder_tags;

    let out = neutralize_directive_braces(text);
    // Every tag the neutralizer breaks ends in one of these fragments.
    if out.contains("system-reminder>") || out.contains("goal-state>") {
        std::borrow::Cow::Owned(neutralize_reminder_tags(out.into_owned()))
    } else {
        out
    }
}

/// Render the per-turn directive continuation nudge.
///
/// Uses chained `.replace` calls rather than `format!` because the
/// template carries literal `{...}` examples inside backticks that
/// `format!` would force-escape. Placeholders are lowercase (see the
/// doc on [`GOAL_CONTINUATION_DIRECTIVE_TEMPLATE`]).
///
/// `objective` is load-bearing and guarded by `debug_assert!`: an
/// empty value renders an `Objective:` line the agent cannot resolve
/// to a goal. `next_step` is NOT guarded because the production
/// caller's `unwrap_or_else` already substitutes a non-empty fallback
/// (`Check your \`{todo_tool}\` list for next steps.`); guarding here
/// would only catch a future refactor that drops that fallback.
///
/// `plan_pointer` is inlined verbatim; `bail_preface`, `verifier_gaps`
/// and `next_step` pass through [`neutralize_directive_slot`] first but
/// are otherwise inlined as passed — pass [`GOAL_CONTINUATION_BAIL_PREFACE`] for
/// `bail_preface` when the stop-detector fired (and the empty string
/// otherwise); pass the empty string for `plan_pointer` when no plan is
/// available; pass [`render_verifier_gaps_block`]'s output for
/// `verifier_gaps` (empty string when the latest verdict carries no
/// gaps) so the freshest verifier findings render above the next-step
/// line; pass the caller's chosen fallback for `next_step` when the
/// plan yields no concrete step.
///
/// Model-controlled slots (`bail_preface`, `verifier_gaps`,
/// `strategist_note`, `next_step`) are sanitized via
/// [`neutralize_directive_slot`] before substitution — inert under any
/// `.replace` order and unable to close the surrounding
/// `<system-reminder>` frame. `objective` is user-authored and stays
/// verbatim. Pinned by
/// `render_goal_continuation_directive_order_dependent_substitution_pinned`.
#[allow(clippy::too_many_arguments)]
/// Name the criteria whose dependencies are satisfied, with the paths each may
/// write, or `""` when there is nothing useful to say.
///
/// Without this, the dependency table the planner filled in has no effect on
/// execution: the implementer sees a flat checklist and picks whatever it likes,
/// including criteria that cannot be finished yet because they extend work that
/// does not exist. Naming the ready set is what turns the contract into
/// scheduling — and naming the write scope alongside it is what makes the set
/// safe to work on concurrently.
///
/// Returns empty when every criterion is ready and none declares a scope: the
/// block would then say nothing the checklist does not already say, and an
/// unconditional block trains the model to skim past it.
pub(super) fn render_ready_wave_block(
    criteria: &[crate::session::goal_tracker::CriterionView],
) -> String {
    if criteria.is_empty() {
        return String::new();
    }
    // A dependency is satisfied once it is CLAIMED, not once it is accepted.
    // Waiting for acceptance would deadlock the goal: the audit panel only runs
    // after every criterion is claimed, so a dependent would wait for an audit
    // that is itself waiting for that dependent. A refutation later strips the
    // audit marks of everything built on the refuted criterion, which is what
    // keeps this from burying unverified work.
    let satisfied = |n: u32| {
        criteria
            .iter()
            .any(|c| c.number == n && (c.exec || c.audit))
    };
    let open = |c: &&crate::session::goal_tracker::CriterionView| !c.audit && !c.deferred;
    // Unclaimed AND unblocked: the criteria that are actually work right now.
    // A claimed-but-unaudited criterion is excluded on purpose — its Exec tick
    // says the implementer believes it is done, and listing it as "work these"
    // invites a rewrite of finished work while its audit is still pending.
    let ready: Vec<&crate::session::goal_tracker::CriterionView> = criteria
        .iter()
        .filter(|c| open(c) && !c.exec && c.depends_on.iter().copied().all(satisfied))
        .collect();
    let claimed = criteria.iter().filter(|c| open(c) && c.exec).count();
    if ready.is_empty() {
        // Nothing left to start. Saying so beats saying nothing when work is
        // merely awaiting its audit: silence here reads as "no instructions",
        // and the model's actual next move is to request verification.
        if claimed > 0 {
            return format!(
                "All remaining criteria ({claimed}) are implemented and waiting on \
                 independent audit — do not redo them; request verification instead.\n\n"
            );
        }
        // Every remaining criterion is blocked or given up on. The harness
        // decides what happens next (defer or finish), and inventing work here
        // would contradict it.
        return String::new();
    }
    let blocked = criteria
        .iter()
        .filter(open)
        .count()
        .saturating_sub(ready.len())
        .saturating_sub(claimed);
    if blocked == 0 && claimed == 0 && ready.iter().all(|c| c.write_scope.is_empty()) {
        return String::new();
    }
    let mut out = String::from("Ready now (dependencies met) — work these, in any order:\n");
    for c in &ready {
        out.push_str(&format!("- criterion {}: {}", c.number, c.text.trim()));
        if !c.write_scope.is_empty() {
            out.push_str(&format!(" — writes {}", c.write_scope.join(", ")));
        }
        out.push('\n');
    }
    if blocked > 0 {
        out.push_str(&format!(
            "{blocked} further criterion(s) wait on the above; do not start them yet.\n",
        ));
    }
    if claimed > 0 {
        out.push_str(&format!(
            "{claimed} criterion(s) are already implemented and waiting on audit; do not \
             redo them.\n",
        ));
    }
    out.push_str(
        "Stay inside the write scope of the criterion you are on — another criterion may own \
         the files outside it.\n\n",
    );
    out
}

impl super::SessionActor {
    /// Implement this round's ready criteria in parallel, one worker each, and
    /// return the report to hand the coordinating model — or `None` when the
    /// round stays serial and the caller should use the ready-wave block.
    ///
    /// Called from the continuation seam, so it runs BEFORE the coordinating
    /// model gets its next turn: by the time that model is prompted, the wave's
    /// work is already merged into the working tree and its Exec marks are set.
    /// The model's job for the round becomes reviewing and integrating what the
    /// workers produced, which is work only it can do — it holds the whole goal.
    ///
    /// Awaits the whole wave. That wait IS the parallel execution; returning
    /// early would hand the model a directive about work still being written
    /// underneath it.
    pub(super) async fn maybe_run_criterion_wave(
        &self,
        objective: &str,
        criteria: &[crate::session::goal_tracker::CriterionView],
    ) -> Option<String> {
        use crate::session::goal_fanout::{
            ChannelWorkerSpawner, FanoutDeclined, merge_wave, plan_wave, render_wave_report,
            repo_available, run_wave,
        };
        if self.goal_fanout_max <= 1 {
            return None;
        }
        let cwd = std::path::PathBuf::from(self.tool_context.cwd.as_str());
        let wave = match plan_wave(
            criteria,
            self.goal_fanout_max,
            repo_available(&cwd),
            self.goal_worktrees_disposed,
        ) {
            Ok(wave) => wave,
            Err(declined) => {
                // A configured-parallel goal that runs serially is worth one
                // line of telemetry: the reason is invisible from the outside,
                // and "why did fan-out not happen" is otherwise unanswerable.
                if !matches!(
                    declined,
                    FanoutDeclined::Disabled | FanoutDeclined::NotEnoughReady { .. }
                ) {
                    self.events
                        .emit(crate::session::events::Event::GoalFanoutDeclined {
                            reason: declined.as_const_str(),
                        });
                }
                return None;
            }
        };
        let event_tx = self.tool_context.subagent_event_tx.clone()?;
        // Tag workers with the live turn's prompt id so cancelling the turn
        // terminates them too, exactly as it does the skeptic panel. Without
        // this a cancelled goal leaves N workers writing into worktrees.
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let spawner = ChannelWorkerSpawner {
            event_tx,
            parent_session_id: self.session_id_string(),
            parent_prompt_id,
            cwd: Some(self.tool_context.cwd.as_str().to_owned()),
            role_model: None,
            role_agent_type: None,
        };
        let numbers: Vec<u32> = wave.iter().map(|c| c.number).collect();
        self.events
            .emit(crate::session::events::Event::GoalFanoutStarted {
                criteria: numbers.clone(),
            });
        let started = std::time::Instant::now();
        let workers = run_wave(&spawner, objective, &wave).await;
        let merges = merge_wave(&self.session_id_string(), &workers).await;
        let landed: Vec<u32> = merges
            .iter()
            .filter(|m| m.landed())
            .map(crate::session::goal_fanout::MergeOutcome::criterion)
            .collect();
        let plan = self.goal_tracker.lock().plan_path();
        // Exec marks are set by the parent, from the merge result — never by the
        // worker that wrote the code. A criterion that conflicted or whose
        // worker died stays unticked and remains open work.
        if !landed.is_empty()
            && let Err(e) = crate::session::goal_acceptance_checklist::set_exec_marks_for_criteria(
                &plan, &landed, true,
            )
        {
            tracing::warn!(error = %e, "criterion wave: could not set Exec marks");
        }
        self.record_observed_writes(&plan, &merges);
        self.events
            .emit(crate::session::events::Event::GoalFanoutFinished {
                criteria: numbers,
                landed: landed.clone(),
                latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        Some(render_wave_report(&workers, &merges))
    }

    /// Write what the wave actually touched back into the plan's write scopes.
    ///
    /// A merge conflict means the plan claimed two criteria were independent
    /// and they were not. Without this the same pair is scheduled together
    /// again next round and collides again, because nothing the wave learned
    /// reached the contract — the conflict was reported to the model as prose
    /// and forgotten.
    ///
    /// Append-only, so this can run unattended: it can only add paths to a
    /// declared scope, which costs parallelism and never grants any. See
    /// [`crate::session::goal_replan`] for what the amendment layer refuses.
    fn record_observed_writes(
        &self,
        plan: &std::path::Path,
        merges: &[crate::session::goal_fanout::MergeOutcome],
    ) {
        use crate::session::goal_replan::{amend_plan_on_disk, amendment_from_observed_writes};
        let amendment =
            amendment_from_observed_writes(&crate::session::goal_fanout::observed_writes(merges));
        if amendment.is_empty() {
            return;
        }
        match amend_plan_on_disk(plan, &amendment) {
            Ok(report) => self.emit_plan_amended(&report),
            Err(e) => tracing::warn!(error = %e, "criterion wave: could not amend the plan"),
        }
    }

    /// Turn audit findings that belong to no criterion into criteria.
    ///
    /// This is the other half of the feedback loop [`record_observed_writes`]
    /// starts. That one repairs a scope the plan got wrong; this one repairs a
    /// plan that is missing an item outright — the case the panel reports as a
    /// finding it cannot attribute to any criterion.
    ///
    /// An unattributed finding is not a nuisance to be tolerated. It is the
    /// contract admitting it does not cover the objective: the work has no
    /// criterion to be scheduled under, no write scope to be parallelised by,
    /// and no checklist row to be audited against, so every round pays a full
    /// audit-mark clear and re-verifies everything to chase a gap it cannot
    /// name. Naming it is what closes the loop.
    ///
    /// Fail-open and capped. The amendment is a proposal;
    /// [`crate::session::goal_replan::apply_amendment`] is what decides how
    /// much of it the contract accepts.
    pub(super) async fn amend_plan_for_unattributed_findings(
        &self,
        findings: &[crate::session::goal_tracker::ClassifierFinding],
    ) {
        use crate::session::goal_replan::{
            ChannelAmenderSpawner, GOAL_AMENDER_MAX_RUNS, GoalAmenderInputs, GoalAmenderOutcome,
            amend_plan_on_disk, run_goal_amender,
        };
        let unattributed: Vec<String> = findings
            .iter()
            .filter(|f| f.criterion.is_none())
            .map(|f| f.message.clone())
            .collect();
        if unattributed.is_empty() {
            return;
        }
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::debug!("goal amender: no subagent coordinator channel; skipping");
            return;
        };

        // Claim the run and read the inputs under ONE lock, then drop it before
        // the spawn await: two concurrent rejections must not both see budget
        // left and spawn two writers against the same plan.
        let claimed = {
            let mut tracker = self.goal_tracker.lock();
            if !tracker.claim_amender_run(GOAL_AMENDER_MAX_RUNS) {
                None
            } else {
                let plan_file = tracker.plan_path();
                tracker
                    .snapshot()
                    .map(|o| (o.objective.clone(), plan_file, o.verifier_id.clone()))
            }
        };
        let Some((objective, plan_file, verifier_id)) = claimed else {
            tracing::debug!("goal amender: no run claimed (budget spent or no goal); skipping");
            return;
        };
        if !plan_file.exists() {
            return;
        }

        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let task_tool_name = self.resolve_goal_tool_names().await.task;
        let tool_names = self.resolve_inherit_role_tool_names().await;
        let amendment_file = crate::session::goal_tracker::amendment_proposal_file(&verifier_id);
        let spawner = ChannelAmenderSpawner {
            event_tx,
            parent_session_id: self.session_id_string(),
            parent_prompt_id,
            cwd: Some(self.tool_context.cwd.as_str().to_owned()),
            trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
        };

        let started = std::time::Instant::now();
        let outcome = run_goal_amender(
            &spawner,
            GoalAmenderInputs {
                objective: &objective,
                plan_file: &plan_file,
                amendment_file: &amendment_file,
                unattributed: &unattributed,
                tool_names: &tool_names,
            },
        )
        .await;
        // Seal the amender's synthetic `task` pair into its own harness trace
        // turn, as every other goal role does.
        self.chat_state_handle.flush_harness_trace_turn();

        let proposed = match &outcome {
            GoalAmenderOutcome::Proposed(a) => a.appended.len(),
            _ => 0,
        };
        self.events
            .emit(crate::session::events::Event::GoalPlanAmenderRan {
                unattributed: unattributed.len(),
                proposed,
                failed_open: matches!(outcome, GoalAmenderOutcome::FailedOpen),
                latency_ms: started.elapsed().as_millis() as u64,
            });

        let GoalAmenderOutcome::Proposed(amendment) = outcome else {
            return;
        };
        match amend_plan_on_disk(&plan_file, &amendment) {
            Ok(report) => {
                let landed = report.changed();
                self.emit_plan_amended(&report);
                // The new rows must reach the criteria view, or the next wave
                // schedules from a plan the scheduler has not read.
                if landed {
                    self.goal_tracker.lock().force_refresh_criteria_view();
                }
            }
            Err(e) => tracing::warn!(error = %e, "goal amender: could not amend the plan"),
        }
    }

    /// Report an applied amendment, including what it refused. No-op when
    /// nothing reached the file — a fully-rejected amendment leaves the plan
    /// byte-identical, and the rejections are carried by the runs that did
    /// change something.
    fn emit_plan_amended(&self, report: &crate::session::goal_replan::AmendmentReport) {
        if !report.changed() {
            return;
        }
        self.events
            .emit(crate::session::events::Event::GoalPlanAmended {
                appended: report.appended.clone(),
                scopes_widened: report.scopes.iter().map(|(n, _)| *n).collect(),
                edges_added: report.edges.len(),
                rejected: report
                    .rejected
                    .iter()
                    .map(crate::session::goal_replan::Rejection::as_const_str)
                    .collect(),
            });
    }
}

pub(super) fn render_goal_continuation_directive(
    objective: &str,
    tokens: u64,
    elapsed: &str,
    bail_preface: &str,
    plan_pointer: &str,
    verifier_gaps: &str,
    strategist_note: &str,
    reverify_block: &str,
    ready_wave: &str,
    next_step: &str,
    todo_tool: &str,
    goal_tool: &str,
    scratch_dir: &str,
    scratch_ready: bool,
) -> String {
    debug_assert!(
        !objective.is_empty(),
        "render_goal_continuation_directive requires a non-empty objective; \
         an empty value leaves the Objective: line dangling in the nudge",
    );
    // Every model-controlled slot is sanitized (bail_preface is a
    // harness const today but sits in the same slot class).
    let bail_preface = neutralize_directive_slot(bail_preface);
    let verifier_gaps = neutralize_directive_slot(verifier_gaps);
    let strategist_note = neutralize_directive_slot(strategist_note);
    let next_step = neutralize_directive_slot(next_step);
    GOAL_CONTINUATION_DIRECTIVE_TEMPLATE
        .replace("{objective}", objective)
        .replace("{tokens}", &tokens.to_string())
        .replace("{elapsed}", elapsed)
        .replace("{bail_preface}", &bail_preface)
        .replace("{plan_pointer}", plan_pointer)
        .replace("{verifier_gaps}", &verifier_gaps)
        .replace("{reverify_block}", reverify_block)
        // Harness-derived from `plan.md` (criterion numbers, text, declared
        // scopes) — no model-controlled text, so it needs no neutralizing.
        .replace("{ready_wave}", ready_wave)
        .replace("{next_step}", &next_step)
        .replace("{todo_tool}", todo_tool)
        .replace("{goal_tool}", goal_tool)
        // `{SCRATCH}` is left literal — the model resolves it to this dir.
        .replace("{scratch_dir}", scratch_dir)
        // Only claim the dir exists when the harness actually created it.
        .replace(
            "{scratch_status}",
            if scratch_ready {
                "(created for you)"
            } else {
                "(create with `mkdir -p` if missing)"
            },
        )
        .replace("{strategist_note}", &strategist_note)
}

/// Render the `{reverify_block}` slot. Empty unless the goal has been
/// refuted at least once AND has run `>= threshold` rounds since the last
/// verification fired. Reframes the loop (passing verification is the ONLY
/// way to finish) and inlines the live count; the lead hardens past
/// `3 * threshold`.
pub(super) fn render_goal_reverify_block(
    rounds_since_verify: u32,
    refuted: bool,
    threshold: u32,
    goal_tool: &str,
) -> String {
    if !refuted || rounds_since_verify < threshold {
        return String::new();
    }
    let lead = if rounds_since_verify >= threshold.saturating_mul(3) {
        "STOP DRIFTING — RE-VERIFY NOW."
    } else {
        "Re-verify before continuing."
    };
    format!(
        "{lead} You have run {rounds_since_verify} rounds since your last \
         verification without calling `{goal_tool}(completed: true)`. The ONLY \
         way to finish this goal is to PASS verification — not to keep editing. \
         If the plan's `## Verification plan` steps now hold, call \
         `{goal_tool}(completed: true)` THIS round to re-trigger the skeptic \
         panel. If they do NOT, name the SINGLE concrete gap still blocking it \
         and fix exactly that — do not make cosmetic changes to look busy.\n\n"
    )
}

/// Render the strategist-note block for the continuation directive.
/// `recommendation` is the capped, model-authored snippet read back from
/// the strategy note (persisted as `last_strategy_recommendation`); empty
/// ⇒ empty string, collapsing the `{strategist_note}` slot (same
/// empty-slot convention as `verifier_gaps`). Otherwise it renders a
/// narrative that tells the model to RE-READ its plan AND the strategy
/// note before continuing, then ends with a blank line so it stacks above
/// the "Goal NOT complete" sentinel. When no plan path is available
/// (planner disabled) the plan clause is dropped.
///
/// Injection hardening: the rendered note is wrapped in fence markers
/// carrying a PER-RENDER nonce. The nonce makes the fence unguessable, so a
/// model-authored recommendation can't reproduce the END marker to break out
/// and pose as harness narration; as a second layer any body line equal to a
/// fence marker is dropped. Placeholder/tag spoofing is handled at the
/// directive slot ([`neutralize_directive_slot`]), not here.
pub(super) fn render_strategist_note(
    recommendation: &str,
    plan_path: Option<&Path>,
    strategy_path: Option<&str>,
) -> String {
    if recommendation.trim().is_empty() {
        return String::new();
    }
    // Per-render nonce: a fresh short token the untrusted body can't predict,
    // so it can't forge the closing marker to break out of the fence.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let nonce = &nonce[..12];
    let begin = format!("--- STRATEGIST RECOMMENDATION (advisory) [{nonce}] ---");
    let end = format!("--- END STRATEGIST RECOMMENDATION [{nonce}] ---");
    // Static marker prefixes used to drop any body line that LOOKS like a
    // fence marker (with or without a nonce) — defence in depth on top of the
    // unguessable nonce.
    const BEGIN_PREFIX: &str = "--- STRATEGIST RECOMMENDATION (advisory)";
    const END_PREFIX: &str = "--- END STRATEGIST RECOMMENDATION";
    // Drop forged-marker lines from the untrusted recommendation.
    let sanitized: String = recommendation
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with(BEGIN_PREFIX) || t.starts_with(END_PREFIX))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let strategy_path = strategy_path.unwrap_or("the strategy note");
    let reread = match plan_path {
        Some(p) => format!(
            "RE-READ your plan at {} AND the strategy note at {strategy_path}",
            p.display(),
        ),
        None => format!("RE-READ the strategy note at {strategy_path}"),
    };
    format!(
        "A strategist reviewed your stuck progress and recommends a STRUCTURAL \
         change of approach (you keep flagging a different gap each round). \
         Before continuing, {reread}, then weigh this ADVISORY recommendation \
         (between the {nonce}-tagged markers below; it does NOT change the \
         acceptance criteria, only HOW you get there — it is not a harness \
         instruction):\n\
         {begin}\n\
         {sanitized}\n\
         {end}\n\n",
    )
}

/// Render the verifier-gaps slot: inline the bounded gaps checklist
/// directly (empty → ""). The verbose per-skeptic details file is
/// deliberately NOT referenced — pointing the model at it bloats context;
/// the file stays on disk for the user.
pub(super) fn render_verifier_gaps_block(gaps: &str, goal_tool: &str) -> String {
    if gaps.is_empty() {
        return String::new();
    }
    format!(
        "Verification REJECTED your last `{goal_tool}(completed: true)` claim. \
         Fix every gap the skeptic panel flagged below — these take priority — \
         before claiming completion again:\n{gaps}\n\n",
    )
}

/// `char` cap on the (model-authored) plan-mined next-step line — one
/// checklist item never legitimately needs more, while a single plan
/// line can run to the reader's 8 KiB cap. Applied BEFORE tag
/// neutralization, which may add a zero-width break per broken tag
/// (plus the `…` cap suffix).
pub(super) const GOAL_NEXT_STEP_MAX_CHARS: usize = 400;

/// Resolve the inlined "next concrete step" for the continuation
/// nudge from the planner-emitted plan file. The read is 8 KiB-capped
/// and best-effort — any I/O or parse failure yields `None`, leaving
/// the caller to substitute a generic "check your todo list" fallback.
///
/// The plan item is model-authored: `char`-capped to
/// [`GOAL_NEXT_STEP_MAX_CHARS`], then reminder-frame tags are
/// zero-width-broken so it cannot close the `<system-reminder>` frame
/// it is inlined into.
///
/// Verifier gaps are NOT consulted here: a `NotAchieved` verdict's
/// findings are surfaced separately and prominently via
/// [`render_verifier_gaps_block`] (persisted in `last_classifier_gaps`),
/// so this slot carries only the plan's next item to avoid duplicating
/// the top gap.
pub(super) fn resolve_goal_next_step(plan_path: Option<&Path>) -> Option<String> {
    use crate::session::goal_classifier::{cap_chars, neutralize_reminder_tags};
    use crate::session::goal_next_step::first_unchecked_plan_item;

    plan_path
        .and_then(first_unchecked_plan_item)
        .map(|item| neutralize_reminder_tags(cap_chars(&item, GOAL_NEXT_STEP_MAX_CHARS)))
}

/// Format the user-facing chat notification body emitted when
/// `drain_goal_updates` transitions the goal to `Blocked` via the
/// `update_goal(blocked_reason: ...)` path. `reason` is the short
/// blocked_reason string; `detail` is the optional longer body from
/// the tool's `message` field. Kept as a free function so the format
/// is testable in isolation and is the single source of truth for the
/// notification text.
pub(super) fn format_blocked_chat_notification(reason: &str, detail: Option<&str>) -> String {
    let mut out = String::with_capacity(reason.len() + detail.map_or(0, str::len) + 96);
    out.push_str("Goal paused — verification blocked.\nReason: ");
    out.push_str(reason);
    out.push('\n');
    if let Some(detail) = detail {
        out.push_str(detail);
        out.push('\n');
    }
    out.push_str("\nType /goal resume to continue after you've addressed it.");
    out
}

/// Per-subagent token state; the goal-scoped marginal is
/// `last_cumulative_reported - resume_anchor_cumulative`.
///
/// Child reports carry the child's CONTEXT token total, so a child
/// compaction freezes the ratchet at its pre-compaction max until the
/// child's context regrows past it.
#[derive(Debug)]
pub(crate) struct SubagentTokenRecord {
    /// `None` for subagents spawned outside any active goal.
    pub goal_id: Option<String>,
    /// Parent's `last_cumulative_reported` at spawn; 0 for fresh spawns.
    pub resume_anchor_cumulative: u64,
    /// Monotonic high-water mark, ratcheted on `SubagentProgress` ticks
    /// and sealed by `SubagentFinished`.
    pub last_cumulative_reported: u64,
    /// Effective model id captured from `SubagentSpawned.model` at spawn
    /// time. Captured here (not at aggregation time) so attribution is
    /// pinned to the model the subagent actually ran on, even if the user
    /// switches the session model mid-goal. `None` (or empty) only when the
    /// wire field was absent; such records fold under the current model id
    /// as a best-effort fallback during aggregation.
    pub model: Option<String>,
    /// Set by `SubagentFinished`; later (stale or spoofed) progress ticks
    /// for the subagent are ignored so they can't move the ratchet.
    pub finished: bool,
}

impl SubagentTokenRecord {
    /// Goal-scoped marginal cost: `last_cumulative_reported -
    /// resume_anchor_cumulative`, saturating so a stale/out-of-order
    /// report below the anchor yields 0 instead of underflowing. Shared by
    /// the single-line total ([`SessionActor::goal_tokens`]) and the
    /// per-model breakdown ([`fold_tokens_by_model`]) so they can't drift.
    pub fn marginal(&self) -> u64 {
        self.last_cumulative_reported
            .saturating_sub(self.resume_anchor_cumulative)
    }
}

/// Fold subagent token records into a `model_id -> marginal_tokens`
/// breakdown for `goal_id`, sorted by tokens descending (ties broken by
/// model id for determinism). Marginal cost per record is
/// [`SubagentTokenRecord::marginal`]. Records whose `model` was absent or
/// empty/whitespace-only on the wire fold under `current_model_id`.
/// Zero-marginal records are skipped.
fn fold_tokens_by_model<'a>(
    records: impl IntoIterator<Item = &'a SubagentTokenRecord>,
    goal_id: &'a str,
    current_model_id: &'a str,
) -> Vec<(String, u64)> {
    let mut by_model: HashMap<&'a str, u64> = HashMap::new();
    for r in records {
        if r.goal_id.as_deref() != Some(goal_id) {
            continue;
        }
        let marginal = r.marginal();
        if marginal == 0 {
            continue;
        }
        // A missing OR empty/whitespace-only captured id folds under the
        // current model so we never create a blank-id bucket.
        let model = r
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(current_model_id);
        let entry = by_model.entry(model).or_insert(0);
        *entry = entry.saturating_add(marginal);
    }
    let mut out: Vec<(String, u64)> = by_model
        .into_iter()
        .map(|(m, t)| (m.to_owned(), t))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The block that turns the planner's dependency table into scheduling.
#[cfg(test)]
mod ready_wave_tests {
    use super::render_ready_wave_block;
    use crate::session::goal_tracker::CriterionView;

    fn view(
        number: u32,
        audit: bool,
        depends_on: Vec<u32>,
        write_scope: Vec<&str>,
    ) -> CriterionView {
        CriterionView {
            number,
            text: format!("do thing {number}"),
            exec: audit,
            audit,
            depends_on,
            write_scope: write_scope.into_iter().map(str::to_owned).collect(),
            wave: None,
            deferred: false,
        }
    }

    #[test]
    fn no_criteria_means_no_block() {
        assert!(render_ready_wave_block(&[]).is_empty());
    }

    #[test]
    fn a_flat_plan_with_no_scopes_adds_nothing_the_checklist_lacks() {
        let criteria = vec![
            view(1, false, vec![], vec![]),
            view(2, false, vec![], vec![]),
        ];
        assert!(
            render_ready_wave_block(&criteria).is_empty(),
            "an unconditional block trains the model to skim past it"
        );
    }

    #[test]
    fn blocked_criteria_are_named_as_off_limits() {
        let criteria = vec![
            view(1, false, vec![], vec![]),
            view(2, false, vec![1], vec![]),
            view(3, false, vec![1], vec![]),
        ];
        let block = render_ready_wave_block(&criteria);
        assert!(block.contains("criterion 1"), "{block}");
        assert!(
            !block.contains("criterion 2"),
            "a blocked criterion must not be offered as ready: {block}"
        );
        assert!(
            block.contains("2 further criterion(s) wait"),
            "the implementer needs to know work exists but is not startable: {block}"
        );
    }

    #[test]
    fn a_satisfied_dependency_releases_its_dependents() {
        let criteria = vec![
            view(1, true, vec![], vec![]),
            view(2, false, vec![1], vec!["src/cli.rs"]),
        ];
        let block = render_ready_wave_block(&criteria);
        assert!(block.contains("criterion 2"), "{block}");
        assert!(
            !block.contains("criterion 1"),
            "an accepted criterion is not work: {block}"
        );
        assert!(
            block.contains("writes src/cli.rs"),
            "the scope is what makes concurrent work safe: {block}"
        );
        assert!(!block.contains("further criterion"), "{block}");
    }

    #[test]
    fn a_claimed_dependency_releases_its_dependents_or_the_goal_deadlocks() {
        // Waiting for criterion 1's AUDIT here would wedge the run: the panel
        // does not run until every criterion is claimed, and criterion 2 cannot
        // be claimed while this block calls it off-limits.
        let mut criteria = vec![
            view(1, false, vec![], vec![]),
            view(2, false, vec![1], vec![]),
        ];
        criteria[0].exec = true;
        let block = render_ready_wave_block(&criteria);
        assert!(
            block.contains("criterion 2"),
            "a claimed dependency must release its dependents: {block}"
        );
        assert!(
            !block.contains("criterion 1"),
            "criterion 1 is claimed, so offering it again invites a rewrite: {block}"
        );
        assert!(
            block.contains("waiting on audit"),
            "the implementer must be told why criterion 1 is absent: {block}"
        );
    }

    #[test]
    fn all_work_claimed_asks_for_verification_instead_of_going_quiet() {
        let mut criteria = vec![
            view(1, false, vec![], vec![]),
            view(2, false, vec![], vec![]),
        ];
        for c in &mut criteria {
            c.exec = true;
        }
        let block = render_ready_wave_block(&criteria);
        assert!(block.contains("request verification"), "{block}");
        assert!(
            block.contains("do not redo them"),
            "silence here reads as 'no instructions' and invites a rewrite: {block}"
        );
    }

    #[test]
    fn deferred_work_is_never_offered() {
        let mut criteria = vec![
            view(1, false, vec![], vec!["a"]),
            view(2, false, vec![], vec!["b"]),
        ];
        criteria[1].deferred = true;
        let block = render_ready_wave_block(&criteria);
        assert!(block.contains("criterion 1"), "{block}");
        assert!(
            !block.contains("criterion 2"),
            "the run already gave up on it: {block}"
        );
        assert!(
            !block.contains("further criterion"),
            "deferred work is not 'waiting', it is abandoned: {block}"
        );
    }

    #[test]
    fn nothing_ready_says_nothing() {
        // Every open criterion is deferred: the harness decides what happens
        // next, and the directive must not contradict it by inventing work.
        let mut criteria = vec![view(1, false, vec![], vec!["a"])];
        criteria[0].deferred = true;
        assert!(render_ready_wave_block(&criteria).is_empty());
    }
}

#[cfg(test)]
mod fold_tokens_by_model_tests {
    use super::{SubagentTokenRecord, fold_tokens_by_model};

    fn rec(goal: Option<&str>, anchor: u64, last: u64, model: Option<&str>) -> SubagentTokenRecord {
        SubagentTokenRecord {
            goal_id: goal.map(str::to_owned),
            resume_anchor_cumulative: anchor,
            last_cumulative_reported: last,
            model: model.map(str::to_owned),
            finished: false,
        }
    }

    #[test]
    fn empty_records_yields_empty() {
        let records: Vec<SubagentTokenRecord> = Vec::new();
        assert!(fold_tokens_by_model(&records, "g1", "cur").is_empty());
    }

    #[test]
    fn mixed_models_sum_marginals_sorted_desc() {
        let records = vec![
            rec(Some("g1"), 0, 100, Some("grok-3")),
            rec(Some("g1"), 100, 500, Some("grok-4")), // marginal 400
            rec(Some("g1"), 0, 50, Some("grok-3")),    // grok-3 total 150
        ];
        let out = fold_tokens_by_model(&records, "g1", "cur");
        assert_eq!(
            out,
            vec![("grok-4".to_owned(), 400), ("grok-3".to_owned(), 150)]
        );
    }

    #[test]
    fn ties_break_by_model_id_ascending() {
        let records = vec![
            rec(Some("g1"), 0, 100, Some("zeta")),
            rec(Some("g1"), 0, 100, Some("alpha")),
        ];
        let out = fold_tokens_by_model(&records, "g1", "cur");
        assert_eq!(
            out,
            vec![("alpha".to_owned(), 100), ("zeta".to_owned(), 100)]
        );
    }

    #[test]
    fn none_model_folds_under_current() {
        let records = vec![rec(Some("g1"), 0, 100, None), rec(Some("g1"), 0, 200, None)];
        let out = fold_tokens_by_model(&records, "g1", "cur-model");
        assert_eq!(out, vec![("cur-model".to_owned(), 300)]);
    }

    #[test]
    fn single_distinct_model_collapses_to_one_entry() {
        let records = vec![
            rec(Some("g1"), 0, 100, Some("grok-4")),
            rec(Some("g1"), 0, 200, None), // folds under current = grok-4
        ];
        let out = fold_tokens_by_model(&records, "g1", "grok-4");
        assert_eq!(out, vec![("grok-4".to_owned(), 300)]);
    }

    #[test]
    fn other_goal_records_excluded() {
        let records = vec![
            rec(Some("g1"), 0, 100, Some("grok-4")),
            rec(Some("g2"), 0, 999, Some("grok-4")),
            rec(None, 0, 999, Some("grok-4")),
        ];
        let out = fold_tokens_by_model(&records, "g1", "cur");
        assert_eq!(out, vec![("grok-4".to_owned(), 100)]);
    }

    #[test]
    fn last_below_anchor_does_not_underflow() {
        let records = vec![rec(Some("g1"), 500, 100, Some("grok-4"))];
        // marginal saturates to 0 -> skipped as a zero-token entry.
        assert!(fold_tokens_by_model(&records, "g1", "cur").is_empty());
    }

    #[test]
    fn captured_model_survives_mid_goal_current_model_switch() {
        // A record captured `grok-4` at spawn keeps it even though the
        // current model at aggregation time is `grok-3`.
        let records = vec![rec(Some("g1"), 0, 100, Some("grok-4"))];
        let out = fold_tokens_by_model(&records, "g1", "grok-3");
        assert_eq!(out, vec![("grok-4".to_owned(), 100)]);
    }

    #[test]
    fn empty_or_whitespace_model_folds_under_current() {
        // `Some("")` and `Some("  ")` must NOT create a blank-id bucket;
        // they fold under the current model exactly like `None`.
        let records = vec![
            rec(Some("g1"), 0, 100, Some("")),
            rec(Some("g1"), 0, 200, Some("   ")),
            rec(Some("g1"), 0, 50, None),
        ];
        let out = fold_tokens_by_model(&records, "g1", "cur-model");
        assert_eq!(out, vec![("cur-model".to_owned(), 350)]);
    }

    #[test]
    fn empty_model_merges_with_current_model_bucket() {
        // An empty id folds into the SAME bucket as records that
        // explicitly captured the current model id.
        let records = vec![
            rec(Some("g1"), 0, 100, Some("grok-4")),
            rec(Some("g1"), 0, 200, Some("")),
        ];
        let out = fold_tokens_by_model(&records, "g1", "grok-4");
        assert_eq!(out, vec![("grok-4".to_owned(), 300)]);
    }
}

/// Resolved per-role `/goal` model selection, cached on the actor.
///
/// `Default` (every role `InheritCurrent`, empty skeptic pool) reproduces
/// today's behavior — `runtime_overrides.model = None` + the role's default
/// `subagent_type`. The kill-switch (`goal_use_current_model_only`) collapses
/// all three to `InheritCurrent` at resolution time, so consumers never need
/// to re-check it.
#[derive(Debug, Clone, Default)]
pub(crate) struct GoalRoleModelConfig {
    /// Planner role choice (single pair or inherit).
    pub(crate) planner: crate::agent::config::GoalRoleModelChoice,
    /// Strategist role choice (single pair or inherit).
    pub(crate) strategist: crate::agent::config::GoalRoleModelChoice,
    /// Ordered skeptic pool; `pool[0]` is skeptic-0's model, the rest are
    /// assigned round-robin by index. Empty ⇒ all skeptics inherit.
    pub(crate) skeptic_pool: Vec<crate::util::config::GoalRoleModel>,
}

/// `goal_enabled` AND `update_goal` registered in the session toolset.
/// Canonical pause message shared by the planner fail-closed path and the
/// session-load reconciler so the user sees one consistent reason.
pub(crate) fn planner_failure_pause_message() -> String {
    "Planning failed; resume with /goal to retry.".to_string()
}

pub(crate) fn goal_slash_and_harness_available(goal_enabled: bool, tool_names: &[String]) -> bool {
    use xai_grok_tools::implementations::grok_build::UPDATE_GOAL_TOOL_NAME;
    goal_enabled && tool_names.iter().any(|n| n == UPDATE_GOAL_TOOL_NAME)
}

/// Active `/goal` session with goal harness enabled (laziness, continuation, TodoGate goal arm).
pub(crate) fn laziness_injection_active(
    goal_harness_enabled: bool,
    goal_status: Option<crate::session::goal_tracker::GoalStatus>,
) -> bool {
    goal_harness_enabled && goal_status == Some(crate::session::goal_tracker::GoalStatus::Active)
}

impl SessionActor {
    pub(super) fn goal_harness_enabled(&self) -> bool {
        self.goal_harness_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True while a `/goal` autonomous run is actively driving the turn
    /// (goal harness enabled and the durable status is `Active`). Used to
    /// tag background tasks spawned during the goal turn as goal-turn-origin.
    pub(super) fn goal_loop_active(&self) -> bool {
        self.goal_harness_enabled()
            && self.goal_tracker.lock().status()
                == Some(crate::session::goal_tracker::GoalStatus::Active)
    }

    /// Tag `task_id`s the goal model spawned itself during the goal turn as
    /// goal-turn origin (see [`Self::goal_turn_task_ids`]). No-op when the goal
    /// loop isn't active.
    pub(super) fn record_goal_turn_task_ids(&self, task_ids: impl IntoIterator<Item = String>) {
        if !self.goal_loop_active() {
            return;
        }
        let mut set = self.goal_turn_task_ids.lock();
        set.extend(task_ids);
    }

    /// Tag `task_id`s reparented from a harness verifier/planner subagent as
    /// goal-turn origin. Gated on the goal harness being enabled — stable across
    /// status flips — NOT on `Active`, so a final-round skeptic exiting as the
    /// goal flips `Active → Blocked` is still suppressed. The caller already
    /// filters to harness-internal children (`surface_completion: false`).
    pub(super) fn record_reparented_goal_turn_task_ids(
        &self,
        task_ids: impl IntoIterator<Item = String>,
    ) {
        if !self.goal_harness_enabled() {
            return;
        }
        let mut set = self.goal_turn_task_ids.lock();
        set.extend(task_ids);
    }

    pub(super) fn sync_goal_harness_from_tools(&self, tool_names: &[String]) -> bool {
        let enabled = goal_slash_and_harness_available(self.goal_enabled, tool_names);
        self.goal_harness_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        enabled
    }

    pub(super) async fn refresh_goal_harness_enabled(&self) {
        let tool_names = self.registered_tool_names().await;
        self.sync_goal_harness_from_tools(&tool_names);
    }

    pub(super) async fn maybe_reconcile_active_goal_without_harness(&self) {
        if self
            .goal_harness_availability_reconciled
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let active = self.goal_tracker.lock().status()
            == Some(crate::session::goal_tracker::GoalStatus::Active);
        // A goal restored as Active means the session was left in the Goal
        // posture. `goal_posture` is in-memory only, so without this the
        // session comes back as Auto and the completion gate — which requires
        // the posture, so that an Auto prompt cannot inherit the goal loop —
        // would quietly stop driving a run the user expects to continue.
        if active && self.goal_harness_enabled() {
            self.goal_posture
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.enqueue_current_mode_update(acp::SessionModeId::new(
                xai_grok_tools::types::SessionMode::Goal.as_id(),
            ));
        }
        if self.goal_harness_enabled() {
            return;
        }
        if !active {
            return;
        }
        let _ = self
            .auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::User,
                "Goal paused: `update_goal` is not available in this session's toolset. \
                 Resume with /goal after the tool is registered."
                    .into(),
            )
            .await;
    }

    /// Load-time safety net: an Active goal restored with `plan_file == None`
    /// (legacy snapshot) gets a plan written for it rather than pausing —
    /// `maybe_run_goal_planner` always resolves to a plan on disk or a genuine
    /// dead end, so the restored goal resumes unattended.
    /// One-shot via `goal_plan_reconciled`; the mid-session retry path lives
    /// in `resume_goal`, not here.
    pub(super) async fn maybe_reconcile_active_goal_without_plan(&self) {
        if self
            .goal_plan_reconciled
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        if !self.goal_planner_enabled || !self.goal_harness_enabled() {
            return;
        }
        let objective = {
            let tracker = self.goal_tracker.lock();
            match tracker.snapshot() {
                Some(o)
                    if o.status == crate::session::goal_tracker::GoalStatus::Active
                        && o.plan_file.is_none() =>
                {
                    o.objective.clone()
                }
                _ => return,
            }
        };
        self.maybe_run_goal_planner(&objective).await;
    }

    /// Queue a synthetic prompt turn that puts the model back on the goal.
    ///
    /// This is how the harness drives itself: the run loop starts pending
    /// inputs as soon as the current completion is handled, so queueing here
    /// continues an unattended run without a user prompt. Mirrors the
    /// `SessionCommand::GoalSummaryTurn` handler, which builds the same item.
    ///
    /// Idempotent against the pending queue — a goal-origin input already
    /// waiting will drive the goal on its own, and a second copy would run two
    /// turns for one recovery.
    pub(super) async fn queue_goal_retry_turn(&self) {
        let mut state = self.state.lock().await;
        if state
            .pending_inputs
            .iter()
            .any(|i| matches!(i.origin, super::PromptOrigin::GoalSummary))
        {
            return;
        }
        let (respond_to, _) = tokio::sync::oneshot::channel();
        state.pending_inputs.push_back(InputItem {
            prompt_id: format!("goal-retry-{}", uuid::Uuid::now_v7()),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                "The previous turn failed for infrastructure reasons, not because the work \
                 was wrong. Continue the goal from where it stopped."
                    .to_string(),
            ))],
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            json_schema: None,
            origin: super::PromptOrigin::GoalSummary,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            queue_meta: None,
            send_now: false,
        });
    }

    /// Write [`fallback_plan_body`](crate::session::goal_planner::fallback_plan_body)
    /// to `plan_file`. Returns whether the plan is now on disk.
    ///
    /// The last line of defense for an unattended run: the goal has a plan the
    /// executor and the skeptic panel can read even though the planner
    /// subagent never delivered one.
    async fn write_fallback_plan(&self, plan_file: &std::path::Path, objective: &str) -> bool {
        let body = crate::session::goal_planner::fallback_plan_body(objective);
        if let Some(parent) = plan_file.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // Blocking write under the plan lock: this replaces the whole contract,
        // so it must not interleave with a checklist rewrite reading the old
        // body. It runs once, before any worker exists, so the block is short.
        let written = crate::session::goal_plan_write::with_plan_lock(plan_file, || {
            std::fs::write(plan_file, body)
        });
        match written {
            Ok(()) => {
                tracing::warn!(
                    plan_file = %plan_file.display(),
                    "goal planner: exhausted retries; harness wrote a single-criterion \
                     fallback plan so the goal can proceed unattended",
                );
                self.send_slash_command_output(
                    "Planner subagent failed; continuing with a harness-written plan that \
                     treats the objective as one acceptance criterion.",
                )
                .await;
                true
            }
            Err(err) => {
                tracing::error!(
                    plan_file = %plan_file.display(),
                    error = %err,
                    "goal planner: fallback plan write failed; goal cannot proceed",
                );
                false
            }
        }
    }

    /// Run the planner subagent for a goal that has no plan yet.
    /// Called from `setup_goal` (fresh goal) and from `resume_goal`
    /// (post-pause retry, honouring the canonical "Planning failed;
    /// resume with /goal to retry." message). No-op when the planner
    /// is disabled, when the coordinator is not plumbed (tests /
    /// compatible template), or when `plan_file` is already populated.
    /// On `FailClosed` the goal is paused with the canonical reason.
    pub(super) async fn maybe_run_goal_planner(&self, objective: &str) {
        if !self.goal_planner_enabled {
            return;
        }
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::debug!("goal planner: no subagent coordinator channel; skipping");
            return;
        };
        let plan_file = {
            let tracker = self.goal_tracker.lock();
            match tracker.snapshot() {
                Some(o) if o.plan_file.is_some() => {
                    tracing::debug!("goal planner: plan already present; skipping");
                    return;
                }
                Some(_) => tracker.plan_path(),
                None => return,
            }
        };

        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        // Fork owns history; fail-open stays OBJECTIVE-only (no last-assistant CONTEXT).
        let context = String::new();

        let task_tool_name = self.resolve_goal_tool_names().await.task;
        // Tag the planner with the goal-creation turn's prompt id so its
        // `subagent.json` / parent `subagents_spawned` ref link to this turn,
        // matching how model-spawned subagents attach to their parent.
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        // The planner is a verbatim mirror-child fork: its request prefix matches
        // the parent and the radix cache is per-model, so it must run on the
        // parent session model. Any configured planner role model is intentionally
        // ignored on this path (breadcrumb below for observability parity with the
        // strategist, which still resolves a role model).
        let role_override = crate::session::goal_planner::RoleSpawnOverride::default();
        if !matches!(
            self.goal_role_models.planner,
            crate::agent::config::GoalRoleModelChoice::InheritCurrent
        ) {
            tracing::info!(
                "goal planner: configured role model ignored — forced to parent model for verbatim-fork cache reuse"
            );
        }
        // Render planner-prompt tool-name placeholders from the PARENT's resolved
        // toolset (the exact tools the verbatim mirror sends) — honoring renames
        // and the Write->Edit fallback — instead of static defaults.
        let tool_names = self.resolve_inherit_role_tool_names().await;
        let inherit_tool_names = tool_names.clone();
        let spawner: std::sync::Arc<dyn crate::session::goal_planner::GoalPlannerSpawner> =
            std::sync::Arc::new(crate::session::goal_planner::ChannelSpawner {
                event_tx,
                parent_session_id: self.session_id_string(),
                parent_prompt_id,
                cwd: Some(self.tool_context.cwd.as_str().to_owned()),
                trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
                role_override,
                events: Some(self.events.writer()),
            });

        // Surface the "planning…" badge while the subagent runs. Cleared
        // unconditionally below so it can never get stuck on screen.
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        self.emit_goal_planning(current_tokens);

        // Unattended-run contract: a planner failure must not stop the goal.
        // Retry the spawn a bounded number of times, and if the planner still
        // produces nothing, write the harness's own plan (the objective as a
        // single criterion) and carry on. The only failure honored as final is
        // the user aborting, which is the user's decision, not a fault.
        let mut last_reason = None;
        let mut planned = None;
        for attempt in 1..=crate::session::goal_planner::GOAL_PLANNER_MAX_RUNS {
            if attempt > 1 {
                tokio::time::sleep(
                    crate::session::goal_planner::GOAL_PLANNER_RETRY_BACKOFF * (attempt - 1),
                )
                .await;
            }
            let outcome = crate::session::goal_planner::run_goal_planner(
                spawner.clone(),
                crate::session::goal_planner::GoalPlannerInputs {
                    objective,
                    context: &context,
                    plan_file: &plan_file,
                    attempt,
                    // Planner is forced to the parent model (no role override), so the
                    // effective role model is always the parent.
                    model_id: crate::session::goal_planner::effective_role_model_id(
                        None, &model_id,
                    ),
                    tool_names: &tool_names,
                    inherit_tool_names: &inherit_tool_names,
                },
                &|e| self.events.emit(e),
            )
            .await;

            // Seal the planner's synthetic `task` pair into its own harness
            // trace turn so it uploads as a sibling `turn_{N}` artifact (the
            // planner is represented by its own turn). Per attempt, so a
            // retried planner does not merge two spawns into one turn.
            self.chat_state_handle.flush_harness_trace_turn();

            match outcome {
                crate::session::goal_planner::GoalPlannerOutcome::Planned { plan_file, .. } => {
                    planned = Some(plan_file);
                    break;
                }
                crate::session::goal_planner::GoalPlannerOutcome::FailClosed { reason, .. } => {
                    last_reason = Some(reason);
                    if matches!(
                        reason,
                        crate::session::events::GoalPlannerFailClosedReason::Aborted
                    ) {
                        break;
                    }
                    tracing::warn!(
                        attempt,
                        max_runs = crate::session::goal_planner::GOAL_PLANNER_MAX_RUNS,
                        reason = reason.as_const_str(),
                        "goal planner: attempt failed; retrying",
                    );
                }
            }
        }

        let outcome = match planned {
            Some(p) => PlannerResolution::Planned(p),
            None if matches!(
                last_reason,
                Some(crate::session::events::GoalPlannerFailClosedReason::Aborted)
            ) =>
            {
                PlannerResolution::UserAborted
            }
            None => match self.write_fallback_plan(&plan_file, objective).await {
                true => PlannerResolution::Planned(plan_file.clone()),
                // Nothing left to try: the harness cannot write to the plan
                // path at all, so there is no contract for the executor or the
                // panel to read and the goal genuinely cannot proceed.
                false => PlannerResolution::Unwritable,
            },
        };

        match outcome {
            PlannerResolution::Planned(plan_file) => {
                // Record `plan_file`, then snapshot the planner's ORIGINAL plan
                // as the immutable baseline the verifier diffs later edits
                // against. Capture once: `maybe_run_goal_planner` only runs when
                // no plan exists yet, and the `is_none()` guard keeps a restart /
                // re-entry from overwriting it.
                let baseline_target = {
                    let mut tracker = self.goal_tracker.lock();
                    let src = tracker.plan_path();
                    let dst = tracker.plan_baseline_path();
                    let need_baseline = tracker
                        .snapshot()
                        .is_some_and(|o| o.plan_baseline_file.is_none());
                    if let Some(o) = tracker.snapshot_mut() {
                        o.plan_file = Some(plan_file);
                    }
                    need_baseline.then_some((src, dst))
                };
                if let Some((src, dst)) = baseline_target {
                    match tokio::fs::copy(&src, &dst).await {
                        Ok(_) => {
                            let mut tracker = self.goal_tracker.lock();
                            // `None` here means the goal ended concurrently during
                            // the copy; the orphaned baseline file is harmless
                            // (never referenced without a recorded path).
                            if let Some(o) = tracker.snapshot_mut() {
                                o.plan_baseline_file = Some(dst);
                            }
                        }
                        Err(err) => tracing::warn!(
                            error = %err,
                            src = %src.display(),
                            "goal planner: failed to snapshot plan baseline; \
                             PLAN_CHANGES will render (none)",
                        ),
                    }
                }
            }
            // Both remaining arms are user intent or a dead filesystem — the
            // two things retrying cannot fix.
            PlannerResolution::UserAborted | PlannerResolution::Unwritable => {
                let _ = self
                    .auto_pause_goal_if_active_with_message(
                        crate::session::goal_tracker::GoalPauseReason::User,
                        planner_failure_pause_message(),
                    )
                    .await;
            }
        }

        // Reset the latch on every exit path (success, fail-closed,
        // mid-run status change) and emit so the "planning…" badge turns
        // off. Unconditional here because `setup_goal` has no later emit;
        // no-op if the orchestration has since vanished.
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let mut tracker = self.goal_tracker.lock();
        if let Some(o) = tracker.snapshot_mut() {
            o.planning_in_flight = false;
        }
        self.goal_notify_sender()
            .emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
    }

    /// Run the stall-triggered strategist subagent (best-effort, fail-OPEN).
    /// Called from `apply_classifier_outcome`'s `NotAchieved` branch once the
    /// consecutive-failure streak hits a multiple of `goal_strategist_every`
    /// and neither the cap nor the stall paused the round. On success the
    /// recommendation + strategy-note path are persisted on the orchestration
    /// so the continuation directive can inline them. Any failure (no
    /// coordinator, spawn error, missing note) is logged and ignored — the
    /// goal keeps running, never pauses. No `goal_tracker` lock is held across
    /// the strategist `.await`.
    pub(super) async fn maybe_run_goal_strategist(&self, attempt: u32, consecutive_failures: u32) {
        // The claim granted the cap bonus up front; every exit that delivers
        // no restructure (early return, FailOpen, future dropped by a turn
        // cancel) must give it back.
        let mut bonus_guard = TrackerDropGuard::new(&self.goal_tracker, |t| {
            t.revoke_strategist_cap_bonus();
        });
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::debug!("goal strategist: no subagent coordinator channel; skipping");
            return;
        };

        // Assemble inputs under one scoped lock, then drop it before the spawn
        // await. `plan_path` / `strategy_path` are derived from the tracker
        // while we hold the lock (cheap path joins).
        let (objective, plan_file, strategy_file, verifier_id) = {
            let tracker = self.goal_tracker.lock();
            let Some(o) = tracker.snapshot() else {
                return;
            };
            (
                o.objective.clone(),
                tracker.plan_path(),
                tracker.strategy_path(),
                o.verifier_id.clone(),
            )
        };

        // The strategist reads the run's traces itself (transcript + verdict
        // history) instead of a pre-assembled gaps/diff packet.
        let session_traces_dir = crate::session::persistence::session_dir(&self.session_info);
        let scratch_root = crate::session::goal_tracker::goal_scratch_root(&verifier_id);

        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();

        let task_tool_name = self.resolve_goal_tool_names().await.task;
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        // Resolve the strategist role override: entitlement +
        // toolset capability gate, per-role fail-open. Borrows `event_tx`
        // for the describe round-trip; `event_tx` moves into the spawner.
        let (role_override, tool_names, inherit_tool_names) = self
            .resolve_goal_single_role_override(
                "strategist",
                &self.goal_role_models.strategist,
                goal::RoleCapability::Strategist,
                &event_tx,
            )
            .await;
        // Cloned for telemetry before `role_override` moves into the spawner.
        let strategist_model_override = role_override.model.clone();
        let spawner: std::sync::Arc<dyn crate::session::goal_strategist::GoalStrategistSpawner> =
            std::sync::Arc::new(crate::session::goal_strategist::ChannelSpawner {
                event_tx,
                parent_session_id: self.session_id_string(),
                parent_prompt_id,
                cwd: Some(self.tool_context.cwd.as_str().to_owned()),
                trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
                role_override,
                events: Some(self.events.writer()),
            });

        let outcome = crate::session::goal_strategist::run_goal_strategist(
            spawner,
            crate::session::goal_strategist::GoalStrategistInputs {
                objective: &objective,
                plan_file: &plan_file,
                strategy_file: &strategy_file,
                session_traces_dir: &session_traces_dir,
                scratch_root: &scratch_root,
                attempt,
                consecutive_failures,
                every: self.goal_strategist_every,
                model_id: crate::session::goal_planner::effective_role_model_id(
                    strategist_model_override.as_deref(),
                    &model_id,
                ),
                tool_names: &tool_names,
                inherit_tool_names: &inherit_tool_names,
            },
            &|e| self.events.emit(e),
        )
        .await;

        // Seal the strategist's synthetic `task` pair into its own harness
        // trace turn (sibling of the planner / skeptic turns). No-op when the
        // spawn recorded nothing.
        self.chat_state_handle.flush_harness_trace_turn();

        // Fail-OPEN: persist on success; any other exit leaves `bonus_guard`
        // armed (the runner already emitted telemetry + warning).
        if let crate::session::goal_strategist::GoalStrategistOutcome::Advised {
            strategy_file,
            recommendation,
            ..
        } = outcome
        {
            self.goal_tracker.lock().record_strategy_recommendation(
                strategy_file.to_string_lossy().into_owned(),
                recommendation,
            );
            bonus_guard.disarm();
        }
    }

    /// Generate the ONE closing user-facing summary after a goal is
    /// verified-achieved and surface it as the goal turn's final message.
    ///
    /// Best-effort / fail-OPEN: gated by `goal_summary_enabled`; any failure
    /// (disabled, no coordinator, spawn error, empty output) is logged /
    /// telemetry'd and skipped — goal completion is NEVER blocked, paused, or
    /// un-achieved (the goal is already Complete when this runs). Read-only:
    /// the spawn pins a read-only toolset and the prompt forbids edits. No
    /// `goal_tracker` lock is held across the summarizer `.await`.
    pub(super) async fn maybe_run_goal_summarizer(&self, attempt: u32) {
        if !self.goal_summary_enabled {
            return;
        }
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::debug!("goal summarizer: no subagent coordinator channel; skipping");
            return;
        };

        // Snapshot inputs under one scoped lock, dropped before the awaits.
        let (objective, plan_file, details_file) = {
            let tracker = self.goal_tracker.lock();
            let Some(o) = tracker.snapshot() else {
                return;
            };
            (
                o.objective.clone(),
                tracker.plan_path(),
                o.last_classifier_details_path.clone(),
            )
        };

        let session_traces_dir = crate::session::persistence::session_dir(&self.session_info);
        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let task_tool_name = self.resolve_goal_tool_names().await.task;
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        // The summarizer always inherits the current model (no per-role key);
        // its §7 prompt names the parent toolset's tools.
        let tool_names = self.resolve_inherit_role_tool_names().await;

        let spawner: std::sync::Arc<dyn crate::session::goal_summarizer::GoalSummarizerSpawner> =
            std::sync::Arc::new(crate::session::goal_summarizer::ChannelSpawner {
                event_tx,
                parent_session_id: self.session_id_string(),
                parent_prompt_id,
                cwd: Some(self.tool_context.cwd.as_str().to_owned()),
                trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
                events: Some(self.events.writer()),
            });

        let outcome = crate::session::goal_summarizer::run_goal_summarizer(
            spawner,
            crate::session::goal_summarizer::GoalSummarizerInputs {
                objective: &objective,
                plan_file: &plan_file,
                details_file: details_file.as_deref(),
                session_traces_dir: &session_traces_dir,
                attempt,
                model_id: &model_id,
                tool_names: &tool_names,
            },
            &|e| self.events.emit(e),
        )
        .await;

        // Seal the summarizer's synthetic `task` pair into its own harness
        // trace turn (sibling of the planner / skeptic / strategist turns).
        self.chat_state_handle.flush_harness_trace_turn();

        // Fail-OPEN: surface the summary on success; any other outcome already
        // emitted telemetry and is skipped. The chunk persists via
        // `updates.jsonl`, so resume/rewind keep it.
        if let crate::session::goal_summarizer::GoalSummarizerOutcome::Summarized {
            summary, ..
        } = outcome
        {
            // Bump the stream start so the summary chunk carries a fresh
            // `streamStartMs`; without it the client appends this closing
            // message to the model's last turn message instead of a new block.
            self.chat_state_handle
                .record_stream_start(chrono::Utc::now().timestamp_millis());
            self.send_slash_command_output(&summary).await;
        }
    }

    /// Build a [`GoalNotifySender`] for the goal orchestrator.
    ///
    /// The sender can emit notifications and push system reminders
    /// independently of `SessionActor` (used by the `spawn_local`
    /// orchestrator task).
    pub(crate) fn goal_notify_sender(&self) -> crate::session::goal_orchestrator::GoalNotifySender {
        crate::session::goal_orchestrator::GoalNotifySender::new(
            self.session_info.id.clone(),
            self.notifications.gateway.clone(),
            self.notifications.persistence_tx.clone(),
        )
    }

    /// Returns `(ratcheted_total, finished_subagent_marginal_sum)` for the
    /// active goal, or `(0, 0)` if no orchestration is loaded.
    ///
    /// The ratcheted total folds in EVERY goal-scoped subagent's marginal
    /// (finished + in-flight) so the displayed/enforced spend tracks live
    /// progress. The second value is the wire `finished_subagent_tokens`
    /// and intentionally folds ONLY sealed (`finished`) records: the pager
    /// adds its own live active-subagent sum on top of that field
    /// (`GoalDisplayState::live_tokens_used`), so including an in-flight
    /// subagent here would double-count it in the live display / budget bar.
    ///
    /// Parent usage is accumulated as a monotonic spend counter
    /// (`parent_tokens_spent`): only POSITIVE deltas of the session token
    /// total are added, anchored at `last_session_tokens_seen`. A
    /// compaction that shrinks the context total merely re-anchors, so
    /// the count can neither decrease nor freeze until context regrows
    /// past a prior peak. Best-effort sampling: growth fully consumed by
    /// a compaction between two calls is unobserved.
    ///
    /// Side-effect: advances the spend accumulator and ratchets
    /// `tokens_used_high_water` monotonically; idempotent under stable
    /// inputs.
    pub(crate) fn goal_tokens(&self, current_session_tokens: i64) -> (i64, i64) {
        let goal_id = {
            let tracker = self.goal_tracker.lock();
            match tracker.snapshot() {
                Some(o) => o.goal_id.clone(),
                None => return (0, 0),
            }
        };
        // `subagent_sum` folds every goal-scoped record (finished + in-flight)
        // into the ratcheted total; `finished_subagent_sum` folds only sealed
        // records and is what ships on the wire as `finished_subagent_tokens`.
        // Keeping them distinct preserves the pager contract — the pager sums
        // running subagents itself and adds them to `finished_subagent_tokens`,
        // so an in-flight marginal must NOT appear in the wire field.
        let (subagent_sum, finished_subagent_sum) = {
            let records = self.subagent_token_records.lock();
            records
                .values()
                .filter(|r| r.goal_id.as_deref() == Some(goal_id.as_str()))
                .fold((0i64, 0i64), |(all, finished), r| {
                    let d = r.marginal();
                    if i64::try_from(d).is_err() {
                        static WARNED: std::sync::Once = std::sync::Once::new();
                        WARNED.call_once(|| {
                            tracing::warn!(
                                marginal = d,
                                "subagent token marginal exceeds i64::MAX; saturating"
                            );
                        });
                    }
                    let d = i64::try_from(d).unwrap_or(i64::MAX);
                    let all = all.saturating_add(d);
                    let finished = if r.finished {
                        finished.saturating_add(d)
                    } else {
                        finished
                    };
                    (all, finished)
                })
        };
        let ratcheted = {
            let mut tracker = self.goal_tracker.lock();
            let o = tracker
                .snapshot_mut()
                .expect("orchestration vanished between locks on single-threaded actor");
            // Legacy snapshots carry no anchor; seed from the goal baseline.
            let last_seen = o.last_session_tokens_seen.unwrap_or(o.token_baseline);
            if current_session_tokens > last_seen {
                o.parent_tokens_spent = o
                    .parent_tokens_spent
                    .saturating_add(current_session_tokens - last_seen);
            }
            o.last_session_tokens_seen = Some(current_session_tokens);
            let raw = o.parent_tokens_spent.saturating_add(subagent_sum);
            o.tokens_used_high_water = o.tokens_used_high_water.max(raw);
            o.tokens_used_high_water
        };
        (ratcheted, finished_subagent_sum)
    }

    /// Thin wrapper around [`Self::goal_tokens`] returning only the
    /// ratcheted total. Inherits the high-water-mark side-effect.
    pub(crate) fn goal_tokens_used(&self, current_session_tokens: i64) -> i64 {
        self.goal_tokens(current_session_tokens).0
    }

    /// Per-model marginal-token breakdown for the active goal's LIVE
    /// active-subagent window, sorted by tokens descending. Empty when no
    /// goal is loaded. Records with no captured model fold under
    /// `current_model_id` (best-effort). See [`fold_tokens_by_model`] for the
    /// fold contract. Fed into `update_live_progress` by the
    /// `SubagentProgress` handler.
    ///
    /// Sealed (`finished`) records are excluded: this feeds
    /// `live_tokens_by_model`, which is rendered under the running subagent's
    /// live block, so spend from earlier finished subagents must not leak in.
    /// This is the per-model analogue of the finished/in-flight split in
    /// [`Self::goal_tokens`]; the ratcheted total path is unaffected.
    pub(crate) fn goal_tokens_by_model(&self, current_model_id: &str) -> Vec<(String, u64)> {
        let goal_id = match self.goal_tracker.lock().snapshot() {
            Some(o) => o.goal_id.clone(),
            None => return Vec::new(),
        };
        let records = self.subagent_token_records.lock();
        fold_tokens_by_model(
            records.values().filter(|r| !r.finished),
            &goal_id,
            current_model_id,
        )
    }

    /// Drop every subagent record attributed to the currently-loaded
    /// goal. Called before `tracker.complete()` so the registry
    /// doesn't accumulate orphaned records across goals.
    ///
    /// **Must not be called while already holding `goal_tracker`.**
    /// This method locks `goal_tracker` itself; `parking_lot::Mutex` is
    /// not reentrant, and a nested lock freezes the session LocalSet
    /// (cancel + prompts stop working). See `doggy_mark_task_done`.
    pub(super) fn prune_subagent_records_for_active_goal(&self) {
        let goal_id = match self.goal_tracker.lock().snapshot() {
            Some(o) => o.goal_id.clone(),
            None => return,
        };
        self.prune_subagent_records_for_goal(&goal_id);
    }

    /// Drop subagent records for a known goal id without touching
    /// `goal_tracker` (safe while the caller already holds that lock).
    pub(super) fn prune_subagent_records_for_goal(&self, goal_id: &str) {
        self.subagent_token_records
            .lock()
            .retain(|_, r| r.goal_id.as_deref() != Some(goal_id));
    }

    /// Push the tool-layer `GoalLoopActive` flag so per-tool-call bg-task /
    /// subagent completion reminders suppress themselves while the goal loop
    /// drives the turn. Mirrors the `CurrentPromptIdResource` push.
    ///
    /// Also mirrors the value into `tool_context.goal_loop_active_gate`, the
    /// shared `Arc<AtomicBool>` the notification bridge (bash auto-wake) and
    /// subagent spawn contexts (subagent auto-wake) read to suppress synthetic
    /// completion prompts mid-goal. Writing both from this one chokepoint keeps
    /// the gate from *persistently* drifting from the resource; the two writes
    /// are sequential (gate store, then async `update_resource`), so a transient
    /// window exists — benign, since those consumers read only the gate.
    pub(super) async fn set_goal_loop_active_resource(&self, active: bool) {
        self.tool_context
            .goal_loop_active_gate
            .store(active, std::sync::atomic::Ordering::Relaxed);
        self.agent
            .borrow()
            .tool_bridge()
            .update_resource(
                xai_grok_tools::implementations::grok_build::task::types::GoalLoopActive(active),
            )
            .await;
    }
}
