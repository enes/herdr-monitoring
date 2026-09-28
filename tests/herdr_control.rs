#![cfg(any(unix, windows))]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use herdr_resource_monitor::herdr::{
    HerdrClient, HerdrError, PaneController, PaneInfo, SocketPaneController,
};
use serde_json::{json, Value};

#[path = "support/local_transport.rs"]
mod local_transport;
use local_transport::{endpoint, LocalListener, LocalStream};

const REQUEST_ID: &str = "herdr.resource-monitor:action";
const PLUGIN_ID: &str = "herdr.resource-monitor";

struct Server {
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
    _transport_guard: local_transport::KeepAlive,
}

impl Server {
    fn start(handler: impl FnOnce(LocalStream) + Send + 'static) -> Self {
        let path = endpoint("ctl");
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

    fn reply(method: &'static str, params: Value, response: Value) -> Self {
        Self::start(move |mut stream| {
            assert_eq!(
                request(&mut stream),
                json!({"id": REQUEST_ID, "method": method, "params": params})
            );
            // Socket reads need not preserve the response frame boundaries.
            let response = format!("{response}\n");
            for fragment in response.as_bytes().chunks(13) {
                stream.write_all(fragment).unwrap();
            }
        })
    }

    fn client(&self) -> SocketPaneController {
        SocketPaneController::with_socket(&self.path)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let result = self.thread.take().unwrap().join();
        let _ = std::fs::remove_file(&self.path);
        if !thread::panicking() {
            result.expect("fake control server failed");
        }
    }
}

fn request(stream: &mut LocalStream) -> Value {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn response(result: Value) -> Value {
    json!({"id": REQUEST_ID, "result": result})
}

fn summary_params() -> Value {
    json!({"plugin_id": PLUGIN_ID, "entrypoint": "summary", "placement": "popup", "focus": false})
}

fn focused_params() -> Value {
    json!({"plugin_id": PLUGIN_ID, "entrypoint": "focused", "placement": "split",
        "target_pane_id": "w1:p1", "direction": "right", "focus": false})
}

fn pane() -> PaneInfo {
    PaneInfo {
        pane_id: "w1:p2".into(),
        terminal_id: "term-2".into(),
        workspace_id: "w1".into(),
        tab_id: "w1:t1".into(),
        label: Some("Resource Details".into()),
        ..Default::default()
    }
}

#[test]
fn summary_opens_popup_without_focus_or_pane_id() {
    let server = Server::reply(
        "plugin.pane.open",
        summary_params(),
        response(json!({"type": "ok"})),
    );
    assert_eq!(server.client().socket_path(), server.path);
    server.client().open_summary(PLUGIN_ID).unwrap();
}

#[test]
fn focused_opens_right_split_at_explicit_target_without_focus() {
    let server = Server::reply(
        "plugin.pane.open",
        focused_params(),
        response(json!({"type": "plugin_pane_opened", "plugin_pane": {
            "plugin_id": PLUGIN_ID, "entrypoint": "focused", "pane": pane()
        }})),
    );
    assert_eq!(
        server.client().open_focused(PLUGIN_ID, "w1:p1").unwrap(),
        pane()
    );
}

#[test]
fn focused_open_rejects_another_plugin_or_entrypoint() {
    for (plugin_id, entrypoint) in [("another.plugin", "focused"), (PLUGIN_ID, "summary")] {
        let server = Server::reply(
            "plugin.pane.open",
            focused_params(),
            response(json!({"type": "plugin_pane_opened", "plugin_pane": {
                "plugin_id": plugin_id, "entrypoint": entrypoint, "pane": pane()
            }})),
        );
        assert!(matches!(
            server.client().open_focused(PLUGIN_ID, "w1:p1"),
            Err(HerdrError::UnexpectedResponse { .. })
        ));
    }
}

#[test]
fn close_uses_plugin_endpoint_and_checks_returned_pane() {
    for returned_id in ["w1:p2", "w1:p99"] {
        let server = Server::reply(
            "plugin.pane.close",
            json!({"pane_id": "w1:p2"}),
            response(json!({"type": "plugin_pane_closed", "pane_id": returned_id})),
        );
        let result = server.client().close_plugin_pane("w1:p2");
        if returned_id == "w1:p2" {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(HerdrError::PaneMismatch { expected, actual })
                if expected == "w1:p2" && actual == "w1:p99")
            );
        }
    }
}

