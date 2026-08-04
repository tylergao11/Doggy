//! Tests for plan, yolo, auto, and permission mode transitions.

use super::*;

/// `ShowPlanNudge` is a no-op when its per-tip gate is off: no tip shown,
/// no count burned, even on a drawable agent.
#[test]
fn show_plan_nudge_no_op_when_flag_off() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
    app.contextual_hints.plan_mode = false;

    let effects = dispatch(Action::ShowPlanNudge, &mut app);
    assert!(effects.is_empty());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!app.agents[&id].ephemeral_tip.is_active());
}

/// `ShowPlanNudge` with the tip on and a drawable agent shows the tip and
/// increments the per-session seen count once (in memory, no effects).
#[test]
fn show_plan_nudge_shows_and_counts_when_flag_on() {
    use crate::tips::plan_nudge::PLAN_NUDGE_SEEN_KEY;
    let mut app = test_app_with_agent();
    app.contextual_hints.plan_mode = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);

    let effects = dispatch(Action::ShowPlanNudge, &mut app);
    assert!(app.agents[&id].ephemeral_tip.is_active());
    assert_eq!(app.tip_seen_counts.get(PLAN_NUDGE_SEEN_KEY), Some(&1));
    assert!(
        effects.is_empty(),
        "seen count is in-memory; nothing persisted"
    );
}

/// `ShowWordSelectTip` is a no-op when its per-tip gate is off.
#[test]
fn show_word_select_tip_no_op_when_flag_off() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
    app.contextual_hints.word_select = false;

    let effects = dispatch(Action::ShowWordSelectTip, &mut app);
    assert!(effects.is_empty());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!app.agents[&id].ephemeral_tip.is_active());
}

/// `ShowWordSelectTip` shows and counts when the gate is on and selection
/// is not already `word_select`.
#[test]
fn show_word_select_tip_shows_and_counts_when_flag_on() {
    use crate::appearance::TextSelection;
    use crate::tips::word_select::WORD_SELECT_TIP_SEEN_KEY;
    crate::appearance::cache::set_keep_text_selection(TextSelection::Flash);
    let mut app = test_app_with_agent();
    app.contextual_hints.word_select = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);

    let effects = dispatch(Action::ShowWordSelectTip, &mut app);
    assert!(app.agents[&id].ephemeral_tip.is_active());
    assert_eq!(app.tip_seen_counts.get(WORD_SELECT_TIP_SEEN_KEY), Some(&1));
    assert!(
        effects.is_empty(),
        "seen count is in-memory; nothing persisted"
    );
}

/// Already on `word_select` → tip is redundant, skip without burning count.
#[test]
fn show_word_select_tip_no_op_when_already_word_select() {
    use crate::appearance::TextSelection;
    crate::appearance::cache::set_keep_text_selection(TextSelection::WordSelect);
    let mut app = test_app_with_agent();
    app.contextual_hints.word_select = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);

    let effects = dispatch(Action::ShowWordSelectTip, &mut app);
    assert!(effects.is_empty());
    assert!(app.tip_seen_counts.is_empty());
    assert!(!app.agents[&id].ephemeral_tip.is_active());
    // Restore default so sibling tests don't inherit word_select.
    crate::appearance::cache::set_keep_text_selection(TextSelection::Flash);
}

/// Accepting the tip (its chord, with the tip on screen) flips the setting
/// to `word_select`, persists it, and retires the tip.
#[test]
fn accept_word_select_tip_flips_setting_and_retires_tip() {
    use crate::appearance::TextSelection;
    use crate::settings::SettingValue;
    crate::appearance::cache::set_keep_text_selection(TextSelection::Flash);
    let mut app = test_app_with_agent();
    app.contextual_hints.word_select = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
    let _ = dispatch(Action::ShowWordSelectTip, &mut app);
    assert!(app.agents[&id].ephemeral_tip.is_active());

    let effects = dispatch(Action::AcceptWordSelectTip, &mut app);
    assert!(
        crate::appearance::cache::load_keep_text_selection().selects_word(),
        "accept must flip the live setting to word_select"
    );
    assert!(
        !app.agents[&id].ephemeral_tip.is_active(),
        "accept must retire the tip"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistSetting {
                key: "keep_text_selection",
                value: SettingValue::Enum("word_select"),
                ..
            }
        )),
        "accept must persist the setting, got: {effects:?}"
    );
    // Restore default so sibling tests don't inherit word_select.
    crate::appearance::cache::set_keep_text_selection(TextSelection::Flash);
}

