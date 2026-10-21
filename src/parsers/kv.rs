//! Lossless line model for flat `key=value` files (architecture §3; R6.1).
//!
//! # Which files use this grammar
//!
//! Two files the app reads share one deliberately simple format:
//!
//! - a dotfiles palette source, `colors/<scheme>` (bare-hex colours, task 3.1 —
//!   [`crate::parsers::palette`] layers the palette rules on top of this module);
//! - notifd's settings file, `~/.config/notifd/config` (task 11.15, analysis §8.2 —
//!   [`crate::core::notifd_config`] layers the daemon's keys and ranges on top).
//!
//! The format, as both consumers read it:
//!
//! ```text
//! # a comment line (the first non-blank character is `#`)
//!
//! key=value
//!   spaced_key = value with inner spaces
//! ```
//!
//! One assignment per line. Whitespace around the key and around the value is not
//! part of either (`key = value` and `key=value` mean the same), a line whose first
//! non-whitespace character is `#` is a comment — so a commented-out `# key=value` is
//! never an entry — and blank lines carry nothing. There is **no inline comment**:
//! everything after the first `=` is the value, `#` included. notifd in particular
//! rejects such a value, so a writer must never produce one (see
//! [`KvFile::set_value`]).
//!
//! # Why a lossless line model (architecture §3)
//!
//! Both files are hand-maintained: they carry explanatory comments, blank-line
//! grouping and a deliberate order. The app's rule for every config parser is to
//! never regenerate a file from a model. [`KvFile`] therefore keeps every line's
//! exact bytes, classifies each, and records the byte span of each entry's value, so
//!
//! - **round-trip identity** holds: [`KvFile::parse`] then [`KvFile::emit`] with no
//!   edit reproduces the input byte-for-byte; and
//! - **single-span edits** hold: [`KvFile::set_value`] replaces only the value bytes
//!   of one line, keeping the key, the spacing around `=`, trailing whitespace and the
//!   line terminator, and [`KvFile::append_entry`] only adds bytes at the end.
//!
//! # What this module does not decide
//!
//! Which keys exist, what a valid value looks like, and which of several entries for
//! the same key counts (the palette treats a duplicate as an error and reads the
//! first; notifd lets the last valid one win) are the consumers' rules. This module
//! only offers both readings ([`Occurrence`]) and reports every entry in file order.

use std::fmt;

/// A parsed `key=value` file that re-emits itself byte-for-byte and edits values in
/// place (see the module docs).
#[derive(Clone, Debug)]
pub struct KvFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the input exactly (round-trip identity).
    lines: Vec<Line>,
}

/// One physical line, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact bytes of the line **including its terminator** (`\n` or `\r\n`, or
    /// none for a final line without one). Only [`KvFile::set_value`] rewrites it, and
    /// only within the value span.
    raw: String,
    /// How the line was classified.
    kind: LineKind,
}

/// The classification of one stored line.
#[derive(Clone, Debug)]
enum LineKind {
    /// Empty or whitespace only.
    Blank,
    /// The first non-whitespace character is `#`.
    Comment,
    /// A `key=value` assignment with an identifier key (see [`is_key_char`]).
    Entry {
        /// The key with surrounding whitespace trimmed.
        key: String,
        /// Byte offset within [`Line::raw`] where the value begins (after the `=`
        /// and any whitespace following it).
        value_start: usize,
        /// Byte offset within [`Line::raw`] where the value ends (before trailing
        /// whitespace and the terminator). `value_start..value_end` is exactly what
        /// an edit replaces.
        value_end: usize,
    },
    /// Neither blank, a comment, nor an assignment with an identifier key (no `=`,
    /// or an empty or non-identifier key). Kept verbatim and never editable.
    Malformed,
}

