//! Injection payloads produced by the orchestrator for the next round.
//!
//! Host turns these into chat messages (goal_summary / system reminder).
//! Rendering templates can stay in the host; this module only carries intent.

use super::audit::AuditFinding;

/// What to inject before the next Executing round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injection {
    /// Open items remain; model must keep working.
    Continue {
        /// Human-readable open-item summary.
        open_summary: String,
    },
    /// Audit failed; model must address findings.
    Fix {
        findings: Vec<AuditFinding>,
    },
    /// The last `repeats` rounds were indistinguishable. Replaces the
    /// Continue/Fix text that has demonstrably stopped working, so the run
    /// gets a different prompt instead of the same one a fourth time.
    Reapproach {
        /// Consecutive identical rounds observed.
        repeats: u32,
        /// Open-item summary, carried so the new message still names the work.
        open_summary: String,
    },
}

impl Injection {
    pub fn continue_with_summary(open_summary: impl Into<String>) -> Self {
        Self::Continue {
            open_summary: open_summary.into(),
        }
    }

    pub fn fix_with(findings: Vec<AuditFinding>) -> Self {
        Self::Fix { findings }
    }

    pub fn reapproach(repeats: u32, open_summary: impl Into<String>) -> Self {
        Self::Reapproach {
            repeats,
            open_summary: open_summary.into(),
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
