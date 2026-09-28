use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::herdr::{AgentInfo, ForegroundProcess, HerdrClient, HerdrError, SessionSnapshot};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    socket: PathBuf,
    context: ActionContext,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "herdr-actions-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = directory.join("plugin");
        fs::create_dir_all(root.join("target/release")).unwrap();
        fs::write(monitor_executable_path(&root), b"fixture").unwrap();
        let socket = fixture_endpoint(&directory, "session");
        let context = ActionContext {
            plugin_id: "test.resources".into(),
            plugin_root: root,
            state_dir: Some(directory.join("state")),
            focused_pane_id: Some("w1:p1".into()),
        };
        Self {
            directory,
            socket,
            context,
        }
    }

    fn executable(&self) -> String {
        monitor_executable_path(&self.context.plugin_root)
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn state(&self) -> TabState {
        self.state_for("w1", "w1:t1")
    }

    fn state_for(&self, workspace_id: &str, tab_id: &str) -> TabState {
        SessionState::acquire(self.context.state_dir.as_deref().unwrap(), &self.socket)
            .unwrap()
            .unwrap()
            .for_tab(workspace_id, tab_id)
            .unwrap()
    }

    fn save(&self, pane: &PaneInfo) {
        self.state_for(&pane.workspace_id, &pane.tab_id)
            .save(&pane.pane_id, &pane.terminal_id)
            .unwrap();
    }

    fn execute(&self, client: &mut FakeClient) -> Result<String, String> {
        execute(Action::ToggleFocused, client, &self.context, &self.socket)
    }
}

