//! Round-to-round progress measure for the completion gate.
//!
//! [`super::decide::decide_after_round`] has exactly one successful exit: no
//! explicit open items *and* verification `Achieved`. Everything else is
//! another round. That is right as a **completion** rule and fatal as a
//! **liveness** rule — a model that neither empties its todo list nor claims
//! completion is told to "continue" forever, and the host loop applying the
//! decision has no reason of its own to stop.
//!
//! The upstream harness hid that hole because continuation was queued as a
//! *separate turn*, so the consecutive-failed-turn back-off in
//! `handle_turn_end` could count it. Once continuation moved inside the turn,
//! nothing counted anything: every round "succeeds", the streak resets, and
//! the loop is unbounded.
//!
//! This module supplies the missing well-founded measure. A round that leaves
//! everything the gate can observe unchanged did not move the task, and a run
//! that fails to move N times running is first re-approached, then cut off.

use super::decide::VerificationOutcome;
use super::open_items::OpenItemsSnapshot;

/// Identical rounds tolerated before the gate stops repeating itself and
/// re-approaches the work from a different angle.
///
/// Three is the same patience the rest of the harness spends before
/// escalating (blocked-attempt streak, continuation back-off), and it is the
/// smallest count that cannot be explained by one unlucky round.
pub const STALL_REAPPROACH_AFTER: u32 = 3;

/// Identical rounds before the gate gives up on the work it is spinning on.
///
/// Counted from the same streak, so the re-approach gets its own
/// [`STALL_REAPPROACH_AFTER`] rounds to take effect before the cut-off lands.
pub const STALL_CUTOFF_AFTER: u32 = STALL_REAPPROACH_AFTER * 2;

/// How stuck the run looks, from the gate's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StallLevel {
    /// Something the gate can observe changed in the last round.
    #[default]
    Progressing,
    /// Nothing changed for [`STALL_REAPPROACH_AFTER`] rounds. Keep going, but
    /// stop sending the message that is not working.
    Reapproach { repeats: u32 },
    /// Nothing changed for [`STALL_CUTOFF_AFTER`] rounds. The current work is
    /// not going to finish by asking again.
    CutOff { repeats: u32 },
}

impl StallLevel {
    /// Stable label for telemetry / tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Progressing => "progressing",
            Self::Reapproach { .. } => "reapproach",
            Self::CutOff { .. } => "cut_off",
        }
    }

    /// Consecutive identical rounds behind this level (1 when progressing).
    pub fn repeats(&self) -> u32 {
        match self {
            Self::Progressing => 1,
            Self::Reapproach { repeats } | Self::CutOff { repeats } => *repeats,
        }
    }
}

/// What a single round did, beyond the gate inputs themselves.
///
/// Tool names are part of the measure because the observed failure was a model
/// that re-read the same files and re-narrated the same plan every round: the
/// gate inputs were identical *and* so was the work. Requiring both to repeat
/// is deliberately conservative — a round that varies its tools is credited
/// with progress even if the checklist did not move, so a long implementation
/// stretch is never mistaken for a stall.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundActivity {
    /// Tool names the round invoked, in call order. Arguments are not
    /// included: re-reading a *different* file with the same tool is the
    /// exploration loop this guard exists to catch.
    pub tools_called: Vec<String>,
    /// Acceptance criteria verification has accepted so far.
    pub verified_criteria: usize,
    /// Acceptance criteria the run has given up on so far.
    pub deferred_criteria: usize,
}

/// Everything the gate can watch move between two rounds, as one string.
///
/// Compared verbatim, so any field changing counts as progress.
pub fn round_fingerprint(
    open: &OpenItemsSnapshot,
    verification: &VerificationOutcome,
    activity: &RoundActivity,
) -> String {
    let verdict = match verification {
        VerificationOutcome::Pending => "pending".to_string(),
        VerificationOutcome::Achieved => "achieved".to_string(),
        VerificationOutcome::Rejected { findings } => {
            // Panel order is not stable across runs; the set of complaints is.
            let mut messages: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
            messages.sort_unstable();
            format!("rejected:{}", messages.join("|"))
        }
    };
    format!(
        "open={}\nverdict={verdict}\nverified={}\ndeferred={}\ntools={}",
        open.summary_line(),
        activity.verified_criteria,
        activity.deferred_criteria,
        activity.tools_called.join(","),
    )
}

/// Consecutive-identical-round counter the host keeps beside its
/// [`super::TaskMachine`].
///
/// In-memory and per-prompt: a new prompt is a new intent, and carrying a
/// streak across one would cut off work the user just asked for.
#[derive(Debug, Clone, Default)]
pub struct StallTracker {
    last: Option<String>,
    repeats: u32,
}

