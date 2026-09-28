use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use herdr_resource_monitor::app::{
    LaunchContext, Mode, Monitor, RootSource, SharedServerFiles, SharedServerKind,
};
use herdr_resource_monitor::herdr::{
    AgentInfo, AgentSessionInfo, ForegroundProcess, HerdrClient, HerdrError, PaneInfo,
    PaneProcessInfo, Result, SessionSnapshot,
};
use herdr_resource_monitor::metrics::{MetricsSnapshot, ProcessMetrics, ProcessMetricsProvider};
use serde_json::Value;

struct Step {
    snapshot: SessionSnapshot,
    processes: BTreeMap<String, Result<PaneProcessInfo>>,
}

struct FakeHerdr {
    steps: VecDeque<Step>,
    current: BTreeMap<String, Result<PaneProcessInfo>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl HerdrClient for FakeHerdr {
    fn snapshot(&mut self) -> Result<SessionSnapshot> {
        self.calls.lock().unwrap().push("snapshot".into());
        let step = self.steps.pop_front().expect("unexpected extra snapshot");
        self.current = step.processes;
        Ok(step.snapshot)
    }

    fn panes(&mut self) -> Result<Vec<PaneInfo>> {
        panic!("the snapshot already provides a coherent pane list")
    }

    fn agents(&mut self) -> Result<Vec<AgentInfo>> {
        panic!("the snapshot already provides agent metadata")
    }

    fn process_info(&mut self, pane_id: &str) -> Result<PaneProcessInfo> {
        self.calls.lock().unwrap().push(pane_id.into());
        self.current
            .remove(pane_id)
            .expect("unexpected process-info request")
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RefreshCall {
    roots: Vec<u32>,
    excluded: Vec<u32>,
}

struct FakeMetrics {
    samples: VecDeque<MetricsSnapshot>,
    calls: Arc<Mutex<Vec<RefreshCall>>>,
}

impl ProcessMetricsProvider for FakeMetrics {
    fn refresh(&mut self, roots: &[u32], excluded_subtrees: &[u32]) -> MetricsSnapshot {
        self.calls.lock().unwrap().push(RefreshCall {
            roots: roots.to_vec(),
            excluded: excluded_subtrees.to_vec(),
        });
        self.samples
            .pop_front()
            .expect("unexpected extra metrics refresh")
    }
}

fn fixture(source: &str) -> (SessionSnapshot, PaneProcessInfo) {
    let data: Value = serde_json::from_str(source).unwrap();
    (
        serde_json::from_value(data["snapshot"]["result"]["snapshot"].clone()).unwrap(),
        serde_json::from_value(data["process_info"]["result"]["process_info"].clone()).unwrap(),
    )
}

fn agent_pane(id: &str) -> PaneInfo {
    PaneInfo {
        pane_id: id.into(),
        terminal_id: format!("terminal-{id}"),
        workspace_id: "workspace".into(),
        tab_id: format!("tab-{id}"),
        label: Some("same agent name".into()),
        agent: Some("codex".into()),
        ..Default::default()
    }
}

fn snapshot(panes: Vec<PaneInfo>, focus: &str) -> SessionSnapshot {
    SessionSnapshot {
        version: "0.9.1".into(),
        protocol: 22,
        focused_pane_id: Some(focus.into()),
        panes,
        ..Default::default()
    }
}

fn process(pane_id: &str, roots: &[u32]) -> PaneProcessInfo {
    PaneProcessInfo {
        pane_id: pane_id.into(),
        shell_pid: Some(1234),
        foreground_process_group_id: Some(5678),
        foreground_processes: roots
            .iter()
            .map(|&pid| ForegroundProcess {
                pid,
                name: "codex".into(),
                argv: Some(vec!["codex".into()]),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn step(snapshot: SessionSnapshot, processes: Vec<PaneProcessInfo>) -> Step {
    Step {
        snapshot,
        processes: processes
            .into_iter()
            .map(|process| (process.pane_id.clone(), Ok(process)))
            .collect(),
    }
}

fn metric(pid: u32, ppid: u32, children: &[u32], cpu: Option<f64>, rss: u64) -> ProcessMetrics {
    ProcessMetrics {
        pid,
        ppid,
        name: format!("process-{pid}"),
        cmdline: format!("process-{pid}"),
        children: children.into(),
        cpu_percent: cpu,
        rss_bytes: Some(rss),
    }
}

fn metrics(processes: Vec<ProcessMetrics>) -> MetricsSnapshot {
    MetricsSnapshot {
        sampled_at: Instant::now(),
        processes: processes
            .into_iter()
            .map(|process| (process.pid, process))
            .collect(),
        missing_roots: Vec::new(),
    }
}

type HerdrCalls = Arc<Mutex<Vec<String>>>;
type MetricsCalls = Arc<Mutex<Vec<RefreshCall>>>;

fn monitor(
    steps: Vec<Step>,
    samples: Vec<MetricsSnapshot>,
    mode: Mode,
    target: Option<&str>,
) -> (Monitor<FakeHerdr, FakeMetrics>, HerdrCalls, MetricsCalls) {
    monitor_with_context(
        steps,
        samples,
        mode,
        LaunchContext {
            monitor_pane_id: Some("monitor".into()),
            target_pane_id: target.map(str::to_owned),
            monitor_pid: 999,
        },
    )
}

fn monitor_with_context(
    steps: Vec<Step>,
    samples: Vec<MetricsSnapshot>,
    mode: Mode,
    context: LaunchContext,
) -> (Monitor<FakeHerdr, FakeMetrics>, HerdrCalls, MetricsCalls) {
    let herdr_calls = Arc::new(Mutex::new(Vec::new()));
    let metrics_calls = Arc::new(Mutex::new(Vec::new()));
    let target = context.target_pane_id.clone();
    let mut app = Monitor::new(
        FakeHerdr {
            steps: steps.into(),
            current: BTreeMap::new(),
            calls: Arc::clone(&herdr_calls),
        },
        FakeMetrics {
            samples: samples.into(),
            calls: Arc::clone(&metrics_calls),
        },
        mode,
        context,
    );
    app.set_target(target);
    (app, herdr_calls, metrics_calls)
}

#[test]
fn absent_foreground_uses_explicit_shell_fallback_and_no_agent_is_required() {
    let (snapshot, process) = fixture(include_str!("fixtures/herdr/no_foreground.json"));
    let pane_id = process.pane_id.clone();
    let shell = process.shell_pid.unwrap();
    let (mut app, _, calls) = monitor(
        vec![step(snapshot, vec![process])],
        vec![metrics(vec![metric(shell, 1, &[], None, 2048)])],
        Mode::Focused,
        Some(&pane_id),
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.panes[0].target.pane_id, pane_id);
    assert_eq!(sample.panes[0].root_source, RootSource::ShellFallback);
    assert_eq!(sample.panes[0].roots, vec![shell]);
    assert_eq!(sample.agent_count, 0);
    assert_eq!(sample.totals.cpu_percent, None);
    assert_eq!(sample.totals.rss_bytes, Some(2048));
    assert_eq!(calls.lock().unwrap()[0].roots, vec![shell]);
}

#[test]
fn foreground_pipeline_is_batched_once_without_using_shell_or_pgid_as_roots() {
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one")], "one"),
            vec![process("one", &[82, 81, 82])],
        )],
        vec![metrics(vec![
            metric(81, 1, &[], Some(10.0), 100),
            metric(82, 1, &[], Some(20.0), 200),
        ])],
        Mode::Focused,
        Some("one"),
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.panes[0].root_source, RootSource::Foreground);
    assert_eq!(sample.panes[0].roots, vec![81, 82]);
    assert_eq!(sample.totals.cpu_percent, Some(30.0));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![RefreshCall {
            roots: vec![81, 82],
            excluded: vec![999]
        }]
    );
}

#[test]
fn explicit_target_survives_unrelated_snapshot_focus_and_refreshes_foreground_roots() {
    let (original, mut first_process) =
        fixture(include_str!("fixtures/herdr/agent_without_session.json"));
    let pane_id = first_process.pane_id.clone();
    first_process.foreground_processes[0].pid = 10;
    let mut second_process = first_process.clone();
    second_process.foreground_processes[0].pid = 20;
    let mut first = original.clone();
    first.focused_pane_id = Some("monitor".into());
    first.panes.push(agent_pane("monitor"));
    let mut second = first.clone();
    second.focused_pane_id = Some("another".into());
    second.panes.push(agent_pane("another"));
    let (mut app, herdr_calls, calls) = monitor(
        vec![
            step(first, vec![first_process]),
            step(second, vec![second_process]),
        ],
        vec![
            metrics(vec![metric(10, 1, &[], None, 100)]),
            metrics(vec![metric(20, 1, &[], Some(25.0), 200)]),
        ],
        Mode::Focused,
        Some(&pane_id),
    );
    let first = app.sample().unwrap();
    let second = app.sample().unwrap();
    assert_eq!(first.panes[0].target.pane_id, pane_id);
    assert_eq!(second.panes[0].target.pane_id, pane_id);
    assert!(second.panes[0]
        .target
        .agent
        .as_ref()
        .unwrap()
        .agent_session
        .is_none());
    assert_eq!(first.panes[0].roots, vec![10]);
    assert_eq!(second.panes[0].roots, vec![20]);
    assert_eq!(second.totals.cpu_percent, Some(25.0));
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(
        *herdr_calls.lock().unwrap(),
        vec!["snapshot", &pane_id, "snapshot", &pane_id]
    );
}

#[test]
fn same_pane_codex_shell_claude_refreshes_metadata_and_roots_in_both_modes() {
    let id = "unchanged-pane";
    for mode in [Mode::Focused, Mode::Summary] {
        let mut codex = agent_pane(id);
        codex.agent_status = "working".into();
        codex.tokens.insert("model".into(), "codex-model".into());
        let mut shell = codex.clone();
        shell.agent = None;
        shell.agent_status = "idle".into();
        shell.tokens.clear();
        shell.display_agent = Some("Codex".into());
        shell.agent_session = Some(AgentSessionInfo {
            source: "fixture".into(),
            agent: "codex".into(),
            kind: "id".into(),
            value: "historical-codex-session".into(),
        });
        let mut claude = shell.clone();
        claude.agent = Some("claude".into());
        claude.display_agent = Some("Claude".into());
        claude.agent_session = None;
        claude.agent_status = "blocked".into();
        claude.cwd = Some("/new-project".into());
        claude.tokens.insert("model".into(), "claude-model".into());
        let mut last = snapshot(vec![claude], id);
        last.agents.push(AgentInfo {
            pane_id: id.into(),
            agent: Some("claude".into()),
            name: Some("reviewer".into()),
            agent_status: "blocked".into(),
            ..Default::default()
        });
        let shell_processes = if mode == Mode::Focused {
            vec![process(id, &[])]
        } else {
            vec![]
        };
        let shell_metrics = if mode == Mode::Focused {
            vec![metric(1234, 1, &[], Some(0.0), 50)]
        } else {
            vec![]
        };
        let (mut app, _, calls) = monitor(
            vec![
                step(snapshot(vec![codex], id), vec![process(id, &[10])]),
                step(snapshot(vec![shell], id), shell_processes),
                step(last, vec![process(id, &[20])]),
            ],
            vec![
                metrics(vec![metric(10, 1, &[], Some(10.0), 100)]),
                metrics(shell_metrics),
                metrics(vec![metric(20, 1, &[], Some(20.0), 200)]),
            ],
            mode,
            Some(id),
        );
        let first = app.sample().unwrap();
        let middle = app.sample().unwrap();
        let last = app.sample().unwrap();
        let original = &first.panes[0].target;
        assert_eq!(original.pane_id, id);
        assert_eq!(
            original.agent.as_ref().unwrap().agent.as_deref(),
            Some("codex")
        );
        assert_eq!(
            original.agent.as_ref().unwrap().tokens["model"],
            "codex-model"
        );
        assert!(original.agent.as_ref().unwrap().agent_session.is_none());
        assert_eq!(first.panes[0].roots, [10]);
        assert_eq!(middle.agent_count, 0);
        if mode == Mode::Focused {
            assert_eq!(middle.panes[0].target.pane_id, id);
            assert!(middle.panes[0].target.agent.is_none());
            assert!(middle.panes[0].target.pane.tokens.is_empty());
            assert_eq!(
                middle.panes[0]
                    .target
                    .pane
                    .agent_session
                    .as_ref()
                    .unwrap()
                    .value,
                "historical-codex-session"
            );
            assert_eq!(
                middle.panes[0].target.pane.display_agent.as_deref(),
                Some("Codex")
            );
            assert_eq!(middle.panes[0].root_source, RootSource::ShellFallback);
            assert_eq!(middle.panes[0].roots, [1234]);
        } else {
            assert!(middle.panes.is_empty());
            assert_eq!(middle.totals.process_count, 0);
        }
        let updated = &last.panes[0].target;
        assert_eq!(updated.pane_id, id);
        let agent = updated.agent.as_ref().unwrap();
        assert_eq!(agent.agent.as_deref(), Some("claude"));
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(agent.agent_status, "blocked");
        assert_eq!(agent.tokens["model"], "claude-model");
        assert_eq!(updated.pane.cwd.as_deref(), Some("/new-project"));
        assert!(agent.agent_session.is_none());
        assert_eq!(last.panes[0].roots, [20]);
        assert_eq!(last.agent_count, 1);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].roots, [10]);
        assert_eq!(calls[2].roots, [20]);
        assert_eq!(
            calls[1].roots,
            if mode == Mode::Focused {
                vec![1234]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn sampler_does_not_choose_a_target_from_snapshot_focus() {
    let (mut app, calls, metrics_calls) = monitor(
        vec![step(snapshot(vec![agent_pane("one")], "one"), vec![])],
        vec![metrics(vec![])],
        Mode::Focused,
        None,
    );
    assert!(app.sample().unwrap().panes.is_empty());
    assert!(calls.lock().unwrap().is_empty());
    assert!(metrics_calls.lock().unwrap()[0].roots.is_empty());
}

#[test]
fn explicit_target_changes_reuse_the_provider_and_clearing_target_drops_old_rows() {
    let current = snapshot(vec![agent_pane("one"), agent_pane("two")], "monitor");
    let (mut app, calls, metrics_calls) = monitor(
        vec![
            step(current.clone(), vec![process("one", &[10])]),
            step(current.clone(), vec![process("two", &[20])]),
            step(current, vec![]),
        ],
        vec![
            metrics(vec![metric(10, 1, &[], None, 100)]),
            metrics(vec![metric(20, 1, &[], Some(12.0), 200)]),
            metrics(vec![]),
        ],
        Mode::Focused,
        Some("one"),
    );
    assert_eq!(app.sample().unwrap().panes[0].target.pane_id, "one");
    app.set_target(Some("two".into()));
    let second = app.sample().unwrap();
    assert_eq!(second.panes[0].target.pane_id, "two");
    assert_eq!(second.totals.cpu_percent, Some(12.0));
    assert_eq!(second.totals.rss_bytes, Some(200));
    app.set_target(None);
    let third = app.sample().unwrap();
    assert!(third.panes.is_empty());
    assert_eq!(third.totals.cpu_percent, None);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["snapshot", "one", "snapshot", "two"]
    );
    assert_eq!(
        metrics_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.roots.clone())
            .collect::<Vec<_>>(),
        vec![vec![10], vec![20], vec![]]
    );
}

#[test]
fn summary_preserves_distinct_panes_but_counts_overlapping_process_trees_once() {
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one"), agent_pane("two")], "one"),
            vec![process("one", &[10]), process("two", &[20])],
        )],
        vec![metrics(vec![
            metric(10, 1, &[20], Some(10.0), 100),
            metric(20, 10, &[30], Some(20.0), 200),
            metric(30, 20, &[], Some(30.0), 300),
        ])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.agent_count, 2);
    assert_eq!(sample.panes.len(), 2);
    assert_eq!(sample.panes[0].stats.own.rss_bytes, Some(100));
    assert_eq!(sample.panes[0].stats.tree.rss_bytes, Some(600));
    assert_eq!(sample.panes[1].stats.tree.rss_bytes, Some(500));
    assert_eq!(sample.totals.rss_bytes, Some(600));
    assert_eq!(sample.totals.cpu_percent, Some(60.0));
    assert_eq!(sample.totals.process_count, 3);
    assert_eq!(calls.lock().unwrap()[0].roots, vec![10, 20]);
}

#[test]
fn summary_uses_recognized_agent_metadata_and_sorts_by_tree_cpu_unknown_last() {
    let mut shell = agent_pane("shell");
    shell.agent = None;
    shell.label = Some("Codex".into());
    shell.display_agent = Some("Codex".into());
    let mut agent_row_only = agent_pane("high");
    agent_row_only.agent = None;
    let mut state = snapshot(
        vec![agent_pane("cold"), shell, agent_pane("low"), agent_row_only],
        "shell",
    );
    state.agents.push(AgentInfo {
        pane_id: "high".into(),
        ..Default::default()
    });
    let (mut app, _, calls) = monitor(
        vec![step(
            state,
            vec![
                process("cold", &[10]),
                process("shell", &[99]),
                process("low", &[20]),
                process("high", &[30]),
            ],
        )],
        vec![metrics(vec![
            metric(10, 1, &[], None, 100),
            metric(20, 1, &[], Some(60.0), 200),
            metric(30, 1, &[31], Some(5.0), 300),
            metric(31, 30, &[], Some(80.0), 400),
        ])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.agent_count, 3);
    assert_eq!(
        sample
            .panes
            .iter()
            .map(|pane| pane.target.pane_id.as_str())
            .collect::<Vec<_>>(),
        vec!["high", "low", "cold"]
    );
    assert_eq!(sample.panes[0].stats.own.cpu_percent, Some(5.0));
    assert_eq!(sample.panes[0].stats.tree.cpu_percent, Some(85.0));
    assert_eq!(sample.totals.process_count, 4);
    assert_eq!(sample.totals.rss_bytes, Some(1000));
    assert_eq!(sample.totals.cpu_percent, None);
    assert_eq!(calls.lock().unwrap()[0].roots, vec![10, 20, 30]);
}

#[test]
fn summary_popup_without_pane_identity_still_includes_its_launch_context_agent() {
    let (mut app, _, calls) = monitor_with_context(
        vec![step(
            snapshot(vec![agent_pane("one"), agent_pane("two")], "one"),
            vec![process("one", &[10]), process("two", &[20])],
        )],
        vec![metrics(vec![
            metric(10, 1, &[], Some(10.0), 100),
            metric(20, 1, &[], Some(20.0), 200),
        ])],
        Mode::Summary,
        LaunchContext {
            monitor_pane_id: None,
            target_pane_id: Some("one".into()),
            monitor_pid: 999,
        },
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.agent_count, 2);
    assert_eq!(sample.panes.len(), 2);
    assert_eq!(sample.totals.cpu_percent, Some(30.0));
    assert_eq!(calls.lock().unwrap()[0].roots, vec![10, 20]);
    assert_eq!(calls.lock().unwrap()[0].excluded, vec![999]);
}

#[test]
fn summary_excludes_own_pane_and_other_monitor_executable_without_trusting_labels() {
    let mut other_monitor = process("other-monitor", &[900]);
    other_monitor.foreground_processes[0].name = "herdr-resource-monitor".into();
    other_monitor.foreground_processes[0].argv = Some(vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "focused".into(),
    ]);
    let mut normal = agent_pane("normal");
    normal.label = Some("herdr-resource-monitor".into());
    let (mut app, herdr_calls, calls) = monitor(
        vec![step(
            snapshot(
                vec![normal, agent_pane("monitor"), agent_pane("other-monitor")],
                "normal",
            ),
            vec![process("normal", &[10]), other_monitor],
        )],
        vec![metrics(vec![metric(10, 1, &[], Some(10.0), 100)])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.agent_count, 1);
    assert_eq!(sample.panes.len(), 1);
    assert_eq!(sample.panes[0].target.pane_id, "normal");
    assert_eq!(calls.lock().unwrap()[0].roots, vec![10]);
    assert_eq!(calls.lock().unwrap()[0].excluded, vec![999, 900]);
    assert_eq!(
        *herdr_calls.lock().unwrap(),
        vec!["snapshot", "normal", "other-monitor"]
    );
}

#[test]
fn summary_recognizes_monitor_from_argv0_when_full_argv_is_unavailable() {
    let mut other_monitor = process("other-monitor", &[900]);
    other_monitor.foreground_processes[0].name = "herdr-resource-monitor".into();
    other_monitor.foreground_processes[0].argv = None;
    other_monitor.foreground_processes[0].argv0 = Some(
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("other-monitor")], "other-monitor"),
            vec![other_monitor],
        )],
        vec![metrics(vec![])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert!(sample.panes.is_empty());
    assert_eq!(sample.agent_count, 0);
    assert!(calls.lock().unwrap()[0].roots.is_empty());
    assert_eq!(calls.lock().unwrap()[0].excluded, vec![999, 900]);
}

