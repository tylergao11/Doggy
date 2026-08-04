//! Pure completion decisions — no I/O, no SessionActor.
//!
//! This is the **only** place that may conclude [`TaskDecision::TaskDone`].
//! Host adapters call [`decide_after_round`] and apply the result; they must
//! not invent a Done path on the side.
//!
//! Acceptance is the **goal verification panel** (classifier / skeptics), not
//! a separate Doggy audit subagent.

use super::audit::AuditFinding;
use super::inject::Injection;
use super::open_items::OpenItemsSnapshot;
use super::progress::{RoundDelta, StallLevel};
use super::state::{PauseReason, TaskPhase, TaskStatus};

/// Decision after a model round finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDecision {
    /// Inject and run another model round.
    RunAnotherRound { injection: Injection },
    /// Successful terminal outcome: no explicit open work + verification Achieved.
    TaskDone,
    /// Non-acceptance stop. Never used for "verification rejected N times".
    TaskPaused { reason: PauseReason },
}

/// Goal-verification outcome after the turn-end drain (classifier panel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    /// No `update_goal(completed)` / no panel verdict this cycle.
    Pending,
    /// Skeptics accepted the claim.
    Achieved,
    /// Skeptics rejected; host injects Fix with these findings (gap text).
    Rejected { findings: Vec<AuditFinding> },
}

/// Inputs for a post-round decision.
#[derive(Debug, Clone)]
pub struct RoundEndView {
    /// Round completed successfully (model stopped cleanly without cancel/infra error).
    pub round_ok: bool,
    pub open_items: OpenItemsSnapshot,
    pub user_cancel: bool,
    pub budget_hit: bool,
    /// Classifier / skeptic panel result after drain.
    pub verification: VerificationOutcome,
    /// How many rounds in a row left every input above unchanged.
    ///
    /// Completion is a safety property and says nothing about liveness: the
    /// rules below can ask for another round forever. This is the termination
    /// measure that stops them — see [`super::progress`].
    pub stall: StallLevel,
    /// What the round that just ended actually moved. Carried into every
    /// injection so the model's input is never a repeat of the last round's.
    pub delta: RoundDelta,
}

/// Mutable machine surface the host keeps on the session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskMachine {
    pub phase: TaskPhase,
    pub status: TaskStatus,
    pub pause_reason: Option<PauseReason>,
}

impl TaskMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a task for the current user goal / prompt.
    pub fn start(&mut self) {
        self.phase = TaskPhase::Executing;
        self.status = TaskStatus::Active;
        self.pause_reason = None;
    }

    /// Apply a decision to local phase/status. Host still performs inject I/O.
    pub fn apply(&mut self, decision: &TaskDecision) {
        match decision {
            TaskDecision::RunAnotherRound { injection } => {
                self.phase = match injection {
                    Injection::Fix { .. } => TaskPhase::Fix,
                    // A re-approach is still execution, just prompted
                    // differently — it must not look like a new phase to the UI.
                    Injection::Continue { .. } | Injection::Reapproach { .. } => {
                        TaskPhase::Executing
                    }
                };
                self.status = TaskStatus::Active;
                self.pause_reason = None;
            }
            TaskDecision::TaskDone => {
                self.phase = TaskPhase::Done;
                self.status = TaskStatus::Done;
                self.pause_reason = None;
            }
            TaskDecision::TaskPaused { reason } => {
                self.phase = TaskPhase::Paused;
                self.status = TaskStatus::Paused;
                self.pause_reason = Some(*reason);
            }
        }
    }

    /// After Fix/Continue inject, re-enter the executing phase for the next round.
    pub fn mark_executing(&mut self) {
        if self.status == TaskStatus::Active {
            self.phase = TaskPhase::Executing;
        }
    }
}

