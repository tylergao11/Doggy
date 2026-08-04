//! Zero-model-cost checks that run before the adversarial skeptic panel.
//!
//! Verification is the expensive part of a goal round: every attempt spawns a
//! panel of subagents that read the repo and argue about it. Most rejections in
//! practice are not subtle — the branch does not compile, the scoped test the
//! plan named is red, the binary the plan promised does not start. Paying three
//! subagents to discover `cargo check` fails is pure waste, and worse, it burns
//! the verification cap that the escalation ladder budgets for real gaps.
//!
//! So the plan may declare commands whose exit status decides pass/fail on its
//! own. They run first; a non-zero exit becomes a rejection with the command's
//! own output as the finding, routed straight back to Fix without a panel.
//!
//! The contract is deliberately narrow:
//! - Only `## Deterministic checks` rows are ever executed, so the set of
//!   commands is visible in the plan the user aligned on before the run.
//! - Commands are capped ([`MAX_CHECKS`]) and time-boxed
//!   ([`CHECK_TIMEOUT`]) so a hanging build cannot stall the goal forever.
//! - A check that cannot be run (spawn failure, timeout) is NOT a rejection.
//!   The gate exists to save money, not to invent verdicts: when it cannot
//!   answer, the panel decides as it always did.

use std::path::Path;
use std::time::Duration;

use crate::session::goal_plan_md::{is_any_header, is_section_header, is_separator_row, table_cells};

/// Plan section holding the executable checks.
const SECTION: &str = "Deterministic checks";

/// Most checks the harness will run in one round.
///
/// The gate is a cost optimisation, so it must stay cheap itself. A plan that
/// declares more is truncated rather than rejected: the extra rows are still
/// covered by the panel, which is the behaviour without the gate at all.
pub(crate) const MAX_CHECKS: usize = 6;

/// Wall-clock limit per check.
///
/// Long enough for a cold `cargo check` on a large workspace, short enough that
/// six hung commands cannot outlive a user's patience. On timeout the check is
/// abandoned (not failed) — see the module docs.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// Lines of command output kept for a failing check.
///
/// Compilers put the useful part last, and the whole point is to hand the
/// implementer something actionable without dumping a 10k-line build log into
/// the rejection nudge.
const OUTPUT_TAIL_LINES: usize = 25;

/// One executable pass/fail check declared by the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeterministicCheck {
    /// 1-based `## Acceptance criteria` number this check decides, when the
    /// plan scoped it to one. `None` means it gates the goal as a whole, so a
    /// failure cannot be narrowed to a single criterion.
    pub criterion: Option<u32>,
    /// Shell command line, run from the workspace root.
    pub command: String,
}

/// A check that ran and failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckFailure {
    pub criterion: Option<u32>,
    pub command: String,
    /// Exit status plus the tail of the command's combined output.
    pub detail: String,
}

impl CheckFailure {
    /// One-line finding for the rejection nudge, in the same shape the skeptic
    /// panel produces (`criterion N · …`) so downstream rendering, criterion
    /// attribution, and audit-mark clearing need no special case.
    pub(crate) fn finding_message(&self) -> String {
        let head = match self.criterion {
            Some(c) => format!("criterion {c} · check · `{}`", self.command),
            None => format!("check · `{}`", self.command),
        };
        format!("{head} — {}", self.detail)
    }
}

