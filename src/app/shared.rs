//! Shared agent servers that run session work outside every pane's tree.
//!
//! Codex 0.158 TUIs hand their sessions to one app-server daemon, and opencode
//! 2.x TUIs attach to one background service. A server is found only through
//! the PID in its own state record, then verified against the live process, so
//! a stale record or reused PID is never presented as that server.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::herdr::PaneTarget;
use crate::metrics::ProcessMetrics;

const RECORD_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SharedServerKind {
    CodexAppServer,
    OpencodeService,
}

impl SharedServerKind {
    const ALL: [Self; 2] = [Self::CodexAppServer, Self::OpencodeService];

    pub fn label(self) -> &'static str {
        match self {
            Self::CodexAppServer => "Codex app-server",
            Self::OpencodeService => "opencode service",
        }
    }

    pub fn agent(self) -> &'static str {
        match self {
            Self::CodexAppServer => "codex",
            Self::OpencodeService => "opencode",
        }
    }

    /// Flags that keep a session out of this server: a session inside the
    /// TUI's own process tree, or a server at another endpoint.
    fn opt_outs(self) -> [&'static str; 2] {
        match self {
            Self::CodexAppServer => ["--no-daemon", "--remote"],
            Self::OpencodeService => ["--standalone", "--server"],
        }
    }

    /// The server a target may use: Herdr recognizes its agent type and its
    /// command line does not opt out. This selects context only; it is not
    /// evidence that the session runs there, and no usage is attributed.
    pub(super) fn for_target(target: &PaneTarget) -> Option<Self> {
        let agent = target.agent.as_ref()?.agent.as_deref()?;
        let kind = Self::ALL.into_iter().find(|kind| kind.agent() == agent)?;
        let opted_out = target
            .process
            .foreground_processes
            .iter()
            .flat_map(|process| process.argv.iter().flatten())
            .any(|arg| {
                kind.opt_outs().iter().any(|flag| {
                    arg == flag
                        || arg
                            .strip_prefix(flag)
                            .is_some_and(|rest| rest.starts_with('='))
                })
            });
        (!opted_out).then_some(kind)
    }

    /// A recorded PID must still run this server's executable and subcommand.
    pub fn matches(self, process: &ProcessMetrics) -> bool {
        let (executable, subcommand) = match self {
            Self::CodexAppServer => ("codex", "app-server"),
            Self::OpencodeService => ("opencode", "serve"),
        };
        let name = process.name.to_ascii_lowercase();
        name.strip_suffix(".exe").unwrap_or(&name) == executable
            && process
                .cmdline
                .split_whitespace()
                .any(|arg| arg == subcommand)
    }
}

/// Recorded PIDs of one server, or why its state record cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SharedServerRecord {
    pub kind: SharedServerKind,
    pub pids: Result<Vec<u32>, String>,
}

/// State record locations. The default reads nothing.
#[derive(Clone, Debug, Default)]
pub struct SharedServerFiles {
    codex_home: Option<PathBuf>,
    state_home: Option<PathBuf>,
}

impl SharedServerFiles {
    /// `CODEX_HOME` or `~/.codex`, and `XDG_STATE_HOME` or `~/.local/state`.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var_os(key))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        let path = |key: &str| {
            lookup(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = path("HOME").or_else(|| path("USERPROFILE"));
        Self {
            codex_home: path("CODEX_HOME")
                .or_else(|| home.as_ref().map(|home| home.join(".codex"))),
            state_home: path("XDG_STATE_HOME")
                .or_else(|| home.map(|home| home.join(".local").join("state"))),
        }
    }

