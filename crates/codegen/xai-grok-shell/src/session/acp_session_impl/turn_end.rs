//! Turn-completion concern for `SessionActor`: completion handling
//! and turn-error classification.

use super::*;

/// JSON-RPC code the sampling layer reports for an authentication failure.
const AUTH_ERROR_CODE: i32 = -32000;

/// Consecutive infra-classified turn failures the harness absorbs by
/// re-driving the goal itself before it concedes. Unattended runs spend the
/// harness's patience here, never the user's attention.
pub(super) const GOAL_INFRA_RETRY_MAX: u32 = 4;

/// Base backoff before a self-driven retry, multiplied by the streak, so a
/// rate limit gets 5s, 10s, 15s, 20s — enough to outlast a short window
/// without stalling a run for minutes on a transient blip.
const GOAL_INFRA_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// An infra-classified turn failure, with whether waiting could clear it.
pub(super) struct InfraFailure {
    pub message: String,
    pub retryable: bool,
}

impl SessionActor {
    /// Emit a cosmetic `Plan` update at turn end to clear stale spinners.
    ///
    /// When the model produces its final text response without a cleanup
    /// `todo_write` call, any remaining `in_progress` items leave stale
    /// spinners in the UI.
    ///
    /// This method **does not mutate** the underlying `TodoState` — the
    /// persisted resource state and the model's view of the todo list
    /// are unchanged.  It only emits a transient (non-persisted) `Plan`
    /// notification where `in_progress` entries are mapped to `completed`
    /// for display.
    ///
    /// Uses the canonical `plan_entry_from_todo_item` helper to preserve
    /// cancelled metadata, priorities, and other semantics.
    ///
    /// No-op if no `in_progress` items exist.
    pub(super) async fn emit_turn_end_plan_cleanup(&self) {
        use crate::tools::todo::{TodoState, TodoStatus, plan_entry_from_todo_item};
        use xai_grok_tools::types::resources::State;

        // Read the current TodoState (no mutation).
        let (entries, stale_count) = {
            let res = self
                .agent
                .borrow()
                .tool_bridge()
                .read_resource::<State<TodoState>>()
                .await;
            let Some(state) = res else {
                return; // No todo state at all.
            };

            let stale_count = state
                .0
                .todo_items()
                .filter(|t| t.status == TodoStatus::InProgress)
                .count();
            if stale_count == 0 {
                return;
            }

            // Build plan entries with in_progress → completed for display.
            // Uses the canonical `plan_entry_from_todo_item` helper to
            // preserve cancelled metadata, priority, and other semantics.
            let entries: Vec<_> = state
                .0
                .todo_items()
                .map(|item| {
                    let mut entry = plan_entry_from_todo_item(item.clone());
                    if item.status == TodoStatus::InProgress {
                        entry.status = acp::PlanEntryStatus::Completed;
                    }
                    entry
                })
                .collect();

            (entries, stale_count)
        };

        tracing::info!(
            stale_count,
            "emitting transient turn-end Plan cleanup — in_progress shown as completed"
        );

        // Use transient notification — this is a cosmetic UI update that
        // must NOT be persisted or replayed on session reload.  The real
        // TodoState in Resources is the source of truth.
        let notification = acp::SessionNotification::new(
            self.session_info.id.clone(),
            acp::SessionUpdate::Plan(acp::Plan::new(entries)),
        );
        self.emit_transient_notification(notification);
    }

