//! Tests for permission request selection, follow-ups, and queue draining.

use super::*;

/// Doggy `current_value_for("permission_mode")` is always `"auto"`, so Reset
/// is the already-at-default path (toast, no persist) — there is no off-default
/// product state to reset from.
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_set_permission_mode_for_permission_mode() {
    use crate::views::modal::{ActiveModal, ResetSettingsResult};
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(app.agents[&AgentId(0)].session.is_yolo());

    setup_reset_confirm_open(&mut app, "permission_mode");

    let effects = dispatch(
        Action::ConfirmResetSetting {
            choice: ResetSettingsResult::Reset,
        },
        &mut app,
    );

    assert!(
        effects.is_empty(),
        "already-at-default Reset must not persist, got {effects:?}"
    );
    assert!(
        app.agents[&AgentId(0)].session.is_yolo(),
        "idempotent reset must leave Auto/yolo armed"
    );
    let toast = agent_toast(&app).expect("already-at-default toast");
    assert!(
        toast.contains("already at default"),
        "expected already-at-default toast, got {toast}"
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(
        matches!(agent.active_modal, Some(ActiveModal::Settings { .. })),
        "confirm must restore the Settings modal"
    );
}

/// **Security-critical:** YOLO ON must drain the per-agent
/// `permission_queue` with `AllowOnce` responses. If this drain
/// path regresses (e.g., the setter falls back to `Cancelled`
/// without an `AllowOnce` lookup), the user enables YOLO and
/// their queued permissions silently get rejected.
#[test]
fn set_yolo_mode_on_drains_permission_queue_with_allow_once() {
    use crate::views::permission_view::{PermissionFocus, PermissionViewState};
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();

    // Inject a fake queued permission. The drain semantics use
    // `find(|o| o.kind == AllowOnce)` so we need ≥1 AllowOnce
    // option for the test to exercise the happy path.
    let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
    let request = acp::RequestPermissionRequest::new(
        acp::SessionId::new(Arc::from("test-sess")),
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-1")),
            acp::ToolCallUpdateFields::default(),
        ),
        vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("opt-allow-once")),
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("opt-reject")),
                "Reject",
                acp::PermissionOptionKind::RejectOnce,
            ),
        ],
    );
    let options = request.options.clone();
    agent.permission_queue.push_back(PermissionViewState {
        request: xai_acp_lib::AcpArgs {
            request,
            response_tx,
        },
        id: 1,
        focus: PermissionFocus::Options,
        options,
        active_idx: 0,
        bash_highlights: None,
        bash_selection_count: 0,
        bash_command_raw: None,
        mcp_scope: None,
        title: "test".to_string(),
        description: vec![],
        args_expanded: false,
        desc_scroll: 0,
        subagent_label: None,
        options_area_height: 0,
        options_scroll_offset: 0,
    });
    assert_eq!(agent.permission_queue.len(), 1);

    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    // Queue is drained.
    assert!(
        app.agents[&AgentId(0)].permission_queue.is_empty(),
        "YOLO ON must drain the permission_queue",
    );
    // Verify the `AllowOnce` response was actually sent (NOT
    // `Cancelled`). The drain semantics use `find(|o| o.kind ==
    // AllowOnce)` — a regression to `Cancelled` here would
    // silently reject every queued permission when the user
    // enables YOLO, which is the exact security failure mode
    // this test prevents.
    match response_rx.try_recv() {
        Ok(Ok(acp::RequestPermissionResponse {
            outcome:
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome {
                    option_id,
                    ..
                }),
            ..
        })) => {
            assert_eq!(
                option_id,
                acp::PermissionOptionId::new(Arc::from("opt-allow-once")),
                "the drain must select the AllowOnce option (NOT Cancelled / RejectOnce)",
            );
        }
        other => panic!(
            "queue drain must send an `AllowOnce` Selected response, got {other:?} — \
                 security regression: queued permissions are NOT being auto-approved on YOLO ON",
        ),
    }
}

#[test]
fn permission_select_clears_double_click_tracker_for_next_prompt() {
    use crate::views::permission_view::PermissionFocus;
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let _rx_front = enqueue_permission_with_enable_always_approve(&mut app);
    let _rx_next = enqueue_permission_with_enable_always_approve(&mut app);

    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.permission_queue.get_mut(1).unwrap().focus = PermissionFocus::FollowupInput;
    agent.last_permission_click = Some((Instant::now(), 1));

    let _ = dispatch(
        Action::PermissionSelect(acp::PermissionOptionId::new(Arc::from("opt-allow-once"))),
        &mut app,
    );

    let agent = &app.agents[&AgentId(0)];
    assert_eq!(agent.permission_queue.len(), 1);
    assert!(
        agent.last_permission_click.is_none(),
        "armed click on the resolved prompt must not pair with a click on the next prompt"
    );
    assert_eq!(
        agent.permission_queue.front().unwrap().focus,
        PermissionFocus::Options,
        "next front must be reset to Options"
    );
}

