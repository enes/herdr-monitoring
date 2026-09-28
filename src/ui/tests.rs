use std::collections::BTreeMap;
use std::time::Instant;

use herdr_resource_monitor::app::PaneSample;
use herdr_resource_monitor::herdr::{correlate, ForegroundProcess, PaneInfo, PaneProcessInfo};
use herdr_resource_monitor::metrics::{ProcessMetrics, RootMetrics};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;

fn sample(pane_id: &str) -> MonitorSample {
    let processes = [
        (10, 1, "node", vec![11], 12.5, 1_048_576),
        (11, 10, "tool", vec![], 5.0, 3_145_728),
    ]
    .into_iter()
    .map(|(pid, ppid, name, children, cpu_percent, rss_bytes)| {
        (
            pid,
            ProcessMetrics {
                pid,
                ppid,
                name: name.into(),
                cmdline: name.into(),
                children,
                cpu_percent: Some(cpu_percent),
                rss_bytes: Some(rss_bytes),
            },
        )
    })
    .collect();
    let metrics = MetricsSnapshot {
        sampled_at: Instant::now(),
        processes,
        missing_roots: vec![],
    };
    let pane = PaneInfo {
        pane_id: pane_id.into(),
        agent: Some("codex".into()),
        display_agent: Some("Codex".into()),
        agent_status: "working".into(),
        workspace_id: "work".into(),
        tab_id: "tab".into(),
        label: Some("example-project".into()),
        cwd: Some("/work/project".into()),
        foreground_cwd: Some("/work/project".into()),
        ..Default::default()
    };
    let process = PaneProcessInfo {
        pane_id: pane_id.into(),
        foreground_processes: vec![ForegroundProcess {
            pid: 10,
            name: "node".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    MonitorSample {
        capacity: SystemCapacity {
            logical_cpus: 8,
            memory_bytes: 32_000_000_000,
        },
        panes: vec![PaneSample {
            target: correlate(&pane, None, process).unwrap(),
            roots: vec![10],
            root_source: RootSource::Foreground,
            stats: metrics.totals(&[10]),
        }],
        servers: vec![],
        totals: metrics.totals(&[10]).tree,
        agent_count: 1,
        errors: vec![],
        metrics,
    }
}

fn selected_pane(state: &ViewState) -> Option<&str> {
    match &state.selection {
        Some(RowId::Pane(id)) => Some(id),
        _ => None,
    }
}

fn selected(state: &ViewState) -> Option<&PaneSample> {
    match state.selected_row()? {
        SummaryRow::Pane(pane) => Some(pane),
        SummaryRow::Server(_) => None,
    }
}

fn screen(mode: Mode, state: &ViewState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, mode, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width).max(1))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn tree_preserves_parentage_without_repeating_an_overlapping_root() {
    let processes = [
        (20, 0, vec![10, 30]),
        (10, 20, vec![40]),
        (30, 20, vec![]),
        (40, 10, vec![]),
    ]
    .into_iter()
    .map(|(pid, ppid, children)| {
        (
            pid,
            ProcessMetrics {
                pid,
                ppid,
                children,
                name: format!("proc{pid}"),
                cmdline: String::new(),
                cpu_percent: None,
                rss_bytes: Some(1_048_576),
            },
        )
    })
    .collect::<BTreeMap<_, _>>();
    let snapshot = MetricsSnapshot {
        sampled_at: Instant::now(),
        processes,
        missing_roots: vec![],
    };
    let lines: Vec<_> = tree_lines(
        &snapshot,
        &[10, 20, 20],
        SystemCapacity::default(),
        60,
        false,
    )
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(lines.len(), 5);
    assert!(lines[1].starts_with("proc20"));
    assert!(lines[1].contains("1.05 MB"));
    assert!(lines[2].starts_with("├─ proc10"));
    assert!(lines[3].starts_with("│  └─ proc40"));
    assert!(lines[4].starts_with("└─ proc30"));
    assert_eq!(percent(Some(0.0)), "0.00%");
    assert_eq!(memory(None), "—");
}

#[test]
fn both_modes_render_their_required_content() {
    for (mode, title, message) in [
        (
            Mode::Summary,
            "HERDR RESOURCE SUMMARY",
            "Collecting agent resources...",
        ),
        (Mode::Focused, "RESOURCE DETAILS", "No target pane"),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| render(frame, mode, &ViewState::default()))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in [title, message, "close"] {
            assert!(screen.contains(expected), "missing {expected:?}: {screen}");
        }
    }
}

#[test]
fn tiny_terminal_sizes_do_not_panic() {
    for mode in [Mode::Summary, Mode::Focused] {
        for (width, height) in [(0, 0), (1, 1), (2, 2), (12, 4)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render(frame, mode, &ViewState::default()))
                .unwrap();
        }
    }
}

