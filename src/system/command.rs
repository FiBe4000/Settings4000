//! Process execution boundary: the [`CommandRunner`] trait and its real
//! implementation (task 2.1, architecture §6).
//!
//! Every side effect that runs an external program — the reload commands
//! (`hyprctl reload`, `swaync-client -rs`, …), `gsettings`, `wpctl`, and the
//! palette `generate-colors` script — goes through this trait rather than
//! touching [`std::process`] directly. Concentrating process execution here
//! buys three things:
//!
//! - **No shell, ever.** A command is a program plus an explicit argument
//!   vector ([`Command`]); it is spawned with [`std::process::Command`], never
//!   through `sh -c` and never by interpolating a string. There is therefore no
//!   command-injection surface anywhere in the app (architecture §10).
//! - **A bounded, self-healing runtime.** Every invocation runs under a
//!   [`DEFAULT_TIMEOUT`] deadline; a child that overruns it is killed and the
//!   call returns [`CommandError::Timeout`] rather than hanging the Apply
//!   pipeline or a reload.
//! - **Uniform logging and testability.** The real runner logs every
//!   invocation and its exit status (R7.3), and tests can swap in the
//!   [`MockCommandRunner`] recorder to assert the exact command sequence and
//!   inject canned outcomes (R6.1).
//!
//! This module is part of the `system/` side-effect layer and, like the rest
//! of it, may be used from `core/` only through the trait — the concrete
//! [`SystemCommandRunner`] is constructed at the top of the app and passed down.

use std::fmt;
use std::io;
use std::io::Read;
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

/// The wall-clock limit every command runs under (architecture §6).
///
/// A child that has not exited within this window is killed and the call
/// returns [`CommandError::Timeout`]. Five seconds is generous for the
/// short-lived reload/query commands the app issues (`hyprctl`, `wpctl`,
/// `swaync-client`, `gsettings`) while still guaranteeing that a wedged or
/// interactively-blocked process can never stall Apply or startup. The value is
/// fixed for production; tests may construct a [`SystemCommandRunner`] with a
/// shorter limit to exercise the timeout path quickly.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound, in bytes, on how much captured stderr is emitted to the logs.
///
/// Captured output is kept in full on the returned [`CommandOutput`] (callers
/// may need all of it — e.g. surfacing a `generate-colors` failure), but only a
/// leading slice is ever written to the journal, and only at `debug`. This
/// keeps a chatty command from flooding the log while still leaving enough of
/// the message to diagnose a failure. Per R7.3/R8.1, full output is never
/// logged at `info`.
const STDERR_LOG_LIMIT: usize = 512;

/// A shell-free process invocation: a program name plus its argument vector.
///
/// This is the only way to describe a command to a [`CommandRunner`]. Because
/// the program and each argument are distinct strings that are handed to the OS
/// `exec` unchanged (never concatenated into a shell line), arguments cannot be
/// reinterpreted as extra commands, flags, or redirections — there is no shell
/// to do the reinterpreting. Build one with the [`Command::new`] +
/// [`Command::arg`]/[`Command::args`] builder, e.g.
/// `Command::new("hyprctl").arg("reload")`.
///
/// The type deliberately shares its short name with [`std::process::Command`];
/// within this crate it refers to this value type, and the standard library's
/// builder is reached as `std::process::Command` in the runner's implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    /// The executable to run, resolved against `PATH` by the OS at spawn time.
    program: String,
    /// The arguments passed verbatim to the program, in order. Does *not*
    /// include the program name itself (unlike C `argv`).
    args: Vec<String>,
}

impl Command {
    /// Starts building a command that runs `program` with no arguments.
    ///
    /// `program` is looked up on `PATH` (or used as a path if it contains a
    /// separator) by the operating system when the command is run.
    pub(crate) fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Appends a single argument, returning the command for chaining.
    #[must_use]
    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments in order, returning the command for chaining.
    #[must_use]
    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The program name this command will run.
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    /// The command's arguments, in order, excluding the program name.
    pub(crate) fn args_slice(&self) -> &[String] {
        &self.args
    }
}

