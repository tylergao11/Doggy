//! Round-to-round progress for the completion gate: what moved, and whether
//! the run is still moving at all.
//!
//! # The loop this module exists to break
//!
//! [`super::decide::decide_after_round`] has exactly one successful exit: no
//! explicit open items *and* verification `Achieved`. Everything else is
//! another round. That is right as a **completion** rule and fatal as a
//! **liveness** rule — a model that neither empties its todo list nor claims
//! completion is told to "continue" forever.
//!
//! Worse, it was told so in the *same words* every time. The continue
//! injection restated the open work and nothing else, and the host prunes the
//! previous copy before pushing the next, so a stalled run fed the model a
//! fixed point: identical instruction, unchanged workspace, identical answer.
//! The observed failure was not a model that could not do the work — it was a
//! model re-narrating its plan because nothing in its input had changed.
//!
//! So this module does two things, and the second is the one that matters:
//!
//! 1. [`RoundLedger`] measures whether anything the gate can observe moved,
//!    and escalates then cuts off when nothing does. That bounds the damage.
//! 2. It also produces a [`RoundDelta`] — what actually changed in the round
//!    that just ended — for the injection to carry. That removes the fixed
//!    point, because the model's input now differs every round whether or not
//!    it made progress, and names the thing it failed to move.
//!
//! # What counts as progress
//!
//! The primary signal is the set of **workspace files touched since the goal
//! baseline**, which the host reads from git. That is ground truth: it asks
//! whether the repository actually changed, not whether the model looked busy.
//! Tool names are a secondary component for the case where the host cannot
//! reach git, and they are weak on purpose — re-reading a different file with
//! the same tool is exactly the exploration loop this guard exists to catch.
//!
//! A round that calls no tools at all is handled separately and immediately:
//! under an active goal, right after being told to continue implementing, a
//! round with zero tool calls is narration by definition, and waiting for it
//! to repeat three times before saying so wastes three rounds.

use super::decide::VerificationOutcome;
use super::open_items::OpenItemsSnapshot;

/// Identical rounds tolerated before the gate stops repeating itself and
/// re-approaches the work from a different angle.
///
/// Three is the patience the rest of the harness spends before escalating
/// (blocked-attempt streak, continuation back-off), and the smallest count
/// that cannot be explained by one unlucky round.
pub const STALL_REAPPROACH_AFTER: u32 = 3;

/// Identical rounds before the gate gives up on the work it is spinning on.
/// Counted from the same streak, so the re-approach gets its own
/// [`STALL_REAPPROACH_AFTER`] rounds to take effect first.
pub const STALL_CUTOFF_AFTER: u32 = STALL_REAPPROACH_AFTER * 2;

/// Consecutive tool-less rounds before the gate gives up.
///
/// Much tighter than [`STALL_CUTOFF_AFTER`] because the evidence is much
/// stronger: a round that called nothing cannot have implemented anything, so
/// there is nothing to be patient about.
pub const IDLE_CUTOFF_AFTER: u32 = 3;

/// Why the gate thinks the run is stuck. Decides which message it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallReason {
    /// Consecutive rounds left every observable input unchanged.
    Repeated,
    /// The round ended without calling a single tool — narration, not work.
    Idle,
}

impl StallReason {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Repeated => "repeated",
            Self::Idle => "idle",
        }
    }
}

/// How stuck the run looks from the gate's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StallLevel {
    /// Something the gate can observe changed in the last round.
    #[default]
    Progressing,
    /// Keep going, but stop sending the message that is not working.
    Reapproach { repeats: u32, reason: StallReason },
    /// This work is not going to finish by asking again.
    CutOff { repeats: u32, reason: StallReason },
}

