//! Doggy **TaskOrchestrator** — completion authority for the runtime.
//!
//! # Why this module exists
//!
//! Upstream Grok Build conflates **round end** (model stops sampling / no
//! tool calls) with **task end**. Goal mode, laziness nudges, and classifiers
//! are optional side paths and can fail-open to "done".
//!
//! Doggy root-cause fix:
//!
//! - **Round** = one `process_conversation_turn` (want→do→want)
//! - **Task** = user goal until verification acceptance (goal classifier /
//!   skeptic panel) — **not** a separate Doggy audit subagent
//! - Only this module's [`decide::decide_after_round`] may return
//!   [`decide::TaskDecision::TaskDone`]
//!
//! Shell re-exports this crate as `xai_grok_shell::session::orchestrator`.
//!
//! See `docs/编排/完成权.md`.

pub mod audit;
pub mod audit_parse;
pub mod decide;
pub mod inject;
pub mod open_items;
pub mod progress;
pub mod state;

pub use audit::{AuditFinding, AuditVerdict};
pub use audit_parse::{AuditParseError, parse_audit_agent_output};
pub use decide::{
    RoundEndView, TaskDecision, TaskMachine, VerificationOutcome, decide_after_round,
};
pub use inject::Injection;
pub use open_items::{OpenItem, OpenItemsSnapshot};
pub use progress::{
    IDLE_CUTOFF_AFTER, RoundActivity, RoundDelta, RoundLedger, STALL_CUTOFF_AFTER,
    STALL_REAPPROACH_AFTER, StallLevel, StallReason, round_fingerprint,
};
pub use state::{PauseReason, TaskPhase, TaskStatus};
