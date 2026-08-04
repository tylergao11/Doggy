//! Plan, yolo, auto, and permission mode transitions and toasts.

use super::ctx::with_active_agent;
use super::session::lifecycle::skip_picker_and_create_session;
use super::settings::ui::refresh_open_settings_modals;
use crate::app::actions::Effect;
use crate::app::app_view::{ActiveView, AppView};
use agent_client_protocol as acp;
use xai_grok_telemetry::session_ctx::log_event;

/// Show the current plan: if a plan file exists, open it in the preview
/// overlay popover. If no plan has been written yet, show a toast.
///
/// Delegates to `AgentView::show_plan_preview()` which reads the plan file
/// from `~/.Doggy/sessions/<urlencoded_cwd>/<session_id>/plan.md`.
pub(super) fn dispatch_show_plan(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        if agent.plan_approval_view.is_some() {
            agent.reopen_plan_approval();
        } else {
            agent.show_plan_preview();
        }
    });
    vec![]
}

/// `/plan` is removed with Plan mode. Point users at Goal.
pub(super) fn dispatch_enter_plan_mode(
    app: &mut AppView,
    description: Option<String>,
) -> Vec<Effect> {
    let _ = description;
    let ActiveView::Agent(id) = app.active_view else {
        app.show_toast("Plan mode is removed. Use Goal (Shift+Tab) or /goal <objective>.");
        return vec![];
    };
    if let Some(agent) = app.agents.get_mut(&id) {
        agent.show_toast("Plan mode is removed. Use Shift+Tab for Goal or /goal <objective>.");
    }
    vec![]
}

/// Set plan mode (on / off). PAGER-owned + ACP-mediated, per-session.
///
/// Optimistic flow: captures effective state (`pending.or(active)`),
/// sets `plan_mode_pending`, refreshes modals, toasts, then emits
/// `Effect::SetSessionMode`. Shell confirms via `CurrentModeUpdate`.
///
/// No explicit rollback �?`SetSessionMode` has no failure surface.
/// If the ACP transport drops, `plan_mode_pending` stays set until
/// the next `CurrentModeUpdate` or session restart.
///
/// Idempotent: same value toasts but skips the ACP round-trip.
pub(super) fn set_plan_mode(
    app: &mut AppView,
    kind: crate::app::actions::PlanModeKind,
) -> Vec<Effect> {
    let _ = kind;
    app.show_toast("Plan mode is removed. Use Goal (Shift+Tab) or /goal <objective>.");
    vec![]
}

/// The single gate for client paths that ENABLE always-approve: `Some(reason)`
/// iff `enabling` and the pin (`app.yolo_policy_block`) is set. Every enabling
/// path routes through here (or [`refuse_if_yolo_locked`]) so new paths stay
/// gated by default; callers must NOT persist on a refusal.
pub(super) fn yolo_enable_blocked(app: &AppView, enabling: bool) -> Option<&'static str> {
    if enabling {
        app.yolo_policy_block
    } else {
        None
    }
}

/// `Vec<Effect>` wrapper for the persisting setters: on a refusal, toast and
/// return `Some(vec![])` (no persist); `None` means proceed.
fn refuse_if_yolo_locked(app: &mut AppView, enabling: bool) -> Option<Vec<Effect>> {
    let warning = yolo_enable_blocked(app, enabling)?;
    app.show_toast(warning);
    Some(vec![])
}

/// Canonical "auto wins only when yolo is off" precedence �?the single source
/// of truth for the yolo-over-auto rule applied at every reconnect / seed / meta
/// site. Callers pass the already-resolved auto signal (a per-session flag or a
/// `permission_mode == Some("auto")` test).
pub(crate) fn effective_auto(yolo: bool, auto: bool) -> bool {
    !yolo && auto
}

/// When the auto gate is off, force the displayed permission mode off Auto and
/// clear every agent's per-session auto flag, so the UI / Shift+Tab cycle /
/// settings snapshot and each tab's badge never show Auto while the feature is
/// disabled. Shared by the startup reconcile and the mid-session kill-switch.
/// Clearing every agent (not just when the global mirror still reads "auto")
/// matters because `switch_to_agent` re-anchors the mirror to the active tab.
pub(crate) fn downgrade_displayed_auto_if_gated(app: &mut AppView) {
    if app.auto_mode_gate {
        return;
    }
    for agent in app.agents.values_mut() {
        agent.session.auto_mode = false;
    }
    if app.current_ui.permission_mode.as_deref() == Some("auto") {
        // Classifier gate off: stay on product Auto (full auto-run).
        app.current_ui.permission_mode = Some("auto".into());
    }
}