fn fixture_endpoint(directory: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        let endpoint = directory.join(format!("{name}.sock"));
        fs::write(&endpoint, b"").unwrap();
        endpoint
    }
    #[cfg(windows)]
    {
        // A named pipe is not a filesystem socket. State identity must not
        // require a marker file or a live server endpoint to exist.
        directory.join(format!("{name}.sock"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn pane(id: &str) -> PaneInfo {
    let workspace_id = id.split_once(':').map_or("w1", |(workspace, _)| workspace);
    PaneInfo {
        pane_id: id.into(),
        terminal_id: format!("terminal-{id}"),
        workspace_id: workspace_id.into(),
        tab_id: format!("{workspace_id}:t1"),
        ..Default::default()
    }
}

fn process(id: &str, executable: &str, mode: Option<&str>) -> PaneProcessInfo {
    let mut argv = vec![executable.to_string()];
    argv.extend(mode.map(str::to_owned));
    PaneProcessInfo {
        pane_id: id.into(),
        foreground_processes: vec![ForegroundProcess {
            pid: 10,
            name: "fixture".into(),
            argv: Some(argv),
            ..Default::default()
        }],
        ..Default::default()
    }
}

struct FakeClient {
    snapshot: SessionSnapshot,
    processes: BTreeMap<String, PaneProcessInfo>,
    process_errors: BTreeSet<String>,
    summaries: Vec<String>,
    opens: Vec<(String, String)>,
    closes: Vec<String>,
    opened: PaneInfo,
    sabotage_save: Option<PathBuf>,
    close_error: bool,
}

impl FakeClient {
    fn new() -> Self {
        Self {
            snapshot: SessionSnapshot {
                panes: vec![pane("w1:p1")],
                focused_pane_id: Some("w1:p1".into()),
                ..Default::default()
            },
            processes: BTreeMap::from([(
                "w1:p1".into(),
                process(
                    "w1:p1",
                    &std::env::current_exe().unwrap().to_string_lossy(),
                    None,
                ),
            )]),
            process_errors: BTreeSet::new(),
            summaries: Vec::new(),
            opens: Vec::new(),
            closes: Vec::new(),
            opened: pane("w1:p2"),
            sabotage_save: None,
            close_error: false,
        }
    }

    fn monitor(&mut self, fixture: &Fixture, id: &str) -> PaneInfo {
        self.monitor_pane(fixture, pane(id))
    }

    fn monitor_pane(&mut self, fixture: &Fixture, pane: PaneInfo) -> PaneInfo {
        self.snapshot.panes.push(pane.clone());
        self.processes.insert(
            pane.pane_id.clone(),
            process(&pane.pane_id, &fixture.executable(), Some("focused")),
        );
        pane
    }
}

impl HerdrClient for FakeClient {
    fn snapshot(&mut self) -> crate::herdr::Result<SessionSnapshot> {
        Ok(self.snapshot.clone())
    }
    fn panes(&mut self) -> crate::herdr::Result<Vec<PaneInfo>> {
        Ok(self.snapshot.panes.clone())
    }
    fn agents(&mut self) -> crate::herdr::Result<Vec<AgentInfo>> {
        Ok(Vec::new())
    }
    fn process_info(&mut self, id: &str) -> crate::herdr::Result<PaneProcessInfo> {
        if self.process_errors.contains(id) {
            Err(HerdrError::Api {
                code: "fixture_failure".into(),
                message: "unavailable".into(),
            })
        } else {
            Ok(self.processes.get(id).unwrap().clone())
        }
    }
}

impl PaneController for FakeClient {
    fn open_summary(&mut self, plugin_id: &str) -> crate::herdr::Result<()> {
        self.summaries.push(plugin_id.into());
        Ok(())
    }
    fn open_focused(&mut self, plugin_id: &str, target: &str) -> crate::herdr::Result<PaneInfo> {
        self.opens.push((plugin_id.into(), target.into()));
        if let Some(path) = self.sabotage_save.as_ref() {
            fs::create_dir(path).unwrap();
        }
        Ok(self.opened.clone())
    }
    fn close_plugin_pane(&mut self, id: &str) -> crate::herdr::Result<()> {
        self.closes.push(id.into());
        if self.close_error {
            Err(HerdrError::Api {
                code: "close_failed".into(),
                message: "fixture".into(),
            })
        } else {
            Ok(())
        }
    }
}

#[test]
fn summary_needs_no_state_or_target() {
    let mut fixture = Fixture::new();
    fixture.context.state_dir = None;
    fixture.context.focused_pane_id = None;
    let mut client = FakeClient::new();
    execute(
        Action::OpenSummary,
        &mut client,
        &fixture.context,
        &fixture.socket,
    )
    .unwrap();
    assert_eq!(client.summaries, ["test.resources"]);
    assert!(client.opens.is_empty());
    assert!(!fixture.directory.join("state").exists());
}

#[test]
fn open_prefers_captured_target_and_persists_terminal_identity() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.snapshot.focused_pane_id = Some("different-current-focus".into());
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens, [("test.resources".into(), "w1:p1".into())]);
    let record = fixture.state().load().unwrap().unwrap();
    assert_eq!(record.focused_monitor_pane_id, "w1:p2");
    assert_eq!(record.terminal_id, "terminal-w1:p2");
    assert_eq!(
        record.session_socket,
        crate::herdr::transport::endpoint_identity(&fixture.socket).unwrap()
    );
}

#[test]
fn existing_monitor_closes_even_when_action_invoked_on_it() {
    let mut fixture = Fixture::new();
    let mut client = FakeClient::new();
    let monitor = client.monitor(&fixture, "w1:p2");
    fixture.save(&monitor);
    fixture.context.focused_pane_id = Some(monitor.pane_id);
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.closes, ["w1:p2"]);
    assert!(client.opens.is_empty());
    assert!(fixture.state().load().unwrap().is_none());
}

#[test]
fn manual_monitor_without_state_is_discovered() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.monitor(&fixture, "w1:p2");
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.closes, ["w1:p2"]);
    assert!(client.opens.is_empty());
}