#[test]
fn same_executable_basename_does_not_hide_an_unrelated_process() {
    let directory = std::env::temp_dir().join(format!(
        "herdr-app-unrelated-monitor-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let basename = format!("herdr-resource-monitor{}", std::env::consts::EXE_SUFFIX);
    let unrelated = directory.join(&basename);
    std::fs::write(&unrelated, b"unrelated executable").unwrap();
    for argv0 in [basename, unrelated.to_string_lossy().into_owned()] {
        let mut foreground = process("normal", &[10]);
        foreground.foreground_processes[0].name = "herdr-resource-monitor".into();
        foreground.foreground_processes[0].argv = Some(vec![argv0, "focused".into()]);
        let (mut app, _, calls) = monitor(
            vec![step(
                snapshot(vec![agent_pane("normal")], "normal"),
                vec![foreground],
            )],
            vec![metrics(vec![metric(10, 1, &[], Some(10.0), 100)])],
            Mode::Summary,
            None,
        );
        let sample = app.sample().unwrap();
        assert_eq!(sample.agent_count, 1);
        assert_eq!(sample.panes[0].target.pane_id, "normal");
        assert_eq!(calls.lock().unwrap()[0].roots, [10]);
        assert_eq!(calls.lock().unwrap()[0].excluded, [999]);
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn monitor_relative_executable_is_resolved_against_foreground_cwd() {
    let executable = std::env::current_exe().unwrap();
    let mut foreground = process("other-monitor", &[900]);
    foreground.foreground_processes[0].cwd =
        Some(executable.parent().unwrap().to_string_lossy().into_owned());
    foreground.foreground_processes[0].argv = Some(vec![format!(
        ".{}{}",
        std::path::MAIN_SEPARATOR,
        executable.file_name().unwrap().to_string_lossy()
    )]);
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("other-monitor")], "other-monitor"),
            vec![foreground],
        )],
        vec![metrics(vec![])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert!(sample.panes.is_empty());
    assert_eq!(calls.lock().unwrap()[0].excluded, [999, 900]);
}

#[test]
fn process_info_failure_makes_totals_unknown_without_losing_known_agent_count() {
    let mut input = step(
        snapshot(vec![agent_pane("one"), agent_pane("unreadable")], "one"),
        vec![process("one", &[10])],
    );
    input.processes.insert(
        "unreadable".into(),
        Err(HerdrError::Api {
            code: "unavailable".into(),
            message: "process lookup failed".into(),
        }),
    );
    let (mut app, _, _) = monitor(
        vec![input],
        vec![metrics(vec![metric(10, 1, &[], Some(10.0), 100)])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.totals.cpu_percent, None);
    assert_eq!(sample.totals.rss_bytes, None);
    assert_eq!(sample.totals.process_count, 1);
    assert!(sample
        .errors
        .iter()
        .any(|error| error.contains("unreadable")));
    assert_eq!(
        sample.agent_count, 2,
        "agent metadata does not depend on process lookup"
    );
}

#[test]
fn pane_without_any_resolvable_pid_makes_summary_coverage_unknown() {
    let mut unavailable = process("unavailable", &[]);
    unavailable.shell_pid = None;
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one"), agent_pane("unavailable")], "one"),
            vec![process("one", &[10]), unavailable],
        )],
        vec![metrics(vec![metric(10, 1, &[], Some(10.0), 100)])],
        Mode::Summary,
        None,
    );
    let sample = app.sample().unwrap();
    assert_eq!(sample.panes[1].root_source, RootSource::NoProcess);
    assert_eq!(sample.panes[1].stats.tree.rss_bytes, None);
    assert_eq!(sample.totals.cpu_percent, None);
    assert_eq!(sample.totals.rss_bytes, None);
    assert_eq!(sample.agent_count, 2);
    assert_eq!(calls.lock().unwrap()[0].roots, vec![10]);
}