/// The accept action is tip-scoped: without the tip on screen it must not
/// touch the setting (Ctrl+Y outside the TTL keeps its normal meaning; a
/// stray action must not become a global toggle).
#[test]
fn accept_word_select_tip_no_op_when_tip_not_showing() {
    use crate::appearance::TextSelection;
    crate::appearance::cache::set_keep_text_selection(TextSelection::Flash);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
    assert!(!app.agents[&id].ephemeral_tip.is_active());

    let effects = dispatch(Action::AcceptWordSelectTip, &mut app);
    assert!(effects.is_empty());
    assert!(
        !crate::appearance::cache::load_keep_text_selection().selects_word(),
        "setting must be untouched without the tip"
    );
}

#[test]
fn permission_mode_slash_gate_offers_toggles_subject_to_auto_feature() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.auto_mode_gate = true;
    app.sync_permission_mode_slash_gate();

    let offered = |app: &AppView, name: &str| {
        app.agents[&id]
            .prompt
            .slash_controller
            .registry()
            .get(name)
            .is_some()
    };

    // Doggy: both slash aliases stay available (gate is hard-on).
    assert!(offered(&app, "always-approve"));
    assert!(offered(&app, "auto"));

    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(offered(&app, "always-approve"));
    assert!(offered(&app, "auto"));

    app.auto_mode_gate = false;
    app.sync_permission_mode_slash_gate();
    // Doggy hard-enables /auto in sync_permission_mode_slash_gate (gate unused).
    assert!(
        offered(&app, "auto"),
        "sync_permission_mode_slash_gate always offers /auto"
    );
    assert!(offered(&app, "always-approve"));
}

/// ON transition: persists `"auto"` with rollback to `"auto"` (Doggy's only
/// permission canonical) and toasts the Auto brand.
#[test]
fn set_yolo_mode_off_to_on_emits_persist_with_rollback() {
    let mut app = test_app_with_agent();
    assert!(!app.agents[&AgentId(0)].session.is_yolo());

    let effects = dispatch(Action::SetYoloMode(true), &mut app);

    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert!(app.default_yolo);
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::PersistPermissionMode {
            canonical,
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            session_id,
            ..
        } => {
            assert_eq!(*canonical, "auto");
            assert_eq!(*prev, "auto");
            assert!(session_id.is_some());
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str()));
}

/// Enabling Auto while legacy plan flags are set must use the under-plan toast
/// (plan edit gate still binds). Without plan flags, the standard Auto toast.
#[test]
fn set_yolo_mode_on_under_plan_uses_plan_aware_toast() {
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_active = true;

    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast(YOLO_ON_UNDER_PLAN_TOAST).as_str()));

    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_pending = Some(true);
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast(YOLO_ON_UNDER_PLAN_TOAST).as_str()));

    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert_eq!(
        agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str())
    );
}

/// Settings-modal Auto commit under legacy plan flags gets the same under-plan
/// toast as Ctrl+O.
#[test]
fn set_permission_mode_always_approve_under_plan_uses_plan_aware_toast() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_active = true;

    let _ = dispatch(Action::SetPermissionMode(PermissionModeKind::Auto), &mut app);
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast(YOLO_ON_UNDER_PLAN_TOAST).as_str()));
}

/// OFF transition still persists `"auto"` (Doggy has no Ask canonical) and
/// toasts the compact Auto brand.
#[test]
fn set_yolo_mode_on_to_off_emits_persist_with_rollback() {
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(app.agents[&AgentId(0)].session.is_yolo());

    let effects = dispatch(Action::SetYoloMode(false), &mut app);

    assert!(!app.agents[&AgentId(0)].session.is_yolo());
    assert!(!app.default_yolo);
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(effects.len(), 1);
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
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto").as_str()));
}

#[test]
fn yolo_on_drain_clears_double_click_tracker() {
    let mut app = test_app_with_agent();
    let _rx = enqueue_permission_with_enable_always_approve(&mut app);

    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .last_permission_click = Some((Instant::now(), 1));

    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    let agent = &app.agents[&AgentId(0)];
    assert!(agent.permission_queue.is_empty());
    assert!(
        agent.last_permission_click.is_none(),
        "YOLO-on drain must invalidate the armed click"
    );
}

