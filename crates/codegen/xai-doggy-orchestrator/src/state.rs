//! Task lifecycle phases and terminal statuses.
//!
//! Round completion is *not* represented here: a round is a step inside
//! [`TaskPhase::Executing`]. Only the orchestrator may move a task to
//! [`TaskStatus::Done`].

/// Where the task sits in the completion loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskPhase {
    /// No task started for this session prompt.
    #[default]
    Idle,
    /// Running a model round (`process_conversation_turn`).
    Executing,
    /// Round ended; evaluating open items (decision step, usually instantaneous).
    CheckOpen,
    /// Reserved historical phase (Doggy audit subagent removed); unused.
    Audit,
    /// Verification rejected; about to inject fix findings and re-enter Executing.
    Fix,
    /// Task accepted — the only successful terminal phase.
    Done,
    /// Stopped without acceptance (user / infra / budget). Resumable in principle.
    Paused,
}

/// Coarse lifecycle status for host / UI / persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Idle,
    /// Task is running (any non-terminal phase except Idle).
    Active,
    /// Accepted via verification Achieved. Only the orchestrator may set this.
    Done,
    /// Not accepted; stopped for a [`PauseReason`].
    Paused,
}

/// Why a task entered [`TaskStatus::Paused`].
///
/// These are **safety valves**, not business abandonment after verification
/// rejection. Rejected verification injects Fix and continues — never pauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// User cancelled the turn / task.
    UserCancel,
    /// Round failed with infrastructure or non-success terminal (auth, sampler, …).
    InfraError,
    /// Session/task token or round budget exhausted.
    BudgetExhausted,
    /// Consecutive rounds achieved nothing the gate can observe. The host must
    /// not turn this into an idle pause — see
    /// `GoalPauseReason::halts_unattended_run`. It means "give up on this work
    /// and let the deferral ladder decide what is still reachable", which is a
    /// *continuation* decision most of the time.
    NoProgress {
        repeats: u32,
        reason: super::progress::StallReason,
    },
}
