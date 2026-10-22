//! GTK-free Input-page domain logic (tasks 6.6, 11.4; architecture §3, §6, §10;
//! analysis §7.1, §8.6; R2.3, R4.2, R4.4, R5.6, R8.3, R6.2).
//!
//! # What this module is
//!
//! The Input page edits the keyboard layout list, keyboard options, mouse
//! sensitivity, and the two touchpad toggles — all staged to
//! `~/.config/hypr/input.lua`. Hyprland runs its Lua config provider, and
//! `hyprland.lua` loads this app-owned file with `dofile`; by its own header it is
//! "schema-flat (one `hl.config` call, no logic)": a single
//! `hl.config({ input = { … } })` table constructor whose fields are Lua literals
//! (`kb_layout = "us,se",`, `natural_scroll = true,`; analysis §7.1, §8.6). Unlike
//! the Display and Theme pages these settings map cleanly onto the fixed
//! [`SettingId`] enum, so they are staged in the shared
//! [`SettingsStore`](crate::core::store) like any other file-backed setting and
//! rendered by the declarative row framework (task 5.2). This module supplies the
//! Input-specific pieces the store cannot:
//!
//! - the **address map** [`input_lua_path`]: which dotted table path
//!   (`input.touchpad.tap_to_click`) each Input setting lives at — used by both the
//!   read side ([`input_values`]) and the write side, so a value is written back to
//!   exactly the address it was read from;
//! - the **usability gate** [`check_input_lua`]: whether the file is something the
//!   surgical editor fully understands, so the page is shown only when the values it
//!   displays are certainly the ones in effect (architecture §10);
//! - the **store-`SettingId` → `input.lua` write glue** [`render_input_lua`], which
//!   applies the store's dirty Input settings through the surgical Lua-literal editor
//!   ([`LuaConfigFile`], architecture §3), producing one lossless [`FileWrite`] whose
//!   diff is limited to the touched literal spans (the steps around the renderer are
//!   the shared [`crate::core::store_write`] glue, task 9.17);
//! - the **keyboard-layout candidate list** sourced from the XKB registry
//!   (`/usr/share/xkb/rules/evdev.xml`), parsed by [`parse_xkb_layouts`], which the
//!   Input page's reorderable add-control offers (R2.3).
//!
//! It lives in `core/` because all of it is pure domain logic over bytes — no GTK,
//! no process side effects — so it is unit-tested headlessly (R6.2); the layering
//! guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import here.
//!
//! # Why the page hides rather than guesses (architecture §10, R4.2)
//!
//! The editor recognises a few line shapes (table openers and closers, `key =
//! <literal>,` entries, comments) and reports everything else as *opaque*. An
//! opaque line in `input.lua` — a computed value, a conditional, a second call —
//! might override, or be overridden by, the literal the page would show and edit,
//! and the editor cannot tell which. So [`check_input_lua`] accepts the file only
//! when its brackets balance, it has an `input` table, and **no** line anywhere in
//! it is opaque; otherwise the startup loader hides the page and logs the reason at
//! `info` with line numbers only, never file contents (R7.3). The whole file is
//! checked, not just the `input` table, because an `hl.config({ input = … })`
//! nested inside an `if` block is opaque code outside every addressable table that
//! can still override the values.
//!
//! # Values keep their Lua kind
//!
//! Each setting has one Lua kind — layouts and options are strings, sensitivity a
//! number, the touchpad toggles booleans — and is only ever written in that kind.
//! The editor refuses to overwrite a literal of a different kind
//! ([`LuaEditError::KindMismatch`]) and refuses strings with a character a Lua
//! string literal cannot hold unescaped (`"`, `\`, control characters — R8.3). Both
//! abort the write rather than produce a file Hyprland would reject.
//!
//! # Why the whole `kb_options` list is one setting (preserve unknowns)
//!
//! [`SettingId::KeyboardOptions`] holds Hyprland's raw comma-joined `kb_options`
//! value, not one setting per option. The Input page shows a curated *switch* for
//! each option it understands (`caps:escape`, `grp:win_space_toggle`), and toggling
//! a switch adds or removes only that token from the string. Because the whole
//! string is the stored value, any option the app has no switch for is carried
//! through an edit **verbatim** — the app never drops a `kb_options` entry it does
//! not recognise (R4.2). The token add/remove logic is the pure
//! `value_from_token_toggle` in [`crate::ui::row`], which every curated switch
//! shares.
//!
//! # Conflict safety comes from the store (R5.6)
//!
//! `input.lua` is loaded through the store at startup, which fingerprints it as the
//! freshness baseline and hands the same tracker to the Apply pipeline
//! ([`apply::run`](crate::core::apply::run)); a commit re-baselines it. So this
//! module deliberately owns **no**
//! [`FreshnessTracker`](crate::core::freshness::FreshnessTracker):
//! [`InputModel::apply_contribution`] just reads the current bytes and renders the
//! edit, and the pipeline's step-2 conflict check aborts the apply if the file
//! changed externally — nothing here can clobber an external edit.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::core::apply::FileWrite;
use crate::core::model::{SettingId, Value, ValueKind};
use crate::core::reload::BackingFile;
use crate::core::store_write::{RenderedEdit, StoreBackedFile, StoreWriteError};
use crate::parsers::lua_literal::{
    LuaConfigFile, LuaEditError, LuaLiteral, LuaLiteralError, LuaLiteralKind, StructureError,
    TableStatus,
};

/// The table path of the Lua table that holds every Input setting: the `input`
/// field of the `hl.config({ … })` call in `input.lua` (analysis §7.1).
pub const INPUT_TABLE: &str = "input";

/// The Input settings in the order the page lists them and the loader reads them.
const INPUT_SETTINGS: [SettingId; 5] = [
    SettingId::KeyboardLayouts,
    SettingId::KeyboardOptions,
    SettingId::MouseSensitivity,
    SettingId::TouchpadNaturalScroll,
    SettingId::TouchpadTapToClick,
];

/// The well-known filesystem locations of the XKB rules registry the keyboard-layout
/// candidate list is parsed from, tried in order (task 6.6 / requirements §3).
///
/// Requirements §3 names `/usr/share/xkb/rules/evdev.xml`, but `xkeyboard-config`
/// installs to the FHS-standard `/usr/share/X11/xkb/...` on virtually every distro (and
/// on the target machine), so both are checked — [`default_xkb_registry`] returns the
/// first that exists. The registry path is still passed explicitly to
/// [`InputModel::load`], so a test injects a fixture path instead of reading the system
/// file, and a machine with neither location degrades to an empty candidate list (R4.4).
pub const XKB_REGISTRY_CANDIDATES: &[&str] = &[
    "/usr/share/xkb/rules/evdev.xml",
    "/usr/share/X11/xkb/rules/evdev.xml",
];

/// The XKB rules registry path to read on the real system: the first
/// [`XKB_REGISTRY_CANDIDATES`] entry that exists, or the first entry as a fallback so a
/// "not found" log names a canonical path (task 6.6).
///
/// Existence is probed rather than assumed because the file's location varies by distro
/// (see [`XKB_REGISTRY_CANDIDATES`]); when none exists, reading the fallback simply
/// yields an empty candidate list (R4.4).
pub fn default_xkb_registry() -> PathBuf {
    XKB_REGISTRY_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(XKB_REGISTRY_CANDIDATES[0]))
}