#[test]
fn other_tab_monitor_and_state_are_preserved_when_opening_here() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let mut other = pane("w1:other");
    other.tab_id = "w1:t2".into();
    let other = client.monitor_pane(&fixture, other);
    fixture.save(&other);
    client.process_errors.insert(other.pane_id.clone());
    // The invocation context still selects this tab when another client owns
    // the session snapshot's current focus.
    client.snapshot.focused_pane_id = Some(other.pane_id.clone());

    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens, [("test.resources".into(), "w1:p1".into())]);
    assert!(client.closes.is_empty());
    assert_eq!(
        fixture
            .state()
            .load()
            .unwrap()
            .unwrap()
            .focused_monitor_pane_id,
        "w1:p2"
    );
    let other_record = fixture.state_for("w1", "w1:t2").load().unwrap().unwrap();
    assert_eq!(other_record.focused_monitor_pane_id, other.pane_id);
}

#[test]
fn discovered_monitor_closes_only_within_invocation_workspace_and_tab() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let mut other_tab = pane("w1:other-tab");
    other_tab.tab_id = "w1:t2".into();
    let mut other_workspace = pane("w2:other-workspace");
    other_workspace.tab_id = "w1:t1".into();
    for other in [other_tab, other_workspace] {
        let other = client.monitor_pane(&fixture, other);
        client.process_errors.insert(other.pane_id);
    }
    client.monitor(&fixture, "w1:local-monitor");

    fixture.execute(&mut client).unwrap();
    assert_eq!(client.closes, ["w1:local-monitor"]);
    assert!(client.opens.is_empty());
}

#[test]
fn missing_invocation_target_does_not_close_an_existing_monitor() {
    let mut fixture = Fixture::new();
    let mut client = FakeClient::new();
    let monitor = client.monitor(&fixture, "w1:p2");
    fixture.save(&monitor);
    fixture.context.focused_pane_id = Some("closed-origin".into());

    fixture.execute(&mut client).unwrap();
    assert!(client.closes.is_empty());
    assert!(client.opens.is_empty());
    assert!(fixture.state().load().unwrap().is_some());
}

#[test]
fn missing_tab_identity_does_not_close_or_open_a_monitor() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.monitor(&fixture, "w1:p2");
    client.snapshot.panes[0].tab_id.clear();

    assert!(fixture
        .execute(&mut client)
        .unwrap_err()
        .contains("tab identity"));
    assert!(client.closes.is_empty());
    assert!(client.opens.is_empty());
}

#[test]
fn recorded_monitor_moved_to_another_tab_is_not_closed() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let monitor = client.monitor(&fixture, "w1:moved");
    fixture.save(&monitor);
    client.snapshot.panes.last_mut().unwrap().tab_id = "w1:t2".into();
    client.process_errors.insert(monitor.pane_id);

    fixture.execute(&mut client).unwrap();
    assert!(client.closes.is_empty());
    assert_eq!(client.opens, [("test.resources".into(), "w1:p1".into())]);
    assert_eq!(
        fixture
            .state()
            .load()
            .unwrap()
            .unwrap()
            .focused_monitor_pane_id,
        "w1:p2"
    );
}

#[test]
fn legacy_session_record_never_closes_a_monitor_in_another_tab() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let mut other = pane("w1:legacy");
    other.tab_id = "w1:t2".into();
    let other = client.monitor_pane(&fixture, other);
    client.process_errors.insert(other.pane_id.clone());
    let state = fixture.state();
    let filename = state.path().file_name().unwrap().to_string_lossy();
    let session_name = filename.split("-tab-").next().unwrap();
    let legacy_path = state.path().with_file_name(format!("{session_name}.json"));
    let legacy = serde_json::to_vec(&serde_json::json!({
        "session_socket": crate::herdr::transport::endpoint_identity(&fixture.socket).unwrap(),
        "focused_monitor_pane_id": other.pane_id,
        "terminal_id": other.terminal_id,
    }))
    .unwrap();
    fs::write(&legacy_path, &legacy).unwrap();
    drop(state);

    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens.len(), 1);
    assert!(client.closes.is_empty());
    assert_eq!(fs::read(legacy_path).unwrap(), legacy);
}

#[test]
fn renamed_monitor_in_same_tab_is_discovered_after_old_id_expires() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    fixture.save(&pane("w1:old"));
    client.monitor(&fixture, "w1:new");
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.closes, ["w1:new"]);
    assert!(client.opens.is_empty());
}

