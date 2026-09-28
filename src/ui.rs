use std::collections::BTreeSet;
use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use herdr_resource_monitor::app::{
    MonitorSample, MonitorUpdate, MonitorWorker, PaneSample, RootSource, SharedServerKind,
    SharedServerSample,
};
use herdr_resource_monitor::metrics::{MetricTotals, MetricsSnapshot, SystemCapacity};

mod presentation;
use presentation::*;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::cli::Mode;

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SummarySort {
    #[default]
    Cpu,
    Memory,
    Name,
}

impl SummarySort {
    fn next(self) -> Self {
        match self {
            Self::Cpu => Self::Memory,
            Self::Memory => Self::Name,
            Self::Name => Self::Cpu,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU ↓",
            Self::Memory => "Memory ↓",
            Self::Name => "Name A–Z",
        }
    }
}

fn sorted_panes(sample: &MonitorSample, sort: SummarySort) -> Vec<&PaneSample> {
    let mut panes: Vec<_> = sample.panes.iter().collect();
    panes.sort_by(|a, b| {
        let ordering = match sort {
            SummarySort::Cpu => {
                let valid = |pane: &PaneSample| {
                    pane.stats
                        .tree
                        .cpu_percent
                        .filter(|value| value.is_finite() && *value >= 0.0)
                };
                valid(b)
                    .partial_cmp(&valid(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
            SummarySort::Memory => b.stats.tree.rss_bytes.cmp(&a.stats.tree.rss_bytes),
            SummarySort::Name => terminal_name(a)
                .to_lowercase()
                .cmp(&terminal_name(b).to_lowercase()),
        };
        ordering.then_with(|| a.target.pane_id.cmp(&b.target.pane_id))
    });
    panes
}

/// One selectable summary row. Shared servers follow every agent pane.
#[derive(Clone, Copy)]
enum SummaryRow<'a> {
    Pane(&'a PaneSample),
    Server(&'a SharedServerSample),
}

/// Selection identity that survives refreshes and sort changes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowId {
    Pane(String),
    Server(SharedServerKind),
}

impl SummaryRow<'_> {
    fn id(self) -> RowId {
        match self {
            Self::Pane(pane) => RowId::Pane(pane.target.pane_id.clone()),
            Self::Server(server) => RowId::Server(server.kind),
        }
    }
}

fn summary_rows(sample: &MonitorSample, sort: SummarySort) -> Vec<SummaryRow<'_>> {
    sorted_panes(sample, sort)
        .into_iter()
        .map(SummaryRow::Pane)
        .chain(sample.servers.iter().map(SummaryRow::Server))
        .collect()
}

#[derive(Default)]
struct ViewState {
    generation: u64,
    target: Option<String>,
    sample: Option<MonitorSample>,
    sample_error: Option<String>,
    status: Option<String>,
    scroll: (u16, u16),
    details: bool,
    summary_sort: SummarySort,
    selection: Option<RowId>,
    summary_detail: bool,
}

impl ViewState {
    fn toggle_details(&mut self) {
        self.details = !self.details;
        self.scroll = (0, 0);
    }

    fn selected_row(&self) -> Option<SummaryRow<'_>> {
        let sample = self.sample.as_ref()?;
        let rows = summary_rows(sample, self.summary_sort);
        self.selection
            .as_ref()
            .and_then(|id| rows.iter().copied().find(|row| row.id() == *id))
            .or_else(|| rows.first().copied())
    }

    fn reconcile_selection(&mut self) {
        let selected = self.selected_row().map(SummaryRow::id);
        if selected != self.selection {
            self.selection = selected;
            self.summary_detail = false;
            self.scroll = (0, 0);
        }
    }

    fn move_selection(&mut self, offset: isize) {
        let Some(sample) = self.sample.as_ref() else {
            return;
        };
        let rows = summary_rows(sample, self.summary_sort);
        if rows.is_empty() {
            return;
        }
        let index = rows
            .iter()
            .position(|row| self.selection.as_ref() == Some(&row.id()))
            .unwrap_or(0);
        let next = index.saturating_add_signed(offset).min(rows.len() - 1);
        self.selection = Some(rows[next].id());
    }

    fn handle_key(&mut self, mode: Mode, key: KeyCode, width: u16, height: u16) {
        if mode == Mode::Summary {
            if key == KeyCode::Char('d') {
                // Returning to the list must also work while a failed sample
                // temporarily leaves the detail view without pane data.
                if self.summary_detail {
                    self.summary_detail = false;
                    self.scroll = (0, 0);
                    return;
                }
                if self.selected_row().is_some() {
                    self.reconcile_selection();
                    self.summary_detail = true;
                    self.scroll = (0, 0);
                }
                return;
            }
            if !self.summary_detail {
                let page = self.sample.as_ref().map_or(1, |sample| {
                    summary_page_size(sample, width, height, self.summary_sort)
                });
                match key {
                    KeyCode::Down => self.move_selection(1),
                    KeyCode::Up => self.move_selection(-1),
                    KeyCode::PageDown => self.move_selection(page as isize),
                    KeyCode::PageUp => self.move_selection(-(page as isize)),
                    KeyCode::Home => self.move_selection(isize::MIN),
                    KeyCode::Char('s') => {
                        self.reconcile_selection();
                        self.summary_sort = self.summary_sort.next();
                    }
                    _ => {}
                }
                return;
            }
        }
        match key {
            KeyCode::Down => self.scroll.0 = self.scroll.0.saturating_add(1),
            KeyCode::Up => self.scroll.0 = self.scroll.0.saturating_sub(1),
            KeyCode::PageDown => self.scroll.0 = self.scroll.0.saturating_add(10),
            KeyCode::PageUp => self.scroll.0 = self.scroll.0.saturating_sub(10),
            KeyCode::Right => self.scroll.1 = self.scroll.1.saturating_add(8),
            KeyCode::Left => self.scroll.1 = self.scroll.1.saturating_sub(8),
            KeyCode::Home => self.scroll = (0, 0),
            KeyCode::Char('d') => self.toggle_details(),
            _ => {}
        }
    }

    fn apply(&mut self, update: MonitorUpdate) {
        match update {
            MonitorUpdate::Target {
                generation,
                pane_id,
            } => {
                if generation < self.generation {
                    return;
                }
                if generation != self.generation || pane_id != self.target {
                    self.generation = generation;
                    self.target = pane_id;
                    self.sample = None;
                    self.sample_error = None;
                    self.scroll = (0, 0);
                }
            }
            MonitorUpdate::Sample { generation, result } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(sample) => {
                        self.sample = Some(sample);
                        self.sample_error = None;
                        self.reconcile_selection();
                    }
                    Err(error) => {
                        self.sample = None;
                        self.sample_error = Some(error);
                    }
                }
            }
            MonitorUpdate::Status(status) => self.status = status,
        }
    }
}

