//! Doggy completion gate host adapter for `SessionActor`.
//!
//! Pure policy lives in `xai_doggy_orchestrator`. This module:
//! - snapshots open items from goal/todos
//! - drains goal updates (classifier / skeptic panel) before deciding
//! - injects Continue / Fix
//! - is the **only** path that may call `GoalTracker::complete()` after
//!   [`TaskDecision::TaskDone`]
//!
//! Acceptance is the existing goal verification panel (`update_goal` →
//! classifier). There is **no** separate Doggy audit subagent.
//!
//! See `docs/编排/完成权.md`.

use super::*;
use xai_doggy_orchestrator::{
    AuditFinding, Injection, OpenItem, OpenItemsSnapshot, PauseReason, RoundActivity, RoundEndView,
    StallTracker, TaskDecision, TaskMachine, VerificationOutcome, decide_after_round,
    round_fingerprint,
};

/// Action for the `handle_prompt` outer loop after one model round.
pub(crate) enum DoggyRoundAction {
    /// Injected a follow-up; run another `process_conversation_turn`.
    Continue,
    /// Task accepted or free-chat / non-bound exit — break with the round result.
    EndTurn,
}

impl SessionActor {
    /// Whether this session is currently under the Doggy task completion gate.
    ///
    /// Bound to an **Active** goal. Free chat (no active goal) keeps
    /// single-round exit. Goal work never treats model stop as task done.
    pub(crate) fn doggy_task_bound(&self) -> bool {
        self.goal_harness_enabled()
            && self.goal_tracker.lock().status()
                == Some(crate::session::goal_tracker::GoalStatus::Active)
    }

    /// Snapshot open items for `decide_after_round`.
    ///
    /// Explicit items = pending/in-progress todos.  
    /// `acceptance_pending` = goal still Active (not yet orchestrator-Done).
    pub(crate) async fn doggy_snapshot_open_items(&self) -> OpenItemsSnapshot {
        let items = self.doggy_pending_todo_items().await;
        let acceptance_pending = matches!(
            self.goal_tracker.lock().status(),
            Some(crate::session::goal_tracker::GoalStatus::Active)
        );
        OpenItemsSnapshot {
            items,
            acceptance_pending,
        }
    }

