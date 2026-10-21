//! Surgical parser and writer for a dotfiles palette source file
//! (`colors/<scheme>`, task 3.1; architecture §3; R5.3 item 1, R6.1).
//!
//! # What this file is
//!
//! In the dotfiles this app manages, a palette scheme is a plain
//! `key=value` file (one entry per line) under the repo's `colors/` directory,
//! e.g. `colors/everforest`. Values are **bare hexadecimal** colors — six hex
//! digits with **no leading `#`** — for example:
//!
//! ```text
//! # Everforest Dark palette
//! bg0=272e33
//! accent0=83c092
//! ```
//!
//! It is the *single source of truth* for the whole color pipeline: running
//! `scripts/generate-colors <scheme>` reads it and regenerates every per-app
//! color file (hyprland vars, SCSS, GTK CSS, …). Those generated files are
//! read-only outputs and are never edited by hand or by this app — only the
//! `colors/<scheme>` source is (analysis §2, §6.4). This module is the parser
//! for that source.
//!
//! # Why a surgical, lossless parser (not a serializer)
//!
//! The palette source is hand-maintained: it carries comments, blank-line
//! grouping, and a deliberate ordering that a maintainer relies on. The
//! architecture's hard rule for every config parser (architecture §3) is to
//! **never regenerate a file from a model**; instead we keep a lossless
//! line/token representation and, when asked to change a value, rewrite *only*
//! that value's byte span and re-emit every other byte identically. Two
//! guarantees follow, both covered by tests:
//!
//! - **Round-trip identity**: [`PaletteFile::parse`] then [`PaletteFile::emit`]
//!   with no edit reproduces the input byte-for-byte.
//! - **Single-span edits**: [`PaletteFile::set_value`] touches exactly the value
//!   of the targeted key and leaves indentation, spacing around `=`, trailing
//!   whitespace, comments, and all other lines untouched.
//!
//! # Scope in v1
//!
//! No v1 UI edits `colors/<scheme>` directly: palette *switching* runs
//! `generate-colors` (R3.2), not a per-key rewrite. The write path
//! ([`set_value`](PaletteFile::set_value)) exists for the R6.1 round-trip
//! coverage and to support future per-key palette editing; it is intentionally
//! present and tested even though nothing wires it to a widget yet.
//!
//! # The schema comes from the generator
//!
//! Which keys a scheme must define is decided by `generate-colors`, which
//! aborts unless every name of its `NAMES=(…)` bash array is present. That list
//! grew from 17 to 18 keys when `shadow` was added (analysis §7.6), and a
//! hard-coded copy in this app would reject every valid scheme the next time it
//! grows. So validation takes a [`PaletteSchema`] value: normally built by
//! [`parse_names_line`] from the generator's own text, otherwise the built-in
//! [`BUILT_IN_PALETTE_KEYS`] fallback, and always labelled with its
//! [`SchemaSource`] so a validation report can say which list it applied
//! (R8.3). This module only parses text it is given; finding and reading the
//! script (via the palette source discovered per R8.5) happens in
//! [`crate::core::theme`].

use std::fmt;
use std::path::{Path, PathBuf};

use crate::parsers::kv::{KvEditError, KvFile, KvLine, Occurrence, is_key_char};

/// The built-in palette schema: the 18 keys `scripts/generate-colors` requires
/// as of dotfiles commit `8a5b20b` (analysis §7.6), in the generator's order.
///
/// This is only the **fallback**. The authoritative key list is the
/// generator's own `NAMES=(…)` array, which [`PaletteSchema::from_generator_script`]
/// reads, so a key added to the dotfiles (as `shadow` was, after the original
/// 17) does not make every valid scheme fail validation until the app is
/// updated. The fallback applies when the script cannot be read or its `NAMES`
/// line cannot be understood; it should be kept in step with the real script so
/// the fallback is as close to the truth as possible.
pub const BUILT_IN_PALETTE_KEYS: [&str; 18] = [
    "bg0", "bg1", "bg2", "bg3", "fg0", "fg1", "fg2", "accent0", "accent1", "accent2", "accent3",
    "red", "orange", "yellow", "green", "blue", "purple", "shadow",
];

/// The exact text that opens the generator's schema line. It must start the
/// line (column 0): the real script assigns the array at top level, and a
/// comment or an indented mention elsewhere in the file (the fixture stub's
/// header talks about `NAMES`, for example) is not the assignment.
const NAMES_LINE_PREFIX: &str = "NAMES=(";

/// Where the key list of a [`PaletteSchema`] came from.
///
/// Carried into every [`SchemaValidation`] so a validation failure can say
/// which list it was checked against (R8.3): "missing `shadow` according to
/// `<repo>/scripts/generate-colors`" and "missing `shadow` according to the
/// app's built-in list" call for different fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaSource {
    /// The keys were read from the `NAMES=(…)` line of this generator script —
    /// the same list the generator itself checks before it emits anything.
    Generator(PathBuf),
    /// The generator script was unreadable or its `NAMES` line was absent or
    /// malformed, so the app's [`BUILT_IN_PALETTE_KEYS`] applied.
    BuiltIn,
}

impl fmt::Display for SchemaSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaSource::Generator(path) => {
                write!(f, "the NAMES line of {}", path.display())
            }
            SchemaSource::BuiltIn => {
                write!(f, "the built-in {}-key list", BUILT_IN_PALETTE_KEYS.len())
            }
        }
    }
}