#[test]
fn read_methods_decode_existing_fixtures_and_preserve_pane_identity() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/herdr/codex.json")).unwrap();
    let server = Server::reply(
        "session.snapshot",
        json!({}),
        response(fixture["snapshot"]["result"].clone()),
    );
    let snapshot = server.client().snapshot().unwrap();
    let pane_id = snapshot.focused_pane_id.as_deref().unwrap();

    let server = Server::reply(
        "pane.list",
        json!({}),
        response(fixture["panes"]["result"].clone()),
    );
    assert_eq!(server.client().panes().unwrap(), snapshot.panes);
    let server = Server::reply(
        "agent.list",
        json!({}),
        response(fixture["agents"]["result"].clone()),
    );
    assert_eq!(server.client().agents().unwrap(), snapshot.agents);
    let server = Server::reply(
        "pane.process_info",
        json!({"pane_id": pane_id}),
        response(fixture["process_info"]["result"].clone()),
    );
    let process = server.client().process_info(pane_id).unwrap();
    let target = snapshot.target(pane_id, process).unwrap();
    assert_eq!(target.pane_id, pane_id);
    assert_eq!(target.agent.unwrap().agent.as_deref(), Some("codex"));
    assert_eq!(target.process.foreground_processes.len(), 1);
    let server = Server::reply(
        "pane.process_info",
        json!({"pane_id": "p:wrong"}),
        response(fixture["process_info"]["result"].clone()),
    );
    assert!(matches!(
        server.client().process_info("p:wrong"),
        Err(HerdrError::PaneMismatch { expected, actual })
            if expected == "p:wrong" && actual == pane_id
    ));
}

#[test]
fn popup_busy_is_reported_with_original_api_code_and_message() {
    let server = Server::reply(
        "plugin.pane.open",
        summary_params(),
        json!({"id": REQUEST_ID, "error": {
            "code": "ui_busy", "message": "a popup pane is already open"
        }}),
    );
    assert!(matches!(server.client().open_summary(PLUGIN_ID),
        Err(HerdrError::Api { code, message })
        if code == "ui_busy" && message == "a popup pane is already open"));
}

#[test]
fn wrong_response_id_or_type_is_rejected() {
    for reply in [
        json!({"id": "another-request", "result": {"type": "ok"}}),
        response(json!({"type": "plugin_pane_closed", "pane_id": "w1:p2"})),
    ] {
        let server = Server::reply("plugin.pane.open", summary_params(), reply);
        assert!(matches!(
            server.client().open_summary(PLUGIN_ID),
            Err(HerdrError::UnexpectedResponse { .. })
        ));
    }
}

#[test]
fn envelope_requires_exactly_one_result_or_error() {
    for reply in [
        json!({"id": REQUEST_ID}),
        json!({"id": REQUEST_ID, "result": {"type": "ok"}, "error": {
            "code": "ui_busy", "message": "already open"
        }}),
    ] {
        let server = Server::reply("plugin.pane.open", summary_params(), reply);
        assert!(matches!(
            server.client().open_summary(PLUGIN_ID),
            Err(HerdrError::InvalidEnvelope)
        ));
    }
}

#[test]
fn malformed_or_truncated_json_is_rejected() {
    let server = Server::start(|mut stream| {
        request(&mut stream);
        stream.write_all(b"not JSON\n").unwrap();
    });
    assert!(matches!(
        server.client().open_summary(PLUGIN_ID),
        Err(HerdrError::Json(_))
    ));
    let server = Server::start(|mut stream| {
        request(&mut stream);
        stream.write_all(b"{\"id\":").unwrap();
    });
    assert!(matches!(server.client().open_summary(PLUGIN_ID),
        Err(HerdrError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof));
}

#[test]
fn absent_response_and_slow_drip_both_obey_total_deadline() {
    for slow_drip in [false, true] {
        let server = Server::start(move |mut stream| {
            request(&mut stream);
            if slow_drip {
                while stream.write_all(b" ").is_ok() {
                    thread::sleep(Duration::from_millis(10));
                }
            } else {
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
            }
        });
        let start = Instant::now();
        assert!(matches!(
            server
                .client()
                .with_timeout(Duration::from_millis(50))
                .open_summary(PLUGIN_ID),
            Err(HerdrError::Timeout(_))
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

#[test]
fn oversized_response_is_bounded_before_json_decode() {
    let server = Server::start(|mut stream| {
        request(&mut stream);
        // Exceed the limit before sending a delimiter or valid JSON.
        let _ = stream.write_all(&vec![b'x'; 8 * 1024 * 1024 + 1]);
    });
    assert!(matches!(
        server.client().open_summary(PLUGIN_ID),
        Err(HerdrError::OutputTooLarge)
    ));
}