#[test]
fn drain_permission_queue_clears_double_click_tracker() {
    let mut app = test_app_with_agent();
    let _rx = enqueue_permission_with_enable_always_approve(&mut app);

    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.last_permission_click = Some((Instant::now(), 1));

    drain_permission_queue(agent);

    assert!(agent.permission_queue.is_empty());
    assert!(
        agent.last_permission_click.is_none(),
        "turn-end/turn-cancel drain must invalidate the armed click"
    );
}

#[test]
fn set_permission_mode_always_approve_blocked_by_policy_pin() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();
    app.yolo_policy_block = Some(POLICY_WARNING);
    app.current_ui.permission_mode = Some("auto".into());

    let effects = dispatch(
        Action::SetPermissionMode(PermissionModeKind::AlwaysApprove),
        &mut app,
    );

    assert!(effects.is_empty());
    assert!(!app.agents[&AgentId(0)].session.is_yolo());
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(agent_toast(&app).as_deref(), Some(POLICY_WARNING));
}

/// Doggy Auto IS full tool auto-run (yolo). Persists `"auto"` with yolo on.
#[test]
fn set_permission_mode_auto_persists_without_yolo() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();

    let effects = dispatch(Action::SetPermissionMode(PermissionModeKind::Auto), &mut app);

    assert!(
        app.agents[&AgentId(0)].session.is_yolo(),
        "Doggy Auto enables full tool auto-run"
    );
    assert!(!app.agents[&AgentId(0)].session.is_auto(), "classifier auto stays off");
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "auto",
                ..
            }
        )),
        "expected PersistPermissionMode(auto), got {effects:?}"
    );
}

/// Unknown rollback canonical falls through to Ask kind, which Doggy maps
/// to Auto (yolo on + `"auto"` display).
#[test]
fn rollback_permission_mode_unknown_canonical_defaults_to_ask() {
    use crate::settings::SettingValue;
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = false;
    app.default_yolo = false;
    app.current_ui.permission_mode = Some("auto".into());

    let effects = apply_setting_rollback(
        &mut app,
        "permission_mode",
        &SettingValue::Enum("bogus-canonical"),
    );
    assert!(effects.is_empty());
    assert!(
        app.agents[&AgentId(0)].session.is_yolo(),
        "Ask kind normalizes to Auto/yolo"
    );
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
}

#[test]
fn rollback_permission_mode_refreshes_open_modal_snapshots() {
    // Rollback uses `set_yolo_mode_inner` (no modal refresh). Pin live mirrors;
    // modal refresh is covered by `set_yolo_mode_refreshes_open_modal_snapshots`.
    use crate::settings::SettingValue;
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = false;
    app.default_yolo = false;

    let effects = apply_setting_rollback(
        &mut app,
        "permission_mode",
        &SettingValue::Enum("ask"),
    );
    assert!(effects.is_empty());
    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert!(app.default_yolo);
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
}


/// Legacy Ask kind normalizes to Auto toast + `"auto"` persist.
#[test]
fn set_permission_mode_ask_emits_brand_consistent_toast() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();

    let effects = dispatch(Action::SetPermissionMode(PermissionModeKind::Ask), &mut app);

    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    match &effects[0] {
        Effect::PersistPermissionMode {
            canonical,
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            ..
        } => {
            assert_eq!(*canonical, "auto");
            assert_eq!(*prev, "auto");
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str()));
}

#[test]
fn set_permission_mode_with_live_yolo_and_no_ui_mirror_rolls_back_to_always_approve() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    app.current_ui.permission_mode = None;

    let effects = dispatch(
        Action::SetPermissionMode(PermissionModeKind::AlwaysApprove),
        &mut app,
    );
    match &effects[0] {
        Effect::PersistPermissionMode {
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            ..
        } => {
            assert_eq!(*prev, "auto", "rollback canonical is always auto");
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
}

/// Non-empty permission_queue → NeedsInput.
#[test]
fn classify_top_level_permission_queue_non_empty_is_needs_input() {
    use crate::views::dashboard::{RowState, classify_top_level};
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    let _rx = push_synthetic_permission(agent, 1, vec![("allow", "Allow")]);
    assert_eq!(classify_top_level(agent), RowState::NeedsInput);
}

#[test]
fn permission_select_reject_does_not_steer_sticky_cursor() {
    use crate::appearance::permission_cursor::{
        DefaultSelectedPermission, last_used_permission, set_last_used_permission,
    };
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let _rx_allow = enqueue_permission_with_enable_always_approve(&mut app);
    let _rx_reject = enqueue_permission_with_enable_always_approve(&mut app);

    set_last_used_permission(DefaultSelectedPermission::AlwaysAllowAllSessions);
    let _ = dispatch(
        Action::PermissionSelect(acp::PermissionOptionId::new(Arc::from("opt-allow-once"))),
        &mut app,
    );
    assert_eq!(
        last_used_permission(),
        DefaultSelectedPermission::AllowOnce,
        "allow selection records the sticky cursor target"
    );

    let _ = dispatch(
        Action::PermissionSelect(acp::PermissionOptionId::new(Arc::from("opt-reject-once"))),
        &mut app,
    );
    assert_eq!(
        last_used_permission(),
        DefaultSelectedPermission::AllowOnce,
        "reject selection must not steer the sticky cursor"
    );
}