/// The set of keys every `colors/<scheme>` file must define, together with
/// where that set came from (R3.2, R8.3; analysis §7.6).
///
/// A value rather than a constant because the authoritative list lives in the
/// dotfiles, not in this app: `generate-colors` aborts on any key of its
/// `NAMES` array that a scheme lacks, so the app validates against that same
/// array whenever it can read it. Building one is pure — the caller supplies
/// the script's text; locating and reading the script is the caller's job
/// ([`load_palette_schema`](crate::core::theme::load_palette_schema)), which
/// keeps detection logic out of this parser module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteSchema {
    /// The required keys in the order the source lists them, without
    /// duplicates. Never empty.
    keys: Vec<String>,
    /// Which list the keys came from.
    source: SchemaSource,
}

impl PaletteSchema {
    /// The fallback schema: [`BUILT_IN_PALETTE_KEYS`], labelled
    /// [`SchemaSource::BuiltIn`].
    pub fn built_in() -> PaletteSchema {
        PaletteSchema {
            keys: BUILT_IN_PALETTE_KEYS
                .iter()
                .map(|key| key.to_string())
                .collect(),
            source: SchemaSource::BuiltIn,
        }
    }

    /// Builds the schema from the text of a `generate-colors` script, labelled
    /// with the script's `path`.
    ///
    /// Fails with the reason when the script's `NAMES=(…)` line is absent or
    /// malformed (see [`parse_names_line`]); the caller then falls back to
    /// [`PaletteSchema::built_in`] and logs the reason. `path` is used only as
    /// the [`SchemaSource::Generator`] label — nothing is read from it here.
    pub fn from_generator_script(
        script: &str,
        path: &Path,
    ) -> Result<PaletteSchema, NamesLineError> {
        Ok(PaletteSchema {
            keys: parse_names_line(script)?,
            source: SchemaSource::Generator(path.to_path_buf()),
        })
    }

    /// The required keys, in the source's order.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Where the keys came from.
    pub fn source(&self) -> &SchemaSource {
        &self.source
    }

    /// Whether `key` is one of the required keys.
    pub fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|schema_key| schema_key == key)
    }
}

/// Why a generator script's `NAMES=(…)` line could not be used as the palette
/// schema. Every case makes the caller fall back to the built-in list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamesLineError {
    /// No line starts with `NAMES=(`.
    Absent,
    /// More than one line starts with `NAMES=(`. Which assignment the
    /// generator's check sees depends on bash control flow this parser does not
    /// model, so neither is trusted. Carries the 1-based line numbers.
    Ambiguous(Vec<usize>),
    /// The array is not closed by `)` on the same line. A multi-line array is
    /// valid bash, but the generator has always written the schema on one line,
    /// so anything else is treated as unrecognised rather than half-understood.
    /// Carries the 1-based line number.
    Unterminated(usize),
    /// The closing `)` is followed by something other than whitespace or a `#`
    /// comment — e.g. a second command on the same line, which this parser does
    /// not interpret. Carries the 1-based line number.
    TrailingText(usize),
    /// The parentheses hold no names. Carries the 1-based line number.
    Empty(usize),
    /// An element is not a bare palette key (ASCII letters, digits and `_`):
    /// a quoted word, a `$variable` or a glob would need bash to expand it.
    /// Carries the 1-based line number and the offending word.
    InvalidName(usize, String),
}

impl fmt::Display for NamesLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamesLineError::Absent => write!(f, "no line starts with `{NAMES_LINE_PREFIX}`"),
            NamesLineError::Ambiguous(lines) => {
                write!(
                    f,
                    "several lines start with `{NAMES_LINE_PREFIX}`: {lines:?}"
                )
            }
            NamesLineError::Unterminated(line) => write!(
                f,
                "line {line}: the NAMES array is not closed on its own line"
            ),
            NamesLineError::TrailingText(line) => write!(
                f,
                "line {line}: the NAMES array is not followed only by whitespace or a comment"
            ),
            NamesLineError::Empty(line) => write!(f, "line {line}: the NAMES array is empty"),
            NamesLineError::InvalidName(line, word) => {
                write!(f, "line {line}: `{word}` is not a bare palette key")
            }
        }
    }
}

impl std::error::Error for NamesLineError {}

/// Extracts the palette key list from the `NAMES=(…)` line of a
/// `generate-colors` script (analysis §7.6).
///
/// The real script declares its schema as one top-level bash array,
/// `NAMES=(bg0 bg1 … purple shadow)`, and aborts when a scheme lacks any of
/// them. This reads exactly that shape: a line beginning at column 0 with
/// `NAMES=(`, whitespace-separated bare words, and a closing `)` on the same
/// line optionally followed by a `#` comment. Lines that merely mention
/// `NAMES` (comments, the `"${NAMES[@]}"` loop) never match because of the
/// column-0 anchor. A repeated name is kept once, in first-seen order — bash
/// would simply check it twice. Anything this does not understand is an
/// error, never a guess, so the caller can fall back to the built-in list.
pub fn parse_names_line(script: &str) -> Result<Vec<String>, NamesLineError> {
    let candidates: Vec<(usize, &str)> = script
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            line.strip_prefix(NAMES_LINE_PREFIX)
                .map(|rest| (index + 1, rest))
        })
        .collect();
    let (line_number, rest) = match candidates.as_slice() {
        [] => return Err(NamesLineError::Absent),
        [single] => *single,
        several => {
            return Err(NamesLineError::Ambiguous(
                several.iter().map(|(line, _)| *line).collect(),
            ));
        }
    };

    // `lines()` already dropped the `\n`; a CRLF file leaves a `\r`, which
    // `trim` below treats as whitespace.
    let Some((inside, after)) = rest.split_once(')') else {
        return Err(NamesLineError::Unterminated(line_number));
    };
    let after = after.trim();
    if !after.is_empty() && !after.starts_with('#') {
        return Err(NamesLineError::TrailingText(line_number));
    }

    let mut keys: Vec<String> = Vec::new();
    for word in inside.split_whitespace() {
        if !word.chars().all(is_key_char) {
            return Err(NamesLineError::InvalidName(line_number, word.to_string()));
        }
        if !keys.iter().any(|key| key == word) {
            keys.push(word.to_string());
        }
    }
    if keys.is_empty() {
        return Err(NamesLineError::Empty(line_number));
    }
    Ok(keys)
}

