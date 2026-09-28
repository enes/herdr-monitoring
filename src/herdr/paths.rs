//! Platform-aware executable identity shared by monitoring and pane actions.

use std::path::{Component, Path, PathBuf};

use super::ForegroundProcess;

pub(crate) fn monitor_executable_path(plugin_root: &Path) -> PathBuf {
    plugin_root.join("target/release").join(format!(
        "herdr-resource-monitor{}",
        std::env::consts::EXE_SUFFIX
    ))
}

pub(crate) fn process_executable(process: &ForegroundProcess) -> Option<PathBuf> {
    let name = process
        .argv
        .as_ref()
        .and_then(|argv| argv.first())
        .or(process.argv0.as_ref())?;
    let path = Path::new(name);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Neither a basename nor a Windows drive-relative/rooted path proves
        // which executable is running. Resolve only explicit relative paths
        // against the foreground process's own absolute working directory.
        if !name.chars().any(std::path::is_separator)
            || path.has_root()
            || path
                .components()
                .any(|part| matches!(part, Component::Prefix(_)))
        {
            return None;
        }
        let cwd = Path::new(process.cwd.as_deref()?);
        if !cwd.is_absolute() {
            return None;
        }
        cwd.join(path)
    };
    path.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(argv0: &str, cwd: Option<&Path>) -> ForegroundProcess {
        ForegroundProcess {
            argv0: Some(argv0.into()),
            cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn executable_identity_requires_an_unambiguous_path() {
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy();
        let cwd = executable.parent().unwrap();
        assert_eq!(process_executable(&process(&name, Some(cwd))), None);
        assert_eq!(
            process_executable(&process(&format!("./{name}"), None)),
            None
        );
        assert_eq!(
            process_executable(&process(&format!("./{name}"), Some(Path::new(".")))),
            None
        );
        assert_eq!(
            process_executable(&process(&format!("./{name}"), Some(cwd))),
            Some(executable.canonicalize().unwrap())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_separators_resolve_but_drive_relative_or_rooted_paths_do_not() {
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy();
        let cwd = executable.parent().unwrap();
        assert_eq!(
            process_executable(&process(&format!(r".\{name}"), Some(cwd))),
            Some(executable.canonicalize().unwrap())
        );
        for ambiguous in [format!(r"C:.\{name}"), format!(r"\{name}")] {
            assert_eq!(process_executable(&process(&ambiguous, Some(cwd))), None);
        }
        assert_eq!(
            monitor_executable_path(Path::new(r"C:\plugin"))
                .file_name()
                .unwrap(),
            "herdr-resource-monitor.exe"
        );
    }
}