#[test]
fn closed_selected_pane_reports_error_instead_of_reusing_old_processes() {
    let (mut app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("another")], "another"),
            vec![],
        )],
        vec![metrics(vec![])],
        Mode::Focused,
        Some("gone"),
    );
    let sample = app.sample().unwrap();
    assert!(sample.panes.is_empty());
    assert_eq!(sample.totals.cpu_percent, None);
    assert!(sample.errors.iter().any(|error| error.contains("gone")));
    assert!(calls.lock().unwrap()[0].roots.is_empty());
}

/// Server state records in a private temporary codex home and state home.
struct ServerState(std::path::PathBuf);

impl ServerState {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("herdr-app-shared-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("codex/app-server-daemon")).unwrap();
        std::fs::create_dir_all(root.join("state/opencode")).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, contents: &str) -> &Self {
        std::fs::write(self.0.join(relative), contents).unwrap();
        self
    }

    fn files(&self) -> SharedServerFiles {
        SharedServerFiles::new(self.0.join("codex"), self.0.join("state"))
    }
}

impl Drop for ServerState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn named(metric: ProcessMetrics, name: &str, cmdline: &str) -> ProcessMetrics {
    ProcessMetrics {
        name: name.into(),
        cmdline: cmdline.into(),
        ..metric
    }
}