/// A parsed palette source file that can re-emit itself byte-for-byte and edit
/// individual values in place.
///
/// Built by [`PaletteFile::parse`]. The line model itself — every line kept verbatim,
/// each entry annotated with its value's byte span — is the shared `key=value` model of
/// [`crate::parsers::kv`], which notifd's settings file uses too (task 11.15). This type
/// adds the palette's own rules on top: values are bare-hex colours, each key is meant
/// to appear once (a duplicate is a schema error, and reads and edits address the
/// **first** entry), and the key set is checked against a [`PaletteSchema`].
#[derive(Clone, Debug)]
pub struct PaletteFile {
    /// The lossless line model. Emitting it reproduces the input exactly when nothing
    /// was edited (round-trip identity).
    file: KvFile,
}

/// A non-fatal problem noticed while parsing a palette file.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (R6.1 acceptance:
/// "malformed lines surfaced as parse warnings, not panics"). [`PaletteFile::parse`]
/// **returns** the collected warnings for the caller to log or ignore — it does not
/// log them itself (mirroring the sibling parsers), so a caller that probes arbitrary
/// files while enumerating palette schemes can drop them without flooding the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseWarning {
    /// 1-based line number the warning concerns, for human-readable diagnostics.
    line: usize,
    /// What was wrong with the line.
    kind: ParseWarningKind,
}

impl ParseWarning {
    /// The 1-based line number this warning concerns.
    pub fn line(&self) -> usize {
        self.line
    }

    /// What was wrong with the line.
    pub fn kind(&self) -> &ParseWarningKind {
        &self.kind
    }
}

impl fmt::Display for ParseWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseWarningKind::MalformedLine => write!(
                f,
                "line {}: not a comment, blank line, or key=value entry",
                self.line
            ),
            ParseWarningKind::InvalidHexValue { key, value } => write!(
                f,
                "line {}: value of `{key}` is not a bare-hex color (got `{value}`)",
                self.line
            ),
        }
    }
}

/// The specific reason a line produced a [`ParseWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseWarningKind {
    /// The line is not blank, not a comment, and not a `key=value` assignment
    /// with a valid identifier key — so it cannot be interpreted as a palette
    /// entry. It is kept byte-for-byte and simply ignored by edits and
    /// validation.
    MalformedLine,
    /// The line *is* a well-formed `key=value` entry, but its value is not a
    /// bare-hex color (six hexadecimal digits, no `#`). The entry is still
    /// addressable and editable; this warning just flags that its current value
    /// is out of the expected format.
    InvalidHexValue {
        /// The entry's key.
        key: String,
        /// The offending value as it appears in the file.
        value: String,
    },
}

/// The result of checking a palette against a [`PaletteSchema`] (R8.3;
/// analysis §7.6).
///
/// A file is schema-valid (see [`SchemaValidation::is_valid`]) exactly when all
/// three lists are empty: every schema key is present exactly once and no
/// out-of-schema key appears. This is intentionally *stricter* than what
/// `generate-colors` enforces: the generator checks only that every key of its
/// `NAMES` array is present (not the value format, and it ignores extra keys —
/// analysis §6.4, §7.6). We additionally reject unknown keys, because a key the
/// generator never reads is almost certainly a typo of one it does, and check
/// for duplicates because each key is meant to appear exactly once and a
/// duplicate would make [`PaletteFile::set_value`]'s first-match edit
/// ambiguous.
///
/// The report records which key list it was checked against
/// ([`schema_source`](Self::schema_source)), so a rejection can be explained
/// as "according to the generator" or "according to the app's built-in list".
#[derive(Debug, PartialEq, Eq)]
pub struct SchemaValidation {
    /// Where the schema this report was checked against came from.
    schema_source: SchemaSource,
    /// Entry keys present in the file that are **not** part of the schema, in
    /// first-seen order.
    unknown_keys: Vec<String>,
    /// Schema keys with **no** entry in the file, in schema order.
    /// `generate-colors` aborts on a missing key, so these must be filled before
    /// a palette apply can succeed. (A key whose only line is malformed or
    /// commented-out counts as missing, since neither is an entry.)
    missing_keys: Vec<String>,
    /// Keys that appear as an entry more than once, in first-seen order — a
    /// well-formed palette defines each key exactly once. Includes both schema
    /// and out-of-schema keys.
    duplicate_keys: Vec<String>,
}