#[test]
fn narrow_summary_preserves_identity_and_normalized_values() {
    let state = ViewState {
        sample: Some(sample("A")),
        ..Default::default()
    };
    let rendered = screen(Mode::Summary, &state, 28, 18);
    for expected in [
        "Total CPU",
        "2.19%",
        "Total memory",
        "4.19 MB",
        "0.01%",
        "example-project",
        "Codex · Working",
        "Processes 2",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}:\n{rendered}"
        );
    }
    assert!(!rendered.contains("17.5%"));
}

#[test]
fn focused_groups_totals_and_hides_technical_metadata_until_requested() {
    let mut state = ViewState {
        sample: Some(sample("A")),
        ..Default::default()
    };
    let rendered = screen(Mode::Focused, &state, 28, 26);
    for expected in [
        "RESOURCE DETAILS",
        "example-project",
        "Codex · Working",
        "RESOURCE USAGE",
        "Total CPU",
        "2.19%",
        "4.19 MB",
        "PROCESSES",
        "node",
        "└─ tool",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}:\n{rendered}"
        );
    }
    assert_eq!(rendered.matches("/work/project").count(), 1);
    for hidden in [
        "CPU own",
        "CPU tree",
        "one core",
        "shell_pid",
        "PID 10",
        "TECHNICAL DETAILS",
    ] {
        assert!(!rendered.contains(hidden), "{hidden}");
    }
    state.scroll = (10, 8);
    state.toggle_details();
    assert!(state.details);
    assert_eq!(state.scroll, (0, 0));
    let details = sample_lines(
        Mode::Focused,
        state.sample.as_ref().unwrap(),
        80,
        state.details,
    )
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    for shown in [
        "TECHNICAL DETAILS",
        "Pane: A",
        "PID 10",
        "shell_pid",
        "foreground_cwd",
    ] {
        assert!(details.contains(shown), "{shown}");
    }
    state.toggle_details();
    assert!(!state.details);
}

#[test]
fn no_recognized_agents_has_an_explicit_empty_state() {
    let mut empty = sample("A");
    empty.panes.clear();
    empty.agent_count = 0;
    let state = ViewState {
        sample: Some(empty),
        ..Default::default()
    };
    assert!(screen(Mode::Summary, &state, 28, 12).contains("No recognized agents"));
}

#[test]
fn summary_separates_terminal_agent_and_status_and_keeps_duplicate_panes() {
    let mut first = sample("A");
    let mut second = sample("B");
    first.panes[0].target.agent.as_mut().unwrap().name = Some("codex-api".into());
    second.panes[0].target.agent.as_mut().unwrap().name = Some("codex-tests".into());
    second.panes[0].target.agent.as_mut().unwrap().agent_status = "idle".into();
    first.panes.push(second.panes.remove(0));
    first.agent_count = 2;
    let state = ViewState {
        sample: Some(first),
        ..Default::default()
    };
    let rendered = screen(Mode::Summary, &state, 100, 18);
    for expected in [
        "TERMINAL",
        "AGENT",
        "STATUS",
        "CPU",
        "MEMORY",
        "[A] example-project",
        "[B] example-project",
        "codex-api",
        "codex-tests",
        "Working",
        "Waiting",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}:\n{rendered}"
        );
    }
}

