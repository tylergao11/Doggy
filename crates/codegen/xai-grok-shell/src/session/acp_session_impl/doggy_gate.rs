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
    AuditFinding, Injection, OpenItem, OpenItemsSnapshot, RoundEndView, TaskDecision, TaskMachine,
    VerificationOutcome, decide_after_round,
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
    pub(crate) async fn run_doggy_round_end(&self, machine: &mut TaskMachine) -> DoggyRoundAction {
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
        let decision = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_items.clone(),
            user_cancel: false,
            budget_hit: false,
            verification,
        });
        machine.apply(&decision);
        let decision_kind = match &decision {
            TaskDecision::RunAnotherRound { injection } => match injection {
                Injection::Fix { .. } => "fix",
                Injection::Continue { .. } => "continue",
            },
            TaskDecision::TaskDone => "done",
            TaskDecision::TaskPaused { .. } => "paused",
        };
        xai_grok_telemetry::unified_log::info(
            "doggy.after_round",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "decision": decision_kind,
                "open": open_items.summary_line(),
            })),
        );
        tracing::info!(
            session_id = %self.session_info.id.0,
            ?decision,
            open = %open_items.summary_line(),
            "doggy: after_round decision"
        );

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
}
