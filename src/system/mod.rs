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

// The command-execution boundary and the atomic file writer are foundational
// infrastructure: they are consumed by the reload command table (task 4.4) and
// the Apply pipeline (task 4.5). The writer is not wired into the app yet, so in a
// non-test binary build its public surface is exercised only by its own tests and
// would otherwise trip the `dead_code` lint. Scope the allowance to `not(test)` so
// the lint stays fully active in test builds (where the surface is used); the
// allowance can be removed once 4.5 lands.
#[cfg_attr(not(test), allow(dead_code))]
pub mod command;
pub mod logging;

// The process-signal boundary (task 4.4): the `ProcessSignaller` trait through
// which the reload table sends SIGUSR1 to kitty and SIGTERM to hypridle. It is
// consumed by `core/reload.rs`, whose executor is in turn driven by the Apply
// pipeline (task 4.5) — not yet wired into the app — so in a non-test build its
// surface is exercised only by its own tests. Scope the allowance to `not(test)`
// so the lint stays active in test builds; remove it once 4.5 wires reloads in.
#[cfg_attr(not(test), allow(dead_code))]
pub mod signal;

#[cfg_attr(not(test), allow(dead_code))]
pub mod writer;
