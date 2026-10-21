//! Config-file parsers and surgical writers (architecture §3).
//!
//! One submodule per file format the app edits (flat `key=value` files — the
//! palette and notifd's settings —, hyprlang, Lua literals, `monitors.conf`, GTK INI,
//! `uwsm/env`). Each parser
//! produces a lossless line/token representation and rewrites only the value span
//! of a targeted key, leaving comments, ordering, and commented-out lines
//! byte-identical. Every parser carries round-trip tests (`parse → edit
//! nothing → emit == input`).
//!
//! Hard layering rule: like [`crate::core`], nothing here may import `gtk` or
//! `relm4` — parsers are pure, display-free, and independently testable
//! (enforced by `tests/module_boundaries.rs`).

// The shared lossless line model for flat `key=value` files (tasks 3.1, 11.15): bare
// assignments, `#` comment lines and blank lines, no inline comments. The palette
// parser (`palette`) and notifd's settings file (`core::notifd_config`) both build on
// it, each adding its own key and value rules.
pub mod kv;

// The palette `key=hex` parser (task 3.1; R3.2), a layer over `kv`. The generated-file readers
// (`generated`) parse a scheme file through it to read the 18 schema colors (`shadow` included)
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

// Lua literal values (task 11.2) and the surgical Lua-literal editor (task 11.3).
// `LuaLiteral` is the single definition of which values may be written into
// Hyprland's Lua config and how (strings double-quoted with `"`, `\` and control
// characters refused; numbers and booleans bare); the live `hyprctl eval` helper
// (`core::reload::HyprlandEval`) renders through it. `LuaConfigFile` edits
// `key = <literal>,` lines of the schema-flat `input.lua` by table path and the
// two cursor `hl.env(…)` lines of `hyprland.lua`, reporting everything else as
// opaque so a loader can hide a page instead of guessing.
pub mod lua_literal;

// The monitors.conf record parser (task 3.3). The Display-page domain model
// (`core::display`) reads the `monitor=` records through it and renders the
// staged mode/scale edits back, keeping the file parseable by the dotfiles'
// `hypr-display-profile.sh` (which derives eDP mode/scale from it).
pub mod monitors;

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