/// Whether a newly created session should start with the **classifier**
/// `auto_mode` flag.
///
/// **Doggy product:** UI/config `"auto"` means full tool allow (yolo), not the
/// retired classifier tier. Always `false` so product `"auto"` cannot seed
/// classifier prompting into new sessions.
pub(super) fn inherit_auto_mode(_app: &AppView) -> bool {
    false
}

/// Keep the active session's classifier `auto_mode` flag off under the Doggy
/// product model. Product `"auto"` is full allow (`yolo`); it must not set the
/// session classifier flag. Still refreshes slash-gate visibility.
pub(super) fn sync_active_auto_flag(app: &mut AppView) {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.session.auto_mode = false;
    }
    // Keep `/auto` feature-gate visibility in lockstep across slash surfaces.
    app.sync_permission_mode_slash_gate();
}

/// State-only `permission_mode` (YOLO) mutation; also called from rollback.
/// Flips to ON are refused while the pin is set.
pub(super) fn set_yolo_mode_inner(app: &mut AppView, new: bool) {
    if yolo_enable_blocked(app, new).is_some() {
        tracing::warn!("always-approve enable blocked by managed policy");
        return;
    }
    // Global mirrors update unconditionally (even if the user navigated
    // away from the agent mid-rollback). Per-agent state is gated below.
    app.default_yolo = new;
    app.permission_mode_from_soft_default = false;
    // Write-only mirror �?see fn doc-comment.
    app.current_ui.permission_mode = Some(if new { "auto" } else { "auto" }.to_string());

    let ActiveView::Agent(id) = app.active_view else {
        return;
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };

    let previous_state = agent.session.is_yolo();

    // Drain ordering invariant: flag flip BEFORE the drain (see fn
    // doc-comment). Do NOT reorder these without re-reading the
    // contract.
    agent.session.yolo_mode = new;

    if new {
        // YOLO ON: auto-approve all queued permissions. Drain runs
        // even on idempotent re-dispatch. Prefers `AllowOnce`; falls
        // back to `Cancelled` (never `AllowAlways`).
        agent.last_permission_click = None;
        for perm in agent.permission_queue.drain(..) {
            if let Some(allow) = perm
                .options
                .iter()
                .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
            {
                perm.request
                    .response_tx
                    .send(Ok(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(allow.option_id.clone()),
                        ),
                    )))
                    .ok();
            } else {
                perm.request
                    .response_tx
                    .send(Ok(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    )))
                    .ok();
            }
        }
        // Restore stashed prompt since queue is now empty.
        if let Some(stashed) = agent.permission_stashed_prompt.take() {
            agent.prompt.restore(stashed);
        }
    }

    // Telemetry + tracing guarded on real state change only.
    if previous_state != new {
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::YoloToggled {
            enabled: new,
            previous_state,
            trigger: xai_grok_telemetry::events::YoloTrigger::Pager,
        });
        tracing::info!(target: "settings", key = "permission_mode", value = new, "setting changed");
    }
}

/// Set YOLO (`permission_mode`). SHELL-owned, emits
/// `Effect::PersistPermissionMode` with rollback. The drain runs
/// unconditionally on YOLO=ON (even duplicate dispatches) because
/// a permission could arrive between dispatches.
fn capture_prev_permission_canonical(_app: &AppView, _prev_yolo: bool) -> &'static str {
    // Doggy only persists Auto.
    "auto"
}