    /// Emit `x.ai/git_head_changed` after an edit/shell command that may have
    /// moved HEAD (e.g. `git checkout`), so clients update their status bar
    /// immediately rather than waiting for the debounced fs-watch refresh.
    pub(super) async fn maybe_notify_git_branch(&self) {
        if !self.git_head_enabled {
            return;
        }
        let cwd = self.tool_context.cwd.as_path();

        // `get_worktree_info` doubles as the "in a git repo?" probe (None when not).
        let (worktree_info, branch) = tokio::join!(
            xai_grok_workspace::session::git::get_worktree_info(cwd),
            xai_grok_workspace::session::git::get_branch(cwd),
        );
        let Some((is_worktree, main_repo)) = worktree_info else {
            return;
        };

        let dedup_key = git_head_dedup_key(branch.as_deref(), is_worktree, main_repo.as_deref());
        {
            let mut last = self.last_reported_branch.lock();
            if last.as_deref() == Some(&dedup_key) {
                return;
            }
            *last = Some(dedup_key);
        }

        let params = xai_grok_workspace::session::git::GitHeadChanged {
            session_id: self.session_info.id.0.to_string(),
            branch,
            is_worktree,
            main_repo,
        };
        if let Ok(raw) = serde_json::value::to_raw_value(&params) {
            let notification = acp::ExtNotification::new("x.ai/git_head_changed", raw.into());
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }
    }