impl SchemaValidation {
    /// Whether the palette conforms to the schema: every schema key present
    /// exactly once and no extras.
    pub fn is_valid(&self) -> bool {
        self.unknown_keys.is_empty()
            && self.missing_keys.is_empty()
            && self.duplicate_keys.is_empty()
    }

    /// Which key list the palette was checked against.
    pub fn schema_source(&self) -> &SchemaSource {
        &self.schema_source
    }

    /// Entry keys outside the schema.
    pub fn unknown_keys(&self) -> &[String] {
        &self.unknown_keys
    }

    /// Schema keys with no entry in the file.
    pub fn missing_keys(&self) -> &[String] {
        &self.missing_keys
    }

    /// Keys that appear as an entry more than once.
    pub fn duplicate_keys(&self) -> &[String] {
        &self.duplicate_keys
    }
}

/// A failure from [`PaletteFile::set_value`].
///
/// Both variants leave the file completely unchanged: the check happens before
/// any byte is spliced, so a rejected edit can never partially rewrite a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetValueError {
    /// No `key=value` entry with the requested key exists in the file. A key
    /// that appears only in a comment or a malformed line is *not* an entry and
    /// is reported here, so an edit can never accidentally target a commented-out
    /// line.
    UnknownKey(String),
    /// The requested new value is not a bare-hex color (six hexadecimal digits,
    /// no `#`). Rejecting it here upholds R8.3 — the app never writes an invalid
    /// value into a working config — independently of the Apply pipeline's own
    /// validation.
    InvalidValue(String),
}

impl fmt::Display for SetValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetValueError::UnknownKey(key) => {
                write!(f, "no palette entry named `{key}` to edit")
            }
            SetValueError::InvalidValue(value) => {
                write!(
                    f,
                    "`{value}` is not a bare-hex color (expected six hex digits)"
                )
            }
        }
    }
}

impl std::error::Error for SetValueError {}

