use std::collections::BTreeSet;

use crate::herdr::{PaneInfo, SessionSnapshot};

#[derive(Clone)]
struct TabScope {
    workspace_id: String,
    tab_id: String,
}

impl TabScope {
    fn from_pane(pane: &PaneInfo) -> Self {
        Self {
            workspace_id: pane.workspace_id.clone(),
            tab_id: pane.tab_id.clone(),
        }
    }

    fn contains(&self, pane: &PaneInfo) -> bool {
        pane.workspace_id == self.workspace_id && pane.tab_id == self.tab_id
    }
}

/// Selection stays in the monitor's tab. A monitor or another tab's focus never
/// erases the last normal target, and closed panes cannot be resurrected by an
/// older snapshot.
pub struct FocusTracker {
    monitor_pane_id: Option<String>,
    monitor_terminal_id: Option<String>,
    scope: Option<TabScope>,
    last_non_monitor_target: Option<String>,
    last_non_monitor_terminal_id: Option<String>,
    closed: BTreeSet<String>,
}

impl FocusTracker {
    pub fn new(monitor_pane_id: Option<String>) -> Self {
        Self {
            monitor_pane_id,
            monitor_terminal_id: None,
            scope: None,
            last_non_monitor_target: None,
            last_non_monitor_terminal_id: None,
            closed: BTreeSet::new(),
        }
    }

    pub fn target(&self) -> Option<&str> {
        self.last_non_monitor_target.as_deref()
    }

    pub fn invalidate(&mut self, pane_id: &str) {
        self.closed.insert(pane_id.into());
        if self.target() == Some(pane_id) {
            self.last_non_monitor_target = None;
            self.last_non_monitor_terminal_id = None;
        }
    }

    /// A new subscription starts from an authoritative snapshot. Closed IDs
    /// from the old connection must not exclude panes restored by a restart.
    /// Retain the last normal target only if it is still the same terminal.
    pub fn begin_connection(&mut self, snapshot: &SessionSnapshot) {
        self.closed.clear();
        let unchanged = snapshot.panes.iter().any(|pane| {
            Some(&pane.pane_id) == self.last_non_monitor_target.as_ref()
                && Some(&pane.terminal_id) == self.last_non_monitor_terminal_id.as_ref()
        });
        if !unchanged {
            self.last_non_monitor_target = None;
            self.last_non_monitor_terminal_id = None;
        }
    }

    pub fn reconcile(
        &mut self,
        snapshot: &SessionSnapshot,
        excluded: &BTreeSet<String>,
        preferred: Option<&str>,
    ) -> bool {
        self.update_scope(snapshot, excluded, preferred);
        self.closed
            .retain(|id| snapshot.panes.iter().any(|pane| &pane.pane_id == id));
        let eligible = |id: &str| {
            Some(id) != self.monitor_pane_id.as_deref()
                && !excluded.contains(id)
                && !self.closed.contains(id)
                && snapshot.panes.iter().any(|pane| {
                    pane.pane_id == id
                        && self
                            .scope
                            .as_ref()
                            .is_some_and(|scope| scope.contains(pane))
                })
        };
        let last = self.target().filter(|id| eligible(id));
        let focused = snapshot
            .focused_pane_id
            .as_deref()
            .filter(|id| eligible(id));
        let next = match preferred {
            Some(id) if eligible(id) => Some(id),
            // Monitor and other-tab focus are ignored, even when the snapshot
            // reflects a different attached client's view.
            Some(_) => last.or(focused),
            None => focused.or(last),
        }
        .map(str::to_owned);
        if next == self.last_non_monitor_target {
            return false;
        }
        self.last_non_monitor_target = next;
        self.last_non_monitor_terminal_id = snapshot
            .panes
            .iter()
            .find(|pane| Some(&pane.pane_id) == self.last_non_monitor_target.as_ref())
            .map(|pane| pane.terminal_id.clone());
        true
    }

    fn update_scope(
        &mut self,
        snapshot: &SessionSnapshot,
        excluded: &BTreeSet<String>,
        preferred: Option<&str>,
    ) {
        if let Some(monitor_id) = &self.monitor_pane_id {
            // Moving a pane can change its pane ID. Its terminal identity keeps
            // the scope attached to the monitor, including across workspaces.
            let monitor = snapshot.panes.iter().find(|pane| {
                self.monitor_terminal_id.as_ref().map_or_else(
                    || &pane.pane_id == monitor_id,
                    |terminal_id| &pane.terminal_id == terminal_id,
                )
            });
            self.scope = monitor.map(TabScope::from_pane);
            if let Some(monitor) = monitor {
                self.monitor_pane_id = Some(monitor.pane_id.clone());
                self.monitor_terminal_id = Some(monitor.terminal_id.clone());
            }
        } else if self.scope.is_none() {
            // A directly launched view has no plugin pane ID. Bind it to the
            // first valid launch/focus target's tab and retain that scope.
            self.scope = preferred
                .into_iter()
                .chain(snapshot.focused_pane_id.as_deref())
                .filter(|id| !excluded.contains(*id) && !self.closed.contains(*id))
                .find_map(|id| snapshot.panes.iter().find(|pane| pane.pane_id == id))
                .map(TabScope::from_pane);
        }
    }
}
