//! Open-items snapshot for the completion gate.
//!
//! A task is not eligible for audit until there is no remaining work *and*
//! acceptance is no longer pending. The implicit "acceptance pending" item
//! prevents the model from emptying the list by talking alone.

/// One structured unfinished item (goal checklist line, todo, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenItem {
    /// Stable id when available (for telemetry / inject text).
    pub id: Option<String>,
    /// Human-readable description.
    pub summary: String,
}

/// Point-in-time view of remaining work for `decide()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenItemsSnapshot {
    /// Explicit unfinished items from goal / checklist trackers.
    pub items: Vec<OpenItem>,
    /// Implicit gate: task has not yet been accepted by audit.
    ///
    /// Starts `true` when a task begins. Cleared only after a passing audit
    /// (host applies that side effect; pure `decide` only reads the flag).
    pub acceptance_pending: bool,
}

impl OpenItemsSnapshot {
    /// Empty explicit list but still waiting for audit acceptance.
    pub fn acceptance_only() -> Self {
        Self {
            items: Vec::new(),
            acceptance_pending: true,
        }
    }

    /// No explicit items and acceptance already cleared — rare mid-decide view
    /// used only after a passing audit is applied.
    pub fn fully_clear() -> Self {
        Self {
            items: Vec::new(),
            acceptance_pending: false,
        }
    }

    /// Explicit unfinished work remains (continue implementing, do not audit yet).
    pub fn has_explicit_work(&self) -> bool {
        !self.items.is_empty()
    }

    /// Anything that blocks [`super::decide::TaskDecision::TaskDone`].
    ///
    /// Explicit items **or** acceptance still pending both count as open.
    pub fn blocks_done(&self) -> bool {
        self.has_explicit_work() || self.acceptance_pending
    }

    /// Eligible to run the audit gate: no explicit items left.
    ///
    /// Acceptance may still be pending — that is why we audit.
    pub fn ready_for_audit(&self) -> bool {
        !self.has_explicit_work()
    }

    /// Short text for continue injections / tests.
    pub fn summary_line(&self) -> String {
        if self.items.is_empty() {
            if self.acceptance_pending {
                "acceptance pending (awaiting audit)".to_string()
            } else {
                "no open items".to_string()
            }
        } else {
            let heads: Vec<&str> = self
                .items
                .iter()
                .take(5)
                .map(|i| i.summary.as_str())
                .collect();
            let extra = self.items.len().saturating_sub(heads.len());
            if extra == 0 {
                format!("open items: {}", heads.join("; "))
            } else {
                format!(
                    "open items: {}; …and {extra} more",
                    heads.join("; ")
                )
            }
        }
    }
}
