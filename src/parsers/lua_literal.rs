//! Lua literal values the app may write into Hyprland's Lua config or send to it
//! live, and the surgical line-oriented editor for the two Lua files the app
//! touches (tasks 11.2 and 11.3; architecture §3 "Lua literal" row, §6, §10;
//! analysis §7.1, §8.6; R5.3 item 1, R6.1, R8.3).
//!
//! # Why this module exists
//!
//! Hyprland ≥ 0.55 runs a **Lua** config provider: `~/.config/hypr/hyprland.lua`
//! is executed as Lua code and the app-owned `input.lua` is one
//! `hl.config({ input = { … } })` table constructor (analysis §7.1). Two parts of
//! the app produce Lua source text from user-chosen values:
//!
//! - the live-tweak helper [`HyprlandEval`](crate::core::reload::HyprlandEval),
//!   which renders an option change as `hyprctl eval 'hl.config({ … })'` because
//!   `hyprctl keyword` is rejected on a Lua session (task 11.2), and
//! - the surgical editor [`LuaConfigFile`] (task 11.3), which rewrites
//!   `key = <literal>,` lines in `input.lua` and the two `hl.env(…)` lines in
//!   `hyprland.lua`.
//!
//! Both must agree on exactly one thing: **which values can be written as a Lua
//! literal at all, and how**. That rule lives here, in one place, so the live call
//! and the file write can never disagree about a value (a value the file writer
//! refuses must not be applied live, and vice versa). Every value either of them
//! writes is a [`LuaLiteral`], and every string goes through
//! [`check_string_value`].
//!
//! # The forbidden-character rule (R8.3)
//!
//! A string is always written as a double-quoted Lua short string with **no
//! escaping**. Rather than escape awkward characters, the app refuses them:
//!
//! - `"` would close the literal early, and `\` would start an escape sequence;
//!   either changes what Lua reads (and, in a file, can break the whole config);
//! - a newline or carriage return is illegal inside a Lua short string, so it
//!   would be a syntax error that stops `hyprland.lua` from loading;
//! - every other control character (tab, NUL, …) is refused too. Lua would accept
//!   some of them raw, but none is a legitimate part of any value the app edits
//!   (layouts, option lists, cursor names, monitor modes), and a raw control
//!   character in a config line is invisible and confusing to a human editor.
//!
//! Rejecting instead of escaping is the stricter choice, and it keeps the writer
//! lossless: what the user chose is exactly what lands between the quotes, so a
//! re-read yields the same value with no escape decoding. Values that come from
//! the app's own pickers never contain these characters; the rule exists so a
//! hand-typed or externally sourced value can never corrupt a working desktop
//! (R8.3).
//!
//! # The editor: line-oriented, deliberately not a Lua parser
//!
//! [`LuaConfigFile`] keeps every physical line verbatim (terminator included), so
//! `parse → emit` with no edit reproduces the input byte for byte, and an edit
//! splices only the bytes of one literal. It understands a small, fixed set of
//! line *shapes* and nothing else:
//!
//! | Shape | Example | Meaning |
//! |---|---|---|
//! | config opener | `hl.config({` | opens the root option table of a top-level `hl.config` call |
//! | table opener | `touchpad = {` | opens the named sub-table `touchpad` of the current table |
//! | entry | `natural_scroll = true,` | one option with a literal value (string, number, boolean) |
//! | closer | `},` or `})` | closes the current table (and, for `})`, the `hl.config` call) |
//! | env call | `hl.env("XCURSOR_SIZE", "24")` | a top-level environment declaration |
//! | comment / blank | `-- …`, empty | byte-preserved, harmless |
//!
//! Anything else — an expression (`vrr = docked and 0 or 1,`), any other function
//! call, a single-quoted or multi-line string, a `--[[ … ]]` block comment, a
//! one-line table (`offset = { 2, 6 },`), a `local`, a loop — is preserved
//! verbatim and classified **opaque**. Each line may carry a trailing `-- comment`,
//! which is kept untouched by an edit.
//!
//! Why not a real Lua parser? `input.lua` is, by its own header, "schema-flat (one
//! `hl.config` call, no logic)", and `hyprland.lua` is real code of which the app
//! may touch exactly two lines (architecture §10). A full parser would be far more
//! code to get right and would still have to refuse the same things; a recogniser
//! for the few shapes the app writes, with everything else opaque, is small enough
//! to verify and fails safe: a shape it does not know can only make a value
//! *unaddressable*, never mis-edited.
//!
//! The editor still has to *lex* every line properly, because a brace inside a
//! string (`[[… hl.dsp.dpms({ action = "off" }) …]]` in `hyprland.lua`) or a
//! comment must not count as nesting. The lexer therefore knows Lua's short
//! strings (with escapes), long brackets (`[[ … ]]`, `[==[ … ]==]`, and the same
//! after `--`), comments, names and numbers, and tracks `{`/`(`/`[` nesting plus
//! the `function`/`if`/`do`/`repeat` … `end`/`until` block depth across lines.
//!
//! # Table paths
//!
//! An entry is addressed by its **table path**, the dotted chain of table keys
//! from the root of an `hl.config({ … })` call down to the entry:
//! `input.touchpad.natural_scroll` is the `natural_scroll` key inside
//! `touchpad = {` inside `input = {`. This is exactly the option path Hyprland
//! gives the value and the same dotted path
//! [`HyprlandEval::config`](crate::core::reload::HyprlandEval::config) takes, so a
//! caller can stage a file edit and the matching live tweak with one string.
//! Only the root table of a **top-level** `hl.config(` call (no open bracket, no
//! open `function`/`if`/loop block) starts a path: a table passed to any other call
//! (`hl.window_rule({ name = …, … })`) or built inside code has no option path, so
//! its lines are opaque — otherwise every rule's `name = …` would collide at one
//! address. Several `hl.config` calls in one file share the path space, as they
//! do in Hyprland.
//!
//! The same rule makes `hyprland.lua`'s own top-level `hl.config({ … })` table
//! addressable: `decoration.rounding` there is an entry like any in `input.lua`.
//! Architecture §3 restricts the app to [`LuaConfigFile::set_env`] in that file
//! (task 11.5). That restriction is a **caller contract**, not something this
//! type enforces, because a parsed [`LuaConfigFile`] does not know which file
//! its text came from.
//!
//! A UTF-8 byte-order mark at the start of a file is not special-cased: it lexes
//! as unknown symbols, so the first line is opaque. When that line is a comment
//! (as in `input.lua`) nothing else changes, since the line belongs to no table.
//! When it is the `hl.config({` itself, the call is not recognised and
//! everything in it is opaque (its tables are [`TableStatus::Missing`]). Neither
//! file the app edits carries a mark, and failing safe is preferable to
//! stripping bytes the lossless writer would then have to put back.
//!
//! # Duplicated keys: the last occurrence wins
//!
//! Hyprland applies `hl.config` calls (and the fields of one table constructor)
//! top to bottom, so when a path is assigned twice the **last** assignment is the
//! one in effect. [`LuaConfigFile::value`] and [`LuaConfigFile::set_value`] both
//! address the last entry with the path, the same rule as the hyprlang, env and
//! INI editors (task 9.8): editing an earlier, shadowed copy would silently have
//! no effect. `hl.env` calls follow the same rule — a later call for the same
//! name overwrites the variable. One case is the exception: inside a single
//! `hl.config` call a repeated *sub-table* (`input = { … }, input = { … }`)
//! replaces the earlier table wholesale rather than merging into it, and a key
//! used once as a sub-table and once as a scalar (`touchpad = { … }` and
//! `touchpad = 5`) leaves only the later one in effect. In both cases the later
//! occurrence is reported opaque instead of being addressed; a scalar that
//! replaces a sub-table is also counted against that sub-table's own
//! [`LuaConfigFile::table_status`], since every entry in it is dead.
//!
//! # Values keep their kind
//!
//! A staged value is written only in the kind of the literal it replaces: a
//! boolean stays a boolean (`natural_scroll = true` can never become `"true"`),
//! a string stays a string, a number stays a number (an integer may replace a
//! float and vice versa — Lua has one number type with two subtypes, and the
//! caller knows which spelling the option wants). A mismatch is an error
//! ([`LuaEditError::KindMismatch`]), never a silent re-typing, because a
//! re-typed option is a config error Hyprland reports at reload.
//!
//! # Opaque lines hide, they do not guess
//!
//! An opaque line inside an addressed table means the file holds something the
//! editor does not understand — a computed value, a conditional, a call. The
//! editor cannot know whether that line overrides, depends on, or is shadowed by
//! the entry it would edit, so it reports the table's state through
//! [`LuaConfigFile::table_status`] and lets the loader **hide** the page instead
//! of showing a value that may not be the one in effect (architecture §10, R4).
//! Brace or block structure the lexer cannot balance (an unclosed `{`, an extra
//! `end`, an unterminated string) is worse: nothing in the file can be located
//! reliably, so [`LuaConfigFile::structure_error`] reports it and every line is
//! treated as opaque — nothing is addressable and every edit is refused.
//!
//! Like every module under `parsers/`, this one never imports `gtk` (enforced by
//! `tests/module_boundaries.rs`).

use std::fmt;

use super::line::{self, RawLine, strip_newline};

/// Lua 5.4's reserved words, which are not valid table keys in the bare
/// `key = value` form the app writes (a reserved word would need the
/// `["key"] = value` form, which the app never emits).
const LUA_RESERVED_WORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// The kind of a Lua literal: what the editor compares before it overwrites a
/// value, so an option is never re-typed (see the module section "Values keep
/// their kind").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LuaLiteralKind {
    /// A double-quoted string literal (`"us,se"`).
    String,
    /// A bare numeral, integer or float (`1`, `0.3`, `-0.5`). Lua has a single
    /// number type with integer and float subtypes; both share this kind.
    Number,
    /// `true` or `false`.
    Boolean,
}

impl fmt::Display for LuaLiteralKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LuaLiteralKind::String => "string",
            LuaLiteralKind::Number => "number",
            LuaLiteralKind::Boolean => "boolean",
        })
    }
}

/// A Lua literal value as plain data: what [`LuaConfigFile::value`] reads out of
/// a file and what a [`LuaLiteral`] holds.
///
/// Unlike a [`LuaLiteral`], a `LuaValue` read from a file is not guaranteed to be
/// writable: an existing string may contain escape sequences (`"a\"b"` decodes to
/// `a"b`), which the forbidden-character rule refuses for new values. That is why
/// reading and writing use two types — a caller that wants to write a value back
/// must build a [`LuaLiteral`] from it, which re-applies the rule.
#[derive(Clone, Debug, PartialEq)]
pub enum LuaValue {
    /// A string, with any escape sequences of the source literal decoded.
    String(String),
    /// A number with Lua's integer subtype (`1`, `0x10`).
    Integer(i64),
    /// A number with Lua's float subtype (`0.3`, `1.0`, `1e-7`).
    Float(f64),
    /// `true` or `false`.
    Boolean(bool),
}

impl LuaValue {
    /// The literal kind of this value; both number subtypes are
    /// [`LuaLiteralKind::Number`].
    pub fn kind(&self) -> LuaLiteralKind {
        match self {
            LuaValue::String(_) => LuaLiteralKind::String,
            LuaValue::Integer(_) | LuaValue::Float(_) => LuaLiteralKind::Number,
            LuaValue::Boolean(_) => LuaLiteralKind::Boolean,
        }
    }

    /// The string, if this is a string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            LuaValue::String(value) => Some(value),
            _ => None,
        }
    }

    /// The boolean, if this is a boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            LuaValue::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// The integer, if this is a number with the integer subtype. A float such as
    /// `1.0` is **not** converted, so a caller can tell `1` from `1.0`.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            LuaValue::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// The number as an `f64`, for either subtype — what a caller wants for an
    /// option such as `sensitivity`, which a hand-edited file may spell `0` or
    /// `0.0`.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            // Every integer a config file plausibly holds is exactly representable;
            // above 2^53 the nearest float is the honest answer for a number read.
            LuaValue::Integer(value) => Some(*value as f64),
            LuaValue::Float(value) => Some(*value),
            _ => None,
        }
    }
}

/// A value that can be written verbatim as a Lua literal: a double-quoted string,
/// an integer, a finite float, or a boolean.
///
/// The inner value is private so a literal can only be built through the checked
/// constructors — a [`LuaLiteral`] that exists is always safe to render, and
/// [`fmt::Display`] (the rendering) cannot fail. Strings are checked by
/// [`check_string_value`]; floats must be finite, because `NaN`/`inf` have no Lua
/// literal spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct LuaLiteral(LuaValue);

impl LuaLiteral {
    /// A string literal, rendered double-quoted (`"us,se"`).
    ///
    /// Fails with [`LuaLiteralError::ForbiddenCharacter`] when `value` contains a
    /// character the forbidden-character rule refuses (see [`check_string_value`]).
    pub fn string(value: &str) -> Result<LuaLiteral, LuaLiteralError> {
        check_string_value(value)?;
        Ok(LuaLiteral(LuaValue::String(value.to_string())))
    }

    /// An integer literal, rendered bare (`10`). Every `i64` is representable.
    pub fn integer(value: i64) -> LuaLiteral {
        LuaLiteral(LuaValue::Integer(value))
    }

    /// A float literal, rendered bare and always with a fractional part or an
    /// exponent (`1.0`, `0.3`, `1e-7`) so Lua reads it as a float rather than an
    /// integer — matching how the dotfiles write float options (`scroll_factor =
    /// 1.0`).
    ///
    /// Fails with [`LuaLiteralError::NonFiniteNumber`] for `NaN` or an infinity,
    /// which have no Lua literal form.
    pub fn float(value: f64) -> Result<LuaLiteral, LuaLiteralError> {
        if !value.is_finite() {
            return Err(LuaLiteralError::NonFiniteNumber(value));
        }
        Ok(LuaLiteral(LuaValue::Float(value)))
    }