#[test]
fn stale_record_opens_new_monitor_without_closing_unrelated_panes() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    fixture.save(&pane("w1:closed"));
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens.len(), 1);
    assert!(client.closes.is_empty());
}

#[test]
fn reused_id_or_changed_process_is_never_closed() {
    for same_terminal in [false, true] {
        let fixture = Fixture::new();
        let mut client = FakeClient::new();
        let mut record_pane = pane("w1:p1");
        if !same_terminal {
            record_pane.terminal_id = "previous-terminal".into();
        }
        fixture.save(&record_pane);
        fixture.execute(&mut client).unwrap();
        assert!(client.closes.is_empty());
        assert_eq!(client.opens.len(), 1);
    }
}

#[test]
fn basename_or_same_title_does_not_prove_monitor_identity() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.snapshot.panes[0].label = Some("Resource Details".into());
    client.processes.insert(
        "w1:p1".into(),
        process(
            "w1:p1",
            &format!("herdr-resource-monitor{}", std::env::consts::EXE_SUFFIX),
            Some("focused"),
        ),
    );
    fixture.save(&pane("w1:p1"));
    assert!(fixture.execute(&mut client).is_err());
    assert!(client.closes.is_empty());
    assert!(client.opens.is_empty());
    assert!(fixture.state().load().unwrap().is_some());
}

#[test]
fn stored_monitor_unknown_or_read_error_preserves_state() {
    for read_error in [false, true] {
        let fixture = Fixture::new();
        let mut client = FakeClient::new();
        let monitor = client.monitor(&fixture, "w1:p2");
        fixture.save(&monitor);
        if read_error {
            client.process_errors.insert("w1:p2".into());
        } else {
            client
                .processes
                .get_mut("w1:p2")
                .unwrap()
                .foreground_processes
                .clear();
        }
        assert!(fixture.execute(&mut client).is_err());
        assert!(fixture.state().load().unwrap().is_some());
        assert!(client.opens.is_empty());
        assert!(client.closes.is_empty());
    }
}

#[test]
fn failed_close_keeps_record_for_retry() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let monitor = client.monitor(&fixture, "w1:p2");
    fixture.save(&monitor);
    client.close_error = true;
    assert!(fixture.execute(&mut client).is_err());
    assert!(fixture.state().load().unwrap().is_some());
    assert!(client.opens.is_empty());
}

#[test]
fn no_captured_target_uses_current_focus_but_missing_captured_target_does_not() {
    let mut fixture = Fixture::new();
    let mut client = FakeClient::new();
    fixture.context.focused_pane_id = Some("closed".into());
    assert!(fixture
        .execute(&mut client)
        .unwrap()
        .contains("No current normal pane"));
    assert!(client.opens.is_empty());
    fixture.context.focused_pane_id = None;
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens.len(), 1);
}

#[test]
fn other_monitor_mode_is_not_used_as_a_target() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.processes.insert(
        "w1:p1".into(),
        process("w1:p1", &fixture.executable(), Some("summary")),
    );
    assert!(fixture
        .execute(&mut client)
        .unwrap()
        .contains("normal terminal"));
    assert!(client.opens.is_empty());
    assert!(client.closes.is_empty());
}

#[test]
fn incomplete_monitor_launch_does_not_create_a_duplicate() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.processes.insert(
        "w1:p1".into(),
        process("w1:p1", &fixture.executable(), None),
    );
    assert!(fixture.execute(&mut client).is_err());
    assert!(client.opens.is_empty());
}

#[test]
fn sessions_with_identical_pane_ids_have_independent_state() {
    let fixture = Fixture::new();
    let second_socket = fixture_endpoint(&fixture.directory, "second");
    let mut client = FakeClient::new();
    fixture.execute(&mut client).unwrap();
    execute(
        Action::ToggleFocused,
        &mut client,
        &fixture.context,
        &second_socket,
    )
    .unwrap();
    let first = fixture.state();
    let second = SessionState::acquire(
        fixture.context.state_dir.as_deref().unwrap(),
        &second_socket,
    )
    .unwrap()
    .unwrap()
    .for_tab("w1", "w1:t1")
    .unwrap();
    assert_ne!(first.path(), second.path());
    assert!(first.load().unwrap().is_some());
    assert!(second.load().unwrap().is_some());
    assert_eq!(client.opens.len(), 2);
    assert!(client.closes.is_empty());
}