impl fmt::Display for Command {
    /// Renders the command as `program arg1 arg2 …` for human-readable logs.
    ///
    /// This is a *display* rendering only: the arguments are space-joined
    /// without quoting because they are never handed to a shell. It must not be
    /// used to reconstruct a command line for execution — commands are always
    /// run from the structured [`Command`] via a [`CommandRunner`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

/// The result of a command that ran to completion (whether it exited zero or
/// non-zero).
///
/// A completed run — even one that reports failure via a non-zero exit code —
/// is an `Ok(CommandOutput)` from [`CommandRunner::run`]; the "could not run to
/// completion" cases (spawn failure, timeout) are the [`CommandError`]
/// variants. Callers distinguish a successful command from a failed one with
/// [`CommandOutput::success`].
///
/// The exit status is stored as its numeric code rather than a
/// [`std::process::ExitStatus`] so the type is fully constructible in tests and
/// carries no platform-specific state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    /// The process's exit code, or `None` when it was terminated by a signal
    /// without producing a code (Unix). `None` is never "success".
    code: Option<i32>,
    /// Everything the process wrote to standard output, captured in full.
    stdout: Vec<u8>,
    /// Everything the process wrote to standard error, captured in full.
    stderr: Vec<u8>,
}

impl CommandOutput {
    /// Whether the command reported success, i.e. exited with code `0`.
    ///
    /// A non-zero code or a signal termination (`code == None`) is a failure.
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The process's exit code, or `None` if it was killed by a signal.
    pub(crate) fn code(&self) -> Option<i32> {
        self.code
    }

    /// The bytes the process wrote to standard output.
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// The bytes the process wrote to standard error.
    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// A command that could not be run to completion.
///
/// These are the failure modes distinct from a plain non-zero exit (which is a
/// successful *run* with an unsuccessful *result* — see [`CommandOutput`]).
/// Keeping them separate lets a caller propagate "the command could not run"
/// with `?` while still inspecting the exit code of a command that did run.
#[derive(Debug)]
pub(crate) enum CommandError {
    /// The program could not be started at all — most often because it is not
    /// on `PATH`, or the file is not executable. Carries the underlying OS
    /// error.
    Spawn(io::Error),
    /// The child was still running after [`limit`](CommandError::Timeout::limit)
    /// elapsed and was killed. Its output is discarded.
    Timeout {
        /// The deadline that was exceeded.
        limit: Duration,
    },
    /// An operating-system error occurred while waiting for the child to exit.
    /// This is rare (it does not include a normal non-zero exit) and indicates
    /// the wait itself failed.
    Wait(io::Error),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::Spawn(error) => write!(f, "failed to spawn command: {error}"),
            CommandError::Timeout { limit } => {
                write!(f, "command timed out after {limit:?} and was killed")
            }
            CommandError::Wait(error) => write!(f, "failed to wait for command: {error}"),
        }
    }
}

impl std::error::Error for CommandError {
    /// Exposes the underlying [`io::Error`] for the spawn/wait variants so the
    /// full error chain is available to callers that print or wrap it. The
    /// timeout variant has no OS-error cause.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CommandError::Spawn(error) | CommandError::Wait(error) => Some(error),
            CommandError::Timeout { .. } => None,
        }
    }
}

/// Runs external programs on behalf of the app.
///
/// This is the single seam through which every process is executed. The real
/// implementation is [`SystemCommandRunner`]; tests use [`MockCommandRunner`].
/// Making it a trait is what lets the Apply pipeline and reload table (tasks
/// 4.4/4.5) be tested by asserting the exact command sequence against a
/// recorder rather than actually mutating the running desktop (R6.1).
pub(crate) trait CommandRunner {
    /// Runs `command` to completion or until the timeout, returning its output.
    ///
    /// Returns `Ok(CommandOutput)` when the process ran to completion — check
    /// [`CommandOutput::success`] for the exit result — and `Err` when it could
    /// not (spawn failure or timeout). Implementations must not invoke a shell.
    fn run(&self, command: &Command) -> Result<CommandOutput, CommandError>;
}

