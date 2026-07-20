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

use std::fmt;

/// The fixed 17-name palette schema every scheme file must define
/// (analysis §2, §6.4).
///
/// `generate-colors` validates that all of these keys are present before it
/// emits anything, and treats the palette as invalid if any is missing. The
/// order here is the canonical order used throughout the dotfiles docs; it is
/// used only to report [missing keys](SchemaValidation::missing_keys) in a
/// stable, human-recognizable order and does **not** constrain the order keys
/// may appear in a file.
pub const PALETTE_KEYS: [&str; 17] = [
    "bg0", "bg1", "bg2", "bg3", "fg0", "fg1", "fg2", "accent0", "accent1", "accent2", "accent3",
    "red", "orange", "yellow", "green", "blue", "purple",
];

/// A parsed palette source file that can re-emit itself byte-for-byte and edit
/// individual values in place.
///
/// Built by [`PaletteFile::parse`]. Internally it is just the file's lines in
/// order, each classified and — for a real `key=value` entry — annotated with
/// the byte span of its value within that line's raw text. Emitting concatenates
/// the raw line texts, so an unedited file reproduces its input exactly; editing
/// a value splices new bytes into a single line's value span and updates that
/// one span, leaving every other line's bytes alone.
#[derive(Clone, Debug)]
pub struct PaletteFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity).
    lines: Vec<Line>,
}

/// One physical line of the file, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of this line **including its terminator**
    /// (`\n` or `\r\n`, or none for a final line with no trailing newline).
    /// This is what [`PaletteFile::emit`] writes back, so it is never rewritten
    /// except by [`PaletteFile::set_value`], which splices only the value span.
    raw: String,
    /// How this line was classified during parsing.
    kind: LineKind,
}

/// The classification of a single line.
///
/// Only [`LineKind::Entry`] lines are addressable by [`PaletteFile::set_value`]
/// and counted by [`PaletteFile::validate`]; blanks, comments, and malformed
/// lines are preserved verbatim and never matched by an edit (so a commented-out
/// `#bg0=…` line can never be mistaken for the real `bg0` entry).
#[derive(Clone, Debug)]
enum LineKind {
    /// A line that is empty or only whitespace.
    Blank,
    /// A comment: the first non-whitespace character is `#`. A commented-out
    /// entry (`# bg0=272e33`) is a comment, never an entry.
    Comment,
    /// A `key=value` assignment with a non-empty identifier key. The value's
    /// byte range within [`Line::raw`] is recorded so it can be read and
    /// rewritten in place. The value is *not* required to be valid hex to be an
    /// entry — an out-of-format value is still an editable entry and is reported
    /// separately as a parse warning (see [`ParseWarningKind::InvalidHexValue`]).
    Entry {
        /// The assignment's key, with surrounding whitespace trimmed
        /// (e.g. `bg0`). Used to match [`PaletteFile::set_value`] targets and to
        /// count keys for [`PaletteFile::validate`].
        key: String,
        /// Byte offset within [`Line::raw`] where the value begins (after the
        /// `=` and any following whitespace).
        value_start: usize,
        /// Byte offset within [`Line::raw`] where the value ends (before any
        /// trailing whitespace and the line terminator). The half-open range
        /// `value_start..value_end` is exactly the bytes an edit replaces.
        value_end: usize,
    },
    /// A non-blank, non-comment line that is not a `key=value` assignment (no
    /// `=`, or an empty/non-identifier key). Preserved verbatim and surfaced as
    /// a [`ParseWarningKind::MalformedLine`] warning; never editable.
    Malformed,
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

/// The result of checking a palette against the fixed 17-key schema
/// (analysis §2, §6.4).
///
/// A file is schema-valid (see [`SchemaValidation::is_valid`]) exactly when all
/// three lists are empty: every schema key is present exactly once and no
/// out-of-schema key appears. This is intentionally *stricter* than what
/// `generate-colors` is documented to enforce: the generator validates only that
/// the 17 keys are all present (not value format, and it is not documented to
/// reject extra/unknown keys — analysis §2, §6.4). We additionally reject
/// unknown keys, and check for duplicates because each of the 17 keys is meant
/// to appear exactly once and a duplicate would make [`PaletteFile::set_value`]'s
/// first-match edit ambiguous.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SchemaValidation {
    /// Entry keys present in the file that are **not** part of the 17-key
    /// schema, in first-seen order. A generator run would reject the file for
    /// carrying these.
    unknown_keys: Vec<String>,
    /// Schema keys with **no** entry in the file, in canonical schema order.
    /// `generate-colors` aborts on a missing key, so these must be filled before
    /// a palette apply can succeed. (A key whose only line is malformed or
    /// commented-out counts as missing, since neither is an entry.)
    missing_keys: Vec<&'static str>,
    /// Keys that appear as an entry more than once, in first-seen order — a
    /// well-formed palette defines each key exactly once. Includes both schema
    /// and out-of-schema keys.
    duplicate_keys: Vec<String>,
}