    /// A boolean literal, rendered bare (`true` / `false`).
    pub fn boolean(value: bool) -> LuaLiteral {
        LuaLiteral(LuaValue::Boolean(value))
    }

    /// The literal's kind, which [`LuaConfigFile::set_value`] compares with the
    /// kind of the literal it would replace (so `natural_scroll = true` is never
    /// rewritten as `"true"`).
    pub fn kind(&self) -> LuaLiteralKind {
        self.0.kind()
    }

    /// The literal's value as plain data (use the `as_*` accessors on it).
    pub fn value(&self) -> &LuaValue {
        &self.0
    }
}

impl fmt::Display for LuaLiteral {
    /// Renders the value as Lua source text: strings double-quoted with no
    /// escaping (the constructor already refused every character that would need
    /// one), numbers and booleans bare.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            LuaValue::String(value) => write!(f, "\"{value}\""),
            LuaValue::Integer(value) => write!(f, "{value}"),
            // `Debug` (not `Display`) keeps the trailing `.0` of an integral float,
            // and uses an exponent for very large/small magnitudes — both valid Lua.
            LuaValue::Float(value) => write!(f, "{value:?}"),
            LuaValue::Boolean(value) => write!(f, "{value}"),
        }
    }
}

/// Checks a string against the forbidden-character rule (see the module docs):
/// `"`, `\`, and every control character (which includes newline and carriage
/// return) are refused.
///
/// This is the single definition of the rule. [`LuaLiteral::string`] applies it,
/// and so does [`LuaConfigFile::set_env`] for the `hl.env` value it writes, so a
/// value refused for a live `hyprctl eval` is refused for the file too.
pub fn check_string_value(value: &str) -> Result<(), LuaLiteralError> {
    match value
        .chars()
        .find(|&character| character == '"' || character == '\\' || character.is_control())
    {
        Some(character) => Err(LuaLiteralError::ForbiddenCharacter {
            value: value.to_string(),
            character,
        }),
        None => Ok(()),
    }
}

/// Whether `name` can be written as a bare Lua table key (`name = value`): a Lua
/// *Name* (`[A-Za-z_][A-Za-z0-9_]*`) that is not a reserved word.
///
/// Used to validate every segment of a table path such as
/// `input.touchpad.natural_scroll` before it is rendered into Lua source, so a key
/// can never inject code into the generated call or the edited file.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !LUA_RESERVED_WORDS.contains(&name)
}

/// Why a value or key cannot be written as Lua source text.
#[derive(Clone, Debug, PartialEq)]
pub enum LuaLiteralError {
    /// A string value contains a character the forbidden-character rule refuses
    /// (`"`, `\`, or a control character such as a newline; see the module docs).
    ForbiddenCharacter {
        /// The refused value.
        value: String,
        /// The first refused character found in it.
        character: char,
    },
    /// A float is `NaN` or infinite, which has no Lua literal spelling.
    NonFiniteNumber(f64),
    /// A table key (or a segment of a dotted table path) is not a valid bare Lua
    /// key — empty, not a Lua *Name*, or a reserved word (see [`is_identifier`]).
    InvalidKey(String),
}

impl fmt::Display for LuaLiteralError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LuaLiteralError::ForbiddenCharacter { value, character } => write!(
                f,
                "`{}` contains {character:?}, which cannot appear in a Lua string value \
                 (double quotes, backslashes and control characters such as newlines are refused)",
                value.escape_debug()
            ),
            LuaLiteralError::NonFiniteNumber(value) => {
                write!(
                    f,
                    "{value} is not a finite number and has no Lua literal form"
                )
            }
            LuaLiteralError::InvalidKey(key) => write!(
                f,
                "`{}` is not a valid Lua table key (it must match [A-Za-z_][A-Za-z0-9_]* \
                 and not be a reserved word)",
                key.escape_debug()
            ),
        }
    }
}

impl std::error::Error for LuaLiteralError {}

// ---------------------------------------------------------------------------
// The surgical editor (task 11.3)
// ---------------------------------------------------------------------------

/// A parsed Lua config file (`input.lua` or `hyprland.lua`) that re-emits itself
/// byte for byte and edits individual literals in place.
///
/// Built by [`LuaConfigFile::parse`]. See the module documentation for the line
/// shapes it recognises, the table-path addressing, and why everything else is
/// reported as opaque.
#[derive(Clone, Debug)]
pub struct LuaConfigFile {
    /// The file's lines in order; concatenating their raw text is the file.
    lines: Vec<Line>,
    /// Every addressable table occurrence (an `hl.config` root table or a
    /// `key = {` sub-table inside one), in the order their openers appear.
    tables: Vec<TableSpan>,
    /// The first structural problem found, if any. When set, every non-blank line
    /// is opaque and `tables` is empty.
    structure_error: Option<StructureError>,
}

/// One physical line, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of the line **including its terminator** (`\n`,
    /// `\r\n`, or none for a final unterminated line). Only an edit mutates it.
    raw: String,
    /// How the line was classified.
    kind: LineKind,
    /// Index into [`LuaConfigFile::tables`] of the innermost addressable table
    /// that is open at the start of this line, or `None` outside every
    /// `hl.config` table. An opaque line inside a table counts against that table
    /// (and every table enclosing it) in [`LuaConfigFile::table_status`].
    table: Option<usize>,
}

impl RawLine for Line {
    fn raw(&self) -> &str {
        &self.raw
    }
}

/// The classification of one line. Byte offsets are relative to [`Line::raw`].
#[derive(Clone, Debug)]
enum LineKind {
    /// Empty or whitespace only.
    Blank,
    /// Only a `--` line comment (after optional indentation). A commented-out
    /// entry or `hl.env` call is a comment, never addressable.
    Comment,
    /// Anything the editor does not understand; preserved, never edited.
    Opaque,
    /// `hl.config({` at top level: opens a root option table.
    ConfigOpen,
    /// `key = {` inside an option table: opens a named sub-table.
    TableOpen {
        /// The full table path of the sub-table it opens.
        path: Vec<String>,
    },
    /// A line of closing brackets for addressable tables: `}` (optionally with a
    /// field separator) or `})` (closing the root table and the `hl.config` call).
    Close {
        /// Whether a `,`/`;` field separator follows the `}`.
        separator: bool,
        /// Where the code ends (before trailing whitespace or a comment): the
        /// position at which a separator is inserted when a field is appended
        /// after this line.
        code_end: usize,
    },
    /// `key = <literal>` with an optional `,`/`;`.
    Entry {
        /// The full table path of the entry, the key last.
        path: Vec<String>,
        /// Start of the literal's bytes (for a negative number, the `-`).
        value_start: usize,
        /// End (exclusive) of the literal's bytes.
        value_end: usize,
        /// The decoded value.
        value: LuaValue,
        /// Whether a `,`/`;` field separator follows the literal.
        separator: bool,
        /// Where the code ends (see [`LineKind::Close::code_end`]).
        code_end: usize,
    },
    /// `hl.env("NAME", "value")` at top level.
    Env {
        /// The decoded first argument.
        name: String,
        /// Start of the second argument's literal (its opening quote).
        value_start: usize,
        /// End (exclusive) of the second argument's literal.
        value_end: usize,
        /// The decoded second argument.
        value: String,
    },
}

/// One occurrence of an addressable table.
#[derive(Clone, Debug)]
struct TableSpan {
    /// The table's path; empty for the root table of an `hl.config` call.
    path: Vec<String>,
    /// Line index of the opener (`hl.config({` or `key = {`).
    open_line: usize,
    /// Line index of the line holding the closing `}`. Always set once the file
    /// parsed without a structure error.
    close_line: Option<usize>,
    /// Line indices of later, opaque entries in the same `hl.config` call that
    /// assign a scalar to this table's own path (`touchpad = 5` after
    /// `touchpad = { … }`), which discards the whole table in Lua. They count
    /// against this table in [`LuaConfigFile::table_status`].
    shadowed_by: Vec<usize>,
}

/// A brace, bracket, block or string structure the lexer could not balance —
/// the reason a whole file is treated as opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructureError {
    /// 1-based line number where the problem was detected (for an unclosed
    /// bracket or block, the line that opened it).
    line: usize,
    /// What is wrong.
    kind: StructureErrorKind,
}

impl StructureError {
    /// The 1-based line number the problem concerns.
    pub fn line(&self) -> usize {
        self.line
    }

    /// What is wrong.
    pub fn kind(&self) -> &StructureErrorKind {
        &self.kind
    }
}

impl fmt::Display for StructureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: ", self.line)?;
        match &self.kind {
            StructureErrorKind::UnmatchedClose(found) => {
                write!(f, "`{found}` closes nothing")
            }
            StructureErrorKind::MismatchedClose { opened, found } => {
                write!(f, "`{found}` cannot close the `{opened}` opened earlier")
            }
            StructureErrorKind::Unclosed(opened) => {
                write!(f, "`{opened}` opened here is never closed")
            }
            StructureErrorKind::UnterminatedString => {
                write!(f, "a string literal is not terminated")
            }
            StructureErrorKind::UnterminatedLongBracket => {
                write!(
                    f,
                    "a `[[ … ]]` string or `--[[ … ]]` comment is never closed"
                )
            }
            StructureErrorKind::UnmatchedEnd => {
                write!(
                    f,
                    "`end`/`until` closes no `function`/`if`/`do`/`repeat` block"
                )
            }
            StructureErrorKind::UnclosedBlock => {
                write!(
                    f,
                    "a `function`/`if`/`do`/`repeat` block opened here never ends"
                )
            }
        }
    }
}

/// The specific structural problem behind a [`StructureError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructureErrorKind {
    /// A `}`, `)` or `]` with nothing open to close.
    UnmatchedClose(char),
    /// A closing bracket of the wrong type (`)` while a `{` is innermost).
    MismatchedClose {
        /// The innermost open bracket.
        opened: char,
        /// The closing bracket found.
        found: char,
    },
    /// A `{`, `(` or `[` still open at end of file.
    Unclosed(char),
    /// A short (quoted) string runs to the end of a line without a closing
    /// quote or a `\` line continuation, or to the end of the file.
    UnterminatedString,
    /// A long bracket (`[[`, `--[[`, `[==[` …) is still open at end of file.
    UnterminatedLongBracket,
    /// An `end` or `until` with no open block.
    UnmatchedEnd,
    /// A `function`/`if`/`do`/`repeat` block still open at end of file.
    UnclosedBlock,
}

/// Whether an addressed table can be trusted, as reported by
/// [`LuaConfigFile::table_status`] — the signal a loader uses to show or hide a
/// page (architecture §10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableStatus {
    /// The table exists and every line in every occurrence of it (nested tables
    /// included) is a recognised shape, a comment or blank.
    Clean,
    /// The table exists but contains opaque lines, listed as 1-based line
    /// numbers. Its entries are still readable, but whether they are the values
    /// in effect cannot be known, so a loader should hide rather than guess.
    ContainsOpaque(Vec<usize>),
    /// No table with this path exists (or the path is not a valid dotted chain of
    /// Lua names).
    Missing,
    /// The file has a [`StructureError`]; nothing in it is addressable.
    UnsafeFile,
}

/// A failure from an edit method of [`LuaConfigFile`].
///
/// Every variant leaves the file completely unchanged: all checks run before any
/// byte is spliced.
#[derive(Clone, Debug, PartialEq)]
pub enum LuaEditError {
    /// A value, key or path segment cannot be written (a forbidden character in
    /// a string, or a key that is not a bare Lua name).
    Literal(LuaLiteralError),
    /// The dotted path is empty where an entry is required, or has an empty
    /// segment (`input..kb_layout`, a trailing `.`). The string is the path.
    EmptyPathSegment(String),
    /// The file has a [`StructureError`], so no line can be located reliably and
    /// every edit is refused.
    UnsafeFile(StructureError),
    /// No entry with this table path exists (the string is the dotted path).
    EntryNotFound(String),
    /// No addressable table with this path exists to insert into.
    TableNotFound(String),
    /// No top-level `hl.env("NAME", …)` call with this name exists.
    EnvNotFound(String),
    /// The new value's kind differs from the existing literal's; the editor never
    /// re-types an option.
    KindMismatch {
        /// The dotted path of the entry.
        path: String,
        /// The kind of the literal in the file.
        existing: LuaLiteralKind,
        /// The kind of the value that was to be written.
        requested: LuaLiteralKind,
    },
    /// [`LuaConfigFile::insert`] was asked to add a key the table already has
    /// (as an entry or a sub-table); use [`LuaConfigFile::set_value`] instead, so
    /// the file never gains a shadowing duplicate.
    KeyExists(String),
    /// The table to insert into contains opaque lines (1-based numbers), so the
    /// editor cannot tell where a new field may safely go.
    TableNotEditable {
        /// The dotted path of the table.
        path: String,
        /// The opaque lines inside it.
        opaque_lines: Vec<usize>,
    },
}

