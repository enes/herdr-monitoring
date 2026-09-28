//! Plugin actions coordinate Herdr panes without starting a terminal UI.

mod state;

use std::path::{Path, PathBuf};

use crate::herdr::paths::{monitor_executable_path, process_executable};
use crate::herdr::{PaneController, PaneInfo, PaneProcessInfo, SocketPaneController};

use state::{SessionState, TabState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    OpenSummary,
    ToggleFocused,
}

#[derive(Debug)]
pub struct ActionContext {
    pub plugin_id: String,
    pub plugin_root: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub focused_pane_id: Option<String>,
}

impl ActionContext {
    pub fn from_env(action: Action) -> Result<Self, String> {
        Self::from_lookup(action, |key| std::env::var(key).ok())
    }

    fn from_lookup(
        action: Action,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let required = |key: &str| {
            lookup(key).filter(|value| !value.is_empty()).ok_or_else(|| {
                format!("{key} is missing. Run this command through `herdr plugin action invoke` or a Herdr plugin keybinding")
            })
        };
        let plugin_id = required("HERDR_PLUGIN_ID")?;
        let plugin_root = PathBuf::from(required("HERDR_PLUGIN_ROOT")?);
        let state_dir = match action {
            Action::ToggleFocused => Some(PathBuf::from(required("HERDR_PLUGIN_STATE_DIR")?)),
            Action::OpenSummary => None,
        };
        #[derive(serde::Deserialize)]
        struct Context {
            focused_pane_id: Option<String>,
        }
        let focused_pane_id = lookup("HERDR_PLUGIN_CONTEXT_JSON")
            .filter(|value| !value.is_empty())
            .map(|json| {
                serde_json::from_str::<Context>(&json)
                    .map(|context| context.focused_pane_id)
                    .map_err(|error| format!("Invalid HERDR_PLUGIN_CONTEXT_JSON: {error}"))
            })
            .transpose()?
            .flatten()
            .filter(|value| !value.is_empty())
            .or_else(|| lookup("HERDR_PANE_ID").filter(|value| !value.is_empty()));
        Ok(Self {
            plugin_id,
            plugin_root,
            state_dir,
            focused_pane_id,
        })
    }
}

pub fn run(action: Action) -> Result<String, String> {
    let context = ActionContext::from_env(action)?;
    let mut controller = SocketPaneController::from_env().map_err(|error| error.to_string())?;
    let socket_path = controller.socket_path().to_path_buf();
    execute(action, &mut controller, &context, &socket_path)
}