impl PaletteFile {
    /// Parses palette source text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved, whether or not it is a valid
    /// entry, so [`emit`](Self::emit) always reproduces the input byte-for-byte.
    /// Lines that cannot be interpreted as a palette entry (and are not comments
    /// or blanks) and entries whose value is not bare hex are returned as
    /// [`ParseWarning`]s, in line order; they are *not* errors and do not stop parsing
    /// (R6.1 acceptance).
    ///
    /// Like the sibling parsers (hyprlang, INI, env), warnings are **returned** for
    /// the caller to log or ignore — this parser does not log them itself. That
    /// distinction matters because the same parse is used both to read the user's
    /// actual scheme file *and* to probe arbitrary files while enumerating the
    /// palette schemes (task 6.3, via [`crate::parsers::generated::parse_scheme_swatch`]):
    /// a non-palette file such as a `README.md` would otherwise emit a `warn` for
    /// every line it holds. The probe drops the warnings; a future per-key editor can
    /// log them. The returned warnings carry only line numbers and, for a bad value,
    /// the offending key/value — never the whole file's contents (R7.3).
    pub fn parse(input: &str) -> (Self, Vec<ParseWarning>) {
        let file = KvFile::parse(input);
        let warnings = file
            .lines()
            .filter_map(|(line, view)| {
                let kind = match view {
                    KvLine::Malformed => ParseWarningKind::MalformedLine,
                    KvLine::Entry { key, value } if !is_bare_hex(value) => {
                        ParseWarningKind::InvalidHexValue {
                            key: key.to_string(),
                            value: value.to_string(),
                        }
                    }
                    KvLine::Blank | KvLine::Comment | KvLine::Entry { .. } => return None,
                };
                Some(ParseWarning { line, kind })
            })
            .collect();
        (PaletteFile { file }, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After [`set_value`](Self::set_value) edits, the output is identical to the
    /// input except within the edited value spans.
    pub fn emit(&self) -> String {
        self.file.emit()
    }

    /// Returns the current value of `key`, if the file has an entry for it.
    ///
    /// Reads the first entry with that key (a well-formed palette has at most
    /// one). Comment and malformed lines are never considered.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.file.value(key, Occurrence::First)
    }

    /// Rewrites the value of `key` to `value`, changing exactly that one value
    /// span and nothing else.
    ///
    /// `value` must be a bare-hex color (six hexadecimal digits, no `#`) or the
    /// edit is rejected with [`SetValueError::InvalidValue`] and the file is left
    /// unchanged (R8.3). If no entry with `key` exists — including when it
    /// appears only in a comment or malformed line — the edit is rejected with
    /// [`SetValueError::UnknownKey`].
    ///
    /// Only the value's byte span is replaced: the key, the spacing around `=`,
    /// any leading indentation or trailing whitespace, the line terminator, and
    /// every other line are left byte-identical. If a key somehow appears more
    /// than once (a schema violation [`validate`](Self::validate) would report),
    /// only the first occurrence is edited.
    pub fn set_value(&mut self, key: &str, value: &str) -> Result<(), SetValueError> {
        if !is_bare_hex(value) {
            return Err(SetValueError::InvalidValue(value.to_string()));
        }
        match self.file.set_value(key, Occurrence::First, value) {
            Ok(()) => {
                tracing::debug!(key, value, "rewrote palette value");
                Ok(())
            }
            Err(KvEditError::UnknownKey(key)) => Err(SetValueError::UnknownKey(key)),
            // Six hex digits never contain a line break or surrounding whitespace, and
            // no key is appended here, so the line model has nothing else to refuse;
            // mapped rather than unwrapped so the function stays panic-free.
            Err(KvEditError::InvalidKey(_) | KvEditError::UnsafeValue(_)) => {
                Err(SetValueError::InvalidValue(value.to_string()))
            }
        }
    }

    /// Checks the file's entry keys against `schema` (R8.3).
    ///
    /// See [`SchemaValidation`] for the exact semantics: it reports out-of-schema
    /// keys, missing schema keys, and duplicate keys, and considers the palette
    /// valid only when all three are empty. This is independent of value-format
    /// validation — a present-but-non-hex value counts the key as present here
    /// and is flagged instead by a parse [`ParseWarningKind::InvalidHexValue`].
    pub fn validate(&self, schema: &PaletteSchema) -> SchemaValidation {
        // Count each entry key's occurrences, preserving first-seen order for
        // stable, readable reports.
        let mut counts: Vec<(String, usize)> = Vec::new();
        for entry in self.file.entries() {
            match counts.iter_mut().find(|(seen, _)| seen == entry.key) {
                Some((_, count)) => *count += 1,
                None => counts.push((entry.key.to_string(), 1)),
            }
        }

        let unknown_keys = counts
            .iter()
            .filter(|(key, _)| !schema.contains(key))
            .map(|(key, _)| key.clone())
            .collect();

        let duplicate_keys = counts
            .iter()
            .filter(|(_, count)| *count > 1)
            .map(|(key, _)| key.clone())
            .collect();

        let missing_keys = schema
            .keys()
            .iter()
            .filter(|schema_key| !counts.iter().any(|(key, _)| key == *schema_key))
            .cloned()
            .collect();

        SchemaValidation {
            schema_source: schema.source().clone(),
            unknown_keys,
            missing_keys,
            duplicate_keys,
        }
    }
}

/// Whether `value` is a bare-hex color: exactly six ASCII hexadecimal digits and
/// no leading `#` (analysis §2 — the palette value format).
///
/// Case-insensitive, matching how hex is read downstream; the dotfiles write
/// lowercase but either case is a valid color.
///
/// Exposed at crate scope because it is the single definition of "a palette
/// color": the typed settings model's hex validator ([`crate::core::model`], task
/// 4.1, R8.3) reuses it so the parser's write guard and the pipeline's pre-write
/// validation cannot disagree about what a valid color is.
pub fn is_bare_hex(value: &str) -> bool {
    value.len() == 6 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `colors/<scheme>` fixture: a header comment, blank-line
    /// grouping, a commented-out example entry, all 18 schema keys with real
    /// Everforest bare-hex values, and a trailing newline — the shape of a
    /// hand-maintained palette source (analysis §2, §6.4, §7.6).
    const EVERFOREST_FIXTURE: &str = "\
# Everforest Dark palette
# edit this file, then run: scripts/generate-colors everforest

# Backgrounds
bg0=272e33
bg1=2e383c
bg2=374145
bg3=414b50

# Foregrounds
fg0=d3c6aa
fg1=9da9a0
fg2=859289

# Accents
accent0=83c092
accent1=a7c080
accent2=7fbbb3
accent3=d699b6

# Named colors
red=e67e80
orange=e69875
yellow=dbbc7f
green=a7c080
blue=7fbbb3
purple=d699b6

# Shadow tint
shadow=0a0c0d

# example override (disabled): highlight=ffffff
";

    #[test]
    fn round_trip_identity_on_a_realistic_fixture() {
        // Headline guarantee (R6.1, architecture §3): parse → emit with no edit
        // reproduces the input byte-for-byte, comments/blanks/order included.
        let (palette, warnings) = PaletteFile::parse(EVERFOREST_FIXTURE);
        assert_eq!(
            palette.emit(),
            EVERFOREST_FIXTURE,
            "emit must reproduce the input byte-for-byte"
        );
        assert!(
            warnings.is_empty(),
            "a clean, well-formed palette must yield no parse warnings, got {warnings:?}"
        );
        // The commented-out `highlight=ffffff` line must NOT be read as an entry.
        assert!(
            palette.value("highlight").is_none(),
            "a commented-out line must never be treated as an entry"
        );
        // A clean fixture with all 18 keys is valid on the built-in schema.
        assert!(palette.validate(&PaletteSchema::built_in()).is_valid());
    }

    #[test]
    fn round_trips_empty_input_and_a_final_line_without_a_newline() {
        // Edge cases for the split/emit contract: emptiness and a missing trailing
        // newline must both survive round-trip.
        for input in [
            "",
            "bg0=272e33",
            "# only a comment",
            "\n\n",
            "bg0=272e33\n# tail",
        ] {
            let (palette, _) = PaletteFile::parse(input);
            assert_eq!(palette.emit(), input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn editing_one_key_changes_exactly_that_value_span() {
        // Accept criterion: an edit changes exactly one value span and leaves
        // every other byte untouched. Prove it by comparing the emitted output to
        // the input line-by-line: only the accent0 line may differ, and it must
        // differ only in its value bytes (the `accent0=` prefix and the rest
        // stay identical).
        let (mut palette, _) = PaletteFile::parse(EVERFOREST_FIXTURE);
        palette
            .set_value("accent0", "abcdef")
            .expect("accent0 exists and abcdef is valid hex");

        let edited = palette.emit();

        let original_lines: Vec<&str> = EVERFOREST_FIXTURE.lines().collect();
        let edited_lines: Vec<&str> = edited.lines().collect();
        assert_eq!(
            original_lines.len(),
            edited_lines.len(),
            "an edit must not add or remove lines"
        );

        let differing: Vec<usize> = original_lines
            .iter()
            .zip(&edited_lines)
            .enumerate()
            .filter_map(|(index, (before, after))| (before != after).then_some(index))
            .collect();
        let accent0_index = original_lines
            .iter()
            .position(|line| *line == "accent0=83c092")
            .expect("the fixture contains the accent0 entry");
        assert_eq!(
            differing,
            vec![accent0_index],
            "exactly the accent0 line must change"
        );
        assert_eq!(
            edited_lines[accent0_index], "accent0=abcdef",
            "only the value span changed: the key, `=`, and layout are preserved"
        );
        assert_eq!(palette.value("accent0"), Some("abcdef"));
    }

    #[test]
    fn editing_preserves_surrounding_whitespace() {
        // The value span excludes indentation, spaces around `=`, and trailing
        // whitespace, so those are preserved verbatim across an edit.
        let input = "  accent2 = 7fbbb3   \n";
        let (mut palette, warnings) = PaletteFile::parse(input);
        assert!(warnings.is_empty());
        palette.set_value("accent2", "abcdef").expect("valid edit");
        assert_eq!(
            palette.emit(),
            "  accent2 = abcdef   \n",
            "only the six value bytes change; all surrounding whitespace is preserved"
        );
    }

    #[test]
    fn set_value_rejects_unknown_key_and_invalid_hex_without_changing_the_file() {
        let (mut palette, _) = PaletteFile::parse(EVERFOREST_FIXTURE);

        // A key that does not exist as an entry is rejected...
        assert_eq!(
            palette.set_value("nonexistent", "abcdef"),
            Err(SetValueError::UnknownKey("nonexistent".to_string()))
        );
        // ...as is a key that appears only in a comment (never matched).
        assert_eq!(
            palette.set_value("highlight", "abcdef"),
            Err(SetValueError::UnknownKey("highlight".to_string()))
        );
        // A non-hex value is rejected before any byte is written (R8.3).
        assert_eq!(
            palette.set_value("bg0", "#abcdef"),
            Err(SetValueError::InvalidValue("#abcdef".to_string()))
        );
        assert_eq!(
            palette.set_value("bg0", "12345"),
            Err(SetValueError::InvalidValue("12345".to_string()))
        );

        // None of the rejected edits changed the file.
        assert_eq!(palette.emit(), EVERFOREST_FIXTURE);
    }

    #[test]
    fn validation_flags_unknown_and_missing_keys() {
        // Accept criterion: schema validation reports out-of-schema keys and
        // missing keys. This palette drops `purple` and adds an unknown `magenta`
        // (and, being a pre-`shadow` 17-key layout, also lacks `shadow`).
        let input = "\
bg0=272e33
bg1=2e383c
bg2=374145
bg3=414b50
fg0=d3c6aa
fg1=9da9a0
fg2=859289
accent0=83c092
accent1=a7c080
accent2=7fbbb3
accent3=d699b6
red=e67e80
orange=e69875
yellow=dbbc7f
green=a7c080
blue=7fbbb3
magenta=d699b6
";
        let (palette, _) = PaletteFile::parse(input);
        let report = palette.validate(&PaletteSchema::built_in());
        assert!(!report.is_valid());
        assert_eq!(report.unknown_keys(), ["magenta".to_string()]);
        assert_eq!(report.missing_keys(), ["purple", "shadow"]);
        assert!(report.duplicate_keys().is_empty());
        assert_eq!(report.schema_source(), &SchemaSource::BuiltIn);
    }

    #[test]
    fn validation_flags_duplicate_keys() {
        // A key defined twice is a schema violation and would make set_value's
        // first-match edit ambiguous, so it is reported.
        let mut input = String::new();
        for key in BUILT_IN_PALETTE_KEYS {
            input.push_str(&format!("{key}=abcdef\n"));
        }
        input.push_str("bg0=123456\n"); // bg0 now appears twice

        let (palette, _) = PaletteFile::parse(&input);
        let report = palette.validate(&PaletteSchema::built_in());
        assert!(!report.is_valid());
        assert_eq!(report.duplicate_keys(), ["bg0".to_string()]);
        assert!(report.unknown_keys().is_empty());
        assert!(report.missing_keys().is_empty());
    }

    #[test]
    fn a_malformed_line_is_preserved_and_warned_without_panicking() {
        // Accept criterion: a malformed line surfaces as a parse warning, not a
        // panic, and is preserved losslessly.
        let input = "\
bg0=272e33
this line has no equals sign
bg1=2e383c
";
        let (palette, warnings) = PaletteFile::parse(input);

        // Lossless: the malformed line is kept byte-for-byte.
        assert_eq!(palette.emit(), input);

        // Surfaced as a warning on the right (1-based) line, not a panic.
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 2);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::MalformedLine);

        // The malformed line is not an addressable entry.
        assert!(palette.value("this").is_none());
    }

    #[test]
    fn a_non_hex_value_is_warned_but_stays_an_editable_entry() {
        // A well-formed assignment with a bad value is reported as an
        // InvalidHexValue warning, yet remains an editable entry (so it can be
        // fixed) and round-trips losslessly.
        let input = "bg0=notacolor\nbg1=2e383c\n";
        let (mut palette, warnings) = PaletteFile::parse(input);

        assert_eq!(palette.emit(), input, "the bad value must be preserved");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 1);
        assert_eq!(
            warnings[0].kind(),
            &ParseWarningKind::InvalidHexValue {
                key: "bg0".to_string(),
                value: "notacolor".to_string(),
            }
        );

        // The entry is still addressable and its value readable and editable.
        assert_eq!(palette.value("bg0"), Some("notacolor"));
        palette.set_value("bg0", "abcdef").expect("editable entry");
        assert_eq!(palette.emit(), "bg0=abcdef\nbg1=2e383c\n");
    }

    #[test]
    fn crlf_line_endings_round_trip_and_an_edit_never_pulls_in_the_carriage_return() {
        // A file with Windows `\r\n` endings must round-trip byte-for-byte (the
        // `\r` is part of the terminator, not the value), and an edit must change
        // only the six value bytes — never dragging the `\r` into the value span.
        let input = "# CRLF palette\r\nbg0=272e33\r\nbg1=2e383c\r\n";
        let (mut palette, warnings) = PaletteFile::parse(input);
        assert!(
            warnings.is_empty(),
            "CRLF values are still valid hex; no warnings expected, got {warnings:?}"
        );

        // Round-trip identity: every `\r\n` survives.
        assert_eq!(palette.emit(), input, "CRLF endings must be preserved");

        // The value span stops before the `\r`, so the read value is clean...
        assert_eq!(palette.value("bg0"), Some("272e33"));

        // ...and editing rewrites only those six bytes, leaving `\r\n` intact.
        palette.set_value("bg0", "abcdef").expect("valid edit");
        assert_eq!(
            palette.emit(),
            "# CRLF palette\r\nbg0=abcdef\r\nbg1=2e383c\r\n",
            "only the value changed; the carriage return and newline are untouched"
        );
    }

    #[test]
    fn an_empty_value_parses_as_an_editable_entry_and_warns() {
        // `bg0=` (nothing after the `=`) is a well-formed assignment with an empty
        // value: it must parse as an addressable Entry (not a malformed line),
        // warn as InvalidHexValue, round-trip losslessly, and never panic. The
        // empty value span sits right after the `=`, so an edit inserts there.
        let input = "bg0=\nbg1=2e383c\n";
        let (mut palette, warnings) = PaletteFile::parse(input);

        assert_eq!(palette.emit(), input, "the empty value must be preserved");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 1);
        assert_eq!(
            warnings[0].kind(),
            &ParseWarningKind::InvalidHexValue {
                key: "bg0".to_string(),
                value: String::new(),
            }
        );

        // The entry is addressable with an empty current value, and editing it
        // splices the new value into the empty span without disturbing anything.
        assert_eq!(palette.value("bg0"), Some(""));
        palette.set_value("bg0", "abcdef").expect("editable entry");
        assert_eq!(palette.emit(), "bg0=abcdef\nbg1=2e383c\n");
    }