pub fn run(mode: Mode, worker: MonitorWorker) -> io::Result<()> {
    // Also restore the terminal if initialization, drawing, or input reading fails.
    // Ratatui installs its own restoration hook for panics.
    let _restore = TerminalRestore;
    let mut terminal = ratatui::try_init()?;
    let mut state = ViewState::default();
    let mut dirty = true;
    let mut disconnected = false;
    let mut viewport = (0, 0);

    loop {
        loop {
            match worker.updates.try_recv() {
                Ok(update) => {
                    state.apply(update);
                    dirty = true;
                }
                Err(TryRecvError::Disconnected) if !disconnected => {
                    state.status = Some("Sampling worker stopped".into());
                    state.sample = None;
                    disconnected = true;
                    dirty = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if dirty {
            terminal.draw(|frame| {
                viewport = (frame.area().width, frame.area().height);
                render(frame, mode, &state);
            })?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if should_close(key) => return Ok(()),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                state.handle_key(mode, key.code, viewport.0, viewport.1);
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}

fn should_close(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn render(frame: &mut Frame, mode: Mode, state: &ViewState) {
    let title = match mode {
        Mode::Summary if state.summary_detail => match state.selected_row() {
            Some(SummaryRow::Server(_)) => "SHARED SERVER DETAILS",
            _ => "AGENT RESOURCE DETAILS",
        },
        Mode::Summary => "HERDR RESOURCE SUMMARY",
        Mode::Focused => "RESOURCE DETAILS",
    };
    // Tiny panes keep every row; larger panes only add horizontal breathing room.
    let margin = u16::from(frame.area().width >= 30);
    let area = frame.area().inner(Margin::new(margin, 0));
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(title.bold().fg(Color::Cyan)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let footer = footer_lines(mode, state.summary_detail, area.width);
    let footer_height = if area.height >= 4 {
        u16::try_from(footer.len())
            .unwrap_or(1)
            .min(area.height - 2)
    } else {
        0
    };
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1 + footer_height),
    );
    let mut content = Vec::new();
    if let Some(status) = &state.status {
        content.push(Line::from(status.clone()).fg(Color::Yellow));
    }
    if let Some(error) = &state.sample_error {
        content.push(Line::from(format!("Unavailable: {error}")).fg(Color::Yellow));
    } else if let Some(sample) = &state.sample {
        if mode == Mode::Summary && !state.summary_detail {
            render_summary(frame, body, sample, state, content);
            content = Vec::new();
        } else if mode == Mode::Summary {
            match state.selected_row() {
                Some(SummaryRow::Pane(pane)) => {
                    content.extend(pane_detail_lines(pane, sample, None, body.width, true));
                    content.extend(
                        sample
                            .errors
                            .iter()
                            .filter(|error| error.starts_with(&format!("{}:", pane.target.pane_id)))
                            .map(|error| Line::from(error.clone()).fg(Color::Yellow)),
                    );
                }
                Some(SummaryRow::Server(server)) => {
                    content.extend(server_detail_lines(server, sample, body.width));
                }
                None => content.push(Line::from("No recognized agents")),
            }
        } else {
            content.extend(sample_lines(mode, sample, body.width, state.details));
        }
    } else {
        content.push(Line::from(match mode {
            Mode::Summary => "Collecting agent resources...".into(),
            Mode::Focused => state.target.as_ref().map_or_else(
                || "No target pane".into(),
                |target| format!("Loading {target}..."),
            ),
        }));
    }
    if !content.is_empty() {
        let max_scroll = content.len().saturating_sub(usize::from(body.height));
        let vertical = state
            .scroll
            .0
            .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
        frame.render_widget(
            Paragraph::new(content).scroll((vertical, state.scroll.1)),
            body,
        );
    }
    if footer_height > 0 {
        frame.render_widget(
            Paragraph::new(footer),
            Rect::new(
                area.x,
                area.bottom().saturating_sub(footer_height),
                area.width,
                footer_height,
            ),
        );
    }
}

fn footer_lines(mode: Mode, detail: bool, width: u16) -> Vec<Line<'static>> {
    let hints: Vec<&str> = match (mode, detail, width >= 62) {
        (Mode::Summary, false, true) => vec!["↑/↓: select  d: details  s: sort  q/Esc: close"],
        (Mode::Summary, false, false) => vec!["↑/↓ select · d details", "s sort · q/Esc close"],
        (Mode::Summary, true, true) => vec!["d: back to list  arrows: scroll  q/Esc: close"],
        (Mode::Summary, true, false) => vec!["d: back · arrows: scroll", "q/Esc: close"],
        (Mode::Focused, _, true) => {
            vec!["q/Esc: close  d: details  arrows: scroll  —: unavailable"]
        }
        (Mode::Focused, _, false) if width >= 35 => vec!["q: close  d: details  arrows: scroll"],
        (Mode::Focused, _, false) => vec!["q: close  d: details"],
    };
    hints
        .into_iter()
        .map(|hint| Line::from(hint).dim())
        .collect()
}

fn section(title: &str) -> Line<'static> {
    Line::from(title.to_owned()).bold().fg(Color::Cyan)
}

fn usage_lines(totals: &MetricTotals, capacity: SystemCapacity, width: u16) -> Vec<Line<'static>> {
    let cpu = capacity.cpu_percent(totals.cpu_percent);
    let ram = capacity.memory_percent(totals.rss_bytes);
    let mut lines = value_lines("Total CPU", &percent(cpu), width);
    lines.push(bar(cpu, width));
    lines.extend(value_lines(
        "Total memory",
        &format!("{} · {}", memory(totals.rss_bytes), percent(ram)),
        width,
    ));
    lines.push(bar(ram, width));
    lines
}

fn sample_lines(
    mode: Mode,
    sample: &MonitorSample,
    width: u16,
    details: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match mode {
        Mode::Summary => {
            lines.extend(summary_header(sample, width, SummarySort::Cpu));
            lines.extend(summary_list(sample, width, SummarySort::Cpu, None).0);
        }
        Mode::Focused => {
            for pane in &sample.panes {
                // Focused samples at most the target agent's shared server.
                let shared = sample.servers.first();
                lines.extend(pane_detail_lines(pane, sample, shared, width, details));
            }
            if sample.panes.is_empty() {
                lines.push(Line::from("No target pane"));
            }
        }
    }
    lines.extend(
        sample
            .errors
            .iter()
            .map(|error| Line::from(error.clone()).fg(Color::Yellow)),
    );
    lines
}

fn pane_detail_lines(
    pane: &PaneSample,
    sample: &MonitorSample,
    shared: Option<&SharedServerSample>,
    width: u16,
    technical: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        section("AGENT / TERMINAL"),
        Line::from(terminal_name(pane)).bold(),
        Line::from(format!("{} · {}", agent_name(pane), agent_state(pane))),
    ];
    if let Some(model) = metadata(pane, &["model"]) {
        lines.push(Line::from(format!("Model: {}", clean(model))));
    }
    if let Some(value) = metadata(pane, &["reasoning_effort", "effort"]) {
        lines.push(Line::from(format!("Reasoning: {}", effort(value))));
    }
    if let Some(folder) = folder(pane) {
        lines.push(Line::from(format!("Folder: {folder}")));
    }
    lines.push(Line::default());
    lines.push(section("RESOURCE USAGE"));
    lines.extend(usage_lines(&pane.stats.tree, sample.capacity, width));
    lines.extend(value_lines(
        "Processes",
        &pane.stats.tree.process_count.to_string(),
        width,
    ));
    if pane.root_source == RootSource::ShellFallback {
        lines.push(Line::from("Watching shell; no foreground process.").dim());
    } else if pane.root_source == RootSource::NoProcess {
        lines.push(Line::from("No process to measure.").dim());
    }
    lines.push(Line::default());
    lines.push(section("PROCESSES"));
    lines.extend(tree_lines(
        &sample.metrics,
        &pane.roots,
        sample.capacity,
        width,
        technical,
    ));
    if sample
        .metrics
        .process_ids(&pane.roots)
        .iter()
        .any(|pid| sample.metrics.processes[pid].name == "ssh")
    {
        lines.push(Line::from("SSH: local processes only; no remote data.").fg(Color::Yellow));
    }
    if !pane.stats.missing_roots.is_empty() {
        lines.push(Line::from("Some process measurements are unavailable.").fg(Color::Yellow));
    }
    if let Some(server) = shared {
        lines.extend(shared_server_lines(server, sample, width, technical));
    }
    if technical {
        lines.push(Line::default());
        lines.push(section("TECHNICAL DETAILS"));
        lines.push(Line::from(format!(
            "Machine: {} logical CPUs · {} RAM",
            sample.capacity.logical_cpus,
            memory(Some(sample.capacity.memory_bytes))
        )));
        lines.push(Line::from(format!("Pane: {}", pane.target.pane_id)).bold());
        lines.push(Line::from(format!(
            "Main process: CPU {} · Memory {}",
            percent(sample.capacity.cpu_percent(pane.stats.own.cpu_percent)),
            memory(pane.stats.own.rss_bytes)
        )));
        // Only this pane's supplied Herdr metadata belongs to this detail view.
        if let Ok(json) = serde_json::to_string_pretty(&pane.target) {
            lines.extend(json.lines().map(|line| Line::from(line.to_owned())));
        }
    }
    lines
}

/// Focused context below the pane's own tree: the shared server this agent's
/// sessions may use. It is labelled as shared and stays out of the pane totals.
fn shared_server_lines(
    server: &SharedServerSample,
    sample: &MonitorSample,
    width: u16,
    technical: bool,
) -> Vec<Line<'static>> {
    let notes = match server.kind {
        SharedServerKind::CodexAppServer => [
            "Shared by every Codex session using it.",
            "Older Codex and --no-daemon run in the pane.",
        ],
        SharedServerKind::OpencodeService => [
            "Shared by every opencode session using it.",
            "--standalone runs a private server in the pane.",
        ],
    };
    let mut lines = vec![
        Line::default(),
        section("SHARED SERVER"),
        Line::from(server.kind.label()).bold(),
    ];
    lines.extend(notes.map(|note| Line::from(note).dim()));
    lines.push(Line::from("Not included in the totals above.").dim());
    lines.extend(value_lines(
        "Server CPU",
        &percent(sample.capacity.cpu_percent(server.stats.tree.cpu_percent)),
        width,
    ));
    lines.extend(value_lines(
        "Server memory",
        &memory(server.stats.tree.rss_bytes),
        width,
    ));
    if let Some(error) = &server.error {
        lines.push(Line::from(format!("Unavailable: {error}")).fg(Color::Yellow));
    }
    if !server.roots.is_empty() {
        lines.extend(tree_lines(
            &sample.metrics,
            &server.roots,
            sample.capacity,
            width,
            technical,
        ));
    }
    lines
}

