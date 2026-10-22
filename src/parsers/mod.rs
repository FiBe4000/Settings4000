//! Config-file parsers and surgical writers (architecture §3).
//!
//! One submodule per file format the app edits (flat `key=value` files — the
//! palette and notifd's settings —, hyprlang, Lua literals, `monitors.conf`, GTK INI,
//! `uwsm/env`), plus the read-only readers for the palette pipeline's generated
//! files. The rules below are shared by every editing parser; each submodule's own
//! docs describe only what is specific to its format (its grammar, how a value is
//! addressed, which occurrence of a repeated key is in effect) and link back here.
//!
//! Hard layering rule: like [`crate::core`], nothing here may import `gtk` or
//! `relm4` — parsers are pure, display-free, and independently testable
//! (enforced by `tests/module_boundaries.rs`).
//!
//! # The parser contract
//!
//! The files this app edits are hand-maintained. They carry comments that explain
//! values, blank-line grouping, a deliberate key order and commented-out
//! alternatives, all of which a maintainer relies on and none of which a data model
//! would remember. So no parser here is a serializer: none ever regenerates a file
//! from parsed values (architecture §3). Each keeps a **lossless line/token
//! representation** — every physical line's exact bytes, terminator included, plus a
//! classification of what the line means — and edits by splicing bytes inside that
//! representation. Two guarantees follow, and every parser has tests for both
//! (R5.3 item 1, R6.1):
//!
//! - **Round-trip identity.** Parsing a file and emitting it again with no edit
//!   reproduces the input byte for byte, comments, ordering and line terminators
//!   included (`parse → edit nothing → emit == input`).
//! - **Single-span edits.** Changing a value rewrites only that value's bytes. Any
//!   leading indentation, the key, the separator and the whitespace around it, any
//!   trailing whitespace or inline comment, the line terminator and every other line
//!   stay byte-identical.
//!   A key that does not exist yet is added as a *new* line (where depends on the
//!   format); adding never rewrites an existing line.
//!
//! A few rules make those guarantees hold on real files rather than only on tidy
//! ones:
//!
//! - **Parsing never fails.** A line the parser cannot interpret is kept verbatim
//!   and reported — as a returned warning or, in the Lua editor, as an opaque line
//!   or a structure error — never raised as an error, so an unexpected line can
//!   neither stop the app from reading the rest of the file nor be lost on the next
//!   write. Most parsers also log their warnings at `warn`; the exceptions say why
//!   in their `parse` docs.
//! - **Unsafe values are refused, not escaped.** Each format has characters a value
//!   must not contain because they would end the value, the line or the file's
//!   structure early (a line break everywhere; `#` in hyprlang; `"` and `\` in a
//!   Lua string; …). An edit carrying one is rejected with an error and leaves the
//!   file completely unchanged (R8.3), and what is written is exactly what is read
//!   back, with no escape decoding in between.
//! - **The effective copy is the one read and edited.** When a key is assigned
//!   more than once, the parser resolves the duplicates the way the program
//!   consuming the file does — for hyprlang, GTK INI, `uwsm/env`, `monitors.conf`
//!   and Hyprland's Lua config the **last** assignment wins — and reads and edits
//!   that same copy, because editing an earlier, shadowed copy would silently
//!   change nothing (task 9.8). The shadowed copies stay byte-identical. Each module
//!   states its format's rule and its exceptions (hyprlang's repeatable keys; the
//!   palette, where a duplicate is a schema error).
//! - **No filesystem access.** A parser turns text into a representation and back
//!   into text; it never opens, creates or writes a file. Reading the live file,
//!   writing it atomically through its symlink and rolling it back are the job of
//!   [`crate::system::writer`] and the Apply pipeline ([`crate::core::apply`]). That
//!   writer only rewrites files that already exist, so where a parser can build a
//!   file from scratch, that path produces bytes only.
//!
//! # Diagnostics carry positions, not contents (R7.3)
//!
//! A parser's warnings and errors say *where* a problem is — a 1-based line number,
//! a count, a section name or key, at most the single value at fault — and never
//! quote the surrounding file. That is what lets a caller log any of them at `warn`
//! without dumping a user's config into the journal, where R7.3 allows only paths,
//! changed keys and outcomes at `info`/`warn` and keeps parsed values for `debug`.
//! The rule is stated here, with the types that carry the data, rather than in
//! [`crate::system::logging`]: that module only chooses where log events go and at
//! which level they are filtered, while what an event may contain is decided where
//! it is built. The per-parser warning and error types are the ones the rule
//! constrains, and the tests that pin it live beside them.

// Format-invariant helpers shared by every line-based parser below (task 9.18):
// lossless splitting and re-joining, terminator stripping and choice, and the
// forbidden-character scan. Private: it is an implementation detail of the
// parsers, and each parser keeps its own line classification and error types.
mod line;

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
