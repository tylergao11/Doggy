//! Doggy completion gate host adapter for `SessionActor`.
//!
//! Pure policy lives in `xai_doggy_orchestrator`. This module:
//! - snapshots open items from the plan's acceptance checklist
//! - drains goal updates (classifier / skeptic panel) before deciding
//! - requests acceptance itself once the implementer has ticked every Exec box
//! - injects Continue / Fix / Reapproach
//! - is the **only** path that may call `GoalTracker::complete()` after
//!   [`TaskDecision::TaskDone`]
//!
//! Acceptance is the existing goal verification panel (`update_goal` →
//! classifier). There is **no** separate Doggy audit subagent.
//!
//! # Completion is decided by acceptance criteria, never by todos
//!
//! Open items used to be the model's pending/in-progress todo list. That made
//! the model's own scratch pad a completion gate: it writes the list, it clears
//! the list, and a single stale entry blocks Done forever even after every
//! acceptance criterion passes. The model could imprison itself, and the gate
//! would keep saying "you still have work" with no way out.
//!
//! The plan's dual-column acceptance checklist is the contract instead —
//! written once, up front, and marked in two columns by two different parties:
//!
//! - `Exec` unticked → the *implementer* still has work → Continue.
//! - every `Exec` ticked, `Audit` not granted → ready for verification, and the
//!   gate requests it rather than waiting for the model to remember.
//! - every `Audit` granted → Done.
//!
//! Todos remain a useful scratch pad for the model; they simply have no vote.
//!
//! See `docs/编排/完成权.md`.

use super::*;
use xai_doggy_orchestrator::{
    AuditFinding, Injection, OpenItem, OpenItemsSnapshot, PauseReason, RoundActivity, RoundDelta,
    RoundEndView, RoundLedger, StallReason, TaskDecision, TaskMachine, VerificationOutcome,
    decide_after_round,
};

/// Criteria the implementer has not claimed done yet, as open items.
///
/// Deferred criteria are excluded: the ladder already gave up on them, and
/// re-listing them as open work is how a run gets asked to redo something it
/// deliberately abandoned.
fn unclaimed_criteria(criteria: &[crate::session::goal_tracker::CriterionView]) -> Vec<OpenItem> {
    criteria
        .iter()
        .filter(|c| !c.exec && !c.deferred)
        .map(|c| OpenItem {
            id: Some(c.number.to_string()),
            summary: format!("criterion {}: {}", c.number, c.text),
        })
        .collect()
}