impl fmt::Display for LuaEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LuaEditError::Literal(error) => write!(f, "{error}"),
            LuaEditError::EmptyPathSegment(path) if path.is_empty() => {
                write!(f, "an empty path names no entry")
            }
            LuaEditError::EmptyPathSegment(path) => write!(
                f,
                "`{}` has an empty segment between dots, so it names no table or key",
                path.escape_debug()
            ),
            LuaEditError::UnsafeFile(error) => write!(
                f,
                "the file's structure cannot be followed ({error}), so it is not edited"
            ),
            LuaEditError::EntryNotFound(path) => write!(f, "no `{path} = <literal>` entry to edit"),
            LuaEditError::TableNotFound(path) => {
                write!(f, "no `{}` table to insert into", display_table_path(path))
            }
            LuaEditError::EnvNotFound(name) => write!(f, "no `hl.env(\"{name}\", …)` call to edit"),
            LuaEditError::KindMismatch {
                path,
                existing,
                requested,
            } => write!(
                f,
                "`{path}` holds a {existing}, so a {requested} value cannot be written to it"
            ),
            LuaEditError::KeyExists(path) => {
                write!(
                    f,
                    "`{path}` already exists, so it is edited rather than inserted"
                )
            }
            LuaEditError::TableNotEditable { path, opaque_lines } => write!(
                f,
                "the `{}` table contains lines that are not plain literals (lines {}), \
                 so no field is inserted into it",
                display_table_path(path),
                opaque_lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for LuaEditError {}

/// The root table of `hl.config` has the empty path; name it readably in errors.
fn display_table_path(path: &str) -> &str {
    if path.is_empty() {
        "hl.config({ … })"
    } else {
        path
    }
}

impl From<LuaLiteralError> for LuaEditError {
    fn from(error: LuaLiteralError) -> Self {
        LuaEditError::Literal(error)
    }
}

impl LuaConfigFile {
    /// Parses Lua source text into a lossless, editable representation.
    ///
    /// Parsing never fails and never loses a byte: [`emit`](Self::emit) always
    /// reproduces the input. A structural problem is recorded (see
    /// [`structure_error`](Self::structure_error)) and logged at `warn` with its
    /// line number only — never file contents (R7.3). Opaque lines are normal in
    /// `hyprland.lua` and are not logged.
    pub fn parse(input: &str) -> Self {
        let raws = line::split_raw_lines(input).map(str::to_string).collect();
        let file = analyze(raws);
        if let Some(error) = &file.structure_error {
            tracing::warn!(
                problem = %error,
                "Lua config structure cannot be followed; treating the file as opaque"
            );
        }
        file
    }

    /// Re-emits the file, byte-for-byte identical to the parsed input when no
    /// edit has been made (round-trip identity).
    pub fn emit(&self) -> String {
        line::emit(&self.lines)
    }

    /// The first structural problem found while parsing, if any. When this is
    /// `Some`, the whole file is opaque: nothing is addressable and every edit
    /// fails with [`LuaEditError::UnsafeFile`].
    pub fn structure_error(&self) -> Option<&StructureError> {
        self.structure_error.as_ref()
    }

    /// The 1-based numbers of every opaque line in the file, in order. Comments
    /// and blank lines are never opaque, except lines inside a multi-line string
    /// or block comment (which are part of that literal or comment).
    pub fn opaque_lines(&self) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| matches!(line.kind, LineKind::Opaque))
            .map(|(index, _)| index + 1)
            .collect()
    }

    /// Whether the table at the dotted `table_path` exists and holds only
    /// recognised lines — what a loader checks before showing a page backed by it
    /// (architecture §10). The empty path names the root table(s) of the
    /// `hl.config` calls; code outside every `hl.config` call belongs to no table
    /// and never affects the result. All occurrences of the table and every table
    /// nested in it are considered.
    pub fn table_status(&self, table_path: &str) -> TableStatus {
        if self.structure_error.is_some() {
            return TableStatus::UnsafeFile;
        }
        let Ok(path) = split_path(table_path, true) else {
            return TableStatus::Missing;
        };
        if !self.tables.iter().any(|table| table.path == path) {
            return TableStatus::Missing;
        }
        let mut opaque: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| matches!(line.kind, LineKind::Opaque))
            .filter(|(_, line)| {
                line.table
                    .is_some_and(|table| self.tables[table].path.starts_with(&path))
            })
            .map(|(index, _)| index + 1)
            .collect();
        // A scalar that replaced one of these tables sits in the parent table, so
        // the walk above misses it; it still makes the replaced table untrustworthy.
        opaque.extend(
            self.tables
                .iter()
                .filter(|table| table.path.starts_with(&path))
                .flat_map(|table| table.shadowed_by.iter().map(|index| index + 1)),
        );
        opaque.sort_unstable();
        opaque.dedup();
        if opaque.is_empty() {
            TableStatus::Clean
        } else {
            TableStatus::ContainsOpaque(opaque)
        }
    }

    /// The value of the entry at the dotted `path` (e.g.
    /// `input.touchpad.natural_scroll`), or `None` if there is no such entry.
    ///
    /// When the path is assigned more than once, this is the **last** assignment —
    /// the one Hyprland ends up with and the one [`set_value`](Self::set_value)
    /// edits. Comment and opaque lines are never read.
    pub fn value(&self, path: &str) -> Option<&LuaValue> {
        let path = split_path(path, false).ok()?;
        self.last_entry(&path)
            .and_then(|index| match &self.lines[index].kind {
                LineKind::Entry { value, .. } => Some(value),
                _ => None,
            })
    }

    /// Rewrites the literal of the entry at the dotted `path`, changing only that
    /// literal's bytes: the key, spacing, separator, a trailing comment and every
    /// other line stay byte-identical. The **last** entry with the path is edited
    /// (see the module docs).
    ///
    /// Errors: [`LuaEditError::UnsafeFile`] for a file with a structure error,
    /// [`LuaEditError::EmptyPathSegment`] or [`LuaEditError::Literal`] for an
    /// invalid path,
    /// [`LuaEditError::EntryNotFound`] when no entry has the path (use
    /// [`insert`](Self::insert) to add one), and [`LuaEditError::KindMismatch`]
    /// when `value` is not of the existing literal's kind.
    ///
    /// Unlike [`insert`](Self::insert), this deliberately does **not** require the
    /// enclosing table to be [`TableStatus::Clean`]: rewriting a recognised literal
    /// in place is safe whatever else the table holds. Whether that literal is
    /// the value in effect is the loader's decision, made through
    /// [`table_status`](Self::table_status) before a page is shown.
    pub fn set_value(&mut self, path: &str, value: &LuaLiteral) -> Result<(), LuaEditError> {
        self.ensure_safe()?;
        let segments = split_path(path, false)?;
        let found = self
            .last_entry(&segments)
            .and_then(|index| match &self.lines[index].kind {
                LineKind::Entry {
                    value_start,
                    value_end,
                    value: existing,
                    ..
                } => Some((index, *value_start, *value_end, existing.kind())),
                _ => None,
            });
        let Some((index, start, end, existing)) = found else {
            return Err(LuaEditError::EntryNotFound(path.to_string()));
        };
        if existing != value.kind() {
            return Err(LuaEditError::KindMismatch {
                path: path.to_string(),
                existing,
                requested: value.kind(),
            });
        }
        self.lines[index]
            .raw
            .replace_range(start..end, &value.to_string());
        self.reanalyze();
        Ok(())
    }

    /// Appends a new `key = value,` line to the table at the dotted `table_path`
    /// (the empty path is the `hl.config` root table), just before the line with
    /// the table's closing brace — used for `kb_variant`/`kb_model`/`kb_rules`,
    /// which `input.lua` does not declare until they are set.
    ///
    /// When the table occurs more than once, the **last** occurrence receives the
    /// line (it is the one Hyprland applies last). The new line copies the
    /// indentation of the last direct field of that table (an entry or a sub-table
    /// opener); an empty table gets its opener's indentation plus one indent unit —
    /// a tab if the opener is indented with tabs, otherwise four spaces, the
    /// dotfiles' style. The separator is always ` = ` and the line ends with `,`.
    /// If the field before the insertion point has no trailing separator (valid
    /// for a last field in Lua), a `,` is added right after it, because without
    /// it the new field would be a syntax error; that is the only case in which a
    /// second line changes.
    ///
    /// Errors: [`LuaEditError::UnsafeFile`]; [`LuaEditError::EmptyPathSegment`]
    /// or [`LuaEditError::Literal`] for an invalid path or key;
    /// [`LuaEditError::TableNotFound`];
    /// [`LuaEditError::KeyExists`] when the key is already present (as an entry
    /// or a sub-table) in any occurrence of the table; and
    /// [`LuaEditError::TableNotEditable`] when the table contains opaque lines.
    pub fn insert(
        &mut self,
        table_path: &str,
        key: &str,
        value: &LuaLiteral,
    ) -> Result<(), LuaEditError> {
        self.ensure_safe()?;
        let table = split_path(table_path, true)?;
        if !is_identifier(key) {
            return Err(LuaLiteralError::InvalidKey(key.to_string()).into());
        }
        match self.table_status(table_path) {
            TableStatus::Clean => {}
            TableStatus::ContainsOpaque(opaque_lines) => {
                return Err(LuaEditError::TableNotEditable {
                    path: table_path.to_string(),
                    opaque_lines,
                });
            }
            TableStatus::Missing | TableStatus::UnsafeFile => {
                return Err(LuaEditError::TableNotFound(table_path.to_string()));
            }
        }

        let mut full_path = table.clone();
        full_path.push(key.to_string());
        let key_taken = self.last_entry(&full_path).is_some()
            || self.tables.iter().any(|span| span.path == full_path);
        if key_taken {
            return Err(LuaEditError::KeyExists(full_path.join(".")));
        }

        let span_index = self
            .tables
            .iter()
            .rposition(|span| span.path == table)
            .ok_or_else(|| LuaEditError::TableNotFound(table_path.to_string()))?;
        let span = &self.tables[span_index];
        let open = span.open_line;
        // A clean table in a structurally sound file always has its closing line,
        // and that line is a recognised closer whose first `}` closes this table.
        let close = span
            .close_line
            .ok_or_else(|| LuaEditError::TableNotFound(table_path.to_string()))?;

        let sibling = (open + 1..close).rev().find(|&index| {
            self.lines[index].table == Some(span_index)
                && matches!(
                    self.lines[index].kind,
                    LineKind::Entry { .. } | LineKind::TableOpen { .. }
                )
        });
        let indent = match sibling {
            Some(index) => leading_whitespace(&self.lines[index].raw).to_string(),
            None => {
                let opener_indent = leading_whitespace(&self.lines[open].raw);
                let unit = if opener_indent.contains('\t') {
                    "\t"
                } else {
                    "    "
                };
                format!("{opener_indent}{unit}")
            }
        };

        // The last field before the insertion point (comments and blank lines
        // carry none). Inside a clean table it is an entry or the closer of a
        // nested table; the opener itself means the table has no field yet.
        let previous = (open + 1..close)
            .rev()
            .find(|&index| !matches!(self.lines[index].kind, LineKind::Blank | LineKind::Comment));
        let comma_at = previous.and_then(|index| match self.lines[index].kind {
            LineKind::Entry {
                separator: false,
                code_end,
                ..
            }
            | LineKind::Close {
                separator: false,
                code_end,
            } => Some((index, code_end)),
            _ => None,
        });

        let terminator = line::terminator_if_any_crlf(&self.lines);
        if let Some((index, code_end)) = comma_at {
            self.lines[index].raw.insert(code_end, ',');
        }
        self.lines.insert(
            close,
            Line {
                raw: format!("{indent}{key} = {value},{terminator}"),
                kind: LineKind::Blank,
                table: None,
            },
        );
        self.reanalyze();
        Ok(())
    }

    /// The value of the top-level `hl.env("name", "value")` call for `name`, or
    /// `None` when there is none. The **last** such call wins, as it does when
    /// Hyprland runs the file.
    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.last_env(name)
            .and_then(|index| match &self.lines[index].kind {
                LineKind::Env { value, .. } => Some(value.as_str()),
                _ => None,
            })
    }

    /// Rewrites the second argument of the last top-level
    /// `hl.env("name", "…")` call — the only edit the app makes to
    /// `hyprland.lua` (the two cursor lines, R3.4). Only the string literal
    /// changes; the call, its spacing, any trailing comment and every other line
    /// stay byte-identical. A commented-out call, or one inside a block or
    /// another call, is never matched.
    ///
    /// Errors: [`LuaEditError::UnsafeFile`]; [`LuaEditError::Literal`] when
    /// `value` breaks the forbidden-character rule; [`LuaEditError::EnvNotFound`].
    pub fn set_env(&mut self, name: &str, value: &str) -> Result<(), LuaEditError> {
        self.ensure_safe()?;
        let literal = LuaLiteral::string(value)?;
        let found = self
            .last_env(name)
            .and_then(|index| match &self.lines[index].kind {
                LineKind::Env {
                    value_start,
                    value_end,
                    ..
                } => Some((index, *value_start, *value_end)),
                _ => None,
            });
        let Some((index, start, end)) = found else {
            return Err(LuaEditError::EnvNotFound(name.to_string()));
        };
        self.lines[index]
            .raw
            .replace_range(start..end, &literal.to_string());
        self.reanalyze();
        Ok(())
    }

    /// Fails with [`LuaEditError::UnsafeFile`] when the file has a structure
    /// error.
    fn ensure_safe(&self) -> Result<(), LuaEditError> {
        match &self.structure_error {
            Some(error) => Err(LuaEditError::UnsafeFile(error.clone())),
            None => Ok(()),
        }
    }

    /// The index of the last entry line whose path equals `path`.
    fn last_entry(&self, path: &[String]) -> Option<usize> {
        self.lines.iter().rposition(|line| match &line.kind {
            LineKind::Entry {
                path: entry_path, ..
            } => entry_path == path,
            _ => false,
        })
    }

    /// The index of the last top-level `hl.env` line for `name`.
    fn last_env(&self, name: &str) -> Option<usize> {
        self.lines.iter().rposition(
            |line| matches!(&line.kind, LineKind::Env { name: env_name, .. } if env_name == name),
        )
    }

    /// Re-classifies the lines after an edit, so the line kinds, offsets and
    /// table spans always describe the current text. An edit only ever writes a
    /// recognised shape, so the result is structurally sound again.
    fn reanalyze(&mut self) {
        let raws = std::mem::take(&mut self.lines)
            .into_iter()
            .map(|line| line.raw)
            .collect();
        *self = analyze(raws);
        debug_assert!(
            self.structure_error.is_none(),
            "an edit must never break the file's structure"
        );
    }
}