#[test]
fn summary_moves_a_shared_server_out_of_the_pane_that_started_it() {
    let state = ServerState::new("opencode");
    state.write(
        "state/opencode/service.json",
        r#"{"url":"http://127.0.0.1:1","pid":11,"password":"fixture-secret"}"#,
    );
    let (app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("starter"), agent_pane("client")], "starter"),
            vec![process("starter", &[10]), process("client", &[20])],
        )],
        vec![metrics(vec![
            named(metric(10, 1, &[11], Some(1.0), 100), "opencode", "opencode"),
            named(
                metric(11, 10, &[12], Some(2.0), 1000),
                "opencode",
                "/bin/opencode serve --service",
            ),
            metric(12, 11, &[], Some(3.0), 2000),
            named(metric(20, 1, &[], Some(4.0), 400), "opencode", "opencode"),
        ])],
        Mode::Summary,
        None,
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert_eq!(calls.lock().unwrap()[0].roots, [10, 11, 20]);
    let pane = |id: &str| {
        sample
            .panes
            .iter()
            .find(|pane| pane.target.pane_id == id)
            .unwrap()
    };
    // The client's work no longer inflates the pane that started the service.
    assert_eq!(pane("starter").stats.tree.rss_bytes, Some(100));
    assert_eq!(pane("starter").stats.tree.process_count, 1);
    assert_eq!(pane("client").stats.tree.rss_bytes, Some(400));
    assert_eq!(sample.agent_count, 2);
    assert_eq!(sample.servers.len(), 1);
    let server = &sample.servers[0];
    assert_eq!(server.kind, SharedServerKind::OpencodeService);
    assert_eq!(server.roots, [11]);
    assert_eq!(server.stats.tree.rss_bytes, Some(3000));
    assert_eq!(server.stats.tree.cpu_percent, Some(5.0));
    assert_eq!(server.stats.tree.process_count, 2);
    assert_eq!(server.stats.own.rss_bytes, Some(1000));
    assert_eq!(sample.totals.rss_bytes, Some(3500));
    assert_eq!(sample.totals.cpu_percent, Some(10.0));
    assert_eq!(sample.totals.process_count, 4);
    assert!(sample.errors.is_empty());
}