    /// Pending/InProgress todos as open items. Missing TodoState → empty
    /// (unlike stop-detector fail-open): empty must not invent work, but
    /// `acceptance_pending` still blocks Done until verification Achieved.
    async fn doggy_pending_todo_items(&self) -> Vec<OpenItem> {
        use crate::tools::todo::{TodoState, TodoStatus};
        use xai_grok_tools::types::resources::State;
        let bridge = self.tool_bridge_handle();
        let Some(state) = bridge.read_resource::<State<TodoState>>().await else {
            return Vec::new();
        };
        state
            .0
            .todo_items_with_ids()
            .filter(|(_id, item)| {
                matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress)
            })
            .map(|(id, item)| OpenItem {
                id: Some(id.clone()),
                summary: item.content.clone(),
            })
            .collect()
    }

    /// What the round just did, for the stall measure.
    ///
    /// The criteria counts come from the plan projection rather than the
    /// classifier verdict because they are the two things that move
    /// monotonically over a run: verification accepting one more criterion, or
    /// the ladder giving up on one. Either is progress; neither can be faked by
    /// narrating.
    fn doggy_round_activity(&self, tools_called: &[String]) -> RoundActivity {
        let mut tracker = self.goal_tracker.lock();
        // The implementer ticks Exec boxes by writing `plan.md` directly, so the
        // cached projection can be older than the round we are judging.
        tracker.force_refresh_criteria_view();
        let Some(o) = tracker.snapshot() else {
            return RoundActivity {
                tools_called: tools_called.to_vec(),
                ..RoundActivity::default()
            };
        };
        RoundActivity {
            tools_called: tools_called.to_vec(),
            verified_criteria: o.criteria_view.iter().filter(|c| c.audit).count(),
            deferred_criteria: o.deferred_criteria.len(),
        }
    }

    /// Turn-end drain so `update_goal(completed: true)` verification runs
    /// **before** open-item decisions. Must not mark Complete
    /// (see goal.rs Achieved / FailOpen paths).
    async fn doggy_drain_goal_updates(&self) {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        self.drain_goal_updates(current_tokens, DrainPurpose::TurnEnd)
            .await;
    }

    /// Map the latest classifier / skeptic panel verdict into a pure
    /// [`VerificationOutcome`] for `decide_after_round`.
    fn doggy_verification_outcome(&self) -> VerificationOutcome {
        use crate::session::goal_tracker::GoalClassifierVerdict;
        let tracker = self.goal_tracker.lock();
        let Some(o) = tracker.snapshot() else {
            return VerificationOutcome::Pending;
        };
        match o.last_classifier_verdict {
            Some(GoalClassifierVerdict::Achieved) => VerificationOutcome::Achieved,
            Some(GoalClassifierVerdict::NotAchieved) => {
                // Prefer the structured panel findings: one entry per rejected
                // criterion lets the Fix injection name exactly what to redo.
                // The prose summary is the fallback for a panel that attributed
                // nothing and for snapshots written before the field existed —
                // one opaque finding still blocks Done, it just cannot narrow.
                if !o.last_classifier_findings.is_empty() {
                    return VerificationOutcome::Rejected {
                        findings: o
                            .last_classifier_findings
                            .iter()
                            .map(|f| AuditFinding {
                                severity: Some("error".into()),
                                criterion: f.criterion,
                                message: f.message.clone(),
                            })
                            .collect(),
                    };
                }
                let message = o
                    .last_classifier_gaps
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        "Verification rejected the completion claim; fix gaps and re-verify."
                            .to_string()
                    });
                VerificationOutcome::Rejected {
                    findings: vec![AuditFinding {
                        severity: Some("error".into()),
                        criterion: None,
                        message,
                    }],
                }
            }
            None => VerificationOutcome::Pending,
        }
    }

    /// Render + inject a Doggy Continue/Fix message (single inject path).
    pub(crate) async fn doggy_inject(&self, injection: Injection) {
        let body = match &injection {
            Injection::Continue { open_summary } => format!(
                "Doggy completion gate — task is NOT done.\n\n\
                 Remaining work:\n{open_summary}\n\n\
                 Continue implementing. Do not stop until open work is finished and \
                 acceptance has been requested via update_goal(completed: true). \
                 The orchestrator decides completion after verification accepts."
            ),
            Injection::Fix { findings } => {
                let list = if findings.is_empty() {
                    "- (no detailed findings; re-check the goal and workspace diff)".to_string()
                } else {
                    findings
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let sev = f
                                .severity
                                .as_deref()
                                .map(|s| format!("[{s}] "))
                                .unwrap_or_default();
                            format!("{}. {sev}{}", i + 1, f.message)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                format!(
                    "Doggy verification REJECTED — task is NOT done.\n\n\
                     Findings to fix:\n{list}\n\n\
                     Address every finding, then call update_goal(completed: true) again. \
                     Completion is only granted after verification Achieved."
                )
            }
            // Deliberately not a louder Continue: the Continue text is what the
            // model has already answered identically N times, so repeating it
            // is the behaviour being corrected. This one forbids the restating
            // that is being mistaken for work and names the three exits.
            Injection::Reapproach {
                repeats,
                open_summary,
            } => format!(
                "Doggy completion gate — the last {repeats} rounds were indistinguishable: \
                 same open work, same verification state, same tools. Restating the plan \
                 is not progress.\n\n\
                 Remaining work:\n{open_summary}\n\n\
                 Do exactly one of these in this round:\n\
                 1. Make a concrete change — write a file, run a command, fix a failing \
                 test — and report what actually changed.\n\
                 2. If the objective is already met, call update_goal(completed: true) so \
                 verification can accept it.\n\
                 3. If it cannot be done in this environment, call \
                 update_goal(blocked_reason: ...) naming the blocker.\n\n\
                 Do not re-plan, do not re-summarise, and do not re-read files you have \
                 already read. Further identical rounds end this run."
            ),
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            kind = injection.kind(),
            "doggy: injecting completion-gate follow-up"
        );
        self.inject_goal_continuation_message(body).await;
    }

    /// Sole path that marks the goal Complete after orchestrator TaskDone.
    async fn doggy_mark_task_done(&self) {
        let sid = self.session_info.id.0.as_ref();
        xai_grok_telemetry::unified_log::info("doggy.mark_done.enter", Some(sid), None);
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        // IMPORTANT: prune BEFORE taking `goal_tracker` for `complete()`.
        // `prune_subagent_records_for_active_goal` itself locks `goal_tracker`
        // (parking_lot::Mutex is NOT reentrant). Calling it while already
        // holding the lock deadlocks the entire session LocalSet:
        // completion_phase stays "checking", cancel never processes, and new
        // prompts sit behind `turn_running` forever. Observed repeatedly after
        // Achieved → TaskDone (e.g. Luoxia sessions 2026-07-30).
        self.prune_subagent_records_for_active_goal();
        // State-only Complete under the mutex; rescue + remove_dir_all run
        // AFTER the lock is released on a blocking pool. Holding the lock
        // across Windows FS (antivirus / locked files) freezes cancel the
        // same way the old re-entrant prune deadlock did.
        let deferred_fs = {
            let mut tracker = self.goal_tracker.lock();
            if tracker.status() == Some(crate::session::goal_tracker::GoalStatus::Active) {
                let (applied, job) = tracker.complete_defer_scratch_cleanup();
                if applied {
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                }
                job
            } else {
                None
            }
        };
        // Surface Done to the UI immediately after in-memory Complete so a
        // stuck scratch rescue/delete cannot leave the chip on "checking".
        let attempt = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|o| o.classifier_runs_attempted)
            .unwrap_or(0);
        self.doggy_emit_completion_ui("done", &[]).await;
        xai_grok_telemetry::unified_log::info(
            "doggy.mark_done.state_done",
            Some(sid),
            Some(serde_json::json!({ "attempt": attempt, "has_fs_job": deferred_fs.is_some() })),
        );
        if let Some(job) = deferred_fs {
            // Optimistic durable path (deterministic from src filename) so the
            // UI "See details" link does not keep pointing into a dir we are
            // about to delete. Do NOT await the blocking job: a stuck Windows
            // `remove_dir_all` / antivirus lock must not freeze cancel.
            if let Some(src) = job.details_src.as_ref()
                && let Some(name) = src.file_name()
            {
                let dest = job.goal_dir.join(name);
                self.goal_tracker.lock().apply_rescued_details_path(dest);
            }
            tokio::task::spawn_blocking(move || {
                let _ = job.run();
            });
        }
        // Closing summarizer only after true Done (was previously on Achieved).
        self.maybe_run_goal_summarizer(attempt).await;
        xai_grok_telemetry::unified_log::info(
            "doggy.mark_done.exit",
            Some(sid),
            Some(serde_json::json!({ "attempt": attempt })),
        );
        tracing::info!(
            session_id = %self.session_info.id.0,
            "doggy: TaskDone — goal marked complete by orchestrator"
        );
    }

    /// Push Doggy completion_phase + findings to the pager via `GoalUpdated`.
    async fn doggy_emit_completion_ui(
        &self,
        phase: &str,
        findings: &[xai_doggy_orchestrator::AuditFinding],
    ) {
        use crate::extensions::notification::{GoalCompletionFinding, SessionUpdate};
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let mut update = {
            let tracker = self.goal_tracker.lock();
            let Some(o) = tracker.snapshot() else {
                return;
            };
            crate::session::goal_orchestrator::build_goal_updated(o, tokens_used, finished_marginal)
        };
        if let SessionUpdate::GoalUpdated {
            ref mut completion_phase,
            ref mut completion_findings,
            ..
        } = update
        {
            *completion_phase = Some(phase.to_string());
            *completion_findings = findings
                .iter()
                .map(|f| GoalCompletionFinding {
                    severity: f.severity.clone(),
                    message: f.message.clone(),
                })
                .collect();
        }
        self.goal_notify_sender().send_update(update);
    }

    /// After a successful model round under an Active goal: decide Continue /
    /// Done / Pause and perform host side effects.
    ///
    /// **Sole production completion authority** for bound tasks.
    ///
    /// `stall` and `tools_called` carry the termination measure: the completion
    /// rules alone can ask for another round forever (see
    /// [`xai_doggy_orchestrator::progress`]), and this is the only loop that
    /// applies them.
    pub(crate) async fn run_doggy_round_end(
        &self,
        machine: &mut TaskMachine,
        stall: &mut StallTracker,
        tools_called: &[String],
    ) -> DoggyRoundAction {
        // Process deferred completed:true / progress before deciding so the
        // classifier verdict is available as VerificationOutcome.
        self.doggy_drain_goal_updates().await;
        self.doggy_emit_completion_ui("checking", &[]).await;

        // Token budget → TaskPaused (not Done). Same chokepoint as
        // handle_turn_end / prepare_goal_continuation.
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        if self.enforce_goal_token_budget(current_tokens).await {
            machine.apply(&TaskDecision::TaskPaused {
                reason: xai_doggy_orchestrator::PauseReason::BudgetExhausted,
            });
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        }

        // Goal may have been paused by drain (blocked, …).
        if !self.doggy_task_bound() {
            machine.apply(&TaskDecision::TaskPaused {
                reason: xai_doggy_orchestrator::PauseReason::InfraError,
            });
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        }

        let open_items = self.doggy_snapshot_open_items().await;
        let verification = self.doggy_verification_outcome();
        let activity = self.doggy_round_activity(tools_called);
        let stall_level = stall.observe(round_fingerprint(&open_items, &verification, &activity));
        let decision = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_items.clone(),
            user_cancel: false,
            budget_hit: false,
            verification,
            stall: stall_level,
        });
        machine.apply(&decision);
        let decision_kind = match &decision {
            TaskDecision::RunAnotherRound { injection } => injection.kind(),
            TaskDecision::TaskDone => "done",
            TaskDecision::TaskPaused { .. } => "paused",
        };
        xai_grok_telemetry::unified_log::info(
            "doggy.after_round",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "decision": decision_kind,
                "open": open_items.summary_line(),
                "stall": stall_level.kind(),
                "stall_repeats": stall_level.repeats(),
            })),
        );
        tracing::info!(
            session_id = %self.session_info.id.0,
            ?decision,
            open = %open_items.summary_line(),
            stall = stall_level.kind(),
            "doggy: after_round decision"
        );

        if let TaskDecision::TaskPaused {
            reason: PauseReason::NoProgress { repeats },
        } = decision
        {
            return self.doggy_cut_off_stalled_work(repeats, stall).await;
        }

        match decision {
            TaskDecision::RunAnotherRound { injection } => {
                let findings = match &injection {
                    Injection::Fix { findings } => findings.as_slice(),
                    _ => &[],
                };
                if !findings.is_empty() {
                    self.doggy_emit_completion_ui("fixing", findings).await;
                } else {
                    self.doggy_emit_completion_ui("executing", &[]).await;
                }
                self.doggy_inject(injection).await;
                machine.mark_executing();
                DoggyRoundAction::Continue
            }
            TaskDecision::TaskDone => {
                self.doggy_mark_task_done().await;
                DoggyRoundAction::EndTurn
            }
            TaskDecision::TaskPaused { .. } => {
                self.doggy_emit_completion_ui("paused", &[]).await;
                DoggyRoundAction::EndTurn
            }
        }
    }

    /// Stop retrying work that `repeats` identical rounds failed to move.
    ///
    /// This does **not** pause the goal directly. `GoalPauseReason::NoProgress`
    /// is barred from halting an unattended run for good reason: "this round
    /// achieved nothing" is a signal to escalate, not to sit idle until a human
    /// returns. So the cut-off hands the blocker to the deferral ladder, which
    /// owns the only legitimate stop — nothing reachable remains — and reports
    /// the deferral list when it takes it.
    ///
    /// That also supplies the termination argument for the round loop: every
    /// cut-off records a deferral, an unattributed deferral blocks every
    /// criterion, so a goal with a plan can be cut off at most once before the
    /// run ends. The absolute round cap in `handle_prompt` covers the remaining
    /// case of a goal with no parseable criteria, where nothing can be deferred
    /// against.
    async fn doggy_cut_off_stalled_work(
        &self,
        repeats: u32,
        stall: &mut StallTracker,
    ) -> DoggyRoundAction {
        let blocker = format!(
            "{repeats} consecutive rounds left the open work, the verification verdict and \
             the tools used identical — retrying this is not converging"
        );
        tracing::warn!(
            session_id = %self.session_info.id.0,
            repeats,
            "doggy: cutting off a round loop that stopped making progress"
        );
        xai_grok_telemetry::unified_log::info(
            "doggy.stall.cut_off",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({ "repeats": repeats })),
        );
        let Some(outcome) = self.defer_blocker_or_end_run(blocker).await else {
            // No Active goal to defer against — the loop has no business
            // continuing either way.
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        };
        if outcome.run_ended {
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        }
        // Criteria are still reachable. The deferral changed the state the
        // streak was measured against, so judging the next round against it
        // would cut off work that never got a chance to run.
        stall.reset();
        self.doggy_emit_completion_ui("executing", &[]).await;
        self.inject_goal_continuation_message(format!(
            "Doggy completion gate — {repeats} identical rounds, so the harness stopped \
             retrying that work and recorded it as blocked.\n\n{}\n\n\
             Pick up a different criterion. Repeating the previous approach will be cut \
             off again.",
            outcome.ack
        ))
        .await;
        DoggyRoundAction::Continue
    }

    /// Last-resort stop when a prompt has run
    /// [`DOGGY_MAX_ROUNDS_PER_PROMPT`] rounds without the goal being accepted.
    ///
    /// Reached only by a loop the stall measure cannot see: one that keeps
    /// changing something observable without ever converging. Reports through
    /// the same deferral ladder so the user gets a reason, and says so plainly
    /// when there was nothing to defer against — a run that stopped this way
    /// must not look like one that finished.
    pub(crate) async fn doggy_end_run_at_round_cap(&self, rounds: u32) {
        tracing::warn!(
            session_id = %self.session_info.id.0,
            rounds,
            "doggy: prompt hit the round cap without acceptance; ending the turn"
        );
        xai_grok_telemetry::unified_log::info(
            "doggy.round_cap",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({ "rounds": rounds })),
        );
        let ended = self
            .defer_blocker_or_end_run(format!(
                "ran {rounds} rounds in a single prompt without the goal being accepted"
            ))
            .await
            .is_some_and(|o| o.run_ended);
        if !ended {
            self.send_slash_command_output(&format!(
                "Goal stopped after {rounds} rounds in one prompt without acceptance. \
                 Nothing could be recorded as blocked, which usually means the plan has no \
                 parseable acceptance criteria — check plan.md before resuming."
            ))
            .await;
        }
        self.doggy_emit_completion_ui("paused", &[]).await;
    }
}