impl SchemaValidation {
    /// Whether the palette conforms to the schema: all 17 keys present exactly
    /// once and no extras.
    pub fn is_valid(&self) -> bool {
        self.unknown_keys.is_empty()
            && self.missing_keys.is_empty()
            && self.duplicate_keys.is_empty()
    }

    /// Entry keys outside the fixed 17-key schema.
    pub fn unknown_keys(&self) -> &[String] {
        &self.unknown_keys
    }

    /// Schema keys with no entry in the file.
    pub fn missing_keys(&self) -> &[&'static str] {
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
    /// [`ParseWarning`]s; they are *not* errors and do not stop parsing (R6.1
    /// acceptance).
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
        let mut lines = Vec::new();
        let mut warnings = Vec::new();

        // `split_inclusive` keeps each line's terminator attached and yields the
        // final unterminated line as-is, so re-joining the pieces is exactly the
        // input. An empty input yields no pieces, so an empty file round-trips to
        // empty. This is what makes emission byte-for-byte lossless.
        for (index, raw) in input.split_inclusive('\n').enumerate() {
            let line_number = index + 1;
            let kind = classify_line(raw, line_number, &mut warnings);
            lines.push(Line {
                raw: raw.to_string(),
                kind,
            });
        }

        (PaletteFile { lines }, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After [`set_value`](Self::set_value) edits, the output is identical to the
    /// input except within the edited value spans.
    pub fn emit(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.raw);
        }
        out
    }

    /// Returns the current value of `key`, if the file has an entry for it.
    ///
    /// Reads the first entry with that key (a well-formed palette has at most
    /// one). Comment and malformed lines are never considered.
    pub fn value(&self, key: &str) -> Option<&str> {
        for line in &self.lines {
            if let LineKind::Entry {
                key: entry_key,
                value_start,
                value_end,
            } = &line.kind
            {
                if entry_key == key {
                    return Some(&line.raw[*value_start..*value_end]);
                }
            }
        }
        None
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

        // Destructure each line into its disjoint fields so the raw text can be
        // spliced while the value span (held in `kind`) is read and updated — two
        // borrows of different fields of the same struct.
        for Line { raw, kind } in &mut self.lines {
            if let LineKind::Entry {
                key: entry_key,
                value_start,
                value_end,
            } = kind
            {
                if entry_key.as_str() == key {
                    // Replace only the value bytes; `replace_range` shifts the
                    // trailing whitespace and terminator automatically, and no
                    // other line's `String` is touched.
                    raw.replace_range(*value_start..*value_end, value);
                    *value_end = *value_start + value.len();
                    tracing::debug!(key, value, "rewrote palette value");
                    return Ok(());
                }
            }
        }

        Err(SetValueError::UnknownKey(key.to_string()))
    }

    /// Checks the file's entry keys against the fixed 17-key schema.
    ///
    /// See [`SchemaValidation`] for the exact semantics: it reports out-of-schema
    /// keys, missing schema keys, and duplicate keys, and considers the palette
    /// valid only when all three are empty. This is independent of value-format
    /// validation — a present-but-non-hex value counts the key as present here
    /// and is flagged instead by a parse [`ParseWarningKind::InvalidHexValue`].
    pub fn validate(&self) -> SchemaValidation {
        // Count each entry key's occurrences, preserving first-seen order for
        // stable, readable reports.
        let mut counts: Vec<(String, usize)> = Vec::new();
        for line in &self.lines {
            if let LineKind::Entry { key, .. } = &line.kind {
                match counts.iter_mut().find(|(seen, _)| seen == key) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((key.clone(), 1)),
                }
            }
        }