#[test]
fn summary_lists_the_codex_daemon_outside_panes_and_counts_it_once() {
    let state = ServerState::new("codex");
    state
        .write(
            "codex/app-server-daemon/daemon-updater.pid",
            r#"{"pid":40}"#,
        )
        .write(
            "codex/app-server-daemon/daemon.pid",
            r#"{"pid":41,"processIdentity":{"startSeconds":1}}"#,
        );
    let (app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one")], "one"),
            vec![process("one", &[10])],
        )],
        vec![metrics(vec![
            metric(10, 1, &[], Some(1.0), 100),
            named(
                metric(40, 1, &[41], Some(0.0), 10),
                "codex",
                "/bin/codex app-server daemon pid-update-loop",
            ),
            named(
                metric(41, 40, &[42], Some(2.0), 1000),
                "codex",
                "/bin/codex app-server --listen unix:// --managed-daemon",
            ),
            named(
                metric(42, 41, &[], Some(3.0), 500),
                "codex-code-mode-host",
                "/bin/codex-code-mode-host",
            ),
        ])],
        Mode::Summary,
        None,
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert_eq!(calls.lock().unwrap()[0].roots, [10, 40, 41]);
    let server = &sample.servers[0];
    assert_eq!(server.kind, SharedServerKind::CodexAppServer);
    assert_eq!(server.roots, [40], "the updater is the outermost root");
    assert_eq!(server.stats.tree.process_count, 3);
    assert_eq!(server.stats.tree.rss_bytes, Some(1510));
    assert_eq!(server.stats.own.rss_bytes, Some(10));
    assert_eq!(sample.panes[0].stats.tree.rss_bytes, Some(100));
    assert_eq!(sample.totals.rss_bytes, Some(1610));
    assert_eq!(sample.totals.process_count, 4);
}