/// One keyboard layout offered by the XKB registry.
///
/// The `code` is what Hyprland stores in `kb_layout` (e.g. `us`, `se`); the
/// `description` is the human label the registry gives it (e.g. `English (US)`). They
/// are kept separate so the UI can show a friendly label while storing the code — the
/// same split the row framework's [`DropDownOption`](crate::ui::row::DropDownOption)
/// uses, which the Input page maps these onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutOption {
    /// The XKB layout code stored in `kb_layout` (e.g. `us`).
    pub code: String,
    /// The registry's human-readable description (e.g. `English (US)`).
    pub description: String,
}

/// The dotted `input.lua` table path each Input [`SettingId`] lives at, or `None`
/// for a setting the Input page does not back (a guard for a caller that passes a
/// foreign id).
///
/// Keyboard layout/options and sensitivity live directly in the `input` table, the
/// two touchpad toggles one table deeper in `input.touchpad`. These are Hyprland's
/// own option paths, so they are also what a live `hyprctl eval` would name
/// ([`HyprlandEval::config`](crate::core::reload::HyprlandEval::config)). Lua keys
/// must be identifiers, which is why the tap toggle is `tap_to_click` — the hyphen
/// of the old hyprlang spelling `tap-to-click` is not valid in a Lua key.
pub fn input_lua_path(id: SettingId) -> Option<&'static str> {
    match id {
        SettingId::KeyboardLayouts => Some("input.kb_layout"),
        SettingId::KeyboardOptions => Some("input.kb_options"),
        SettingId::MouseSensitivity => Some("input.sensitivity"),
        SettingId::TouchpadNaturalScroll => Some("input.touchpad.natural_scroll"),
        SettingId::TouchpadTapToClick => Some("input.touchpad.tap_to_click"),
        _ => None,
    }
}

/// The Lua kind each Input setting is stored as in `input.lua`.
fn expected_kind(id: SettingId) -> Option<LuaLiteralKind> {
    match id {
        SettingId::KeyboardLayouts | SettingId::KeyboardOptions => Some(LuaLiteralKind::String),
        SettingId::MouseSensitivity => Some(LuaLiteralKind::Number),
        SettingId::TouchpadNaturalScroll | SettingId::TouchpadTapToClick => {
            Some(LuaLiteralKind::Boolean)
        }
        _ => None,
    }
}

/// Why `input.lua` cannot back the Input page — the reason the startup loader logs
/// when it hides the page (architecture §10, R4.2).
///
/// Every variant's [`Display`](fmt::Display) names line numbers at most, never file
/// contents (R7.3).
#[derive(Debug)]
pub enum InputLuaUnavailable {
    /// The file could not be read — most commonly it does not exist.
    Unreadable(io::Error),
    /// Its brackets, blocks or strings do not balance, so no line can be located
    /// reliably and every edit would be refused.
    UnsafeStructure(StructureError),
    /// It has no `hl.config({ input = { … } })` table to read or write.
    NoInputTable,
    /// It contains lines the editor cannot classify (1-based line numbers), so the
    /// literals it can read may not be the values in effect.
    OpaqueLines(Vec<usize>),
}

impl fmt::Display for InputLuaUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputLuaUnavailable::Unreadable(error) => write!(f, "input.lua is unreadable: {error}"),
            InputLuaUnavailable::UnsafeStructure(error) => {
                write!(f, "input.lua's structure cannot be followed ({error})")
            }
            InputLuaUnavailable::NoInputTable => {
                write!(
                    f,
                    "input.lua has no `hl.config({{ input = {{ … }} }})` table"
                )
            }
            InputLuaUnavailable::OpaqueLines(lines) => write!(
                f,
                "input.lua has lines that are not plain `key = <literal>,` fields (lines {})",
                lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for InputLuaUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InputLuaUnavailable::Unreadable(error) => Some(error),
            // `StructureError` is a plain description (it is shown through `Display`
            // above), not an error type of its own.
            InputLuaUnavailable::UnsafeStructure(_)
            | InputLuaUnavailable::NoInputTable
            | InputLuaUnavailable::OpaqueLines(_) => None,
        }
    }
}

