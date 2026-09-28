use std::collections::BTreeSet;

use herdr_resource_monitor::app::FocusTracker;
use herdr_resource_monitor::herdr::{PaneInfo, SessionSnapshot};

fn snapshot(panes: &[&str], focused: Option<&str>) -> SessionSnapshot {
    SessionSnapshot {
        focused_pane_id: focused.map(str::to_owned),
        panes: panes
            .iter()
            .map(|id| PaneInfo {
                pane_id: (*id).into(),
                terminal_id: format!("terminal-{id}"),
                workspace_id: "workspace".into(),
                tab_id: "tab".into(),
                label: Some("same label".into()),
                focused: Some(*id) == focused,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn scoped_snapshot(panes: &[(&str, &str, &str)], focused: Option<&str>) -> SessionSnapshot {
    let ids: Vec<_> = panes.iter().map(|(id, _, _)| *id).collect();
    let mut state = snapshot(&ids, focused);
    for (pane, (_, workspace_id, tab_id)) in state.panes.iter_mut().zip(panes) {
        pane.workspace_id = (*workspace_id).into();
        pane.tab_id = (*tab_id).into();
    }
    state
}

fn tracker() -> FocusTracker {
    FocusTracker::new(Some("monitor".into()))
}

#[test]
fn normal_focus_changes_target_but_monitor_focus_keeps_last_normal_pane() {
    let mut focus = tracker();
    let excluded = BTreeSet::new();
    assert!(focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("A")),
        &excluded,
        None
    ));
    assert_eq!(focus.target(), Some("A"));
    assert!(!focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("monitor")),
        &excluded,
        None
    ));
    assert_eq!(focus.target(), Some("A"));
    assert!(focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("B")),
        &excluded,
        None
    ));
    assert_eq!(focus.target(), Some("B"));
    assert!(!focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("B")),
        &excluded,
        None
    ));
}

#[test]
fn startup_on_monitor_can_use_a_verified_launch_target_without_agent_session_metadata() {
    let mut focus = tracker();
    let state = snapshot(&["shell", "monitor"], Some("monitor"));
    assert!(focus.reconcile(&state, &BTreeSet::new(), Some("shell")));
    assert_eq!(focus.target(), Some("shell"));
    assert!(!focus.reconcile(&state, &BTreeSet::new(), Some("monitor")));
    assert_eq!(focus.target(), Some("shell"));
}

#[test]
fn focus_event_preference_wins_over_an_older_snapshot_focus() {
    let mut focus = tracker();
    let stale = snapshot(&["A", "B", "monitor"], Some("A"));
    focus.reconcile(&stale, &BTreeSet::new(), None);
    assert!(focus.reconcile(&stale, &BTreeSet::new(), Some("B")));
    assert_eq!(focus.target(), Some("B"));
    assert!(!focus.reconcile(&stale, &BTreeSet::new(), Some("monitor")));
    assert_eq!(
        focus.target(),
        Some("B"),
        "monitor event cannot restore stale snapshot focus"
    );
    assert!(!focus.reconcile(&stale, &BTreeSet::new(), Some("missing")));
    assert_eq!(focus.target(), Some("B"));
    assert!(focus.reconcile(&stale, &BTreeSet::new(), None));
    assert_eq!(
        focus.target(),
        Some("A"),
        "reconnect snapshot is authoritative without an event target"
    );
}

#[test]
fn missing_or_excluded_event_target_cannot_override_valid_current_focus() {
    let mut focus = tracker();
    let state = snapshot(&["A", "other-monitor", "monitor"], Some("A"));
    let excluded = BTreeSet::from(["other-monitor".into()]);
    assert!(focus.reconcile(&state, &excluded, Some("missing")));
    assert_eq!(focus.target(), Some("A"));
    assert!(!focus.reconcile(&state, &excluded, Some("other-monitor")));
    assert_eq!(focus.target(), Some("A"));
}

#[test]
fn other_monitor_and_popup_like_exclusions_preserve_the_last_normal_target() {
    let mut focus = tracker();
    let excluded = BTreeSet::from(["other-monitor".into(), "plugin-popup".into()]);
    focus.reconcile(
        &snapshot(
            &["A", "other-monitor", "plugin-popup", "monitor"],
            Some("A"),
        ),
        &excluded,
        None,
    );
    for transient in ["other-monitor", "plugin-popup", "monitor"] {
        assert!(!focus.reconcile(
            &snapshot(
                &["A", "other-monitor", "plugin-popup", "monitor"],
                Some(transient)
            ),
            &excluded,
            None,
        ));
        assert_eq!(focus.target(), Some("A"));
    }
}