/// Decide what happens after a model round ends.
///
/// Invariants:
/// - Explicit open items → Continue (never Done).
/// - No explicit items + verification Achieved → [`TaskDecision::TaskDone`].
/// - No explicit items + verification Rejected → Fix (never Done, never pause).
/// - No explicit items + verification Pending → Continue (ask for re-verify).
/// - Cancel / budget / failed round → Pause, never Done.
///
/// Those rules decide *whether the task is done*; on their own they can ask
/// for another round forever. [`RoundEndView::stall`] is the termination
/// measure layered on top: it may only downgrade a request for another round,
/// never override a terminal outcome, so a task that genuinely finished is
/// still accepted no matter how alike the rounds getting there looked.
pub fn decide_after_round(view: &RoundEndView) -> TaskDecision {
    let decision = decide_completion(view);
    let TaskDecision::RunAnotherRound { injection } = decision else {
        return decision;
    };
    match view.stall {
        StallLevel::Progressing => TaskDecision::RunAnotherRound { injection },
        StallLevel::Reapproach { repeats, reason } => TaskDecision::RunAnotherRound {
            injection: Injection::reapproach(
                repeats,
                reason,
                view.open_items.summary_line(),
                view.delta.clone(),
            ),
        },
        StallLevel::CutOff { repeats, reason } => TaskDecision::TaskPaused {
            reason: PauseReason::NoProgress { repeats, reason },
        },
    }
}