#[test]
fn enable_always_approve_sends_response_and_flips_yolo_and_persists() {
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let mut response_rx = enqueue_permission_with_enable_always_approve(&mut app);
    assert!(!app.agents[&AgentId(0)].session.is_yolo());

    let effects = dispatch(
        Action::PermissionSelect(acp::PermissionOptionId::new(Arc::from(
            xai_grok_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID,
        ))),
        &mut app,
    );

    assert!(
        app.agents[&AgentId(0)].permission_queue.is_empty(),
        "enable-always-approve must drain the queue"
    );
    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert!(app.default_yolo);
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "auto",
                ..
            }
        )),
        "Doggy persists Auto, got {effects:?}"
    );
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
                option_id.0.as_ref(),
                xai_grok_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID
            );
        }
        other => panic!("expected Selected enable-always-approve, got {other:?}"),
    }
}

/// If the user picks "enable-always-approve" while YOLO is ALREADY
/// on, the dispatcher must NOT re-emit `PersistPermissionMode`
/// (which would queue a redundant disk write + ACP notification).
/// In practice YOLO-on suppresses the permission panel entirely
/// (`handle_permission_request` auto-approves), so this state is
/// only reachable in tests, but the idempotency guard matters for
/// future code paths that might pre-seed YOLO state.
#[test]
fn enable_always_approve_is_idempotent_when_yolo_already_on() {
    use std::sync::Arc;

    let mut app = test_app_with_agent();

    // Pre-flip YOLO on. We bypass the panel suppression by injecting
    // the permission AFTER the flip — exercises the dispatcher's
    // idempotency guard directly.
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(app.agents[&AgentId(0)].session.is_yolo());

    let mut response_rx = enqueue_permission_with_enable_always_approve(&mut app);

    let effects = dispatch(
        Action::PermissionSelect(acp::PermissionOptionId::new(Arc::from(
            xai_grok_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID,
        ))),
        &mut app,
    );

    // Response still flows (the current request is allowed once).
    match response_rx.try_recv() {
        Ok(Ok(acp::RequestPermissionResponse {
            outcome: acp::RequestPermissionOutcome::Selected(_),
            ..
        })) => {}
        other => panic!("expected Selected response, got {other:?}"),
    }

    // No redundant PersistPermissionMode. (The initial SetYoloMode
    // dispatch above already produced one for the YOLO-flip.)
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPermissionMode { .. })),
        "redundant PersistPermissionMode when YOLO already on — the dispatcher \
             must short-circuit to avoid double-writing config.toml and double-firing \
             x.ai/yolo_mode_changed",
    );
}

/// **Security-critical fallback:**
/// when a queued permission has NO `AllowOnce` option (only
/// `AllowAlways` / `RejectAlways`), the drain MUST send
/// `Cancelled` — NOT silently fall through to `AllowAlways`
/// which would whitelist the operation indefinitely.
///
/// This pins the safety contract: YOLO never auto-picks a
/// more-permissive option than `AllowOnce`. A regression that
/// added an `else if find(AllowAlways)` fallback would
/// dramatically widen the blast radius of a single YOLO toggle.
#[test]
fn set_yolo_mode_on_with_no_allow_once_option_sends_cancelled() {
    use crate::views::permission_view::{PermissionFocus, PermissionViewState};
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();

    // Inject a permission with only AllowAlways + RejectAlways
    // (NO AllowOnce). The drain must NOT pick AllowAlways even
    // though it's the only "Allow" option — that would breach
    // the safety contract.
    let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
    let request = acp::RequestPermissionRequest::new(
        acp::SessionId::new(Arc::from("test-sess")),
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-noallow-1")),
            acp::ToolCallUpdateFields::default(),
        ),
        vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("opt-allow-always")),
                "Allow always",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from("opt-reject-always")),
                "Reject always",
                acp::PermissionOptionKind::RejectAlways,
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
        title: "noallow-test".to_string(),
        description: vec![],
        args_expanded: false,
        desc_scroll: 0,
        subagent_label: None,
        options_area_height: 0,
        options_scroll_offset: 0,
    });

    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    // Queue drained.
    assert!(app.agents[&AgentId(0)].permission_queue.is_empty());
    // Cancelled (NOT Selected{AllowAlways}).
    match response_rx.try_recv() {
        Ok(Ok(acp::RequestPermissionResponse {
            outcome: acp::RequestPermissionOutcome::Cancelled,
            ..
        })) => {
            // Correct — preserved the safety contract.
        }
        Ok(Ok(acp::RequestPermissionResponse {
            outcome:
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome {
                    option_id,
                    ..
                }),
            ..
        })) => panic!(
            "drain picked `{option_id:?}` instead of Cancelled — SAFETY CONTRACT \
                 VIOLATION: YOLO must never pick a more-permissive option than AllowOnce. \
                 Either AllowAlways (whitelist forever) or RejectAlways (deny forever) \
                 would be wrong; the drain must Cancel and let the caller's higher level \
                 decide.",
        ),
        other => panic!("expected Cancelled response, got {other:?}"),
    }
}

