//! Doggy completion gate host adapter for `SessionActor`.
//!
//! Pure policy lives in `xai_doggy_orchestrator`. This module:
//! - snapshots open items from goal/todos
//! - injects Continue / Fix
//! - runs the audit step (Phase C: dedicated read-only audit subagent;
//!   structured JSON `pass` / `findings`; never fail-open to pass)
//! - is the **only** path that may call `GoalTracker::complete()` after
//!   [`TaskDecision::TaskDone`]
//!
//! See `docs/编排/完成权.md`.

use super::*;
use xai_doggy_orchestrator::{
    AuditEndView, AuditFinding, AuditVerdict, Injection, OpenItem, OpenItemsSnapshot, RoundEndView,
    TaskDecision, TaskMachine, decide_after_audit, decide_after_round, parse_audit_agent_output,
};

const DOGGY_AUDIT_PROMPT_TEMPLATE: &str = include_str!("../templates/doggy_audit_prompt.md");
const DOGGY_AUDIT_SUBAGENT_TYPE: &str = "general-purpose";
const DOGGY_AUDIT_DESCRIPTION: &str = "doggy task auditor";

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
    /// Phase B: bound to an **Active** goal. Free chat (no active goal) keeps
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
    /// `acceptance_pending` still blocks Done until audit pass.
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
    /// **before** open-item / audit decisions. Must not mark Complete
    /// (see goal.rs Achieved / FailOpen paths).
    async fn doggy_drain_goal_updates(&self) {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        self.drain_goal_updates(current_tokens, DrainPurpose::TurnEnd)
            .await;
    }

    /// Phase C audit: spawn a **read-only** Doggy auditor subagent, parse
    /// structured JSON, never fail-open to pass.
    ///
    /// Precedence:
    /// 1. Drain `update_goal` so prior skeptic gaps (if any) feed the prompt.
    /// 2. Spawn the auditor (independent of model self-claim / Achieved).
    /// 3. On spawn/parse failure: return **fail** (optionally carrying prior
    ///    NotAchieved gaps) — never synthetic pass.
    pub(crate) async fn doggy_run_audit(&self) -> AuditVerdict {
        self.doggy_drain_goal_updates().await;

        match self.doggy_spawn_audit_agent().await {
            Ok(text) => match parse_audit_agent_output(&text) {
                Ok(verdict) => {
                    tracing::info!(
                        session_id = %self.session_info.id.0,
                        pass = verdict.pass,
                        findings = verdict.findings.len(),
                        "doggy: audit agent returned structured verdict"
                    );
                    verdict
                }
                Err(err) => {
                    tracing::warn!(
                        session_id = %self.session_info.id.0,
                        error = %err,
                        "doggy: audit agent output parse failed"
                    );
                    self.doggy_audit_fail_closed(format!("Audit agent output unusable: {err}"))
                }
            },
            Err(err) => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    error = %err,
                    "doggy: audit agent spawn failed"
                );
                self.doggy_audit_fail_closed(format!("Audit agent spawn failed: {err}"))
            }
        }
    }

    /// Fail closed: never pass. Prefer prior NotAchieved gaps when present so
    /// the implementer still gets concrete Fix text if the auditor could not run.
    fn doggy_audit_fail_closed(&self, reason: String) -> AuditVerdict {
        use crate::session::goal_tracker::GoalClassifierVerdict;
        let gaps = {
            let tracker = self.goal_tracker.lock();
            tracker.snapshot().and_then(|o| {
                if o.last_classifier_verdict == Some(GoalClassifierVerdict::NotAchieved) {
                    o.last_classifier_gaps.clone()
                } else {
                    None
                }
            })
        };
        let mut findings = vec![AuditFinding {
            severity: Some("error".into()),
            message: reason,
        }];
        if let Some(gaps) = gaps {
            findings.push(AuditFinding {
                severity: Some("error".into()),
                message: format!("Prior verification gaps (still open):\n{gaps}"),
            });
        }
        AuditVerdict::failed(findings)
    }

    /// Build the auditor prompt from live goal + chat context.
    async fn doggy_build_audit_prompt(&self) -> Result<String, String> {
        let (objective, plan_file, prior_gaps, verifier_id) = {
            let tracker = self.goal_tracker.lock();
            let o = tracker
                .snapshot()
                .ok_or_else(|| "no active goal orchestration".to_string())?;
            (
                o.objective.clone(),
                o.plan_file.clone(),
                o.last_classifier_gaps.clone(),
                o.verifier_id.clone(),
            )
        };
        let final_response = {
            let items = self.chat_state_handle.get_conversation().await;
            crate::session::goal_classifier::evidence::extract_final_response(&items)
                .unwrap_or_else(|| "(no assistant summary available)".into())
        };
        let plan_block = match plan_file {
            Some(p) => format!("Plan file path (read it): {}", p.display()),
            None => "(no plan file — judge OBJECTIVE alone)".to_string(),
        };
        let prior = prior_gaps.unwrap_or_else(|| "(none — first audit for this cycle)".into());
        let _ = verifier_id; // reserved for future scratch-scoped audit artifacts
        Ok(DOGGY_AUDIT_PROMPT_TEMPLATE
            .replace("{OBJECTIVE}", &objective)
            .replace("{PLAN_FILE}", &plan_block)
            .replace("{PRIOR_GAPS}", &prior)
            .replace("{FINAL_RESPONSE}", &final_response))
    }

    /// Spawn a synchronous (foreground) general-purpose subagent in
    /// **read-only** capability mode and return its terminal text.
    async fn doggy_spawn_audit_agent(&self) -> Result<String, String> {
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentEvent, SubagentRequest, SubagentRuntimeOverrides,
        };
        use xai_tool_types::SubagentCapabilityMode;

        let prompt = self.doggy_build_audit_prompt().await?;
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            return Err("subagent coordinator channel not available".into());
        };
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let id = format!("doggy-audit-{}", uuid::Uuid::now_v7());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: id.clone(),
            prompt,
            description: DOGGY_AUDIT_DESCRIPTION.to_string(),
            subagent_type: DOGGY_AUDIT_SUBAGENT_TYPE.to_string(),
            parent_session_id: self.session_info.id.0.to_string(),
            parent_prompt_id,
            resume_from: None,
            cwd: Some(self.session_info.cwd.clone()),
            runtime_overrides: SubagentRuntimeOverrides {
                capability_mode: Some(SubagentCapabilityMode::ReadOnly),
                ..Default::default()
            },
            run_in_background: false,
            surface_completion: false,
            fork_context: false,
            result_tx,
        };
        if event_tx
            .send(SubagentEvent::Spawn(Box::new(request)))
            .is_err()
        {
            return Err("subagent coordinator channel closed".into());
        }
        tracing::info!(
            session_id = %self.session_info.id.0,
            audit_id = %id,
            "doggy: spawned audit subagent"
        );
        let result = result_rx
            .await
            .map_err(|_| "audit subagent result channel dropped".to_string())?;
        if !result.success {
            let message = result
                .error
                .unwrap_or_else(|| "unknown audit subagent error".to_string());
            if result.cancelled {
                return Err(format!("audit subagent cancelled: {message}"));
            }
            return Err(message);
        }
        Ok(result.output.to_string())
    }

    /// Render + inject a Doggy Continue/Fix message (single inject path).
    pub(crate) async fn doggy_inject(&self, injection: Injection) {
        let body = match &injection {
            Injection::Continue { open_summary } => format!(
                "Doggy completion gate — task is NOT done.\n\n\
                 Remaining work:\n{open_summary}\n\n\
                 Continue implementing. Do not stop until open work is finished and \
                 acceptance has been requested. The orchestrator decides completion, not you."
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
                    "Doggy audit FAILED — task is NOT done.\n\n\
                     Findings to fix:\n{list}\n\n\
                     Address every finding, then continue. Completion is only granted after a \
                     later audit pass."
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
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        {
            let mut tracker = self.goal_tracker.lock();
            if tracker.status() == Some(crate::session::goal_tracker::GoalStatus::Active) {
                self.prune_subagent_records_for_active_goal();
                tracker.complete();
                let notify = self.goal_notify_sender();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
            }
        }
        // Closing summarizer only after true Done (was previously on Achieved).
        let attempt = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|o| o.classifier_runs_attempted)
            .unwrap_or(0);
        self.maybe_run_goal_summarizer(attempt).await;
        self.doggy_emit_completion_ui("done", &[]).await;
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
            crate::session::goal_orchestrator::build_goal_updated(
                o,
                tokens_used,
                finished_marginal,
            )
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
    /// Audit / Done / Pause and perform host side effects.
    ///
    /// **Sole production completion authority** for bound tasks (replaces
    /// `run_goal_round_end` + legacy `maybe_queue_goal_continuation` on the
    /// success path).
    pub(crate) async fn run_doggy_round_end(
        &self,
        machine: &mut TaskMachine,
    ) -> DoggyRoundAction {
        // Process deferred completed:true / progress before deciding.
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
        let decision = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_items.clone(),
            user_cancel: false,
            budget_hit: false,
        });
        machine.apply(&decision);
        tracing::info!(
            session_id = %self.session_info.id.0,
            ?decision,
            open = %open_items.summary_line(),
            "doggy: after_round decision"
        );

        match decision {
            TaskDecision::RunAnotherRound { injection } => {
                self.doggy_inject(injection).await;
                machine.mark_executing();
                self.doggy_emit_completion_ui("executing", &[]).await;
                DoggyRoundAction::Continue
            }
            TaskDecision::SpawnAudit => self.doggy_after_spawn_audit(machine).await,
            TaskDecision::TaskDone => {
                // decide_after_round must never return Done; belt-and-suspenders.
                tracing::error!("doggy: after_round returned TaskDone (invariant violation)");
                DoggyRoundAction::EndTurn
            }
            TaskDecision::TaskPaused { .. } => {
                self.doggy_emit_completion_ui("paused", &[]).await;
                DoggyRoundAction::EndTurn
            }
        }
    }

    async fn doggy_after_spawn_audit(&self, machine: &mut TaskMachine) -> DoggyRoundAction {
        self.doggy_emit_completion_ui("auditing", &[]).await;

        // Budget can trip during a long audit spawn.
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        if self.enforce_goal_token_budget(current_tokens).await {
            machine.apply(&TaskDecision::TaskPaused {
                reason: xai_doggy_orchestrator::PauseReason::BudgetExhausted,
            });
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        }
        if !self.doggy_task_bound() {
            machine.apply(&TaskDecision::TaskPaused {
                reason: xai_doggy_orchestrator::PauseReason::InfraError,
            });
            self.doggy_emit_completion_ui("paused", &[]).await;
            return DoggyRoundAction::EndTurn;
        }

        let verdict = self.doggy_run_audit().await;
        // User cancel during audit surfaces as spawn failure → fail verdict →
        // Fix inject, or channel drop. Turn-level cancel aborts handle_prompt
        // before we get here (round not Completed).

        let open_items = self.doggy_snapshot_open_items().await;
        let decision = decide_after_audit(&AuditEndView {
            verdict: verdict.clone(),
            open_items,
            user_cancel: false,
            budget_hit: false,
        });
        machine.apply(&decision);
        tracing::info!(
            session_id = %self.session_info.id.0,
            pass = verdict.pass,
            ?decision,
            "doggy: after_audit decision"
        );

        match decision {
            TaskDecision::TaskDone => {
                self.doggy_mark_task_done().await;
                DoggyRoundAction::EndTurn
            }
            TaskDecision::RunAnotherRound { injection } => {
                let findings = match &injection {
                    Injection::Fix { findings } => findings.as_slice(),
                    _ => &[],
                };
                self.doggy_emit_completion_ui("fixing", findings).await;
                self.doggy_inject(injection).await;
                machine.mark_executing();
                DoggyRoundAction::Continue
            }
            TaskDecision::TaskPaused { .. } => {
                self.doggy_emit_completion_ui("paused", &[]).await;
                DoggyRoundAction::EndTurn
            }
            TaskDecision::SpawnAudit => {
                tracing::error!("doggy: after_audit returned SpawnAudit (invariant violation)");
                DoggyRoundAction::EndTurn
            }
        }
    }
}