impl From<InputLuaUnavailable> for io::Error {
    /// Lets the startup store loader, whose contract is an [`io::Result`], report an
    /// unusable file as a skipped one: a read failure keeps its own error, any other
    /// reason becomes [`io::ErrorKind::InvalidData`] carrying the reason's text.
    fn from(reason: InputLuaUnavailable) -> Self {
        match reason {
            InputLuaUnavailable::Unreadable(error) => error,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

/// Decides whether a parsed `input.lua` may back the Input page (architecture §10;
/// see the module docs on why the page hides rather than guesses).
///
/// Accepts the file only when it has no structure error, has an `input` table, and
/// contains no opaque line anywhere. The last check covers
/// [`LuaConfigFile::table_status`]'s report for the `input` table (every line it
/// counts against the table is itself an opaque line) and also catches code outside
/// every addressable table.
///
/// This is deliberately stricter than the loader gate the Lua-literal editor was
/// designed around (`table_status("input") == Clean`, task 11.3): `table_status`
/// ignores code outside every top-level `hl.config` call, and such code — an
/// `hl.config({ input = … })` inside an `if` block, say — is exactly what can
/// override the literals the page would show (see the module section "Why the page
/// hides rather than guesses"). One consequence: the editor classifies every
/// `--[[ … ]]` block comment as opaque — a multi-line one and even one closed on its
/// own line (`--[[ off ]]`, a task-11.3 decision pinned by the editor's tests) — so
/// a block comment anywhere in `input.lua` hides the page. Plain `--` line comments
/// are ordinary comments and never do.
pub fn check_input_lua(file: &LuaConfigFile) -> Result<(), InputLuaUnavailable> {
    if let Some(error) = file.structure_error() {
        return Err(InputLuaUnavailable::UnsafeStructure(error.clone()));
    }
    match file.table_status(INPUT_TABLE) {
        TableStatus::Clean | TableStatus::ContainsOpaque(_) => {}
        // `UnsafeFile` only accompanies a structure error, which returned above.
        TableStatus::Missing | TableStatus::UnsafeFile => {
            return Err(InputLuaUnavailable::NoInputTable);
        }
    }
    let opaque = file.opaque_lines();
    if opaque.is_empty() {
        Ok(())
    } else {
        Err(InputLuaUnavailable::OpaqueLines(opaque))
    }
}

/// Reads `input.lua` at `path`, parses it, and runs [`check_input_lua`] on it,
/// returning the bytes read (the store's freshness baseline must be exactly what was
/// parsed) together with the parsed file.
pub fn read_input_lua(path: &Path) -> Result<(Vec<u8>, LuaConfigFile), InputLuaUnavailable> {
    let bytes = std::fs::read(path).map_err(InputLuaUnavailable::Unreadable)?;
    let file = LuaConfigFile::parse(&String::from_utf8_lossy(&bytes));
    check_input_lua(&file)?;
    Ok((bytes, file))
}

/// The Input settings a usable `input.lua` holds, as store originals (tasks 6.6,
/// 11.4).
///
/// Only keys actually present with the expected Lua kind become settings: a key
/// whose literal has another kind (`natural_scroll = 1`) is skipped and logged at
/// `info` with its path and the two kinds — never its value (R7.3) — so its row
/// renders its default and refuses edits rather than writing a re-typed option.
/// Sensitivity is read as a number of either subtype (`0` and `0.0` both load as
/// `0.0`). One key is seeded when absent: an empty [`SettingId::KeyboardOptions`],
/// whenever the `input` table exists, so a curated option switch can stage a first
/// option, which [`render_input_lua`] then inserts as a new field. An out-of-range
/// value is loaded verbatim; the store validates only staged edits, so a hand-written
/// config is never rejected at load.
pub fn input_values(file: &LuaConfigFile) -> Vec<(SettingId, Value)> {
    let mut values = Vec::new();
    for id in INPUT_SETTINGS {
        let (Some(path), Some(expected)) = (input_lua_path(id), expected_kind(id)) else {
            continue;
        };
        let Some(found) = file.value(path) else {
            if id == SettingId::KeyboardOptions
                && matches!(
                    file.table_status(INPUT_TABLE),
                    TableStatus::Clean | TableStatus::ContainsOpaque(_)
                )
            {
                values.push((id, Value::String(String::new())));
            }
            continue;
        };
        let value = match id {
            SettingId::KeyboardLayouts | SettingId::KeyboardOptions => {
                found.as_str().map(|text| Value::String(text.to_string()))
            }
            SettingId::MouseSensitivity => found.as_number().map(Value::Float),
            _ => found.as_bool().map(Value::Bool),
        };
        match value {
            Some(value) => values.push((id, value)),
            None => tracing::info!(
                path,
                expected = %expected,
                found = %found.kind(),
                "input.lua holds this option with an unexpected Lua kind; the setting is \
                 not offered (R4.2)"
            ),
        }
    }
    values
}

/// The [`LuaLiteral`] an Input setting's [`Value`] is written as, `None` when the
/// value's kind does not match the setting (a guard, not an expected case — the
/// store validates kinds on stage), or the literal's own refusal (a string with a
/// forbidden character, a non-finite number — R8.3).
fn input_literal(id: SettingId, value: &Value) -> Option<Result<LuaLiteral, LuaLiteralError>> {
    match (id, value) {
        (SettingId::KeyboardLayouts | SettingId::KeyboardOptions, Value::String(text)) => {
            Some(LuaLiteral::string(text))
        }
        (SettingId::MouseSensitivity, Value::Float(number)) => Some(LuaLiteral::float(*number)),
        (SettingId::TouchpadNaturalScroll | SettingId::TouchpadTapToClick, Value::Bool(flag)) => {
            Some(Ok(LuaLiteral::boolean(*flag)))
        }
        _ => None,
    }
}

/// Writes `literal` at the dotted `path`: rewrites the existing literal when the file
/// has one, otherwise inserts a new `key = literal,` field into the parent table.
///
/// Insertion is how a key the file does not declare gets its first value — today the
/// seeded `kb_options` (see [`input_values`]), and in general any `input` option such
/// as `kb_variant`/`kb_model`/`kb_rules`, which `input.lua` omits until set
/// (analysis §7.1). The editor refuses to insert into a table with opaque lines and
/// refuses a key that already exists as a sub-table, so this never adds a shadowing
/// duplicate.
fn set_or_insert(
    file: &mut LuaConfigFile,
    path: &str,
    literal: &LuaLiteral,
) -> Result<(), LuaEditError> {
    if file.value(path).is_some() {
        return file.set_value(path, literal);
    }
    let (table, key) = path.rsplit_once('.').unwrap_or(("", path));
    file.insert(table, key, literal)
}

/// Applies the store's dirty Input `edits` to the current `input.lua` `bytes`,
/// returning the complete new bytes plus the dotted paths it changed (e.g.
/// `input.kb_layout`) for the apply-level log line (tasks 6.6, 11.4; R5.3 item 1).
///
/// This is the pure store-`SettingId` → `input.lua` glue. It parses the current file
/// losslessly, then for each `(id, value)` rewrites **only** that setting's literal
/// span through the surgical Lua-literal editor — or, for a key the file does not
/// declare yet, inserts one new field before the closing brace of its table — so
/// comments, ordering, unrelated fields and every untouched byte stay identical.
///
/// Returns an [`InputRenderError`] when an edit is refused — the editor's own
/// refusals (a string value with a forbidden character, R8.3; a literal of another
/// kind on disk, [`LuaEditError::KindMismatch`]; a missing table; a file whose
/// structure cannot be followed) or a staged value whose kind does not fit its
/// setting — leaving the caller to abort the Apply rather than emit a partial file.
/// A setting with no Input path is ignored (it does not belong to this file).
pub fn render_input_lua(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<RenderedEdit, InputRenderError> {
    let mut file = LuaConfigFile::parse(&String::from_utf8_lossy(bytes));

    let mut changed_keys = Vec::new();
    for (id, value) in edits {
        let Some(path) = input_lua_path(*id) else {
            // Not an Input-backed setting: it belongs to another file.
            continue;
        };
        // The store checks kinds when a value is staged, so a mismatch here is a bug
        // upstream. It must still fail loudly: skipping it would let the store's
        // commit promote a value that never reached disk (R8.3).
        let literal = input_literal(*id, value)
            .ok_or(InputRenderError::ValueKind {
                path,
                found: value.kind(),
            })?
            .map_err(LuaEditError::from)?;
        set_or_insert(&mut file, path, &literal)?;
        changed_keys.push(path.to_string());
    }

    Ok(RenderedEdit {
        contents: file.emit().into_bytes(),
        changed_keys,
    })
}

/// Why [`render_input_lua`] could not apply the staged Input edits to `input.lua`.
#[derive(Clone, Debug, PartialEq)]
pub enum InputRenderError {
    /// The Lua-literal editor refused an edit (see [`LuaEditError`]).
    Edit(LuaEditError),
    /// A staged value's kind does not fit its setting (a boolean for a layout list,
    /// say), so it has no Lua literal to be written as. The store rejects such values
    /// when they are staged; this is the defensive backstop.
    ValueKind {
        /// The dotted `input.lua` path of the setting.
        path: &'static str,
        /// The kind of the value that was staged.
        found: ValueKind,
    },
}

impl fmt::Display for InputRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputRenderError::Edit(error) => write!(f, "{error}"),
            InputRenderError::ValueKind { path, found } => write!(
                f,
                "`{path}` cannot hold a {found:?} value, so it is not written"
            ),
        }
    }
}

impl std::error::Error for InputRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InputRenderError::Edit(error) => Some(error),
            InputRenderError::ValueKind { .. } => None,
        }
    }
}

impl From<LuaEditError> for InputRenderError {
    fn from(error: LuaEditError) -> Self {
        InputRenderError::Edit(error)
    }
}

