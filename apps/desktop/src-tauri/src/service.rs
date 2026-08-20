//! Hosting of the `dsh web` service process.
//!
//! The desktop shell is a thin supervisor: it spawns the harness's own
//! `dsh --profile web` process (which owns every plugin, session, and the
//! HTTP/WebSocket surface), waits for the readiness line the web runtime
//! prints (`dsh web: http://127.0.0.1:<port>`), and then points the window at
//! that URL. Stopping the shell stops the service; an unexpected service exit
//! is reported so the shell can restart it.
//!
//! Dev mode launches the checkout's CLI from source
//! (`node --import tsx/esm apps/cli/src/bin.ts web --port 0`), so a `tauri
//! dev` run always reflects the current workspace, including any rebuilt
//! client bundles. Bundled mode (release build) launches the resources the
//! bundle embeds — see apps/desktop/README.md for the resource layout.

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// The readiness line the `dsh web` runtime prints once the server binds:
/// `dsh web: http://127.0.0.1:<port>` (optionally followed by a LAN literal).
const READY_PREFIX: &str = "dsh web: ";

/// How long to wait for the service to print its readiness line before
/// reporting a startup failure.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace period between SIGTERM and SIGKILL when stopping the service.
const TERM_GRACE: Duration = Duration::from_secs(3);

/// Errors from hosting the `dsh web` service.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Spawning the service process failed.
    #[error("failed to spawn the dsh web service: {0}")]
    Spawn(#[from] std::io::Error),
    /// The child exited before printing its readiness line.
    #[error("the dsh web service exited before readiness (code {code:?})")]
    ExitedBeforeReady { code: Option<i32> },
    /// No readiness line arrived within the timeout.
    #[error("timed out waiting for the dsh web readiness line after {timeout:?}")]
    ReadyTimeout { timeout: Duration },
}

/// A running `dsh web` child: its pid plus the readiness URL once resolved.
#[derive(Debug)]
pub struct ServiceHandle {
    /// Process id of the child, used for signal-based shutdown.
    pub pid: u32,
    /// The resolved GUI URL; `None` until the readiness line arrives.
    pub url: Option<String>,
}

/// Fixed desktop port, separate from the browser edition's default 3080. A
/// stable origin keeps the WebView's localStorage (plugin prefs, skin, task
/// board, aionui collapse state) persistent across launches; a random port
/// would create a fresh origin every start and drop every stored preference.
const DESKTOP_PORT: &str = "31080";

/// Spawn the checkout's CLI from source. The manifest lives at
/// `<repo>/apps/desktop/src-tauri`, so the repo root is three parents up.
pub fn dev_service_command() -> Result<Command, ServiceError> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| std::io::Error::other("CARGO_MANIFEST_DIR has no repo root above it"))?;
    let mut cmd = Command::new("node");
    // --no-open: rc.8's web runtime opens the default browser after startup
    // (openBrowser defaults true); the desktop shell renders its own WebView
    // window and must not hijack the user's browser.
    cmd.args(["--import", "tsx/esm", "apps/cli/src/bin.ts", "web", "--port", DESKTOP_PORT, "--no-open"])
        .current_dir(repo);
    Ok(cmd)
}

/// Spawn the bundled service from the app's resource directory. The bundle
/// embeds a portable Node runtime, the built dsh CLI, and a clean web
/// profile; see apps/desktop/README.md.
pub fn bundled_service_command(resource_dir: &Path) -> Result<Command, ServiceError> {
    let node = resource_dir.join("runtime").join("bin").join("node");
    let entry = resource_dir.join("dsh").join("lib").join("bin.js");
    let mut cmd = Command::new(node);
    cmd.arg(entry)
        .arg("web")
        .arg("--port")
        .arg(DESKTOP_PORT)
        // --no-open: the shell renders its own window; rc.8 would otherwise
        // hand off to the default browser.
        .arg("--no-open")
        .current_dir(resource_dir.join("dsh"));
    Ok(cmd)
}

/// Read the service's stdout until the readiness line, then keep draining so
/// the child never blocks on a full pipe. Runs on a dedicated thread per
/// spawn; the caller reaps the child afterwards.
fn drain_stdout(mut stdout: impl BufRead, ready_tx: Sender<String>) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(rest) = line.strip_prefix(READY_PREFIX) {
                    let url = rest.split_whitespace().next().unwrap_or("").to_string();
                    if !url.is_empty() && ready_tx.send(url).is_err() {
                        // The shell is gone; stop reading.
                        return;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Spawn the service, drain its output, and wait for readiness.
///
/// The child is spawned in its own process group (Unix `process_group(0)`),
/// so a shutdown can signal the whole group — dsh web owns its own children
/// (shells, workers) and a pid-only kill would orphan them. Orphans keep
/// writing the shared `~/.dsh/sessions` logs and corrupt the session file
/// (seq gaps) when a later instance runs against the same profile.
///
/// The child is intentionally not reaped here: the caller keeps the returned
/// handle for shutdown and receives `exit_code` when the process actually
/// ends, so a restart decision follows the real exit.
pub fn spawn_service(mut command: Command) -> Result<(ServiceHandle, Receiver<Option<i32>>), ServiceError> {
    let mut child: Child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (exited_tx, exited_rx) = mpsc::channel();

    // Drain thread owns the child (reaps it on exit) and the stdout reader.
    thread::spawn(move || {
        drain_stdout(BufReader::new(stdout), ready_tx);
        // The pipe closed; reap and report the real exit code.
        let code = child.wait().ok().and_then(|status| status.code());
        let _ = exited_tx.send(code);
    });

    let url = wait_for_ready(&ready_rx, READY_TIMEOUT)?;
    Ok((
        ServiceHandle { pid, url: Some(url) },
        exited_rx,
    ))
}

/// Wait for the readiness line, reporting failures instead of hanging.
fn wait_for_ready(ready_rx: &Receiver<String>, timeout: Duration) -> Result<String, ServiceError> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ServiceError::ReadyTimeout { timeout });
        }
        match ready_rx.recv_timeout(remaining) {
            Ok(url) => return Ok(url),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(ServiceError::ReadyTimeout { timeout });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The drain thread ended without a readiness line: the child
                // died before serving. The exit code is on the other channel.
                return Err(ServiceError::ExitedBeforeReady { code: None });
            }
        }
    }
}

/// Signal one pid, returning whether the signal was delivered.
fn signal(pid: i32, sig: i32) -> bool {
    unsafe { libc::kill(pid, sig) == 0 }
}

/// Whether a pid (or process group, when negative) still exists.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Gracefully stop a running service and its whole process group: SIGTERM to
/// the group, then SIGKILL after the grace period. A no-op when the process
/// is already gone.
pub fn stop_service(pid: u32) {
    let pid = pid as i32;
    // Negative pid targets the process group the child was spawned into.
    let group = -pid;
    if signal(group, libc::SIGTERM) {
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if !alive(group) && !alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        return;
    }
    // No process group (spawned before the group change, or already dead):
    // fall back to signalling the pid directly.
    if signal(pid, libc::SIGTERM) {
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if !alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_prefix_parses_url_line() {
        let line = "dsh web: http://127.0.0.1:51234 (LAN: http://192.168.1.5:51234)";
        let rest = line.strip_prefix(READY_PREFIX).unwrap();
        assert_eq!(rest.split_whitespace().next(), Some("http://127.0.0.1:51234"));
    }

    #[test]
    fn ready_timeout_reports() {
        let (_tx, rx) = mpsc::channel::<String>();
        let err = wait_for_ready(&rx, Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, ServiceError::ReadyTimeout { .. }));
    }
}
