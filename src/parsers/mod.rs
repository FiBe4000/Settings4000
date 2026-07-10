//! Config-file parsers and surgical writers (architecture §3).
//!
//! One submodule per file format the app edits (palette `key=hex`, hyprlang,
//! `monitors.conf`, swaync JSON, GTK INI, `uwsm/env`). Each parser produces a
//! lossless line/token representation and rewrites only the value span of a
//! targeted key, leaving comments, ordering, and commented-out lines
//! byte-identical. Every parser carries round-trip tests (`parse → edit
//! nothing → emit == input`).
//!
//! Hard layering rule: like [`crate::core`], nothing here may import `gtk` or
//! `relm4` — parsers are pure, display-free, and independently testable
//! (enforced by `tests/module_boundaries.rs`).

// The palette parser (task 3.1) will be consumed by the SettingsStore (task 4.2,
// which reads and stages `colors/<scheme>` for the theme page) and the theme
// palette page (task 6.3) — neither of which exists yet. Until they wire it in,
// its public surface is exercised only by its own tests, so a non-test build
// would flag every item as dead code. Scope the allowance to `not(test)` so the
// `dead_code` lint stays fully active in test builds (where the surface is used);
// remove it once 4.2/6.3 consume the parser.
#[cfg_attr(not(test), allow(dead_code))]
pub mod palette;

// The hyprlang parser (task 3.2) will be consumed by the SettingsStore (task 4.2)
// and the input (6.6), power/idle (6.8), wallpaper/lock (6.5), and cursor-env
// (6.4) pages — none of which exist yet. Until they wire it in, its public
// surface is exercised only by its own tests, so a non-test build would flag
// every item as dead code. Scope the allowance to `not(test)` so the `dead_code`
// lint stays active in test builds; remove it once those tasks consume it.
#[cfg_attr(not(test), allow(dead_code))]
pub mod hyprlang;

// The monitors.conf record parser (task 3.3) will be consumed by the
// SettingsStore (task 4.2) and the Display page (task 6.1) — neither of which
// exists yet. Until they wire it in, its public surface is exercised only by its
// own tests, so a non-test build would flag every item as dead code. Scope the
// allowance to `not(test)` so the `dead_code` lint stays active in test builds;
// remove it once 4.2/6.1 consume the parser.
#[cfg_attr(not(test), allow(dead_code))]
pub mod monitors;

// The swaync JSON adapter (task 3.4) will be consumed by the SettingsStore
// (task 4.2) and the Notifications page (task 6.7, which edits position,
// timeouts, and a do-not-disturb toggle) — neither of which exists yet. Until
// they wire it in, its public surface is exercised only by its own tests, so a
// non-test build would flag every item as dead code. Scope the allowance to
// `not(test)` so the `dead_code` lint stays active in test builds; remove it
// once 4.2/6.7 consume the adapter.
#[cfg_attr(not(test), allow(dead_code))]
pub mod swaync;

// The GTK settings.ini editor (task 3.5) will be consumed by the SettingsStore
// (task 4.2) and the Theme page (task 6.4, which writes the same theme/icon/
// cursor values into both gtk-3.0 and gtk-4.0 settings.ini) — neither of which
// exists yet. Until they wire it in, its public surface is exercised only by its
// own tests, so a non-test build would flag every item as dead code. Scope the
// allowance to `not(test)` so the `dead_code` lint stays active in test builds;
// remove it once 4.2/6.4 consume the editor.
#[cfg_attr(not(test), allow(dead_code))]
pub mod ini;
