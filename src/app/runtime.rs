use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::herdr::{
    socket_path_from_env, EventShutdown, EventStream, HerdrClient, HerdrEvent, SocketPaneController,
};
use crate::metrics::{LocalProcessMetricsProvider, ProcessMetricsProvider};

use super::{
    is_monitor_process, FocusTracker, LaunchContext, Mode, Monitor, MonitorSample,
    SharedServerFiles,
};

#[cfg(test)]
mod socket_tests;

#[derive(Debug)]
pub enum MonitorUpdate {
    Target {
        generation: u64,
        pane_id: Option<String>,
    },
    Sample {
        generation: u64,
        result: Result<MonitorSample, String>,
    },
    Status(Option<String>),
}

#[derive(Clone, Default)]
struct Selection {
    generation: u64,
    pane_id: Option<String>,
}

#[derive(Default)]
struct Shared {
    selection: Mutex<Selection>,
    changed: Condvar,
    stop: Arc<AtomicBool>,
    socket: Mutex<Option<EventShutdown>>,
}

impl Shared {
    fn publish(&self, pane_id: Option<String>, sender: &mpsc::Sender<MonitorUpdate>) {
        let mut selection = self.selection.lock().unwrap();
        if selection.pane_id != pane_id {
            selection.generation += 1;
            selection.pane_id = pane_id.clone();
            let _ = sender.send(MonitorUpdate::Target {
                generation: selection.generation,
                pane_id,
            });
            self.changed.notify_all();
        }
    }

    fn wait_retry(&self, duration: Duration) {
        let selection = self.selection.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(selection, duration, |_| !self.stop.load(Ordering::Relaxed))
            .unwrap();
    }
}