#[test]
fn tabs_and_workspaces_have_independent_records_with_full_scope_identity() {
    let fixture = Fixture::new();
    let mut paths = BTreeSet::new();
    for (workspace, tab, monitor) in [
        ("w1", "w1:t1", "monitor-one"),
        ("w1", "w1:t2", "monitor-two"),
        ("w2", "w1:t1", "monitor-three"),
    ] {
        let state = fixture.state_for(workspace, tab);
        assert!(state.load().unwrap().is_none());
        assert!(paths.insert(state.path().to_path_buf()));
        state.save(monitor, "terminal").unwrap();
    }
    for (workspace, tab, monitor) in [
        ("w1", "w1:t1", "monitor-one"),
        ("w1", "w1:t2", "monitor-two"),
        ("w2", "w1:t1", "monitor-three"),
    ] {
        let record = fixture.state_for(workspace, tab).load().unwrap().unwrap();
        assert_eq!(record.workspace_id, workspace);
        assert_eq!(record.tab_id, tab);
        assert_eq!(record.focused_monitor_pane_id, monitor);
    }
}

#[test]
fn lock_contention_skips_and_drop_allows_next_invocation() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let held = fixture.state();
    assert!(fixture.execute(&mut client).unwrap().contains("skipped"));
    assert!(client.opens.is_empty());
    drop(held);
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.opens.len(), 1);
}

#[cfg(unix)]
#[test]
fn socket_symlink_uses_the_same_state_lock() {
    let fixture = Fixture::new();
    let alias = fixture.directory.join("alias.sock");
    std::os::unix::fs::symlink(&fixture.socket, &alias).unwrap();
    let held = fixture.state();
    assert!(
        SessionState::acquire(fixture.context.state_dir.as_deref().unwrap(), &alias)
            .unwrap()
            .is_none()
    );
    drop(held);
    assert!(
        SessionState::acquire(fixture.context.state_dir.as_deref().unwrap(), &alias)
            .unwrap()
            .is_some()
    );
}

#[cfg(windows)]
#[test]
fn pipe_endpoint_case_alias_uses_the_same_state_lock_without_a_marker_file() {
    let fixture = Fixture::new();
    assert!(!fixture.socket.exists());
    let alias = PathBuf::from(fixture.socket.to_string_lossy().to_ascii_uppercase());
    let held = fixture.state();
    assert!(
        SessionState::acquire(fixture.context.state_dir.as_deref().unwrap(), &alias)
            .unwrap()
            .is_none()
    );
    drop(held);
    assert!(
        SessionState::acquire(fixture.context.state_dir.as_deref().unwrap(), &alias)
            .unwrap()
            .is_some()
    );
}

#[test]
fn state_save_replaces_previous_record_without_leaving_a_temporary_file() {
    let fixture = Fixture::new();
    let state = fixture.state();
    state.save("w1:first", "first-terminal").unwrap();
    state.save("w1:second", "second-terminal").unwrap();
    let record = state.load().unwrap().unwrap();
    assert_eq!(record.focused_monitor_pane_id, "w1:second");
    assert_eq!(record.terminal_id, "second-terminal");
    let state_dir = fixture.context.state_dir.as_deref().unwrap();
    assert!(fs::read_dir(state_dir).unwrap().all(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|extension| extension != "tmp")
    }));
}

#[test]
fn relative_executable_identity_closes_the_verified_monitor() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    let monitor = client.monitor(&fixture, "w1:relative");
    fixture.save(&monitor);
    let foreground = &mut client
        .processes
        .get_mut(&monitor.pane_id)
        .unwrap()
        .foreground_processes[0];
    foreground.argv = Some(vec![
        format!(
            ".{}herdr-resource-monitor{}",
            std::path::MAIN_SEPARATOR,
            std::env::consts::EXE_SUFFIX
        ),
        "focused".into(),
    ]);
    foreground.cwd = Some(
        fixture
            .context
            .plugin_root
            .join("target/release")
            .to_string_lossy()
            .into_owned(),
    );
    fixture.execute(&mut client).unwrap();
    assert_eq!(client.closes, ["w1:relative"]);
    assert!(client.opens.is_empty());
}