impl StallLevel {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Progressing => "progressing",
            Self::Reapproach { .. } => "reapproach",
            Self::CutOff { .. } => "cut_off",
        }
    }

    /// Consecutive bad rounds behind this level (0 when progressing).
    pub fn repeats(&self) -> u32 {
        match self {
            Self::Progressing => 0,
            Self::Reapproach { repeats, .. } | Self::CutOff { repeats, .. } => *repeats,
        }
    }

    pub fn reason(&self) -> Option<StallReason> {
        match self {
            Self::Progressing => None,
            Self::Reapproach { reason, .. } | Self::CutOff { reason, .. } => Some(*reason),
        }
    }

    /// Severity order, so two independent signals can be combined without the
    /// weaker one masking the stronger.
    fn severity(&self) -> u8 {
        match self {
            Self::Progressing => 0,
            Self::Reapproach { .. } => 1,
            Self::CutOff { .. } => 2,
        }
    }

    fn worst(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

/// The observable state of a round, as the host sees it.
///
/// Criterion numbers rather than counts, so the delta can name *which* one
/// moved instead of reporting that some number went up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundActivity {
    /// Workspace paths touched since the goal baseline, sorted.
    ///
    /// `None` means the host could not tell (no git baseline, git failed). It
    /// is not the same as `Some(vec![])` and must never be conflated with it:
    /// "I cannot see the workspace" is not evidence that the workspace is
    /// unchanged, and treating it as such would cut off a healthy run.
    pub changed_files: Option<Vec<String>>,
    /// Tool names the round invoked, in call order. Arguments are deliberately
    /// excluded — see the module docs.
    pub tools_called: Vec<String>,
    /// Criteria the implementer has ticked as done (Exec column).
    pub execed: Vec<u32>,
    /// Criteria independent verification has granted (Audit column).
    pub verified: Vec<u32>,
    /// Criteria the run has given up on.
    pub deferred: Vec<u32>,
}

impl RoundActivity {
    /// The round called at least one tool.
    pub fn worked(&self) -> bool {
        !self.tools_called.is_empty()
    }
}

/// What changed during the round that just ended.
///
/// Carried by the injection so the next round's input is never a repeat of the
/// last one, and so "you changed nothing" is stated rather than implied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundDelta {
    /// 1-based round number within this prompt.
    pub round: u32,
    /// Workspace paths touched for the first time this round.
    pub newly_changed_files: Vec<String>,
    /// Whether the host could see the workspace at all this round.
    pub workspace_visible: bool,
    /// Tool names the round invoked, in call order.
    pub tools_called: Vec<String>,
    /// Criteria newly ticked by the implementer this round.
    pub newly_execed: Vec<u32>,
    /// Criteria newly granted by verification this round.
    pub newly_verified: Vec<u32>,
    /// Criteria newly given up on this round.
    pub newly_deferred: Vec<u32>,
}

/// Files named individually in a delta before it summarises the rest.
const DELTA_FILES_SHOWN: usize = 8;

impl RoundDelta {
    /// The round called at least one tool.
    pub fn worked(&self) -> bool {
        !self.tools_called.is_empty()
    }

    /// Nothing observable happened: no tool ran, no file appeared, no
    /// criterion moved.
    pub fn moved_nothing(&self) -> bool {
        !self.worked()
            && self.newly_changed_files.is_empty()
            && self.newly_execed.is_empty()
            && self.newly_verified.is_empty()
            && self.newly_deferred.is_empty()
    }

    /// What the round did to the workspace, for the injection.
    pub fn workspace_line(&self) -> String {
        if !self.workspace_visible {
            return "workspace changes could not be read (no git baseline)".to_string();
        }
        if self.newly_changed_files.is_empty() {
            return "no new files were touched in the workspace".to_string();
        }
        let shown: Vec<&str> = self
            .newly_changed_files
            .iter()
            .take(DELTA_FILES_SHOWN)
            .map(String::as_str)
            .collect();
        let extra = self.newly_changed_files.len() - shown.len();
        if extra == 0 {
            format!("newly touched: {}", shown.join(", "))
        } else {
            format!("newly touched: {}, and {extra} more", shown.join(", "))
        }
    }