/// Splits a dotted path into its segments, checking each is a bare Lua key.
/// `allow_empty` admits the empty path (the `hl.config` root table).
fn split_path(path: &str, allow_empty: bool) -> Result<Vec<String>, LuaEditError> {
    if path.is_empty() {
        return if allow_empty {
            Ok(Vec::new())
        } else {
            Err(LuaEditError::EmptyPathSegment(String::new()))
        };
    }
    path.split('.')
        .map(|segment| {
            if segment.is_empty() {
                Err(LuaEditError::EmptyPathSegment(path.to_string()))
            } else if is_identifier(segment) {
                Ok(segment.to_string())
            } else {
                Err(LuaLiteralError::InvalidKey(segment.to_string()).into())
            }
        })
        .collect()
}

/// The leading spaces and tabs of a line.
fn leading_whitespace(raw: &str) -> &str {
    &raw[..raw.len() - raw.trim_start_matches([' ', '\t']).len()]
}

// ---------------------------------------------------------------------------
// Line analysis
// ---------------------------------------------------------------------------

/// An open bracket during analysis.
#[derive(Clone, Copy, Debug)]
enum Frame {
    /// The `(` of a top-level `hl.config(` call, with its 0-based line.
    ConfigCall { line: usize },
    /// The `{` of an addressable table; the index is into the table spans.
    Table(usize),
    /// Any other open bracket (`{`, `(` or `[`), with the 0-based line that
    /// opened it for diagnostics.
    Other { bracket: u8, line: usize },
}

impl Frame {
    /// The opening bracket byte of the frame.
    fn bracket(self) -> u8 {
        match self {
            Frame::ConfigCall { .. } => b'(',
            Frame::Table(_) => b'{',
            Frame::Other { bracket, .. } => bracket,
        }
    }
}

/// The state carried from line to line while classifying a file.
struct Analyzer {
    /// Whether a long bracket or a continued short string is open.
    mode: LexMode,
    /// The open brackets, outermost first.
    frames: Vec<Frame>,
    /// The 0-based lines that opened each still-open `function`/`if`/`do`/
    /// `repeat` block, outermost first.
    blocks: Vec<usize>,
    /// The addressable table occurrences found so far.
    tables: Vec<TableSpan>,
    /// The entry paths assigned so far in the current `hl.config` call, so a
    /// later `key = {` for one of them can be recognised as replacing it.
    call_entries: Vec<Vec<String>>,
}

/// Classifies every line of a file, producing a [`LuaConfigFile`]. On the first
/// structural problem the analysis stops and every non-blank line becomes opaque.
fn analyze(raws: Vec<String>) -> LuaConfigFile {
    let mut analyzer = Analyzer {
        mode: LexMode::Code,
        frames: Vec::new(),
        blocks: Vec::new(),
        tables: Vec::new(),
        call_entries: Vec::new(),
    };
    let mut lines = Vec::with_capacity(raws.len());
    let mut error = None;

    for (index, raw) in raws.iter().enumerate() {
        match analyzer.line(index, strip_newline(raw)) {
            Ok((kind, table)) => lines.push(Line {
                raw: raw.clone(),
                kind,
                table,
            }),
            Err(kind) => {
                error = Some(StructureError {
                    line: index + 1,
                    kind,
                });
                break;
            }
        }
    }
    if error.is_none() {
        error = analyzer.finish(raws.len());
    }

    if error.is_some() {
        let lines = raws
            .into_iter()
            .map(|raw| {
                let kind = if strip_newline(&raw).trim().is_empty() {
                    LineKind::Blank
                } else {
                    LineKind::Opaque
                };
                Line {
                    raw,
                    kind,
                    table: None,
                }
            })
            .collect();
        return LuaConfigFile {
            lines,
            tables: Vec::new(),
            structure_error: error,
        };
    }

    LuaConfigFile {
        lines,
        tables: analyzer.tables,
        structure_error: None,
    }
}

impl Analyzer {
    /// Classifies one line (0-based `index`, terminator stripped) and applies its
    /// bracket and block effects. Returns the kind and the innermost addressable
    /// table open at the start of the line.
    fn line(
        &mut self,
        index: usize,
        content: &str,
    ) -> Result<(LineKind, Option<usize>), StructureErrorKind> {
        let table = self.innermost_table();
        let started_in_code = self.mode == LexMode::Code;
        let (tokens, end_mode) = lex_line(content, self.mode)?;
        self.mode = end_mode;

        // A line that starts inside a long bracket or a continued string is a
        // piece of a multi-line literal or comment: opaque, whatever it holds.
        let kind = if started_in_code {
            self.classify(&tokens, content, index)
        } else {
            LineKind::Opaque
        };

        match &kind {
            LineKind::ConfigOpen => {
                self.call_entries.clear();
                self.frames.push(Frame::ConfigCall { line: index });
                self.open_table(Vec::new(), index);
            }
            LineKind::Entry { path, .. } => {
                self.call_entries.push(path.clone());
                self.track_brackets(&tokens, index)?;
            }
            LineKind::TableOpen { path } => self.open_table(path.clone(), index),
            // Closers were validated against the frame stack by `classify`, so the
            // generic pass pops exactly the table (and call) frames they name.
            _ => self.track_brackets(&tokens, index)?,
        }
        self.track_blocks(&tokens, content, index)?;
        Ok((kind, table))
    }

    /// Checks the end-of-file state: nothing may remain open.
    fn finish(&self, line_count: usize) -> Option<StructureError> {
        let last_line = line_count.max(1);
        match self.mode {
            LexMode::Code => {}
            LexMode::LongBracket { .. } => {
                return Some(StructureError {
                    line: last_line,
                    kind: StructureErrorKind::UnterminatedLongBracket,
                });
            }
            LexMode::ShortString { .. } => {
                return Some(StructureError {
                    line: last_line,
                    kind: StructureErrorKind::UnterminatedString,
                });
            }
        }
        if let Some(frame) = self.frames.first() {
            let line = match *frame {
                Frame::Other { line, .. } | Frame::ConfigCall { line } => line,
                Frame::Table(span) => self.tables[span].open_line,
            };
            return Some(StructureError {
                line: line + 1,
                kind: StructureErrorKind::Unclosed(frame.bracket() as char),
            });
        }
        self.blocks.first().map(|&line| StructureError {
            line: line + 1,
            kind: StructureErrorKind::UnclosedBlock,
        })
    }

    /// The innermost open addressable table, if any.
    fn innermost_table(&self) -> Option<usize> {
        self.frames.iter().rev().find_map(|frame| match frame {
            Frame::Table(span) => Some(*span),
            _ => None,
        })
    }

    /// Whether the current position is directly inside an addressable table:
    /// the innermost bracket is such a table and no code block is open (a
    /// `key = value` inside a function body is a statement, not a field).
    fn in_option_table(&self) -> bool {
        self.blocks.is_empty() && matches!(self.frames.last(), Some(Frame::Table(_)))
    }

    /// Whether the current position is plain top level: no open bracket or block.
    fn at_top_level(&self) -> bool {
        self.frames.is_empty() && self.blocks.is_empty()
    }

    /// The indices of the table occurrences with `path` opened so far in the
    /// current `hl.config` call (tables are recorded in opener order, so they are
    /// the ones from the call's root table onwards).
    fn call_tables_with(&self, path: &[String]) -> Vec<usize> {
        let call_root = match self.frames.get(1) {
            Some(Frame::Table(root)) => *root,
            _ => return Vec::new(),
        };
        (call_root..self.tables.len())
            .filter(|&span| self.tables[span].path == path)
            .collect()
    }

    /// The table path of `key` inside the innermost open addressable table.
    fn child_path(&self, key: String) -> Vec<String> {
        let mut path = self
            .innermost_table()
            .map(|table| self.tables[table].path.clone())
            .unwrap_or_default();
        path.push(key);
        path
    }

    /// Records a new table occurrence and pushes its frame.
    fn open_table(&mut self, path: Vec<String>, index: usize) {
        self.tables.push(TableSpan {
            path,
            open_line: index,
            close_line: None,
            shadowed_by: Vec::new(),
        });
        self.frames.push(Frame::Table(self.tables.len() - 1));
    }

    /// Matches a line's tokens against the recognised shapes (see the module
    /// docs). Anything that does not match exactly is opaque. `index` is the
    /// 0-based line, recorded on a sub-table that a scalar entry replaces.
    fn classify(&mut self, tokens: &[Token], content: &str, index: usize) -> LineKind {
        // A trailing `--` comment is kept but plays no part in the shape.
        let code = match tokens.last() {
            Some(token) if token.kind == TokenKind::LineComment => &tokens[..tokens.len() - 1],
            _ => tokens,
        };
        if code.is_empty() {
            return if tokens.is_empty() {
                LineKind::Blank
            } else {
                LineKind::Comment
            };
        }
        if code
            .iter()
            .any(|token| token.kind == TokenKind::Unclassifiable)
        {
            return LineKind::Opaque;
        }
        let text = |token: &Token| &content[token.start..token.end];
        let is_name =
            |token: &Token, name: &str| token.kind == TokenKind::Name && text(token) == name;
        let is_symbol = |token: &Token, symbol: u8| token.kind == TokenKind::Symbol(symbol);
        let is_separator = |token: &Token| is_symbol(token, b',') || is_symbol(token, b';');
        let code_end = code[code.len() - 1].end;

        if self.at_top_level() {
            // hl.config({
            if code.len() == 5
                && is_name(&code[0], "hl")
                && is_symbol(&code[1], b'.')
                && is_name(&code[2], "config")
                && is_symbol(&code[3], b'(')
                && is_symbol(&code[4], b'{')
            {
                return LineKind::ConfigOpen;
            }
            // hl.env("NAME", "value") with an optional `;`
            if (code.len() == 8 || (code.len() == 9 && is_symbol(&code[8], b';')))
                && is_name(&code[0], "hl")
                && is_symbol(&code[1], b'.')
                && is_name(&code[2], "env")
                && is_symbol(&code[3], b'(')
                && is_symbol(&code[5], b',')
                && is_symbol(&code[7], b')')
            {
                if let (Some(name), Some(value)) = (
                    double_quoted(&code[4], content),
                    double_quoted(&code[6], content),
                ) {
                    return LineKind::Env {
                        name,
                        value_start: code[6].start,
                        value_end: code[6].end,
                        value,
                    };
                }
            }
            return LineKind::Opaque;
        }

        if !self.in_option_table() {
            return LineKind::Opaque;
        }

        // `}` closing a sub-table, optionally followed by a field separator; or
        // `})` closing a root table and its `hl.config` call, optionally with `;`.
        if is_symbol(&code[0], b'}') {
            let closes_root = matches!(
                self.frames.len().checked_sub(2).map(|i| self.frames[i]),
                Some(Frame::ConfigCall { .. })
            );
            let shape_ok = if closes_root {
                code.len() >= 2
                    && is_symbol(&code[1], b')')
                    && (code.len() == 2 || (code.len() == 3 && is_symbol(&code[2], b';')))
            } else {
                code.len() == 1 || (code.len() == 2 && is_separator(&code[1]))
            };
            if !shape_ok {
                return LineKind::Opaque;
            }
            return LineKind::Close {
                separator: !closes_root && code.len() == 2,
                code_end,
            };
        }

        // Every remaining shape starts with `name =`.
        if code.len() < 3
            || code[0].kind != TokenKind::Name
            || !is_identifier(text(&code[0]))
            || !is_symbol(&code[1], b'=')
        {
            return LineKind::Opaque;
        }
        let key = text(&code[0]).to_string();

        // key = {
        if code.len() == 3 && is_symbol(&code[2], b'{') {
            // Within one table constructor a repeated field *replaces* the earlier
            // one, so a second `input = {` (or an `input = {` after a scalar
            // `input = …`) in the same `hl.config` call discards what came first —
            // unlike two separate calls, which Hyprland merges option by option.
            // The last-occurrence rule would then read dead values, so the repeat
            // is opaque and the loader hides the page instead.
            let path = self.child_path(key);
            if !self.call_tables_with(&path).is_empty() || self.call_entries.contains(&path) {
                return LineKind::Opaque;
            }
            return LineKind::TableOpen { path };
        }

        // key = <literal> with an optional separator
        let Some((value, literal_tokens)) = literal_at(&code[2..], content) else {
            return LineKind::Opaque;
        };
        let rest = &code[2 + literal_tokens..];
        let separator = match rest {
            [] => false,
            [token] if is_separator(token) => true,
            _ => return LineKind::Opaque,
        };
        // A scalar replacing a sub-table of the same call (`touchpad = 5` after
        // `touchpad = { … }`): the same replacement rule, the other way round.
        // Every entry of the replaced table is dead, so it is told too.
        let path = self.child_path(key);
        let replaced = self.call_tables_with(&path);
        if !replaced.is_empty() {
            for span in replaced {
                self.tables[span].shadowed_by.push(index);
            }
            return LineKind::Opaque;
        }
        LineKind::Entry {
            path,
            value_start: code[2].start,
            value_end: code[1 + literal_tokens].end,
            value,
            separator,
            code_end,
        }
    }

