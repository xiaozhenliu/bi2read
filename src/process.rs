//! Subprocess spawning and cancellation.
//!
//! Spawns child processes via `std::process::Command`, piping stdout/stderr to
//! an optional log file (appended). On Unix, each child is launched in its own
//! process group so that cancellation can terminate the entire tree
//! (including shell-wrapped descendants) rather than only the direct child.
//! Cancellation is graceful-then-forceful (SIGTERM/SIGKILL on POSIX).

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use thiserror::Error;

/// Specification of a subprocess to run.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Optional file that stdout+stderr (merged, 2>&1) are appended to.
    pub log: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

impl SubprocessSpec {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            log: None,
            env: Vec::new(),
        }
    }

    /// Builder: set the working directory.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Builder: append merged stdout/stderr to this file.
    pub fn log(mut self, log: impl Into<PathBuf>) -> Self {
        self.log = Some(log.into());
        self
    }

    /// Builder: add an environment variable for this subprocess only.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Resolve a bare executable name for both terminal and Finder-launched App
/// processes. Finder does not reliably inherit the user's shell `PATH`, so
/// common per-user and macOS package-manager locations are checked after the
/// inherited path. Explicit paths are left untouched.
fn resolve_executable(
    program: &str,
    environment: &[(OsString, OsString)],
) -> Result<PathBuf, SubprocessError> {
    let search_path = environment
        .iter()
        .find(|(key, _)| key.as_os_str() == std::ffi::OsStr::new("PATH"))
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH"));
    let home = environment
        .iter()
        .find(|(key, _)| key.as_os_str() == std::ffi::OsStr::new("HOME"))
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    resolve_executable_from(program, search_path.as_deref(), home.as_deref())
}

fn resolve_executable_from(
    program: &str,
    search_path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, SubprocessError> {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') || program.contains('\\') {
        return Ok(path.to_path_buf());
    }

    let mut directories: Vec<PathBuf> = search_path
        .into_iter()
        .flat_map(|path| std::env::split_paths(path))
        .collect();
    if let Some(home) = home {
        directories.extend([home.join(".local").join("bin"), home.join("bin")]);
    }
    #[cfg(target_os = "macos")]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
    ]);

    directories.dedup();
    if let Some(candidate) = directories
        .into_iter()
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
    {
        return Ok(candidate);
    }

    Err(SubprocessError::Spawn(format!(
        "executable '{program}' not found in PATH or standard macOS locations; \
         install it or add its directory to PATH"
    )))
}

#[derive(Debug, Error)]
pub enum SubprocessError {
    #[error("spawn: {0}")]
    Spawn(String),
    #[error("wait: {0}")]
    Wait(String),
    #[error("cancelled")]
    Cancelled,
}

/// Opaque handle to a running child process.
///
/// Dropping this does NOT kill the child (use `cancel()`); it only detaches.
/// `cancel()` is graceful-then-forceful: on Unix it sends
/// SIGTERM to the child's process group, waits briefly, then SIGKILL.
/// `wait()` blocks until exit and returns the exit code.
pub struct ChildHandle {
    child: Option<Child>,
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl ChildHandle {
    /// Request cancellation: kill the process group, wait briefly, then
    /// force-kill. Reaps the zombie so it doesn't linger.
    pub fn cancel(&mut self) -> Result<(), SubprocessError> {
        let child = match self.child.as_mut() {
            Some(c) => c,
            None => return Ok(()),
        };
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                // Send SIGTERM to the whole process group.
                unsafe {
                    libc::killpg(pgid as i32, libc::SIGTERM);
                }
                // Poll for a grace period for graceful exit.
                let deadline = std::time::Instant::now() + CANCEL_GRACE;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return Ok(()),
                        Ok(None) => {
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => return Err(SubprocessError::Wait(e.to_string())),
                    }
                }
                // Force-kill the whole group.
                unsafe {
                    libc::killpg(pgid as i32, libc::SIGKILL);
                }
            }
        }
        // Also kill the direct child as a fallback.
        let _ = child.kill();
        match child.wait() {
            Ok(_) => Ok(()),
            Err(e) => Err(SubprocessError::Wait(e.to_string())),
        }
    }

    /// Block until the child exits, returning its exit status code.
    /// Returns the raw exit code (128+signum on signal death) so callers
    /// can distinguish.
    pub fn wait(mut self) -> Result<i32, SubprocessError> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| SubprocessError::Wait("no child".into()))?;
        let status = child
            .wait()
            .map_err(|e| SubprocessError::Wait(e.to_string()))?;
        Ok(status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            1
        }))
    }

    /// True if the child is still running (best-effort, non-blocking).
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            None => false,
            Some(c) => matches!(c.try_wait(), Ok(None)),
        }
    }
}

/// Grace period for graceful termination before force-killing.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Spawn a subprocess per the spec. On Unix the child is placed in its own
/// process group so cancellation can kill the entire descendant tree.
pub fn spawn(spec: SubprocessSpec) -> Result<ChildHandle, SubprocessError> {
    let executable = resolve_executable(&spec.argv[0], &spec.env)?;
    let mut cmd = Command::new(executable);
    cmd.args(&spec.argv[1..]);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.envs(spec.env);
    // Place the child in its own process group so we can later killpg it
    // together with any shell-wrapped descendants.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Merge stderr into stdout and append to the log file if given. Without a
    // log file, stdout is discarded: the CLI owns stdout for its JSON envelope
    // and helper commands (ffprobe, docker rm) would otherwise leak into it.
    // stderr still inherits so diagnostics stay visible in interactive runs.
    if let Some(log) = &spec.log {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SubprocessError::Spawn(e.to_string()))?;
        }
        let f = File::options()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| SubprocessError::Spawn(e.to_string()))?;
        cmd.stdout(Stdio::from(
            f.try_clone()
                .map_err(|e| SubprocessError::Spawn(e.to_string()))?,
        ));
        cmd.stderr(Stdio::from(f));
    } else {
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::inherit());
    }
    cmd.stdin(Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| SubprocessError::Spawn(e.to_string()))?;
    // Capture the process group id (== child pid when process_group(0) is used).
    #[cfg(unix)]
    let pgid = Some(child.id());
    Ok(ChildHandle {
        child: Some(child),
        #[cfg(unix)]
        pgid,
    })
}

