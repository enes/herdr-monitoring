//! Application orchestration. Terminal rendering never runs Herdr commands.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod focus;
mod runtime;
mod shared;
pub use focus::FocusTracker;
pub use runtime::{MonitorUpdate, MonitorWorker};
pub use shared::{SharedServerFiles, SharedServerKind};

use crate::herdr::paths::process_executable;
use crate::herdr::{HerdrClient, PaneProcessInfo, PaneTarget};
use crate::metrics::{
    MetricTotals, MetricsSnapshot, ProcessMetricsProvider, RootMetrics, SystemCapacity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Summary,
    Focused,
}

#[derive(Clone, Debug, Default)]
pub struct LaunchContext {
    pub monitor_pane_id: Option<String>,
    pub target_pane_id: Option<String>,
    pub monitor_pid: u32,
}

impl LaunchContext {
    pub fn from_env() -> Result<Self, String> {
        let target_pane_id = match std::env::var("HERDR_PLUGIN_CONTEXT_JSON") {
            Ok(json) if !json.is_empty() => {
                #[derive(serde::Deserialize)]
                struct Context {
                    focused_pane_id: Option<String>,
                }
                serde_json::from_str::<Context>(&json)
                    .map_err(|error| format!("invalid HERDR_PLUGIN_CONTEXT_JSON: {error}"))?
                    .focused_pane_id
            }
            _ => None,
        };
        Ok(Self {
            monitor_pane_id: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|id| !id.is_empty()),
            target_pane_id,
            monitor_pid: std::process::id(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSource {
    Foreground,
    ShellFallback,
    NoProcess,
}

pub fn metric_roots(process: &PaneProcessInfo) -> (Vec<u32>, RootSource) {
    let roots: BTreeSet<_> = process.foreground_processes.iter().map(|p| p.pid).collect();
    if !roots.is_empty() {
        (roots.into_iter().collect(), RootSource::Foreground)
    } else if let Some(pid) = process.shell_pid {
        (vec![pid], RootSource::ShellFallback)
    } else {
        (Vec::new(), RootSource::NoProcess)
    }
}

#[derive(Debug)]
pub struct PaneSample {
    pub target: PaneTarget,
    pub roots: Vec<u32>,
    pub root_source: RootSource,
    pub stats: RootMetrics,
}

/// A server shared by several sessions, sampled apart from every pane.
#[derive(Debug)]
pub struct SharedServerSample {
    pub kind: SharedServerKind,
    /// Outermost verified PIDs; empty when the server's state is unusable.
    pub roots: Vec<u32>,
    pub stats: RootMetrics,
    /// Why the server's state record could not be used.
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct MonitorSample {
    pub capacity: SystemCapacity,
    pub panes: Vec<PaneSample>,
    pub servers: Vec<SharedServerSample>,
    pub totals: MetricTotals,
    pub agent_count: usize,
    pub errors: Vec<String>,
    pub metrics: MetricsSnapshot,
}

pub struct Monitor<C, P> {
    client: C,
    provider: P,
    mode: Mode,
    context: LaunchContext,
    focused_target: Option<String>,
    shared_servers: SharedServerFiles,
    stop: Arc<AtomicBool>,
}

impl<C: HerdrClient, P: ProcessMetricsProvider> Monitor<C, P> {
    pub fn new(client: C, provider: P, mode: Mode, context: LaunchContext) -> Self {
        Self {
            client,
            provider,
            mode,
            context,
            focused_target: None,
            shared_servers: SharedServerFiles::default(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Summary also lists the shared servers recorded in these state files.
    pub fn with_shared_servers(mut self, files: SharedServerFiles) -> Self {
        self.shared_servers = files;
        self
    }

    pub fn set_target(&mut self, pane_id: Option<String>) {
        self.focused_target =
            pane_id.filter(|id| Some(id) != self.context.monitor_pane_id.as_ref());
    }

    pub fn sample(&mut self) -> Result<MonitorSample, String> {
        if self.mode == Mode::Focused && self.focused_target.is_none() {
            return Ok(MonitorSample {
                capacity: self.provider.capacity(),
                panes: Vec::new(),
                servers: Vec::new(),
                totals: MetricTotals::default(),
                agent_count: 0,
                errors: Vec::new(),
                metrics: self.provider.refresh(&[], &[self.context.monitor_pid]),
            });
        }
        let snapshot = self.client.snapshot().map_err(|error| error.to_string())?;
        let mut targets = Vec::new();
        let mut roots = BTreeSet::new();
        let mut excluded = vec![self.context.monitor_pid];
        let mut errors = Vec::new();

        for pane in &snapshot.panes {
            if self.stop.load(Ordering::Relaxed) {
                return Err("monitor stopped".into());
            }
            if Some(&pane.pane_id) == self.context.monitor_pane_id.as_ref() {
                continue;
            }
            if self.mode == Mode::Focused && self.focused_target.as_ref() != Some(&pane.pane_id) {
                continue;
            }
            if self.mode == Mode::Summary
                && pane.agent.as_deref().is_none_or(|agent| agent.is_empty())
                && !snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane.pane_id)
            {
                continue;
            }
            let process = match self.client.process_info(&pane.pane_id) {
                Ok(process) => process,
                Err(error) => {
                    errors.push(format!("{}: {error}", pane.pane_id));
                    PaneProcessInfo {
                        pane_id: pane.pane_id.clone(),
                        ..Default::default()
                    }
                }
            };
            // Other instances have no plugin ownership field in PaneInfo. Use their
            // verified foreground executable identity, never a user-editable title.
            if is_monitor_process(&process) {
                excluded.extend(process.foreground_processes.iter().map(|p| p.pid));
                continue;
            }
            match snapshot.target(&pane.pane_id, process) {
                Ok(target) => {
                    let (pane_roots, source) = metric_roots(&target.process);
                    if source == RootSource::NoProcess {
                        errors.push(format!("{}: no process available", pane.pane_id));
                    }
                    roots.extend(&pane_roots);
                    targets.push((target, pane_roots, source));
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        if self.mode == Mode::Focused {
            if let Some(id) = &self.focused_target {
                if !snapshot.panes.iter().any(|pane| &pane.pane_id == id) {
                    errors.push(format!("Target pane {id} no longer exists"));
                }
            }
        }
        // A shared server works for several sessions, so it is sampled as its
        // own root instead of belonging to whichever pane happened to start it.
        // Focused mode shows only the target agent's server, as context.
        let records = match self.mode {
            Mode::Summary => self.shared_servers.read(),
            Mode::Focused => targets
                .first()
                .and_then(|(target, _, _)| SharedServerKind::for_target(target))
                .and_then(|kind| self.shared_servers.read_one(kind))
                .into_iter()
                .collect(),
        };
        let mut requested = roots.clone();
        for record in &records {
            match &record.pids {
                Ok(pids) => requested.extend(pids),
                // Focused context cannot make the pane's own totals unknown.
                Err(error) if self.mode == Mode::Summary => {
                    errors.push(format!("{}: {error}", record.kind.label()))
                }
                Err(_) => {}
            }
        }
        let requested: Vec<_> = requested.into_iter().collect();
        let mut metrics = self.provider.refresh(&requested, &excluded);
        // A stale record or reused PID does not identify a running server.
        let servers: Vec<_> = records
            .into_iter()
            .filter_map(|record| match record.pids {
                Ok(pids) => {
                    let live: Vec<_> = pids
                        .into_iter()
                        .filter(|pid| {
                            metrics
                                .processes
                                .get(pid)
                                .is_some_and(|process| record.kind.matches(process))
                        })
                        .collect();
                    (!live.is_empty()).then_some((record.kind, live, None))
                }
                Err(error) => Some((record.kind, Vec::new(), Some(error))),
            })
            .collect();
        let detached: Vec<_> = servers
            .iter()
            .flat_map(|(_, pids, _)| metrics.outer_roots(pids))
            .collect();
        metrics.detach(&detached);
        // Summary totals cover the servers; focused totals stay the pane's own.
        if self.mode == Mode::Summary {
            roots.extend(servers.iter().flat_map(|(_, pids, _)| pids.iter().copied()));
        }
        let roots: Vec<_> = roots.into_iter().collect();
        let mut totals = metrics.totals(&roots).tree;
        if !errors.is_empty() {
            totals.cpu_percent = None;
            totals.rss_bytes = None;
        }
        let mut panes: Vec<_> = targets
            .into_iter()
            .map(|(target, roots, root_source)| {
                let (roots, stats) = root_stats(&metrics, &roots);
                PaneSample {
                    target,
                    roots,
                    root_source,
                    stats,
                }
            })
            .collect();
        let servers: Vec<_> = servers
            .into_iter()
            .map(|(kind, pids, error)| {
                let (roots, stats) = root_stats(&metrics, &pids);
                SharedServerSample {
                    kind,
                    roots,
                    stats,
                    error,
                }
            })
            .collect();
        if self.mode == Mode::Summary {
            panes.sort_by(|a, b| {
                match (a.stats.tree.cpu_percent, b.stats.tree.cpu_percent) {
                    (Some(a), Some(b)) => b.total_cmp(&a),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
                .then_with(|| a.target.pane_id.cmp(&b.target.pane_id))
            });
            if panes.is_empty() && servers.is_empty() && errors.is_empty() {
                totals = MetricTotals {
                    cpu_percent: Some(0.0),
                    rss_bytes: Some(0),
                    process_count: 0,
                };
            }
        }
        let agent_count = panes
            .iter()
            .filter(|pane| pane.target.agent.is_some())
            .count();
        Ok(MonitorSample {
            capacity: self.provider.capacity(),
            panes,
            servers,
            totals,
            agent_count,
            errors,
            metrics,
        })
    }
}

/// Herdr may list an agent and its children in the same foreground group.
/// Own means the outer process(es); tree still includes every distinct root
/// and descendant.
fn root_stats(metrics: &MetricsSnapshot, roots: &[u32]) -> (Vec<u32>, RootMetrics) {
    let mut stats = metrics.totals(roots);
    let outer = metrics.outer_roots(roots);
    stats.own = metrics.totals(&outer).own;
    (outer, stats)
}

fn is_monitor_process(process: &PaneProcessInfo) -> bool {
    let Some(executable) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
    else {
        return false;
    };
    process
        .foreground_processes
        .iter()
        .any(|process| process_executable(process).as_deref() == Some(executable.as_path()))
}