/// The completion rules alone, before the stall measure is applied.
fn decide_completion(view: &RoundEndView) -> TaskDecision {
    if view.user_cancel {
        return TaskDecision::TaskPaused {
            reason: PauseReason::UserCancel,
        };
    }
    if view.budget_hit {
        return TaskDecision::TaskPaused {
            reason: PauseReason::BudgetExhausted,
        };
    }
    if !view.round_ok {
        return TaskDecision::TaskPaused {
            reason: PauseReason::InfraError,
        };
    }

    if view.open_items.has_explicit_work() {
        return TaskDecision::RunAnotherRound {
            injection: Injection::continue_with(
                view.open_items.summary_line(),
                view.delta.clone(),
            ),
        };
    }

    match &view.verification {
        VerificationOutcome::Achieved => TaskDecision::TaskDone,
        VerificationOutcome::Rejected { findings } => TaskDecision::RunAnotherRound {
            injection: Injection::fix_with(findings.clone()),
        },
        VerificationOutcome::Pending => TaskDecision::RunAnotherRound {
            injection: Injection::continue_with(
                "acceptance pending — when every acceptance criterion is met, call \
                 update_goal(completed: true) so verification can accept the task",
                view.delta.clone(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_items::OpenItem;
    use crate::progress::StallReason;

    fn open_with_items(summaries: &[&str]) -> OpenItemsSnapshot {
        OpenItemsSnapshot {
            items: summaries
                .iter()
                .map(|s| OpenItem {
                    id: None,
                    summary: (*s).to_string(),
                })
                .collect(),
            acceptance_pending: true,
        }
    }

    fn round(
        open: OpenItemsSnapshot,
        verification: VerificationOutcome,
    ) -> RoundEndView {
        RoundEndView {
            round_ok: true,
            open_items: open,
            user_cancel: false,
            budget_hit: false,
            verification,
            stall: StallLevel::Progressing,
            delta: RoundDelta::default(),
        }
    }

    #[test]
    fn successful_round_with_open_items_continues() {
        let d = decide_after_round(&round(
            open_with_items(&["wire orchestrator", "add tests"]),
            VerificationOutcome::Achieved,
        ));
        match d {
            TaskDecision::RunAnotherRound {
                injection: Injection::Continue { open_summary, .. },
            } => {
                assert!(open_summary.contains("wire orchestrator"));
            }
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn every_continue_carries_the_round_delta() {
        // The fixed point that produced the observed loop: the host prunes the
        // previous directive, so an injection without a delta is byte-identical
        // every round and the model answers it identically.
        let view = RoundEndView {
            delta: RoundDelta {
                round: 4,
                tools_called: vec!["read_file".into()],
                ..RoundDelta::default()
            },
            ..round(
                OpenItemsSnapshot::acceptance_only(),
                VerificationOutcome::Pending,
            )
        };
        match decide_after_round(&view) {
            TaskDecision::RunAnotherRound {
                injection: Injection::Continue { delta, .. },
            } => assert_eq!(delta.round, 4),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn open_items_block_done_even_when_verified() {
        let d = decide_after_round(&round(
            open_with_items(&["still open"]),
            VerificationOutcome::Achieved,
        ));
        assert!(matches!(
            d,
            TaskDecision::RunAnotherRound {
                injection: Injection::Continue { .. }
            }
        ));
    }

    #[test]
    fn verified_achieved_without_explicit_items_is_task_done() {
        let d = decide_after_round(&round(
            OpenItemsSnapshot::acceptance_only(),
            VerificationOutcome::Achieved,
        ));
        assert_eq!(d, TaskDecision::TaskDone);
    }

    #[test]
    fn pending_verification_continues_not_done() {
        let d = decide_after_round(&round(
            OpenItemsSnapshot::acceptance_only(),
            VerificationOutcome::Pending,
        ));
        assert!(matches!(
            d,
            TaskDecision::RunAnotherRound {
                injection: Injection::Continue { .. }
            }
        ));
        assert!(!matches!(d, TaskDecision::TaskDone));
    }

    #[test]
    fn rejected_verification_fixes_never_done() {
        let findings = vec![AuditFinding {
            severity: Some("error".into()),
            criterion: Some(2),
            message: "missing lock co-packaging".into(),
        }];
        let d = decide_after_round(&round(
            OpenItemsSnapshot::acceptance_only(),
            VerificationOutcome::Rejected {
                findings: findings.clone(),
            },
        ));
        match d {
            TaskDecision::RunAnotherRound {
                injection: Injection::Fix { findings: ref f },
            } => {
                assert_eq!(f.len(), 1);
                assert!(f[0].message.contains("lock"));
            }
            other => panic!("expected Fix, got {other:?}"),
        }
    }

    #[test]
    fn a_stalled_run_is_re_approached_before_it_is_cut_off() {
        // The failure this guard exists for: acceptance never claimed, so the
        // completion rules ask for another round every time. Without the stall
        // measure this is an unbounded loop injecting the same text forever.
        let stalled = |stall| RoundEndView {
            stall,
            ..round(
                OpenItemsSnapshot::acceptance_only(),
                VerificationOutcome::Pending,
            )
        };
        match decide_after_round(&stalled(StallLevel::Reapproach {
            repeats: 3,
            reason: StallReason::Repeated,
        })) {
            TaskDecision::RunAnotherRound {
                injection: Injection::Reapproach { repeats, reason, .. },
            } => {
                assert_eq!(repeats, 3);
                assert_eq!(reason, StallReason::Repeated);
            }
            other => panic!("expected Reapproach, got {other:?}"),
        }
        assert_eq!(
            decide_after_round(&stalled(StallLevel::CutOff {
                repeats: 6,
                reason: StallReason::Repeated,
            })),
            TaskDecision::TaskPaused {
                reason: PauseReason::NoProgress {
                    repeats: 6,
                    reason: StallReason::Repeated
                }
            },
        );
    }

    #[test]
    fn a_tool_less_round_is_re_approached_not_continued() {
        // Narration answered with the same "continue implementing" is the
        // fixed point; it has to get a different message on the first offence.
        let view = RoundEndView {
            stall: StallLevel::Reapproach {
                repeats: 1,
                reason: StallReason::Idle,
            },
            ..round(
                open_with_items(&["delete the old folder"]),
                VerificationOutcome::Pending,
            )
        };
        match decide_after_round(&view) {
            TaskDecision::RunAnotherRound {
                injection: Injection::Reapproach { reason, .. },
            } => assert_eq!(reason, StallReason::Idle),
            other => panic!("expected Reapproach, got {other:?}"),
        }
    }

    #[test]
    fn open_todos_alone_can_stall_the_run_forever_without_the_measure() {
        // Explicit items short-circuit before verification is even read, so an
        // open-item list the model never closes is the second unbounded path.
        let view = RoundEndView {
            stall: StallLevel::CutOff {
                repeats: 6,
                reason: StallReason::Repeated,
            },
            ..round(
                open_with_items(&["port the loader"]),
                VerificationOutcome::Achieved,
            )
        };
        assert_eq!(
            decide_after_round(&view),
            TaskDecision::TaskPaused {
                reason: PauseReason::NoProgress {
                    repeats: 6,
                    reason: StallReason::Repeated
                }
            },
        );
    }

    /// The regression test for the bug this measure exists for.
    ///
    /// Drives the host loop's exact shape against a model that answers every
    /// injection the same way and never claims completion. Before the stall
    /// measure this ran until the user cancelled it.
    #[test]
    fn the_round_loop_terminates_against_a_model_that_never_completes() {
        use crate::progress::{IDLE_CUTOFF_AFTER, RoundActivity, RoundLedger};

        let mut ledger = RoundLedger::new();
        let open = OpenItemsSnapshot::acceptance_only();
        let verification = VerificationOutcome::Pending;
        // The observed failure: narration only, so not even a tool name moves.
        let activity = RoundActivity::default();

        let mut injections: Vec<&'static str> = Vec::new();
        for round in 1..=1_000u32 {
            let (stall, delta) = ledger.observe(&open, &verification, &activity);
            let decision = decide_after_round(&RoundEndView {
                round_ok: true,
                open_items: open.clone(),
                user_cancel: false,
                budget_hit: false,
                verification: verification.clone(),
                stall,
                delta,
            });
            match decision {
                TaskDecision::RunAnotherRound { injection } => injections.push(injection.kind()),
                TaskDecision::TaskPaused {
                    reason: PauseReason::NoProgress { repeats, reason },
                } => {
                    assert_eq!(round, IDLE_CUTOFF_AFTER);
                    assert_eq!(repeats, IDLE_CUTOFF_AFTER);
                    assert_eq!(reason, StallReason::Idle);
                    assert_eq!(
                        injections,
                        vec!["reapproach"; (IDLE_CUTOFF_AFTER - 1) as usize],
                        "a tool-less round never gets the plain continue that \
                         produced the fixed point",
                    );
                    return;
                }
                other => panic!("unexpected decision {other:?}"),
            }
        }
        panic!("the completion gate never stopped asking for another round");
    }

    /// Same shape, but the model does call tools — it just never converges.
    /// The repeat measure has to stop it where the idle measure cannot.
    #[test]
    fn the_round_loop_terminates_against_a_model_that_works_but_never_converges() {
        use crate::progress::{RoundActivity, RoundLedger, STALL_CUTOFF_AFTER};

        let mut ledger = RoundLedger::new();
        let open = OpenItemsSnapshot::acceptance_only();
        let verification = VerificationOutcome::Pending;
        let activity = RoundActivity {
            tools_called: vec!["grep".into(), "read_file".into()],
            ..RoundActivity::default()
        };

        for round in 1..=1_000u32 {
            let (stall, delta) = ledger.observe(&open, &verification, &activity);
            match decide_after_round(&RoundEndView {
                round_ok: true,
                open_items: open.clone(),
                user_cancel: false,
                budget_hit: false,
                verification: verification.clone(),
                stall,
                delta,
            }) {
                TaskDecision::RunAnotherRound { .. } => {}
                TaskDecision::TaskPaused {
                    reason: PauseReason::NoProgress { reason, .. },
                } => {
                    assert_eq!(round, STALL_CUTOFF_AFTER);
                    assert_eq!(reason, StallReason::Repeated);
                    return;
                }
                other => panic!("unexpected decision {other:?}"),
            }
        }
        panic!("the completion gate never stopped asking for another round");
    }

    #[test]
    fn a_stall_never_overrides_a_terminal_outcome() {
        // Done and Pause are conclusions about the work; the stall measure only
        // limits how many times the gate may ask for more of it.
        let hard_stall = StallLevel::CutOff {
            repeats: 99,
            reason: StallReason::Repeated,
        };
        let done = RoundEndView {
            stall: hard_stall,
            ..round(
                OpenItemsSnapshot::acceptance_only(),
                VerificationOutcome::Achieved,
            )
        };
        assert_eq!(decide_after_round(&done), TaskDecision::TaskDone);

        let cancelled = RoundEndView {
            user_cancel: true,
            stall: hard_stall,
            ..round(
                OpenItemsSnapshot::acceptance_only(),
                VerificationOutcome::Pending,
            )
        };
        assert_eq!(
            decide_after_round(&cancelled),
            TaskDecision::TaskPaused {
                reason: PauseReason::UserCancel
            },
        );
    }

    #[test]
    fn user_cancel_pauses_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_with_items(&["x"]),
            user_cancel: true,
            budget_hit: false,
            verification: VerificationOutcome::Pending,
            stall: StallLevel::Progressing,
            delta: RoundDelta::default(),
        });
        assert_eq!(
            d,
            TaskDecision::TaskPaused {
                reason: PauseReason::UserCancel
            }
        );
    }

    #[test]
    fn budget_hit_pauses_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: OpenItemsSnapshot::acceptance_only(),
            user_cancel: false,
            budget_hit: true,
            verification: VerificationOutcome::Achieved,
            stall: StallLevel::Progressing,
            delta: RoundDelta::default(),
        });
        assert_eq!(
            d,
            TaskDecision::TaskPaused {
                reason: PauseReason::BudgetExhausted
            }
        );
    }

    #[test]
    fn failed_round_pauses_infra_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: false,
            open_items: OpenItemsSnapshot::acceptance_only(),
            user_cancel: false,
            budget_hit: false,
            verification: VerificationOutcome::Achieved,
            stall: StallLevel::Progressing,
            delta: RoundDelta::default(),
        });
        assert_eq!(
            d,
            TaskDecision::TaskPaused {
                reason: PauseReason::InfraError
            }
        );
    }

    #[test]
    fn machine_apply_done_is_only_done_status_transition() {
        let mut m = TaskMachine::new();
        m.start();
        assert_eq!(m.status, TaskStatus::Active);

        m.apply(&TaskDecision::RunAnotherRound {
            injection: Injection::continue_with("work", RoundDelta::default()),
        });
        assert_ne!(m.status, TaskStatus::Done);

        m.apply(&TaskDecision::TaskDone);
        assert_eq!(m.phase, TaskPhase::Done);
        assert_eq!(m.status, TaskStatus::Done);
    }

    #[test]
    fn machine_apply_fix_then_mark_executing() {
        let mut m = TaskMachine::new();
        m.start();
        let fix = TaskDecision::RunAnotherRound {
            injection: Injection::fix_with(vec![AuditFinding {
                severity: None,
                criterion: None,
                message: "x".into(),
            }]),
        };
        m.apply(&fix);
        assert_eq!(m.phase, TaskPhase::Fix);
        m.mark_executing();
        assert_eq!(m.phase, TaskPhase::Executing);
        assert_eq!(m.status, TaskStatus::Active);
    }

    #[test]
    fn no_fail_open_done_from_round_alone() {
        // Model stopped + empty explicit list without verification must not Done.
        let d = decide_after_round(&round(
            OpenItemsSnapshot {
                items: vec![],
                acceptance_pending: false,
            },
            VerificationOutcome::Pending,
        ));
        assert!(!matches!(d, TaskDecision::TaskDone));
    }
}
