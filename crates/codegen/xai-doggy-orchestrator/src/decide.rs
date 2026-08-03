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
                    Injection::Continue { .. } => TaskPhase::Executing,
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
pub fn decide_after_round(view: &RoundEndView) -> TaskDecision {
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
            injection: Injection::continue_with_summary(view.open_items.summary_line()),
        };
    }

    match &view.verification {
        VerificationOutcome::Achieved => TaskDecision::TaskDone,
        VerificationOutcome::Rejected { findings } => TaskDecision::RunAnotherRound {
            injection: Injection::fix_with(findings.clone()),
        },
        VerificationOutcome::Pending => TaskDecision::RunAnotherRound {
            injection: Injection::continue_with_summary(
                "acceptance pending — when the objective is met, call \
                 update_goal(completed: true) so verification can accept the task"
                    .to_string(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_items::OpenItem;

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
                injection: Injection::Continue { open_summary },
            } => {
                assert!(open_summary.contains("wire orchestrator"));
            }
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
    fn user_cancel_pauses_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_with_items(&["x"]),
            user_cancel: true,
            budget_hit: false,
            verification: VerificationOutcome::Pending,
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
            injection: Injection::continue_with_summary("work"),
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
