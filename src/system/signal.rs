//! Process-signal delivery boundary: the [`ProcessSignaller`] trait and its real
//! implementation (task 4.4, architecture §6).
//!
//! # Why a second side-effect seam
//!
//! Almost every reload the app issues is a subprocess and goes through
//! [`CommandRunner`](crate::system::command::CommandRunner). Two reloads are not
//! subprocesses but POSIX signals delivered directly to running processes:
//!
//! - **kitty color reload** — kitty re-reads its config on `SIGUSR1`, so a palette
//!   change signals every running kitty (architecture §6; kitty has no reload
//!   command in v1);
//! - **hypridle restart** — the fallback path (when no systemd user unit is
//!   actively managing hypridle) terminates the running hypridle with `SIGTERM`
//!   before respawning it.
//!
//! A signal cannot be modelled as a `Command`, so it needs its own seam. Like
//! `CommandRunner`, this is a trait so the reload table (`core/reload.rs`) can be
//! tested by asserting which processes were signalled with which signal, against a
//! recorder, rather than delivering real signals to the test host (R6.1). The real
//! [`SystemProcessSignaller`] delivers the signal via `nix::sys::signal::kill` — no
//! shell, no `pkill` subprocess (architecture §6: "sent directly via `nix::kill`,
//! no shell").
//!
//! # Matching processes
//!
//! A target is named by its executable basename (`kitty`, `hypridle`). The real
//! signaller finds matching PIDs by scanning `/proc/<pid>/cmdline` for the basename
//! of `argv[0]` — the same untruncated-name approach
//! [`crate::core::detect`] uses for liveness, and for the same reason (`/proc/comm`
//! is capped at 15 bytes). This module is part of the `system/` side-effect layer;
//! `core/` reaches it only through the [`ProcessSignaller`] trait.

use std::io;
use std::path::Path;

use nix::sys::signal::Signal;
use nix::unistd::Pid;

/// Delivers POSIX signals to running processes on behalf of the reload table.
///
/// The real implementation is [`SystemProcessSignaller`]; tests use
/// `MockProcessSignaller`. Making it a trait is what lets the kitty and hypridle
/// reloads (task 4.4) be tested by asserting the exact `(process, signal, PIDs)`
/// against a recorder rather than signalling real processes (R6.1).
pub(crate) trait ProcessSignaller {
    /// Sends `signal` to every running process whose executable basename equals
    /// `process_name`, returning the PIDs that were signalled.
    ///
    /// Matching a *process name* rather than taking explicit PIDs keeps the caller
    /// (the reload table) free of PID discovery: "reload all kitty instances" is
    /// exactly `signal_all("kitty", SIGUSR1)`. The returned PIDs are for logging and
    /// test assertions (R7.3). An empty result is not an error — it means no
    /// matching process is running (e.g. a race where the daemon exited between
    /// detection and reload); the `Err` case is reserved for a genuine inability to
    /// enumerate processes at all.
    fn signal_all(&self, process_name: &str, signal: Signal) -> io::Result<Vec<i32>>;
}

/// The production [`ProcessSignaller`]: scans procfs for matching processes and
/// delivers the signal via `nix::sys::signal::kill`.
///
/// It is a zero-sized unit struct because it holds no state — every call rescans
/// `/proc` for the current PIDs, since a daemon's PID can change between reloads
/// (e.g. hypridle after a respawn).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemProcessSignaller;

impl SystemProcessSignaller {
    /// Creates the signaller. Stateless — see the type documentation.
    pub(crate) fn new() -> Self {
        SystemProcessSignaller
    }
}

impl ProcessSignaller for SystemProcessSignaller {
    fn signal_all(&self, process_name: &str, signal: Signal) -> io::Result<Vec<i32>> {
        let pids = running_pids_named(process_name)?;
        let mut signalled = Vec::new();
        for pid in pids {
            // `kill` here is `nix::sys::signal::kill`, a thin wrapper over the
            // `kill(2)` syscall — it delivers a signal, it does not spawn or run
            // `/bin/kill`. There is no shell and nothing is interpolated.
            match nix::sys::signal::kill(Pid::from_raw(pid), signal) {
                Ok(()) => signalled.push(pid),
                Err(errno) => {
                    // A process that exited between the scan and the signal (ESRCH),
                    // or one we may not signal (EPERM), is logged and skipped rather
                    // than failing the whole reload (R5.5).
                    tracing::warn!(
                        pid,
                        process = process_name,
                        signal = ?signal,
                        %errno,
                        "failed to deliver signal to process"
                    );
                }
            }
        }
        tracing::info!(
            process = process_name,
            signal = ?signal,
            count = signalled.len(),
            "signalled processes"
        );
        Ok(signalled)
    }
}

