//! Bounded discovery probes, with process-group cleanup provided by the shared runner.
//! PATH and filesystem resolution can still wait on the underlying filesystem.
use omarchy_process_runner::{Output, Request};
use std::{
    env,
    ffi::{CString, OsStr, OsString},
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn resolve(program: &OsStr, directory: &Path) -> Option<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        return Some(if Path::new(program).is_absolute() {
            program.into()
        } else {
            directory.join(program)
        });
    }
    let path = env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    if path.as_bytes().len() > 16_384 {
        return None;
    }
    for entry in env::split_paths(&path).take(256) {
        let root = if entry.is_absolute() {
            entry
        } else {
            directory.join(entry)
        };
        let candidate = root.join(program);
        let name = CString::new(candidate.as_os_str().as_bytes()).ok()?;
        // SAFETY: valid NUL-terminated path and fixed flags. Effective access,
        // including ACLs, must agree with the credentials used by exec.
        let executable =
            unsafe { libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::X_OK, libc::AT_EACCESS) }
                == 0;
        if executable && fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn probe(
    program: &OsStr,
    arguments: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> Option<Output> {
    let deadline = Instant::now().checked_add(timeout)?;
    let directory = env::current_dir().ok()?;
    let program = resolve(program, &directory)?;
    // Preserve configuration/socket discovery, but do not inherit provider secrets.
    let environment = [
        "HOME",
        "PATH",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "CODEX_HOME",
        "TMUX",
        "TMUX_TMPDIR",
        "LANG",
        "LC_ALL",
        "USER",
        "LOGNAME",
    ]
    .into_iter()
    .filter_map(|name| env::var_os(name).map(|value| (OsString::from(name), value)))
    .collect();
    omarchy_process_runner::run(
        &Request {
            program,
            arguments: arguments.iter().map(OsString::from).collect(),
            directory,
            environment,
            timeout: deadline.checked_duration_since(Instant::now())?,
            output_limit,
        },
        &AtomicBool::new(false),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_argv_and_failure_status_preserved() {
        let out = probe(
            OsStr::new("/bin/sh"),
            &["-c", "printf '%s' \"$1\"; exit 7", "probe", "literal;$x"],
            Duration::from_secs(1),
            100,
        )
        .unwrap();
        assert_eq!(out.stdout, b"literal;$x");
        assert_eq!(out.status.code(), Some(7));
    }
    #[test]
    fn output_and_sleep_are_bounded() {
        assert!(
            probe(
                OsStr::new("/bin/sh"),
                &["-c", "while :; do printf overflow; done"],
                Duration::from_secs(1),
                1024
            )
            .is_none()
        );
        let start = Instant::now();
        assert!(
            probe(
                OsStr::new("/bin/sh"),
                &["-c", "sleep 10"],
                Duration::from_millis(30),
                100
            )
            .is_none()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn descendant_pipe_does_not_leave_a_reader_waiting() {
        let start = Instant::now();
        let result = probe(
            OsStr::new("/bin/sh"),
            &["-c", "sleep 10 & exit 0"],
            Duration::from_secs(1),
            100,
        )
        .unwrap();
        assert!(result.status.success());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