#[test]
fn closed_target_uses_current_normal_focus_or_clears_when_only_monitor_is_focused() {
    let mut focus = tracker();
    let excluded = BTreeSet::new();
    focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("A")),
        &excluded,
        None,
    );
    focus.invalidate("A");
    focus.reconcile(&snapshot(&["B", "monitor"], Some("B")), &excluded, None);
    assert_eq!(focus.target(), Some("B"));
    focus.invalidate("B");
    focus.reconcile(
        &snapshot(&["C", "monitor"], Some("monitor")),
        &excluded,
        None,
    );
    assert_eq!(
        focus.target(),
        None,
        "a random live pane is never a fallback"
    );
}

#[test]
fn invalidated_target_stays_ineligible_while_old_snapshot_still_lists_it() {
    let mut focus = tracker();
    let stale = snapshot(&["A", "B", "monitor"], Some("A"));
    focus.reconcile(&stale, &BTreeSet::new(), None);
    focus.invalidate("A");
    focus.reconcile(&stale, &BTreeSet::new(), Some("A"));
    assert_eq!(
        focus.target(),
        None,
        "late focus cannot resurrect a closed or exited target"
    );
    focus.reconcile(&stale, &BTreeSet::new(), None);
    assert_eq!(
        focus.target(),
        None,
        "stale snapshot focus cannot resurrect the target"
    );
    focus.reconcile(
        &snapshot(&["B", "monitor"], Some("B")),
        &BTreeSet::new(),
        None,
    );
    assert_eq!(focus.target(), Some("B"));
}

#[test]
fn reconnect_snapshot_replaces_a_stale_target_and_does_not_keep_disappeared_panes() {
    let mut focus = tracker();
    let excluded = BTreeSet::new();
    focus.reconcile(&snapshot(&["A", "monitor"], Some("A")), &excluded, None);
    assert!(focus.reconcile(&snapshot(&["B", "monitor"], Some("B")), &excluded, None));
    assert_eq!(focus.target(), Some("B"));
    assert!(focus.reconcile(&snapshot(&["monitor"], Some("monitor")), &excluded, None));
    assert_eq!(focus.target(), None);
}

#[test]
fn target_becoming_a_monitor_is_discarded_without_selecting_an_unfocused_pane() {
    let mut focus = tracker();
    focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("A")),
        &BTreeSet::new(),
        None,
    );
    let excluded = BTreeSet::from(["A".into()]);
    assert!(focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("monitor")),
        &excluded,
        None
    ));
    assert_eq!(focus.target(), None);
}

#[test]
fn no_focus_or_missing_preference_never_selects_an_arbitrary_pane() {
    let mut focus = tracker();
    assert!(!focus.reconcile(
        &snapshot(&["A", "B", "monitor"], None),
        &BTreeSet::new(),
        Some("missing")
    ));
    assert_eq!(focus.target(), None);
    focus.reconcile(
        &snapshot(&["A", "B", "monitor"], Some("A")),
        &BTreeSet::new(),
        None,
    );
    assert!(!focus.reconcile(
        &snapshot(&["A", "B", "monitor"], None),
        &BTreeSet::new(),
        None
    ));
    assert_eq!(focus.target(), Some("A"));
    focus.invalidate("B");
    assert_eq!(
        focus.target(),
        Some("A"),
        "unrelated closure leaves the target intact"
    );
}

#[test]
fn tracker_without_a_monitor_pane_can_keep_the_underlying_target_during_popup_focus() {
    let mut focus = FocusTracker::new(None);
    focus.reconcile(&snapshot(&["A"], Some("A")), &BTreeSet::new(), None);
    // Real Herdr popups have no pane ID; their launch leaves the normal pane in the snapshot.
    assert!(!focus.reconcile(&snapshot(&["A"], Some("A")), &BTreeSet::new(), None));
    assert_eq!(focus.target(), Some("A"));
}

