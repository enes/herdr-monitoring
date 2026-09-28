//! Platform-specific local transport; JSON framing remains in control/events.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::{LocalStream, ShutdownHandle};
#[cfg(windows)]
pub(super) use windows::{LocalStream, ShutdownHandle};

/// Stable identity for a Herdr endpoint without treating Windows pipe names as files.
pub(crate) fn endpoint_identity(path: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        path.canonicalize()
    }
    #[cfg(windows)]
    {
        // Named pipe names are case-insensitive. Do not canonicalize the marker:
        // Herdr maps its configured spelling directly into the pipe namespace.
        let name = windows_pipe_name(path)?;
        Ok(PathBuf::from(name.to_string_lossy().to_ascii_lowercase()))
    }
}

/// Herdr 0.9.1 uses interprocess 2.4.2's GenericNamespaced mapping on Windows:
/// prepend the local pipe namespace to the configured endpoint's lossy string.
/// Even a filesystem-looking endpoint is a logical name, not a file to open.
#[cfg(any(windows, test))]
fn windows_pipe_name(path: &Path) -> io::Result<PathBuf> {
    let name = path.to_string_lossy();
    if name.is_empty() || name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Herdr endpoint must be nonempty and contain no NUL characters",
        ));
    }
    Ok(PathBuf::from(format!(r"\\.\pipe\{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_pipe_mapping_matches_herdr_namespaced_endpoints() {
        assert_eq!(
            windows_pipe_name(Path::new(r"C:\Users\example\herdr\herdr.sock")).unwrap(),
            PathBuf::from(r"\\.\pipe\C:\Users\example\herdr\herdr.sock")
        );
        assert_eq!(
            windows_pipe_name(Path::new("session-a.sock")).unwrap(),
            PathBuf::from(r"\\.\pipe\session-a.sock")
        );
        // The input is always the logical Herdr endpoint; do not reinterpret it.
        assert_eq!(
            windows_pipe_name(Path::new(r"\\.\pipe\example")).unwrap(),
            PathBuf::from(r"\\.\pipe\\\.\pipe\example")
        );
    }

    #[test]
    fn windows_pipe_mapping_rejects_invalid_names() {
        for name in ["", "bad\0name"] {
            assert_eq!(
                windows_pipe_name(Path::new(name)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_identity_does_not_require_a_marker_file() {
        let upper = endpoint_identity(Path::new(r"C:\Missing\Session.sock")).unwrap();
        let lower = endpoint_identity(Path::new(r"c:\missing\session.sock")).unwrap();
        assert_eq!(upper, lower);
        assert_ne!(
            upper,
            endpoint_identity(Path::new(r"C:\Missing\Other.sock")).unwrap()
        );
    }
}
