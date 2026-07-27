//! `/always-approve` — deprecated alias of `/auto`.
//!
//! Kept so old muscle memory still works; product name is Auto.

use crate::app::actions::{Action, PermissionModeKind};
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Deprecated alias of Auto mode.
pub struct AlwaysApproveCommand;

impl SlashCommand for AlwaysApproveCommand {
    fn name(&self) -> &str {
        "always-approve"
    }

    fn description(&self) -> &str {
        "Alias of /auto (tools auto-run)"
    }

    fn usage(&self) -> &str {
        "/always-approve"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::SetPermissionMode(PermissionModeKind::Auto))
    }
}