fn server_detail_lines(
    server: &SharedServerSample,
    sample: &MonitorSample,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        section("SHARED SERVER"),
        Line::from(server.kind.label()).bold(),
        Line::from(format!("{} · Shared", server.kind.agent())),
        Line::from("Shared by every session that uses it; not part of any pane.").dim(),
        Line::default(),
        section("RESOURCE USAGE"),
    ];
    lines.extend(usage_lines(&server.stats.tree, sample.capacity, width));
    lines.extend(value_lines(
        "Processes",
        &server.stats.tree.process_count.to_string(),
        width,
    ));
    if server.roots.is_empty() {
        lines.push(Line::from("No process to measure.").dim());
    }
    if let Some(error) = &server.error {
        lines.push(Line::from(format!("Unavailable: {error}")).fg(Color::Yellow));
    }
    lines.push(Line::default());
    lines.push(section("PROCESSES"));
    lines.extend(tree_lines(
        &sample.metrics,
        &server.roots,
        sample.capacity,
        width,
        true,
    ));
    lines.push(Line::default());
    lines.push(section("TECHNICAL DETAILS"));
    lines.push(Line::from(format!(
        "Machine: {} logical CPUs · {} RAM",
        sample.capacity.logical_cpus,
        memory(Some(sample.capacity.memory_bytes))
    )));
    lines.push(Line::from(format!(
        "Main process: CPU {} · Memory {}",
        percent(sample.capacity.cpu_percent(server.stats.own.cpu_percent)),
        memory(server.stats.own.rss_bytes)
    )));
    lines
}

