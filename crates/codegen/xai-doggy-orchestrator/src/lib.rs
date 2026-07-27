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
//! - **Task** = user goal until audit acceptance
//! - Only this module's [`decide::decide_after_audit`] may return
//!   [`decide::TaskDecision::TaskDone`]
//!
//! Phase A ships pure policy + unit tests. Host wiring into `handle_prompt`
//! is Phase B+.
//!
//! Shell re-exports this crate as `xai_grok_shell::session::orchestrator`.
//!
//! See `docs/编排/完成权.md`.

pub mod audit;
pub mod audit_parse;
pub mod decide;
pub mod inject;
pub mod open_items;
pub mod state;

pub use audit::{AuditFinding, AuditVerdict};
pub use audit_parse::{AuditParseError, parse_audit_agent_output};
pub use decide::{
    AuditEndView, RoundEndView, TaskDecision, TaskMachine, decide_after_audit, decide_after_round,
};
pub use inject::Injection;
pub use open_items::{OpenItem, OpenItemsSnapshot};
pub use state::{PauseReason, TaskPhase, TaskStatus};