/// A borrowed view of one line, as [`KvFile::lines`] yields it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvLine<'a> {
    /// An empty or whitespace-only line.
    Blank,
    /// A comment line — including a commented-out assignment.
    Comment,
    /// An assignment.
    Entry {
        /// The key, whitespace-trimmed.
        key: &'a str,
        /// The value, whitespace-trimmed; everything after the first `=`.
        value: &'a str,
    },
    /// A line that is none of the above. Consumers typically report it as a parse
    /// warning; it is preserved and never edited.
    Malformed,
}

/// One assignment, as [`KvFile::entries`] yields it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvEntry<'a> {
    /// The 1-based line number, for diagnostics that must not quote file contents.
    pub line: usize,
    /// The key, whitespace-trimmed.
    pub key: &'a str,
    /// The value, whitespace-trimmed.
    pub value: &'a str,
}

/// Which entry a read or an edit addresses when a key is assigned on several lines.
///
/// A well-formed file assigns each key once, and then both choices name the same
/// line. They differ only for a duplicated key, where the right answer depends on the
/// consumer: the palette reads the first (and reports the duplicate as a schema
/// error), notifd's reader applies every line in order, so the last valid one wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occurrence {
    /// The first entry for the key, in file order.
    First,
    /// The last entry for the key, in file order.
    Last,
}

/// Why [`KvFile::set_value`] or [`KvFile::append_entry`] refused an edit.
///
/// Every variant leaves the file unchanged: the checks run before any byte is
/// spliced, so a refused edit can never partially rewrite a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvEditError {
    /// No entry has this key. A key that appears only in a comment or a malformed line
    /// is not an entry, so an edit can never land on a commented-out line.
    UnknownKey(String),
    /// The key to append is empty or contains a character outside [`is_key_char`], so
    /// the appended line would not read back as an entry for that key.
    InvalidKey(String),
    /// The value contains a line break (which would split the line and inject a
    /// second one) or starts or ends with whitespace (which a reader trims, so the
    /// value would not read back as written).
    UnsafeValue(String),
}

impl fmt::Display for KvEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KvEditError::UnknownKey(key) => write!(f, "no `{key}=` entry to edit"),
            KvEditError::InvalidKey(key) => {
                write!(
                    f,
                    "`{key}` is not a valid key (letters, digits and `_` only)"
                )
            }
            KvEditError::UnsafeValue(value) => write!(
                f,
                "`{}` cannot be written as a value: it must not contain a line break or \
                 start or end with whitespace",
                value.escape_debug()
            ),
        }
    }
}

impl std::error::Error for KvEditError {}

impl KvFile {
    /// Parses `input` into the lossless line model. Never fails: every line is kept,
    /// and lines that are not assignments are classified rather than rejected (see
    /// [`KvLine`]), so [`emit`](Self::emit) always reproduces the input.
    ///
    /// Lines are split on `\n`; a `\r` right before it is treated as part of the
    /// terminator, so CRLF files read and round-trip correctly.
    pub fn parse(input: &str) -> KvFile {
        // `split_inclusive` keeps each terminator attached and yields a final
        // unterminated line as-is, so re-joining the pieces is exactly the input; an
        // empty input yields no pieces and round-trips to empty.
        let lines = input
            .split_inclusive('\n')
            .map(|raw| Line {
                raw: raw.to_string(),
                kind: classify(raw),
            })
            .collect();
        KvFile { lines }
    }

    /// The file as text — byte-for-byte the parsed input when nothing was edited.
    pub fn emit(&self) -> String {
        self.lines.iter().map(|line| line.raw.as_str()).collect()
    }

    /// Every line with its 1-based number, in file order.
    pub fn lines(&self) -> impl Iterator<Item = (usize, KvLine<'_>)> {
        self.lines
            .iter()
            .enumerate()
            .map(|(index, line)| (index + 1, line.view()))
    }

