//! The side-effect boundary (architecture §2).
//!
//! All interaction with the outside world is funneled through this layer:
//! process execution via the `CommandRunner` trait (no shell — arg vectors
//! only, so there is no injection surface), atomic file IO that follows
//! symlinks, and logging initialization. Concentrating side effects here lets
//! tests inject a mock command recorder and temporary directories to assert
//! exact behavior (R6.1).
//!
//! The concrete abstractions (`CommandRunner`, the atomic writer, freshness
//! tracking) are added in the System-boundary tasks (§2 of `docs/tasks.md`).
//! Logging initialization ([`logging`]) also lives here, since directing output
//! to the systemd journal is itself a side effect (architecture §2).

pub mod logging;