        let unknown_keys = counts
            .iter()
            .filter(|(key, _)| !PALETTE_KEYS.contains(&key.as_str()))
            .map(|(key, _)| key.clone())
            .collect();

        let duplicate_keys = counts
            .iter()
            .filter(|(_, count)| *count > 1)
            .map(|(key, _)| key.clone())
            .collect();

        let missing_keys = PALETTE_KEYS
            .iter()
            .copied()
            .filter(|schema_key| !counts.iter().any(|(key, _)| key.as_str() == *schema_key))
            .collect();

        SchemaValidation {
            unknown_keys,
            missing_keys,
            duplicate_keys,
        }
    }
}

/// Classifies one raw line (terminator included) and records a parse warning if
/// it is malformed or carries a non-hex value.
///
/// The value span it computes for an entry is expressed as byte offsets into
/// `raw`, so it can be stored directly in [`LineKind::Entry`] and later used to
/// splice a replacement value.
fn classify_line(raw: &str, line_number: usize, warnings: &mut Vec<ParseWarning>) -> LineKind {
    // Work against the content without its line terminator so the terminator is
    // never mistaken for part of a value and stays out of the value span. The raw
    // string (with terminator) is what we re-emit; content is only for locating
    // the `=` and the value.
    let content = strip_terminator(raw);

    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        return LineKind::Blank;
    }
    if trimmed.starts_with('#') {
        // A comment — including a commented-out entry like `# bg0=272e33`. The `#`
        // check comes before the `=` check so such a line is never treated as an
        // editable entry.
        return LineKind::Comment;
    }

    // Try to read the line as `key=value`.
    let Some(eq) = content.find('=') else {
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::MalformedLine,
        });
        return LineKind::Malformed;
    };

    let key = content[..eq].trim();
    if key.is_empty() || !key.chars().all(is_key_char) {
        // No usable key (empty, or containing characters a palette key never has),
        // so this is not an assignment we can address.
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::MalformedLine,
        });
        return LineKind::Malformed;
    }

    // The value is the content after `=` with surrounding whitespace trimmed. The
    // offsets are relative to `content`, which shares the same origin as `raw`
    // (content is a prefix of raw), so they index into `raw` directly.
    let after_eq = &content[eq + 1..];
    let leading_ws = after_eq.len() - after_eq.trim_start().len();
    let value = after_eq.trim();
    let value_start = eq + 1 + leading_ws;
    let value_end = value_start + value.len();

    if !is_bare_hex(value) {
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::InvalidHexValue {
                key: key.to_string(),
                value: value.to_string(),
            },
        });
    }

    LineKind::Entry {
        key: key.to_string(),
        value_start,
        value_end,
    }
}

/// Returns `content` with a trailing `\n` or `\r\n` removed, so value-span
/// computation never runs into the line terminator.
///
/// The terminator is only stripped for locating the value; the caller keeps the
/// full `raw` (terminator included) for lossless emission.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

/// Whether `c` may appear in a palette key.
///
/// Palette keys are simple identifiers (`bg0`, `accent0`, `red`, …): ASCII
/// letters, digits, and underscore. A character outside this set makes the line
/// malformed rather than an entry.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
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
    /// grouping, a commented-out example entry, all 17 schema keys with real
    /// Everforest-style bare-hex values, and a trailing newline — the shape of a
    /// hand-maintained palette source (analysis §2, §6.4).
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
        // A clean fixture with all 17 keys is schema-valid.
        assert!(palette.validate().is_valid());
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
        // missing keys. This palette drops `purple` and adds an unknown `magenta`.
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
        let report = palette.validate();
        assert!(!report.is_valid());
        assert_eq!(report.unknown_keys(), ["magenta".to_string()]);
        assert_eq!(report.missing_keys(), ["purple"]);
        assert!(report.duplicate_keys().is_empty());
    }

    #[test]
    fn validation_flags_duplicate_keys() {
        // A key defined twice is a schema violation and would make set_value's
        // first-match edit ambiguous, so it is reported.
        let mut input = String::new();
        for key in PALETTE_KEYS {
            input.push_str(&format!("{key}=abcdef\n"));
        }
        input.push_str("bg0=123456\n"); // bg0 now appears twice

        let (palette, _) = PaletteFile::parse(&input);
        let report = palette.validate();
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
}