    /// Applies the bracket tokens of a line to the frame stack. An opener pushes
    /// a generic frame (only `classify`'s two opener shapes push table frames); a
    /// closer pops the innermost frame, which must be of the matching type, and
    /// records the closing line of a table it ends.
    fn track_brackets(&mut self, tokens: &[Token], index: usize) -> Result<(), StructureErrorKind> {
        for token in tokens {
            let TokenKind::Symbol(symbol) = token.kind else {
                continue;
            };
            let opener = match symbol {
                b'{' | b'(' | b'[' => {
                    self.frames.push(Frame::Other {
                        bracket: symbol,
                        line: index,
                    });
                    continue;
                }
                b'}' => b'{',
                b')' => b'(',
                b']' => b'[',
                _ => continue,
            };
            let frame = self
                .frames
                .pop()
                .ok_or(StructureErrorKind::UnmatchedClose(symbol as char))?;
            if frame.bracket() != opener {
                return Err(StructureErrorKind::MismatchedClose {
                    opened: frame.bracket() as char,
                    found: symbol as char,
                });
            }
            if let Frame::Table(span) = frame {
                self.tables[span].close_line = Some(index);
            }
        }
        Ok(())
    }

    /// Tracks Lua's keyword-delimited blocks. `function`, `if`, `do` and `repeat`
    /// open one; `end` and `until` close one. `while … do` and `for … do` are
    /// counted through their `do`, and `then`/`else`/`elseif` do not change the
    /// depth, so every block is counted exactly once.
    fn track_blocks(
        &mut self,
        tokens: &[Token],
        content: &str,
        index: usize,
    ) -> Result<(), StructureErrorKind> {
        for token in tokens.iter().filter(|token| token.kind == TokenKind::Name) {
            match &content[token.start..token.end] {
                "function" | "if" | "do" | "repeat" => self.blocks.push(index),
                "end" | "until" => {
                    self.blocks.pop().ok_or(StructureErrorKind::UnmatchedEnd)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Decodes a token that is a complete double-quoted string. Single-quoted strings
/// are deliberately not recognised: the app only ever writes double quotes, and
/// an unfamiliar spelling is safer opaque than guessed at.
fn double_quoted(token: &Token, content: &str) -> Option<String> {
    if token.kind != (TokenKind::String { quote: b'"' }) {
        return None;
    }
    decode_short_string(&content[token.start + 1..token.end - 1])
}

/// Reads a literal at the start of `tokens`: a double-quoted string, `true`/
/// `false`, a numeral, or `-` followed by a numeral. Returns the value and how
/// many tokens it spans.
fn literal_at(tokens: &[Token], content: &str) -> Option<(LuaValue, usize)> {
    let first = tokens.first()?;
    let text = |token: &Token| &content[token.start..token.end];
    match first.kind {
        TokenKind::String { .. } => {
            double_quoted(first, content).map(|value| (LuaValue::String(value), 1))
        }
        TokenKind::Name => match text(first) {
            "true" => Some((LuaValue::Boolean(true), 1)),
            "false" => Some((LuaValue::Boolean(false), 1)),
            _ => None,
        },
        TokenKind::Number => parse_number(text(first)).map(|value| (value, 1)),
        TokenKind::Symbol(b'-') => {
            let number = tokens
                .get(1)
                .filter(|token| token.kind == TokenKind::Number)?;
            let value = match parse_number(text(number))? {
                LuaValue::Integer(value) => LuaValue::Integer(value.checked_neg()?),
                LuaValue::Float(value) => LuaValue::Float(-value),
                _ => return None,
            };
            Some((value, 2))
        }
        _ => None,
    }
}

/// Parses a Lua numeral token: a decimal or hexadecimal integer that fits an
/// `i64`, or a finite decimal float. Anything else (hex floats, an integer that
/// Lua would silently turn into a float) is not recognised, so its line is opaque.
fn parse_number(text: &str) -> Option<LuaValue> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(LuaValue::Integer);
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().ok().map(LuaValue::Integer);
    }
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(LuaValue::Float)
}

/// Decodes the body of a Lua short string (between the quotes), resolving every
/// escape sequence Lua 5.4 defines. Returns `None` for an invalid escape or a
/// result that is not UTF-8, which makes the line opaque rather than misread.
fn decode_short_string(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let escape = *bytes.get(i + 1)?;
        i += 2;
        match escape {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' | b'"' | b'\'' => out.push(escape),
            // `\z` skips the whitespace that follows it.
            b'z' => {
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
            }
            b'x' => {
                let hex = body.get(i..i + 2)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b'0'..=b'9' => {
                // Up to three decimal digits, starting with the escape itself.
                let start = i - 1;
                let mut end = start;
                while end < bytes.len() && end < start + 3 && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                out.push(body[start..end].parse::<u8>().ok()?);
                i = end;
            }
            b'u' => {
                let rest = body.get(i..)?.strip_prefix('{')?;
                let close = rest.find('}')?;
                let code = u32::from_str_radix(&rest[..close], 16).ok()?;
                let mut buffer = [0; 4];
                out.extend_from_slice(char::from_u32(code)?.encode_utf8(&mut buffer).as_bytes());
                i += 1 + close + 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

/// What the lexer is inside of at a line boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexMode {
    /// Ordinary code.
    Code,
    /// A long bracket (`[==[ … ]==]`, as a string or after `--` as a comment)
    /// with `level` equals signs.
    LongBracket { level: usize },
    /// A short string continued onto the next line by a trailing `\` (or `\z`).
    ShortString { quote: u8 },
}

/// The lexical class of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    /// A Lua name (identifier or keyword).
    Name,
    /// A short string that opens and closes on this line.
    String { quote: u8 },
    /// A numeral.
    Number,
    /// Any other single byte outside strings and comments (brackets, `=`, `,`,
    /// `.`, operators).
    Symbol(u8),
    /// A `--` comment running to the end of the line.
    LineComment,
    /// A piece of a long bracket string or comment, or of a short string that
    /// continues onto another line — never part of a recognised shape.
    Unclassifiable,
}

/// One token: its class and its byte range within the line.
#[derive(Clone, Copy, Debug)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

/// Splits one line (without terminator) into tokens, starting in `mode`, and
/// returns the tokens and the mode at the end of the line. Fails only for a short
/// string that is neither closed nor continued on its line.
fn lex_line(content: &str, mut mode: LexMode) -> Result<(Vec<Token>, LexMode), StructureErrorKind> {
    let bytes = content.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let push = |tokens: &mut Vec<Token>, kind, start, end| tokens.push(Token { kind, start, end });

    loop {
        match mode {
            LexMode::LongBracket { level } => match find_long_close(bytes, i, level) {
                Some(end) => {
                    push(&mut tokens, TokenKind::Unclassifiable, i, end);
                    i = end;
                    mode = LexMode::Code;
                }
                None => {
                    push(&mut tokens, TokenKind::Unclassifiable, i, bytes.len());
                    return Ok((tokens, mode));
                }
            },
            LexMode::ShortString { quote } => match scan_short_string(bytes, i, quote) {
                ShortString::Closed(end) => {
                    push(&mut tokens, TokenKind::Unclassifiable, i, end);
                    i = end;
                    mode = LexMode::Code;
                }
                ShortString::Continued => {
                    push(&mut tokens, TokenKind::Unclassifiable, i, bytes.len());
                    return Ok((tokens, mode));
                }
                ShortString::Unterminated => return Err(StructureErrorKind::UnterminatedString),
            },
            LexMode::Code => {
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | 0x0b | 0x0c) {
                    i += 1;
                }
                let Some(&byte) = bytes.get(i) else {
                    return Ok((tokens, mode));
                };
                let start = i;
                if bytes[i..].starts_with(b"--") {
                    if let Some((level, after)) = long_open(bytes, i + 2) {
                        push(&mut tokens, TokenKind::Unclassifiable, start, after);
                        i = after;
                        mode = LexMode::LongBracket { level };
                    } else {
                        push(&mut tokens, TokenKind::LineComment, start, bytes.len());
                        return Ok((tokens, mode));
                    }
                } else if let Some((level, after)) = long_open(bytes, i) {
                    push(&mut tokens, TokenKind::Unclassifiable, start, after);
                    i = after;
                    mode = LexMode::LongBracket { level };
                } else if byte == b'"' || byte == b'\'' {
                    match scan_short_string(bytes, i + 1, byte) {
                        ShortString::Closed(end) => {
                            push(&mut tokens, TokenKind::String { quote: byte }, start, end);
                            i = end;
                        }
                        ShortString::Continued => {
                            push(&mut tokens, TokenKind::Unclassifiable, start, bytes.len());
                            return Ok((tokens, LexMode::ShortString { quote: byte }));
                        }
                        ShortString::Unterminated => {
                            return Err(StructureErrorKind::UnterminatedString);
                        }
                    }
                } else if byte.is_ascii_digit()
                    || (byte == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
                {
                    i = scan_number(bytes, i);
                    push(&mut tokens, TokenKind::Number, start, i);
                } else if byte.is_ascii_alphabetic() || byte == b'_' {
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    push(&mut tokens, TokenKind::Name, start, i);
                } else {
                    i += 1;
                    push(&mut tokens, TokenKind::Symbol(byte), start, i);
                }
            }
        }
    }
}

/// If a long-bracket opener (`[`, any number of `=`, `[`) starts at `at`, returns
/// its level and the offset just past it.
fn long_open(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    if bytes.get(at) != Some(&b'[') {
        return None;
    }
    let mut i = at + 1;
    while bytes.get(i) == Some(&b'=') {
        i += 1;
    }
    (bytes.get(i) == Some(&b'[')).then_some((i - at - 1, i + 1))
}

/// Finds the closer of a long bracket of `level` (`]`, `level` × `=`, `]`) at or
/// after `from`, returning the offset just past it.
fn find_long_close(bytes: &[u8], from: usize, level: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b']' {
            let mut j = i + 1;
            while bytes.get(j) == Some(&b'=') {
                j += 1;
            }
            if j - i - 1 == level && bytes.get(j) == Some(&b']') {
                return Some(j + 1);
            }
        }
        i += 1;
    }
    None
}

/// How a short string scanned to the end of its line ended.
enum ShortString {
    /// Closed by its quote; the offset is just past the quote.
    Closed(usize),
    /// Continued onto the next line by a trailing `\` or `\z`.
    Continued,
    /// The line ended inside the string with no continuation — a Lua syntax error.
    Unterminated,
}

/// Scans a short string body from `from` for the closing `quote`, honouring
/// escapes (a `\` always consumes the next byte, so `\"` never closes).
fn scan_short_string(bytes: &[u8], from: usize, quote: u8) -> ShortString {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return ShortString::Continued;
                }
                if bytes[i + 1] == b'z' {
                    i += 2;
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return ShortString::Continued;
                    }
                } else {
                    i += 2;
                }
            }
            byte if byte == quote => return ShortString::Closed(i + 1),
            _ => i += 1,
        }
    }
    ShortString::Unterminated
}

