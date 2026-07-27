//! Parse structured output from the Doggy audit subagent.
//!
//! The agent must end with a JSON object (optionally fenced). No fail-open:
//! unparseable output is an error for the host to turn into a failed
//! [`AuditVerdict`], never a synthetic pass.

use crate::audit::{AuditFinding, AuditVerdict};

/// Errors while extracting an [`AuditVerdict`] from agent text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditParseError {
    /// No JSON object found in the response.
    NoJson,
    /// JSON found but invalid or missing required fields.
    Invalid(String),
}

impl std::fmt::Display for AuditParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoJson => f.write_str("audit agent response contained no JSON object"),
            Self::Invalid(d) => write!(f, "audit agent JSON invalid: {d}"),
        }
    }
}

/// Parse the audit subagent's terminal text into an [`AuditVerdict`].
///
/// Accepts:
/// - a bare JSON object
/// - a ```json fenced block
/// - the last `{...}` object in the text (agents often narrate then JSON)
pub fn parse_audit_agent_output(text: &str) -> Result<AuditVerdict, AuditParseError> {
    let json_str = extract_json_object(text).ok_or(AuditParseError::NoJson)?;
    let value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| AuditParseError::Invalid(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| AuditParseError::Invalid("root must be an object".into()))?;

    let pass = obj
        .get("pass")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| AuditParseError::Invalid("missing boolean field `pass`".into()))?;

    let findings = match obj.get("findings") {
        None => Vec::new(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(parse_finding)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(AuditParseError::Invalid(
                "`findings` must be an array".into(),
            ));
        }
    };

    // Invariant: pass with non-empty findings is treated as fail (strict).
    if pass && !findings.is_empty() {
        return Ok(AuditVerdict::failed(findings));
    }
    if pass {
        Ok(AuditVerdict::passed())
    } else {
        let findings = if findings.is_empty() {
            vec![AuditFinding {
                severity: Some("error".into()),
                message: "Audit reported pass=false with no findings; treat as incomplete."
                    .into(),
            }]
        } else {
            findings
        };
        Ok(AuditVerdict::failed(findings))
    }
}

fn parse_finding(v: &serde_json::Value) -> Result<AuditFinding, AuditParseError> {
    let obj = v
        .as_object()
        .ok_or_else(|| AuditParseError::Invalid("finding must be an object".into()))?;
    let message = obj
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuditParseError::Invalid("finding missing non-empty `message`".into()))?
        .to_string();
    let severity = obj
        .get("severity")
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(AuditFinding { severity, message })
}

/// Pull the first fenced ```json block, else a balanced `{...}` that looks
/// like an audit verdict (contains `"pass"`). Nested finding objects must
/// not win over the root.
fn extract_json_object(text: &str) -> Option<String> {
    if let Some(from_fence) = extract_fenced_json(text) {
        return Some(from_fence);
    }
    extract_audit_brace_object(text)
}

fn extract_fenced_json(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    // Prefer language-tagged fences first.
    for marker in ["```json", "```"] {
        let mut search_from = 0;
        while let Some(rel) = lower[search_from..].find(marker) {
            let start = search_from + rel;
            let after_marker = start + marker.len();
            let rest = text.get(after_marker..)?;
            let rest = rest.strip_prefix('\r').unwrap_or(rest);
            let rest = rest.strip_prefix('\n').unwrap_or(rest);
            let Some(end) = rest.find("```") else {
                search_from = after_marker;
                continue;
            };
            let body = rest[..end].trim();
            if body.starts_with('{') {
                return Some(body.to_string());
            }
            search_from = after_marker;
        }
    }
    None
}

/// Prefer the first balanced object whose text contains `"pass"`; otherwise
/// the first balanced object (so bare `{}` still surfaces as Invalid).
fn extract_audit_brace_object(text: &str) -> Option<String> {
    let mut first: Option<String> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        if let Some(obj) = extract_balanced_from(text, i) {
            if first.is_none() {
                first = Some(obj.clone());
            }
            if obj.contains("\"pass\"") {
                return Some(obj);
            }
            i += obj.len().max(1);
        } else {
            i += 1;
        }
    }
    first
}

fn extract_balanced_from(text: &str, start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if start >= bytes.len() || bytes[start] != b'{' {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_pass() {
        let v = parse_audit_agent_output(r#"{"pass": true, "findings": []}"#).unwrap();
        assert!(v.pass);
        assert!(v.findings.is_empty());
    }

    #[test]
    fn parses_fail_with_findings() {
        let text = r#"{
          "pass": false,
          "findings": [
            {"severity": "error", "message": "tests missing"},
            {"message": "goal criterion 2 unmet"}
          ]
        }"#;
        let v = parse_audit_agent_output(text).unwrap();
        assert!(!v.pass);
        assert_eq!(v.findings.len(), 2);
        assert_eq!(v.findings[0].severity.as_deref(), Some("error"));
        assert!(v.findings[1].message.contains("criterion 2"));
    }

    #[test]
    fn parses_fenced_json_after_prose() {
        let text = r#"
I reviewed the workspace against the objective.

```json
{"pass": false, "findings": [{"message": "README still empty"}]}
```
"#;
        let v = parse_audit_agent_output(text).unwrap();
        assert!(!v.pass);
        assert!(v.findings[0].message.contains("README"));
    }

    #[test]
    fn parses_trailing_object_after_narration() {
        let text = r#"
Checked the diff. Several issues remain.
{"pass": false, "findings": [{"severity": "warning", "message": "no integration test"}]}
"#;
        let v = parse_audit_agent_output(text).unwrap();
        assert!(!v.pass);
        assert_eq!(v.findings[0].severity.as_deref(), Some("warning"));
    }

    #[test]
    fn pass_with_findings_becomes_fail() {
        let text = r#"{"pass": true, "findings": [{"message": "nit"}]}"#;
        let v = parse_audit_agent_output(text).unwrap();
        assert!(!v.pass);
        assert_eq!(v.findings.len(), 1);
    }

    #[test]
    fn fail_without_findings_gets_placeholder() {
        let v = parse_audit_agent_output(r#"{"pass": false}"#).unwrap();
        assert!(!v.pass);
        assert_eq!(v.findings.len(), 1);
        assert!(v.findings[0].message.contains("pass=false"));
    }

    #[test]
    fn missing_pass_is_error() {
        let err = parse_audit_agent_output(r#"{"findings": []}"#).unwrap_err();
        assert!(matches!(err, AuditParseError::Invalid(_)));
    }

    #[test]
    fn no_json_is_error() {
        let err = parse_audit_agent_output("looks fine to me, ship it").unwrap_err();
        assert_eq!(err, AuditParseError::NoJson);
    }
}