#[test]
fn target_changes_clear_values_reset_scroll_and_reject_late_samples() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Target {
        generation: 1,
        pane_id: Some("A".into()),
    });
    state.apply(MonitorUpdate::Sample {
        generation: 1,
        result: Ok(sample("A")),
    });
    state.scroll = (10, 8);
    state.apply(MonitorUpdate::Target {
        generation: 2,
        pane_id: Some("B".into()),
    });
    assert!(state.sample.is_none());
    assert_eq!(state.scroll, (0, 0));
    assert!(screen(Mode::Focused, &state, 40, 9).contains("Loading B..."));
    state.apply(MonitorUpdate::Sample {
        generation: 1,
        result: Ok(sample("A")),
    });
    assert!(state.sample.is_none());
    state.apply(MonitorUpdate::Sample {
        generation: 2,
        result: Ok(sample("B")),
    });
    state.apply(MonitorUpdate::Sample {
        generation: 1,
        result: Err("old failure".into()),
    });
    assert_eq!(state.sample.as_ref().unwrap().panes[0].target.pane_id, "B");
    assert!(state.sample_error.is_none());
    state.apply(MonitorUpdate::Target {
        generation: 1,
        pane_id: Some("A".into()),
    });
    assert_eq!(state.target.as_deref(), Some("B"));
}

#[test]
fn connection_status_survives_samples_and_reconnect_clears_stale_values() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Target {
        generation: 1,
        pane_id: Some("A".into()),
    });
    state.apply(MonitorUpdate::Status(Some("Events disconnected".into())));
    state.apply(MonitorUpdate::Sample {
        generation: 1,
        result: Ok(sample("A")),
    });
    let rendered = screen(Mode::Focused, &state, 42, 20);
    assert!(rendered.contains("Events disconnected"));
    assert!(rendered.contains("2.19%"));
    state.apply(MonitorUpdate::Target {
        generation: 2,
        pane_id: None,
    });
    state.apply(MonitorUpdate::Sample {
        generation: 1,
        result: Ok(sample("A")),
    });
    assert!(state.sample.is_none());
    assert!(screen(Mode::Focused, &state, 42, 9).contains("No target pane"));
    state.apply(MonitorUpdate::Status(None));
    assert!(state.status.is_none());
}

#[test]
fn current_sample_error_does_not_leave_old_metrics_visible() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(sample("A")),
    });
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Err("process lookup failed".into()),
    });
    let rendered = screen(Mode::Summary, &state, 50, 9);
    assert!(rendered.contains("Unavailable: process lookup failed"));
    assert!(!rendered.contains("17.5%"));
}

#[test]
fn close_keys_work_but_unrelated_keys_and_releases_do_not_close() {
    for key in [
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        assert!(should_close(key));
        assert!(!should_close(KeyEvent {
            kind: KeyEventKind::Release,
            ..key
        }));
    }
    assert!(!should_close(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::NONE
    )));
    assert!(!should_close(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE
    )));
}