/// Parses the XKB rules registry XML into the keyboard-layout candidate list (task
/// 6.6; requirements §3).
///
/// The registry (`evdev.xml`) lists layouts as
/// `<layoutList><layout><configItem><name>us</name><description>English (US)</description>…`.
/// This extracts each layout's **base** name and description — the `<configItem>` before
/// its `<variantList>`, so a layout *variant* (e.g. `dvorak`) is never mistaken for a
/// base layout — de-duplicates by code, and sorts by description for a stable,
/// browsable add-list. It never fails: a registry whose structure it does not recognise
/// (or an empty one) yields an empty list, which the Input page shows as an empty
/// add-control (R4.4). A small hand-rolled scan is used rather than adding an XML
/// dependency, matching the project's dependency-light parsers (§3); it targets exactly
/// this file's regular shape.
pub fn parse_xkb_layouts(xml: &str) -> Vec<LayoutOption> {
    let Some(list) = slice_between(xml, "<layoutList>", "</layoutList>") else {
        return Vec::new();
    };

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut layouts = Vec::new();
    // Each `<layout>…</layout>` block holds one base layout. Splitting on the opening
    // tag and taking everything up to the matching close isolates each block; layouts
    // do not nest, so this is unambiguous.
    for chunk in list.split("<layout>").skip(1) {
        let block = chunk.split("</layout>").next().unwrap_or(chunk);
        // The layout's own configItem precedes its `<variantList>`; restricting to the
        // text before the variant list keeps variant names/descriptions out.
        let base = block.split("<variantList").next().unwrap_or(block);
        let (Some(name), Some(description)) = (
            slice_between(base, "<name>", "</name>"),
            slice_between(base, "<description>", "</description>"),
        ) else {
            continue;
        };
        let code = unescape_xml(name.trim());
        if code.is_empty() {
            continue;
        }
        if seen.insert(code.clone()) {
            layouts.push(LayoutOption {
                code,
                description: unescape_xml(description.trim()),
            });
        }
    }

    layouts.sort_by(|a, b| a.description.cmp(&b.description));
    layouts
}

/// Reads and parses the XKB registry at `registry`, degrading to an empty list when it
/// is missing or unreadable (R4.4).
///
/// A missing registry, or one that parses to no layouts, is logged at `info` (the
/// hidden/absent-feature signal, R4.2) — never as an error, since a machine without the
/// XKB rules file simply offers no layouts to add, which the Input page handles.
fn read_xkb_layouts(registry: &Path) -> Vec<LayoutOption> {
    match std::fs::read_to_string(registry) {
        Ok(xml) => {
            let layouts = parse_xkb_layouts(&xml);
            if layouts.is_empty() {
                tracing::info!(
                    path = %registry.display(),
                    "XKB registry parsed no layouts; the keyboard-layout add-list is empty (R4.4)"
                );
            }
            layouts
        }
        Err(error) => {
            tracing::info!(
                path = %registry.display(),
                %error,
                "XKB registry unreadable; the keyboard-layout add-list is empty (R4.4)"
            );
            Vec::new()
        }
    }
}

