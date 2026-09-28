//! Per-tab runtime records guarded by one session lock, released by the OS on exit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::herdr::transport::endpoint_identity;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MonitorRecord {
    pub session_socket: PathBuf,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused_monitor_pane_id: String,
    pub terminal_id: String,
}

pub(super) struct SessionState {
    _lock: File,
    socket: PathBuf,
    directory: PathBuf,
    name: String,
}

pub(super) struct TabState {
    session: SessionState,
    workspace_id: String,
    tab_id: String,
    path: PathBuf,
}

impl SessionState {
    pub fn acquire(directory: &Path, socket: &Path) -> Result<Option<Self>, String> {
        let socket = endpoint_identity(socket)
            .map_err(|error| format!("Cannot identify the current Herdr socket: {error}"))?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("Cannot create HERDR_PLUGIN_STATE_DIR: {error}"))?;
        // Stable across Rust versions and processes. The full key is checked on load.
        let hash = socket
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        let name = format!("focused-{hash:016x}");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("{name}.lock")))
            .map_err(|error| format!("Cannot open the monitor action lock: {error}"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("Cannot lock monitor state: {error}"));
            }
        }
        Ok(Some(Self {
            _lock: lock,
            socket,
            directory: directory.to_owned(),
            name,
        }))
    }

    pub fn for_tab(self, workspace_id: &str, tab_id: &str) -> Result<TabState, String> {
        if workspace_id.is_empty() || tab_id.is_empty() {
            return Err(
                "The current pane has no workspace or tab identity; no pane was changed".into(),
            );
        }
        // Length prefixes distinguish ambiguous joins without exposing tab IDs
        // as filesystem paths. Validate the full identities when reading back.
        let mut hash = 0xcbf29ce484222325_u64;
        for part in [workspace_id, tab_id] {
            for byte in (part.len() as u64)
                .to_le_bytes()
                .iter()
                .chain(part.as_bytes())
            {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            }
        }
        // Legacy session-wide JSON is intentionally left alone. In-tab process
        // discovery recovers an existing monitor without touching other tabs.
        let path = self
            .directory
            .join(format!("{}-tab-{hash:016x}.json", self.name));
        Ok(TabState {
            session: self,
            workspace_id: workspace_id.to_owned(),
            tab_id: tab_id.to_owned(),
            path,
        })
    }
}

impl TabState {
    pub fn load(&self) -> Result<Option<MonitorRecord>, String> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Cannot read monitor state: {error}")),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Cannot read monitor state: {error}"))?;
        if bytes.len() > 16 * 1024 {
            return Err("Monitor state exceeds the expected size; no pane was changed".into());
        }
        let record: MonitorRecord = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "Invalid monitor state at {}: {error}; no pane was changed",
                self.path.display()
            )
        })?;
        if record.session_socket != self.session.socket {
            return Err(
                "Monitor state belongs to a different Herdr session; no pane was changed".into(),
            );
        }
        if record.workspace_id != self.workspace_id || record.tab_id != self.tab_id {
            return Err(
                "Monitor state belongs to a different Herdr tab; no pane was changed".into(),
            );
        }
        Ok(Some(record))
    }

    pub fn save(&self, pane_id: &str, terminal_id: &str) -> Result<(), String> {
        let record = MonitorRecord {
            session_socket: self.session.socket.clone(),
            workspace_id: self.workspace_id.clone(),
            tab_id: self.tab_id.clone(),
            focused_monitor_pane_id: pane_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            // Close the temporary handle before the atomic replacement,
            // including on Windows where rename obeys handle sharing rules.
            drop(file);
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|error| format!("Cannot save monitor state: {error}"))
    }

    pub fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Cannot clear stale monitor state: {error}")),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