/// Scans a numeral starting at `from` and returns the offset just past it. This
/// is a tokenizer, not a validator: it takes the maximal run of numeral
/// characters, and [`parse_number`] decides later whether the text is a value
/// the editor recognises.
fn scan_number(bytes: &[u8], from: usize) -> usize {
    let hex = bytes.get(from) == Some(&b'0') && matches!(bytes.get(from + 1), Some(b'x' | b'X'));
    let exponent: &[u8] = if hex { b"pP" } else { b"eE" };
    let mut i = from;
    while i < bytes.len() {
        let byte = bytes[i];
        let numeral_char = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.';
        // A sign belongs to the numeral only right after an exponent marker
        // (`1e-7`); anywhere else it is an operator.
        let exponent_sign =
            matches!(byte, b'+' | b'-') && i > from && exponent.contains(&bytes[i - 1]);
        if !(numeral_char || exponent_sign) {
            break;
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_render_double_quoted_and_verbatim() {
        let layouts = LuaLiteral::string("us,se").expect("a plain layout list is allowed");
        assert_eq!(layouts.to_string(), "\"us,se\"");
        // Single quotes, `#`, `--` and non-ASCII text need no escaping inside a
        // double-quoted Lua string, so they are allowed and kept verbatim.
        let odd = LuaLiteral::string("it's #1 -- ok ö").expect("allowed characters");
        assert_eq!(odd.to_string(), "\"it's #1 -- ok ö\"");
        assert_eq!(LuaLiteral::string("").unwrap().to_string(), "\"\"");
    }

    #[test]
    fn numbers_and_booleans_render_bare() {
        assert_eq!(LuaLiteral::integer(10).to_string(), "10");
        assert_eq!(LuaLiteral::integer(-3).to_string(), "-3");
        assert_eq!(LuaLiteral::float(0.3).unwrap().to_string(), "0.3");
        assert_eq!(
            LuaLiteral::float(1.0).unwrap().to_string(),
            "1.0",
            "an integral float keeps its fractional part so Lua reads a float"
        );
        assert_eq!(LuaLiteral::float(-0.5).unwrap().to_string(), "-0.5");
        assert_eq!(LuaLiteral::boolean(true).to_string(), "true");
        assert_eq!(LuaLiteral::boolean(false).to_string(), "false");
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(
                    LuaLiteral::float(value),
                    Err(LuaLiteralError::NonFiniteNumber(_))
                ),
                "{value} has no Lua literal form"
            );
        }
    }

    #[test]
    fn the_forbidden_character_rule_refuses_quotes_backslashes_and_control_characters() {
        for (value, character) in [
            ("a\"b", '"'),
            ("C:\\path", '\\'),
            ("two\nlines", '\n'),
            ("carriage\rreturn", '\r'),
            ("tab\there", '\t'),
            ("nul\0byte", '\0'),
        ] {
            assert_eq!(
                check_string_value(value),
                Err(LuaLiteralError::ForbiddenCharacter {
                    value: value.to_string(),
                    character,
                }),
                "{value:?} must be refused"
            );
            assert!(
                LuaLiteral::string(value).is_err(),
                "the string constructor applies the same rule to {value:?}"
            );
        }
        assert_eq!(
            check_string_value("grp:win_space_toggle,caps:escape"),
            Ok(())
        );
    }

    #[test]
    fn identifiers_follow_the_lua_name_rule_and_exclude_reserved_words() {
        for name in ["input", "natural_scroll", "_private", "kb_layout2"] {
            assert!(is_identifier(name), "{name} is a valid Lua key");
        }
        for name in [
            "",
            "2fast",
            "tap-to-click",
            "a.b",
            "a b",
            "end",
            "true",
            "ö",
        ] {
            assert!(!is_identifier(name), "{name:?} is not a valid bare Lua key");
        }
    }

    #[test]
    fn errors_describe_the_problem_for_a_reader() {
        let error = check_string_value("a\nb").unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("a\\nb"),
            "the value is shown escaped: {message}"
        );
        assert!(message.contains("newlines"), "{message}");
        assert!(
            LuaLiteralError::InvalidKey("tap-to-click".to_string())
                .to_string()
                .contains("tap-to-click")
        );
    }

    // -----------------------------------------------------------------------
    // The surgical editor (task 11.3)
    // -----------------------------------------------------------------------

    /// The app-owned `input.lua`, exactly as the dotfiles ship it (analysis §7.1).
    const INPUT_LUA: &str = include_str!("../../tests/fixtures/dotfiles/config/hypr/input.lua");
    /// The real 485-line `hyprland.lua`, of which only the two cursor `hl.env`
    /// lines are addressable.
    const HYPRLAND_LUA: &str =
        include_str!("../../tests/fixtures/dotfiles/config/hypr/hyprland.lua");

    /// Joins hand-written fixture lines with `\n`, ending with a final newline,
    /// so a test's Lua source reads as the file would.
    fn lua(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// Parses `text`, asserting it is structurally sound.
    fn parse_ok(text: &str) -> LuaConfigFile {
        let file = LuaConfigFile::parse(text);
        assert_eq!(file.structure_error(), None, "unexpected structure error");
        file
    }

    /// The 0-based indices at which two texts' lines differ, for texts with the
    /// same number of lines — the byte-diff check behind "an edit changes exactly
    /// one line".
    fn differing_lines(before: &str, after: &str) -> Vec<usize> {
        let before: Vec<&str> = before.split_inclusive('\n').collect();
        let after: Vec<&str> = after.split_inclusive('\n').collect();
        assert_eq!(
            before.len(),
            after.len(),
            "an in-place edit keeps the line count"
        );
        (0..before.len())
            .filter(|&index| before[index] != after[index])
            .collect()
    }

    /// The 0-based index of the first line equal to `line` (terminator excluded).
    fn line_index(text: &str, line: &str) -> usize {
        text.lines()
            .position(|candidate| candidate == line)
            .unwrap_or_else(|| panic!("no line {line:?}"))
    }

    /// Asserts `after` is `before` with exactly one line inserted at `index`, and
    /// returns that line.
    fn inserted_line(before: &str, after: &str, index: usize) -> String {
        let mut before: Vec<&str> = before.split_inclusive('\n').collect();
        let after: Vec<&str> = after.split_inclusive('\n').collect();
        assert_eq!(after.len(), before.len() + 1, "exactly one line is added");
        let inserted = after[index].to_string();
        before.insert(index, &inserted);
        assert_eq!(before, after, "every other line is byte-identical");
        inserted
    }

    #[test]
    fn literal_kind_and_value_accessors() {
        let layout = LuaLiteral::string("us").unwrap();
        assert_eq!(layout.kind(), LuaLiteralKind::String);
        assert_eq!(layout.value().as_str(), Some("us"));
        assert_eq!(layout.value().as_bool(), None);
        assert_eq!(LuaLiteral::boolean(true).kind(), LuaLiteralKind::Boolean);
        assert_eq!(LuaLiteral::boolean(true).value().as_bool(), Some(true));
        let integer = LuaLiteral::integer(2);
        assert_eq!(integer.kind(), LuaLiteralKind::Number);
        assert_eq!(integer.value().as_integer(), Some(2));
        assert_eq!(integer.value().as_number(), Some(2.0));
        let float = LuaLiteral::float(1.0).unwrap();
        assert_eq!(
            float.kind(),
            LuaLiteralKind::Number,
            "both subtypes share a kind"
        );
        assert_eq!(float.value().as_integer(), None, "1.0 is not reported as 1");
        assert_eq!(float.value().as_number(), Some(1.0));
    }

    #[test]
    fn the_fixture_input_lua_round_trips_and_is_fully_addressable() {
        let file = parse_ok(INPUT_LUA);
        assert_eq!(file.emit(), INPUT_LUA);
        assert_eq!(file.opaque_lines(), Vec::<usize>::new());
        assert_eq!(file.table_status("input"), TableStatus::Clean);
        assert_eq!(file.table_status("input.touchpad"), TableStatus::Clean);
        assert_eq!(file.table_status(""), TableStatus::Clean);
        assert_eq!(file.table_status("input.mouse"), TableStatus::Missing);

        let value = |path| file.value(path).cloned();
        assert_eq!(
            value("input.kb_layout"),
            Some(LuaValue::String("us,se".into()))
        );
        assert_eq!(
            value("input.kb_options"),
            Some(LuaValue::String("grp:win_space_toggle,caps:escape".into()))
        );
        assert_eq!(value("input.sensitivity"), Some(LuaValue::Float(0.3)));
        assert_eq!(value("input.follow_mouse"), Some(LuaValue::Integer(1)));
        assert_eq!(
            value("input.touchpad.natural_scroll"),
            Some(LuaValue::Boolean(true))
        );
        assert_eq!(
            value("input.touchpad.tap_to_click"),
            Some(LuaValue::Boolean(true))
        );
        assert_eq!(
            value("input.touchpad.scroll_factor"),
            Some(LuaValue::Float(1.0))
        );
        // Absent keys and wrong nesting are not found.
        assert_eq!(value("input.kb_variant"), None);
        assert_eq!(value("input.natural_scroll"), None);
        assert_eq!(value("touchpad.natural_scroll"), None);
        assert_eq!(value("input.touchpad.tap-to-click"), None);
    }

    #[test]
    fn the_fixture_hyprland_lua_round_trips_and_exposes_the_cursor_env_lines() {
        let file = parse_ok(HYPRLAND_LUA);
        assert_eq!(file.emit(), HYPRLAND_LUA);
        assert_eq!(file.env_value("XCURSOR_THEME"), Some("breeze_cursors"));
        assert_eq!(file.env_value("XCURSOR_SIZE"), Some("24"));
        assert_eq!(file.env_value("GTK_THEME"), None);
    }

    #[test]
    fn hyprland_lua_is_opaque_everywhere_but_the_env_lines_and_its_option_table() {
        let file = parse_ok(HYPRLAND_LUA);
        let opaque = file.opaque_lines();
        let env_lines = [
            line_index(HYPRLAND_LUA, r#"hl.env("XCURSOR_THEME", "breeze_cursors")"#) + 1,
            line_index(HYPRLAND_LUA, r#"hl.env("XCURSOR_SIZE", "24")"#) + 1,
        ];
        // The one top-level `hl.config({ … })` call, whose root table is the only
        // place a table path can start (1-based, inclusive).
        let config_open = line_index(HYPRLAND_LUA, "hl.config({") + 1;
        let config_close = config_open
            + HYPRLAND_LUA
                .lines()
                .skip(config_open)
                .position(|line| line == "})")
                .expect("the hl.config call closes")
            + 1;

        for (index, line) in HYPRLAND_LUA.lines().enumerate() {
            let number = index + 1;
            let harmless = line.trim().is_empty() || line.trim_start().starts_with("--");
            if harmless || env_lines.contains(&number) {
                assert!(
                    !opaque.contains(&number),
                    "line {number} is not opaque: {line}"
                );
            } else if !(config_open..=config_close).contains(&number) {
                assert!(opaque.contains(&number), "line {number} is opaque: {line}");
            }
        }

        // Inside the option table, plain literals are addressable by their option
        // path, while computed values and one-line tables are opaque.
        assert_eq!(
            file.value("decoration.rounding"),
            Some(&LuaValue::Integer(13))
        );
        assert_eq!(
            file.value("decoration.shadow.render_power"),
            Some(&LuaValue::Integer(2))
        );
        assert_eq!(file.value("general.col.active_border"), None);
        assert_eq!(file.value("misc.vrr"), None);
        assert_eq!(file.table_status("cursor"), TableStatus::Clean);
        for table in ["", "general", "general.col", "decoration", "misc"] {
            assert!(
                matches!(file.table_status(table), TableStatus::ContainsOpaque(_)),
                "{table:?} holds computed values"
            );
        }
        // A table passed to a call other than hl.config has no option path.
        assert_eq!(file.value("name"), None);
        assert_eq!(file.value("no_blur"), None);
        assert_eq!(file.table_status("match"), TableStatus::Missing);
    }

    #[test]
    fn hl_env_edits_leave_every_other_hyprland_lua_line_byte_identical() {
        let theme_line = line_index(HYPRLAND_LUA, r#"hl.env("XCURSOR_THEME", "breeze_cursors")"#);
        let size_line = line_index(HYPRLAND_LUA, r#"hl.env("XCURSOR_SIZE", "24")"#);

        let mut file = parse_ok(HYPRLAND_LUA);
        file.set_env("XCURSOR_THEME", "Adwaita").unwrap();
        let edited = file.emit();
        assert_eq!(differing_lines(HYPRLAND_LUA, &edited), vec![theme_line]);
        assert_eq!(
            edited.lines().nth(theme_line),
            Some(r#"hl.env("XCURSOR_THEME", "Adwaita")"#)
        );
        assert_eq!(file.env_value("XCURSOR_THEME"), Some("Adwaita"));

        file.set_env("XCURSOR_SIZE", "32").unwrap();
        let edited = file.emit();
        assert_eq!(
            differing_lines(HYPRLAND_LUA, &edited),
            vec![theme_line, size_line]
        );
        assert_eq!(
            edited.lines().nth(size_line),
            Some(r#"hl.env("XCURSOR_SIZE", "32")"#)
        );
        assert!(edited.lines().count() > 400);

        assert_eq!(
            file.set_env("XCURSOR_PATH", "x"),
            Err(LuaEditError::EnvNotFound("XCURSOR_PATH".into()))
        );
    }

    #[test]
    fn a_quote_bearing_value_is_refused_and_the_file_is_unchanged() {
        // No `#` anywhere — the hyprlang rule would let this through; the Lua rule
        // must not, because the `"` would end the string literal early.
        let value = r#"breeze" .. os.getenv("HOME") .. ""#;
        assert!(!value.contains('#'));
        let mut file = parse_ok(HYPRLAND_LUA);
        assert!(matches!(
            file.set_env("XCURSOR_THEME", value),
            Err(LuaEditError::Literal(LuaLiteralError::ForbiddenCharacter {
                character: '"',
                ..
            }))
        ));
        assert_eq!(file.emit(), HYPRLAND_LUA);
        assert!(
            LuaLiteral::string(value).is_err(),
            "no literal can carry it either"
        );
    }

    #[test]
    fn targeted_edits_change_exactly_one_literal_span() {
        let cases = [
            (
                "input.kb_layout",
                LuaLiteral::string("us,se,de").unwrap(),
                r#"        kb_layout = "us,se,de","#,
            ),
            (
                "input.sensitivity",
                LuaLiteral::float(-0.5).unwrap(),
                "        sensitivity = -0.5,",
            ),
            (
                "input.follow_mouse",
                LuaLiteral::integer(2),
                "        follow_mouse = 2,",
            ),
            (
                "input.touchpad.natural_scroll",
                LuaLiteral::boolean(false),
                "            natural_scroll = false,",
            ),
            (
                "input.touchpad.tap_to_click",
                LuaLiteral::boolean(false),
                "            tap_to_click = false,",
            ),
        ];
        for (path, value, expected) in cases {
            let mut file = parse_ok(INPUT_LUA);
            file.set_value(path, &value).unwrap();
            let edited = file.emit();
            let changed = differing_lines(INPUT_LUA, &edited);
            assert_eq!(changed.len(), 1, "{path}: exactly one line changes");
            assert_eq!(edited.lines().nth(changed[0]), Some(expected), "{path}");
            assert_eq!(file.value(path), Some(value.value()), "{path} re-reads");
            assert_eq!(file.table_status("input"), TableStatus::Clean);
        }
    }

    #[test]
    fn a_number_may_change_subtype_but_no_value_changes_kind() {
        let mut file = parse_ok(INPUT_LUA);
        // A float option may be written as an integer (and vice versa): one kind.
        file.set_value("input.sensitivity", &LuaLiteral::integer(0))
            .unwrap();
        assert_eq!(file.value("input.sensitivity"), Some(&LuaValue::Integer(0)));

        let before = file.emit();
        for (path, value, existing) in [
            (
                "input.touchpad.natural_scroll",
                LuaLiteral::string("true").unwrap(),
                LuaLiteralKind::Boolean,
            ),
            (
                "input.kb_layout",
                LuaLiteral::boolean(true),
                LuaLiteralKind::String,
            ),
            (
                "input.follow_mouse",
                LuaLiteral::string("1").unwrap(),
                LuaLiteralKind::Number,
            ),
            (
                "input.follow_mouse",
                LuaLiteral::boolean(true),
                LuaLiteralKind::Number,
            ),
        ] {
            assert_eq!(
                file.set_value(path, &value),
                Err(LuaEditError::KindMismatch {
                    path: path.into(),
                    existing,
                    requested: value.kind(),
                }),
                "{path}"
            );
        }
        assert_eq!(file.emit(), before, "a refused edit changes nothing");
        assert_eq!(
            file.set_value("input.kb_variant", &LuaLiteral::string("x").unwrap()),
            Err(LuaEditError::EntryNotFound("input.kb_variant".into()))
        );
        assert_eq!(
            file.set_value("input.tap-to-click", &LuaLiteral::boolean(true)),
            Err(LuaEditError::Literal(LuaLiteralError::InvalidKey(
                "tap-to-click".into()
            )))
        );
    }

    #[test]
    fn insertion_matches_the_sibling_indentation_before_the_closing_brace() {
        let mut file = parse_ok(INPUT_LUA);
        file.insert(
            "input",
            "kb_variant",
            &LuaLiteral::string(",nodeadkeys").unwrap(),
        )
        .unwrap();
        let edited = file.emit();
        // The `input` table closes on the second-to-last `},` line (indent 4).
        let close = line_index(INPUT_LUA, "    },");
        assert_eq!(
            inserted_line(INPUT_LUA, &edited, close),
            "        kb_variant = \",nodeadkeys\",\n"
        );
        assert_eq!(
            file.value("input.kb_variant").and_then(LuaValue::as_str),
            Some(",nodeadkeys")
        );
        assert_eq!(file.table_status("input"), TableStatus::Clean);

        // A nested table copies its own, deeper indentation.
        let mut file = parse_ok(INPUT_LUA);
        file.insert("input.touchpad", "drag_lock", &LuaLiteral::boolean(true))
            .unwrap();
        let close = line_index(INPUT_LUA, "        },");
        assert_eq!(
            inserted_line(INPUT_LUA, &file.emit(), close),
            "            drag_lock = true,\n"
        );

        // The inserted key is now an entry: a second insert is refused, an edit works.
        assert_eq!(
            file.insert("input.touchpad", "drag_lock", &LuaLiteral::boolean(false)),
            Err(LuaEditError::KeyExists("input.touchpad.drag_lock".into()))
        );
        file.set_value("input.touchpad.drag_lock", &LuaLiteral::boolean(false))
            .unwrap();
        assert_eq!(
            file.insert("input", "touchpad", &LuaLiteral::boolean(false)),
            Err(LuaEditError::KeyExists("input.touchpad".into())),
            "a sub-table name is taken too"
        );
        assert_eq!(
            file.insert("input.mouse", "x", &LuaLiteral::integer(1)),
            Err(LuaEditError::TableNotFound("input.mouse".into()))
        );
        assert_eq!(
            file.insert("input", "end", &LuaLiteral::integer(1)),
            Err(LuaEditError::Literal(LuaLiteralError::InvalidKey(
                "end".into()
            )))
        );
    }

    #[test]
    fn insertion_into_an_empty_table_indents_one_unit_past_the_opener() {
        let spaces = "hl.config({\n    input = {\n    },\n})\n";
        let mut file = parse_ok(spaces);
        file.insert("input", "kb_model", &LuaLiteral::string("pc105").unwrap())
            .unwrap();
        assert_eq!(
            file.emit(),
            "hl.config({\n    input = {\n        kb_model = \"pc105\",\n    },\n})\n"
        );

        let tabs = "hl.config({\n\tinput = {\n\t},\n})\n";
        let mut file = parse_ok(tabs);
        file.insert("input", "kb_rules", &LuaLiteral::string("evdev").unwrap())
            .unwrap();
        assert_eq!(
            file.emit(),
            "hl.config({\n\tinput = {\n\t\tkb_rules = \"evdev\",\n\t},\n})\n"
        );

        // The root table of the call (empty path) is a table too.
        let mut file = parse_ok("hl.config({\n})\n");
        file.insert("", "debug", &LuaLiteral::boolean(true))
            .unwrap();
        assert_eq!(file.emit(), "hl.config({\n    debug = true,\n})\n");
    }

    #[test]
    fn insertion_after_a_field_without_a_separator_adds_the_missing_comma() {
        // Lua allows the last field without a trailing comma; a field appended
        // after it would be a syntax error without one.
        let mut file = parse_ok(&lua(&[
            "hl.config({",
            "    input = {",
            r#"        kb_layout = "us" -- main"#,
            "    },",
            "})",
        ]));
        file.insert("input", "kb_variant", &LuaLiteral::string("").unwrap())
            .unwrap();
        assert_eq!(
            file.emit(),
            lua(&[
                "hl.config({",
                "    input = {",
                r#"        kb_layout = "us", -- main"#,
                r#"        kb_variant = "","#,
                "    },",
                "})",
            ])
        );

        // The same after a nested table's closing brace.
        let mut file = parse_ok(&lua(&[
            "hl.config({",
            "    input = {",
            "        touchpad = {",
            "        }",
            "    },",
            "})",
        ]));
        file.insert("input", "kb_model", &LuaLiteral::string("x").unwrap())
            .unwrap();
        assert_eq!(
            file.emit(),
            lua(&[
                "hl.config({",
                "    input = {",
                "        touchpad = {",
                "        },",
                r#"        kb_model = "x","#,
                "    },",
                "})",
            ])
        );
    }

    #[test]
    fn the_last_occurrence_of_a_duplicated_key_is_read_and_edited() {
        let text = lua(&[
            "hl.config({",
            "    input = {",
            r#"        kb_layout = "us","#,
            r#"        kb_layout = "se","#,
            "    },",
            "})",
            "hl.config({",
            "    input = {",
            "        follow_mouse = 1,",
            "    },",
            "})",
            "hl.config({",
            "    input = {",
            "        follow_mouse = 2,",
            "    },",
            "})",
        ]);
        let mut file = parse_ok(&text);
        assert_eq!(
            file.value("input.kb_layout"),
            Some(&LuaValue::String("se".into()))
        );
        assert_eq!(
            file.value("input.follow_mouse"),
            Some(&LuaValue::Integer(2))
        );

        file.set_value("input.kb_layout", &LuaLiteral::string("de").unwrap())
            .unwrap();
        file.set_value("input.follow_mouse", &LuaLiteral::integer(0))
            .unwrap();
        assert_eq!(
            differing_lines(&text, &file.emit()),
            vec![3, 13],
            "only the last copies change; the shadowed ones stay byte-identical"
        );

        // An insertion goes into the last occurrence of the table, the one
        // Hyprland applies last: just before its closing `},` (line index 14).
        file.insert("input", "kb_model", &LuaLiteral::string("pc105").unwrap())
            .unwrap();
        let inserted = file.emit();
        assert_eq!(
            inserted.lines().nth(14),
            Some(r#"        kb_model = "pc105","#),
            "{inserted}"
        );
        assert_eq!(inserted.matches("kb_model").count(), 1);

        // hl.env follows the same rule: a later call overwrites the variable.
        let mut file = parse_ok(&lua(&[r#"hl.env("A", "1")"#, r#"hl.env("A", "2")"#]));
        assert_eq!(file.env_value("A"), Some("2"));
        file.set_env("A", "3").unwrap();
        assert_eq!(
            file.emit(),
            lua(&[r#"hl.env("A", "1")"#, r#"hl.env("A", "3")"#])
        );
    }

    #[test]
    fn a_repeated_sub_table_in_one_call_is_opaque_but_separate_calls_merge() {
        // In one constructor the second `input = {` replaces the first table, so
        // `input.a` is not in effect even though only the first copy declares it.
        let one_call = lua(&[
            "hl.config({",
            "    input = {",
            "        a = 1,",
            "    },",
            "    input = {",
            "        b = 2,",
            "    },",
            "})",
        ]);
        let file = parse_ok(&one_call);
        assert_eq!(file.emit(), one_call);
        assert_eq!(file.opaque_lines(), vec![5, 6, 7]);
        assert!(matches!(
            file.table_status(""),
            TableStatus::ContainsOpaque(_)
        ));

        // Two calls are applied one after the other, so both tables count.
        let two_calls = lua(&[
            "hl.config({",
            "    input = {",
            "        a = 1,",
            "    },",
            "})",
            "hl.config({",
            "    input = {",
            "        b = 2,",
            "    },",
            "})",
        ]);
        let file = parse_ok(&two_calls);
        assert_eq!(file.table_status("input"), TableStatus::Clean);
        assert_eq!(file.value("input.a"), Some(&LuaValue::Integer(1)));
        assert_eq!(file.value("input.b"), Some(&LuaValue::Integer(2)));
    }

    #[test]
    fn a_key_used_as_both_scalar_and_sub_table_in_one_call_is_opaque() {
        // Table first, then a scalar: Lua keeps `touchpad = 5`, so every entry of
        // the earlier table is dead and the scalar line is not addressed either.
        let table_then_scalar = lua(&[
            "hl.config({",
            "    input = {",
            "        touchpad = {",
            "            a = 1,",
            "        },",
            "        touchpad = 5,",
            "    },",
            "})",
        ]);
        let file = parse_ok(&table_then_scalar);
        assert_eq!(file.emit(), table_then_scalar);
        assert_eq!(file.opaque_lines(), vec![6]);
        assert_eq!(file.value("input.touchpad"), None);
        assert_eq!(
            file.table_status("input.touchpad"),
            TableStatus::ContainsOpaque(vec![6]),
            "the replaced table itself reports the line that replaced it"
        );
        assert_eq!(
            file.table_status("input"),
            TableStatus::ContainsOpaque(vec![6])
        );

        // Scalar first, then a table: Lua keeps the table, so the later opener and
        // its contents are opaque and the enclosing table says so.
        let scalar_then_table = lua(&[
            "hl.config({",
            "    input = {",
            "        touchpad = 5,",
            "        touchpad = {",
            "            a = 1,",
            "        },",
            "    },",
            "})",
        ]);
        let file = parse_ok(&scalar_then_table);
        assert_eq!(file.emit(), scalar_then_table);
        assert_eq!(file.opaque_lines(), vec![4, 5, 6]);
        assert_eq!(file.value("input.touchpad.a"), None);
        assert_eq!(file.table_status("input.touchpad"), TableStatus::Missing);
        assert_eq!(
            file.table_status("input"),
            TableStatus::ContainsOpaque(vec![4, 5, 6])
        );

        // Across two separate calls the options merge, so nothing is opaque.
        let two_calls = lua(&[
            "hl.config({",
            "    input = {",
            "        touchpad = 5,",
            "    },",
            "})",
            "hl.config({",
            "    input = {",
            "        touchpad = {",
            "            a = 1,",
            "        },",
            "    },",
            "})",
        ]);
        assert_eq!(parse_ok(&two_calls).opaque_lines(), Vec::<usize>::new());
    }

    #[test]
    fn a_config_call_without_parentheses_is_wholly_opaque() {
        // `hl.config { … }` is valid Lua, but not a shape the editor writes, so
        // nothing in it is addressed.
        let text = lua(&[
            "hl.config {",
            "    input = {",
            r#"        kb_layout = "us","#,
            "    },",
            "}",
        ]);
        let file = parse_ok(&text);
        assert_eq!(file.emit(), text);
        assert_eq!(file.opaque_lines(), vec![1, 2, 3, 4, 5]);
        assert_eq!(file.table_status("input"), TableStatus::Missing);
        assert_eq!(file.value("input.kb_layout"), None);
    }

    #[test]
    fn a_leading_byte_order_mark_makes_only_the_first_line_opaque() {
        // Before a comment line (the fixture's header) the mark is harmless.
        let text = format!("\u{feff}{INPUT_LUA}");
        let file = parse_ok(&text);
        assert_eq!(file.emit(), text);
        assert_eq!(file.opaque_lines(), vec![1]);
        assert_eq!(file.table_status("input"), TableStatus::Clean);
        assert_eq!(
            file.value("input.kb_layout"),
            Some(&LuaValue::String("us,se".into()))
        );

        // Directly before `hl.config({` it hides the call: fail safe, not guessed.
        let text = format!(
            "\u{feff}{}",
            lua(&[
                "hl.config({",
                "    input = {",
                "        a = 1,",
                "    },",
                "})"
            ])
        );
        let file = parse_ok(&text);
        assert_eq!(file.emit(), text);
        assert_eq!(file.table_status("input"), TableStatus::Missing);
        assert_eq!(file.value("input.a"), None);
    }

    #[test]
    fn empty_path_segments_have_their_own_error() {
        let mut file = parse_ok(INPUT_LUA);
        for path in ["", "input..kb_layout", "input.", ".input"] {
            let error = file
                .set_value(path, &LuaLiteral::integer(1))
                .expect_err("an empty segment names nothing");
            assert_eq!(error, LuaEditError::EmptyPathSegment(path.into()));
            assert!(
                !error.to_string().contains("valid Lua table key"),
                "{error}"
            );
        }
        assert_eq!(
            LuaEditError::EmptyPathSegment(String::new()).to_string(),
            "an empty path names no entry"
        );
        assert!(
            LuaEditError::EmptyPathSegment("input..kb_layout".into())
                .to_string()
                .contains("empty segment")
        );
        assert_eq!(
            file.insert("input..touchpad", "x", &LuaLiteral::integer(1)),
            Err(LuaEditError::EmptyPathSegment("input..touchpad".into()))
        );
        assert_eq!(file.table_status("input."), TableStatus::Missing);
    }

    #[test]
    fn a_key_in_sibling_tables_is_addressed_by_its_full_path() {
        let text = lua(&[
            "hl.config({",
            "    input = {",
            "        natural_scroll = false,",
            "        touchpad = {",
            "            natural_scroll = true,",
            "        },",
            "        tablet = {",
            "            natural_scroll = false,",
            "        },",
            "    },",
            "})",
        ]);
        let mut file = parse_ok(&text);
        assert_eq!(
            file.value("input.natural_scroll"),
            Some(&LuaValue::Boolean(false))
        );
        assert_eq!(
            file.value("input.touchpad.natural_scroll"),
            Some(&LuaValue::Boolean(true))
        );

        file.set_value("input.touchpad.natural_scroll", &LuaLiteral::boolean(false))
            .unwrap();
        assert_eq!(differing_lines(&text, &file.emit()), vec![4]);
        file.set_value("input.natural_scroll", &LuaLiteral::boolean(true))
            .unwrap();
        assert_eq!(
            differing_lines(&text, &file.emit()),
            vec![2, 4],
            "the outer key edits its own line, not a sibling table's"
        );
        assert_eq!(
            file.value("input.tablet.natural_scroll"),
            Some(&LuaValue::Boolean(false))
        );
    }

    #[test]
    fn an_expression_inside_the_input_table_is_reported_opaque() {
        let text = lua(&[
            "hl.config({",
            "    input = {",
            r#"        kb_layout = "us","#,
            "        sensitivity = 0.1 * 3,",
            "        follow_mouse = get_mode(),",
            "        touchpad = {",
            "            natural_scroll = true,",
            "        },",
            "    },",
            "})",
        ]);
        let mut file = parse_ok(&text);
        assert_eq!(file.emit(), text);
        assert_eq!(file.opaque_lines(), vec![4, 5]);
        assert_eq!(
            file.table_status("input"),
            TableStatus::ContainsOpaque(vec![4, 5])
        );
        assert_eq!(
            file.table_status(""),
            TableStatus::ContainsOpaque(vec![4, 5])
        );
        assert_eq!(
            file.table_status("input.touchpad"),
            TableStatus::Clean,
            "a sibling's opaque line does not taint a nested table"
        );
        assert_eq!(file.value("input.sensitivity"), None);
        assert_eq!(
            file.value("input.kb_layout"),
            Some(&LuaValue::String("us".into())),
            "recognised entries stay readable; the loader decides"
        );
        assert_eq!(
            file.insert("input", "kb_model", &LuaLiteral::string("x").unwrap()),
            Err(LuaEditError::TableNotEditable {
                path: "input".into(),
                opaque_lines: vec![4, 5],
            })
        );
    }

    #[test]
    fn unrecognised_shapes_are_opaque_and_preserved() {
        // Each case is the body of `hl.config({ input = { … } })` (lines 3 on)
        // and the 1-based lines expected to be opaque.
        let cases: [(&[&str], Vec<usize>); 7] = [
            // A block comment, even on one line.
            (&["        --[[ off ]]"], vec![3]),
            // A multi-line block comment: every line it touches.
            (
                &["        --[==[", "        a = 1,", "        ]==]"],
                vec![3, 4, 5],
            ),
            // A long string value spanning lines.
            (&["        kb_layout = [[us,", "se]],"], vec![3, 4]),
            // A single-quoted string, a one-line table, a function value.
            (
                &[
                    "        kb_layout = 'us',",
                    "        offset = { 2, 6 },",
                    "        cb = function() return 1 end,",
                ],
                vec![3, 4, 5],
            ),
            // A multi-line function body inside the table: the statement inside it
            // looks like an entry but is code.
            (
                &[
                    "        cb = function()",
                    "            x = 1",
                    "        end,",
                ],
                vec![3, 4, 5],
            ),
            // Two closers on one line are not a recognised closer. (The bare `{`
            // keeps the file balanced: the wrapper's `},` then closes that plain
            // bracket rather than an option table, so it is opaque too.)
            (
                &["        touchpad = {", "        }},", "    {"],
                vec![4, 5, 6],
            ),
            // A string continued onto the next line by a trailing backslash.
            (&[r#"        kb_layout = "us\"#, r#"se","#], vec![3, 4]),
        ];
        for (body, opaque) in cases {
            let mut lines = vec!["hl.config({", "    input = {"];
            lines.extend_from_slice(body);
            lines.extend_from_slice(&["    },", "})"]);
            let text = lua(&lines);
            let file = parse_ok(&text);
            assert_eq!(file.emit(), text);
            assert_eq!(file.opaque_lines(), opaque, "{text}");
            assert!(matches!(
                file.table_status("input"),
                TableStatus::ContainsOpaque(_)
            ));
        }
    }

    #[test]
    fn hl_config_only_starts_a_path_at_top_level() {
        // Inside a block or another call the table is code, not an option table.
        let text = lua(&[
            "if docked then",
            "    hl.config({",
            "        input = {",
            r#"            kb_layout = "us","#,
            "        },",
            "    })",
            "end",
            r#"hl.on("x", function()"#,
            "    hl.config({",
            "        misc = {",
            "            vrr = 1,",
            "        },",
            "    })",
            "end)",
        ]);
        let file = parse_ok(&text);
        assert_eq!(file.value("input.kb_layout"), None);
        assert_eq!(file.value("misc.vrr"), None);
        assert_eq!(file.table_status("input"), TableStatus::Missing);
        assert_eq!(file.opaque_lines(), (1..=14).collect::<Vec<_>>());

        // The same for hl.env: only a top-level call is the cursor declaration,
        // never a commented-out one or one inside a block.
        let env = |size: &str| {
            lua(&[
                r#"-- hl.env("XCURSOR_SIZE", "48")"#,
                r#"if x then hl.env("XCURSOR_SIZE", "32") end"#,
                &format!(r#"hl.env("XCURSOR_SIZE", "{size}") -- canonical"#),
            ])
        };
        let mut file = parse_ok(&env("24"));
        assert_eq!(file.env_value("XCURSOR_SIZE"), Some("24"));
        file.set_env("XCURSOR_SIZE", "30").unwrap();
        assert_eq!(file.emit(), env("30"));
    }

    #[test]
    fn trailing_comments_spacing_and_escapes_survive_an_edit() {
        let text = lua(&[
            "hl.config({",
            "    input = {",
            "        natural_scroll   =   true ,  -- note }",
            r#"        kb_options = "a\"b\\c\x41\66\u{e9}\z   d","#,
            "    }, -- end of input",
            "});",
        ]);
        let mut file = parse_ok(&text);
        assert_eq!(file.emit(), text);
        assert_eq!(
            file.value("input.kb_options"),
            Some(&LuaValue::String("a\"b\\cABéd".into())),
            "escapes are decoded for reading"
        );
        file.set_value("input.natural_scroll", &LuaLiteral::boolean(false))
            .unwrap();
        file.set_value(
            "input.kb_options",
            &LuaLiteral::string("caps:escape").unwrap(),
        )
        .unwrap();
        assert_eq!(
            file.emit(),
            lua(&[
                "hl.config({",
                "    input = {",
                "        natural_scroll   =   false ,  -- note }",
                r#"        kb_options = "caps:escape","#,
                "    }, -- end of input",
                "});",
            ])
        );
    }

    #[test]
    fn an_invalid_escape_makes_the_line_opaque() {
        let file = parse_ok(&lua(&[
            "hl.config({",
            "    input = {",
            r#"        kb_layout = "\q","#,
            "    },",
            "})",
        ]));
        assert_eq!(file.opaque_lines(), vec![3]);
        assert_eq!(file.value("input.kb_layout"), None);
    }

    #[test]
    fn numerals_are_read_with_their_subtype() {
        let file = parse_ok(&lua(&[
            "hl.config({",
            "    a = {",
            "        i = 10,",
            "        h = 0x1F,",
            "        f = .5,",
            "        e = 1e3,",
            "        n = - 2,",
            "        big = 99999999999999999999,",
            "        hexfloat = 0x1p4,",
            "    },",
            "})",
        ]));
        assert_eq!(file.value("a.i"), Some(&LuaValue::Integer(10)));
        assert_eq!(file.value("a.h"), Some(&LuaValue::Integer(31)));
        assert_eq!(file.value("a.f"), Some(&LuaValue::Float(0.5)));
        assert_eq!(file.value("a.e"), Some(&LuaValue::Float(1000.0)));
        assert_eq!(file.value("a.n"), Some(&LuaValue::Integer(-2)));
        // Lua would silently read an overflowing integer as a float; the editor
        // does not guess.
        assert_eq!(file.value("a.big"), None);
        // Hex floats are valid Lua but not a spelling the editor reads.
        assert_eq!(file.value("a.hexfloat"), None);
        assert_eq!(file.opaque_lines(), vec![8, 9]);
    }

    #[test]
    fn line_endings_and_the_final_newline_state_round_trip() {
        let crlf = "hl.config({\r\n    input = {\r\n        a = 1,\r\n    },\r\n})";
        for text in [
            "",
            "\n",
            "hl.config({\n})",
            "-- only a comment",
            crlf,
            &format!("{crlf}\r\n"),
        ] {
            assert_eq!(parse_ok(text).emit(), text, "{text:?}");
        }
        let mut file = parse_ok(crlf);
        file.insert("input", "b", &LuaLiteral::integer(2)).unwrap();
        assert_eq!(
            file.emit(),
            "hl.config({\r\n    input = {\r\n        a = 1,\r\n        b = 2,\r\n    },\r\n})",
            "a CRLF file gets a CRLF line; the missing final newline stays missing"
        );
        file.set_value("input.a", &LuaLiteral::integer(3)).unwrap();
        let edited = file.emit();
        assert!(
            edited.contains("        a = 3,\r\n"),
            "the edited line keeps its CRLF terminator: {edited:?}"
        );
        assert!(edited.ends_with("})"));
    }

    #[test]
    fn unbalanced_structure_makes_the_whole_file_unsafe() {
        for (text, line, kind) in [
            (
                "hl.config({\n    input = {\n        a = 1,\n})\n",
                4,
                StructureErrorKind::MismatchedClose {
                    opened: '{',
                    found: ')',
                },
            ),
            (
                "hl.config({\n    input = {\n        a = 1,\n    },\n",
                1,
                StructureErrorKind::Unclosed('('),
            ),
            ("a = 1\n}\n", 2, StructureErrorKind::UnmatchedClose('}')),
            ("x = \"open\n", 1, StructureErrorKind::UnterminatedString),
            (
                "x = [[open\nstill\n",
                2,
                StructureErrorKind::UnterminatedLongBracket,
            ),
            ("end\n", 1, StructureErrorKind::UnmatchedEnd),
            ("if x then\n  y()\n", 1, StructureErrorKind::UnclosedBlock),
        ] {
            let mut file = LuaConfigFile::parse(text);
            let error = file
                .structure_error()
                .cloned()
                .unwrap_or_else(|| panic!("{text:?} is unbalanced"));
            assert_eq!((error.line(), error.kind()), (line, &kind), "{text:?}");
            assert_eq!(file.emit(), text, "an unsafe file still round-trips");
            assert_eq!(file.table_status("input"), TableStatus::UnsafeFile);
            assert_eq!(file.value("input.a"), None);
            assert_eq!(
                file.set_value("input.a", &LuaLiteral::integer(2)),
                Err(LuaEditError::UnsafeFile(error.clone()))
            );
            assert_eq!(
                file.insert("input", "b", &LuaLiteral::integer(2)),
                Err(LuaEditError::UnsafeFile(error.clone()))
            );
            assert_eq!(file.set_env("A", "b"), Err(LuaEditError::UnsafeFile(error)));
            let non_blank = text.lines().filter(|line| !line.trim().is_empty()).count();
            assert_eq!(file.opaque_lines().len(), non_blank, "every line is opaque");
        }
    }

    #[test]
    fn braces_inside_strings_and_comments_do_not_count_as_nesting() {
        // Modelled on hyprland.lua's lock-screen Escape bind, whose `[[…]]`
        // command string contains a Lua table constructor.
        let file = parse_ok(&lua(&[
            r#"hl.bind("Escape", dsp.exec_cmd([[x 'hl.dsp.dpms({ action = "off" })']]),"#,
            "        { locked = true })",
            "-- }",
            r#"local s = "{" .. '}' -- {"#,
            "hl.config({",
            "    input = {",
            r#"        kb_layout = "{us}", -- }}"#,
            "    },",
            "})",
        ]));
        assert_eq!(
            file.value("input.kb_layout"),
            Some(&LuaValue::String("{us}".into()))
        );
        assert_eq!(file.table_status("input"), TableStatus::Clean);
    }

    #[test]
    fn edit_errors_describe_the_problem_for_a_reader() {
        let mismatch = LuaEditError::KindMismatch {
            path: "input.touchpad.natural_scroll".into(),
            existing: LuaLiteralKind::Boolean,
            requested: LuaLiteralKind::String,
        };
        let message = mismatch.to_string();
        assert!(
            message.contains("natural_scroll") && message.contains("boolean"),
            "{message}"
        );
        let unsafe_file = LuaConfigFile::parse("end\n");
        let message =
            LuaEditError::UnsafeFile(unsafe_file.structure_error().unwrap().clone()).to_string();
        assert!(message.contains("line 1"), "{message}");
        assert!(
            LuaEditError::TableNotFound(String::new())
                .to_string()
                .contains("hl.config"),
            "the root table is named readably"
        );
    }
}