    #[test]
    fn set_value_edits_only_the_first_occurrence_of_a_duplicate_key() {
        // Pins the documented first-match behavior: when a key appears twice (a
        // schema violation validate() reports), set_value rewrites exactly the
        // first occurrence and leaves the second byte-identical.
        let input = "bg0=111111\nbg1=2e383c\nbg0=222222\n";
        let (mut palette, _) = PaletteFile::parse(input);

        palette
            .set_value("bg0", "abcdef")
            .expect("first bg0 is editable");

        assert_eq!(
            palette.emit(),
            "bg0=abcdef\nbg1=2e383c\nbg0=222222\n",
            "only the first bg0 value changes; the duplicate is untouched"
        );
        // `value()` also reads the first occurrence, consistent with set_value.
        assert_eq!(palette.value("bg0"), Some("abcdef"));
    }

    #[test]
    fn is_bare_hex_accepts_six_digits_of_either_case_and_rejects_the_rest() {
        assert!(is_bare_hex("272e33"));
        assert!(is_bare_hex("ABCDEF"));
        assert!(!is_bare_hex("#272e33"), "a leading # is not bare hex");
        assert!(!is_bare_hex("272e3"), "too short");
        assert!(!is_bare_hex("272e333"), "too long");
        assert!(!is_bare_hex("gggggg"), "non-hex digits");
        assert!(!is_bare_hex(""), "empty");
    }