#[test]
fn corrupt_or_foreign_record_does_not_modify_panes() {
    let fixture = Fixture::new();
    let path = fixture.state().path().to_path_buf();
    let mut client = FakeClient::new();
    fs::write(&path, b"not JSON").unwrap();
    assert!(fixture
        .execute(&mut client)
        .unwrap_err()
        .contains("Invalid monitor state"));
    fs::write(&path, br#"{"session_socket":"/different-session","workspace_id":"w1","tab_id":"w1:t1","focused_monitor_pane_id":"w1:p1","terminal_id":"terminal-w1:p1"}"#).unwrap();
    assert!(fixture
        .execute(&mut client)
        .unwrap_err()
        .contains("different Herdr session"));
    assert!(client.opens.is_empty());
    assert!(client.closes.is_empty());
}

#[test]
fn record_with_foreign_tab_scope_does_not_modify_panes() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    fixture.save(&pane("w1:p2"));
    let state = fixture.state();
    let mut record = state.load().unwrap().unwrap();
    let path = state.path().to_path_buf();
    drop(state);
    for (workspace, tab) in [("w2", "w1:t1"), ("w1", "w1:t2")] {
        record.workspace_id = workspace.into();
        record.tab_id = tab.into();
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(fixture
            .execute(&mut client)
            .unwrap_err()
            .contains("different Herdr tab"));
    }
    assert!(client.opens.is_empty());
    assert!(client.closes.is_empty());
}

#[test]
fn save_failure_rolls_back_only_new_pane() {
    let fixture = Fixture::new();
    let mut client = FakeClient::new();
    client.sabotage_save = Some(fixture.state().path().to_path_buf());
    let error = fixture.execute(&mut client).unwrap_err();
    assert!(error.contains("newly opened monitor was closed"));
    assert_eq!(client.opens.len(), 1);
    assert_eq!(client.closes, ["w1:p2"]);
}

#[test]
fn environment_requires_plugin_paths_and_prefers_context() {
    let mut values = BTreeMap::from([
        ("HERDR_PLUGIN_ID", "example.resources"),
        ("HERDR_PLUGIN_ROOT", "/plugin"),
        ("HERDR_PLUGIN_STATE_DIR", "/state"),
        (
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"focused_pane_id":"w1:p1"}"#,
        ),
        ("HERDR_PANE_ID", "w1:p2"),
    ]);
    let context = ActionContext::from_lookup(Action::ToggleFocused, |key| {
        values.get(key).map(|value| value.to_string())
    })
    .unwrap();
    assert_eq!(context.focused_pane_id.as_deref(), Some("w1:p1"));
    values.remove("HERDR_PLUGIN_CONTEXT_JSON");
    let context = ActionContext::from_lookup(Action::ToggleFocused, |key| {
        values.get(key).map(|value| value.to_string())
    })
    .unwrap();
    assert_eq!(context.focused_pane_id.as_deref(), Some("w1:p2"));
    values.remove("HERDR_PLUGIN_STATE_DIR");
    assert!(
        ActionContext::from_lookup(Action::ToggleFocused, |key| values
            .get(key)
            .map(|value| value.to_string()))
        .unwrap_err()
        .contains("HERDR_PLUGIN_STATE_DIR")
    );
    assert!(ActionContext::from_lookup(Action::OpenSummary, |key| values
        .get(key)
        .map(|value| value.to_string()))
    .is_ok());
    values.remove("HERDR_PLUGIN_ROOT");
    assert!(ActionContext::from_lookup(Action::OpenSummary, |key| values
        .get(key)
        .map(|value| value.to_string()))
    .unwrap_err()
    .contains("HERDR_PLUGIN_ROOT"));
}