/// The production [`CommandRunner`]: spawns real processes via
/// [`std::process::Command`] under the [`DEFAULT_TIMEOUT`] deadline.
///
/// Each invocation is spawned with no controlling shell and with stdin closed;
/// stdout and stderr are captured and the invocation plus its exit status is
/// logged (R7.3). A child that overruns the timeout is killed.
#[derive(Clone, Debug)]
pub(crate) struct SystemCommandRunner {
    /// Wall-clock deadline applied to every command this runner spawns.
    timeout: Duration,
}

impl SystemCommandRunner {
    /// Creates a runner using the fixed production [`DEFAULT_TIMEOUT`] (5 s).
    pub(crate) fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Creates a runner with an explicit timeout.
    ///
    /// This exists only so tests can exercise the timeout-and-kill path without
    /// waiting five real seconds; production code always uses [`Self::new`],
    /// which fixes the deadline at [`DEFAULT_TIMEOUT`].
    #[cfg(test)]
    fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
        // Spawn with stdin closed (a command that reads stdin gets EOF rather
        // than blocking until it hits the timeout) and both output streams
        // piped so they can be captured. No shell is involved: the program and
        // arguments go straight to the OS `exec` (architecture §10).
        let spawn_result = ProcessCommand::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                // A spawn failure is usually a missing binary; log it at `warn`
                // (the command did not run at all) and let the caller decide
                // how to surface it.
                tracing::warn!(
                    program = %command.program,
                    args = ?command.args,
                    error = %error,
                    "command failed to spawn"
                );
                return Err(CommandError::Spawn(error));
            }
        };

        // Drain both output pipes concurrently with the wait, on one reader
        // thread per stream. This is load-bearing, not an optimization: a child
        // that writes more than the OS pipe buffer holds (~64 KiB per stream on
        // Linux) blocks in `write(2)` until the parent reads the pipe, so a
        // runner that drained only *after* the wait would deadlock — the child
        // never exits, the wait burns the full deadline, and a perfectly healthy
        // command is killed with its output lost. Reading as the child produces
        // output avoids that entirely; the documented `pw-dump` JSON consumer
        // (architecture §7) routinely exceeds 64 KiB. `.take()` moves each pipe
        // out of the child so the reader thread owns it; the thread reaches EOF
        // when the child closes its write end, which happens both on a normal
        // exit and when we `kill()` it below. Both handles are joined on every
        // return path, so no reader thread ever outlives the call.
        let stdout_reader = spawn_pipe_reader(child.stdout.take());
        let stderr_reader = spawn_pipe_reader(child.stderr.take());

        // Block until the child exits or the deadline passes. `wait_timeout`
        // uses the OS wait primitives, so it neither polls nor busy-waits.
        match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => {
                // The child has exited, so both write ends are closed and the
                // readers have hit (or will imminently hit) EOF; joining them
                // collects the full captured output without blocking.
                let output = CommandOutput {
                    code: status.code(),
                    stdout: join_pipe_reader(stdout_reader),
                    stderr: join_pipe_reader(stderr_reader),
                };

                // R7.3: log the invocation and its exit status at `info` — the
                // command and result, but never the full output. A truncated
                // slice of stderr is logged at `debug` only, for diagnosis.
                tracing::info!(
                    program = %command.program,
                    args = ?command.args,
                    exit_code = ?output.code,
                    success = output.success(),
                    "ran command"
                );
                log_stderr_excerpt(command, &output);

                Ok(output)
            }
            Ok(None) => {
                // The deadline passed with the child still running. Kill it —
                // closing its pipe write ends so the readers reach EOF — then
                // reap the zombie and join the readers so neither thread is left
                // running. All three are best-effort: if the child has exited in
                // the meantime the kill/wait simply no-op or error harmlessly.
                // The captured output is discarded on timeout.
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe_reader(stdout_reader);
                let _ = join_pipe_reader(stderr_reader);
                tracing::warn!(
                    program = %command.program,
                    args = ?command.args,
                    timeout = ?self.timeout,
                    "command exceeded its timeout and was killed"
                );
                Err(CommandError::Timeout {
                    limit: self.timeout,
                })
            }
            Err(error) => {
                // Waiting itself failed (a rare OS-level error). Kill and reap
                // the child, then join the readers so neither thread is left
                // running, before reporting the wait failure.
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe_reader(stdout_reader);
                let _ = join_pipe_reader(stderr_reader);
                tracing::warn!(
                    program = %command.program,
                    args = ?command.args,
                    error = %error,
                    "failed while waiting for command to exit"
                );
                Err(CommandError::Wait(error))
            }
        }
    }
}