pub(super) fn set_yolo_mode(app: &mut AppView, new: bool) -> Vec<Effect> {
    // Managed policy pins always-approve off �?no state change, no persist.
    if let Some(blocked) = refuse_if_yolo_locked(app, new) {
        return blocked;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    // Capture LIVE yolo + plan state and session_id atomically for rollback.
    let (prev_yolo, session_id, effective_plan) = app
        .agents
        .get(&id)
        .map(|a| {
            (
                a.session.is_yolo(),
                a.session.session_id.clone(),
                a.plan_mode_pending.unwrap_or(a.plan_mode_active),
            )
        })
        .unwrap_or((false, None, false));
    let prev_canonical = capture_prev_permission_canonical(app, prev_yolo);

    set_yolo_mode_inner(app, new);

    // Refresh modal snapshots so the indicator reflects the new value.
    refresh_open_settings_modals(app);
    // Toggling yolo always lands on ask/always-approve (never auto); keep the
    // per-session auto display flag in sync (clears it).
    sync_active_auto_flag(app);

    // Toast on every save. YOLO ON gets a weightier visual; under an active
    // plan mode, say the plan edit gate stays binding �?"all tool actions
    // auto-run" would overpromise while the shell rejects non-plan-file edits.
    if new && effective_plan {
        app.show_toast(YOLO_ON_UNDER_PLAN_TOAST);
    } else {
        app.show_toast(&yolo_toast(new));
    }

    // Forward write is always "ask" or "always-approve" (bool entry
    // point). Rollback uses `prev_canonical` with LIVE precedence.
    let canonical: &'static str = "auto";
    vec![Effect::PersistPermissionMode {
        canonical,
        session_id,
        persist: crate::app::actions::PermissionModePersist::WithRollback(prev_canonical),
    }]
}

/// Set permission mode by typed kind. Entry point from the settings
/// modal. Mirrors `set_yolo_mode` but preserves the canonical string
/// (the inner collapses "default" onto "ask"; this setter restores
/// the distinction by overriding `app.current_ui.permission_mode`
/// after the inner call). Rollback uses LIVE-precedence canonical.
pub(super) fn set_permission_mode(
    app: &mut AppView,
    kind: crate::app::actions::PermissionModeKind,
) -> Vec<Effect> {
    // Doggy: Auto is full tool auto-run (yolo). The old classifier `auto_mode_gate`
    // no longer demotes Auto �?Ask.
    let kind = kind;
    // Managed policy pins auto off �?keep the modal on live state.
    if let Some(blocked) = refuse_if_yolo_locked(app, kind.is_always_approve()) {
        refresh_open_settings_modals(app);
        return blocked;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    // Capture LIVE yolo + plan state and session_id atomically for rollback.
    let (prev_yolo, session_id, effective_plan) = app
        .agents
        .get(&id)
        .map(|a| {
            (
                a.session.is_yolo(),
                a.session.session_id.clone(),
                a.plan_mode_pending.unwrap_or(a.plan_mode_active),
            )
        })
        .unwrap_or((false, None, false));
    let prev_canonical = capture_prev_permission_canonical(app, prev_yolo);

    // State mutation via shared inner. We overwrite the canonical
    // below for the Default case. Inner clears the soft-default latch.
    set_yolo_mode_inner(app, kind.is_always_approve());

    // Restore the "default" distinction the inner's bool-projection
    // collapses. No-op for `AlwaysApprove` and `Ask`.
    app.current_ui.permission_mode = Some(kind.as_canonical().to_string());

    // Refresh modal so its snapshot reflects the overridden canonical.
    refresh_open_settings_modals(app);
    // Classifier session flag stays off (product Auto is full allow / yolo).
    sync_active_auto_flag(app);

    // Toast on every save (plan-aware for AlwaysApprove, mirroring
    // `set_yolo_mode` �?the plan edit gate stays binding under yolo).
    if kind.is_always_approve() && effective_plan {
        app.show_toast(YOLO_ON_UNDER_PLAN_TOAST);
    } else {
        app.show_toast(&permission_mode_toast(kind));
    }

    vec![Effect::PersistPermissionMode {
        canonical: kind.as_canonical(),
        session_id,
        persist: crate::app::actions::PermissionModePersist::WithRollback(prev_canonical),
    }]
}

/// Build the toast for a `permission_mode` commit. `AlwaysApprove`
/// reuses `yolo_toast(true)` (destructive). `Ask` and `Default` get
/// dedicated "Permission mode: ..." toasts matching the picker brand.
pub(super) fn permission_mode_toast(_kind: crate::app::actions::PermissionModeKind) -> String {
    yolo_toast(true)
}

/// YOLO-ON toast when plan mode is active: always-approve arms the permission
/// fast path, but the shell's plan-mode gate still rejects non-plan-file
/// edits, so the standard "all tool actions auto-run" would overpromise.
pub(super) const YOLO_ON_UNDER_PLAN_TOAST: &str =
    "\u{26A0} Auto ON: plan mode still blocks free file edits until you exit Plan";

/// Build the Auto toast (full tool auto-run).
fn yolo_toast(new: bool) -> String {
    if new {
        "\u{2713} Auto: tools auto-run".to_string()
    } else {
        // Product has no Ask tier �?turning "off" still lands on Auto semantics.
        "\u{2713} Auto".to_string()
    }
}

/// Toggle YOLO mode (Ctrl+O keybinding path). Delegates to the
/// registry-driven `set_yolo_mode` so permission-queue draining,
/// telemetry, and persistence all flow through a single code path.
pub(super) fn dispatch_toggle_yolo(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get(&id) else {
        return vec![];
    };
    let new = !agent.session.yolo_mode;
    set_yolo_mode(app, new)
}

/// Shift+Tab mode cycle from the agent chat view: the shared cycle body plus
/// plan-nudge acceptance telemetry (the nudge advertises this chord). The
/// dashboard peek calls [`dispatch_cycle_mode_and_sync`] instead, so a peeked
/// agent �?whose prompt the user is not looking at �?never attributes an accept
/// and never collapses Auto/Always-Approve for the nudge jump.
pub(super) fn dispatch_cycle_mode(app: &mut AppView) -> Vec<Effect> {
    // Capture the pre-cycle nudge visibility + plan state so only a transition
    // into Plan taken while the nudge is on screen attributes as an acceptance;
    // a disabled/absent nudge never emits.
    let (nudge_showing, in_plan_before) = active_agent_plan_nudge_state(app);
    // Tip copy promises one Shift+Tab �?Plan; collapse Auto/Always-Approve to
    // ask first so the ring's Normal→Plan arm is the sole Plan entry.
    let mut effects = collapse_to_ask_for_nudge_jump(app).unwrap_or_default();
    effects.extend(dispatch_cycle_mode_and_sync(app));
    // Re-read only `in_plan`, via the same mut agent handle used to retire the
    // nudge: entering Plan with the nudge up is an acceptance.
    if nudge_showing
        && !in_plan_before
        && let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
        && agent.plan_mode_pending.unwrap_or(agent.plan_mode_active)
    {
        log_event(xai_grok_telemetry::events::ContextualTip {
            tip: xai_grok_telemetry::events::ContextualTipKind::PlanMode,
            action: xai_grok_telemetry::events::ContextualTipAction::Accepted,
        });
        // Retire the now-stale nudge so one impression maps to at most one
        // acceptance �?a full mode loop back to Plan within the ~3s TTL would
        // otherwise re-emit �?unifying with the undo/image tips' clear-on-accept.
        agent
            .ephemeral_tip
            .clear(crate::tips::plan_nudge::PLAN_NUDGE_KEY);
    }
    effects
}

/// When the plan nudge is showing and the active agent is in Auto or
/// Always-Approve, collapse permission to ask (no banner / no Plan effects)
/// so the subsequent ring step is Normal→Plan. Returns `None` when the ring
/// should run alone (Normal, absent nudge, already-in-plan, or no session).
/// Agent-view only �?peek never calls this.
fn collapse_to_ask_for_nudge_jump(app: &mut AppView) -> Option<Vec<Effect>> {
    let ActiveView::Agent(id) = app.active_view else {
        return None;
    };
    let agent = app.agents.get(&id)?;
    if agent.ephemeral_tip.current_key() != Some(crate::tips::plan_nudge::PLAN_NUDGE_KEY) {
        return None;
    }
    let in_plan = agent.plan_mode_pending.unwrap_or(agent.plan_mode_active);
    if in_plan {
        return None;
    }
    let in_yolo = agent.session.is_yolo();
    let in_auto = agent.session.is_auto();
    // Normal �?Plan is already a single ring step; only collapse Auto / yolo.
    if !in_yolo && !in_auto {
        return None;
    }
    let session_id = agent.session.session_id.clone()?;

    if in_yolo {
        set_yolo_mode_inner(app, false);
    }
    app.current_ui.permission_mode = Some("ask".into());
    sync_active_auto_flag(app);
    tracing::info!("Mode cycle: collapse to ask for plan nudge jump");
    Some(vec![Effect::PersistPermissionMode {
        canonical: "ask",
        session_id: Some(session_id),
        persist: crate::app::actions::PermissionModePersist::BestEffort,
    }])
}

/// The Shift+Tab cycle body shared by the agent view and the dashboard peek:
/// apply the mode, then keep the per-session `auto_mode` display flag in sync
/// with the freshly written canonical mode �?covering every arm (including the
/// pre-session and policy-pin early returns) without per-arm edits. Deliberately
/// telemetry-free: the dashboard peek reuses it so it can't attribute a
/// plan-nudge acceptance for an agent the user isn't viewing.
pub(super) fn dispatch_cycle_mode_and_sync(app: &mut AppView) -> Vec<Effect> {
    app.permission_mode_from_soft_default = false;
    let effects = dispatch_cycle_mode_inner(app);
    sync_active_auto_flag(app);
    effects
}

/// The active agent's `(plan nudge visible, optimistically in plan mode)`, or
/// `(false, false)` with no active agent. Lets [`dispatch_cycle_mode`] attribute
/// a shift+tab that turns plan mode on while the nudge shows as an acceptance.
pub(super) fn active_agent_plan_nudge_state(app: &AppView) -> (bool, bool) {
    let ActiveView::Agent(id) = app.active_view else {
        return (false, false);
    };
    match app.agents.get(&id) {
        Some(agent) => (
            agent.ephemeral_tip.current_key() == Some(crate::tips::plan_nudge::PLAN_NUDGE_KEY),
            agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
        ),
        None => (false, false),
    }
}

/// Current resident product posture for Shift+Tab (Auto / Goal).
fn resident_mode_of(
    agent: &crate::app::agent_view::AgentView,
) -> xai_grok_tools::types::SessionMode {
    use xai_grok_tools::types::SessionMode;
    // Plan mode is deleted �?never treat legacy plan flags as a cycle stop.
    let in_goal = agent.goal_mode_pending.unwrap_or(agent.goal_mode_active);
    if in_goal {
        return SessionMode::Goal;
    }
    SessionMode::Default
}

/// Apply a product posture to agent flags (goal pending + full permission).
fn apply_resident_mode_flags(
    agent: &mut crate::app::agent_view::AgentView,
    mode: xai_grok_tools::types::SessionMode,
    yolo_locked: bool,
) {
    use xai_grok_tools::types::SessionMode;
    // Clear any legacy plan flags permanently.
    agent.plan_mode_pending = Some(false);
    agent.plan_mode_active = false;
    match mode.as_product() {
        SessionMode::Goal => {
            agent.goal_mode_pending = Some(true);
            agent.session.yolo_mode = !yolo_locked;
            agent.deferred_session_mode = Some(SessionMode::Goal);
        }
        SessionMode::Default => {
            agent.goal_mode_pending = Some(false);
            agent.goal_mode_active = false;
            agent.session.yolo_mode = !yolo_locked;
            agent.deferred_session_mode = None;
        }
    }
}

/// Cycle session mode: **Auto �?Goal �?Auto**.
///
/// - Auto: ordinary chat; full tool permission by default
/// - Goal: full tool permission; acceptance-criteria-driven run-until-verified
///
/// Plan mode is deleted from the product surface.
fn dispatch_cycle_mode_inner(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let yolo_locked = app.yolo_policy_block.is_some();
    let yolo_warning = app.yolo_policy_block;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let current = resident_mode_of(agent);
    let next = current.product_cycle();
    let label = next.product_label();

    let Some(session_id) = agent.session.session_id.clone() else {
        apply_resident_mode_flags(agent, next, yolo_locked);
        app.default_yolo = !yolo_locked;
        app.current_ui.permission_mode = Some("auto".into());
        if let Some(warning) = yolo_warning.filter(|_| yolo_locked) {
            agent.show_toast(warning);
        }
        agent.show_mode_switch_banner(label);
        tracing::info!(
            from = current.product_label(),
            to = label,
            "Mode cycle (pre-session)"
        );
        refresh_open_settings_modals(app);
        let mut effects = vec![Effect::PersistPermissionMode {
            canonical: "auto",
            session_id: None,
            persist: crate::app::actions::PermissionModePersist::BestEffort,
        }];
        effects.extend(skip_picker_and_create_session(app, id));
        return effects;
    };

    let _ = agent;

    if let Some(agent) = app.agents.get_mut(&id) {
        apply_resident_mode_flags(agent, next, yolo_locked);
        if let Some(warning) = yolo_warning.filter(|_| yolo_locked) {
            agent.show_toast(warning);
        }
        agent.show_mode_switch_banner(label);
    }

    set_yolo_mode_inner(app, !yolo_locked);
    app.current_ui.permission_mode = Some("auto".into());
    refresh_open_settings_modals(app);
    tracing::info!(from = current.product_label(), to = label, "Mode cycle");

    let mode_id = next.as_product().as_id();

    vec![
        Effect::SetSessionMode {
            session_id: session_id.clone(),
            mode_id: acp::SessionModeId::new(mode_id),
        },
        Effect::PersistPermissionMode {
            canonical: "auto",
            session_id: Some(session_id),
            persist: crate::app::actions::PermissionModePersist::BestEffort,
        },
    ]
}