    /// The schema-bearing excerpt of the real `scripts/generate-colors` at
    /// dotfiles commit `8a5b20b`: the comment and loop lines around it mention
    /// `NAMES` too, which is exactly why the match is anchored at column 0.
    const GENERATOR_EXCERPT: &str = "\
#!/usr/bin/env bash
# the palette schema is taken from its `NAMES` array line
declare -A colors
NAMES=(bg0 bg1 bg2 bg3 fg0 fg1 fg2 accent0 accent1 accent2 accent3 red orange yellow green blue purple shadow)

# Verify all expected colors are present
for name in \"${NAMES[@]}\"; do
    :
done
";

    /// Renders a palette with one `key=abcdef` entry per key.
    fn palette_with(keys: &[&str]) -> PaletteFile {
        let text: String = keys.iter().map(|key| format!("{key}=abcdef\n")).collect();
        PaletteFile::parse(&text).0
    }

    #[test]
    fn the_names_line_of_the_real_generator_yields_the_18_key_schema() {
        let keys = parse_names_line(GENERATOR_EXCERPT).expect("the real line parses");
        assert_eq!(
            keys, BUILT_IN_PALETTE_KEYS,
            "the fallback mirrors the script"
        );
        assert_eq!(keys.last().map(String::as_str), Some("shadow"));

        let path = Path::new("/repo/scripts/generate-colors");
        let schema = PaletteSchema::from_generator_script(GENERATOR_EXCERPT, path)
            .expect("the real line parses");
        assert_eq!(
            schema.source(),
            &SchemaSource::Generator(path.to_path_buf())
        );
    }