    /// Every assignment in file order, duplicates included.
    pub fn entries(&self) -> impl Iterator<Item = KvEntry<'_>> {
        self.lines().filter_map(|(line, view)| match view {
            KvLine::Entry { key, value } => Some(KvEntry { line, key, value }),
            _ => None,
        })
    }

    /// The value of `key`'s first or last entry (see [`Occurrence`]), or `None` when
    /// no entry has that key. Comments and malformed lines are never considered.
    pub fn value(&self, key: &str, occurrence: Occurrence) -> Option<&str> {
        let mut matching = self.entries().filter(|entry| entry.key == key);
        let entry = match occurrence {
            Occurrence::First => matching.next(),
            Occurrence::Last => matching.last(),
        };
        entry.map(|entry| entry.value)
    }

    /// Replaces the value of `key`'s first or last entry with `value`, changing
    /// exactly that value span.
    ///
    /// The key, any indentation, the spacing around `=`, trailing whitespace, the line
    /// terminator and every other line stay byte-identical. Refused, with the file
    /// unchanged, when no entry has the key ([`KvEditError::UnknownKey`]) or when
    /// `value` is not safe to write ([`KvEditError::UnsafeValue`]). Whether the value
    /// is *meaningful* for the key is the caller's check; this one only guarantees the
    /// file keeps its shape.
    pub fn set_value(
        &mut self,
        key: &str,
        occurrence: Occurrence,
        value: &str,
    ) -> Result<(), KvEditError> {
        check_value(value)?;
        let mut matching = self
            .lines
            .iter_mut()
            .filter(|line| matches!(&line.kind, LineKind::Entry { key: k, .. } if k == key));
        let line = match occurrence {
            Occurrence::First => matching.next(),
            Occurrence::Last => matching.last(),
        }
        .ok_or_else(|| KvEditError::UnknownKey(key.to_string()))?;

        // Destructured into its two fields so the raw text can be spliced while the
        // span stored in the kind is read and updated.
        let Line { raw, kind } = line;
        // Always an entry: the filter above kept only `LineKind::Entry` lines, so the
        // implicit non-entry case of this `if let` is unreachable by construction.
        if let LineKind::Entry {
            value_start,
            value_end,
            ..
        } = kind
        {
            raw.replace_range(*value_start..*value_end, value);
            *value_end = *value_start + value.len();
        }
        Ok(())
    }

    /// Appends a new `key=value` line at the end of the file.
    ///
    /// The line is written in the bare `key=value` form with the file's own line
    /// terminator (`\r\n` when the last terminated line uses it, else `\n`). A final
    /// line without a terminator first gets one, so the new entry never joins it.
    /// Refused, with the file unchanged, for a key that would not read back as this
    /// entry ([`KvEditError::InvalidKey`]) or an unsafe value
    /// ([`KvEditError::UnsafeValue`]). Appending a key that already has an entry is
    /// allowed (it creates a duplicate); callers that want one entry per key edit the
    /// existing one with [`set_value`](Self::set_value) instead.
    pub fn append_entry(&mut self, key: &str, value: &str) -> Result<(), KvEditError> {
        if key.is_empty() || !key.chars().all(is_key_char) {
            return Err(KvEditError::InvalidKey(key.to_string()));
        }
        check_value(value)?;

        let terminator = self
            .lines
            .iter()
            .rev()
            .find(|line| line.raw.ends_with('\n'))
            .map_or("\n", |line| {
                if line.raw.ends_with("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                }
            });
        if let Some(last) = self.lines.last_mut() {
            if !last.raw.ends_with('\n') {
                last.raw.push_str(terminator);
            }
        }
        let raw = format!("{key}={value}{terminator}");
        let kind = classify(&raw);
        self.lines.push(Line { raw, kind });
        Ok(())
    }
}

impl Line {
    /// The borrowed view of this line.
    fn view(&self) -> KvLine<'_> {
        match &self.kind {
            LineKind::Blank => KvLine::Blank,
            LineKind::Comment => KvLine::Comment,
            LineKind::Malformed => KvLine::Malformed,
            LineKind::Entry {
                key,
                value_start,
                value_end,
            } => KvLine::Entry {
                key,
                value: &self.raw[*value_start..*value_end],
            },
        }
    }
}