#[test]
fn reconnect_clears_old_closed_ids_and_bootstraps_a_reused_pane_id() {
    let mut focus = tracker();
    let old = snapshot(&["A", "monitor"], Some("A"));
    focus.reconcile(&old, &BTreeSet::new(), None);
    focus.invalidate("A");
    let mut restarted = old.clone();
    restarted.panes[0].terminal_id = "replacement-terminal".into();
    focus.begin_connection(&restarted);
    focus.reconcile(&restarted, &BTreeSet::new(), None);
    assert_eq!(focus.target(), Some("A"));
}

#[test]
fn reconnect_retains_last_target_only_when_terminal_identity_matches() {
    let mut focus = tracker();
    let old = snapshot(&["A", "monitor"], Some("A"));
    focus.reconcile(&old, &BTreeSet::new(), None);
    let mut restarted = snapshot(&["A", "monitor"], Some("monitor"));
    focus.begin_connection(&restarted);
    focus.reconcile(&restarted, &BTreeSet::new(), None);
    assert_eq!(focus.target(), Some("A"));

    restarted.panes[0].terminal_id = "unrelated-terminal".into();
    focus.begin_connection(&restarted);
    focus.reconcile(&restarted, &BTreeSet::new(), None);
    assert_eq!(focus.target(), None);
}

#[test]
fn focus_from_other_tabs_or_workspaces_preserves_the_last_local_target() {
    let mut focus = tracker();
    let panes = [
        ("A", "workspace", "local"),
        ("B", "workspace", "local"),
        ("other-tab", "workspace", "foreign"),
        ("other-workspace", "foreign-workspace", "local"),
        ("monitor", "workspace", "local"),
    ];
    focus.reconcile(&scoped_snapshot(&panes, Some("A")), &BTreeSet::new(), None);
    for foreign in ["other-tab", "other-workspace"] {
        let state = scoped_snapshot(&panes, Some(foreign));
        assert!(!focus.reconcile(&state, &BTreeSet::new(), Some(foreign)));
        assert_eq!(focus.target(), Some("A"));
        assert!(!focus.reconcile(&state, &BTreeSet::new(), None));
        assert_eq!(focus.target(), Some("A"));
    }
    assert!(focus.reconcile(
        &scoped_snapshot(&panes, Some("B")),
        &BTreeSet::new(),
        Some("B")
    ));
    assert_eq!(focus.target(), Some("B"));
}

#[test]
fn bootstrap_with_foreign_global_focus_uses_the_verified_local_launch_target() {
    let mut focus = tracker();
    let state = scoped_snapshot(
        &[
            ("shell", "workspace", "local"),
            ("foreign", "workspace", "other-tab"),
            ("monitor", "workspace", "local"),
        ],
        Some("foreign"),
    );
    focus.begin_connection(&state);
    assert!(focus.reconcile(&state, &BTreeSet::new(), Some("shell")));
    assert_eq!(focus.target(), Some("shell"));
}

#[test]
fn a_foreign_launch_target_cannot_override_the_monitor_tab() {
    let mut focus = tracker();
    let state = scoped_snapshot(
        &[
            ("shell", "workspace", "local"),
            ("foreign", "workspace", "other-tab"),
            ("monitor", "workspace", "local"),
        ],
        Some("foreign"),
    );
    assert!(!focus.reconcile(&state, &BTreeSet::new(), Some("foreign")));
    assert_eq!(focus.target(), None);
}

#[test]
fn closing_the_local_target_does_not_fall_back_to_foreign_global_focus() {
    let mut focus = tracker();
    focus.reconcile(
        &scoped_snapshot(
            &[
                ("A", "workspace", "local"),
                ("foreign", "workspace", "other-tab"),
                ("monitor", "workspace", "local"),
            ],
            Some("A"),
        ),
        &BTreeSet::new(),
        None,
    );
    focus.invalidate("A");
    let state = scoped_snapshot(
        &[
            ("B", "workspace", "local"),
            ("foreign", "workspace", "other-tab"),
            ("monitor", "workspace", "local"),
        ],
        Some("foreign"),
    );
    focus.reconcile(&state, &BTreeSet::new(), None);
    assert_eq!(focus.target(), None);
}

