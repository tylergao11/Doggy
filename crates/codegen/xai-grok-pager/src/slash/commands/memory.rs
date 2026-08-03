//! `/memory` -- review memory edits staged by post-session reflection.
//!
//! Approval is pull-based on purpose. Reflection never interrupts a run: under
//! `[memory.reflection] apply = "staged"` it parks its proposals in a queue and
//! the status bar shows a passive count. This command is the only thing that
//! acts on that queue, so an unattended run is never waiting on a human.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

/// Inspect and resolve the staged-memory queue.
pub struct MemoryCommand;

const SUBCOMMANDS: &[(&str, &str)] = &[
    ("pending", "List memory edits awaiting approval"),
    ("approve", "Apply every staged edit to MEMORY.md"),
    ("discard", "Drop every staged edit"),
];

impl SlashCommand for MemoryCommand {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Review staged memory edits"
    }

    fn usage(&self) -> &str {
        "/memory [pending|approve|discard]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[pending|approve|discard]")
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(
            SUBCOMMANDS
                .iter()
                .map(|(name, description)| ArgItem {
                    display: (*name).to_owned(),
                    match_text: (*name).to_owned(),
                    insert_text: (*name).to_owned(),
                    description: (*description).to_owned(),
                })
                .collect(),
        )
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        // Bare `/memory` lists rather than doing anything: the destructive
        // choices stay explicit.
        match args.trim() {
            "" | "pending" => CommandResult::Action(Action::ShowMemoryPending),
            "approve" => CommandResult::Action(Action::ResolveMemoryPending { approve: true }),
            "discard" => CommandResult::Action(Action::ResolveMemoryPending { approve: false }),
            other => CommandResult::Error(format!(
                "Unknown subcommand '{other}'. Usage: /memory [pending|approve|discard]"
            )),
        }
    }
}