/// Classifies one raw line (terminator included) and, for an assignment, records
/// the value's byte span within `raw`.
fn classify(raw: &str) -> LineKind {
    // Work on the content without its terminator, so the terminator never ends up
    // inside a value span. `content` is a prefix of `raw`, so offsets into it are
    // offsets into `raw` too.
    let content = strip_terminator(raw);

    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        return LineKind::Blank;
    }
    // Checked before the `=` so a commented-out assignment is never an entry.
    if trimmed.starts_with('#') {
        return LineKind::Comment;
    }

    let Some(eq) = content.find('=') else {
        return LineKind::Malformed;
    };
    let key = content[..eq].trim();
    if key.is_empty() || !key.chars().all(is_key_char) {
        return LineKind::Malformed;
    }

    let after_eq = &content[eq + 1..];
    let leading_whitespace = after_eq.len() - after_eq.trim_start().len();
    let value = after_eq.trim();
    let value_start = eq + 1 + leading_whitespace;
    LineKind::Entry {
        key: key.to_string(),
        value_start,
        value_end: value_start + value.len(),
    }
}

/// `raw` without a trailing `\n` or `\r\n`.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

/// Refuses a value that would not survive being written and read back as written.
fn check_value(value: &str) -> Result<(), KvEditError> {
    if value.contains(['\n', '\r']) || value.trim() != value {
        return Err(KvEditError::UnsafeValue(value.to_string()));
    }
    Ok(())
}

/// Whether `c` may appear in a key: ASCII letters, digits and `_`.
///
/// Both consumers' keys are plain identifiers (`bg0`, `accent0`, `timeout_low_ms`),
/// so a line whose key has any other character is malformed rather than an entry.
pub fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small file with every line kind: a comment, a blank line, a spaced entry
    /// with trailing whitespace, a commented-out assignment, a malformed line, a
    /// duplicated key, and a final line without a terminator.
    const MIXED: &str = "\
# header comment

  alpha = one two\x20\x20\x20