/// Returns the PIDs of every running process whose `argv[0]` basename equals
/// `process_name`.
///
/// Reads `/proc/<pid>/cmdline` for each numeric `/proc` entry and compares the
/// basename of `argv[0]` (the untruncated executable name — see the module docs).
/// Enumeration failing outright (`/proc` unreadable) is surfaced as an `Err` so the
/// caller can report the reload could not be attempted; a single process that exits
/// mid-scan, or one with an empty/non-UTF-8 `cmdline`, is simply skipped (a benign
/// race, not an error).
fn running_pids_named(process_name: &str) -> io::Result<Vec<i32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let file_name = entry.file_name();
        // Only numeric `/proc` entries are process directories; the rest
        // (`self`, `cpuinfo`, …) fail the parse and are skipped.
        let Some(pid) = file_name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            if process_basename(&cmdline).as_deref() == Some(process_name) {
                pids.push(pid);
            }
        }
    }
    Ok(pids)
}

/// Extracts the basename of `argv[0]` from raw `/proc/<pid>/cmdline` bytes, or
/// `None` when unavailable.
///
/// `cmdline` is NUL-separated `argv`, so `argv[0]` is the bytes up to the first
/// NUL; its basename is the executable name a target is matched against. Returns
/// `None` for an empty `cmdline` (a kernel thread or zombie), a non-UTF-8
/// `argv[0]`, or a path with no final component.
fn process_basename(cmdline: &[u8]) -> Option<String> {
    let argv0 = cmdline.split(|&byte| byte == 0).next()?;
    if argv0.is_empty() {
        return None;
    }
    let argv0 = std::str::from_utf8(argv0).ok()?;
    Some(Path::new(argv0).file_name()?.to_str()?.to_string())
}

/// A test double that records every signal request and returns pre-configured
/// PIDs, without delivering any real signal (R6.1).
///
/// Tests use it to assert the *exact* processes a reload signals — which name,
/// which signal, and the PIDs it targeted — the kitty and hypridle reloads (task
/// 4.4) are verified this way. It is seeded with a name→PIDs map standing in for
/// the "running" processes; [`ProcessSignaller::signal_all`] returns the PIDs for
/// the requested name (empty if the name is not present) and records the call.
///
/// State is behind a [`std::sync::Mutex`] so the recorder can be shared behind an
/// `&dyn ProcessSignaller` and inspected after use while `signal_all` takes `&self`.
#[cfg(test)]
pub(crate) struct MockProcessSignaller {
    inner: std::sync::Mutex<MockSignalState>,
}

/// The interior, lock-guarded state of a [`MockProcessSignaller`].
#[cfg(test)]
struct MockSignalState {
    /// The processes considered "running", mapped to the PIDs a matching
    /// [`ProcessSignaller::signal_all`] call reports as signalled.
    running: std::collections::BTreeMap<String, Vec<i32>>,
    /// When set, every [`ProcessSignaller::signal_all`] call fails with a fresh
    /// [`io::Error`] of this kind instead of returning PIDs — so the reload
    /// executor's `ReloadError::Signal` path (process enumeration failing) can be
    /// exercised. Stored as a [`io::ErrorKind`] rather than an [`io::Error`] because
    /// the latter is not `Clone`.
    fail_with: Option<io::ErrorKind>,
    /// Every [`ProcessSignaller::signal_all`] call, in order.
    calls: Vec<SignalCall>,
}

/// One recorded [`ProcessSignaller::signal_all`] invocation.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SignalCall {
    /// The process name that was targeted.
    pub(crate) process_name: String,
    /// The signal that was requested.
    pub(crate) signal: Signal,
    /// The PIDs reported as signalled (the mock's configured PIDs for the name).
    pub(crate) pids: Vec<i32>,
}

