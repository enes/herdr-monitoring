#![cfg(any(unix, windows))]

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use herdr_resource_monitor::herdr::{
    CliHerdrClient, CommandOutput, CommandRunner, EventStream, HerdrError, HerdrEvent, Result,
};
use serde_json::{json, Value};

#[path = "support/local_transport.rs"]
mod local_transport;
use local_transport::{endpoint, LocalListener, LocalStream};

const ACK: &str =
    "{\"id\":\"herdr.resource-monitor:events\",\"result\":{\"type\":\"subscription_started\"}}\n";

struct Server {
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
    _transport_guard: local_transport::KeepAlive,
}

impl Server {
    fn start(handler: impl FnOnce(LocalStream) + Send + 'static) -> Self {
        let path = endpoint("ev");
        let listener = LocalListener::bind(&path).unwrap();
        let transport_guard = local_transport::keep_alive(&listener);
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            handler(stream);
        });
        Self {
            path,
            thread: Some(thread),
            _transport_guard: transport_guard,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let result = self.thread.take().unwrap().join();
        let _ = std::fs::remove_file(&self.path);
        if !thread::panicking() {
            result.expect("fake server failed");
        }
    }
}

fn request(stream: &mut LocalStream) -> Value {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn acknowledged(mut stream: LocalStream, body: &[u8]) {
    request(&mut stream);
    stream.write_all(ACK.as_bytes()).unwrap();
    let _ = stream.write_all(body);
}

#[test]
fn subscribes_to_verified_names_and_reads_fragmented_and_batched_events() {
    let (checked_tx, checked_rx) = mpsc::channel();
    let server = Server::start(move |mut stream| {
        let request = request(&mut stream);
        assert_eq!(request["method"], "events.subscribe");
        let names: Vec<_> = request["params"]["subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "pane.focused",
                "pane.created",
                "pane.closed",
                "pane.exited",
                "pane.moved",
                "tab.focused",
                "tab.closed",
                "workspace.focused",
                "workspace.closed",
                "layout.updated"
            ]
        );
        checked_tx.send(()).unwrap();
        for fragment in ACK.as_bytes().chunks(7) {
            stream.write_all(fragment).unwrap();
        }
        let events = [
            json!({"event":"pane_focused","data":{"type":"pane_focused","pane_id":"p:2","workspace_id":"w:1"}}),
            json!({"event":"tab_focused","data":{"type":"tab_focused","tab_id":"t:2","workspace_id":"w:1"}}),
            json!({"event":"workspace_focused","data":{"type":"workspace_focused","workspace_id":"w:2"}}),
            json!({"event":"pane_closed","data":{"type":"pane_closed","pane_id":"p:2","workspace_id":"w:1"}}),
        ];
        let data = events
            .iter()
            .map(|value| format!("{value}\n"))
            .collect::<String>();
        for fragment in data.as_bytes().chunks(11) {
            stream.write_all(fragment).unwrap();
        }
    });
    let mut events = EventStream::connect(&server.path).unwrap();
    checked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::PaneFocused { pane_id, .. }) if pane_id == "p:2")
    );
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::TabFocused { tab_id, .. }) if tab_id == "t:2")
    );
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::WorkspaceFocused { workspace_id }) if workspace_id == "w:2")
    );
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::PaneClosed { pane_id, .. }) if pane_id == "p:2")
    );
    assert!(events.next_event().unwrap().is_none());
}

#[test]
fn shutdown_wakes_a_blocked_reader_and_is_idempotent() {
    let server = Server::start(|mut stream| {
        request(&mut stream);
        stream.write_all(ACK.as_bytes()).unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    });
    let mut events = EventStream::connect(&server.path).unwrap();
    let shutdown = events.shutdown_handle();
    let (result_tx, result_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        result_tx.send(events.next_event()).unwrap();
    });
    shutdown.shutdown().unwrap();
    shutdown.shutdown().unwrap();
    assert!(result_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap()
        .is_none());
    reader.join().unwrap();
}

#[test]
fn eof_ack_errors_and_wrong_ack_are_distinct() {
    let server = Server::start(|mut stream| {
        request(&mut stream);
    });
    assert!(matches!(
        EventStream::connect(&server.path),
        Err(HerdrError::EventProtocol(_))
    ));
    let server = Server::start(|mut stream| {
        request(&mut stream);
        stream.write_all(b"{\"id\":\"herdr.resource-monitor:events\",\"error\":{\"code\":\"unsupported_subscription\",\"message\":\"unsupported\"}}\n").unwrap();
    });
    assert!(
        matches!(EventStream::connect(&server.path), Err(HerdrError::Api { code, .. }) if code == "unsupported_subscription")
    );
    let server = Server::start(|mut stream| {
        request(&mut stream);
        stream
            .write_all(b"{\"id\":\"wrong\",\"result\":{\"type\":\"subscription_started\"}}\n")
            .unwrap();
    });
    assert!(matches!(
        EventStream::connect(&server.path),
        Err(HerdrError::EventProtocol(_))
    ));
}

