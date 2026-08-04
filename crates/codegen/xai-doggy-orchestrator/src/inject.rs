//! Injection payloads produced by the orchestrator for the next round.
//!
//! Host turns these into chat messages (goal_summary / system reminder).
//! Rendering templates can stay in the host; this module only carries intent.

use super::audit::AuditFinding;
use super::progress::{RoundDelta, StallReason};

/// What to inject before the next Executing round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injection {
    /// Open items remain; model must keep working.
    Continue {
        /// Human-readable open-item summary.
        open_summary: String,
        /// What the round that just ended actually changed.
        ///
        /// Carried so the message is never a verbatim repeat of the last one.
        /// The continue text used to restate the open work and nothing else,
        /// and the host prunes the previous copy before pushing the next, so a
        /// stalled run saw the identical instruction against an identical
        /// workspace every round — a fixed point that produced an identical
        /// answer. The delta is what makes each round's input new.
        delta: RoundDelta,
    },
    /// Audit failed; model must address findings.
    Fix { findings: Vec<AuditFinding> },
    /// The run stopped moving. Replaces the Continue/Fix text that has
    /// demonstrably stopped working, so the model gets a different prompt
    /// instead of the same one again.
    Reapproach {
        /// Consecutive bad rounds observed.
        repeats: u32,
        /// Which kind of stuck this is; the host words the two differently.
        reason: StallReason,
        /// Open-item summary, carried so the new message still names the work.
        open_summary: String,
        /// What the round that just ended actually changed.
        delta: RoundDelta,
    },
}

impl Injection {
    pub fn continue_with(open_summary: impl Into<String>, delta: RoundDelta) -> Self {
        Self::Continue {
            open_summary: open_summary.into(),
            delta,
        }
    }

    pub fn fix_with(findings: Vec<AuditFinding>) -> Self {
        Self::Fix { findings }
    }

    pub fn reapproach(
        repeats: u32,
        reason: StallReason,
        open_summary: impl Into<String>,
        delta: RoundDelta,
    ) -> Self {
        Self::Reapproach {
            repeats,
            reason,
            open_summary: open_summary.into(),
            delta,
        }
    }

    /// Stable kind label for telemetry / tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Continue { .. } => "continue",
            Self::Fix { .. } => "fix",
            Self::Reapproach { .. } => "reapproach",
        }
    }
}