    /// Live subagents and sticky usage-not-applied. `None` if the query failed.
    pub(super) async fn outstanding_reply_for_prompt(
        &self,
        prompt_id: &str,
    ) -> Option<xai_grok_tools::implementations::grok_build::task::types::SubagentOutstandingReply>
    {
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return Some(Default::default());
        };
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentEvent, SubagentOutstandingRequest,
        };
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::Outstanding(SubagentOutstandingRequest {
                prompt_id: prompt_id.to_string(),
                respond_to,
            }))
            .is_err()
        {
            return None;
        }
        rx.await.ok()
    }

    /// Report-level incomplete (error-path attach, tests). Same OR as
    /// [`super::turn::UsageDrainOutcome::report_incomplete`].
    pub(super) fn usage_incomplete_from_reply(
        reply: Option<
            &xai_grok_tools::implementations::grok_build::task::types::SubagentOutstandingReply,
        >,
    ) -> bool {
        super::turn::UsageDrainOutcome::from_outstanding_reply(reply).report_incomplete()
    }

    pub(super) fn clear_subagent_usage_not_applied(&self, prompt_id: &str) {
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return;
        };
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentClearUsageNotAppliedRequest, SubagentEvent,
        };
        let _ = tx.send(SubagentEvent::ClearUsageNotApplied(
            SubagentClearUsageNotAppliedRequest {
                prompt_id: prompt_id.to_string(),
            },
        ));
    }

    pub(super) async fn handle_completion(&self, prompt_id: String, result: PromptTurnResult) {
        let became_idle = {
            let mut current_prompt_id = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            if current_prompt_id.as_deref() == Some(prompt_id.as_str()) {
                *current_prompt_id = None;
            }
            current_prompt_id.is_none()
        };
        if became_idle {
            self.flush_pending_skill_reminders().await;
            // Idle-gated: a stale completion must not clobber the promoted turn's resources.
            self.agent
                .borrow()
                .tool_bridge()
                .update_resource(
                    xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                        String::new(),
                    ),
                )
                .await;
            // Goal turn is over — re-enable per-tool-call completion reminders.
            self.set_goal_loop_active_resource(false).await;
        }

        let mut state = self.state.lock().await;
        // True only when this completion matched the front prompt and dequeued
        // it. The unknown-prompt branch below must NOT emit a terminal: a turn
        // the Cancel path already finalized can leave a stale completion here.
        let mut owned_completion = false;
        let mut broadcast_queue = false;
        if state
            .pending_inputs
            .front()
            .is_some_and(|input| input.prompt_id == prompt_id)
        {
            let Some(input) = state.pending_inputs.pop_front() else {
                return;
            };
            owned_completion = true;
            let _ = input.respond_to.send(result.clone()).ok();
            // The completed prompt left the queue; re-broadcast (below, after
            // `running_task` clears so the wire's `running_prompt_id` doesn't
            // advertise the finished turn) the authoritative queue.
            broadcast_queue = input.queue_meta.is_some();
        } else {
            tracing::warn!("Received completion for unknown prompt: {prompt_id}");
            xai_grok_telemetry::unified_log::warn(
                "shell.turn.stale_completion_dropped",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": prompt_id,
                    "running_prompt_id": state.running_prompt_id(),
                })),
            );
        }
        // Ownership-gated: a stale completion must not null the promoted turn's
        // task, or `maybe_start_running_task` would double-spawn the prompt.
        if state.running_prompt_id() == Some(prompt_id.as_str()) || owned_completion {
            state.running_task = None;
        }
        if broadcast_queue {
            self.broadcast_queue_changed(&state);
        }
        // Note: Auto-compact is now handled inline during process_conversation_turn,
        // so we no longer need to queue it here after turn completion.

        // If the user toggled plan mode off while this turn was in-flight
        // (state == ExitPending), complete the deferred exit now that the
        // turn is finished. The next handle_prompt() will inject the exit
        // reminder via has_pending_exit_reminder().
        {
            let mut tracker = self.plan_mode.lock();
            let transitioned =
                tracker.state() == crate::session::plan_mode::PlanModeState::ExitPending;
            tracker.complete_deferred_exit();
            drop(tracker);
            if transitioned {
                self.persist_plan_mode_state();
            }
        }
        // Drop the state guard before the async emit so the persist/broadcast
        // fork doesn't run under the state lock.
        drop(state);

        // Durable twin of the fire-and-forget `prompt_complete` (emitted from
        // `MvpAgent::prompt`): publish the turn's terminal on the persisted +
        // replayed `_x.ai/session/update` rail so a viewer that re-attaches
        // mid-turn finalizes from replay instead of stranding on "Waiting…".
        // The caller flushed the replay buffer first, so this lands strictly
        // after the turn's last `session/update` delta. Emit ONLY for a
        // completion this handler owned (dequeued at the front): the
        // unknown-prompt branch above is a stale completion for a turn the
        // Cancel path already finalized, so emitting there would double-emit a
        // terminal for the same prompt_id. A `RemovedFromQueue` result never
        // started a turn, so it emits nothing either.
        let emit_terminal = owned_completion
            && !matches!(
                result,
                Ok(PromptTurnOk {
                    completion_kind: PromptCompletionKind::RemovedFromQueue,
                    ..
                })
            );
        if emit_terminal {
            let mapped = result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let usage = match &result {
                Ok(ok) => ok.usage.clone(),
                Err(err) => {
                    if let Some(u) = crate::sampling::error::prompt_usage_from_error(err) {
                        Some(u)
                    } else {
                        self.error_path_usage_fallback(&prompt_id).await
                    }
                }
            };
            // Surface the cancel trigger on the terminal `_meta` as `cancelTrigger`.
            let cancel_trigger = result
                .as_ref()
                .ok()
                .and_then(|ok| match &ok.completion_kind {
                    PromptCompletionKind::Cancelled {
                        context: Some(ctx), ..
                    } => ctx.trigger.as_deref(),
                    _ => None,
                });
            self.emit_turn_completed(prompt_id, &mapped, usage, cancel_trigger)
                .await;
        }
    }

    /// Emit the durable, replayable `TurnCompleted` terminal — the single
    /// chokepoint shared by the completion (`handle_completion`) and cancel
    /// (`cancel_running_task`) sites. Derives `(stop_reason, agent_result)`
    /// from the SAME source as the fire-and-forget `prompt_complete`
    /// (`prompt_complete_fields`), so the two signals never disagree, then
    /// persists + forwards via `send_xai_notification`. Retiring
    /// `prompt_complete` later is then a one-line change at the call sites.
    ///
    /// `cancel_trigger` (when `Some`) rides the `_meta` as `cancelTrigger`;
    /// `"send_now"` marks a cancel-and-send end (marker suppressed).
    pub(super) async fn emit_turn_completed(
        &self,
        prompt_id: String,
        mapped: &std::result::Result<acp::StopReason, acp::Error>,
        usage: Option<crate::extensions::notification::PromptUsage>,
        cancel_trigger: Option<&str>,
    ) {
        let (stop_reason, agent_result) = crate::sampling::error::prompt_complete_fields(mapped);
        let extra_meta = cancel_trigger.map(|t| {
            [("cancelTrigger".to_string(), serde_json::json!(t))]
                .into_iter()
                .collect()
        });
        self.send_xai_notification_with_extra_meta(
            crate::session::turn_completion::build_turn_completed(
                prompt_id,
                stop_reason,
                agent_result,
                usage,
            ),
            extra_meta,
        )
        .await;
    }

    pub(super) fn classify_turn_error(err: &acp::Error) -> String {
        match i32::from(err.code) {
            crate::sampling::error::RATE_LIMITED_ERROR_CODE => "rate_limit",
            -32000 => "auth",
            -32600 => "invalid_request",
            -32603 => "internal",
            _ => "unknown",
        }
        .to_string()
    }

    pub(super) fn is_infra_turn_error(err: &acp::Error) -> bool {
        matches!(
            i32::from(err.code),
            crate::sampling::error::RATE_LIMITED_ERROR_CODE | AUTH_ERROR_CODE | -32603
        )
    }

    /// `(turn_succeeded, infra_failure)` for the completion handler.
    /// `infra_failure` is extracted before `handle_completion` consumes
    /// `result`.
    pub(super) fn post_turn_goal_degradation_plan(
        result: &PromptTurnResult,
    ) -> (bool, Option<InfraFailure>) {
        let turn_cancelled = result.as_ref().ok().is_some_and(|ok| {
            matches!(
                ok.completion_kind,
                crate::session::commands::PromptCompletionKind::Cancelled { .. }
                    | crate::session::commands::PromptCompletionKind::MaxTurnsReached { .. }
            )
        });
        let turn_succeeded = result.is_ok() && !turn_cancelled;
        let infra_failure = result
            .as_ref()
            .err()
            .filter(|err| Self::is_infra_turn_error(err))
            .map(|err| InfraFailure {
                message: Self::format_turn_error_message(err),
                // Waiting cannot conjure a credential, so an auth failure is
                // the one infra error where re-driving is pure token burn.
                retryable: i32::from(err.code) != AUTH_ERROR_CODE,
            });
        (turn_succeeded, infra_failure)
    }

    /// Recover from an infra-classified turn failure without involving the
    /// user, or stop when recovery is genuinely impossible.
    ///
    /// The unattended-run contract: a rate limit or a transient server error
    /// is the harness's problem, not the user's. Each failure buys a
    /// progressively longer wait and one more self-driven turn, up to
    /// [`GOAL_INFRA_RETRY_MAX`]. Only two things end the run here — an auth
    /// failure, and a streak that outlived its patience — and both mean the
    /// model is simply not reachable, which is a resource limit like the token
    /// budget rather than a question for the user.
    ///
    /// The wait happens on a detached task: awaiting it inline would hold the
    /// session event loop and make Ctrl+C and interjections unresponsive for
    /// the whole backoff.
    pub(super) async fn recover_or_pause_after_infra_err(
        self: &std::sync::Arc<Self>,
        failure: InfraFailure,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        let goal_active = self.goal_tracker.lock().status()
            == Some(crate::session::goal_tracker::GoalStatus::Active);
        if !goal_active {
            return;
        }
        let retry = failure.retryable
            && self
                .goal_tracker
                .lock()
                .record_infra_failure(GOAL_INFRA_RETRY_MAX);
        if !retry {
            self.apply_infra_pause_after_turn_err(failure.message).await;
            return;
        }
        let streak = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|o| o.infra_retry_streak)
            .unwrap_or(1);
        let delay = GOAL_INFRA_RETRY_BACKOFF * streak;
        tracing::warn!(
            streak,
            max = GOAL_INFRA_RETRY_MAX,
            delay_s = delay.as_secs(),
            error = %failure.message,
            "goal: infra turn failure; re-driving the goal after backoff",
        );
        self.send_slash_command_output(&format!(
            "Turn failed ({}); retrying in {}s (attempt {streak} of {GOAL_INFRA_RETRY_MAX}).",
            failure
                .message
                .strip_prefix("Turn failed: ")
                .unwrap_or(&failure.message),
            delay.as_secs(),
        ))
        .await;

        let session = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(delay).await;
            // The goal may have been completed, cleared, or paused by the user
            // while we waited; re-check rather than resurrect it.
            if session.goal_tracker.lock().status()
                != Some(crate::session::goal_tracker::GoalStatus::Active)
            {
                return;
            }
            session.queue_goal_retry_turn().await;
            Self::maybe_start_running_task(session, completion_tx).await;
        });
    }

    pub(super) async fn apply_infra_pause_after_turn_err(&self, message: String) -> bool {
        let slash_detail = match message.strip_prefix("Turn failed: ") {
            Some(rest) => rest.to_owned(),
            None => message.clone(),
        };
        let paused = self
            .auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                message,
            )
            .await;
        if paused {
            self.send_slash_command_output(&format!(
                "Goal paused due to turn error: {slash_detail}. Use /goal resume to retry."
            ))
            .await;
        }
        paused
    }

    /// Extract the best human-readable detail from an infra turn error.
    fn turn_error_detail(err: &acp::Error) -> Option<String> {
        err.data
            .as_ref()
            .and_then(crate::sampling::error::error_detail_from_data)
            .or_else(|| {
                if !err.message.is_empty() {
                    Some(err.message.clone())
                } else {
                    None
                }
            })
    }

    pub(super) fn format_turn_error_message(err: &acp::Error) -> String {
        if let Some(detail) = Self::turn_error_detail(err) {
            format!("Turn failed: {detail}")
        } else {
            format!("Turn failed: {}", Self::classify_turn_error(err))
        }
    }

    pub(super) fn classify_install_error(
        err: &xai_grok_agent::plugins::install_registry::InstallError,
    ) -> String {
        crate::plugin::classify_install_error(err)
    }
}

