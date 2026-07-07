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
//! shell-free process execution and the atomic file [`writer`] for
//! symlink-following writes; freshness tracking (task 2.3) is still to come.
//! Logging initialization ([`logging`]) also lives here, since directing output
//! to the systemd journal is itself a side effect (architecture §2).

// The command-execution boundary and the atomic file writer are foundational
// infrastructure: they are consumed by the reload command table (task 4.4) and
// the Apply pipeline (task 4.5), which have not been implemented yet. Until they
// wire these into the app, each module's public surface is exercised only by its
// own tests, so the non-test binary build would flag every item as dead code.
// Scope the allowance to `not(test)` so the `dead_code` lint stays fully active
// in test builds (where the surface is used); the allowance can be removed once
// 4.4/4.5 land.
#[cfg_attr(not(test), allow(dead_code))]
pub mod command;
pub mod logging;
#[cfg_attr(not(test), allow(dead_code))]
pub mod writer;
