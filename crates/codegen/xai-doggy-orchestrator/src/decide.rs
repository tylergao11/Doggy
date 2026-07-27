//! Pure completion decisions — no I/O, no SessionActor.
//!
//! This is the **only** place that may conclude [`TaskDecision::TaskDone`].
//! Host adapters call [`decide_after_round`] / [`decide_after_audit`] and
//! apply the result; they must not invent a Done path on the side.

use super::audit::AuditVerdict;
use super::inject::Injection;
use super::open_items::OpenItemsSnapshot;
use super::state::{PauseReason, TaskPhase, TaskStatus};

/// Decision after a model round or an audit finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDecision {
    /// Inject and run another model round.
    RunAnotherRound { injection: Injection },
    /// Explicit items are clear — spawn the audit agent (must not mark Done).
    SpawnAudit,
    /// Only successful terminal outcome. Requires audit pass + no open work.
    TaskDone,
    /// Non-acceptance stop. Never used for "audit failed N times".
    TaskPaused { reason: PauseReason },
}

/// Inputs for a post-round decision (audit not yet run this step).
#[derive(Debug, Clone)]
pub struct RoundEndView {
    /// Round completed successfully (model stopped cleanly without cancel/infra error).
    pub round_ok: bool,
    pub open_items: OpenItemsSnapshot,
    pub user_cancel: bool,
    pub budget_hit: bool,
}

/// Inputs after the audit agent returns a structured verdict.
#[derive(Debug, Clone)]
pub struct AuditEndView {
    pub verdict: AuditVerdict,
    pub open_items: OpenItemsSnapshot,
    pub user_cancel: bool,
    pub budget_hit: bool,
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

    /// Apply a decision to local phase/status. Host still performs inject/spawn I/O.
    pub fn apply(&mut self, decision: &TaskDecision) {
        match decision {
            TaskDecision::RunAnotherRound { injection } => {
                self.phase = match injection {
                    Injection::Fix { .. } => TaskPhase::Fix,
                    Injection::Continue { .. } => TaskPhase::Executing,
                };
                // Fix immediately transitions back to executing once inject is done;
                // host may call [`Self::mark_executing`] after inject.
                self.status = TaskStatus::Active;
                self.pause_reason = None;
            }
            TaskDecision::SpawnAudit => {
                self.phase = TaskPhase::Audit;
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

/// Decide what happens after a model round ends (before audit for this cycle).
///
/// Invariants:
/// - Never returns [`TaskDecision::TaskDone`] (Done requires audit pass).
/// - Explicit open items → Continue; no explicit items → SpawnAudit.
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

    // Successful round: completion gate.
    if view.open_items.has_explicit_work() {
        return TaskDecision::RunAnotherRound {
            injection: Injection::continue_with_summary(view.open_items.summary_line()),
        };
    }

    // No explicit items — must audit. Even if acceptance_pending is false
    // (host bug), we still refuse silent Done here.
    TaskDecision::SpawnAudit
}

/// Decide what happens after the audit agent returns.
///
/// Invariants:
/// - Pass + no blocking open items → TaskDone (sole Done path).
/// - Fail → Fix injection (no attempt counter, no pause-for-fail).
/// - Pass but explicit items reappeared → Continue (do not Done).
/// - Cancel / budget → Pause.
pub fn decide_after_audit(view: &AuditEndView) -> TaskDecision {
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

    if view.verdict.pass {
        // Explicit work must not remain. Acceptance flag is host-cleared on
        // Done; we still require no explicit items.
        if view.open_items.has_explicit_work() {
            return TaskDecision::RunAnotherRound {
                injection: Injection::continue_with_summary(view.open_items.summary_line()),
            };
        }
        return TaskDecision::TaskDone;
    }

    // Audit failed — always fix, never pause for "too many failures".
    TaskDecision::RunAnotherRound {
        injection: Injection::fix_with(view.verdict.findings.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditFinding;
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

    #[test]
    fn successful_round_with_open_items_continues() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_with_items(&["wire orchestrator", "add tests"]),
            user_cancel: false,
            budget_hit: false,
        });
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
    fn successful_round_without_explicit_items_spawns_audit_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: OpenItemsSnapshot::acceptance_only(),
            user_cancel: false,
            budget_hit: false,
        });
        assert_eq!(d, TaskDecision::SpawnAudit);
        // Root cause fix: no silent TaskDone after model stop.
        assert!(!matches!(d, TaskDecision::TaskDone));
    }