#[test]
fn percentages_units_and_bars_share_machine_capacity() {
    let capacity = SystemCapacity {
        logical_cpus: 8,
        memory_bytes: 32_000_000_000,
    };
    assert_eq!(percent(capacity.cpu_percent(Some(182.0))), "22.75%");
    assert_eq!(percent(capacity.cpu_percent(Some(98.72))), "12.34%");
    assert_eq!(
        percent(capacity.memory_percent(Some(2_100_000_000))),
        "6.56%"
    );
    assert_eq!(memory(Some(2_100_000_000)), "2.10 GB");
    assert_eq!(memory(Some(420_000_000)), "420.00 MB");
    assert_eq!(percent(capacity.cpu_percent(None)), "—");
    assert_eq!(
        percent(SystemCapacity::default().cpu_percent(Some(100.0))),
        "—"
    );
    assert_eq!(
        percent(SystemCapacity::default().memory_percent(Some(1))),
        "—"
    );
    assert_eq!(capacity.cpu_percent(Some(f64::NAN)), None);
    assert_eq!(capacity.cpu_percent(Some(-1.0)), None);
    for (value, color, filled) in [
        (0.0, Color::Green, 0),
        (25.0, Color::Green, 5),
        (50.0, Color::Yellow, 10),
        (79.0, Color::Yellow, 16),
        (80.0, Color::Red, 16),
        (120.0, Color::Red, 20),
    ] {
        let line = bar(Some(value), 20);
        assert_eq!(line.style.fg, Some(color));
        assert_eq!(line.width(), 20);
        assert_eq!(line.to_string().matches('█').count(), filled);
    }
    assert_eq!(bar(None, 20).style.fg, Some(Color::DarkGray));
    assert!(!bar(None, 20).to_string().contains('█'));
}

