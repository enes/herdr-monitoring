//! PPID trees and CPU deltas with persistent per-process sampling state.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Instant, SystemTime};

use super::source::{ProcessMeta, ProcessReading};
use super::{MetricsSnapshot, ProcessMetrics};

struct CpuSample {
    total: f64,
    at: Instant,
    started: Option<SystemTime>,
}

#[derive(Default)]
pub(super) struct Collector {
    previous_cpu: HashMap<u32, CpuSample>,
}

impl Collector {
    pub fn sample(
        &mut self,
        processes: Vec<ProcessMeta>,
        roots: &[u32],
        excluded: &[u32],
        now: Instant,
        mut read: impl FnMut(u32) -> Option<ProcessReading>,
    ) -> MetricsSnapshot {
        let mut forest = Self::build_forest(processes);
        let mut omitted = HashSet::new();
        let mut pending = excluded.to_vec();
        while let Some(pid) = pending.pop() {
            if omitted.insert(pid) {
                if let Some(meta) = forest.get(&pid) {
                    pending.extend(meta.children.iter().copied());
                }
            }
        }
        forest.retain(|pid, _| !omitted.contains(pid));
        for meta in forest.values_mut() {
            meta.children.retain(|pid| !omitted.contains(pid));
            meta.children.sort_unstable();
        }

        let roots: BTreeSet<_> = roots.iter().copied().collect();
        let mut processes = BTreeMap::new();
        let mut visited = HashSet::new();
        // One shared visited set keeps every baseline and resource count unique
        // even when roots overlap or a child root sorts before its parent.
        for &root in &roots {
            self.walk(&forest, root, now, &mut read, &mut visited, &mut processes);
        }
        self.previous_cpu.retain(|pid, _| visited.contains(pid));
        let missing_roots = roots
            .into_iter()
            .filter(|pid| !processes.contains_key(pid))
            .collect();
        MetricsSnapshot {
            sampled_at: now,
            processes,
            missing_roots,
        }
    }

    fn build_forest(processes: Vec<ProcessMeta>) -> HashMap<u32, ProcessMeta> {
        let mut forest: HashMap<_, _> = processes
            .into_iter()
            .map(|mut process| {
                process.children.clear();
                (process.pid, process)
            })
            .collect();
        let links: Vec<_> = forest
            .values()
            .filter(|process| process.ppid != process.pid)
            .map(|process| (process.ppid, process.pid))
            .collect();
        for (parent, child) in links {
            if let Some(process) = forest.get_mut(&parent) {
                process.children.push(child);
            }
        }
        forest
    }

    fn walk(
        &mut self,
        forest: &HashMap<u32, ProcessMeta>,
        pid: u32,
        now: Instant,
        read: &mut impl FnMut(u32) -> Option<ProcessReading>,
        visited: &mut HashSet<u32>,
        processes: &mut BTreeMap<u32, ProcessMetrics>,
    ) {
        let Some(meta) = forest.get(&pid) else {
            return;
        };
        if !visited.insert(pid) {
            return;
        }
        processes.insert(pid, self.measure(meta, now, read(pid)));
        for &child in &meta.children {
            self.walk(forest, child, now, read, visited, processes);
        }
    }

    fn measure(
        &mut self,
        meta: &ProcessMeta,
        now: Instant,
        reading: Option<ProcessReading>,
    ) -> ProcessMetrics {
        let mut process = ProcessMetrics {
            pid: meta.pid,
            ppid: meta.ppid,
            name: meta.name.clone(),
            cmdline: meta.cmdline.clone(),
            children: meta.children.clone(),
            cpu_percent: None,
            rss_bytes: None,
        };
        if let Some(reading) = reading {
            process.rss_bytes = Some(reading.rss);
            let total = reading.cpu_time;
            if let Some(previous) = self.previous_cpu.get(&meta.pid) {
                let elapsed = now.saturating_duration_since(previous.at).as_secs_f64();
                if elapsed > 0.0
                    && reading.started.is_some()
                    && previous.started == reading.started
                    && total >= previous.total
                {
                    process.cpu_percent =
                        Some((((total - previous.total) / elapsed) * 100.0).max(0.0));
                }
            }
            self.previous_cpu.insert(
                meta.pid,
                CpuSample {
                    total,
                    at: now,
                    started: reading.started,
                },
            );
        } else {
            self.previous_cpu.remove(&meta.pid);
        }
        process
    }
}