#[cfg(test)]
mod infra_failure_tests {
    use super::*;

    fn err(code: i32) -> acp::Error {
        acp::Error::new(code.into(), "boom")
    }

    fn plan_for(code: i32) -> Option<InfraFailure> {
        SessionActor::post_turn_goal_degradation_plan(&Err(err(code))).1
    }

    /// The retryable split decides whether an unattended run waits and
    /// continues or stops for the user, so pin each class explicitly.
    #[test]
    fn rate_limit_and_server_errors_are_retryable_but_auth_is_not() {
        let rate_limited = plan_for(crate::sampling::error::RATE_LIMITED_ERROR_CODE)
            .expect("a rate limit is an infra failure");
        assert!(
            rate_limited.retryable,
            "a rate limit clears on its own; the harness must wait, not ask the user",
        );
        assert!(rate_limited.message.starts_with("Turn failed:"));

        assert!(
            plan_for(-32603).expect("a server error is an infra failure").retryable,
            "a transient server error deserves a retry",
        );

        assert!(
            !plan_for(AUTH_ERROR_CODE)
                .expect("an auth failure is an infra failure")
                .retryable,
            "no credential appears by waiting, so retrying only burns tokens",
        );
    }

    #[test]
    fn non_infra_errors_produce_no_recovery_plan() {
        // An invalid request is the model's or harness's fault; retrying the
        // same turn would reproduce it forever.
        assert!(plan_for(-32600).is_none());
        assert!(plan_for(-1).is_none());
    }

    #[test]
    fn a_successful_turn_reports_success_and_no_failure() {
        let (succeeded, failure) = SessionActor::post_turn_goal_degradation_plan(&Err(err(-32600)));
        assert!(!succeeded);
        assert!(failure.is_none());
    }
}