    /// `read_file ×3, grep` in call order, for the injection.
    pub fn tool_usage_line(&self) -> String {
        if self.tools_called.is_empty() {
            return "no tools were called".to_string();
        }
        let mut counted: Vec<(&str, usize)> = Vec::new();
        for name in &self.tools_called {
            match counted.iter_mut().find(|(seen, _)| *seen == name.as_str()) {
                Some((_, n)) => *n += 1,
                None => counted.push((name.as_str(), 1)),
            }
        }
        counted
            .into_iter()
            .map(|(name, n)| {
                if n == 1 {
                    name.to_string()
                } else {
                    format!("{name} \u{d7}{n}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Criterion movement, or `None` when none of the three columns moved.
    pub fn criteria_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        let list = |ns: &[u32]| {
            ns.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !self.newly_execed.is_empty() {
            parts.push(format!("marked done: {}", list(&self.newly_execed)));
        }
        if !self.newly_verified.is_empty() {
            parts.push(format!("verified: {}", list(&self.newly_verified)));
        }
        if !self.newly_deferred.is_empty() {
            parts.push(format!("given up on: {}", list(&self.newly_deferred)));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Everything the gate can watch move between two rounds, as one string.
/// Compared verbatim, so any component changing counts as progress.
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
    // An unreadable workspace collapses to one constant, so it contributes
    // nothing either way and the remaining components decide.
    let changed = match &activity.changed_files {
        Some(files) => files.join(","),
        None => "?".to_string(),
    };
    let numbers = |ns: &[u32]| ns.iter().map(u32::to_string).collect::<Vec<_>>().join("+");
    format!(
        "open={}\nverdict={verdict}\nchanged={changed}\nexec={}\nverified={}\ndeferred={}\ntools={}",
        open.summary_line(),
        numbers(&activity.execed),
        numbers(&activity.verified),
        numbers(&activity.deferred),
        activity.tools_called.join(","),
    )
}

/// Cross-round state the host keeps beside its [`super::TaskMachine`].
///
/// In-memory and per-prompt: a new prompt is a new intent, and carrying a
/// streak across one would cut off work the user just asked for.
#[derive(Debug, Clone, Default)]
pub struct RoundLedger {
    round: u32,
    last_fingerprint: Option<String>,
    repeats: u32,
    idle_streak: u32,
    changed_files: Vec<String>,
    execed: Vec<u32>,
    verified: Vec<u32>,
    deferred: Vec<u32>,
    verification_requested_for: Option<Vec<u32>>,
}

impl RoundLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the round that just ended: report how stuck the run looks and
    /// what changed since the previous round.
    pub fn observe(
        &mut self,
        open: &OpenItemsSnapshot,
        verification: &VerificationOutcome,
        activity: &RoundActivity,
    ) -> (StallLevel, RoundDelta) {
        self.round = self.round.saturating_add(1);

        let fingerprint = round_fingerprint(open, verification, activity);
        match self.last_fingerprint.as_deref() {
            Some(previous) if previous == fingerprint => {
                self.repeats = self.repeats.saturating_add(1)
            }
            _ => self.repeats = 1,
        }
        self.last_fingerprint = Some(fingerprint);

        if activity.worked() {
            self.idle_streak = 0;
        } else {
            self.idle_streak = self.idle_streak.saturating_add(1);
        }

        let seen_workspace = activity.changed_files.as_deref();
        let delta = RoundDelta {
            round: self.round,
            newly_changed_files: seen_workspace
                .map(|files| added_paths(&self.changed_files, files))
                .unwrap_or_default(),
            workspace_visible: seen_workspace.is_some(),
            tools_called: activity.tools_called.clone(),
            newly_execed: added(&self.execed, &activity.execed),
            newly_verified: added(&self.verified, &activity.verified),
            newly_deferred: added(&self.deferred, &activity.deferred),
        };
        // An unreadable workspace leaves the running set alone rather than
        // clearing it: one failed git call must not make every previously
        // touched file look new on the round after it recovers.
        if let Some(files) = seen_workspace {
            self.changed_files = files.to_vec();
        }
        self.execed = activity.execed.clone();
        self.verified = activity.verified.clone();
        self.deferred = activity.deferred.clone();

        (self.repeat_level().worst(self.idle_level()), delta)
    }

    /// Sticky above [`STALL_REAPPROACH_AFTER`]: once the gate has escalated it
    /// must not fall back to the plain continue that already failed three
    /// times, or the run oscillates between the two messages forever.
    fn repeat_level(&self) -> StallLevel {
        if self.repeats >= STALL_CUTOFF_AFTER {
            StallLevel::CutOff {
                repeats: self.repeats,
                reason: StallReason::Repeated,
            }
        } else if self.repeats >= STALL_REAPPROACH_AFTER {
            StallLevel::Reapproach {
                repeats: self.repeats,
                reason: StallReason::Repeated,
            }
        } else {
            StallLevel::Progressing
        }
    }

    /// A tool-less round is called out on the first offence: the gate just
    /// asked for implementation and got prose, and there is no reading of that
    /// under which waiting is the right move.
    fn idle_level(&self) -> StallLevel {
        if self.idle_streak >= IDLE_CUTOFF_AFTER {
            StallLevel::CutOff {
                repeats: self.idle_streak,
                reason: StallReason::Idle,
            }
        } else if self.idle_streak >= 1 {
            StallLevel::Reapproach {
                repeats: self.idle_streak,
                reason: StallReason::Idle,
            }
        } else {
            StallLevel::Progressing
        }
    }

    /// Whether the harness has already asked for acceptance on behalf of the
    /// implementer for this exact set of finished criteria.
    ///
    /// The gate's only successful exit needs a completion claim, and that claim
    /// is model-initiated: an implementer that ticks every box and then keeps
    /// talking never reaches it. The harness can ask on its behalf, but only
    /// once per set — asking again with nothing new would be the same fixed
    /// point one level up.
    pub fn verification_already_requested_for(&self, execed: &[u32]) -> bool {
        self.verification_requested_for
            .as_deref()
            .is_some_and(|previous| previous == execed)
    }

    pub fn mark_verification_requested(&mut self, execed: &[u32]) {
        self.verification_requested_for = Some(execed.to_vec());
    }

    /// Forget the streaks after the host has changed the world itself (a
    /// cut-off deferral), so the next round is judged against the new state
    /// rather than the one that produced the cut-off. Running totals are kept:
    /// they are cumulative state, not streaks.
    pub fn reset_streaks(&mut self) {
        self.last_fingerprint = None;
        self.repeats = 0;
        self.idle_streak = 0;
    }

    /// Rounds observed so far in this prompt.
    pub fn round(&self) -> u32 {
        self.round
    }
}

/// Entries in `now` that were not in `before`.
fn added(before: &[u32], now: &[u32]) -> Vec<u32> {
    now.iter().filter(|n| !before.contains(n)).copied().collect()
}

/// Paths in `now` that were not in `before`.
fn added_paths(before: &[String], now: &[String]) -> Vec<String> {
    now.iter()
        .filter(|p| !before.contains(p))
        .cloned()
        .collect()
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

    /// A round that called tools and left the workspace exactly as `files`.
    fn round_with(tools: &[&str], files: &[&str]) -> RoundActivity {
        RoundActivity {
            changed_files: Some(files.iter().map(|f| (*f).to_string()).collect()),
            tools_called: tools.iter().map(|t| (*t).to_string()).collect(),
            ..RoundActivity::default()
        }
    }

    #[test]
    fn a_round_that_touched_a_new_file_is_progress() {
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["port the loader"]), VerificationOutcome::Pending);
        ledger.observe(&o, &v, &round_with(&["write_file"], &["src/a.rs"]));
        let (level, delta) = ledger.observe(
            &o,
            &v,
            &round_with(&["write_file"], &["src/a.rs", "src/b.rs"]),
        );
        assert_eq!(level, StallLevel::Progressing);
        assert_eq!(delta.newly_changed_files, vec!["src/b.rs".to_string()]);
        assert_eq!(delta.workspace_line(), "newly touched: src/b.rs");
    }

    #[test]
    fn rounds_that_leave_the_workspace_alone_escalate_then_cut_off() {
        // Same tools, same files, nothing verified: the model is busy but the
        // repository is not moving.
        let mut ledger = RoundLedger::new();
        let (o, v, a) = (
            open(&["rebuild the binary"]),
            VerificationOutcome::Pending,
            round_with(&["grep", "read_file"], &["src/a.rs"]),
        );
        let levels: Vec<StallLevel> = (0..STALL_CUTOFF_AFTER)
            .map(|_| ledger.observe(&o, &v, &a).0)
            .collect();
        assert_eq!(levels[0], StallLevel::Progressing);
        assert_eq!(levels[1], StallLevel::Progressing);
        assert_eq!(
            levels[2],
            StallLevel::Reapproach {
                repeats: 3,
                reason: StallReason::Repeated
            }
        );
        assert_eq!(
            levels[(STALL_CUTOFF_AFTER - 1) as usize],
            StallLevel::CutOff {
                repeats: STALL_CUTOFF_AFTER,
                reason: StallReason::Repeated
            },
        );
    }

    #[test]
    fn an_unreadable_workspace_is_not_read_as_an_unchanged_one() {
        // git failing must not manufacture a stall, and must not make every
        // known file look new once it recovers.
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["x"]), VerificationOutcome::Pending);
        ledger.observe(&o, &v, &round_with(&["write_file"], &["src/a.rs"]));
        let blind = RoundActivity {
            changed_files: None,
            tools_called: vec!["write_file".into()],
            ..RoundActivity::default()
        };
        let (_, delta) = ledger.observe(&o, &v, &blind);
        assert!(!delta.workspace_visible);
        assert!(delta.newly_changed_files.is_empty());
        assert_eq!(
            delta.workspace_line(),
            "workspace changes could not be read (no git baseline)",
        );
        let (_, delta) = ledger.observe(
            &o,
            &v,
            &round_with(&["write_file"], &["src/a.rs", "src/b.rs"]),
        );
        assert_eq!(
            delta.newly_changed_files,
            vec!["src/b.rs".to_string()],
            "the blind round must not have discarded what was already known",
        );
    }

    #[test]
    fn a_tool_less_round_is_called_out_immediately() {
        // The observed failure: the model answered "continue implementing"
        // with a plan and no tool calls. Three rounds of patience here is
        // three rounds of prose.
        let mut ledger = RoundLedger::new();
        let (level, delta) = ledger.observe(
            &open(&["delete the old folder"]),
            &VerificationOutcome::Pending,
            &RoundActivity {
                changed_files: Some(Vec::new()),
                ..RoundActivity::default()
            },
        );
        assert_eq!(
            level,
            StallLevel::Reapproach {
                repeats: 1,
                reason: StallReason::Idle
            },
            "narration must not be answered with the same continue message",
        );
        assert!(delta.moved_nothing());
        assert_eq!(delta.tool_usage_line(), "no tools were called");
        assert_eq!(
            delta.workspace_line(),
            "no new files were touched in the workspace"
        );
    }

    #[test]
    fn consecutive_tool_less_rounds_cut_off_sooner_than_repeats_would() {
        let mut ledger = RoundLedger::new();
        let v = VerificationOutcome::Pending;
        let idle = RoundActivity {
            changed_files: Some(Vec::new()),
            ..RoundActivity::default()
        };
        // Each round differs in the open list, so the repeat measure never
        // fires; only the idle streak can stop this.
        for round in 1..IDLE_CUTOFF_AFTER {
            let summary = format!("step {round}");
            let (level, _) = ledger.observe(&open(&[&summary]), &v, &idle);
            assert!(matches!(
                level,
                StallLevel::Reapproach {
                    reason: StallReason::Idle,
                    ..
                }
            ));
        }
        let (level, _) = ledger.observe(&open(&["step last"]), &v, &idle);
        assert_eq!(
            level,
            StallLevel::CutOff {
                repeats: IDLE_CUTOFF_AFTER,
                reason: StallReason::Idle
            },
        );
        assert!(
            IDLE_CUTOFF_AFTER < STALL_CUTOFF_AFTER,
            "an idle run must not have to wait for the repeat budget",
        );
    }

    #[test]
    fn one_working_round_clears_the_idle_streak() {
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["x"]), VerificationOutcome::Pending);
        let idle = RoundActivity {
            changed_files: Some(Vec::new()),
            ..RoundActivity::default()
        };
        ledger.observe(&o, &v, &idle);
        ledger.observe(&o, &v, &idle);
        let (level, _) = ledger.observe(&o, &v, &round_with(&["write_file"], &["src/a.rs"]));
        assert_eq!(level, StallLevel::Progressing);
    }