impl StallTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the round that just ended and report how stuck the run looks.
    ///
    /// The level is sticky above [`STALL_REAPPROACH_AFTER`]: once the gate has
    /// escalated it must not fall back to the plain continue that already
    /// failed three times, or the run oscillates between the two messages
    /// instead of ever reaching the cut-off.
    pub fn observe(&mut self, fingerprint: String) -> StallLevel {
        match self.last.as_deref() {
            Some(previous) if previous == fingerprint => {
                self.repeats = self.repeats.saturating_add(1)
            }
            _ => self.repeats = 1,
        }
        self.last = Some(fingerprint);
        if self.repeats >= STALL_CUTOFF_AFTER {
            StallLevel::CutOff {
                repeats: self.repeats,
            }
        } else if self.repeats >= STALL_REAPPROACH_AFTER {
            StallLevel::Reapproach {
                repeats: self.repeats,
            }
        } else {
            StallLevel::Progressing
        }
    }

    /// Forget the streak after the host has changed the world itself (a
    /// cut-off deferral), so the next round is judged against the new state
    /// rather than the one that produced the cut-off.
    pub fn reset(&mut self) {
        self.last = None;
        self.repeats = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditFinding;
    use crate::open_items::OpenItem;

    fn open(summaries: &[&str]) -> OpenItemsSnapshot {
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

    fn activity(tools: &[&str]) -> RoundActivity {
        RoundActivity {
            tools_called: tools.iter().map(|t| (*t).to_string()).collect(),
            verified_criteria: 0,
            deferred_criteria: 0,
        }
    }

    fn fp(o: &OpenItemsSnapshot, v: &VerificationOutcome, a: &RoundActivity) -> String {
        round_fingerprint(o, v, a)
    }

    #[test]
    fn identical_rounds_escalate_then_cut_off() {
        let mut tracker = StallTracker::new();
        let (o, v, a) = (
            open(&["rebuild the binary"]),
            VerificationOutcome::Pending,
            activity(&["grep", "read_file"]),
        );
        let levels: Vec<StallLevel> = (0..STALL_CUTOFF_AFTER)
            .map(|_| tracker.observe(fp(&o, &v, &a)))
            .collect();
        assert_eq!(levels[0], StallLevel::Progressing);
        assert_eq!(levels[1], StallLevel::Progressing);
        assert_eq!(levels[2], StallLevel::Reapproach { repeats: 3 });
        assert_eq!(levels[4], StallLevel::Reapproach { repeats: 5 });
        assert_eq!(
            levels[(STALL_CUTOFF_AFTER - 1) as usize],
            StallLevel::CutOff {
                repeats: STALL_CUTOFF_AFTER
            },
            "the run must stop asking once the same round has happened {STALL_CUTOFF_AFTER} times",
        );
    }

    #[test]
    fn any_observable_change_restarts_the_streak() {
        let v = VerificationOutcome::Pending;
        let a = activity(&["read_file"]);
        // Each pair differs in exactly one component of the fingerprint.
        let variants: Vec<(OpenItemsSnapshot, VerificationOutcome, RoundActivity)> = vec![
            (open(&["a"]), v.clone(), a.clone()),
            (open(&["b"]), v.clone(), a.clone()),
            (open(&["b"]), VerificationOutcome::Achieved, a.clone()),
            (open(&["b"]), VerificationOutcome::Achieved, activity(&["bash"])),
            (
                open(&["b"]),
                VerificationOutcome::Achieved,
                RoundActivity {
                    verified_criteria: 1,
                    ..activity(&["bash"])
                },
            ),
            (
                open(&["b"]),
                VerificationOutcome::Achieved,
                RoundActivity {
                    verified_criteria: 1,
                    deferred_criteria: 1,
                    ..activity(&["bash"])
                },
            ),
        ];
        let mut tracker = StallTracker::new();
        for (o, ver, act) in &variants {
            assert_eq!(
                tracker.observe(fp(o, ver, act)),
                StallLevel::Progressing,
                "a round that changed something is progress, however small",
            );
        }
    }

    #[test]
    fn a_late_change_rescues_a_run_that_was_about_to_be_cut_off() {
        let mut tracker = StallTracker::new();
        let (o, v, a) = (
            open(&["stuck"]),
            VerificationOutcome::Pending,
            activity(&["read_file"]),
        );
        for _ in 0..(STALL_CUTOFF_AFTER - 1) {
            tracker.observe(fp(&o, &v, &a));
        }
        let moved = RoundActivity {
            verified_criteria: 1,
            ..a.clone()
        };
        assert_eq!(tracker.observe(fp(&o, &v, &moved)), StallLevel::Progressing);
        assert_eq!(
            tracker.observe(fp(&o, &v, &moved)),
            StallLevel::Progressing,
            "the streak restarts from the round that moved, not from the old count",
        );
    }

    #[test]
    fn reset_clears_the_streak_so_a_deferral_is_judged_fresh() {
        let mut tracker = StallTracker::new();
        let (o, v, a) = (
            open(&["stuck"]),
            VerificationOutcome::Pending,
            activity(&["read_file"]),
        );
        for _ in 0..STALL_CUTOFF_AFTER {
            tracker.observe(fp(&o, &v, &a));
        }
        tracker.reset();
        assert_eq!(tracker.observe(fp(&o, &v, &a)), StallLevel::Progressing);
    }

    #[test]
    fn rejection_fingerprint_ignores_panel_ordering_but_not_content() {
        let o = OpenItemsSnapshot::acceptance_only();
        let a = activity(&["read_file"]);
        let finding = |m: &str| AuditFinding {
            severity: Some("error".into()),
            criterion: None,
            message: m.to_string(),
        };
        let one = VerificationOutcome::Rejected {
            findings: vec![finding("no test"), finding("no impl")],
        };
        let reordered = VerificationOutcome::Rejected {
            findings: vec![finding("no impl"), finding("no test")],
        };
        let different = VerificationOutcome::Rejected {
            findings: vec![finding("no impl")],
        };
        assert_eq!(fp(&o, &one, &a), fp(&o, &reordered, &a));
        assert_ne!(fp(&o, &one, &a), fp(&o, &different, &a));
    }

    #[test]
    fn same_tool_on_different_files_is_not_progress() {
        // The observed failure: grep/read_file every round, different targets,
        // nothing written. Arguments are deliberately outside the fingerprint.
        let mut tracker = StallTracker::new();
        let (o, v, a) = (
            open(&["port the legacy loader"]),
            VerificationOutcome::Pending,
            activity(&["grep", "read_file", "read_file"]),
        );
        let mut last = StallLevel::Progressing;
        for _ in 0..STALL_CUTOFF_AFTER {
            last = tracker.observe(fp(&o, &v, &a));
        }
        assert!(matches!(last, StallLevel::CutOff { .. }));
    }
}
