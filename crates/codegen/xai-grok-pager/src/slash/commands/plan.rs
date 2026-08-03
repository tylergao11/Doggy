//! `/plan` — Plan mode removed; redirect to Goal.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Former plan-mode entry; now points users at Goal.
pub struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &str {
        "plan"
    }

    fn description(&self) -> &str {
        "Plan mode removed — use /goal <objective> or Shift+Tab for Goal"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn offered_when_session_less(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/plan (removed; use /goal)"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[ignored]")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        // Route description into Goal when provided; otherwise toast-only path
        // via SetPlanMode which already says plan is removed.
        if trimmed.is_empty() {
            return CommandResult::Action(Action::SetPlanMode(
                crate::app::actions::PlanModeKind::Off,
            ));
        }
        // EnterGoal-style: set Goal mode then prompt with objective.
        CommandResult::Action(Action::EnterPlanMode {
            description: Some(trimmed.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::actions::PlanModeKind;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn make_ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn no_args_dispatches_set_plan_mode_off_toast_path() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = make_ctx(&models, &bundle);
        match PlanCommand.run(&mut ctx, "") {
            CommandResult::Action(Action::SetPlanMode(PlanModeKind::Off)) => {}
            other => panic!("expected SetPlanMode(Off) removal path, got {other:?}"),
        }
    }

    #[test]
    fn with_description_still_routes_through_enter_plan_action() {
        // Dispatcher converts EnterPlanMode into a toast-only removal path
        // (dispatch_enter_plan_mode no longer activates Plan).
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = make_ctx(&models, &bundle);
        match PlanCommand.run(&mut ctx, "do the thing") {
            CommandResult::Action(Action::EnterPlanMode { description }) => {
                assert_eq!(description.as_deref(), Some("do the thing"));
            }
            other => panic!("expected EnterPlanMode action for desc, got {other:?}"),
        }
    }
}