#[test]
fn stale_or_reused_server_records_add_no_rows_and_keep_totals_complete() {
    let state = ServerState::new("stale");
    state
        .write("state/opencode/service.json", r#"{"pid":50}"#)
        .write("codex/app-server-daemon/daemon.pid", r#"{"pid":60}"#);
    let (app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one")], "one"),
            vec![process("one", &[10])],
        )],
        vec![metrics(vec![
            metric(10, 1, &[], Some(1.0), 100),
            named(metric(60, 1, &[], Some(9.0), 900), "zsh", "zsh app-server"),
        ])],
        Mode::Summary,
        None,
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert_eq!(calls.lock().unwrap()[0].roots, [10, 50, 60]);
    assert!(sample.servers.is_empty());
    assert!(sample.errors.is_empty());
    assert_eq!(sample.totals.rss_bytes, Some(100));
    assert_eq!(sample.totals.cpu_percent, Some(1.0));
    assert_eq!(sample.totals.process_count, 1);
}

#[test]
fn unusable_server_state_is_reported_and_makes_totals_unknown() {
    let state = ServerState::new("unusable");
    state.write("state/opencode/service.json", "not json");
    let (app, _, calls) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one")], "one"),
            vec![process("one", &[10])],
        )],
        vec![metrics(vec![metric(10, 1, &[], Some(1.0), 100)])],
        Mode::Summary,
        None,
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert_eq!(calls.lock().unwrap()[0].roots, [10]);
    assert_eq!(sample.servers.len(), 1);
    assert!(sample.servers[0].roots.is_empty());
    assert!(sample.servers[0].error.is_some());
    assert_eq!(sample.servers[0].stats.tree.rss_bytes, None);
    assert!(sample
        .errors
        .iter()
        .any(|error| error.starts_with("opencode service: unrecognized state")));
    assert_eq!(sample.totals.cpu_percent, None);
    assert_eq!(sample.totals.rss_bytes, None);
}