#[test]
fn summary_and_focused_totals_use_the_same_scale_as_process_rows() {
    let data = sample("A");
    let a = sample_lines(Mode::Summary, &data, 90, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let b = sample_lines(Mode::Focused, &data, 90, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for text in [&a, &b] {
        assert!(text.contains("2.19%")); // (12.5 + 5) / 8 cores
        assert!(text.contains("4.19 MB"));
        assert!(text.contains("0.01%"));
    }
    assert!(b.contains("1.56%")); // own node only
    assert!(b.contains("0.62%")); // own child only
}

#[test]
fn titles_optional_metadata_and_folder_are_not_guessed_or_repeated() {
    let mut data = sample("A");
    let pane = &mut data.panes[0];
    pane.target.pane.label = None;
    pane.target.pane.extra.insert(
        "terminal_title_stripped".into(),
        serde_json::json!("api-terminal"),
    );
    pane.target.agent.as_mut().unwrap().name = Some("codex-review".into());
    pane.target
        .agent
        .as_mut()
        .unwrap()
        .tokens
        .insert("fixture_model".into(), "not-an-api-field".into());
    assert_eq!(terminal_name(pane), "api-terminal");
    assert_eq!(agent_name(pane), "codex-review");
    assert_eq!(metadata(pane, &["model"]), None);
    pane.target
        .agent
        .as_mut()
        .unwrap()
        .tokens
        .insert("model".into(), "reported-model".into());
    pane.target
        .agent
        .as_mut()
        .unwrap()
        .tokens
        .insert("reasoning_effort".into(), "xhigh".into());
    pane.target.pane.label = Some("User title".into());
    assert_eq!(terminal_name(pane), "User title");
    let view = sample_lines(Mode::Focused, &data, 80, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(view.contains("Model: reported-model"));
    assert!(view.contains("Reasoning: Extra high"));
    assert_eq!(view.matches("/work/project").count(), 1);
    assert!(!view.contains("fixture_model"));
}

#[test]
fn historical_agent_metadata_is_rendered_as_shell_with_raw_details_preserved() {
    let mut data = sample("shell");
    let pane = &mut data.panes[0];
    pane.target.pane.agent = None;
    pane.target.pane.agent_status = "idle".into();
    pane.target.pane.agent_session = Some(herdr_resource_monitor::herdr::AgentSessionInfo {
        source: "fixture".into(),
        agent: "codex".into(),
        kind: "id".into(),
        value: "historical-session".into(),
    });
    pane.target = correlate(&pane.target.pane, None, pane.target.process.clone()).unwrap();
    assert_eq!(agent_name(pane), "Shell");
    let normal = sample_lines(Mode::Focused, &data, 80, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(normal.contains("Shell · Waiting"));
    assert!(!normal.contains("Codex"));
    let technical = sample_lines(Mode::Focused, &data, 80, true)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(technical.contains("historical-session"));
}

#[test]
fn long_unicode_names_keep_table_values_aligned_and_all_sizes_render() {
    assert_eq!(
        text_width(&column("Ｗｉｄｅ 🚀 long terminal title", 12, false)),
        12
    );
    assert_eq!(text_width(&column("12.34%", 9, true)), 9);
    let mut data = sample("A");
    data.panes[0].target.pane.label = Some("Ｗｉｄｅ 🚀 long terminal title".repeat(4));
    let state = ViewState {
        sample: Some(data),
        ..Default::default()
    };
    for width in [0, 1, 12, 28, 42, 76, 100] {
        for mode in [Mode::Summary, Mode::Focused] {
            screen(mode, &state, width, 30);
        }
    }
    let rendered = screen(Mode::Summary, &state, 100, 18);
    let row = rendered
        .lines()
        .find(|line| line.contains("Codex"))
        .unwrap();
    assert!(row.contains("2.19%"));
    assert!(row.contains("4.19 MB"));
}

fn summary_sample(rows: &[(&str, &str, Option<f64>, Option<u64>)]) -> MonitorSample {
    let mut data = sample("unused");
    data.panes = rows
        .iter()
        .map(|(id, name, cpu, memory)| {
            let mut pane = sample(id).panes.remove(0);
            pane.target.pane.label = Some((*name).into());
            pane.stats.tree.cpu_percent = *cpu;
            pane.stats.tree.rss_bytes = *memory;
            pane
        })
        .collect();
    data.agent_count = data.panes.len();
    data
}

#[test]
fn summary_sort_modes_order_values_and_keep_unknowns_last_with_stable_ties() {
    let data = summary_sample(&[
        ("D", "zebra", None, None),
        ("C", "Beta", Some(70.0), Some(400)),
        ("B", "alpha", Some(30.0), Some(900)),
        ("A", "ALPHA", Some(30.0), Some(900)),
        ("E", "epsilon", Some(f64::NAN), Some(1)),
    ]);
    let ids = |sort| {
        sorted_panes(&data, sort)
            .iter()
            .map(|pane| pane.target.pane_id.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(SummarySort::Cpu), ["C", "A", "B", "D", "E"]);
    assert_eq!(ids(SummarySort::Memory), ["A", "B", "C", "E", "D"]);
    assert_eq!(ids(SummarySort::Name), ["A", "B", "C", "E", "D"]);
    assert_eq!(SummarySort::Cpu.next().next().next(), SummarySort::Cpu);
}

#[test]
fn summary_selection_tracks_pane_identity_across_sort_and_refresh() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(summary_sample(&[
            ("A", "zebra", Some(80.0), Some(100)),
            ("B", "alpha", Some(40.0), Some(900)),
            ("C", "middle", Some(20.0), Some(500)),
        ])),
    });
    assert_eq!(selected_pane(&state), Some("A"));
    state.handle_key(Mode::Summary, KeyCode::Down, 100, 20);
    assert_eq!(selected_pane(&state), Some("B"));
    for sort in [SummarySort::Memory, SummarySort::Name, SummarySort::Cpu] {
        state.handle_key(Mode::Summary, KeyCode::Char('s'), 100, 20);
        assert_eq!(state.summary_sort, sort);
        assert_eq!(selected_pane(&state), Some("B"));
    }
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 20);
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(summary_sample(&[
            ("C", "middle", Some(100.0), Some(500)),
            ("B", "alpha-renamed", Some(90.0), Some(901)),
            ("A", "zebra", Some(1.0), Some(100)),
        ])),
    });
    assert_eq!(selected_pane(&state), Some("B"));
    assert!(state.summary_detail);
    assert_eq!(terminal_name(selected(&state).unwrap()), "alpha-renamed");
    assert_eq!(selected(&state).unwrap().stats.tree.rss_bytes, Some(901));
}

