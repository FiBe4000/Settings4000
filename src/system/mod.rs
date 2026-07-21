//! The side-effect boundary (architecture §2).
//!
//! All interaction with the outside world is funneled through this layer:
//! process execution via the `CommandRunner` trait (no shell — arg vectors
//! only, so there is no injection surface), atomic file IO that follows
//! symlinks, and logging initialization. Concentrating side effects here lets
//! tests inject a mock command recorder and temporary directories to assert
//! exact behavior (R6.1).
//!
//! The concrete abstractions live here: [`command`]'s `CommandRunner` for
//! shell-free process execution, [`signal`]'s `ProcessSignaller` for delivering
//! POSIX signals to running processes (the kitty/hypridle reloads that are not
//! subprocesses), and the atomic file [`writer`] for symlink-following writes.
//! Logging initialization ([`logging`]) also lives here, since directing output to
//! the systemd journal is itself a side effect (architecture §2). File
//! *freshness*/conflict tracking (task 2.3) is domain logic and lives in
//! [`crate::core::freshness`], not here — it only reads files to compare them,
//! which needs no side-effect abstraction.

// The command-execution boundary: the `CommandRunner` trait and its production
// implementation. Every subprocess the app spawns goes through it as an arg
// vector with a 5 s timeout (R6.1) — the reload command table
// (`core::reload`), the Apply pipeline's `generate-colors` step, the
// runtime-only page controls (sound, network, notifications DND,
// laptop-display toggle), and the startup loader's probes.
pub mod command;
pub mod logging;

// The process-signal boundary: the `ProcessSignaller` trait through which the
// reload table (`core::reload`) sends SIGUSR1 to kitty and SIGTERM to hypridle
// — the reloads delivered as signals to already-running processes rather than
// as subprocesses. The Apply pipeline drives the reload executor after the
// file writes.
pub mod signal;

// The atomic file writer (R8.5): it canonicalizes the target first — so a file
// symlinked into the dotfiles repo has its real target rewritten with the link
// preserved — then writes temp-file-beside → fsync → atomic rename. The Apply
// pipeline (`core::apply`) is its sole consumer; every staged `FileWrite`
// reaches disk through it.
pub mod writer;