/// **Security-critical multi-item drain:** the
/// drain loop must fully empty the queue, not stop at the first
/// item. A regression that swapped `drain(..)` for `pop_front()`
/// would silently leak queued permissions on YOLO toggle. With
/// 3 items in the queue, this catches an off-by-N drain bug.
#[test]
fn set_yolo_mode_on_drains_multi_item_queue() {
    use crate::views::permission_view::{PermissionFocus, PermissionViewState};
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();

    // Inject 3 permissions, each with AllowOnce.
    let mut response_rxs = Vec::new();
    for i in 0..3u32 {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        response_rxs.push(response_rx);
        let request = acp::RequestPermissionRequest::new(
            acp::SessionId::new(Arc::from("test-sess")),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from(format!("tc-multi-{i}"))),
                acp::ToolCallUpdateFields::default(),
            ),
            vec![acp::PermissionOption::new(
                acp::PermissionOptionId::new(Arc::from(format!("opt-allow-once-{i}"))),
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            )],
        );
        let options = request.options.clone();
        agent.permission_queue.push_back(PermissionViewState {
            request: xai_acp_lib::AcpArgs {
                request,
                response_tx,
            },
            id: i as usize + 1,
            focus: PermissionFocus::Options,
            options,
            active_idx: 0,
            bash_highlights: None,
            bash_selection_count: 0,
            bash_command_raw: None,
            mcp_scope: None,
            title: format!("multi-{i}"),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: None,
            options_area_height: 0,
            options_scroll_offset: 0,
        });
    }
    assert_eq!(agent.permission_queue.len(), 3);

    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    // Queue fully drained.
    assert!(
        app.agents[&AgentId(0)].permission_queue.is_empty(),
        "multi-item drain must fully empty the queue",
    );
    // All 3 channels received the AllowOnce response.
    for (i, mut rx) in response_rxs.into_iter().enumerate() {
        match rx.try_recv() {
            Ok(Ok(acp::RequestPermissionResponse {
                outcome: acp::RequestPermissionOutcome::Selected(_),
                ..
            })) => {} // OK
            other => panic!(
                "item {i} did not receive AllowOnce Selected response: {other:?} — \
                     drain skipped items beyond the first?",
            ),
        }
    }
}

/// **Security-critical:** re-dispatching
/// `SetYoloMode(true)` when already on MUST still drain any
/// permissions that arrived between the two dispatches. A future
/// "optimization" that skipped the drain on no-op redispatch
/// would lose security-critical state.
#[test]
fn set_yolo_mode_on_duplicate_dispatch_still_drains_queue() {
    use crate::views::permission_view::{PermissionFocus, PermissionViewState};
    use std::sync::Arc;

    let mut app = test_app_with_agent();
    // First dispatch: turn YOLO ON. Queue is empty so no drain.
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(app.agents[&AgentId(0)].session.is_yolo());

    // Now inject a permission AFTER the first dispatch.
    let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
    let request = acp::RequestPermissionRequest::new(
        acp::SessionId::new(Arc::from("test-sess")),
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-dup-1")),
            acp::ToolCallUpdateFields::default(),
        ),
        vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("opt-allow-once")),
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        )],
    );
    let options = request.options.clone();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .permission_queue
        .push_back(PermissionViewState {
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
            title: "dup-test".to_string(),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: None,
            options_area_height: 0,
            options_scroll_offset: 0,
        });

    // Second dispatch (same value): MUST still drain. A
    // "skip-drain-on-no-op" regression would leak this permission.
    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    assert!(
        app.agents[&AgentId(0)].permission_queue.is_empty(),
        "duplicate YOLO=true dispatch MUST drain any permission that arrived \
             between dispatches — Security Issue 27 regression",
    );
    match response_rx.try_recv() {
        Ok(Ok(acp::RequestPermissionResponse {
            outcome: acp::RequestPermissionOutcome::Selected(_),
            ..
        })) => {} // OK
        other => panic!(
            "duplicate dispatch must auto-approve the newly queued permission, got {other:?}",
        ),
    }
}