/// Whether the harness should request acceptance on the model's behalf.
///
/// True exactly when the implementer has ticked every criterion still in play
/// and verification has granted none of them — that is, the work is claimed
/// complete and nobody has asked the panel to look at it. An empty checklist
/// is never ready: there is no contract to have satisfied, so the model
/// remains the only party that can claim completion.
fn acceptance_is_claimed(criteria: &[crate::session::goal_tracker::CriterionView]) -> bool {
    let mut live = criteria.iter().filter(|c| !c.deferred).peekable();
    live.peek().is_some()
        && live.all(|c| c.exec)
        && criteria.iter().all(|c| !c.audit)
}

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
    /// Three conditions, and the posture one is not redundant. Switching
    /// Shift+Tab back to Auto only clears `goal_posture`; it deliberately
    /// leaves a half-finished goal Active so the user can return to it. Without
    /// this check the chip reads "Auto" — the light, daily posture — while the
    /// heavy multi-round completion gate is still driving the turn underneath,
    /// which is exactly how a routine Auto prompt inherited Goal's round loop.
    ///
    /// Auto keeps single-round exit. The goal is not cancelled, just not driven.
    pub(crate) fn doggy_task_bound(&self) -> bool {
        self.goal_harness_enabled()
            && self.goal_posture.load(std::sync::atomic::Ordering::Relaxed)
            && self.goal_tracker.lock().status()
                == Some(crate::session::goal_tracker::GoalStatus::Active)
    }

    /// Snapshot open items for `decide_after_round`, from the plan's acceptance
    /// checklist. See the module docs for why todos have no vote.
    ///
    /// Explicit items = criteria the implementer has not ticked.
    /// `acceptance_pending` = goal still Active (not yet orchestrator-Done).
    pub(crate) async fn doggy_snapshot_open_items(&self) -> OpenItemsSnapshot {
        let items = self.doggy_unclaimed_criteria();
        let acceptance_pending = matches!(
            self.goal_tracker.lock().status(),
            Some(crate::session::goal_tracker::GoalStatus::Active)
        );
        OpenItemsSnapshot {
            items,
            acceptance_pending,
        }
    }

    /// Criteria the implementer has not claimed done, as open items.
    ///
    /// A plan that cannot be projected yields none — empty must not invent
    /// work, and `acceptance_pending` still blocks Done on its own.
    fn doggy_unclaimed_criteria(&self) -> Vec<OpenItem> {
        let mut tracker = self.goal_tracker.lock();
        tracker.force_refresh_criteria_view();
        match tracker.snapshot() {
            Some(o) => unclaimed_criteria(&o.criteria_view),
            None => Vec::new(),
        }
    }

    /// What the round just did, for the stall measure and the injected delta.
    ///
    /// Criteria come from the plan projection rather than the classifier
    /// verdict because they are what moves monotonically over a run: the
    /// implementer ticking Exec, verification granting Audit, or the ladder
    /// giving up. All three are progress; none can be faked by narrating.
    async fn doggy_round_activity(&self, tools_called: &[String]) -> RoundActivity {
        let (baseline, criteria) = {
            let mut tracker = self.goal_tracker.lock();
            // The implementer ticks Exec boxes by writing `plan.md` directly, so
            // the cached projection can be older than the round we are judging.
            tracker.force_refresh_criteria_view();
            match tracker.snapshot() {
                Some(o) => (
                    o.changes_baseline_commit.clone(),
                    o.criteria_view.clone(),
                ),
                None => (None, Vec::new()),
            }
        };
        // Ground truth, and the reason the measure is not a guess about intent:
        // it asks whether the repository moved, not whether the model looked
        // busy. `None` means the harness could not see — never "nothing
        // changed" (see `RoundActivity::changed_files`).
        let changed_files = crate::session::goal_classifier::evidence::changed_file_names(
            baseline.as_deref(),
            self.tool_context.cwd.as_path(),
        )
        .await;
        let pick = |f: fn(&crate::session::goal_tracker::CriterionView) -> bool| {
            criteria
                .iter()
                .filter(|c| f(c))
                .map(|c| c.number)
                .collect::<Vec<u32>>()
        };
        RoundActivity {
            changed_files,
            tools_called: tools_called.to_vec(),
            execed: pick(|c| c.exec),
            verified: pick(|c| c.audit),
            deferred: pick(|c| c.deferred),
        }
    }

    /// Ask the verification panel for acceptance once the implementer has
    /// ticked every Exec box, without waiting for the model to remember.
    ///
    /// The old exit was entirely model-initiated: `acceptance_pending` cleared
    /// only when the model called `update_goal(completed: true)`. A model that
    /// finished the work and simply did not say so left the run asking for
    /// another round forever — the gate could see every criterion was claimed
    /// and still had no way to act on it.
    ///
    /// The synthesized claim goes through the *same* drain the tool call uses,
    /// so the panel, the attempt cap, and the rejection path are unchanged. The
    /// harness is only supplying the sentence the model forgot; it is not
    /// granting anything.
    ///
    /// Returns `true` when a claim was submitted, so the caller can re-read the
    /// verdict before deciding.
    async fn doggy_request_acceptance_if_claimed(&self, ledger: &mut RoundLedger) -> bool {
        use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput;

        let claimed = {
            let mut tracker = self.goal_tracker.lock();
            tracker.force_refresh_criteria_view();
            match tracker.snapshot() {
                Some(o) if acceptance_is_claimed(&o.criteria_view) => Some(
                    o.criteria_view
                        .iter()
                        .filter(|c| c.exec)
                        .map(|c| c.number)
                        .collect::<Vec<u32>>(),
                ),
                _ => None,
            }
        };
        let Some(execed) = claimed else {
            return false;
        };
        // Once per set of finished criteria. A rejection leaves the Exec marks
        // ticked, so re-asking every round would spend the whole verification
        // attempt cap re-litigating the same claim — the fixed point this fix
        // exists to remove, one level up. New work ticks a new box, which is
        // new information and may be asked about again.
        if ledger.verification_already_requested_for(&execed) {
            return false;
        }
        ledger.mark_verification_requested(&execed);
        tracing::info!(
            session_id = %self.session_info.id.0,
            "doggy: every Exec is ticked; requesting acceptance without waiting for the model"
        );
        xai_grok_telemetry::unified_log::info(
            "doggy.acceptance.auto_requested",
            Some(self.session_info.id.0.as_ref()),
            None,
        );
        let (ack_tx, _ack_rx) = tokio::sync::oneshot::channel();
        let input = UpdateGoalInput {
            completed: Some(true),
            message: Some(
                "Requested by the completion gate: every Exec mark on the acceptance \
                 checklist is ticked."
                    .to_string(),
            ),
            blocked_reason: None,
        };
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        self.drain_goal_updates_with_extra(
            current_tokens,
            DrainPurpose::TurnEnd,
            vec![(input, ack_tx)],
        )
        .await;
        true
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

    /// Render the round delta as the opening paragraph of an injection.
    ///
    /// This is the part that keeps the gate from being a fixed point.
    /// `inject_goal_continuation_message` prunes the previous directive before
    /// pushing the next, so without this the model receives byte-identical
    /// input on every stalled round and — reasonably — produces byte-identical
    /// output. Naming the round number and what did or did not move makes each
    /// round's input distinct, and states "you changed nothing" outright
    /// instead of leaving the model to infer it from a restated goal.
    fn doggy_render_delta(delta: &RoundDelta) -> String {
        let mut lines = vec![format!("Round {} just ended.", delta.round)];
        if delta.moved_nothing() {
            lines.push(
                "Nothing changed: no tools were called and no acceptance criterion moved."
                    .to_string(),
            );
            return lines.join("\n");
        }
        // Workspace first: it is the only line the model cannot argue with,
        // and "no new files were touched" is the sentence a narrating round
        // most needs to read about itself.
        lines.push(format!("- workspace: {}", delta.workspace_line()));
        lines.push(format!("- tools used: {}", delta.tool_usage_line()));
        let listed = |ns: &[u32]| {
            ns.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !delta.newly_execed.is_empty() {
            lines.push(format!(
                "- you marked criteria {} done (awaiting verification)",
                listed(&delta.newly_execed)
            ));
        }
        if !delta.newly_verified.is_empty() {
            lines.push(format!(
                "- verification accepted criteria {}",
                listed(&delta.newly_verified)
            ));
        }
        if !delta.newly_deferred.is_empty() {
            lines.push(format!(
                "- criteria {} were recorded as blocked and will not be retried",
                listed(&delta.newly_deferred)
            ));
        }
        if delta.newly_execed.is_empty()
            && delta.newly_verified.is_empty()
            && delta.newly_deferred.is_empty()
        {
            lines.push("- no acceptance criterion moved".to_string());
        }
        lines.join("\n")
    }

    /// Render + inject a Doggy Continue/Fix/Reapproach message (single inject path).
    pub(crate) async fn doggy_inject(&self, injection: Injection) {
        let body = match &injection {
            Injection::Continue {
                open_summary,
                delta,
            } => format!(
                "Doggy completion gate — task is NOT done.\n\n\
                 {}\n\n\
                 Remaining work:\n{open_summary}\n\n\
                 Continue implementing. Completion is decided by the plan's acceptance \
                 checklist, not by your todo list: tick the Exec box for each criterion \
                 as you finish it. The gate requests verification on its own once every \
                 Exec is ticked.",
                Self::doggy_render_delta(delta),
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
            // model has already answered identically, so repeating it is the
            // behaviour being corrected. This one forbids the restating that is
            // being mistaken for work and names the concrete exits.
            Injection::Reapproach {
                repeats,
                reason,
                open_summary,
                delta,
            } => {
                let diagnosis = match reason {
                    StallReason::Idle => format!(
                        "you produced {repeats} round(s) without calling a single tool. \
                         Describing what you are about to do is not doing it"
                    ),
                    StallReason::Repeated => format!(
                        "the last {repeats} rounds were indistinguishable: same open \
                         criteria, same verification state, same tools. Restating the \
                         plan is not progress"
                    ),
                };
                format!(
                    "Doggy completion gate — {diagnosis}.\n\n\
                     {}\n\n\
                     Remaining work:\n{open_summary}\n\n\
                     Do exactly one of these in this round, starting with a tool call:\n\
                     1. Make a concrete change — write a file, run a command, fix a \
                     failing test — then tick that criterion's Exec box in the plan.\n\
                     2. If a criterion is already satisfied, tick its Exec box now; the \
                     gate requests verification itself once all of them are ticked.\n\
                     3. If it cannot be done in this environment, call \
                     update_goal(blocked_reason: ...) naming the blocker.\n\n\
                     Do not re-plan, do not re-summarise, and do not re-read files you \
                     have already read. Further rounds like this one end the run.",
                    Self::doggy_render_delta(delta),
                )
            }
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
        ledger: &mut RoundLedger,
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

        // Acceptance before measurement: the implementer may have finished the
        // checklist this round, and a run that is actually done must not be
        // scored as a stall on its way to being accepted.
        if self.doggy_request_acceptance_if_claimed(ledger).await && !self.doggy_task_bound() {
            // Verification accepted and something downstream already closed the
            // goal out; nothing left for this loop to drive.
            self.doggy_emit_completion_ui("done", &[]).await;
            return DoggyRoundAction::EndTurn;
        }

        let open_items = self.doggy_snapshot_open_items().await;
        let verification = self.doggy_verification_outcome();
        let activity = self.doggy_round_activity(tools_called).await;
        let (stall_level, delta) = ledger.observe(&open_items, &verification, &activity);
        let decision = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_items.clone(),
            user_cancel: false,
            budget_hit: false,
            verification,
            stall: stall_level,
            delta,
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
                "stall_reason": stall_level.reason().map(|r| r.kind()),
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
            reason: PauseReason::NoProgress { repeats, reason },
        } = decision
        {
            return self.doggy_cut_off_stalled_work(repeats, reason, ledger).await;
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
        reason: StallReason,
        ledger: &mut RoundLedger,
    ) -> DoggyRoundAction {
        let blocker = match reason {
            StallReason::Idle => format!(
                "{repeats} consecutive rounds ended without a single tool call — the run \
                 is narrating rather than implementing"
            ),
            StallReason::Repeated => format!(
                "{repeats} consecutive rounds left the open criteria, the verification \
                 verdict and the tools used identical — retrying this is not converging"
            ),
        };
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
        ledger.reset_streaks();
        self.doggy_emit_completion_ui("executing", &[]).await;
        self.inject_goal_continuation_message(format!(
            "Doggy completion gate — {repeats} rounds with no progress, so the harness \
             stopped retrying that work and recorded it as blocked.\n\n{}\n\n\
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::goal_tracker::CriterionView;

    /// `(number, exec, audit, deferred)`.
    fn criterion(number: u32, exec: bool, audit: bool, deferred: bool) -> CriterionView {
        CriterionView {
            number,
            text: format!("criterion {number} text"),
            exec,
            audit,
            depends_on: Vec::new(),
            write_scope: Vec::new(),
            wave: None,
            deferred,
        }
    }

    #[test]
    fn open_work_is_the_criteria_the_implementer_has_not_claimed() {
        let plan = [
            criterion(1, true, true, false),
            criterion(2, false, false, false),
            criterion(3, true, false, false),
        ];
        let open = unclaimed_criteria(&plan);
        assert_eq!(open.len(), 1, "only criterion 2 is still unclaimed");
        assert_eq!(open[0].id.as_deref(), Some("2"));
        assert!(open[0].summary.starts_with("criterion 2:"));
    }

    #[test]
    fn a_deferred_criterion_is_not_open_work() {
        // The ladder already gave up on it; listing it as open would ask the
        // run to redo work it deliberately abandoned, forever.
        let plan = [criterion(1, true, true, false), criterion(2, false, false, true)];
        assert!(unclaimed_criteria(&plan).is_empty());
    }

    #[test]
    fn a_stale_todo_can_no_longer_block_done() {
        // The regression this change exists for. Open work is now derived
        // solely from the plan, so nothing the model writes to its own todo
        // list can appear here — and a fully ticked plan reads as no open work
        // no matter what the todo list still says.
        let plan = [criterion(1, true, true, false), criterion(2, true, true, false)];
        assert!(unclaimed_criteria(&plan).is_empty());
    }

    #[test]
    fn acceptance_is_requested_once_every_exec_box_is_ticked() {
        // The other half of the old deadlock: the model finished the work and
        // never said so, and the harness had no way to speak for it.
        assert!(acceptance_is_claimed(&[
            criterion(1, true, false, false),
            criterion(2, true, false, false),
        ]));
    }

    #[test]
    fn acceptance_waits_while_any_criterion_is_unclaimed() {
        assert!(!acceptance_is_claimed(&[
            criterion(1, true, false, false),
            criterion(2, false, false, false),
        ]));
    }

    #[test]
    fn a_deferred_criterion_does_not_hold_acceptance_back() {
        assert!(acceptance_is_claimed(&[
            criterion(1, true, false, false),
            criterion(2, false, false, true),
        ]));
    }

    #[test]
    fn acceptance_is_not_re_requested_once_verification_has_granted_one() {
        // Otherwise every subsequent round would re-submit the same claim and
        // burn the panel's attempt cap on work already being judged.
        assert!(!acceptance_is_claimed(&[
            criterion(1, true, true, false),
            criterion(2, true, false, false),
        ]));
    }

    #[test]
    fn an_empty_checklist_is_never_claimed_for_the_model() {
        // No contract to have satisfied. Speaking for the model here would let
        // an unparseable plan auto-submit a completion it cannot support.
        assert!(!acceptance_is_claimed(&[]));
        assert!(!acceptance_is_claimed(&[criterion(1, false, false, true)]));
    }

    #[test]
    fn the_delta_states_plainly_when_a_round_moved_nothing() {
        // This sentence is the whole point of the delta: the observed loop was
        // a model re-reading an instruction that never changed. Saying "you
        // changed nothing" makes the input differ *and* names the failure.
        let rendered = SessionActor::doggy_render_delta(&RoundDelta {
            round: 4,
            ..RoundDelta::default()
        });
        assert!(rendered.contains("Round 4"));
        assert!(rendered.contains("Nothing changed"));
    }

    #[test]
    fn consecutive_deltas_never_render_identically() {
        // The fixed point in one assertion: the host prunes the previous
        // directive, so two rounds that render the same text feed the model
        // byte-identical input and get byte-identical output back.
        let render = |round| {
            SessionActor::doggy_render_delta(&RoundDelta {
                round,
                ..RoundDelta::default()
            })
        };
        assert_ne!(render(1), render(2));
    }

    #[test]
    fn the_delta_names_what_moved() {
        let rendered = SessionActor::doggy_render_delta(&RoundDelta {
            round: 2,
            tools_called: vec!["write_file".into(), "write_file".into(), "bash".into()],
            newly_execed: vec![3],
            newly_verified: vec![1, 2],
            ..RoundDelta::default()
        });
        assert!(rendered.contains("write_file \u{d7}2, bash"));
        assert!(rendered.contains("criteria 3"));
        assert!(rendered.contains("criteria 1, 2"));
        assert!(!rendered.contains("Nothing changed"));
    }

    #[test]
    fn a_round_that_used_tools_but_moved_no_criterion_says_so() {
        let rendered = SessionActor::doggy_render_delta(&RoundDelta {
            round: 7,
            tools_called: vec!["read_file".into()],
            ..RoundDelta::default()
        });
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("no acceptance criterion moved"));
    }
}
