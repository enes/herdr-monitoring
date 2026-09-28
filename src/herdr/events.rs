//! Newline-delimited protocol-22 events over the local JSON API socket.
//!
//! Subscription names are dotted; delivered generic event/data tags use underscores.
//! `connect` returns after the acknowledgement. Take a snapshot *after* that point,
//! then consume queued events; reconnect and state reconciliation belong to the app.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::transport::{LocalStream, ShutdownHandle};
use super::{CliHerdrClient, HerdrError, PaneInfo, Result};

const REQUEST_ID: &str = "herdr.resource-monitor:events";
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LINE_BYTES: usize = 1024 * 1024;
const SUBSCRIPTIONS: &[&str] = &[
    "pane.focused",
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.moved",
    "tab.focused",
    "tab.closed",
    "workspace.focused",
    "workspace.closed",
    "layout.updated",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HerdrEvent {
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        pane: PaneInfo,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    LayoutUpdated {
        layout: Value,
    },
    #[serde(skip)]
    Unknown {
        kind: String,
        data: Value,
    },
}

/// Cloneable cancellation handle; socket shutdown immediately wakes a blocked reader.
#[derive(Clone)]
pub struct EventShutdown {
    stream: ShutdownHandle,
    cancelled: Arc<AtomicBool>,
}

impl EventShutdown {
    pub fn shutdown(&self) -> io::Result<()> {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.stream.shutdown()
    }
}

pub struct EventStream {
    reader: BufReader<LocalStream>,
    shutdown: EventShutdown,
}

impl EventStream {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let mut stream = LocalStream::connect(path, ACK_TIMEOUT)?;
        stream.set_write_timeout(Some(ACK_TIMEOUT))?;
        let shutdown = EventShutdown {
            stream: stream.shutdown_handle()?,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let request = json!({
            "id": REQUEST_ID,
            "method": "events.subscribe",
            "params": {"subscriptions": SUBSCRIPTIONS.iter()
                .map(|kind| json!({"type": kind})).collect::<Vec<_>>()},
        });
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut connection = Self {
            reader: BufReader::new(stream),
            shutdown,
        };
        let ack = connection
            .read_line(Some(Instant::now() + ACK_TIMEOUT))?
            .ok_or_else(|| {
                HerdrError::EventProtocol(
                    "connection ended before subscription acknowledgement".into(),
                )
            })?;
        let ack: Value = serde_json::from_slice(&ack)?;
        check_error(&ack)?;
        if ack.get("id").and_then(Value::as_str) != Some(REQUEST_ID)
            || ack
                .get("result")
                .and_then(|result| result.get("type"))
                .and_then(Value::as_str)
                != Some("subscription_started")
        {
            return Err(HerdrError::EventProtocol(
                "unexpected subscription acknowledgement".into(),
            ));
        }
        connection.reader.get_ref().set_read_timeout(None)?;
        Ok(connection)
    }

    pub fn shutdown_handle(&self) -> EventShutdown {
        self.shutdown.clone()
    }

    /// Blocks until an event, EOF or shutdown. It never polls current focus.
    pub fn next_event(&mut self) -> Result<Option<HerdrEvent>> {
        if self.shutdown.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let line = match self.read_line(None) {
            result if self.shutdown.cancelled.load(Ordering::Acquire) => {
                drop(result);
                return Ok(None);
            }
            result => result?,
        };
        let Some(line) = line else {
            return Ok(None);
        };
        let value: Value = serde_json::from_slice(&line)?;
        check_error(&value)?;
        let kind = value
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| HerdrError::EventProtocol("missing event tag".into()))?;
        let data = value
            .get("data")
            .ok_or_else(|| HerdrError::EventProtocol("missing event data".into()))?;
        if data.get("type").and_then(Value::as_str) != Some(kind) {
            return Err(HerdrError::EventProtocol(
                "event and data.type tags disagree".into(),
            ));
        }
        let event = match kind {
            "pane_focused" | "pane_created" | "pane_closed" | "pane_exited" | "pane_moved"
            | "tab_focused" | "tab_closed" | "workspace_focused" | "workspace_closed"
            | "layout_updated" => serde_json::from_value(data.clone())?,
            _ => HerdrEvent::Unknown {
                kind: kind.to_owned(),
                data: data.clone(),
            },
        };
        Ok(Some(event))
    }

    fn read_line(&mut self, deadline: Option<Instant>) -> Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        loop {
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(HerdrError::Timeout(ACK_TIMEOUT));
                }
                self.reader.get_ref().set_read_timeout(Some(remaining))?;
            }
            let available = match self.reader.fill_buf() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if deadline.is_some()
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                {
                    return Err(HerdrError::Timeout(ACK_TIMEOUT));
                }
                result => result?,
            };
            if available.is_empty() {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Err(HerdrError::EventProtocol(
                        "truncated JSON event line".into(),
                    ))
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            if line.len() + count > MAX_LINE_BYTES {
                return Err(HerdrError::OutputTooLarge);
            }
            line.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if newline.is_some() {
                return Ok(Some(line));
            }
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown();
    }
}

fn check_error(value: &Value) -> Result<()> {
    if let Some(error) = value.get("error") {
        #[derive(Deserialize)]
        struct ApiError {
            code: String,
            message: String,
        }
        let error: ApiError = serde_json::from_value(error.clone())?;
        return Err(HerdrError::Api {
            code: error.code,
            message: error.message,
        });
    }
    Ok(())
}

/// Plugin panes receive HERDR_SOCKET_PATH. Standalone callers delegate named
/// sessions/config defaults to the installed CLI, whose `status` exposes `socket`.
pub fn socket_path_from_env() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    CliHerdrClient::from_env().socket_path()
}
