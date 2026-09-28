use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{HerdrError, Result};

/// Known fields follow the running Herdr schema; additional fields are retained.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: String,
    pub protocol: u32,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub panes: Vec<PaneInfo>,
    pub agents: Vec<AgentInfo>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl SessionSnapshot {
    pub fn target(&self, pane_id: &str, process: PaneProcessInfo) -> Result<PaneTarget> {
        let pane = self
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .ok_or_else(|| HerdrError::MissingPane(pane_id.to_owned()))?;
        let agent = self.agents.iter().find(|agent| agent.pane_id == pane_id);
        correlate(pane, agent, process)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    pub agent_status: String,
    pub revision: u64,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub state_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    pub agent_status: String,
    pub revision: u64,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub state_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    /// Herdr supplies `id` or `path`; this is metadata, never a process key.
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneProcessInfo {
    pub pane_id: String,
    pub shell_pid: Option<u32>,
    pub foreground_process_group_id: Option<u32>,
    pub tty: Option<String>,
    #[serde(default)]
    pub foreground_processes: Vec<ForegroundProcess>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl PaneProcessInfo {
    /// An empty foreground stays empty; neither a PGID nor shell PID is a fallback root.
    pub fn foreground_pids(&self) -> Vec<u32> {
        let mut pids: Vec<_> = self.foreground_processes.iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv: Option<Vec<String>>,
    pub argv0: Option<String>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentMetadata {
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    pub agent_status: String,
    pub agent_session: Option<AgentSessionInfo>,
    pub state_labels: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
    /// The original agent row remains available without discarding unknown metadata.
    pub info: Option<AgentInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneTarget {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    pub pane: PaneInfo,
    pub agent: Option<AgentMetadata>,
    pub process: PaneProcessInfo,
}

/// Attach metadata using pane_id, never a title, native session, binary name or PID.
pub fn correlate(
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    process: PaneProcessInfo,
) -> Result<PaneTarget> {
    if process.pane_id != pane.pane_id {
        return Err(HerdrError::PaneMismatch {
            expected: pane.pane_id.clone(),
            actual: process.pane_id,
        });
    }
    if let Some(agent) = agent {
        if agent.pane_id != pane.pane_id {
            return Err(HerdrError::PaneMismatch {
                expected: pane.pane_id.clone(),
                actual: agent.pane_id.clone(),
            });
        }
    }
    // Presentation and persisted session metadata can outlive the agent. Only
    // current agent recognition establishes this relationship; the original
    // pane still preserves historical fields for the technical view.
    let metadata =
        if agent.is_some() || pane.agent.as_deref().is_some_and(|agent| !agent.is_empty()) {
            let mut state_labels = pane.state_labels.clone();
            let mut tokens = pane.tokens.clone();
            if let Some(agent) = agent {
                state_labels.extend(agent.state_labels.clone());
                tokens.extend(agent.tokens.clone());
            }
            Some(AgentMetadata {
                agent: agent
                    .and_then(|a| a.agent.clone())
                    .or_else(|| pane.agent.clone()),
                display_agent: agent
                    .and_then(|a| a.display_agent.clone())
                    .or_else(|| pane.display_agent.clone()),
                name: agent.and_then(|a| a.name.clone()),
                agent_status: agent
                    .map_or_else(|| pane.agent_status.clone(), |a| a.agent_status.clone()),
                agent_session: agent
                    .and_then(|a| a.agent_session.clone())
                    .or_else(|| pane.agent_session.clone()),
                state_labels,
                tokens,
                info: agent.cloned(),
            })
        } else {
            None
        };
    Ok(PaneTarget {
        pane_id: pane.pane_id.clone(),
        workspace_id: pane.workspace_id.clone(),
        tab_id: pane.tab_id.clone(),
        focused: pane.focused,
        pane: pane.clone(),
        agent: metadata,
        process,
    })
}