#[test]
fn missing_ack_has_a_deadline() {
    let server = Server::start(|mut stream| {
        request(&mut stream);
        let mut end = Vec::new();
        // The client's handshake timeout closes the connection.
        stream.read_to_end(&mut end).unwrap();
    });
    assert!(matches!(
        EventStream::connect(&server.path),
        Err(HerdrError::Timeout(_))
    ));
}

#[test]
fn mismatched_tags_and_truncated_or_invalid_json_are_rejected() {
    for body in [
        b"{\"event\":\"pane_focused\",\"data\":{\"type\":\"pane_closed\",\"pane_id\":\"p:1\",\"workspace_id\":\"w:1\"}}\n".as_slice(),
        b"{\"event\":\"pane_focused\"",
    ] {
        let data = body.to_vec();
        let server = Server::start(move |stream| acknowledged(stream, &data));
        let mut events = EventStream::connect(&server.path).unwrap();
        assert!(matches!(events.next_event(), Err(HerdrError::EventProtocol(_))));
    }
    let server = Server::start(|stream| acknowledged(stream, b"bad JSON\n"));
    let mut events = EventStream::connect(&server.path).unwrap();
    assert!(matches!(events.next_event(), Err(HerdrError::Json(_))));
}

#[test]
fn late_api_error_is_reported_and_future_events_remain_explicit() {
    let server = Server::start(|stream| {
        acknowledged(stream, b"{\"event\":\"future_event\",\"data\":{\"type\":\"future_event\",\"marker\":42}}\n{\"id\":\"herdr.resource-monitor:events\",\"error\":{\"code\":\"stream_failed\",\"message\":\"gone\"}}\n")
    });
    let mut events = EventStream::connect(&server.path).unwrap();
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::Unknown { kind, data }) if kind == "future_event" && data["marker"] == 42)
    );
    assert!(
        matches!(events.next_event(), Err(HerdrError::Api { code, .. }) if code == "stream_failed")
    );
}

#[test]
fn event_line_is_bounded_before_unbounded_allocation() {
    let server = Server::start(|stream| acknowledged(stream, &vec![b'x'; 1024 * 1024 + 1]));
    let mut events = EventStream::connect(&server.path).unwrap();
    assert!(matches!(
        events.next_event(),
        Err(HerdrError::OutputTooLarge)
    ));
}

#[test]
fn pane_move_preserves_old_and_new_ids() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/herdr/codex.json")).unwrap();
    let pane = fixture["snapshot"]["result"]["snapshot"]["panes"][0].clone();
    let body = format!(
        "{}\n",
        json!({"event":"pane_moved","data":{"type":"pane_moved","previous_pane_id":"p:old","previous_workspace_id":"w:old","previous_tab_id":"t:old","pane":pane}})
    );
    let server = Server::start(move |stream| acknowledged(stream, body.as_bytes()));
    let mut events = EventStream::connect(&server.path).unwrap();
    assert!(
        matches!(events.next_event().unwrap(), Some(HerdrEvent::PaneMoved { previous_pane_id, pane, .. }) if previous_pane_id == "p:old" && pane.pane_id == "p:1")
    );
}

#[test]
fn standalone_socket_resolution_uses_cli_status_without_guessing_config_paths() {
    struct Runner;
    impl CommandRunner for Runner {
        fn run(&mut self, binary: &OsStr, args: &[&str], _: Duration) -> Result<CommandOutput> {
            assert_eq!(binary, "fixture-herdr");
            assert_eq!(args, ["status", "server", "--json"]);
            Ok(CommandOutput { success:true, code:Some(0), stdout:br#"{"status":"not_running","running":false,"socket":"/custom/config/sessions/work/herdr.sock"}"#.to_vec(), stderr:vec![] })
        }
    }
    let path = CliHerdrClient::with_runner("fixture-herdr", Runner)
        .socket_path()
        .unwrap();
    assert_eq!(
        path,
        PathBuf::from("/custom/config/sessions/work/herdr.sock")
    );
}