/// Returns the slice of `haystack` between the first `open` and the following `close`,
/// or `None` if either is missing. A tiny helper for the targeted XKB scan.
fn slice_between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let rest = &haystack[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

/// Decodes the five predefined XML entities that appear in registry descriptions.
///
/// `&amp;` is decoded last so a decoded `&` is never re-interpreted as the start of
/// another entity. This is sufficient for `evdev.xml`, which uses only these entities.
fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The Input page's runtime-loaded helpers: where `input.lua` lives and the XKB
/// layout candidates (tasks 6.6, 11.4).
///
/// Built once on the startup worker (architecture §8) when Hyprland runs its Lua
/// config provider, and held by the window. It is intentionally **not** a staging
/// model — every Input setting is staged in the shared store, so dirty tracking,
/// validation, reset, and commit all flow through the store. This just supplies the
/// layout add-list and turns the store's dirty Input settings into the `input.lua`
/// [`FileWrite`] on Apply. Its existence is the signal that the Input page may be
/// shown: [`InputModel::load`] fails when `input.lua` cannot back the page.
pub struct InputModel {
    /// The `input.lua` write target: its live XDG path (R8.5), read fresh when
    /// rendering a write, plus how the shared glue labels and logs it (see
    /// [`write_target`]).
    input_lua: StoreBackedFile,
    /// The XKB layout candidates the reorderable add-control offers (R2.3).
    layouts: Vec<LayoutOption>,
}

impl InputModel {
    /// Builds the model: checks that `input.lua` can back the page
    /// ([`read_input_lua`]), records its path, and reads the XKB layout candidates
    /// from `xkb_registry` (the production entry point, called from the startup
    /// worker — architecture §8).
    ///
    /// Fails with the reason when the file cannot back the page, which the caller
    /// logs before hiding the page (R4.2). Reading the (large) registry here, off the
    /// main thread, keeps it inside the R8.1 cold-start budget; an unreadable registry
    /// degrades to no candidates (R4.4).
    pub fn load(
        input_lua: PathBuf,
        xkb_registry: &Path,
    ) -> Result<InputModel, InputLuaUnavailable> {
        read_input_lua(&input_lua)?;
        Ok(InputModel {
            input_lua: write_target(input_lua),
            layouts: read_xkb_layouts(xkb_registry),
        })
    }

    /// The XKB layout candidates for the reorderable add-control (R2.3).
    pub fn layout_options(&self) -> &[LayoutOption] {
        &self.layouts
    }

    /// The Input page's contribution to an Apply: the store's dirty Input settings
    /// rendered into one surgical `input.lua` [`FileWrite`] (task 6.6; named like
    /// every other page's Apply seam since task 9.17).
    ///
    /// `dirty` is the store's dirty Input settings (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category)).
    /// The shared glue ([`StoreBackedFile::render_write`]) reads the current file bytes
    /// and applies them through [`render_input_lua`]; see it for the three answers this
    /// can give — no write, one write, or an error the caller must turn into an aborted
    /// Apply rather than a skipped file. The error side is reached when the file
    /// vanished or was edited by hand into something the editor refuses since it was
    /// loaded.
    pub fn apply_contribution(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, StoreWriteError<InputRenderError>> {
        self.input_lua.render_write(dirty, render_input_lua)
    }
}

/// How the shared store-backed write glue addresses, labels and logs `input.lua`
/// (task 9.17).
///
/// Only the path varies between runs, so everything else the glue needs is fixed
/// here, at the one place a reader can check it against the file it describes.
fn write_target(path: PathBuf) -> StoreBackedFile {
    StoreBackedFile {
        path,
        name: "input.lua",
        backing: BackingFile::InputLua,
        read_failure_log: "could not read input.lua to render the Input edits; aborting \
                           the apply (R8.3)",
        render_failure_log: "failed to render an input.lua edit; aborting the apply (R8.3)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::model::Category;
    use crate::core::reload::ReloadParams;
    use crate::core::store::{FileReader, FileValues, SettingsStore};
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;
    use crate::testing::differing_lines;

    /// The app-owned `input.lua` shape (analysis §7.1): header comments, one
    /// `hl.config` call holding the `input` table with string, number and boolean
    /// literals, blank-line grouping, and the nested `touchpad` table. `kb_options`
    /// carries an entry the app has no switch for (`compose:ralt`).
    const INPUT_LUA: &str = "\
-- Input configuration (keyboard layout/options, sensitivity, touchpad)
-- Loaded by hyprland.lua via dofile. App-owned: written by the settings app;
-- keep it small and schema-flat (one hl.config call, no logic).
hl.config({
    input = {
        kb_layout = \"us,se\",
        kb_options = \"grp:win_space_toggle,caps:escape,compose:ralt\",

        sensitivity = 0.3,
        follow_mouse = 1,

        touchpad = {
            natural_scroll = true,
            tap_to_click = true,
            scroll_factor = 1.0,
        },
    },
})
";

    /// Renders `edits` into `INPUT_LUA` and returns the emitted text.
    fn render(edits: &[(SettingId, Value)]) -> String {
        let edit = render_input_lua(INPUT_LUA.as_bytes(), edits).expect("edits render");
        String::from_utf8(edit.contents).expect("valid utf-8")
    }

    /// The 0-based index of the line of `text` that is exactly `line`.
    fn line_of(text: &str, line: &str) -> usize {
        text.lines()
            .position(|candidate| candidate == line)
            .unwrap_or_else(|| panic!("the text contains the line {line:?}"))
    }

    /// Writes `contents` as `input.lua` in a fresh temp dir.
    fn input_lua_file(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.lua");
        fs::write(&path, contents).expect("write input.lua");
        (dir, path)
    }

    #[test]
    fn each_control_maps_to_the_right_table_path() {
        // The store-SettingId -> input.lua address map: the five Input settings resolve
        // to their `input.*` / `input.touchpad.*` paths with Lua key names (the tap
        // toggle has an underscore), and a foreign setting does not.
        assert_eq!(
            input_lua_path(SettingId::KeyboardLayouts),
            Some("input.kb_layout")
        );
        assert_eq!(
            input_lua_path(SettingId::KeyboardOptions),
            Some("input.kb_options")
        );
        assert_eq!(
            input_lua_path(SettingId::MouseSensitivity),
            Some("input.sensitivity")
        );
        assert_eq!(
            input_lua_path(SettingId::TouchpadNaturalScroll),
            Some("input.touchpad.natural_scroll")
        );
        assert_eq!(
            input_lua_path(SettingId::TouchpadTapToClick),
            Some("input.touchpad.tap_to_click")
        );
        assert_eq!(input_lua_path(SettingId::DimTimeout), None);
    }

    #[test]
    fn the_input_values_load_with_their_kinds() {
        // The read side: every Input setting maps from its Lua literal at the right
        // table depth, with the store value kind each row expects.
        let file = LuaConfigFile::parse(INPUT_LUA);
        assert_eq!(
            input_values(&file),
            vec![
                (
                    SettingId::KeyboardLayouts,
                    Value::String("us,se".to_string())
                ),
                (
                    SettingId::KeyboardOptions,
                    Value::String("grp:win_space_toggle,caps:escape,compose:ralt".to_string())
                ),
                (SettingId::MouseSensitivity, Value::Float(0.3)),
                (SettingId::TouchpadNaturalScroll, Value::Bool(true)),
                (SettingId::TouchpadTapToClick, Value::Bool(true)),
            ]
        );
    }

    #[test]
    fn an_integer_sensitivity_loads_as_a_float() {
        // A hand-edited `sensitivity = 0` is the same Lua number as `0.0`.
        let file = LuaConfigFile::parse(
            "hl.config({\n    input = {\n        sensitivity = 0,\n    },\n})\n",
        );
        assert!(input_values(&file).contains(&(SettingId::MouseSensitivity, Value::Float(0.0))));
    }

    #[test]
    fn a_literal_of_the_wrong_kind_is_not_loaded() {
        // `natural_scroll = 1` is a number where the toggle expects a boolean: the
        // setting is skipped (its row refuses edits) instead of being coerced, so the
        // app can never write it back re-typed.
        let file = LuaConfigFile::parse(
            "hl.config({\n    input = {\n        kb_layout = \"us\",\n        touchpad = {\n            natural_scroll = 1,\n        },\n    },\n})\n",
        );
        let values = input_values(&file);
        assert!(
            !values
                .iter()
                .any(|(id, _)| *id == SettingId::TouchpadNaturalScroll),
            "a mistyped option is not offered: {values:?}"
        );
        assert!(values.contains(&(SettingId::KeyboardLayouts, Value::String("us".to_string()))));
    }

    #[test]
    fn empty_keyboard_options_need_an_input_table() {
        // With an `input` table but no `kb_options` field, an empty KeyboardOptions is
        // seeded so a curated switch can stage a first option (the writer inserts the
        // field on Apply). Without an `input` table there is nothing to write into.
        let with_table = LuaConfigFile::parse(
            "hl.config({\n    input = {\n        kb_layout = \"us\",\n    },\n})\n",
        );
        assert!(
            input_values(&with_table)
                .contains(&(SettingId::KeyboardOptions, Value::String(String::new())))
        );

        let without_table = LuaConfigFile::parse("-- no input table here\n");
        assert!(input_values(&without_table).is_empty());
    }

    #[test]
    fn the_shipped_shape_passes_the_gate() {
        // The app-owned file as the dotfiles ship it is fully understood.
        assert!(check_input_lua(&LuaConfigFile::parse(INPUT_LUA)).is_ok());
    }

    #[test]
    fn an_opaque_line_inside_the_input_table_fails_the_gate() {
        // A computed value inside the table: the editor cannot know whether the
        // literals around it are the values in effect, so the page must hide.
        let text = INPUT_LUA.replace(
            "        follow_mouse = 1,\n",
            "        follow_mouse = docked and 0 or 1,\n",
        );
        let opaque_line = line_of(&text, "        follow_mouse = docked and 0 or 1,") + 1;
        match check_input_lua(&LuaConfigFile::parse(&text)) {
            Err(InputLuaUnavailable::OpaqueLines(lines)) => assert_eq!(lines, vec![opaque_line]),
            other => panic!("expected OpaqueLines, got {other:?}"),
        }
    }

    #[test]
    fn an_opaque_override_outside_the_input_table_fails_the_gate() {
        // A conditional `hl.config` after the main call is code outside every
        // addressable table, yet it can override `kb_layout` — the whole file is
        // checked, not just the `input` table.
        let text = format!(
            "{INPUT_LUA}if os.getenv(\"DOCKED\") then\n    hl.config({{ input = {{ kb_layout = \"de\" }} }})\nend\n"
        );
        let file = LuaConfigFile::parse(&text);
        assert_eq!(file.table_status(INPUT_TABLE), TableStatus::Clean);
        let first_added = INPUT_LUA.lines().count() + 1;
        match check_input_lua(&file) {
            Err(InputLuaUnavailable::OpaqueLines(lines)) => assert_eq!(
                lines,
                vec![first_added, first_added + 1, first_added + 2],
                "the three added lines are reported by number"
            ),
            other => panic!("expected OpaqueLines, got {other:?}"),
        }
    }

    #[test]
    fn block_comments_hide_the_page_but_line_comments_do_not() {
        // The editor treats every `--[[ … ]]` block comment as opaque — one spanning
        // several lines and one closed on its own line alike — and the gate checks the
        // whole file, so a block comment outside the `input` table still hides the
        // page. Plain `--` line comments (the shipped header) are harmless.
        let line_comment = format!("-- off\n{INPUT_LUA}");
        assert!(check_input_lua(&LuaConfigFile::parse(&line_comment)).is_ok());

        let one_line = format!("--[[ off ]]\n{INPUT_LUA}");
        assert!(matches!(
            check_input_lua(&LuaConfigFile::parse(&one_line)),
            Err(InputLuaUnavailable::OpaqueLines(lines)) if lines == vec![1]
        ));

        let first_added = INPUT_LUA.lines().count() + 1;
        let multi_line = format!("{INPUT_LUA}--[[\nkb_layout = \"de\",\n]]\n");
        assert!(matches!(
            check_input_lua(&LuaConfigFile::parse(&multi_line)),
            Err(InputLuaUnavailable::OpaqueLines(lines))
                if lines == vec![first_added, first_added + 1, first_added + 2]
        ));
    }

    #[test]
    fn unbalanced_structure_fails_the_gate() {
        let unclosed = INPUT_LUA.replace("    },\n})\n", "");
        assert!(matches!(
            check_input_lua(&LuaConfigFile::parse(&unclosed)),
            Err(InputLuaUnavailable::UnsafeStructure(_))
        ));
    }

    #[test]
    fn a_missing_input_table_fails_the_gate() {
        assert!(matches!(
            check_input_lua(&LuaConfigFile::parse(
                "hl.config({\n    general = {\n        gaps_in = 5,\n    },\n})\n"
            )),
            Err(InputLuaUnavailable::NoInputTable)
        ));
    }

    #[test]
    fn the_gate_reasons_name_lines_but_never_contents() {
        // R7.3: the logged reason may say *where*, never *what*.
        let text = INPUT_LUA.replace(
            "        follow_mouse = 1,\n",
            "        follow_mouse = secret_expr(),\n",
        );
        let line = line_of(&text, "        follow_mouse = secret_expr(),") + 1;
        let reason = check_input_lua(&LuaConfigFile::parse(&text))
            .expect_err("the opaque line fails the gate")
            .to_string();
        assert!(
            reason.contains(&format!("(lines {line})")),
            "names the line: {reason}"
        );
        assert!(!reason.contains("secret_expr"), "no contents: {reason}");
    }

    #[test]
    fn a_layout_reorder_rewrites_only_the_kb_layout_literal() {
        // The ordered layout list serialises to `kb_layout = "se,us",`, and reordering
        // it edits only that one line (R2.3).
        let edited = render(&[(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )]);
        let target = line_of(INPUT_LUA, "        kb_layout = \"us,se\",");
        assert_eq!(differing_lines(INPUT_LUA, &edited), vec![target]);
        assert_eq!(
            edited.lines().nth(target),
            Some("        kb_layout = \"se,us\",")
        );
    }

    #[test]
    fn an_unknown_kb_option_survives_an_edit_verbatim() {
        // Toggling a curated option off (here removing `caps:escape`, as the UI's
        // token toggle would) leaves the option the app has no switch for
        // (`compose:ralt`) untouched — the app never drops an unrecognised option.
        let edited = render(&[(
            SettingId::KeyboardOptions,
            Value::String("grp:win_space_toggle,compose:ralt".to_string()),
        )]);
        let target = line_of(
            INPUT_LUA,
            "        kb_options = \"grp:win_space_toggle,caps:escape,compose:ralt\",",
        );
        assert_eq!(differing_lines(INPUT_LUA, &edited), vec![target]);
        assert_eq!(
            edited.lines().nth(target),
            Some("        kb_options = \"grp:win_space_toggle,compose:ralt\",")
        );
    }

    #[test]
    fn each_input_edit_targets_its_own_line() {
        // The number and the two nested booleans each rewrite only their own literal,
        // at the right table depth, in their own Lua kind.
        let sensitivity = render(&[(SettingId::MouseSensitivity, Value::Float(0.5))]);
        let s_line = line_of(INPUT_LUA, "        sensitivity = 0.3,");
        assert_eq!(differing_lines(INPUT_LUA, &sensitivity), vec![s_line]);
        assert_eq!(
            sensitivity.lines().nth(s_line),
            Some("        sensitivity = 0.5,")
        );

        let natural = render(&[(SettingId::TouchpadNaturalScroll, Value::Bool(false))]);
        let n_line = line_of(INPUT_LUA, "            natural_scroll = true,");
        assert_eq!(differing_lines(INPUT_LUA, &natural), vec![n_line]);
        assert_eq!(
            natural.lines().nth(n_line),
            Some("            natural_scroll = false,")
        );

        let tap = render(&[(SettingId::TouchpadTapToClick, Value::Bool(false))]);
        let t_line = line_of(INPUT_LUA, "            tap_to_click = true,");
        assert_eq!(differing_lines(INPUT_LUA, &tap), vec![t_line]);
        assert_eq!(
            tap.lines().nth(t_line),
            Some("            tap_to_click = false,")
        );
    }

    #[test]
    fn kb_options_cleared_to_empty_writes_an_empty_string() {
        // Removing the last option writes `kb_options = "",` on the same line.
        let edited = render(&[(SettingId::KeyboardOptions, Value::String(String::new()))]);
        let target = line_of(
            INPUT_LUA,
            "        kb_options = \"grp:win_space_toggle,caps:escape,compose:ralt\",",
        );
        assert_eq!(differing_lines(INPUT_LUA, &edited), vec![target]);
        assert_eq!(
            edited.lines().nth(target),
            Some("        kb_options = \"\",")
        );
    }

    #[test]
    fn kb_options_is_inserted_when_absent() {
        // The seeded-empty case: a hand-written input.lua without `kb_options` gets
        // one new field before the `input` table's closing brace, indented like its
        // siblings; every original line is preserved.
        const NO_OPTIONS: &str = "\
hl.config({
    input = {
        kb_layout = \"us,se\",
        sensitivity = 0.3,
    },
})
";
        let edit = render_input_lua(
            NO_OPTIONS.as_bytes(),
            &[(
                SettingId::KeyboardOptions,
                Value::String("caps:escape".to_string()),
            )],
        )
        .expect("the insert renders");
        assert_eq!(
            String::from_utf8(edit.contents).expect("valid utf-8"),
            "\
hl.config({
    input = {
        kb_layout = \"us,se\",
        sensitivity = 0.3,
        kb_options = \"caps:escape\",
    },
})
"
        );
        assert_eq!(edit.changed_keys, vec!["input.kb_options".to_string()]);
    }

    #[test]
    fn an_option_the_file_omits_is_inserted_then_edited_in_place() {
        // The generic insert path, through an option the model has no setting for
        // (`kb_variant`, which input.lua omits until it is set — analysis §7.1): the
        // first write inserts one field; a second write to the same path rewrites that
        // field rather than adding a shadowing duplicate.
        let mut file = LuaConfigFile::parse(INPUT_LUA);
        let variant = LuaLiteral::string("nodeadkeys").expect("a plain string");
        set_or_insert(&mut file, "input.kb_variant", &variant).expect("inserts");
        let inserted = file.emit();
        assert_eq!(inserted.lines().count(), INPUT_LUA.lines().count() + 1);
        let new_line = line_of(&inserted, "        kb_variant = \"nodeadkeys\",");
        assert_eq!(
            inserted.lines().nth(new_line + 1),
            Some("    },"),
            "the field closes the input table"
        );

        let other = LuaLiteral::string("dvorak").expect("a plain string");
        set_or_insert(&mut file, "input.kb_variant", &other).expect("edits in place");
        let edited = file.emit();
        assert_eq!(differing_lines(&inserted, &edited), vec![new_line]);
        assert_eq!(
            edited.lines().nth(new_line),
            Some("        kb_variant = \"dvorak\",")
        );
    }

    #[test]
    fn a_kind_mismatch_on_disk_refuses_the_write() {
        // The file holds `natural_scroll = 1` (a number); a boolean is never written
        // over it, the whole render fails instead.
        let text = INPUT_LUA.replace("natural_scroll = true", "natural_scroll = 1");
        let error = render_input_lua(
            text.as_bytes(),
            &[(SettingId::TouchpadNaturalScroll, Value::Bool(false))],
        )
        .expect_err("a re-typed option is refused");
        assert_eq!(
            error,
            InputRenderError::Edit(LuaEditError::KindMismatch {
                path: "input.touchpad.natural_scroll".to_string(),
                existing: LuaLiteralKind::Number,
                requested: LuaLiteralKind::Boolean,
            })
        );
    }

    #[test]
    fn a_string_with_a_forbidden_character_refuses_the_write() {
        // R8.3: a `"` would end the Lua string early; the render fails rather than
        // writing a file Hyprland cannot load.
        let error = render_input_lua(
            INPUT_LUA.as_bytes(),
            &[(
                SettingId::KeyboardLayouts,
                Value::String("us\",se".to_string()),
            )],
        )
        .expect_err("a quote in a layout list is refused");
        assert!(matches!(
            error,
            InputRenderError::Edit(LuaEditError::Literal(LuaLiteralError::ForbiddenCharacter {
                character: '"',
                ..
            }))
        ));
    }

    #[test]
    fn a_wrong_kind_staged_value_fails_the_render() {
        // A boolean staged for the layout list has no Lua literal. Skipping it would
        // let the store commit a value that was never written, so the render fails
        // and names the path; an otherwise valid edit in the same batch does not mask
        // it.
        let error = render_input_lua(
            INPUT_LUA.as_bytes(),
            &[
                (SettingId::MouseSensitivity, Value::Float(0.5)),
                (SettingId::KeyboardLayouts, Value::Bool(true)),
            ],
        )
        .expect_err("a mistyped staged value is refused");
        assert_eq!(
            error,
            InputRenderError::ValueKind {
                path: "input.kb_layout",
                found: ValueKind::Bool,
            }
        );
        assert!(error.to_string().contains("input.kb_layout"));
    }

    #[test]
    fn no_edits_round_trips_the_file_byte_for_byte() {
        let edit = render_input_lua(INPUT_LUA.as_bytes(), &[]).expect("renders");
        assert_eq!(edit.contents, INPUT_LUA.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    /// A trimmed but structurally faithful `evdev.xml`: a `<layoutList>` with three base
    /// layouts (one carrying a variant that must be ignored, one using an XML entity)
    /// plus the surrounding `<modelList>`/`<optionList>` the scan must not confuse for
    /// layouts.
    const EVDEV_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xkbConfigRegistry version="1.1">
  <modelList>
    <model><configItem><name>pc105</name><description>Generic 105-key</description></configItem></model>
  </modelList>
  <layoutList>
    <layout>
      <configItem>
        <name>us</name>
        <shortDescription>en</shortDescription>
        <description>English (US)</description>
      </configItem>
      <variantList>
        <variant><configItem><name>dvorak</name><description>English (Dvorak)</description></configItem></variant>
      </variantList>
    </layout>
    <layout>
      <configItem>
        <name>se</name>
        <description>Swedish</description>
      </configItem>
    </layout>
    <layout>
      <configItem>
        <name>epo</name>
        <description>Esperanto (H &amp; X)</description>
      </configItem>
    </layout>
  </layoutList>
  <optionList>
    <option><configItem><name>caps:escape</name><description>Caps as Escape</description></configItem></option>
  </optionList>
</xkbConfigRegistry>
"#;

    #[test]
    fn parses_base_layouts_from_the_xkb_registry() {
        // The candidate list is the base layouts only — a variant (`dvorak`), the model,
        // and the option are excluded — with XML entities decoded and the list sorted by
        // description.
        let layouts = parse_xkb_layouts(EVDEV_XML);
        assert_eq!(
            layouts,
            vec![
                LayoutOption {
                    code: "us".to_string(),
                    description: "English (US)".to_string(),
                },
                LayoutOption {
                    code: "epo".to_string(),
                    description: "Esperanto (H & X)".to_string(),
                },
                LayoutOption {
                    code: "se".to_string(),
                    description: "Swedish".to_string(),
                },
            ],
            "base layouts only, entity-decoded, sorted by description"
        );
    }

    #[test]
    fn xkb_parse_degrades_on_absent_or_garbled_input() {
        // No registry structure -> no candidates, never a panic (R4.4).
        assert!(parse_xkb_layouts("").is_empty());
        assert!(parse_xkb_layouts("<html><body>not xkb</body></html>").is_empty());
        assert!(
            parse_xkb_layouts("<layoutList></layoutList>").is_empty(),
            "an empty layout list yields no candidates"
        );
    }

    #[test]
    fn read_xkb_layouts_degrades_when_the_registry_is_missing() {
        // A missing registry file degrades to an empty candidate list rather than an
        // error (R4.4).
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("no-such-evdev.xml");
        assert!(read_xkb_layouts(&missing).is_empty());
    }

    #[test]
    fn input_model_layout_options_come_from_the_registry() {
        // The model exposes the parsed candidates for the add-control (R2.3).
        let (dir, path) = input_lua_file(INPUT_LUA);
        let registry = dir.path().join("evdev.xml");
        fs::write(&registry, EVDEV_XML).expect("write registry fixture");
        let model = InputModel::load(path, &registry).expect("a usable input.lua");
        let codes: Vec<&str> = model
            .layout_options()
            .iter()
            .map(|l| l.code.as_str())
            .collect();
        assert_eq!(codes, vec!["us", "epo", "se"]);
    }

    #[test]
    fn the_model_is_not_built_for_a_missing_or_unusable_file() {
        // No model means no Input page (R4.2): a missing file and an opaque one both
        // report why.
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            InputModel::load(dir.path().join("input.lua"), Path::new("/nonexistent-xkb")),
            Err(InputLuaUnavailable::Unreadable(_))
        ));

        let (_dir, path) =
            input_lua_file(&INPUT_LUA.replace("follow_mouse = 1", "follow_mouse = f()"));
        assert!(matches!(
            InputModel::load(path, Path::new("/nonexistent-xkb")),
            Err(InputLuaUnavailable::OpaqueLines(_))
        ));
    }

    #[test]
    fn apply_contribution_is_none_when_clean() {
        // No dirty Input settings -> Ok(None), no write (the common clean case).
        let (_dir, path) = input_lua_file(INPUT_LUA);
        let model = InputModel::load(path, Path::new("/nonexistent-xkb")).expect("usable");
        assert!(matches!(model.apply_contribution(&[]), Ok(None)));
    }

    #[test]
    fn a_hand_edit_into_a_refusal_fails_the_contribution() {
        // The file passed the gate at load but was hand-edited since: a kind mismatch
        // must surface as an error so the Apply aborts, rather than skipping the write
        // and letting the store commit an unwritten value. The message names input.lua,
        // since the dialog quotes it.
        let (_dir, path) = input_lua_file(INPUT_LUA);
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb")).expect("usable");
        fs::write(
            &path,
            INPUT_LUA.replace("natural_scroll = true", "natural_scroll = 1"),
        )
        .expect("hand edit");
        let error = model
            .apply_contribution(&[(SettingId::TouchpadNaturalScroll, Value::Bool(false))])
            .expect_err("the refused edit fails the contribution");
        assert!(matches!(
            error,
            StoreWriteError::Render {
                error: InputRenderError::Edit(LuaEditError::KindMismatch { .. }),
                ..
            }
        ));
        assert!(
            error
                .to_string()
                .starts_with("the input.lua edit could not be applied: "),
            "the message names input.lua: {error}"
        );
    }

    #[test]
    fn a_dirty_edit_on_an_unreadable_file_is_an_error() {
        // A dirty Input edit against a file that vanished since load is an error (the
        // Apply aborts), never a silent skip that would let commit_apply promote an
        // unwritten value.
        let (_dir, path) = input_lua_file(INPUT_LUA);
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb")).expect("usable");
        fs::remove_file(&path).expect("remove input.lua");
        let error = model
            .apply_contribution(&[(SettingId::MouseSensitivity, Value::Float(0.5))])
            .expect_err("a missing input.lua must fail");
        assert!(matches!(error, StoreWriteError::Read { .. }));
        assert!(
            error
                .to_string()
                .starts_with("input.lua could not be read: "),
            "the message names input.lua: {error}"
        );
    }

    #[test]
    fn an_input_edit_applies_with_a_config_only_reload() {
        // The end-to-end store-SettingId -> FileWrite glue: a dirty Input setting
        // renders a surgical input.lua FileWrite whose diff is limited to the touched
        // line, and applying it through the shared pipeline writes that file and
        // triggers exactly `hyprctl reload config-only` — nothing else (R5.3, task
        // 11.4).
        let (_dir, path) = input_lua_file(INPUT_LUA);
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb")).expect("usable");

        let dirty = vec![(SettingId::MouseSensitivity, Value::Float(0.5))];
        let write = model
            .apply_contribution(&dirty)
            .expect("no error rendering the write")
            .expect("a dirty Input setting produces a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::InputLua);
        // The drift guard (tasks 9.10, 9.27) checks the plan's validations against the
        // settings each write declares, so a write that declared fewer than it carries
        // would hide its own drift. Pinned here: exactly the dirty settings it renders.
        let rendered: Vec<SettingId> = dirty.iter().map(|(id, _)| *id).collect();
        assert_eq!(write.validated_by, rendered);
        assert_eq!(write.changed_keys, vec!["input.sensitivity".to_string()]);

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline input.lua");

        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            links: Vec::new(),
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        match apply::run(&plan, &tracker, &caps, &runner, &signaller) {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(written, vec![path.clone()]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(
            differing_lines(INPUT_LUA, &on_disk),
            vec![line_of(INPUT_LUA, "        sensitivity = 0.3,")]
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("reload").arg("config-only")]
        );
        assert!(signaller.calls().is_empty());
    }

    #[test]
    fn a_second_apply_after_commit_is_not_a_self_conflict() {
        // Proves the window's fold-before-`store_writes`-capture plus `commit_apply`'s
        // re-baseline of input.lua work through the REAL renderer: load input.lua into
        // a real store, stage an Input edit, build the write via `apply_contribution`,
        // run `apply::run` over the store's freshness, commit, then stage a SECOND edit
        // and re-apply — the outcome must be `Applied`, not `Conflicted` (the app's own
        // first write must not read as an external change on the next apply, R5.6).
        let (_dir, path) = input_lua_file(INPUT_LUA);
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb")).expect("usable");

        // Load the real originals + freshness baseline, as the startup load does. The
        // conflict-reload path is not exercised here, so a trivial re-reader suffices.
        let reader: FileReader = Box::new(|p: &Path| {
            Ok(FileValues {
                bytes: fs::read(p)?,
                values: Vec::new(),
            })
        });
        let mut store = SettingsStore::new();
        let (bytes, file) = read_input_lua(&path).expect("usable");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: input_values(&file),
            },
            reader,
        );

        // No hyprctl, so the InputLua reload is gated out — this test is about the
        // conflict check and re-baseline, not the reload set.
        let caps = Capabilities::for_tests(&[], &[], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        store
            .stage(SettingId::MouseSensitivity, Value::Float(0.5))
            .expect("stage the first edit");
        let dirty = store.dirty_in_category(Category::Input);
        let write = model
            .apply_contribution(&dirty)
            .expect("no error")
            .expect("a write");
        let written_bytes = write.contents.clone();
        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            links: Vec::new(),
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let written = match apply::run(&plan, store.freshness(), &caps, &runner, &signaller) {
            ApplyOutcome::Applied { written, .. } => written,
            other => panic!("expected Applied on the first apply, got {other:?}"),
        };
        assert_eq!(written, vec![path.clone()]);
        store.commit_apply(&[(path.clone(), written_bytes)]);

        store
            .stage(
                SettingId::KeyboardLayouts,
                Value::String("se,us".to_string()),
            )
            .expect("stage the second edit");
        let dirty2 = store.dirty_in_category(Category::Input);
        let write2 = model
            .apply_contribution(&dirty2)
            .expect("no error")
            .expect("a write");
        let plan2 = ApplyPlan {
            validations: dirty2,
            writes: vec![write2],
            links: Vec::new(),
            palette: None,
            reload_params: ReloadParams::default(),
        };
        assert!(
            matches!(
                apply::run(&plan2, store.freshness(), &caps, &runner, &signaller),
                ApplyOutcome::Applied { .. }
            ),
            "the second apply must not self-conflict after the first commit re-baselined \
             input.lua through the real renderer"
        );
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert!(
            on_disk.contains("kb_layout = \"se,us\",") && on_disk.contains("sensitivity = 0.5,"),
            "both applied edits are on disk: {on_disk:?}"
        );
    }
}