/// Spawns a thread that reads a child pipe to end into a byte buffer.
///
/// This is what lets the runner drain stdout and stderr *concurrently* with the
/// timed wait instead of after it: each stream is read on its own thread as the
/// child produces output, so a child that writes more than the OS pipe buffer
/// holds never blocks in `write(2)` waiting for the parent — which would
/// otherwise deadlock the wait (see [`CommandRunner::run`] on
/// [`SystemCommandRunner`]).
///
/// The thread takes ownership of the pipe handle (moved in via [`Option::take`]
/// at the call site) and returns the collected bytes when it reaches EOF, which
/// happens when the child closes its write end — on a normal exit or when the
/// runner kills it. A read error (which should not happen for a child's own
/// pipe) is treated as "no more output" rather than failing the whole call,
/// since the exit status is the load-bearing result and partial output is still
/// useful for logging. The `Option` is `None` only if a stream was not piped, in
/// which case the thread simply returns an empty buffer.
fn spawn_pipe_reader<R>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

/// Joins a pipe-reader thread and returns the bytes it captured.
///
/// A panic in the reader thread (not expected — its only fallible operation, the
/// read, is already swallowed) is downgraded to empty output rather than
/// propagated, keeping output capture best-effort and consistent with a read
/// error: the command's exit status remains the load-bearing result.
fn join_pipe_reader(handle: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Logs a truncated, lossy view of a command's stderr at `debug`, when there is
/// any.
///
/// Kept separate from the `info` invocation log so that full output never
/// appears at `info` (R7.3): operators see the command and its exit status at
/// `info`, and only opt into the stderr excerpt by raising the level to
/// `debug`. The excerpt is capped at [`STDERR_LOG_LIMIT`] bytes.
fn log_stderr_excerpt(command: &Command, output: &CommandOutput) {
    if output.stderr.is_empty() {
        return;
    }
    let end = output.stderr.len().min(STDERR_LOG_LIMIT);
    let excerpt = String::from_utf8_lossy(&output.stderr[..end]);
    tracing::debug!(
        program = %command.program,
        truncated = output.stderr.len() > STDERR_LOG_LIMIT,
        stderr = %excerpt,
        "command stderr"
    );
}

/// A test double that records every invocation and returns pre-scripted
/// outcomes (R6.1).
///
/// Tests use it to assert the *exact* sequence of commands a piece of logic
/// issues — the reload table and Apply pipeline (tasks 4.4/4.5) are verified
/// this way — without spawning any real process. It records each [`Command`] in
/// order and answers [`CommandRunner::run`] from a queue of canned outcomes,
/// falling back to a successful empty result once the queue is exhausted so a
/// test only needs to script the outcomes it cares about.
///
/// State is held behind a [`std::sync::Mutex`] so the recorder can be shared
/// (e.g. behind an `&dyn CommandRunner`) and observed after use while `run`
/// takes `&self`.
#[cfg(test)]
pub(crate) struct MockCommandRunner {
    inner: std::sync::Mutex<MockState>,
}

/// The interior, lock-guarded state of a [`MockCommandRunner`].
#[cfg(test)]
struct MockState {
    /// Every command passed to [`CommandRunner::run`], in call order.
    recorded: Vec<Command>,
    /// Canned outcomes returned in FIFO order; when empty, `run` returns a
    /// successful empty [`CommandOutput`].
    outcomes: std::collections::VecDeque<Result<CommandOutput, CommandError>>,
}

#[cfg(test)]
impl MockCommandRunner {
    /// Creates a recorder that returns success for every call (until/unless
    /// outcomes are queued).
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(MockState {
                recorded: Vec::new(),
                outcomes: std::collections::VecDeque::new(),
            }),
        }
    }

    /// Creates a recorder pre-loaded with `outcomes`, returned one per call in
    /// order; further calls after the queue drains return success.
    pub(crate) fn with_outcomes<I>(outcomes: I) -> Self
    where
        I: IntoIterator<Item = Result<CommandOutput, CommandError>>,
    {
        Self {
            inner: std::sync::Mutex::new(MockState {
                recorded: Vec::new(),
                outcomes: outcomes.into_iter().collect(),
            }),
        }
    }

    /// Returns a snapshot of the commands recorded so far, in call order.
    pub(crate) fn recorded(&self) -> Vec<Command> {
        self.lock().recorded.clone()
    }

    /// Locks the interior state, panicking on a poisoned mutex.
    ///
    /// A poisoned lock means another thread panicked while holding it, which in
    /// a test is a bug to surface loudly — hence the `expect` (this is test-only
    /// code, so the "no panics on runtime-fallible paths" rule does not apply).
    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.inner
            .lock()
            .expect("mock command runner mutex was poisoned")
    }
}

