use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{AgentInfo, PaneInfo, PaneProcessInfo, SessionSnapshot};

pub type Result<T> = std::result::Result<T, HerdrError>;

#[derive(Debug)]
pub enum HerdrError {
    Io(io::Error),
    Json(serde_json::Error),
    Timeout(Duration),
    OutputTooLarge,
    CommandFailed {
        code: Option<i32>,
        message: String,
    },
    Api {
        code: String,
        message: String,
    },
    UnexpectedResponse {
        expected: &'static str,
        actual: String,
    },
    InvalidEnvelope,
    EventProtocol(String),
    MissingPane(String),
    PaneMismatch {
        expected: String,
        actual: String,
    },
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Herdr command I/O: {error}"),
            Self::Json(error) => write!(f, "Invalid Herdr JSON: {error}"),
            Self::Timeout(timeout) => write!(
                f,
                "Herdr command timed out after {:.1}s",
                timeout.as_secs_f64()
            ),
            Self::OutputTooLarge => f.write_str("Herdr command output exceeded the size limit"),
            Self::CommandFailed { code, message } => {
                write!(f, "Herdr command failed ({code:?}): {message}")
            }
            Self::Api { code, message } => write!(f, "Herdr API {code}: {message}"),
            Self::UnexpectedResponse { expected, actual } => {
                write!(f, "Expected Herdr {expected}, received {actual}")
            }
            Self::InvalidEnvelope => {
                f.write_str("Herdr response must contain exactly one result or error")
            }
            Self::EventProtocol(message) => write!(f, "Herdr event protocol: {message}"),
            Self::MissingPane(pane) => write!(f, "Pane {pane} is absent from the snapshot"),
            Self::PaneMismatch { expected, actual } => write!(
                f,
                "Herdr pane mismatch: expected {expected}, received {actual}"
            ),
        }
    }
}

impl std::error::Error for HerdrError {}

impl From<io::Error> for HerdrError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for HerdrError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// These calls only read current state. No event subscription or background polling.
pub trait HerdrClient {
    fn snapshot(&mut self) -> Result<SessionSnapshot>;
    fn panes(&mut self) -> Result<Vec<PaneInfo>>;
    fn agents(&mut self) -> Result<Vec<AgentInfo>>;
    fn process_info(&mut self, pane_id: &str) -> Result<PaneProcessInfo>;
}

#[derive(Debug)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandRunner {
    fn run(&mut self, binary: &OsStr, args: &[&str], timeout: Duration) -> Result<CommandOutput>;
}

#[derive(Debug, Default)]
pub struct ProcessCommandRunner;

const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

fn read_output(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(OUTPUT_LIMIT + 1).read_to_end(&mut data)?;
    Ok(data)
}