    #[test]
    fn successful_round_fully_clear_still_requires_audit() {
        // Even a fully clear snapshot must not Done without audit.
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: OpenItemsSnapshot::fully_clear(),
            user_cancel: false,
            budget_hit: false,
        });
        assert_eq!(d, TaskDecision::SpawnAudit);
    }

    #[test]
    fn user_cancel_pauses_not_done() {
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: open_with_items(&["x"]),
            user_cancel: true,
            budget_hit: false,
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
        });
        assert_eq!(
            d,
            TaskDecision::TaskPaused {
                reason: PauseReason::InfraError
            }
        );
    }

    #[test]
    fn audit_pass_is_sole_task_done_path() {
        let d = decide_after_audit(&AuditEndView {
            verdict: AuditVerdict::passed(),
            open_items: OpenItemsSnapshot::acceptance_only(),
            user_cancel: false,
            budget_hit: false,
        });
        assert_eq!(d, TaskDecision::TaskDone);
    }

    #[test]
    fn audit_pass_with_explicit_items_continues_not_done() {
        let d = decide_after_audit(&AuditEndView {
            verdict: AuditVerdict::passed(),
            open_items: open_with_items(&["still open"]),
            user_cancel: false,
            budget_hit: false,
        });
        assert!(matches!(
            d,
            TaskDecision::RunAnotherRound {
                injection: Injection::Continue { .. }
            }
        ));
    }

    #[test]
    fn audit_fail_always_fixes_never_pauses() {
        let findings = vec![AuditFinding {
            severity: Some("error".into()),
            message: "missing tests for decide()".into(),
        }];
        let d = decide_after_audit(&AuditEndView {
            verdict: AuditVerdict::failed(findings.clone()),
            open_items: OpenItemsSnapshot::acceptance_only(),
            user_cancel: false,
            budget_hit: false,
        });
        match d {
            TaskDecision::RunAnotherRound {
                injection: Injection::Fix { findings: ref f },
            } => {
                assert_eq!(f.len(), 1);
                assert!(f[0].message.contains("missing tests"));
            }
            other => panic!("expected Fix, got {other:?}"),
        }
        // No attempt-limit pause path exists in the API.
        assert!(!matches!(d, TaskDecision::TaskPaused { .. }));
        assert!(!matches!(d, TaskDecision::TaskDone));
    }

    #[test]
    fn audit_fail_repeatedly_never_becomes_done() {
        let findings = vec![AuditFinding {
            severity: None,
            message: "still wrong".into(),
        }];
        for _ in 0..20 {
            let d = decide_after_audit(&AuditEndView {
                verdict: AuditVerdict::failed(findings.clone()),
                open_items: OpenItemsSnapshot::acceptance_only(),
                user_cancel: false,
                budget_hit: false,
            });
            assert!(matches!(
                d,
                TaskDecision::RunAnotherRound {
                    injection: Injection::Fix { .. }
                }
            ));
        }
    }

    #[test]
    fn machine_apply_done_is_only_done_status_transition() {
        let mut m = TaskMachine::new();
        m.start();
        assert_eq!(m.status, TaskStatus::Active);

        m.apply(&TaskDecision::SpawnAudit);
        assert_eq!(m.phase, TaskPhase::Audit);
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
        // Regression: model stopped + empty explicit list must not Done.
        let d = decide_after_round(&RoundEndView {
            round_ok: true,
            open_items: OpenItemsSnapshot {
                items: vec![],
                acceptance_pending: false,
            },
            user_cancel: false,
            budget_hit: false,
        });
        assert_eq!(d, TaskDecision::SpawnAudit);
    }
}
