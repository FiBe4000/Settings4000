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

// The palette `key=hex` parser (task 3.1; R3.2). The generated-file readers
// (`generated`) parse a scheme file through it to read the 17 schema colors
// for the Theme page's swatches, and `core::model` reuses its bare-hex
// validator. The app never writes `colors/<scheme>` itself — a scheme switch
// runs `generate-colors` — so the surgical `set_value` surface is exercised
// only by tests; it is kept because every parser honors the same lossless
// parse/edit/emit contract (architecture §3).
pub mod palette;

// The hyprlang parser (task 3.2) — the highest-risk parser (nested sections,
// `source=`, repeatable keys). The startup loader reads the hypr config files
// through it, and the input (`core::input`), power/idle (`core::power`), and
// wallpaper/lock + cursor-env (`core::theme`) write glue renders its surgical
// edits with it.
pub mod hyprlang;

// The monitors.conf record parser (task 3.3). The Display-page domain model
// (`core::display`) reads the `monitor=` records through it and renders the
// staged mode/scale edits back, keeping the file parseable by the dotfiles'
// `hypr-display-profile.sh` (which derives eDP mode/scale from it).
pub mod monitors;

// The swaync JSON adapter (task 3.4). The startup loader reads
// `swaync/config.json` through it into the store, and the Notifications-page
// write glue (`core::notifications`) renders the staged position/timeout
// edits back.
pub mod swaync;

// The GTK settings.ini editor (task 3.5). The Themes model (`core::theme`)
// writes the same theme/icon/cursor values into both `gtk-3.0` and `gtk-4.0`
// `settings.ini` through it (R3.4: every duplicated copy is written
// identically).
pub mod ini;

// The uwsm/env editor (task 3.6). The Themes model (`core::theme`) writes the
// `uwsm/env` copy of the cursor theme/size through it and reads the
// `GTK_THEME` override that gates the GTK-theme drop-down (R3.3).
pub mod env;

// The generated-file readers (task 3.7). The palette-scheme model
// (`core::theme`) detects and preselects the active scheme from the
// `# Generated from …` header and reads the per-scheme swatches for the Theme
// page's palette previews. Unlike the sibling modules this one is read-only —
// the generated files must never be written by the app.
pub mod generated;