    pub fn new(codex_home: impl Into<PathBuf>, state_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: Some(codex_home.into()),
            state_home: Some(state_home.into()),
        }
    }

    fn record_paths(&self, kind: SharedServerKind) -> Vec<PathBuf> {
        match kind {
            // The updater is the daemon's parent; both belong to the server.
            SharedServerKind::CodexAppServer => self
                .codex_home
                .iter()
                .flat_map(|home| {
                    ["daemon-updater.pid", "daemon.pid"]
                        .map(|name| home.join("app-server-daemon").join(name))
                })
                .collect(),
            SharedServerKind::OpencodeService => self
                .state_home
                .iter()
                .map(|state| state.join("opencode").join("service.json"))
                .collect(),
        }
    }

    /// One record per server with state. A missing record means that server is
    /// not running; an unreadable or unrecognized record is an explicit error.
    pub(super) fn read(&self) -> Vec<SharedServerRecord> {
        SharedServerKind::ALL
            .into_iter()
            .filter_map(|kind| self.read_one(kind))
            .collect()
    }

    pub(super) fn read_one(&self, kind: SharedServerKind) -> Option<SharedServerRecord> {
        let mut pids = Vec::new();
        for path in self.record_paths(kind) {
            match read_pid(&path) {
                Ok(Some(pid)) => pids.push(pid),
                Ok(None) => {}
                Err(error) => {
                    return Some(SharedServerRecord {
                        kind,
                        pids: Err(error),
                    })
                }
            }
        }
        (!pids.is_empty()).then_some(SharedServerRecord {
            kind,
            pids: Ok(pids),
        })
    }
}

/// Only `pid` is decoded. Other fields, such as opencode's service password,
/// are skipped by the decoder and never stored.
#[derive(Deserialize)]
struct PidRecord {
    pid: u32,
}