#[test]
fn reconnect_never_adopts_a_target_from_another_tab() {
    let mut focus = tracker();
    let panes = [
        ("A", "workspace", "local"),
        ("foreign", "workspace", "other-tab"),
        ("monitor", "workspace", "local"),
    ];
    focus.reconcile(&scoped_snapshot(&panes, Some("A")), &BTreeSet::new(), None);
    let mut restarted = scoped_snapshot(&panes, Some("foreign"));
    focus.begin_connection(&restarted);
    focus.reconcile(&restarted, &BTreeSet::new(), None);
    assert_eq!(focus.target(), Some("A"));

    restarted.panes.retain(|pane| pane.pane_id != "A");
    focus.begin_connection(&restarted);
    focus.reconcile(&restarted, &BTreeSet::new(), None);
    assert_eq!(focus.target(), None);
}

#[test]
fn moving_the_monitor_to_a_new_tab_discards_the_previous_tab_target() {
    let mut focus = tracker();
    let panes = [
        ("A", "workspace", "old-tab"),
        ("B", "workspace", "new-tab"),
        ("monitor", "workspace", "old-tab"),
    ];
    focus.reconcile(&scoped_snapshot(&panes, Some("A")), &BTreeSet::new(), None);
    let mut moved = scoped_snapshot(&panes, Some("monitor"));
    moved
        .panes
        .iter_mut()
        .find(|pane| pane.pane_id == "monitor")
        .unwrap()
        .tab_id = "new-tab".into();
    assert!(focus.reconcile(&moved, &BTreeSet::new(), None));
    assert_eq!(focus.target(), None);
    assert!(focus.reconcile(&moved, &BTreeSet::new(), Some("B")));
    assert_eq!(focus.target(), Some("B"));
    assert!(!focus.reconcile(&moved, &BTreeSet::new(), Some("A")));
    assert_eq!(focus.target(), Some("B"));
}

#[test]
fn moving_the_monitor_with_a_changed_pane_id_follows_its_terminal_identity() {
    let mut focus = tracker();
    let panes = [
        ("A", "old-workspace", "old-tab"),
        ("B", "new-workspace", "new-tab"),
        ("monitor", "old-workspace", "old-tab"),
    ];
    focus.reconcile(&scoped_snapshot(&panes, Some("A")), &BTreeSet::new(), None);
    let mut moved = scoped_snapshot(&panes, Some("B"));
    let monitor = moved
        .panes
        .iter_mut()
        .find(|pane| pane.pane_id == "monitor")
        .unwrap();
    monitor.pane_id = "moved-monitor".into();
    monitor.workspace_id = "new-workspace".into();
    monitor.tab_id = "new-tab".into();
    assert!(focus.reconcile(&moved, &BTreeSet::new(), Some("B")));
    assert_eq!(focus.target(), Some("B"));
    assert!(!focus.reconcile(&moved, &BTreeSet::new(), Some("moved-monitor")));
    assert_eq!(focus.target(), Some("B"));
}

#[test]
fn a_missing_monitor_cannot_select_another_panes_tab() {
    let mut focus = tracker();
    focus.reconcile(
        &snapshot(&["A", "monitor"], Some("A")),
        &BTreeSet::new(),
        None,
    );
    assert!(focus.reconcile(
        &snapshot(&["A", "B"], Some("B")),
        &BTreeSet::new(),
        Some("B")
    ));
    assert_eq!(focus.target(), None);
}

#[test]
fn direct_mode_scopes_tracking_to_the_initial_selected_tab() {
    for preferred in [None, Some("A")] {
        let mut focus = FocusTracker::new(None);
        let panes = [
            ("A", "workspace", "local"),
            ("B", "workspace", "local"),
            ("foreign", "workspace", "other-tab"),
        ];
        let initial_focus = if preferred.is_some() { "foreign" } else { "A" };
        focus.reconcile(
            &scoped_snapshot(&panes, Some(initial_focus)),
            &BTreeSet::new(),
            preferred,
        );
        assert_eq!(focus.target(), Some("A"));
        let mut foreign = scoped_snapshot(&panes, Some("foreign"));
        assert!(!focus.reconcile(&foreign, &BTreeSet::new(), Some("foreign")));
        assert!(!focus.reconcile(&foreign, &BTreeSet::new(), None));
        assert_eq!(focus.target(), Some("A"));
        assert!(focus.reconcile(&foreign, &BTreeSet::new(), Some("B")));
        assert_eq!(focus.target(), Some("B"));

        foreign.panes.retain(|pane| pane.tab_id != "local");
        focus.begin_connection(&foreign);
        focus.reconcile(&foreign, &BTreeSet::new(), None);
        assert_eq!(focus.target(), None);
    }
}