#[cfg(test)]
impl CommandRunner for MockCommandRunner {
    fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
        let mut state = self.lock();
        state.recorded.push(command.clone());
        state.outcomes.pop_front().unwrap_or_else(|| {
            Ok(CommandOutput {
                code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        })
    }
}

#[cfg(test)]
impl CommandOutput {
    /// Builds an outcome with the given exit code and empty output, for scripting
    /// [`MockCommandRunner`] and asserting on [`CommandOutput`] in tests.
    pub(crate) fn fake(code: i32) -> Self {
        Self {
            code: Some(code),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// Builds an outcome with the given exit code and captured streams, for
    /// tests that assert on captured output.
    pub(crate) fn fake_with_streams(code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builder_collects_program_and_args_in_order() {
        let command = Command::new("hyprctl").arg("reload");
        assert_eq!(command.program(), "hyprctl");
        assert_eq!(command.args_slice(), ["reload"]);

        let multi = Command::new("wpctl").args(["set-default", "42"]);
        assert_eq!(multi.program(), "wpctl");
        assert_eq!(multi.args_slice(), ["set-default", "42"]);
    }

    #[test]
    fn command_display_space_joins_program_and_args() {
        let command = Command::new("swaync-client").arg("-rs");
        assert_eq!(command.to_string(), "swaync-client -rs");
    }

    #[test]
    fn command_output_success_is_exit_zero_only() {
        assert!(CommandOutput::fake(0).success());
        assert!(!CommandOutput::fake(1).success());
        // A signal termination (no exit code) is never a success.
        let signalled = CommandOutput {
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!signalled.success());
    }

    #[test]
    fn default_timeout_is_five_seconds() {
        // The production default must remain 5 s (task 2.1, architecture §6);
        // `new` and `Default` must both adopt it. Tests use `with_timeout` for a
        // shorter deadline, but that must never change the production value.
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(SystemCommandRunner::new().timeout, DEFAULT_TIMEOUT);
        assert_eq!(SystemCommandRunner::default().timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn command_error_reports_its_io_source_but_timeout_has_none() {
        use std::error::Error;

        let spawn = CommandError::Spawn(io::Error::from(io::ErrorKind::NotFound));
        assert!(spawn.source().is_some());

        let timeout = CommandError::Timeout {
            limit: Duration::from_secs(5),
        };
        assert!(timeout.source().is_none());
    }

    // --- MockCommandRunner (R6.1) ---------------------------------------------

    #[test]
    fn mock_records_every_invocation_in_order() {
        let runner = MockCommandRunner::new();

        let first = Command::new("hyprctl").arg("reload");
        let second = Command::new("swaync-client").arg("-rs");
        let _ = runner.run(&first);
        let _ = runner.run(&second);

        // The recorder must preserve program, args, and call order exactly so a
        // caller can assert the precise command sequence.
        assert_eq!(runner.recorded(), vec![first, second]);
    }

    #[test]
    fn mock_returns_queued_outcomes_in_order_then_defaults_to_success() {
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(0, "ready\n", "")),
            Ok(CommandOutput::fake(1)),
        ]);

        let first = runner
            .run(&Command::new("first"))
            .expect("first queued outcome is Ok");
        assert!(first.success());
        assert_eq!(first.stdout(), b"ready\n");

        let second = runner
            .run(&Command::new("second"))
            .expect("second queued outcome is Ok");
        assert!(!second.success());
        assert_eq!(second.code(), Some(1));

        // Once the queue is drained, further calls default to success so tests
        // need only script the outcomes they care about.
        let third = runner
            .run(&Command::new("third"))
            .expect("default outcome is Ok");
        assert!(third.success());
    }

    #[test]
    fn mock_can_inject_a_failure_outcome() {
        // A caller (e.g. the Apply pipeline) must be testable against a failing
        // command, so the recorder can hand back a `CommandError`.
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Timeout {
            limit: DEFAULT_TIMEOUT,
        })]);