fn summary_header(sample: &MonitorSample, width: u16, sort: SummarySort) -> Vec<Line<'static>> {
    let mut lines = usage_lines(&sample.totals, sample.capacity, width);
    lines.extend(value_lines(
        &format!("Agents {}", sample.agent_count),
        &format!("Processes {}", sample.totals.process_count),
        width,
    ));
    if !sample.errors.is_empty() {
        lines.push(
            Line::from(format!("Unavailable data: {}", sample.errors.len())).fg(Color::Yellow),
        );
    }
    lines.push(Line::from(format!("Sort: {}  (s)", sort.label())).dim());
    let row_width = width.saturating_sub(2);
    if row_width >= 76 {
        lines.push(section(&format!(
            "  {} {} {} {} {}",
            column("TERMINAL", usize::from(row_width).saturating_sub(55), false),
            column("AGENT", 16, false),
            column("STATUS", 14, false),
            column("CPU", 9, true),
            column("MEMORY", 12, true)
        )));
    } else {
        lines.push(section("TERMINAL / AGENT / STATUS"));
    }
    lines
}

fn summary_entries(
    sample: &MonitorSample,
    width: u16,
    sort: SummarySort,
) -> Vec<(SummaryRow<'_>, Vec<Line<'static>>)> {
    let width = width.saturating_sub(2); // Reserve the selection marker.
    summary_rows(sample, sort)
        .into_iter()
        .map(|row| {
            let lines = match row {
                SummaryRow::Pane(pane) => {
                    let mut terminal = terminal_name(pane);
                    if sample
                        .panes
                        .iter()
                        .filter(|other| terminal_name(other) == terminal)
                        .count()
                        > 1
                    {
                        terminal = format!("[{}] {terminal}", pane.target.pane_id);
                    }
                    row_lines(
                        &terminal,
                        &agent_name(pane),
                        agent_state(pane),
                        &pane.stats.tree,
                        sample.capacity,
                        width,
                    )
                }
                SummaryRow::Server(server) => row_lines(
                    server.kind.label(),
                    server.kind.agent(),
                    "Shared",
                    &server.stats.tree,
                    sample.capacity,
                    width,
                ),
            };
            (row, lines)
        })
        .collect()
}