#[test]
fn focused_shows_the_target_agents_server_as_context_outside_pane_totals() {
    let state = ServerState::new("focused");
    state.write("state/opencode/service.json", r#"{"pid":11}"#);
    let mut pane = agent_pane("oc");
    pane.agent = Some("opencode".into());
    let mut tui = process("oc", &[10]);
    tui.foreground_processes[0].argv = Some(vec!["opencode".into()]);
    let (app, _, calls) = monitor(
        vec![step(snapshot(vec![pane], "oc"), vec![tui])],
        vec![metrics(vec![
            named(metric(10, 1, &[11], Some(1.0), 100), "opencode", "opencode"),
            named(
                metric(11, 10, &[12], Some(2.0), 1000),
                "opencode",
                "/bin/opencode serve --service",
            ),
            metric(12, 11, &[], Some(3.0), 2000),
        ])],
        Mode::Focused,
        Some("oc"),
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert_eq!(calls.lock().unwrap()[0].roots, [10, 11]);
    assert_eq!(sample.panes[0].stats.tree.rss_bytes, Some(100));
    assert_eq!(
        sample.totals.rss_bytes,
        Some(100),
        "context stays out of the pane totals"
    );
    assert_eq!(sample.totals.process_count, 1);
    assert_eq!(sample.servers.len(), 1);
    assert_eq!(sample.servers[0].kind, SharedServerKind::OpencodeService);
    assert_eq!(sample.servers[0].stats.tree.rss_bytes, Some(3000));
}

#[test]
fn focused_skips_opted_out_or_other_agents_and_keeps_totals_on_bad_server_state() {
    let state = ServerState::new("focused-skip");
    state
        .write("codex/app-server-daemon/daemon.pid", r#"{"pid":41}"#)
        .write("state/opencode/service.json", r#"{"pid":50}"#);
    for (agent, argv) in [
        ("codex", ["codex", "--no-daemon"]),
        ("opencode", ["opencode", "--server=http://127.0.0.1:1"]),
        ("claude", ["claude", "--continue"]),
    ] {
        let mut pane = agent_pane("one");
        pane.agent = Some(agent.into());
        let mut foreground = process("one", &[10]);
        foreground.foreground_processes[0].argv = Some(argv.map(str::to_string).to_vec());
        let (app, _, calls) = monitor(
            vec![step(snapshot(vec![pane], "one"), vec![foreground])],
            vec![metrics(vec![metric(10, 1, &[], Some(1.0), 100)])],
            Mode::Focused,
            Some("one"),
        );
        let sample = app.with_shared_servers(state.files()).sample().unwrap();
        assert_eq!(calls.lock().unwrap()[0].roots, [10], "{agent}");
        assert!(sample.servers.is_empty(), "{agent}");
    }

    state.write("codex/app-server-daemon/daemon.pid", "not json");
    let (app, _, _) = monitor(
        vec![step(
            snapshot(vec![agent_pane("one")], "one"),
            vec![process("one", &[10])],
        )],
        vec![metrics(vec![metric(10, 1, &[], Some(1.0), 100)])],
        Mode::Focused,
        Some("one"),
    );
    let sample = app.with_shared_servers(state.files()).sample().unwrap();
    assert!(sample.errors.is_empty());
    assert_eq!(sample.totals.rss_bytes, Some(100));
    assert!(sample.servers[0]
        .error
        .as_deref()
        .unwrap()
        .contains("unrecognized state"));
}