/// Parse the `## Deterministic checks` table, if the plan declares one.
///
/// Expected shape (the `Criterion` cell is `-` for a goal-wide check):
///
/// ```text
/// ## Deterministic checks
/// | # | Criterion | Command |
/// |---|-----------|---------|
/// | 1 | 2         | cargo check -p foo |
/// | 2 | -         | cargo test -p foo -- goal_plan |
/// ```
///
/// Rows that are unparseable or hold an empty command are skipped instead of
/// failing the goal: a malformed row costs the run its shortcut, not its
/// verdict.
pub(crate) fn parse_checks(body: &str) -> Vec<DeterministicCheck> {
    let mut checks = Vec::new();
    let mut in_section = false;
    let mut section_level = 0usize;
    for line in body.lines() {
        if is_section_header(line, SECTION) {
            in_section = true;
            section_level = crate::session::goal_plan_md::header_level(line);
            continue;
        }
        if in_section && is_any_header(line) && crate::session::goal_plan_md::header_level(line) <= section_level {
            break;
        }
        if !in_section {
            continue;
        }
        let cells = table_cells(line);
        if cells.len() < 3 || is_separator_row(&cells) {
            continue;
        }
        // A header row (`| # | Criterion | Command |`) has no numeric index.
        if cells[0].trim().parse::<u32>().is_err() {
            continue;
        }
        // Rejoin any trailing cells: a command may legitimately contain `|`
        // (`cargo test 2>&1 | tail`), and splitting on it must not truncate the
        // command the plan actually declared.
        let command = cells[2..].join(" | ").trim().to_string();
        if command.is_empty() {
            continue;
        }
        checks.push(DeterministicCheck {
            criterion: cells[1].trim().parse::<u32>().ok(),
            command,
        });
        if checks.len() == MAX_CHECKS {
            break;
        }
    }
    checks
}

/// Run `checks` from `cwd` and report the ones that failed, in plan order.
///
/// Stops at the first failure. The implementer can only act on one thing at a
/// time, and a red compile makes every later check meaningless — reporting the
/// consequences alongside the cause would send the fix in three directions.
pub(crate) async fn run_checks(cwd: &Path, checks: &[DeterministicCheck]) -> Option<CheckFailure> {
    for check in checks {
        match run_one(cwd, check).await {
            CheckOutcome::Passed => {}
            CheckOutcome::Failed(failure) => {
                tracing::info!(
                    command = %check.command,
                    criterion = ?check.criterion,
                    "deterministic gate: check failed; rejecting without a skeptic panel",
                );
                return Some(failure);
            }
            CheckOutcome::Unusable(why) => {
                tracing::warn!(
                    command = %check.command,
                    reason = %why,
                    "deterministic gate: check could not be run; leaving the verdict to the panel",
                );
            }
        }
    }
    None
}

enum CheckOutcome {
    Passed,
    Failed(CheckFailure),
    /// The check produced no verdict (could not spawn, or timed out).
    Unusable(String),
}

async fn run_one(cwd: &Path, check: &DeterministicCheck) -> CheckOutcome {
    let mut cmd = shell_command(&check.command);
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(CHECK_TIMEOUT, cmd.output()).await {
        Err(_) => CheckOutcome::Unusable(format!("no result within {CHECK_TIMEOUT:?}")),
        Ok(Err(e)) => CheckOutcome::Unusable(e.to_string()),
        Ok(Ok(out)) if out.status.success() => CheckOutcome::Passed,
        Ok(Ok(out)) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            CheckOutcome::Failed(CheckFailure {
                criterion: check.criterion,
                command: check.command.clone(),
                detail: format!(
                    "exited {}: {}",
                    out.status.code().map_or("(signal)".into(), |c| c.to_string()),
                    tail(&combined),
                ),
            })
        }
    }
}

/// Build the platform's shell invocation. Mirrors the hook runner so a command
/// that works in a hook works here.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
    #[cfg(not(unix))]
    {
        let inv = xai_grok_config::shell::shell_command_argv(command);
        let mut c = tokio::process::Command::new(&inv.program);
        c.args(&inv.args).envs(inv.env);
        c
    }
}

/// Last [`OUTPUT_TAIL_LINES`] non-blank lines, joined for a one-field finding.
fn tail(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return "(no output)".to_string();
    }
    let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
    lines[start..].join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "\
# Goal

## Acceptance criteria
| Exec | Audit | Criterion |
|------|-------|-----------|
| [x] | [ ] | it builds |

## Deterministic checks
| # | Criterion | Command |
|---|-----------|---------|
| 1 | 2 | cargo check -p foo |
| 2 | - | cargo test -p foo 2>&1 | tail -5 |