    #[test]
    fn the_delta_names_which_criterion_moved() {
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["x"]), VerificationOutcome::Pending);
        let first = RoundActivity {
            execed: vec![1],
            ..round_with(&["write_file"], &["src/a.rs"])
        };
        let (_, delta) = ledger.observe(&o, &v, &first);
        assert_eq!(delta.round, 1);
        assert_eq!(delta.newly_execed, vec![1]);
        assert_eq!(delta.criteria_line().as_deref(), Some("marked done: 1"));

        let second = RoundActivity {
            execed: vec![1, 2],
            verified: vec![1],
            ..round_with(&["write_file", "write_file", "bash"], &["src/a.rs"])
        };
        let (_, delta) = ledger.observe(&o, &v, &second);
        assert_eq!(delta.round, 2);
        assert_eq!(
            delta.newly_execed,
            vec![2],
            "criterion 1 was already ticked; only new movement is a delta",
        );
        assert_eq!(delta.newly_verified, vec![1]);
        assert_eq!(
            delta.criteria_line().as_deref(),
            Some("marked done: 2; verified: 1")
        );
        assert_eq!(delta.tool_usage_line(), "write_file \u{d7}2, bash");
    }

    #[test]
    fn ticking_a_criterion_counts_as_progress_without_touching_a_file() {
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["x"]), VerificationOutcome::Pending);
        let a = round_with(&["read_file"], &["src/a.rs"]);
        ledger.observe(&o, &v, &a);
        ledger.observe(&o, &v, &a);
        let moved = RoundActivity {
            execed: vec![3],
            ..a.clone()
        };
        assert_eq!(ledger.observe(&o, &v, &moved).0, StallLevel::Progressing);
        assert_eq!(
            ledger.observe(&o, &v, &moved).0,
            StallLevel::Progressing,
            "the streak restarts from the round that moved",
        );
    }

    #[test]
    fn verification_is_requested_once_per_set_of_finished_criteria() {
        let mut ledger = RoundLedger::new();
        assert!(!ledger.verification_already_requested_for(&[1, 2]));
        ledger.mark_verification_requested(&[1, 2]);
        assert!(
            ledger.verification_already_requested_for(&[1, 2]),
            "asking again with nothing new is the same fixed point one level up",
        );
        assert!(
            !ledger.verification_already_requested_for(&[1, 2, 3]),
            "a newly finished criterion is new information and may be asked about",
        );
    }

    #[test]
    fn reset_streaks_clears_stalls_but_keeps_the_running_totals() {
        let mut ledger = RoundLedger::new();
        let (o, v) = (open(&["x"]), VerificationOutcome::Pending);
        let a = RoundActivity {
            execed: vec![1],
            ..round_with(&["read_file"], &["src/a.rs"])
        };
        for _ in 0..STALL_CUTOFF_AFTER {
            ledger.observe(&o, &v, &a);
        }
        ledger.reset_streaks();
        let (level, delta) = ledger.observe(&o, &v, &a);
        assert_eq!(level, StallLevel::Progressing);
        assert!(delta.newly_execed.is_empty());
        assert!(
            delta.newly_changed_files.is_empty(),
            "src/a.rs was already known before the reset; it is not new movement",
        );
    }

    #[test]
    fn rejection_fingerprint_ignores_panel_ordering_but_not_content() {
        let o = OpenItemsSnapshot::acceptance_only();
        let a = round_with(&["read_file"], &[]);
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
        assert_eq!(
            round_fingerprint(&o, &one, &a),
            round_fingerprint(&o, &reordered, &a)
        );
        assert_ne!(
            round_fingerprint(&o, &one, &a),
            round_fingerprint(&o, &different, &a)
        );
    }

    #[test]
    fn workspace_line_summarises_a_long_file_list() {
        let files: Vec<String> = (0..DELTA_FILES_SHOWN + 3)
            .map(|i| format!("src/f{i}.rs"))
            .collect();
        let delta = RoundDelta {
            newly_changed_files: files,
            workspace_visible: true,
            ..RoundDelta::default()
        };
        let line = delta.workspace_line();
        assert!(line.starts_with("newly touched: src/f0.rs"));
        assert!(line.ends_with("and 3 more"), "got {line}");
    }
}