#[test]
fn set_yolo_mode_redispatch_same_value_still_emits_effect_and_toast() {
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    let effects = dispatch(Action::SetYoloMode(true), &mut app);
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::PersistPermissionMode {
            canonical,
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            ..
        } => {
            assert_eq!(
                *canonical, "auto",
                "Effect.canonical must be 'auto' on duplicate YOLO=true"
            );
            assert_eq!(*prev, "auto");
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str()));
}

#[test]
fn set_yolo_mode_toast_format() {
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str()));

    let _ = dispatch(Action::SetYoloMode(false), &mut app);
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto").as_str()));
}

#[test]
fn set_yolo_mode_on_blocked_by_policy_pin() {
    let mut app = test_app_with_agent();
    app.yolo_policy_block = Some(POLICY_WARNING);
    app.current_ui.permission_mode = Some("auto".into());

    let effects = dispatch(Action::SetYoloMode(true), &mut app);

    assert!(effects.is_empty(), "pin must refuse enable with no persist");
    assert!(!app.agents[&AgentId(0)].session.is_yolo());
    assert!(!app.default_yolo);
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(agent_toast(&app).as_deref(), Some(POLICY_WARNING));
}

#[test]
fn set_yolo_mode_off_allowed_under_policy_pin() {
    let mut app = test_app_with_agent();
    // Seed ON outside the pin, then pin, then OFF.
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    app.yolo_policy_block = Some(POLICY_WARNING);

    let effects = dispatch(Action::SetYoloMode(false), &mut app);

    assert!(!app.agents[&AgentId(0)].session.is_yolo());
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "auto",
                ..
            }
        )),
        "OFF under pin still persists Auto, got {effects:?}"
    );
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto").as_str()));
}

#[test]
fn set_yolo_mode_no_op_when_no_active_agent() {
    let mut app = test_app(); // no agent, active_view = Welcome
    let default_yolo_before = app.default_yolo;
    let perm_mode_before = app.current_ui.permission_mode.clone();

    let effects = dispatch(Action::SetYoloMode(true), &mut app);
    assert!(
        effects.is_empty(),
        "no active agent → no Effect, got {effects:?}",
    );
    // Defense-in-depth: SHARED state must NOT mutate.
    assert_eq!(app.default_yolo, default_yolo_before);
    assert_eq!(app.current_ui.permission_mode, perm_mode_before);
}

#[test]
fn set_yolo_mode_refreshes_open_modal_snapshots() {
    use crate::views::modal::ActiveModal;
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::OpenSettings, &mut app);

    let _ = dispatch(Action::SetYoloMode(true), &mut app);

    let agent = app.agents.get(&AgentId(0)).unwrap();
    let Some(ActiveModal::Settings { state }) = &agent.active_modal else {
        panic!("Settings modal must remain open");
    };
    assert!(
        state.pager_snapshot.yolo_mode,
        "pager_snapshot.yolo_mode must refresh"
    );
    assert_eq!(
        state.ui_snapshot.permission_mode.as_deref(),
        Some("auto"),
        "ui_snapshot.permission_mode must also refresh"
    );
    let cur = crate::settings::current_value_for(
        "permission_mode",
        &state.ui_snapshot,
        &state.pager_snapshot,
    )
    .expect("permission_mode must resolve");
    assert_eq!(cur, crate::settings::SettingValue::Enum("auto"));
}