pub fn execute<C: PaneController>(
    action: Action,
    client: &mut C,
    context: &ActionContext,
    socket_path: &Path,
) -> Result<String, String> {
    if action == Action::OpenSummary {
        client
            .open_summary(&context.plugin_id)
            .map_err(|error| error.to_string())?;
        return Ok("Resource summary opened".into());
    }

    let state_dir = context.state_dir.as_deref().ok_or_else(|| {
        "HERDR_PLUGIN_STATE_DIR is missing; invoke toggle-focused as a Herdr plugin action"
            .to_string()
    })?;
    let Some(session) = SessionState::acquire(state_dir, socket_path)? else {
        return Ok("A monitor toggle is already running; this invocation was skipped".into());
    };
    let snapshot = client.snapshot().map_err(|error| error.to_string())?;
    let target_id = context
        .focused_pane_id
        .as_deref()
        .or(snapshot.focused_pane_id.as_deref());
    let Some(target) = target_id
        .and_then(|target_id| snapshot.panes.iter().find(|pane| pane.pane_id == target_id))
    else {
        return Ok("No current normal pane is available; focus a terminal and try again".into());
    };
    let state = session.for_tab(&target.workspace_id, &target.tab_id)?;
    let in_tab =
        |pane: &PaneInfo| pane.workspace_id == target.workspace_id && pane.tab_id == target.tab_id;
    let executable = monitor_executable_path(&context.plugin_root)
        .canonicalize()
        .map_err(|error| format!("Cannot locate this plugin's monitor executable: {error}"))?;
    let record = state.load()?;
    let mut processes = std::collections::BTreeMap::new();

    if let Some(record) = record {
        if let Some(pane) = snapshot.panes.iter().find(|pane| {
            in_tab(pane)
                && pane.pane_id == record.focused_monitor_pane_id
                && pane.terminal_id == record.terminal_id
        }) {
            let process = read_process(client, pane)?;
            match monitor_identity(&process, &executable) {
                Identity::Focused => return close_monitor(client, &state, pane),
                Identity::Unknown => {
                    return Err("The existing monitor's process details are not available yet; retry toggle-focused".into());
                }
                Identity::OtherMonitor | Identity::Normal => {
                    processes.insert(pane.pane_id.clone(), process);
                }
            }
        }
        state.clear()?;
    }

    // Recover monitors only in the invoking pane's tab, including manually
    // opened panes and legacy session-wide records. Never infer ownership from
    // a user-editable terminal name, or inspect monitors in another tab.
    for pane in snapshot.panes.iter().filter(|pane| in_tab(pane)) {
        let process = match processes.get(&pane.pane_id) {
            Some(process) => process.clone(),
            None => read_process(client, pane)?,
        };
        if monitor_identity(&process, &executable) == Identity::Focused {
            return close_monitor(client, &state, pane);
        }
        if contains_executable(&process, &executable)
            && monitor_identity(&process, &executable) == Identity::Unknown
        {
            return Err("A monitor is starting; retry toggle-focused when its process details are available".into());
        }
        processes.insert(pane.pane_id.clone(), process);
    }

    let process = &processes[&target.pane_id];
    if contains_executable(process, &executable) {
        return Ok("Focus a normal terminal before opening the resource monitor".into());
    }
    let opened = client
        .open_focused(&context.plugin_id, &target.pane_id)
        .map_err(|error| error.to_string())?;
    if let Err(error) = state.save(&opened.pane_id, &opened.terminal_id) {
        return match client.close_plugin_pane(&opened.pane_id) {
            Ok(()) => Err(format!("{error}; the newly opened monitor was closed")),
            Err(rollback) => Err(format!(
                "{error}; could not close the newly opened monitor {}: {rollback}",
                opened.pane_id
            )),
        };
    }
    Ok(format!("Resource monitor opened in {}", opened.pane_id))
}

fn read_process<C: PaneController>(
    client: &mut C,
    pane: &PaneInfo,
) -> Result<PaneProcessInfo, String> {
    let process = client
        .process_info(&pane.pane_id)
        .map_err(|error| format!("Cannot verify pane {}: {error}", pane.pane_id))?;
    if process.pane_id != pane.pane_id {
        return Err(format!(
            "Herdr returned process details for a different pane than {}",
            pane.pane_id
        ));
    }
    Ok(process)
}

fn close_monitor<C: PaneController>(
    client: &mut C,
    state: &TabState,
    pane: &PaneInfo,
) -> Result<String, String> {
    client
        .close_plugin_pane(&pane.pane_id)
        .map_err(|error| error.to_string())?;
    state.clear()?;
    Ok(format!("Resource monitor closed: {}", pane.pane_id))
}

#[derive(Debug, PartialEq, Eq)]
enum Identity {
    Focused,
    OtherMonitor,
    Normal,
    Unknown,
}

fn contains_executable(process: &PaneProcessInfo, executable: &Path) -> bool {
    process
        .foreground_processes
        .iter()
        .any(|process| process_executable(process).as_deref() == Some(executable))
}

fn monitor_identity(process: &PaneProcessInfo, executable: &Path) -> Identity {
    if process.foreground_processes.is_empty() {
        return Identity::Unknown;
    }
    let mut unknown = false;
    let mut other_monitor = false;
    for foreground in &process.foreground_processes {
        let resolved = process_executable(foreground);
        if resolved.is_none() {
            unknown = true;
        }
        if resolved.as_deref() == Some(executable) {
            match foreground
                .argv
                .as_ref()
                .and_then(|argv| argv.get(1))
                .map(String::as_str)
            {
                Some("focused") => return Identity::Focused,
                Some("summary") => other_monitor = true,
                _ => unknown = true,
            }
        }
    }
    if other_monitor {
        Identity::OtherMonitor
    } else if unknown {
        Identity::Unknown
    } else {
        Identity::Normal
    }
}

#[cfg(test)]
mod tests;