/// A table row on wide screens, otherwise a card that stacks identity and values.
fn row_lines(
    terminal: &str,
    agent: &str,
    status: &str,
    totals: &MetricTotals,
    capacity: SystemCapacity,
    width: u16,
) -> Vec<Line<'static>> {
    let cpu = percent(capacity.cpu_percent(totals.cpu_percent));
    let ram = memory(totals.rss_bytes);
    if width >= 76 {
        let name_width = usize::from(width).saturating_sub(55);
        return vec![Line::from(format!(
            "{} {} {} {} {}",
            column(terminal, name_width, false),
            column(agent, 16, false),
            column(status, 14, false),
            column(&cpu, 9, true),
            column(&ram, 12, true)
        ))];
    }
    let mut lines = vec![Line::from(terminal.to_owned()).bold()];
    let identity = format!("{agent} · {status}");
    if text_width(&identity) <= usize::from(width) {
        lines.push(Line::from(identity));
    } else {
        lines.push(Line::from(agent.to_owned()));
        lines.push(Line::from(status.to_owned()));
    }
    lines.extend(value_lines(&format!("CPU {cpu}"), &ram, width));
    lines.push(Line::default());
    lines
}

/// List lines below the pinned header, and the selected row's line range.
fn summary_list(
    sample: &MonitorSample,
    width: u16,
    sort: SummarySort,
    selected: Option<&RowId>,
) -> (Vec<Line<'static>>, (usize, usize)) {
    let mut lines = Vec::new();
    let mut selection = (0, 0);
    if sample.panes.is_empty() {
        lines.push(Line::from("No recognized agents"));
    }
    let mut servers = false;
    for (row, entry) in summary_entries(sample, width, sort) {
        if matches!(row, SummaryRow::Server(_)) && !servers {
            servers = true;
            lines.push(section("  SHARED SERVERS"));
        }
        let active = selected == Some(&row.id());
        let start = lines.len();
        for (index, mut line) in entry.into_iter().enumerate() {
            line.spans
                .insert(0, Span::raw(if active && index == 0 { "> " } else { "  " }));
            if active {
                line = line.fg(Color::Black).bg(Color::Cyan);
            }
            lines.push(line);
        }
        if active {
            selection = (start, lines.len());
        }
    }
    (lines, selection)
}