#[test]
fn disappearing_summary_selection_returns_to_list_and_empty_details_are_safe() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(summary_sample(&[
            ("A", "alpha", Some(80.0), Some(100)),
            ("B", "beta", Some(40.0), Some(900)),
        ])),
    });
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 80, 20);
    state.scroll = (10, 8);
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(summary_sample(&[("B", "beta", Some(40.0), Some(900))])),
    });
    assert_eq!(selected_pane(&state), Some("B"));
    assert!(!state.summary_detail);
    assert_eq!(state.scroll, (0, 0));
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(summary_sample(&[])),
    });
    for key in [
        KeyCode::Char('d'),
        KeyCode::Down,
        KeyCode::PageUp,
        KeyCode::Home,
    ] {
        state.handle_key(Mode::Summary, key, 28, 12);
    }
    assert!(state.selection.is_none());
    assert!(!state.summary_detail);
    assert!(screen(Mode::Summary, &state, 28, 12).contains("No recognized agents"));
}

#[test]
fn summary_details_show_only_selected_pane_and_arrows_scroll_without_reselecting() {
    let mut data = summary_sample(&[
        ("A", "selected-terminal", Some(80.0), Some(100)),
        ("B", "other-terminal", Some(40.0), Some(900)),
    ]);
    data.panes[1].target.agent.as_mut().unwrap().name = Some("other-agent-secret".into());
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(data),
    });
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 100);
    let rendered = screen(Mode::Summary, &state, 100, 250);
    for expected in [
        "AGENT RESOURCE DETAILS",
        "selected-terminal",
        "PROCESSES",
        "└─ tool",
        "TECHNICAL DETAILS",
        "Pane: A",
        "shell_pid",
    ] {
        assert!(rendered.contains(expected), "missing {expected}");
    }
    assert!(!rendered.contains("other-terminal"));
    assert!(!rendered.contains("other-agent-secret"));
    assert!(!rendered.contains("Pane: B"));
    state.handle_key(Mode::Summary, KeyCode::Down, 100, 20);
    state.handle_key(Mode::Summary, KeyCode::Right, 100, 20);
    assert_eq!(selected_pane(&state), Some("A"));
    assert_eq!(state.scroll, (1, 8));
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 20);
    assert!(!state.summary_detail);
    assert_eq!(state.scroll, (0, 0));
    state.handle_key(Mode::Summary, KeyCode::Down, 100, 20);
    assert_eq!(selected_pane(&state), Some("B"));
}

#[test]
fn selected_summary_rows_scroll_into_view_while_totals_stay_pinned() {
    let names: Vec<_> = (0..20).map(|i| format!("terminal-{i:02}")).collect();
    let rows: Vec<_> = names
        .iter()
        .map(|name| (name.as_str(), name.as_str(), Some(1.0), Some(100)))
        .collect();
    for (width, height) in [(100, 16), (28, 18)] {
        let mut state = ViewState::default();
        state.apply(MonitorUpdate::Sample {
            generation: 0,
            result: Ok(summary_sample(&rows)),
        });
        let initial = screen(Mode::Summary, &state, width, height);
        for _ in 0..19 {
            state.handle_key(Mode::Summary, KeyCode::Down, width, height);
        }
        let last = screen(Mode::Summary, &state, width, height);
        assert_eq!(selected_pane(&state), Some("terminal-19"));
        assert!(last.contains("> terminal-19"), "{last}");
        assert!(!last.contains("terminal-00"));
        assert!(last.contains("Total CPU"));
        assert!(last.contains("Total memory"));
        assert!(last.contains("Sort: CPU ↓"));
        let header = |text: &str| text.lines().take(7).collect::<Vec<_>>().join("\n");
        assert_eq!(header(&initial), header(&last));
        state.handle_key(Mode::Summary, KeyCode::Home, width, height);
        assert_eq!(selected_pane(&state), Some("terminal-00"));
        assert!(screen(Mode::Summary, &state, width, height).contains("> terminal-00"));
        state.handle_key(Mode::Summary, KeyCode::PageDown, width, height);
        assert_ne!(selected_pane(&state), Some("terminal-00"));
        state.handle_key(Mode::Summary, KeyCode::PageUp, width, height);
        assert_eq!(selected_pane(&state), Some("terminal-00"));
        for (w, h) in [(0, 0), (1, 1), (2, 2), (12, 4)] {
            screen(Mode::Summary, &state, w, h);
        }
    }
}