## Verification plan
1. gating: read the file
";

    #[test]
    fn parses_scoped_and_goal_wide_checks() {
        let checks = parse_checks(PLAN);
        assert_eq!(
            checks,
            vec![
                DeterministicCheck {
                    criterion: Some(2),
                    command: "cargo check -p foo".into(),
                },
                DeterministicCheck {
                    criterion: None,
                    command: "cargo test -p foo 2>&1 | tail -5".into(),
                },
            ],
            "a `-` criterion means goal-wide, and a piped command must survive \
             the table's own delimiter"
        );
    }

    #[test]
    fn stops_at_the_next_section_and_ignores_other_tables() {
        let checks = parse_checks(PLAN);
        assert!(
            checks.iter().all(|c| c.command.starts_with("cargo")),
            "rows from the acceptance table must not be read as commands: {checks:?}"
        );
    }

    #[test]
    fn a_plan_without_the_section_declares_no_checks() {
        assert!(parse_checks("# Goal\n\n## Acceptance criteria\n").is_empty());
    }

    #[test]
    fn malformed_rows_cost_the_shortcut_not_the_verdict() {
        let body = "## Deterministic checks\n| # | Criterion | Command |\n|---|---|---|\n\
                    | x | 1 | not-a-numbered-row |\n| 1 | 1 |  |\n| 2 | 1 | cargo check |\n";
        assert_eq!(
            parse_checks(body),
            vec![DeterministicCheck {
                criterion: Some(1),
                command: "cargo check".into(),
            }],
            "an unnumbered row and an empty command are skipped, not fatal"
        );
    }

    #[test]
    fn the_check_count_is_capped() {
        let mut body = String::from("## Deterministic checks\n| # | Criterion | Command |\n|---|---|---|\n");
        for i in 1..=(MAX_CHECKS + 4) {
            body.push_str(&format!("| {i} | - | echo {i} |\n"));
        }
        assert_eq!(
            parse_checks(&body).len(),
            MAX_CHECKS,
            "extra rows fall back to the panel rather than making the gate expensive"
        );
    }

    #[test]
    fn finding_message_matches_the_panel_shape() {
        let scoped = CheckFailure {
            criterion: Some(3),
            command: "cargo check".into(),
            detail: "exited 101: error[E0432]".into(),
        };
        assert_eq!(
            scoped.finding_message(),
            "criterion 3 · check · `cargo check` — exited 101: error[E0432]",
        );
        let wide = CheckFailure {
            criterion: None,
            ..scoped
        };
        assert!(
            wide.finding_message().starts_with("check · "),
            "an unscoped failure must not invent a criterion number"
        );
    }

    #[test]
    fn tail_keeps_the_end_and_drops_blank_lines() {
        let out = (1..=40).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("line{i}\n\n"));
            acc
        });
        let t = tail(&out);
        assert!(t.contains("line40"), "the useful part of a build log is last");
        assert!(!t.contains("line10"), "older lines are dropped: {t}");
        assert!(
            !t.contains(" /  / "),
            "blank lines must not become empty segments: {t}"
        );
    }

    #[test]
    fn no_output_is_reported_as_such() {
        assert_eq!(tail("\n\n  \n"), "(no output)");
    }

    #[tokio::test]
    async fn a_failing_check_short_circuits_the_rest() {
        let dir = std::env::temp_dir();
        let checks = vec![
            DeterministicCheck {
                criterion: Some(1),
                command: "exit 3".into(),
            },
            DeterministicCheck {
                criterion: Some(2),
                command: "exit 4".into(),
            },
        ];
        let failure = run_checks(&dir, &checks).await.expect("first check fails");
        assert_eq!(failure.criterion, Some(1));
        assert!(
            failure.detail.contains("exited 3"),
            "the exit status is what the implementer needs: {}",
            failure.detail
        );
    }

    #[tokio::test]
    async fn passing_checks_report_nothing() {
        let dir = std::env::temp_dir();
        let checks = vec![DeterministicCheck {
            criterion: None,
            command: "exit 0".into(),
        }];
        assert!(run_checks(&dir, &checks).await.is_none());
    }
}