/// Legacy `Default` kind normalizes to Auto (full tool permission).
#[test]
fn set_permission_mode_default_overrides_canonical_to_default() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();

    let effects = dispatch(
        Action::SetPermissionMode(PermissionModeKind::Default),
        &mut app,
    );

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
fn set_permission_mode_always_approve_from_default_captures_prev_canonical() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();
    app.current_ui.permission_mode = Some("auto".into());
    assert_eq!(
        app.current_ui.permission_mode.as_deref(),
        Some("auto"),
        "test setup: prior canonical must be 'auto'"
    );

    let effects = dispatch(
        Action::SetPermissionMode(PermissionModeKind::AlwaysApprove),
        &mut app,
    );

    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    match &effects[0] {
        Effect::PersistPermissionMode {
            canonical,
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            ..
        } => {
            assert_eq!(
                *canonical, "auto",
                "PersistPermissionMode canonical must be `auto`"
            );
            assert_eq!(*prev, "auto");
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
    assert_eq!(agent_toast(&app).as_deref(), Some(expected_toast("\u{2713} Auto: tools auto-run").as_str()));
}

/// Rollback canonical is always `"auto"` — LIVE/mirror precedence is gone.
#[test]
fn set_yolo_mode_with_live_yolo_and_default_ui_mirror_rolls_back_to_default() {
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SetYoloMode(true), &mut app);
    // Stale mirror label that used to mean "soft default" — ignored for rollback.
    app.current_ui.permission_mode = Some("default".into());

    let effects = dispatch(Action::SetYoloMode(true), &mut app);
    match &effects[0] {
        Effect::PersistPermissionMode {
            persist: crate::app::actions::PermissionModePersist::WithRollback(prev),
            ..
        } => {
            assert_eq!(
                *prev, "auto",
                "LIVE yolo=true must still roll back to Doggy's only canonical `auto`"
            );
        }
        other => panic!("expected PersistPermissionMode, got {other:?}"),
    }
}

#[test]
fn permission_mode_toast_returns_brand_consistent_strings() {
    use crate::app::actions::PermissionModeKind;
    let expected = "\u{2713} Auto: tools auto-run";
    for kind in [
        PermissionModeKind::Auto,
        PermissionModeKind::AlwaysApprove,
        PermissionModeKind::Ask,
        PermissionModeKind::Default,
    ] {
        assert_eq!(
            permission_mode_toast(kind),
            expected,
            "every kind collapses to the Auto toast"
        );
    }
}

#[test]
fn active_agent_plan_nudge_state_tracks_nudge_and_plan() {
    let mut app = test_app_with_agent();
    // No tip, not in plan.
    assert_eq!(active_agent_plan_nudge_state(&app), (false, false));
    // Plan nudge on the active agent, still not in plan.
    let _ = app.agents.get_mut(&AgentId(0)).unwrap().ephemeral_tip.show(
        crate::tips::plan_nudge::plan_nudge_tip(),
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(active_agent_plan_nudge_state(&app), (true, false));
    // Entering plan mode flips the second element (the accept condition:
    // nudge showing && !before && after).
    app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_pending = Some(true);
    assert_eq!(active_agent_plan_nudge_state(&app), (true, true));
}

/// `Action::SetTheme("auto")` enables `AUTO_MODE`, persists
/// `"auto"` (the canonical), and applies the resolved theme.
/// Specifically the "auto enablement" branch.
#[test]
fn set_theme_auto_enables_auto_mode_and_persists_auto() {
    use crate::settings::SettingValue;
    with_theme_test_env(|| {
        // Mock the system appearance so resolve_auto deterministically
        // picks a known concrete theme.
        crate::theme::system_appearance::set_mock(Some(
            crate::theme::system_appearance::SystemAppearance::Dark,
        ));

        let mut app = test_app_with_agent();
        assert!(!crate::theme::cache::is_auto_mode());
        let effects = dispatch(Action::SetTheme("auto".into()), &mut app);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::PersistSetting { key, value, .. } => {
                assert_eq!(*key, "theme");
                assert_eq!(
                    *value,
                    SettingValue::Enum("auto"),
                    "auto commit persists `auto` (NOT the resolved concrete theme)",
                );
            }
            other => panic!("expected PersistSetting, got {other:?}"),
        }
        assert_eq!(app.current_ui.theme.as_deref(), Some("auto"));
        assert!(
            crate::theme::cache::is_auto_mode(),
            "auto commit must enable AUTO_MODE",
        );
    });
}

