//! Explicit-root process metrics for Herdr panes.
//!
//! CPU is raw core percent: 100% means one fully occupied logical core.
//! Missing/cold CPU and unreadable memory stay unknown, rather than becoming 0.

mod collector;
mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// One observed local OS process. Pane and agent identities belong to the caller.
#[derive(Clone, Debug)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cmdline: String,
    pub children: Vec<u32>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricTotals {
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    /// Distinct processes observed in the current process table.
    pub process_count: usize,
}

/// Host capacity for presentation. Collector CPU values remain core percentages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemCapacity {
    pub logical_cpus: usize,
    pub memory_bytes: u64,
}

impl SystemCapacity {
    pub fn cpu_percent(self, raw: Option<f64>) -> Option<f64> {
        raw.filter(|value| value.is_finite() && *value >= 0.0)
            .filter(|_| self.logical_cpus > 0)
            .map(|value| value / self.logical_cpus as f64)
    }

    pub fn memory_percent(self, bytes: Option<u64>) -> Option<f64> {
        bytes
            .filter(|_| self.memory_bytes > 0)
            .map(|value| value as f64 / self.memory_bytes as f64 * 100.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RootMetrics {
    /// Only distinct requested roots, never their children.
    pub own: MetricTotals,
    /// The union of requested roots and all their observed descendants.
    pub tree: MetricTotals,
    pub missing_roots: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct MetricsSnapshot {
    pub sampled_at: Instant,
    pub processes: BTreeMap<u32, ProcessMetrics>,
    pub missing_roots: Vec<u32>,
}

impl MetricsSnapshot {
    /// Keep requested roots that are not descendants of another requested root.
    /// Missing roots stay visible so incomplete totals remain unknown. Within a
    /// parent cycle, the lowest requested PID represents that shared subtree.
    pub fn outer_roots(&self, roots: &[u32]) -> Vec<u32> {
        let requested: BTreeSet<_> = roots.iter().copied().collect();
        let reachable: BTreeMap<u32, BTreeSet<u32>> = requested
            .iter()
            .map(|&pid| (pid, self.process_ids(&[pid]).into_iter().collect()))
            .collect();
        requested
            .into_iter()
            .filter(|root| {
                !reachable.iter().any(|(other, descendants)| {
                    if other == root || !descendants.contains(root) {
                        return false;
                    }
                    // A strict ancestor wins regardless of PID ordering. For
                    // mutually reachable roots, retain one stable representative.
                    !reachable[root].contains(other) || other < root
                })
            })
            .collect()
    }

    /// Present each given root as a separate tree: remove it from its parent's
    /// children so another root's tree stops there. Measurements are unchanged.
    pub fn detach(&mut self, roots: &[u32]) {
        let roots: BTreeSet<_> = roots.iter().copied().collect();
        for process in self.processes.values_mut() {
            process.children.retain(|child| !roots.contains(child));
        }
    }

    /// Stable depth-first membership for an explicit root set. Overlapping
    /// roots appear once; missing roots are omitted and reported by `totals`.
    pub fn process_ids(&self, roots: &[u32]) -> Vec<u32> {
        let mut roots = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();
        let mut pending: Vec<_> = roots.into_iter().rev().collect();
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        while let Some(pid) = pending.pop() {
            let Some(process) = self.processes.get(&pid) else {
                continue;
            };
            if seen.insert(pid) {
                result.push(pid);
                pending.extend(process.children.iter().rev().copied());
            }
        }
        result
    }

    /// Select several roots from this one sample without measuring again.
    /// An absent requested root makes resource totals incomplete (`None`),
    /// while process_count still describes the observed portion of the tree.
    pub fn totals(&self, roots: &[u32]) -> RootMetrics {
        let requested: BTreeSet<_> = roots.iter().copied().collect();
        let missing_roots: Vec<_> = requested
            .iter()
            .filter(|pid| !self.processes.contains_key(pid))
            .copied()
            .collect();
        let own: BTreeSet<_> = requested
            .iter()
            .filter(|pid| self.processes.contains_key(pid))
            .copied()
            .collect();
        let tree = self.process_ids(roots).into_iter().collect();
        let complete = missing_roots.is_empty();
        RootMetrics {
            own: self.sum(&own, complete),
            tree: self.sum(&tree, complete),
            missing_roots,
        }
    }

    fn sum(&self, pids: &BTreeSet<u32>, complete: bool) -> MetricTotals {
        let mut totals = MetricTotals {
            cpu_percent: Some(0.0),
            rss_bytes: Some(0),
            process_count: pids.len(),
        };
        for pid in pids {
            let Some(process) = self.processes.get(pid) else {
                totals.cpu_percent = None;
                totals.rss_bytes = None;
                continue;
            };
            totals.cpu_percent = totals
                .cpu_percent
                .zip(process.cpu_percent)
                .map(|(sum, value)| sum + value);
            totals.rss_bytes = totals
                .rss_bytes
                .zip(process.rss_bytes)
                .and_then(|(sum, value)| sum.checked_add(value));
        }
        if !complete || pids.is_empty() {
            totals.cpu_percent = None;
            totals.rss_bytes = None;
        }
        totals
    }
}

/// Refresh all requested roots in one batch. Keep the same provider between
/// calls so CPU deltas use the previous sample, independent of UI focus.
pub trait ProcessMetricsProvider {
    fn refresh(&mut self, roots: &[u32], excluded_subtrees: &[u32]) -> MetricsSnapshot;

    fn capacity(&self) -> SystemCapacity {
        SystemCapacity::default()
    }
}

pub struct LocalProcessMetricsProvider {
    source: source::SystemSource,
    collector: collector::Collector,
}

impl LocalProcessMetricsProvider {
    pub fn new() -> Self {
        Self {
            source: source::SystemSource::new(),
            collector: collector::Collector::default(),
        }
    }
}

impl Default for LocalProcessMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessMetricsProvider for LocalProcessMetricsProvider {
    fn capacity(&self) -> SystemCapacity {
        self.source.capacity()
    }

    fn refresh(&mut self, roots: &[u32], excluded_subtrees: &[u32]) -> MetricsSnapshot {
        // No target means no OS enumeration, but still pass an empty batch to
        // the collector so a later target cannot reuse an old CPU baseline.
        let topology = if roots.is_empty() {
            Vec::new()
        } else {
            self.source.refresh()
        };
        let now = Instant::now();
        self.collector
            .sample(topology, roots, excluded_subtrees, now, |pid| {
                self.source.read(pid)
            })
    }
}

#[cfg(test)]
mod tests;