/// Convenience: run a spec to completion and return the exit code.
pub fn run(spec: SubprocessSpec) -> Result<i32, SubprocessError> {
    spawn(spec)?.wait()
}

/// Captured result of a bounded helper command such as `docker inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run `argv` with stdout/stderr captured, killing the whole process group if
/// it has not exited within `timeout`. `Ok(None)` means the deadline passed:
/// callers treat that as "the tool is unresponsive", which is exactly the
/// Docker Desktop stale-socket failure mode where `docker` blocks forever.
pub fn capture_with_timeout(
    argv: Vec<String>,
    timeout: Duration,
) -> Result<Option<CapturedOutput>, SubprocessError> {
    let executable = resolve_executable(&argv[0], &[])?;
    let mut cmd = Command::new(executable);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| SubprocessError::Spawn(e.to_string()))?;
    #[cfg(unix)]
    let pgid = child.id() as i32;

    fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    #[cfg(unix)]
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(SubprocessError::Wait(e.to_string())),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(status.map(|status| CapturedOutput {
        code: status.code().unwrap_or(1),
        stdout,
        stderr,
    }))
}

/// Force-remove a named Docker container, used during cancellation cleanup
/// Returns the exit code (0 = removed or already gone).
#[cfg(unix)]
pub fn docker_rm_force(container_name: &str) -> i32 {
    let spec = SubprocessSpec::new(vec![
        "docker".into(),
        "rm".into(),
        "-f".into(),
        container_name.into(),
    ]);
    run(spec).unwrap_or(1)
}

#[cfg(not(unix))]
pub fn docker_rm_force(container_name: &str) -> i32 {
    let spec = SubprocessSpec::new(vec![
        "docker".into(),
        "rm".into(),
        "-f".into(),
        container_name.into(),
    ]);
    run(spec).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_home_local_bin_when_not_on_path() {
        let root = std::env::temp_dir().join(format!(
            "bi2read-process-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let bin = home.join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("ffmpeg");
        std::fs::write(&executable, b"placeholder").unwrap();

        let resolved = resolve_executable_from(
            "ffmpeg",
            Some(std::ffi::OsStr::new("/missing")),
            Some(&home),
        )
        .unwrap();
        assert_eq!(resolved, executable);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_executable_reports_search_guidance() {
        let program = format!("ffmpeg-missing-{}", std::process::id());
        let error = resolve_executable_from(
            &program,
            Some(std::ffi::OsStr::new("/missing")),
            Some(Path::new("/missing-home")),
        )
        .expect_err("missing executable must fail before spawning");
        assert!(error.to_string().contains("add its directory to PATH"));
    }

    #[test]
    fn runs_true_exit_zero() {
        let spec = SubprocessSpec::new(vec!["true".into()]);
        assert_eq!(run(spec).unwrap(), 0);
    }

    #[test]
    fn runs_false_exit_nonzero() {
        let spec = SubprocessSpec::new(vec!["false".into()]);
        assert!(run(spec).unwrap() != 0);
    }

    #[test]
    #[cfg(unix)]
    fn cancel_kills_sleeping_child() {
        // `sleep 30` would block; cancel should terminate it well before.
        let mut handle = spawn(SubprocessSpec::new(vec!["sleep".into(), "30".into()])).unwrap();
        assert!(handle.is_running());
        handle.cancel().unwrap();
        // After cancel the child is gone.
        assert!(!handle.is_running());
    }

    #[test]
    #[cfg(unix)]
    fn cancel_kills_process_group_descendants() {
        // `sh -c 'sleep 30 & wait'` spawns a grandchild sleep process.
        // Process-group cancellation must kill both the shell wrapper and
        // the sleep descendant.
        let mut handle = spawn(SubprocessSpec::new(vec![
            "sh".into(),
            "-c".into(),
            "sleep 30 & wait".into(),
        ]))
        .unwrap();
        assert!(handle.is_running());
        // 只检查本测试自己的进程组，避免匹配到机器上其他无关的 `sleep 30`。
        let pgid = handle.pgid.expect("unix spawn records a process group");
        handle.cancel().unwrap();
        assert!(!handle.is_running());
        // Give the kernel a moment to reap.
        std::thread::sleep(Duration::from_millis(100));
        let ps = std::process::Command::new("pgrep")
            .args(["-g", &pgid.to_string()])
            .output()
            .ok();
        if let Some(out) = ps {
            // pgrep returns non-zero if no match; stdout should be empty.
            assert!(
                out.stdout.is_empty(),
                "a process from group {pgid} is still alive after cancel"
            );
        }
    }

    #[test]
    fn log_is_appended() {
        let dir = std::env::temp_dir().join("bi2read-process-test");
        std::fs::remove_dir_all(&dir).ok();
        let log = dir.join("out.log");
        // First write.
        run(SubprocessSpec::new(vec!["sh".into(), "-c".into(), "echo hello".into()]).log(&log))
            .unwrap();
        // Second write should append, not truncate.
        run(SubprocessSpec::new(vec!["sh".into(), "-c".into(), "echo world".into()]).log(&log))
            .unwrap();
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