/// Event reads block on a dedicated socket. Sampling has a separate one-second
/// clock and persistent collector. UI receives only state/data messages.
pub struct MonitorWorker {
    pub updates: mpsc::Receiver<MonitorUpdate>,
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl MonitorWorker {
    pub fn start(mode: Mode, context: LaunchContext) -> Result<Self, String> {
        let path = socket_path_from_env().map_err(|error| error.to_string())?;
        Ok(Self::with_socket(mode, context, path))
    }

    fn with_socket(mode: Mode, context: LaunchContext, path: PathBuf) -> Self {
        let monitor = Monitor::new(
            SocketPaneController::with_socket(path.clone()),
            LocalProcessMetricsProvider::new(),
            mode,
            context.clone(),
        )
        .with_shared_servers(SharedServerFiles::from_env());
        let shared = Arc::new(Shared::default());
        let (sender, updates) = mpsc::channel();
        let mut handles = vec![spawn_sampler(monitor, Arc::clone(&shared), sender.clone())];
        if mode == Mode::Focused {
            let event_shared = Arc::clone(&shared);
            handles.push(thread::spawn(move || {
                follow_focus(context, path, event_shared, sender)
            }));
        }
        Self {
            updates,
            shared,
            handles,
        }
    }
}

fn spawn_sampler<C, P>(
    mut monitor: Monitor<C, P>,
    shared: Arc<Shared>,
    sender: mpsc::Sender<MonitorUpdate>,
) -> JoinHandle<()>
where
    C: HerdrClient + Send + 'static,
    P: ProcessMetricsProvider + Send + 'static,
{
    monitor.stop = Arc::clone(&shared.stop);
    thread::spawn(move || {
        let mut next_sample_at = Instant::now();
        while !shared.stop.load(Ordering::Relaxed) {
            let current = shared.selection.lock().unwrap();
            // Focus changes publish immediately, but only the latest target is
            // sampled on the metric clock. Rapid focus must not multiply full
            // process-table refreshes. Clearing a disconnected target is immediate.
            let (current, _) = shared
                .changed
                .wait_timeout_while(
                    current,
                    next_sample_at.saturating_duration_since(Instant::now()),
                    |current| {
                        !(shared.stop.load(Ordering::Relaxed)
                            || monitor.mode == Mode::Focused && current.pane_id.is_none())
                    },
                )
                .unwrap();
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            let selection = current.clone();
            drop(current);
            let started = Instant::now();
            let idle = monitor.mode == Mode::Focused && selection.pane_id.is_none();
            if !idle {
                next_sample_at = started + Duration::from_secs(1);
            }
            monitor.set_target(selection.pane_id);
            let result = monitor.sample();
            let current = shared.selection.lock().unwrap();
            // Never publish old-target work after a focus transition. The UI
            // also checks generations, including a message already in flight.
            if current.generation == selection.generation
                && sender
                    .send(MonitorUpdate::Sample {
                        generation: selection.generation,
                        result,
                    })
                    .is_err()
            {
                break;
            }
            let remaining = Duration::from_secs(1).saturating_sub(started.elapsed());
            if idle {
                // Clear collector baselines once, then sleep until a target
                // arrives. No process-table polling while there is no target.
                drop(
                    shared
                        .changed
                        .wait_while(current, |current| {
                            current.generation == selection.generation
                                && !shared.stop.load(Ordering::Relaxed)
                        })
                        .unwrap(),
                );
                continue;
            }
            let _ = shared
                .changed
                .wait_timeout_while(current, remaining, |current| {
                    current.generation == selection.generation
                        && !shared.stop.load(Ordering::Relaxed)
                })
                .unwrap();
        }
    })
}

fn follow_focus(
    context: LaunchContext,
    path: PathBuf,
    shared: Arc<Shared>,
    sender: mpsc::Sender<MonitorUpdate>,
) {
    let mut tracker = FocusTracker::new(context.monitor_pane_id.clone());
    let mut client = SocketPaneController::with_socket(path.clone());
    let mut launch_target = context.target_pane_id;
    let mut retry = Duration::from_millis(500);
    while !shared.stop.load(Ordering::Relaxed) {
        let result = (|| -> Result<(), String> {
            let mut events = EventStream::connect(&path).map_err(|error| error.to_string())?;
            {
                let mut socket = shared.socket.lock().unwrap();
                if shared.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                *socket = Some(events.shutdown_handle());
            }
            // Subscribe/ack before snapshot: the socket buffers changes that
            // arrive during bootstrap. Reconcile afresh on every reconnect.
            let snapshot = client.snapshot().map_err(|error| error.to_string())?;
            tracker.begin_connection(&snapshot);
            // The pane that opened this monitor is its initial target. A
            // snapshot can reflect another attached client's focus; it must
            // not replace the captured launch context. Later pane-focus events
            // still select new targets, and reconnects use a fresh snapshot.
            let preferred = launch_target.take();
            reconcile(
                &mut tracker,
                &mut client,
                &snapshot,
                preferred.as_deref(),
                &shared.stop,
            );
            shared.publish(tracker.target().map(str::to_owned), &sender);
            let _ = sender.send(MonitorUpdate::Status(None));
            retry = Duration::from_millis(500);
            while !shared.stop.load(Ordering::Relaxed) {
                let Some(event) = events.next_event().map_err(|error| error.to_string())? else {
                    return Err("Herdr event connection closed".into());
                };
                if matches!(
                    event,
                    HerdrEvent::Unknown { .. }
                        | HerdrEvent::TabFocused { .. }
                        | HerdrEvent::WorkspaceFocused { .. }
                ) {
                    // Herdr emits workspace/tab/pane focus together. Only the
                    // pane event identifies the actual target. Re-reading focus
                    // for the preceding events can restore another client's
                    // stale snapshot focus before a monitor focus is ignored.
                    continue;
                }
                let preferred = event_preferred(&mut tracker, &event);
                // A closed/moved target must disappear before any blocking lookup.
                shared.publish(tracker.target().map(str::to_owned), &sender);
                let snapshot = client.snapshot().map_err(|error| error.to_string())?;
                reconcile(
                    &mut tracker,
                    &mut client,
                    &snapshot,
                    preferred.as_deref(),
                    &shared.stop,
                );
                shared.publish(tracker.target().map(str::to_owned), &sender);
            }
            Ok(())
        })();
        if let Some(socket) = shared.socket.lock().unwrap().take() {
            let _ = socket.shutdown();
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if let Err(error) = result {
            shared.publish(None, &sender);
            let _ = sender.send(MonitorUpdate::Status(Some(format!(
                "Reconnecting to Herdr: {error}"
            ))));
        }
        shared.wait_retry(retry);
        retry = (retry * 2).min(Duration::from_secs(5));
    }
}

fn event_preferred(tracker: &mut FocusTracker, event: &HerdrEvent) -> Option<String> {
    match event {
        HerdrEvent::PaneFocused { pane_id, .. } => Some(pane_id.clone()),
        HerdrEvent::PaneClosed { pane_id, .. } | HerdrEvent::PaneExited { pane_id, .. } => {
            tracker.invalidate(pane_id);
            tracker.target().map(str::to_owned)
        }
        HerdrEvent::PaneMoved {
            previous_pane_id,
            pane,
            ..
        } if tracker.target() == Some(previous_pane_id) => {
            tracker.invalidate(previous_pane_id);
            Some(pane.pane_id.clone())
        }
        // Layout and unrelated lifecycle events must not override a manual
        // pane focus from another client with an older snapshot focus.
        _ => tracker.target().map(str::to_owned),
    }
}

fn reconcile<C: HerdrClient>(
    tracker: &mut FocusTracker,
    client: &mut C,
    snapshot: &crate::herdr::SessionSnapshot,
    preferred: Option<&str>,
    stop: &AtomicBool,
) {
    let candidates: BTreeSet<_> = preferred
        .into_iter()
        .chain(snapshot.focused_pane_id.as_deref())
        .chain(tracker.target())
        .collect();
    let mut excluded = BTreeSet::new();
    for id in candidates {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(process) = client.process_info(id) {
            if is_monitor_process(&process) {
                excluded.insert(id.to_owned());
            }
        }
    }
    tracker.reconcile(snapshot, &excluded, preferred);
}

impl Drop for MonitorWorker {
    fn drop(&mut self) {
        {
            // Pair stop/notify with the same mutex used by the waiters so a
            // shutdown cannot fall between the predicate check and sleep.
            let _selection = self.shared.selection.lock().unwrap();
            self.shared.stop.store(true, Ordering::Relaxed);
            self.shared.changed.notify_all();
        }
        if let Some(socket) = self.shared.socket.lock().unwrap().take() {
            let _ = socket.shutdown();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::herdr::{
        AgentInfo, ForegroundProcess, PaneInfo, PaneProcessInfo, Result as HerdrResult,
        SessionSnapshot,
    };
    use crate::metrics::{MetricsSnapshot, ProcessMetrics};

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            terminal_id: format!("terminal-{id}"),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            ..Default::default()
        }
    }

    fn snapshot(focused: &str) -> SessionSnapshot {
        SessionSnapshot {
            focused_pane_id: Some(focused.into()),
            panes: ["A", "B", "C", "monitor"].into_iter().map(pane).collect(),
            ..Default::default()
        }
    }

    struct FakeHerdr {
        snapshot: SessionSnapshot,
        cleared_before_lookup: Option<Arc<Shared>>,
    }

    impl FakeHerdr {
        fn assert_selection_cleared(&self) {
            if let Some(shared) = &self.cleared_before_lookup {
                assert!(shared.selection.lock().unwrap().pane_id.is_none());
            }
        }
    }

    impl HerdrClient for FakeHerdr {
        fn snapshot(&mut self) -> HerdrResult<SessionSnapshot> {
            self.assert_selection_cleared();
            Ok(self.snapshot.clone())
        }

        fn panes(&mut self) -> HerdrResult<Vec<PaneInfo>> {
            panic!("sampling should use one coherent snapshot")
        }

        fn agents(&mut self) -> HerdrResult<Vec<AgentInfo>> {
            panic!("sampling should use one coherent snapshot")
        }

        fn process_info(&mut self, pane_id: &str) -> HerdrResult<PaneProcessInfo> {
            self.assert_selection_cleared();
            Ok(PaneProcessInfo {
                pane_id: pane_id.into(),
                foreground_processes: vec![ForegroundProcess {
                    pid: if pane_id == "A" { 10 } else { 20 },
                    name: "task".into(),
                    argv: Some(vec!["task".into()]),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
    }

    #[test]
    fn unrelated_lifecycle_events_keep_event_selected_pane_over_other_client_snapshot() {
        let stale = snapshot("A");
        let mut tracker = FocusTracker::new(Some("monitor".into()));
        tracker.reconcile(&stale, &BTreeSet::new(), Some("B"));
        let mut client = FakeHerdr {
            snapshot: stale.clone(),
            cleared_before_lookup: None,
        };
        for event in [
            HerdrEvent::LayoutUpdated {
                layout: serde_json::json!({"fixture": true}),
            },
            HerdrEvent::PaneCreated { pane: pane("C") },
            HerdrEvent::PaneClosed {
                pane_id: "C".into(),
                workspace_id: "workspace".into(),
            },
        ] {
            let preferred = event_preferred(&mut tracker, &event);
            reconcile(
                &mut tracker,
                &mut client,
                &stale,
                preferred.as_deref(),
                &AtomicBool::new(false),
            );
            assert_eq!(tracker.target(), Some("B"));
        }
    }

    #[test]
    fn complete_monitor_focus_triplet_keeps_latest_pane_event_over_stale_snapshot() {
        let stale = snapshot("A");
        let mut tracker = FocusTracker::new(Some("monitor".into()));
        tracker.reconcile(&stale, &BTreeSet::new(), Some("B"));
        let mut client = FakeHerdr {
            snapshot: stale.clone(),
            cleared_before_lookup: None,
        };
        for event in [
            HerdrEvent::WorkspaceFocused {
                workspace_id: "workspace".into(),
            },
            HerdrEvent::TabFocused {
                tab_id: "tab".into(),
                workspace_id: "workspace".into(),
            },
            HerdrEvent::PaneFocused {
                pane_id: "monitor".into(),
                workspace_id: "workspace".into(),
            },
        ] {
            let preferred = event_preferred(&mut tracker, &event);
            reconcile(
                &mut tracker,
                &mut client,
                &stale,
                preferred.as_deref(),
                &AtomicBool::new(false),
            );
            assert_eq!(tracker.target(), Some("B"));
        }
    }

    #[test]
    fn closed_or_exited_target_is_cleared_before_snapshot_and_process_lookups() {
        for event in [
            HerdrEvent::PaneClosed {
                pane_id: "B".into(),
                workspace_id: "workspace".into(),
            },
            HerdrEvent::PaneExited {
                pane_id: "B".into(),
                workspace_id: "workspace".into(),
            },
        ] {
            let mut tracker = FocusTracker::new(Some("monitor".into()));
            tracker.reconcile(&snapshot("B"), &BTreeSet::new(), None);
            let shared = Arc::new(Shared::default());
            let (sender, updates) = mpsc::channel();
            shared.publish(Some("B".into()), &sender);
            let _ = updates.recv().unwrap();

            let preferred = event_preferred(&mut tracker, &event);
            assert_eq!(tracker.target(), None);
            shared.publish(tracker.target().map(str::to_owned), &sender);
            assert!(matches!(
                updates.recv().unwrap(),
                MonitorUpdate::Target { pane_id: None, .. }
            ));

            // The fake fails if either lookup happens while the old selection is published.
            let mut client = FakeHerdr {
                snapshot: snapshot("B"),
                cleared_before_lookup: Some(Arc::clone(&shared)),
            };
            let stale = client.snapshot().unwrap();
            reconcile(
                &mut tracker,
                &mut client,
                &stale,
                preferred.as_deref(),
                &shared.stop,
            );
            assert_eq!(
                tracker.target(),
                None,
                "stale state cannot resurrect an exited target"
            );
        }
    }

    struct BlockingMetrics {
        entered: mpsc::Sender<(usize, Vec<u32>)>,
        unblock_first: Option<mpsc::Receiver<()>>,
        refresh_count: usize,
        calls: Arc<Mutex<Vec<Vec<u32>>>>,
    }

    impl ProcessMetricsProvider for BlockingMetrics {
        fn refresh(&mut self, roots: &[u32], excluded: &[u32]) -> MetricsSnapshot {
            assert!(excluded.contains(&999));
            self.refresh_count += 1;
            self.calls.lock().unwrap().push(roots.into());
            self.entered
                .send((self.refresh_count, roots.into()))
                .unwrap();
            if let Some(unblock) = self.unblock_first.take() {
                unblock
                    .recv_timeout(Duration::from_secs(3))
                    .expect("first sample was not released");
            }
            MetricsSnapshot {
                sampled_at: Instant::now(),
                processes: roots
                    .iter()
                    .map(|&pid| {
                        (
                            pid,
                            ProcessMetrics {
                                pid,
                                ppid: 1,
                                name: "task".into(),
                                cmdline: "task".into(),
                                children: vec![],
                                cpu_percent: Some(self.refresh_count as f64),
                                rss_bytes: Some(u64::from(pid)),
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
                missing_roots: vec![],
            }
        }
    }

    fn shutdown_worker(worker: MonitorWorker) {
        let (stopped, shutdown) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            drop(worker);
            stopped.send(()).unwrap();
        });
        shutdown
            .recv_timeout(Duration::from_millis(500))
            .expect("shutdown did not wake the sampler wait");
        cleanup.join().unwrap();
    }

    #[test]
    fn shutdown_wakes_sampler_waiting_for_its_first_normal_target() {
        let (entered, sampling) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let monitor = Monitor::new(
            FakeHerdr {
                snapshot: snapshot("A"),
                cleared_before_lookup: None,
            },
            BlockingMetrics {
                entered,
                unblock_first: None,
                refresh_count: 0,
                calls: Arc::clone(&calls),
            },
            Mode::Focused,
            LaunchContext {
                monitor_pane_id: Some("monitor".into()),
                monitor_pid: 999,
                ..Default::default()
            },
        );
        let shared = Arc::new(Shared::default());
        let (sender, updates) = mpsc::channel();
        let handle = spawn_sampler(monitor, Arc::clone(&shared), sender);
        let worker = MonitorWorker {
            updates,
            shared,
            handles: vec![handle],
        };
        assert_eq!(
            sampling.recv_timeout(Duration::from_secs(3)).unwrap(),
            (1, vec![])
        );
        let update = worker.updates.recv_timeout(Duration::from_secs(3)).unwrap();
        let MonitorUpdate::Sample {
            generation: 0,
            result,
        } = update
        else {
            panic!("expected the initial empty sample, got {update:?}");
        };
        assert!(result.unwrap().panes.is_empty());
        shutdown_worker(worker);
        assert_eq!(*calls.lock().unwrap(), vec![Vec::<u32>::new()]);
    }

    #[test]
    fn target_switch_discards_inflight_sample_reuses_provider_and_shutdown_wakes_sampler() {
        let (entered, sampling) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let monitor = Monitor::new(
            FakeHerdr {
                snapshot: snapshot("A"),
                cleared_before_lookup: None,
            },
            BlockingMetrics {
                entered,
                unblock_first: Some(blocked),
                refresh_count: 0,
                calls: Arc::clone(&calls),
            },
            Mode::Focused,
            LaunchContext {
                monitor_pane_id: Some("monitor".into()),
                target_pane_id: None,
                monitor_pid: 999,
            },
        );
        let shared = Arc::new(Shared::default());
        let (sender, updates) = mpsc::channel();
        shared.publish(Some("A".into()), &sender);
        assert!(matches!(
            updates.recv().unwrap(),
            MonitorUpdate::Target { generation: 1, .. }
        ));
        let handle = spawn_sampler(monitor, Arc::clone(&shared), sender.clone());
        let worker = MonitorWorker {
            updates,
            shared: Arc::clone(&shared),
            handles: vec![handle],
        };

        assert_eq!(
            sampling.recv_timeout(Duration::from_secs(3)).unwrap(),
            (1, vec![10])
        );
        // This switch happens while A's provider refresh is blocked, not between ticks.
        shared.publish(Some("B".into()), &sender);
        unblock.send(()).unwrap();
        assert_eq!(
            sampling.recv_timeout(Duration::from_secs(3)).unwrap(),
            (2, vec![20])
        );
        assert!(
            matches!(worker.updates.recv_timeout(Duration::from_secs(3)).unwrap(),
            MonitorUpdate::Target { generation: 2, pane_id: Some(ref id) } if id == "B")
        );
        let next = worker.updates.recv_timeout(Duration::from_secs(3)).unwrap();
        let MonitorUpdate::Sample { generation, result } = next else {
            panic!("expected the replacement target's sample, got {next:?}");
        };
        assert_eq!(
            generation, 2,
            "A's result must never be published after the target changes"
        );
        let sample = result.unwrap();
        assert_eq!(sample.panes[0].target.pane_id, "B");
        assert_eq!(
            sample.totals.cpu_percent,
            Some(2.0),
            "the same provider survives the switch"
        );
        assert!(!sample.metrics.processes.contains_key(&10));
        assert!(worker.updates.try_recv().is_err());

        // B just completed, so an unwoken sampler would still wait almost one second.
        shutdown_worker(worker);
        assert_eq!(*calls.lock().unwrap(), vec![vec![10], vec![20]]);
    }

    #[test]
    fn rapid_focus_changes_coalesce_on_the_metric_clock() {
        let (entered, sampling) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let monitor = Monitor::new(
            FakeHerdr {
                snapshot: snapshot("A"),
                cleared_before_lookup: None,
            },
            BlockingMetrics {
                entered,
                unblock_first: None,
                refresh_count: 0,
                calls: Arc::clone(&calls),
            },
            Mode::Focused,
            LaunchContext {
                monitor_pane_id: Some("monitor".into()),
                monitor_pid: 999,
                ..Default::default()
            },
        );
        let shared = Arc::new(Shared::default());
        let (sender, updates) = mpsc::channel();
        shared.publish(Some("A".into()), &sender);
        let handle = spawn_sampler(monitor, Arc::clone(&shared), sender.clone());
        let worker = MonitorWorker {
            updates,
            shared: Arc::clone(&shared),
            handles: vec![handle],
        };
        assert_eq!(
            sampling.recv_timeout(Duration::from_secs(3)).unwrap(),
            (1, vec![10])
        );
        for id in ["B", "A", "B", "A", "B", "A", "B", "A", "B", "C"] {
            shared.publish(Some(id.into()), &sender);
        }
        assert_eq!(
            shared.selection.lock().unwrap().pane_id.as_deref(),
            Some("C")
        );
        assert!(sampling.recv_timeout(Duration::from_millis(500)).is_err());
        assert_eq!(
            sampling.recv_timeout(Duration::from_secs(2)).unwrap(),
            (2, vec![20])
        );
        shutdown_worker(worker);
        assert_eq!(*calls.lock().unwrap(), vec![vec![10], vec![20]]);
    }
}