    #[test]
    fn a_names_mention_that_does_not_start_the_line_is_never_matched() {
        // Comments, indented assignments and the `"${NAMES[@]}"` loop all
        // contain `NAMES`, some even `NAMES=(`; none is the top-level schema.
        let only_mentions = "\
# NAMES=(fake list in a comment)
    NAMES=(indented inside a function)
echo NAMES=(not an assignment)
for name in \"${NAMES[@]}\"; do :; done
";
        assert_eq!(parse_names_line(only_mentions), Err(NamesLineError::Absent));

        // With the real line present as well, the mentions do not make it
        // ambiguous, and only the real line's keys are taken.
        let with_real_line = format!("{only_mentions}NAMES=(bg0 shadow)\n");
        assert_eq!(
            parse_names_line(&with_real_line),
            Ok(vec!["bg0".to_string(), "shadow".to_string()])
        );
    }

    #[test]
    fn the_names_line_tolerates_spacing_a_trailing_comment_crlf_and_repeats() {
        assert_eq!(
            parse_names_line("NAMES=(  bg0\tbg1  bg0 )  # the schema\r\n"),
            Ok(vec!["bg0".to_string(), "bg1".to_string()]),
            "extra whitespace, a comment after `)`, a CR and a repeated name are fine"
        );
    }

    #[test]
    fn a_names_line_the_parser_cannot_understand_is_an_error_not_a_guess() {
        // Each of these makes the caller fall back to the built-in list.
        let cases = [
            ("x=1\n", NamesLineError::Absent),
            ("NAMES=(bg0\n  bg1)\n", NamesLineError::Unterminated(1)),
            ("NAMES=(bg0 bg1) extra\n", NamesLineError::TrailingText(1)),
            ("NAMES=( )\n", NamesLineError::Empty(1)),
            (
                "NAMES=(bg0 \"bg1\")\n",
                NamesLineError::InvalidName(1, "\"bg1\"".to_string()),
            ),
            (
                "NAMES=(bg0 $EXTRA)\n",
                NamesLineError::InvalidName(1, "$EXTRA".to_string()),
            ),
            (
                "NAMES=(bg0)\nNAMES=(bg0 bg1)\n",
                NamesLineError::Ambiguous(vec![1, 2]),
            ),
        ];
        for (script, expected) in cases {
            assert_eq!(parse_names_line(script), Err(expected), "for {script:?}");
            assert!(
                PaletteSchema::from_generator_script(script, Path::new("/g")).is_err(),
                "no schema is built from {script:?}"
            );
        }
    }

    #[test]
    fn a_17_key_scheme_is_missing_shadow_according_to_the_generator() {
        // The pre-`shadow` layout of every scheme before analysis §7.6.
        let path = Path::new("/repo/scripts/generate-colors");
        let schema = PaletteSchema::from_generator_script(GENERATOR_EXCERPT, path)
            .expect("the real line parses");
        let report = palette_with(&BUILT_IN_PALETTE_KEYS[..17]).validate(&schema);
        assert!(!report.is_valid());
        assert_eq!(report.missing_keys(), ["shadow"]);
        assert!(report.unknown_keys().is_empty());
        assert_eq!(
            report.schema_source(),
            &SchemaSource::Generator(path.to_path_buf()),
            "the report names the generator as the list it applied"
        );
    }

    #[test]
    fn a_19th_generator_name_is_required_without_a_code_change() {
        // A future dotfiles key: the generator grows a 19th name, so a 19-key
        // scheme is valid and today's 18-key scheme lacks the new key.
        let script = GENERATOR_EXCERPT.replace("purple shadow)", "purple shadow glow)");
        let schema = PaletteSchema::from_generator_script(&script, Path::new("/g"))
            .expect("the extended line parses");
        assert_eq!(schema.keys().len(), 19);

        let mut nineteen: Vec<&str> = BUILT_IN_PALETTE_KEYS.to_vec();
        nineteen.push("glow");
        assert!(palette_with(&nineteen).validate(&schema).is_valid());
        // The same 19-key file is invalid on the built-in list (unknown `glow`).
        assert_eq!(
            palette_with(&nineteen)
                .validate(&PaletteSchema::built_in())
                .unknown_keys(),
            ["glow"]
        );

        let report = palette_with(&BUILT_IN_PALETTE_KEYS).validate(&schema);
        assert_eq!(report.missing_keys(), ["glow"]);
    }

    #[test]
    fn the_schema_source_reads_as_a_plain_sentence_fragment() {
        assert_eq!(
            SchemaSource::Generator(PathBuf::from("/repo/scripts/generate-colors")).to_string(),
            "the NAMES line of /repo/scripts/generate-colors"
        );
        assert_eq!(
            SchemaSource::BuiltIn.to_string(),
            "the built-in 18-key list"
        );
    }
}