#[cfg(test)]
impl MockProcessSignaller {
    /// Creates a recorder with no running processes; every call reports zero PIDs.
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(MockSignalState {
                running: std::collections::BTreeMap::new(),
                fail_with: None,
                calls: Vec::new(),
            }),
        }
    }

    /// Creates a recorder seeded with the given running processes and their PIDs.
    pub(crate) fn with_running<I>(running: I) -> Self
    where
        I: IntoIterator<Item = (String, Vec<i32>)>,
    {
        Self {
            inner: std::sync::Mutex::new(MockSignalState {
                running: running.into_iter().collect(),
                fail_with: None,
                calls: Vec::new(),
            }),
        }
    }

    /// Creates a recorder whose every [`ProcessSignaller::signal_all`] call fails
    /// with an [`io::Error`] of `kind`, standing in for a process-enumeration
    /// failure so the reload executor's `ReloadError::Signal` path can be tested.
    pub(crate) fn failing(kind: io::ErrorKind) -> Self {
        Self {
            inner: std::sync::Mutex::new(MockSignalState {
                running: std::collections::BTreeMap::new(),
                fail_with: Some(kind),
                calls: Vec::new(),
            }),
        }
    }

    /// Returns a snapshot of the recorded calls, in order.
    pub(crate) fn calls(&self) -> Vec<SignalCall> {
        self.lock().calls.clone()
    }

    /// Locks the interior state, panicking on a poisoned mutex.
    ///
    /// A poisoned lock means another thread panicked while holding it, a bug to
    /// surface loudly in a test — hence the `expect` (test-only code, so the "no
    /// panics on runtime-fallible paths" rule does not apply).
    fn lock(&self) -> std::sync::MutexGuard<'_, MockSignalState> {
        self.inner
            .lock()
            .expect("mock process signaller mutex was poisoned")
    }
}

#[cfg(test)]
impl ProcessSignaller for MockProcessSignaller {
    fn signal_all(&self, process_name: &str, signal: Signal) -> io::Result<Vec<i32>> {
        let mut state = self.lock();
        // A configured failure is recorded (with no PIDs, since none were signalled)
        // and returned as a fresh error, so a caller can assert both that it was
        // attempted and that it surfaced the failure.
        if let Some(kind) = state.fail_with {
            state.calls.push(SignalCall {
                process_name: process_name.to_string(),
                signal,
                pids: Vec::new(),
            });
            return Err(io::Error::from(kind));
        }
        let pids = state.running.get(process_name).cloned().unwrap_or_default();
        state.calls.push(SignalCall {
            process_name: process_name.to_string(),
            signal,
            pids: pids.clone(),
        });
        Ok(pids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_basename_is_argv0_basename_untruncated() {
        assert_eq!(
            process_basename(b"/usr/bin/kitty\0--session\0foo\0"),
            Some("kitty".to_string()),
            "the basename of argv[0] is used and argv[1..] is ignored"
        );
        assert_eq!(
            process_basename(b"hypridle\0"),
            Some("hypridle".to_string())
        );
        // Empty cmdline (kernel thread / zombie) yields no name.
        assert_eq!(process_basename(b""), None);
        assert_eq!(process_basename(b"\0"), None);
    }

    #[test]
    fn real_signaller_finds_no_pids_for_an_impossible_name() {
        // On any healthy host `/proc` is readable, so scanning for a name that
        // cannot be running returns an empty PID list rather than an error — and
        // signals nothing.
        let signaller = SystemProcessSignaller::new();
        let signalled = signaller
            .signal_all("settings4000-definitely-not-a-process", Signal::SIGUSR1)
            .expect("scanning /proc should succeed on a healthy host");
        assert!(
            signalled.is_empty(),
            "a process that cannot be running must be signalled to no PIDs"
        );
    }

    #[test]
    fn mock_returns_configured_pids_and_records_the_call() {
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![111, 222])]);

        let pids = signaller
            .signal_all("kitty", Signal::SIGUSR1)
            .expect("the mock never fails");
        assert_eq!(pids, vec![111, 222]);

        // A name that is not "running" reports no PIDs but is still recorded.
        let none = signaller
            .signal_all("hypridle", Signal::SIGTERM)
            .expect("the mock never fails");
        assert!(none.is_empty());

        assert_eq!(
            signaller.calls(),
            vec![
                SignalCall {
                    process_name: "kitty".to_string(),
                    signal: Signal::SIGUSR1,
                    pids: vec![111, 222],
                },
                SignalCall {
                    process_name: "hypridle".to_string(),
                    signal: Signal::SIGTERM,
                    pids: Vec::new(),
                },
            ]
        );
    }
}
