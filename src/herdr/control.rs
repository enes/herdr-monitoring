//! Bounded raw socket requests for sampling, focus reconciliation and actions.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::transport::LocalStream;
use super::{
    AgentInfo, HerdrClient, HerdrError, PaneInfo, PaneProcessInfo, Result, SessionSnapshot,
};

pub trait PaneController: HerdrClient {
    fn open_summary(&mut self, plugin_id: &str) -> Result<()>;
    fn open_focused(&mut self, plugin_id: &str, target_pane_id: &str) -> Result<PaneInfo>;
    fn close_plugin_pane(&mut self, pane_id: &str) -> Result<()>;
}

pub struct SocketPaneController {
    socket_path: PathBuf,
    timeout: Duration,
}

const REQUEST_ID: &str = "herdr.resource-monitor:action";
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

impl SocketPaneController {
    pub fn from_env() -> Result<Self> {
        Ok(Self::with_socket(super::socket_path_from_env()?))
    }

    pub fn with_socket(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(2),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn remaining(&self, start: Instant) -> Result<Duration> {
        self.timeout
            .checked_sub(start.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(HerdrError::Timeout(self.timeout))
    }

    fn io_error(&self, error: io::Error) -> HerdrError {
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                HerdrError::Timeout(self.timeout)
            }
            _ => HerdrError::Io(error),
        }
    }

    fn request(&self, method: &str, params: Value, expected: &'static str) -> Result<Value> {
        let start = Instant::now();
        let mut stream = LocalStream::connect(&self.socket_path, self.timeout)
            .map_err(|error| self.io_error(error))?;
        let mut request = serde_json::to_vec(&json!({
            "id": REQUEST_ID, "method": method, "params": params,
        }))?;
        request.push(b'\n');
        let mut written = 0;
        while written < request.len() {
            stream.set_write_timeout(Some(self.remaining(start)?))?;
            match stream.write(&request[written..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(self.io_error(error)),
            }
        }
        let mut line = Vec::new();
        loop {
            stream.set_read_timeout(Some(self.remaining(start)?))?;
            let mut chunk = [0; 8192];
            let count = match stream.read(&mut chunk) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(self.io_error(error)),
            };
            let end = chunk[..count].iter().position(|byte| *byte == b'\n');
            let bytes = &chunk[..end.unwrap_or(count)];
            if line.len() + bytes.len() > OUTPUT_LIMIT {
                return Err(HerdrError::OutputTooLarge);
            }
            line.extend_from_slice(bytes);
            if end.is_some() {
                break;
            }
        }
        let envelope: Envelope = serde_json::from_slice(&line)?;
        if envelope.id != REQUEST_ID {
            return Err(HerdrError::UnexpectedResponse {
                expected: REQUEST_ID,
                actual: envelope.id,
            });
        }
        match (envelope.result, envelope.error) {
            (Some(result), None) => {
                let actual = result["type"].as_str().unwrap_or("<missing>");
                if actual != expected {
                    return Err(HerdrError::UnexpectedResponse {
                        expected,
                        actual: actual.into(),
                    });
                }
                Ok(result)
            }
            (None, Some(error)) => Err(HerdrError::Api {
                code: error.code,
                message: error.message,
            }),
            _ => Err(HerdrError::InvalidEnvelope),
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    id: String,
    result: Option<Value>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

impl HerdrClient for SocketPaneController {
    fn snapshot(&mut self) -> Result<SessionSnapshot> {
        let mut result = self.request("session.snapshot", json!({}), "session_snapshot")?;
        Ok(serde_json::from_value(result["snapshot"].take())?)
    }

    fn panes(&mut self) -> Result<Vec<PaneInfo>> {
        let mut result = self.request("pane.list", json!({}), "pane_list")?;
        Ok(serde_json::from_value(result["panes"].take())?)
    }

    fn agents(&mut self) -> Result<Vec<AgentInfo>> {
        let mut result = self.request("agent.list", json!({}), "agent_list")?;
        Ok(serde_json::from_value(result["agents"].take())?)
    }

    fn process_info(&mut self, pane_id: &str) -> Result<PaneProcessInfo> {
        let mut result = self.request(
            "pane.process_info",
            json!({"pane_id":pane_id}),
            "pane_process_info",
        )?;
        let process: PaneProcessInfo = serde_json::from_value(result["process_info"].take())?;
        if process.pane_id != pane_id {
            return Err(HerdrError::PaneMismatch {
                expected: pane_id.into(),
                actual: process.pane_id,
            });
        }
        Ok(process)
    }
}

impl PaneController for SocketPaneController {
    fn open_summary(&mut self, plugin_id: &str) -> Result<()> {
        self.request(
            "plugin.pane.open",
            json!({"plugin_id":plugin_id,"entrypoint":"summary","placement":"popup","focus":false}),
            "ok",
        )?;
        Ok(())
    }

    fn open_focused(&mut self, plugin_id: &str, target_pane_id: &str) -> Result<PaneInfo> {
        #[derive(Deserialize)]
        struct Opened {
            plugin_id: String,
            entrypoint: String,
            pane: PaneInfo,
        }
        let mut result = self.request(
            "plugin.pane.open",
            json!({"plugin_id":plugin_id,"entrypoint":"focused","placement":"split",
                "target_pane_id":target_pane_id,"direction":"right","focus":false}),
            "plugin_pane_opened",
        )?;
        let opened: Opened = serde_json::from_value(result["plugin_pane"].take())?;
        if opened.plugin_id != plugin_id || opened.entrypoint != "focused" {
            return Err(HerdrError::UnexpectedResponse {
                expected: "requested plugin's focused entrypoint",
                actual: format!("{}.{}", opened.plugin_id, opened.entrypoint),
            });
        }
        Ok(opened.pane)
    }

    fn close_plugin_pane(&mut self, pane_id: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Closed {
            pane_id: String,
        }
        let result = self.request(
            "plugin.pane.close",
            json!({"pane_id":pane_id}),
            "plugin_pane_closed",
        )?;
        let closed: Closed = serde_json::from_value(result)?;
        if closed.pane_id != pane_id {
            return Err(HerdrError::PaneMismatch {
                expected: pane_id.into(),
                actual: closed.pane_id,
            });
        }
        Ok(())
    }
}