# beta=commented
not an assignment
beta=first
gamma=3
beta=second";

    #[test]
    fn parse_then_emit_reproduces_the_input_byte_for_byte() {
        for input in [
            MIXED,
            "",
            "\n\n",
            "key=value",
            "key=value\r\nother=x\r\n",
            "# only a comment\n",
            "=no key\n",
        ] {
            assert_eq!(KvFile::parse(input).emit(), input, "{input:?}");
        }
    }

    #[test]
    fn lines_are_classified_and_numbered() {
        let file = KvFile::parse(MIXED);
        let lines: Vec<(usize, KvLine<'_>)> = file.lines().collect();
        assert_eq!(
            lines,
            vec![
                (1, KvLine::Comment),
                (2, KvLine::Blank),
                (
                    3,
                    KvLine::Entry {
                        key: "alpha",
                        value: "one two"
                    }
                ),
                (4, KvLine::Comment),
                (5, KvLine::Malformed),
                (
                    6,
                    KvLine::Entry {
                        key: "beta",
                        value: "first"
                    }
                ),
                (
                    7,
                    KvLine::Entry {
                        key: "gamma",
                        value: "3"
                    }
                ),
                (
                    8,
                    KvLine::Entry {
                        key: "beta",
                        value: "second"
                    }
                ),
            ]
        );
    }

    #[test]
    fn everything_after_the_first_equals_sign_is_the_value() {
        // No inline comments: a `#` after the value belongs to it, and a second `=`
        // does too.
        let file = KvFile::parse("a=5000 # five seconds\nb=x=y\n");
        assert_eq!(
            file.value("a", Occurrence::First),
            Some("5000 # five seconds")
        );
        assert_eq!(file.value("b", Occurrence::First), Some("x=y"));
    }

    #[test]
    fn a_duplicated_key_is_read_first_or_last_on_request() {
        let file = KvFile::parse(MIXED);
        assert_eq!(file.value("beta", Occurrence::First), Some("first"));
        assert_eq!(file.value("beta", Occurrence::Last), Some("second"));
        assert_eq!(file.value("gamma", Occurrence::Last), Some("3"));
        assert_eq!(file.value("missing", Occurrence::Last), None);
        // The commented-out `beta=commented` is not an entry.
        assert_eq!(
            file.entries()
                .filter(|entry| entry.key == "beta")
                .map(|entry| entry.line)
                .collect::<Vec<_>>(),
            vec![6, 8]
        );
    }

    #[test]
    fn set_value_rewrites_only_the_value_span_and_keeps_the_spacing() {
        let mut file = KvFile::parse(MIXED);
        file.set_value("alpha", Occurrence::First, "three")
            .expect("alpha is an entry");
        assert_eq!(
            file.emit(),
            MIXED.replace("  alpha = one two   \n", "  alpha = three   \n")
        );
    }

    #[test]
    fn set_value_addresses_the_requested_occurrence() {
        let mut last = KvFile::parse(MIXED);
        last.set_value("beta", Occurrence::Last, "changed")
            .expect("beta is an entry");
        assert_eq!(last.emit(), MIXED.replace("beta=second", "beta=changed"));

        let mut first = KvFile::parse(MIXED);
        first
            .set_value("beta", Occurrence::First, "changed")
            .expect("beta is an entry");
        assert_eq!(first.emit(), MIXED.replace("beta=first", "beta=changed"));
    }

    #[test]
    fn set_value_keeps_a_crlf_terminator() {
        let mut file = KvFile::parse("a = 1\r\nb=2\r\n");
        file.set_value("a", Occurrence::Last, "10")
            .expect("a exists");
        assert_eq!(file.emit(), "a = 10\r\nb=2\r\n");
    }

    #[test]
    fn refused_edits_leave_the_file_unchanged() {
        let mut file = KvFile::parse(MIXED);
        assert_eq!(
            file.set_value("missing", Occurrence::Last, "x"),
            Err(KvEditError::UnknownKey("missing".to_string()))
        );
        // A key that only appears commented out is not an entry.
        assert_eq!(
            KvFile::parse("# key=1\n").set_value("key", Occurrence::First, "2"),
            Err(KvEditError::UnknownKey("key".to_string()))
        );
        for unsafe_value in ["two\nlines", "carriage\r", " leading", "trailing "] {
            assert_eq!(
                file.set_value("gamma", Occurrence::Last, unsafe_value),
                Err(KvEditError::UnsafeValue(unsafe_value.to_string()))
            );
            assert_eq!(
                file.append_entry("delta", unsafe_value),
                Err(KvEditError::UnsafeValue(unsafe_value.to_string()))
            );
        }
        for bad_key in ["", "has space", "dash-key", "a=b"] {
            assert_eq!(
                file.append_entry(bad_key, "1"),
                Err(KvEditError::InvalidKey(bad_key.to_string()))
            );
        }
        assert_eq!(file.emit(), MIXED);
    }

    #[test]
    fn append_entry_adds_a_bare_line_at_the_end() {
        let mut file = KvFile::parse("# settings\na=1\n");
        file.append_entry("b", "2").expect("a valid entry");
        assert_eq!(file.emit(), "# settings\na=1\nb=2\n");
        assert_eq!(file.value("b", Occurrence::Last), Some("2"));
    }

    #[test]
    fn append_entry_terminates_an_unterminated_last_line_first() {
        let mut file = KvFile::parse("a=1");
        file.append_entry("b", "2").expect("a valid entry");
        assert_eq!(file.emit(), "a=1\nb=2\n");
    }

    #[test]
    fn append_entry_follows_the_files_crlf_terminator() {
        let mut file = KvFile::parse("a=1\r\n# tail");
        file.append_entry("b", "2").expect("a valid entry");
        assert_eq!(file.emit(), "a=1\r\n# tail\r\nb=2\r\n");
    }

    #[test]
    fn append_entry_to_an_empty_file_writes_one_line() {
        let mut file = KvFile::parse("");
        file.append_entry("a", "1").expect("a valid entry");
        assert_eq!(file.emit(), "a=1\n");
    }
}
