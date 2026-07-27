//! Audit gate types (verdict only — spawning the subagent is host I/O).

/// One finding from the audit agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    /// Severity hint for inject text (`error`, `warning`, …). Free-form for now.
    pub severity: Option<String>,
    /// What failed or needs change.
    pub message: String,
}

/// Structured result of an audit round.
///
/// Host must parse the subagent output into this shape. Fail-open is **not**
/// represented: infra failure to obtain a verdict is a [`super::state::PauseReason::InfraError`]
/// (or re-run policy), never a synthetic pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditVerdict {
    /// `true` only when the auditor accepts the work against the goal.
    pub pass: bool,
    /// Empty when `pass`; otherwise reasons to fix.
    pub findings: Vec<AuditFinding>,
}

impl AuditVerdict {
    pub fn passed() -> Self {
        Self {
            pass: true,
            findings: Vec::new(),
        }
    }

    pub fn failed(findings: Vec<AuditFinding>) -> Self {
        Self {
            pass: false,
            findings,
        }
    }

    /// Format findings for a Fix injection body.
    pub fn findings_text(&self) -> String {
        if self.findings.is_empty() {
            return "Audit failed with no detailed findings; re-check the goal and diff."
                .to_string();
        }
        self.findings
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let sev = f
                    .severity
                    .as_deref()
                    .map(|s| format!("[{s}] "))
                    .unwrap_or_default();
                format!("{}. {sev}{}", i + 1, f.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