#[test]
fn summary_warns_about_unavailable_data_and_details_keep_errors_pane_specific() {
    let mut data = summary_sample(&[
        ("A", "alpha", Some(80.0), Some(100)),
        ("B", "beta", None, None),
    ]);
    data.errors = vec![
        "A: process lookup failed".into(),
        "B: other process unavailable".into(),
    ];
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(data),
    });
    let list = screen(Mode::Summary, &state, 100, 20);
    assert!(list.contains("Unavailable data: 2"));
    assert!(!list.contains("A: process lookup failed"));
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 250);
    let detail = screen(Mode::Summary, &state, 100, 250);
    assert!(detail.contains("A: process lookup failed"));
    assert!(!detail.contains("B: other process unavailable"));

    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Err("snapshot temporarily unavailable".into()),
    });
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 20);
    assert!(!state.summary_detail);
    assert!(screen(Mode::Summary, &state, 100, 20).contains("snapshot temporarily unavailable"));
}

fn server_sample() -> MonitorSample {
    let mut data = sample("A");
    for (pid, ppid, name, cmdline, children, cpu, rss) in [
        (40, 1, "codex", "codex app-server", vec![41], 8.0, 2_000_000),
        (
            41,
            40,
            "codex-code-mode-host",
            "code-mode-host",
            vec![],
            0.0,
            1_000_000,
        ),
    ] {
        data.metrics.processes.insert(
            pid,
            ProcessMetrics {
                pid,
                ppid,
                name: name.into(),
                cmdline: cmdline.into(),
                children,
                cpu_percent: Some(cpu),
                rss_bytes: Some(rss),
            },
        );
    }
    data.servers.push(SharedServerSample {
        kind: SharedServerKind::CodexAppServer,
        roots: vec![40],
        stats: data.metrics.totals(&[40]),
        error: None,
    });
    data
}

#[test]
fn shared_servers_follow_agent_rows_and_open_their_own_process_tree() {
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(server_sample()),
    });
    let list = screen(Mode::Summary, &state, 100, 24);
    let lines: Vec<_> = list.lines().collect();
    let position = |text: &str| {
        lines
            .iter()
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("missing {text}:\n{list}"))
    };
    assert!(position("example-project") < position("SHARED SERVERS"));
    assert!(position("SHARED SERVERS") < position("Codex app-server"));
    let row = lines[position("Codex app-server")];
    for expected in ["codex", "Shared", "1.00%", "3.00 MB"] {
        assert!(row.contains(expected), "missing {expected}: {row}");
    }
    assert!(!row.contains("> Codex"), "the first agent stays selected");

    state.handle_key(Mode::Summary, KeyCode::Down, 100, 24);
    assert_eq!(
        state.selection,
        Some(RowId::Server(SharedServerKind::CodexAppServer))
    );
    assert!(screen(Mode::Summary, &state, 100, 24).contains("> Codex app-server"));
    // Selection keeps the server through refreshes and sort changes.
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(server_sample()),
    });
    state.handle_key(Mode::Summary, KeyCode::Char('s'), 100, 24);
    assert_eq!(
        state.selection,
        Some(RowId::Server(SharedServerKind::CodexAppServer))
    );

    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 24);
    let detail = screen(Mode::Summary, &state, 100, 40);
    for expected in [
        "SHARED SERVER DETAILS",
        "Codex app-server",
        "codex · Shared",
        "not part of any pane",
        "Total memory",
        "3.00 MB",
        "└─ codex-code-mode-host",
        "PID 40 · codex app-server",
        "Main process: CPU 1.00% · Memory 2.00 MB",
    ] {
        assert!(detail.contains(expected), "missing {expected}:\n{detail}");
    }
    assert!(!detail.contains("example-project"));
    assert!(!detail.contains("Pane: A"));
    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 24);
    assert!(!state.summary_detail);

    let narrow = screen(Mode::Summary, &state, 28, 30);
    for expected in ["SHARED SERVERS", "Codex app-server", "codex · Shared"] {
        assert!(narrow.contains(expected), "missing {expected}:\n{narrow}");
    }
    for (width, height) in [(0, 0), (1, 1), (2, 2), (12, 4), (42, 12)] {
        screen(Mode::Summary, &state, width, height);
    }
}

