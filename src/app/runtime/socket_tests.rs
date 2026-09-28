use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;

use serde_json::{json, Value};

#[path = "../../../tests/support/local_transport.rs"]
mod local_transport;
use local_transport::{endpoint, LocalListener, LocalStream};

use super::*;
use crate::herdr::{PaneInfo, SessionSnapshot};

struct FakeServer {
    path: PathBuf,
    snapshot: Arc<Mutex<SessionSnapshot>>,
    requests: Arc<Mutex<Vec<String>>>,
    subscriptions: mpsc::Receiver<LocalStream>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    fn new() -> Self {
        let path = endpoint("reconnect");
        let listener = LocalListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let snapshot = Arc::new(Mutex::new(SessionSnapshot {
            focused_pane_id: Some("A".into()),
            panes: ["A", "B", "monitor"]
                .into_iter()
                .map(|id| PaneInfo {
                    pane_id: id.into(),
                    terminal_id: format!("terminal-{id}"),
                    workspace_id: "workspace".into(),
                    tab_id: "tab".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }));
        let state = Arc::clone(&snapshot);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let (sender, subscriptions) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let state = Arc::clone(&state);
                        let recorded = Arc::clone(&recorded);
                        let sender = sender.clone();
                        handlers.push(thread::spawn(move || {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let mut line = String::new();
                            BufReader::new(&mut stream).read_line(&mut line).unwrap();
                            let request: Value = serde_json::from_str(&line).unwrap();
                            let method = request["method"].as_str().unwrap();
                            recorded.lock().unwrap().push(method.into());
                            let result = match method {
                                "events.subscribe" => json!({"type":"subscription_started"}),
                                "session.snapshot" => json!({
                                    "type":"session_snapshot", "snapshot":*state.lock().unwrap()
                                }),
                                "pane.process_info" => json!({
                                    "type":"pane_process_info",
                                    "process_info":{"pane_id":request["params"]["pane_id"]}
                                }),
                                unexpected => panic!("unexpected request: {unexpected}"),
                            };
                            writeln!(stream, "{}", json!({"id":request["id"], "result":result}))
                                .unwrap();
                            if method == "events.subscribe" {
                                sender.send(stream).unwrap();
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            path,
            snapshot,
            requests,
            subscriptions,
            stop,
            thread: Some(thread),
        }
    }

    fn subscription(&self) -> LocalStream {
        self.subscriptions
            .recv_timeout(Duration::from_secs(4))
            .unwrap()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        let _ = std::fs::remove_file(&self.path);
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn event(stream: &mut LocalStream, kind: &str, pane_id: &str) {
    writeln!(
        stream,
        "{}",
        json!({
            "event":kind,
            "data":{"type":kind,"pane_id":pane_id,"workspace_id":"workspace","tab_id":"tab"}
        })
    )
    .unwrap();
}

fn receive_until(worker: &MonitorWorker, check: impl Fn(&MonitorUpdate) -> bool) -> MonitorUpdate {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let update = worker
            .updates
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("worker did not publish the expected update");
        if check(&update) {
            return update;
        }
    }
}

fn target(worker: &MonitorWorker, expected: Option<&str>) -> u64 {
    match receive_until(worker, |update| {
        matches!(update,
        MonitorUpdate::Target {pane_id, ..} if pane_id.as_deref() == expected)
    }) {
        MonitorUpdate::Target { generation, .. } => generation,
        _ => unreachable!(),
    }
}

#[test]
fn real_worker_prefers_launch_target_over_snapshot_then_follows_focus_events() {
    let server = FakeServer::new();
    let worker = MonitorWorker::with_socket(
        Mode::Focused,
        LaunchContext {
            monitor_pane_id: Some("monitor".into()),
            target_pane_id: Some("B".into()),
            monitor_pid: std::process::id(),
        },
        server.path.clone(),
    );
    let mut subscription = server.subscription();
    let initial = receive_until(&worker, |update| {
        matches!(
            update,
            MonitorUpdate::Target {
                pane_id: Some(_),
                ..
            }
        )
    });
    let MonitorUpdate::Target {
        generation: initial_generation,
        pane_id,
    } = initial
    else {
        unreachable!()
    };
    assert_eq!(
        pane_id.as_deref(),
        Some("B"),
        "the pane that opened the monitor must win over another client's snapshot focus"
    );

    // The captured origin selects only the initial target; actual later focus
    // events still switch the monitor even if the snapshot has not changed.
    event(&mut subscription, "pane_focused", "A");
    assert!(target(&worker, Some("A")) > initial_generation);
}

#[test]
fn real_worker_ignores_other_tab_focus_and_reconnect_snapshot() {
    let server = FakeServer::new();
    {
        let mut state = server.snapshot.lock().unwrap();
        state.panes.push(PaneInfo {
            pane_id: "other".into(),
            terminal_id: "terminal-other".into(),
            workspace_id: "workspace".into(),
            tab_id: "other-tab".into(),
            ..Default::default()
        });
        state.focused_pane_id = Some("other".into());
    }
    let worker = MonitorWorker::with_socket(
        Mode::Focused,
        LaunchContext {
            monitor_pane_id: Some("monitor".into()),
            target_pane_id: Some("B".into()),
            monitor_pid: std::process::id(),
        },
        server.path.clone(),
    );
    let mut subscription = server.subscription();
    target(&worker, Some("B"));

    event(&mut subscription, "pane_focused", "other");
    event(&mut subscription, "pane_focused", "monitor");
    // The local focus event is a barrier after the foreign focus notifications.
    event(&mut subscription, "pane_focused", "A");
    receive_until(&worker, |update| {
        if let MonitorUpdate::Target { pane_id, .. } = update {
            assert_ne!(pane_id.as_deref(), Some("other"));
        }
        matches!(update, MonitorUpdate::Target { pane_id: Some(id), .. } if id == "A")
    });

    subscription.shutdown(Shutdown::Both).unwrap();
    receive_until(&worker, |update| {
        matches!(update, MonitorUpdate::Status(Some(message))
        if message.starts_with("Reconnecting to Herdr:"))
    });
    let reconnected = server.subscription();
    receive_until(&worker, |update| {
        if let MonitorUpdate::Target { pane_id, .. } = update {
            assert_ne!(pane_id.as_deref(), Some("other"));
        }
        matches!(update, MonitorUpdate::Target { pane_id: Some(id), .. } if id == "A")
    });
    drop(worker);
    drop(reconnected);
}

#[test]
fn real_worker_reconnects_reacks_and_rebootstraps_a_reused_closed_pane_id() {
    let server = FakeServer::new();
    let worker = MonitorWorker::with_socket(
        Mode::Focused,
        LaunchContext {
            monitor_pane_id: Some("monitor".into()),
            monitor_pid: std::process::id(),
            ..Default::default()
        },
        server.path.clone(),
    );
    let mut subscription = server.subscription();
    let initial_generation = target(&worker, Some("A"));
    assert_eq!(server.requests.lock().unwrap()[0], "events.subscribe");

    // Focus B despite a different client's stale snapshot A, then click the
    // monitor using Herdr's complete workspace/tab/pane focus event sequence.
    event(&mut subscription, "pane_focused", "B");
    let b_generation = target(&worker, Some("B"));
    for kind in ["workspace_focused", "tab_focused", "pane_focused"] {
        event(&mut subscription, kind, "monitor");
    }
    // A lifecycle event acts as a barrier after the triplet on the same stream.
    event(&mut subscription, "pane_closed", "B");
    target(&worker, None);
    let selected = target(&worker, Some("A"));
    assert!(selected > b_generation && b_generation > initial_generation);

    // Closure is observed while the old snapshot still lists A. Reconnect
    // must not carry its closed-ID tombstone into a replacement terminal.
    event(&mut subscription, "pane_closed", "A");
    target(&worker, None);
    subscription.shutdown(Shutdown::Both).unwrap();
    receive_until(&worker, |update| {
        matches!(update, MonitorUpdate::Status(Some(message))
        if message.starts_with("Reconnecting to Herdr:"))
    });
    {
        let mut state = server.snapshot.lock().unwrap();
        state.panes[0].terminal_id = "replacement-terminal-A".into();
        state.focused_pane_id = Some("A".into());
    }
    let reconnected = server.subscription();
    let replacement_generation = target(&worker, Some("A"));
    assert!(replacement_generation > selected);
    let sample = receive_until(&worker, |update| {
        matches!(update,
        MonitorUpdate::Sample {generation, result: Ok(sample)}
            if *generation == replacement_generation && !sample.panes.is_empty())
    });
    let MonitorUpdate::Sample {
        result: Ok(sample), ..
    } = sample
    else {
        unreachable!()
    };
    assert_eq!(
        sample.panes[0].target.pane.terminal_id,
        "replacement-terminal-A"
    );
    let requests = server.requests.lock().unwrap();
    let acknowledgements: Vec<_> = requests
        .iter()
        .enumerate()
        .filter(|(_, method)| method.as_str() == "events.subscribe")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(acknowledgements.len(), 2);
    assert_eq!(requests[acknowledgements[1] + 1], "session.snapshot");
    drop(requests);
    drop(worker);
    drop(reconnected);
}
