//! `/auto` — switch to Auto form (ordinary chat, full tool permission).
//!
//! Doggy resident postures: Plan / Auto / Goal. `/auto` exits plan mode and
//! uses full tool permission (the product default).

use crate::app::actions::{Action, PermissionModeKind};
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Enter Auto mode (ordinary chat with full tool permission).
pub struct AutoCommand;

impl SlashCommand for AutoCommand {
    fn name(&self) -> &str {
        "auto"
    }

    fn description(&self) -> &str {
        "Switch to Auto mode (ordinary chat, full tool permission)"
    }

    fn usage(&self) -> &str {
        "/auto"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::SetPermissionMode(PermissionModeKind::Auto))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    #[test]
    fn sets_auto() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: PagerLocalSnapshot::default(),
        };
        assert!(matches!(
            AutoCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::SetPermissionMode(PermissionModeKind::Auto))
        ));
    }
}