        let result = runner.run(&Command::new("generate-colors").arg("nord"));
        assert!(matches!(result, Err(CommandError::Timeout { .. })));
        // The invocation is recorded even though it "failed".
        assert_eq!(runner.recorded().len(), 1);
    }

    // --- SystemCommandRunner against real processes ---------------------------
    //
    // These spawn the coreutils helpers `true`, `false`, `echo`, `cat`, `head`,
    // and `sleep`, which are present on the Linux desktop the app targets. They
    // are gated to Unix so the suite still builds elsewhere.

    #[cfg(unix)]
    #[test]
    fn real_runner_reports_success_for_true() {
        let output = SystemCommandRunner::new()
            .run(&Command::new("true"))
            .expect("`true` should run to completion");
        assert!(output.success());
        assert_eq!(output.code(), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_reports_non_zero_exit_for_false() {
        // `false` runs fine but exits non-zero: that is a completed run with an
        // unsuccessful result — `Ok`, not `Err`.
        let output = SystemCommandRunner::new()
            .run(&Command::new("false"))
            .expect("`false` should run to completion");
        assert!(!output.success());
        assert_eq!(output.code(), Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_captures_stdout() {
        let output = SystemCommandRunner::new()
            .run(&Command::new("echo").arg("settings4000"))
            .expect("`echo` should run to completion");
        assert!(output.success());
        assert_eq!(output.stdout(), b"settings4000\n");
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_captures_stderr_on_failure() {
        // `cat` of a missing file exits non-zero and writes a diagnostic to
        // stderr; assert both the failure and that stderr was captured (the
        // exact wording is locale/coreutils-dependent, so only non-emptiness is
        // checked).
        let output = SystemCommandRunner::new()
            .run(&Command::new("cat").arg("/nonexistent/settings4000/probe"))
            .expect("`cat` should run to completion");
        assert!(!output.success());
        assert!(
            !output.stderr().is_empty(),
            "expected `cat` to write a diagnostic to stderr"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_times_out_and_kills_a_slow_command() {
        use std::time::Instant;

        // A short injected deadline keeps the test fast; production stays at the
        // 5 s `DEFAULT_TIMEOUT` (asserted separately). `sleep 30` would far
        // outlast the deadline, so the runner must kill it and report a timeout.
        let runner = SystemCommandRunner::with_timeout(Duration::from_millis(200));

        let start = Instant::now();
        let result = runner.run(&Command::new("sleep").arg("30"));
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(CommandError::Timeout { .. })),
            "expected a timeout, got {result:?}"
        );
        // The call must return promptly after the deadline rather than waiting
        // out the full sleep — proof the child was actually killed. A wide
        // margin over the 200 ms deadline keeps this robust on a busy machine.
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout should return shortly after the deadline, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_reports_spawn_failure_for_a_missing_program() {
        let result =
            SystemCommandRunner::new().run(&Command::new("settings4000-definitely-not-a-binary"));
        assert!(
            matches!(result, Err(CommandError::Spawn(_))),
            "expected a spawn failure, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_does_not_interpret_shell_metacharacters_in_arguments() {
        // The module's headline invariant is that arguments are handed to the OS
        // `exec` verbatim, never to a shell (architecture §10). This makes that
        // an enforced test rather than a claim: the argument packs shell
        // metacharacters that, under any `sh -c` interpolation, would split on
        // `;`, run `touch` as a second command, and substitute `$(whoami)`.
        // `echo` (coreutils, resolved on PATH — not the shell builtin) instead
        // prints the whole string as its single literal argument, and the
        // side-effect file the injected `touch` would have created never appears.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let sentinel = dir.path().join("pwned");
        let sentinel_path = sentinel.to_str().expect("temp path should be valid UTF-8");
        let payload = format!("hi; touch {sentinel_path}; $(whoami)");

        let output = SystemCommandRunner::new()
            .run(&Command::new("echo").arg(&payload))
            .expect("`echo` should run to completion");

        assert!(output.success());
        // `echo` emits its argument verbatim plus a trailing newline: no shell
        // word-splitting on `;`, no command substitution of `$(whoami)`.
        let mut expected = payload.into_bytes();
        expected.push(b'\n');
        assert_eq!(output.stdout(), expected.as_slice());
        // The clincher: the sentinel does not exist, proving the injected
        // `touch` never ran — there was no shell to run it.
        assert!(
            !sentinel.exists(),
            "shell metacharacters in an argument must not be executed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_captures_large_output_without_stalling() {
        use std::time::Instant;

        // Emit far more than the OS pipe buffer (~64 KiB per stream on Linux).
        // If the runner drained stdout only *after* the wait, `head` would block
        // in `write(2)` once the buffer filled, never exit, and the call would
        // burn the full 5 s `DEFAULT_TIMEOUT` before returning a spurious
        // `Timeout` with the output lost. Concurrent draining reads as the child
        // produces output, so a healthy large-output command completes promptly
        // with every byte captured. This pins the concurrent-draining fix and
        // guards the documented `pw-dump` JSON consumer (architecture §7).
        const BYTES: usize = 200_000;
        let count = BYTES.to_string();

        let start = Instant::now();
        let output = SystemCommandRunner::new()
            .run(&Command::new("head").args(["-c", count.as_str(), "/dev/zero"]))
            .expect("`head` should run to completion");
        let elapsed = start.elapsed();

        assert!(output.success());
        assert_eq!(
            output.stdout().len(),
            BYTES,
            "the full large output must be captured, not truncated at the pipe buffer"
        );
        // A stall would return only at the ~5 s deadline; a comfortable margin
        // below that catches the regression while staying robust on a busy CI.
        assert!(
            elapsed < Duration::from_secs(4),
            "a large-output command must not stall near the timeout, took {elapsed:?}"
        );
    }
}