// Keep the selected row or card entirely visible whenever it fits. Very short
// viewports start at its title, rather than showing only its final metric line.
fn selection_scroll(start: usize, end: usize, total: usize, height: usize) -> usize {
    let desired = if end - start > height {
        start
    } else {
        end.saturating_sub(height)
    };
    desired.min(total.saturating_sub(height))
}

fn render_summary(
    frame: &mut Frame,
    area: Rect,
    sample: &MonitorSample,
    state: &ViewState,
    mut header: Vec<Line<'static>>,
) {
    if area.height == 0 {
        return;
    }
    header.extend(summary_header(sample, area.width, state.summary_sort));
    // Even a tiny popup leaves one line available for the selected item.
    let header_height = header.len().min(usize::from(area.height.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, header_height as u16),
    );
    let list_area = Rect::new(
        area.x,
        area.y + header_height as u16,
        area.width,
        area.height - header_height as u16,
    );
    let selected = state.selected_row().map(SummaryRow::id);
    let (lines, selection) =
        summary_list(sample, area.width, state.summary_sort, selected.as_ref());
    let offset = selection_scroll(
        selection.0,
        selection.1,
        lines.len(),
        usize::from(list_area.height),
    );
    // Slice rather than converting an arbitrarily long list offset to u16.
    let visible: Vec<_> = lines
        .into_iter()
        .skip(offset)
        .take(usize::from(list_area.height))
        .collect();
    frame.render_widget(Paragraph::new(visible), list_area);
}