fn read_pid(path: &Path) -> Result<Option<u32>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(format!("{} exceeds the expected size", path.display()));
    }
    serde_json::from_slice::<PidRecord>(&bytes)
        .map(|record| Some(record.pid))
        .map_err(|error| format!("unrecognized state in {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);

    impl Directory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("herdr-shared-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(path.join("codex/app-server-daemon")).unwrap();
            std::fs::create_dir_all(path.join("state/opencode")).unwrap();
            Self(path)
        }

        fn files(&self) -> SharedServerFiles {
            SharedServerFiles::new(self.0.join("codex"), self.0.join("state"))
        }

        fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
            std::fs::write(self.0.join(relative), contents).unwrap();
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn process(name: &str, cmdline: &str) -> ProcessMetrics {
        ProcessMetrics {
            pid: 1,
            ppid: 0,
            name: name.into(),
            cmdline: cmdline.into(),
            children: vec![],
            cpu_percent: None,
            rss_bytes: None,
        }
    }

    #[test]
    fn records_decode_only_the_pid_from_each_state_file() {
        let directory = Directory::new("records");
        // Shapes observed from codex 0.158.0 and opencode 2.0.12.
        directory.write(
            "codex/app-server-daemon/daemon-updater.pid",
            r#"{"pid":40,"processStartTime":"x","processIdentity":{"startSeconds":1}}"#,
        );
        directory.write(
            "codex/app-server-daemon/daemon.pid",
            r#"{"pid":41,"processIdentity":{"bootId":"b"},"executableIdentity":{"digest":[1]}}"#,
        );
        directory.write(
            "state/opencode/service.json",
            r#"{"id":"i","version":"2.0.12","url":"http://127.0.0.1:1","pid":50,"password":"fixture-secret"}"#,
        );
        let records = directory.files().read();
        assert_eq!(
            records,
            vec![
                SharedServerRecord {
                    kind: SharedServerKind::CodexAppServer,
                    pids: Ok(vec![40, 41]),
                },
                SharedServerRecord {
                    kind: SharedServerKind::OpencodeService,
                    pids: Ok(vec![50]),
                },
            ]
        );
        assert!(!format!("{records:?}").contains("fixture-secret"));
    }

    #[test]
    fn missing_state_means_not_running_and_unusable_state_is_an_error() {
        let directory = Directory::new("unusable");
        assert!(directory.files().read().is_empty());
        assert!(SharedServerFiles::default().read().is_empty());

        directory.write("codex/app-server-daemon/daemon.pid", r#"{"pid":41}"#);
        let records = directory.files().read();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].pids,
            Ok(vec![41]),
            "the updater record is optional"
        );

        directory.write("state/opencode/service.json", "not json");
        directory.write(
            "codex/app-server-daemon/daemon-updater.pid",
            r#"{"pid":"forty"}"#,
        );
        let records = directory.files().read();
        for (record, kind) in records.iter().zip(SharedServerKind::ALL) {
            assert_eq!(record.kind, kind);
            assert!(record
                .pids
                .as_ref()
                .unwrap_err()
                .contains("unrecognized state"));
        }

        directory.write(
            "state/opencode/service.json",
            vec![b' '; RECORD_LIMIT as usize + 1],
        );
        let records = directory.files().read();
        assert!(records[1]
            .pids
            .as_ref()
            .unwrap_err()
            .contains("exceeds the expected size"));
    }

    #[test]
    fn locations_follow_overrides_and_fall_back_to_the_home_directory() {
        let lookup = |vars: &'static [(&'static str, &'static str)]| {
            SharedServerFiles::from_lookup(move |key| {
                vars.iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            })
        };
        let defaults = lookup(&[("HOME", "/home/me"), ("CODEX_HOME", "")]);
        assert_eq!(defaults.codex_home, Some(PathBuf::from("/home/me/.codex")));
        assert_eq!(
            defaults.state_home,
            Some(Path::new("/home/me").join(".local").join("state"))
        );
        let overrides = lookup(&[
            ("HOME", "/home/me"),
            ("CODEX_HOME", "/codex"),
            ("XDG_STATE_HOME", "/state"),
        ]);
        assert_eq!(overrides.codex_home, Some(PathBuf::from("/codex")));
        assert_eq!(overrides.state_home, Some(PathBuf::from("/state")));
        let windows = lookup(&[("USERPROFILE", r"C:\Users\me")]);
        assert_eq!(
            windows.codex_home,
            Some(Path::new(r"C:\Users\me").join(".codex"))
        );
        let none = lookup(&[]);
        assert!(none.codex_home.is_none() && none.state_home.is_none());
    }

    fn target(agent: Option<&str>, argv: &[&str]) -> PaneTarget {
        use crate::herdr::{correlate, ForegroundProcess, PaneInfo, PaneProcessInfo};
        let pane = PaneInfo {
            pane_id: "pane".into(),
            agent: agent.map(Into::into),
            ..Default::default()
        };
        let process = PaneProcessInfo {
            pane_id: "pane".into(),
            foreground_processes: vec![ForegroundProcess {
                pid: 10,
                argv: Some(argv.iter().map(|arg| arg.to_string()).collect()),
                ..Default::default()
            }],
            ..Default::default()
        };
        correlate(&pane, None, process).unwrap()
    }

    #[test]
    fn target_uses_its_agent_server_unless_its_command_line_opts_out() {
        let kind = |agent, argv| SharedServerKind::for_target(&target(agent, argv));
        assert_eq!(
            kind(Some("codex"), &["codex"]),
            Some(SharedServerKind::CodexAppServer)
        );
        assert_eq!(
            kind(Some("opencode"), &["opencode", "-c"]),
            Some(SharedServerKind::OpencodeService)
        );
        // A longer flag that merely shares a prefix is not an opt-out.
        assert_eq!(
            kind(
                Some("codex"),
                &["codex", "--remote-auth-token-env", "TOKEN"]
            ),
            Some(SharedServerKind::CodexAppServer)
        );
        for (agent, argv) in [
            (Some("codex"), &["codex", "--no-daemon"][..]),
            (Some("codex"), &["codex", "--remote=ws://host:1"]),
            (Some("opencode"), &["opencode", "--standalone"]),
            (Some("opencode"), &["opencode", "--server", "http://host:1"]),
            (Some("claude"), &["claude"]),
            (None, &["codex"]),
        ] {
            assert_eq!(kind(agent, argv), None, "{agent:?} {argv:?}");
        }
    }

    #[test]
    fn recorded_pid_must_still_run_the_server_subcommand() {
        let codex = SharedServerKind::CodexAppServer;
        let opencode = SharedServerKind::OpencodeService;
        assert!(codex.matches(&process(
            "codex",
            "/x/bin/codex app-server --listen unix:// --managed-daemon"
        )));
        assert!(codex.matches(&process(
            "codex",
            "/x/bin/codex app-server daemon pid-update-loop"
        )));
        assert!(codex.matches(&process("CODEX.EXE", r"C:\x\codex.exe app-server")));
        assert!(opencode.matches(&process("opencode", "/x/opencode serve --service")));
        for (kind, name, cmdline) in [
            (codex, "codex", "codex"),
            (codex, "zsh", "zsh app-server"),
            (codex, "codex-code-mode-host", "/x/codex-code-mode-host"),
            (opencode, "opencode", "opencode"),
            (opencode, "opencode", "/x/opencode --standalone"),
            (opencode, "codex", "codex serve"),
        ] {
            assert!(!kind.matches(&process(name, cmdline)), "{name} {cmdline}");
        }
    }
}