#[test]
fn unusable_server_state_stays_unknown_and_reports_only_in_its_details() {
    let mut data = sample("A");
    data.panes.clear();
    data.agent_count = 0;
    data.servers.push(SharedServerSample {
        kind: SharedServerKind::OpencodeService,
        roots: vec![],
        stats: RootMetrics::default(),
        error: Some("unrecognized state in /state/opencode/service.json".into()),
    });
    data.errors = vec![
        "opencode service: unrecognized state in /state/opencode/service.json".into(),
        "A: process lookup failed".into(),
    ];
    let mut state = ViewState::default();
    state.apply(MonitorUpdate::Sample {
        generation: 0,
        result: Ok(data),
    });
    let list = screen(Mode::Summary, &state, 100, 20);
    for expected in [
        "No recognized agents",
        "SHARED SERVERS",
        "> opencode service",
        "Unavailable data: 2",
    ] {
        assert!(list.contains(expected), "missing {expected}:\n{list}");
    }
    let row = list
        .lines()
        .find(|line| line.contains("opencode service"))
        .unwrap();
    assert_eq!(row.matches('—').count(), 2, "{row}");
    assert!(!list.contains("unrecognized state"));

    state.handle_key(Mode::Summary, KeyCode::Char('d'), 100, 40);
    let detail = screen(Mode::Summary, &state, 100, 40);
    for expected in [
        "SHARED SERVER DETAILS",
        "No process to measure.",
        "Unavailable: unrecognized state in /state/opencode/service.json",
    ] {
        assert!(detail.contains(expected), "missing {expected}:\n{detail}");
    }
    assert!(!detail.contains("A: process lookup failed"));
}

#[test]
fn focused_shows_the_target_agents_shared_server_below_its_own_tree() {
    let data = server_sample();
    let text = |technical| {
        sample_lines(Mode::Focused, &data, 60, technical)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let view = text(false);
    let position = |text: &str| {
        view.find(text)
            .unwrap_or_else(|| panic!("missing {text}:\n{view}"))
    };
    for expected in [
        "Shared by every Codex session using it.",
        "Older Codex and --no-daemon run in the pane.",
        "Not included in the totals above.",
        "Server memory",
        "3.00 MB",
        "└─ codex-code-mode-host",
    ] {
        position(expected);
    }
    // The pane's own tree and totals come first and exclude the server.
    assert!(position("└─ tool") < position("SHARED SERVER"));
    assert!(position("4.19 MB") < position("SHARED SERVER"));
    assert!(!view.contains("PID 40"));
    let technical = text(true);
    assert!(technical.contains("PID 40 · codex app-server"));
    assert!(
        technical.find("SHARED SERVER").unwrap() < technical.find("TECHNICAL DETAILS").unwrap()
    );

    let mut unusable = sample("A");
    unusable.servers.push(SharedServerSample {
        kind: SharedServerKind::OpencodeService,
        roots: vec![],
        stats: RootMetrics::default(),
        error: Some("unrecognized state".into()),
    });
    let view = sample_lines(Mode::Focused, &unusable, 60, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "opencode service",
        "--standalone runs a private server in the pane.",
        "Unavailable: unrecognized state",
        "4.19 MB",
    ] {
        assert!(view.contains(expected), "missing {expected}:\n{view}");
    }
}