fn summary_page_size(sample: &MonitorSample, width: u16, height: u16, sort: SummarySort) -> usize {
    let width = width.saturating_sub(2 * u16::from(width >= 30));
    let footer = footer_lines(Mode::Summary, false, width).len();
    let header = summary_header(sample, width, sort).len();
    let available = usize::from(height)
        .saturating_sub(1 + footer + header)
        .max(1);
    let row_height = summary_entries(sample, width, sort)
        .iter()
        .map(|(_, lines)| lines.len())
        .max()
        .unwrap_or(1);
    (available / row_height).max(1)
}

/// Presentation only: edges were discovered by the process collector.
fn tree_lines(
    metrics: &MetricsSnapshot,
    roots: &[u32],
    capacity: SystemCapacity,
    width: u16,
    details: bool,
) -> Vec<Line<'static>> {
    let ids: BTreeSet<_> = metrics.process_ids(roots).into_iter().collect();
    let top: Vec<_> = ids
        .iter()
        .copied()
        .filter(|pid| !ids.contains(&metrics.processes[pid].ppid))
        .collect();
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    let table = width >= 42;
    let name_width = usize::from(width).saturating_sub(23);
    if table {
        lines.push(
            Line::from(format!(
                "{} {} {}",
                column("Process", name_width, false),
                column("CPU", 9, true),
                column("Memory", 12, true)
            ))
            .dim(),
        );
    }
    // The second pass also makes malformed/cyclic fixture topology visible once.
    for root in top.into_iter().chain(ids.iter().copied()) {
        let mut pending = vec![(root, String::new(), String::new())];
        while let Some((pid, prefix, connector)) = pending.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let process = &metrics.processes[&pid];
            let name = format!("{prefix}{connector}{}", clean(&process.name));
            let cpu = percent(capacity.cpu_percent(process.cpu_percent));
            let ram = memory(process.rss_bytes);
            if table {
                lines.push(Line::from(format!(
                    "{} {} {}",
                    column(&name, name_width, false),
                    column(&cpu, 9, true),
                    column(&ram, 12, true)
                )));
            } else {
                lines.push(Line::from(name));
                lines.extend(value_lines(&format!("  {cpu}"), &ram, width));
            }
            if details {
                lines.push(Line::from(format!("  PID {pid} · {}", clean(&process.cmdline))).dim());
            }
            let children: Vec<_> = process
                .children
                .iter()
                .filter(|child| ids.contains(child))
                .copied()
                .collect();
            let continuation = match connector.as_str() {
                "├─ " => "│  ",
                "└─ " => "   ",
                _ => "",
            };
            let child_prefix = format!("{prefix}{continuation}");
            for (index, child) in children.iter().enumerate().rev() {
                pending.push((
                    *child,
                    child_prefix.clone(),
                    if index + 1 == children.len() {
                        "└─ "
                    } else {
                        "├─ "
                    }
                    .into(),
                ));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests;