impl CommandRunner for ProcessCommandRunner {
    fn run(&mut self, binary: &OsStr, args: &[&str], timeout: Duration) -> Result<CommandOutput> {
        let started = Instant::now();
        // Pass argv directly and inherit Herdr's session/socket environment.
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (sender, receiver) = mpsc::channel();
        let out_sender = sender.clone();
        std::thread::spawn(move || {
            let _ = out_sender.send((true, read_output(stdout)));
        });
        std::thread::spawn(move || {
            let _ = sender.send((false, read_output(stderr)));
        });
        let mut out = None;
        let mut err = None;
        let result = (|| loop {
            while let Ok((is_stdout, data)) = receiver.try_recv() {
                let data = data?;
                if data.len() as u64 > OUTPUT_LIMIT {
                    return Err(HerdrError::OutputTooLarge);
                }
                if is_stdout {
                    out = Some(data);
                } else {
                    err = Some(data);
                }
            }
            if let Some(status) = child.try_wait()? {
                if out.is_some() && err.is_some() {
                    return Ok(CommandOutput {
                        success: status.success(),
                        code: status.code(),
                        stdout: out.take().unwrap(),
                        stderr: err.take().unwrap(),
                    });
                }
            }
            if started.elapsed() >= timeout {
                return Err(HerdrError::Timeout(timeout));
            }
            std::thread::sleep(Duration::from_millis(5));
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

pub struct CliHerdrClient<R = ProcessCommandRunner> {
    binary: OsString,
    runner: R,
    timeout: Duration,
}

impl CliHerdrClient<ProcessCommandRunner> {
    pub fn from_env() -> Self {
        let binary = std::env::var_os("HERDR_BIN_PATH")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("herdr"));
        Self::with_runner(binary, ProcessCommandRunner)
    }
}

impl<R: CommandRunner> CliHerdrClient<R> {
    pub fn with_runner(binary: impl Into<OsString>, runner: R) -> Self {
        Self {
            binary: binary.into(),
            runner,
            timeout: Duration::from_secs(2),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Ask Herdr to resolve its own config and named-session socket defaults.
    pub fn socket_path(&mut self) -> Result<std::path::PathBuf> {
        let output =
            self.runner
                .run(&self.binary, &["status", "server", "--json"], self.timeout)?;
        if !output.success {
            return Err(HerdrError::CommandFailed {
                code: output.code,
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        #[derive(Deserialize)]
        struct Status {
            socket: String,
        }
        let status: Status = serde_json::from_slice(&output.stdout)?;
        if status.socket.is_empty() {
            return Err(HerdrError::EventProtocol("empty API socket path".into()));
        }
        Ok(status.socket.into())
    }

    fn request(&mut self, args: &[&str], expected: &'static str) -> Result<serde_json::Value> {
        let output = self.runner.run(&self.binary, args, self.timeout)?;
        if !output.success {
            // Herdr prints API error envelopes to stderr and exits 1.
            for bytes in [&output.stderr, &output.stdout] {
                if let Ok(envelope) = serde_json::from_slice::<Envelope>(bytes) {
                    if let Some(error) = envelope.error {
                        return Err(HerdrError::Api {
                            code: error.code,
                            message: error.message,
                        });
                    }
                }
            }
            return Err(HerdrError::CommandFailed {
                code: output.code,
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let envelope: Envelope = serde_json::from_slice(&output.stdout)?;
        match (envelope.result, envelope.error) {
            (Some(result), None) => {
                let actual = result
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<missing>");
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
    #[serde(rename = "id")]
    _id: String,
    result: Option<serde_json::Value>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

impl<R: CommandRunner> HerdrClient for CliHerdrClient<R> {
    fn snapshot(&mut self) -> Result<SessionSnapshot> {
        #[derive(Deserialize)]
        struct Body {
            snapshot: SessionSnapshot,
        }
        let body: Body =
            serde_json::from_value(self.request(&["api", "snapshot"], "session_snapshot")?)?;
        Ok(body.snapshot)
    }

    fn panes(&mut self) -> Result<Vec<PaneInfo>> {
        #[derive(Deserialize)]
        struct Body {
            panes: Vec<PaneInfo>,
        }
        let body: Body = serde_json::from_value(self.request(&["pane", "list"], "pane_list")?)?;
        Ok(body.panes)
    }

    fn agents(&mut self) -> Result<Vec<AgentInfo>> {
        #[derive(Deserialize)]
        struct Body {
            agents: Vec<AgentInfo>,
        }
        let body: Body = serde_json::from_value(self.request(&["agent", "list"], "agent_list")?)?;
        Ok(body.agents)
    }

    fn process_info(&mut self, pane_id: &str) -> Result<PaneProcessInfo> {
        #[derive(Deserialize)]
        struct Body {
            process_info: PaneProcessInfo,
        }
        let body: Body = serde_json::from_value(self.request(
            &["pane", "process-info", "--pane", pane_id],
            "pane_process_info",
        )?)?;
        if body.process_info.pane_id != pane_id {
            return Err(HerdrError::PaneMismatch {
                expected: pane_id.to_owned(),
                actual: body.process_info.pane_id,
            });
        }
        Ok(body.process_info)
    }
}
