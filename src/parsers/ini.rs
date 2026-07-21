//! Surgical, lossless editor for GTK `settings.ini` files (task 3.5;
//! architecture §3; R5.3 item 1, R6.1).
//!
//! # What this file is
//!
//! GTK reads its default theme, icon, cursor, and font settings from a GLib key
//! file (INI-shaped) at `~/.config/gtk-3.0/settings.ini` and
//! `~/.config/gtk-4.0/settings.ini`. Both files carry a single `[Settings]`
//! group whose keys the app's Theme page edits, for example (analysis §6.5):
//!
//! ```text
//! # written by the dotfiles; see the XSETTINGS/dconf caveat below
//! [Settings]
//! gtk-theme-name=Everforest-Green-Dark
//! gtk-icon-theme-name=Everforest-Dark
//! gtk-cursor-theme-name=Nordic-cursors
//! gtk-cursor-theme-size=16
//! gtk-font-name=Noto Sans  10
//! gtk-application-prefer-dark-theme=1
//! ```
//!
//! In the dotfiles this app manages, both files are tracked in-repo and
//! symlinked into place by `setup.sh` (analysis §6.5), so on the target machine
//! the **common path is editing an existing `[Settings]` section**. Creating the
//! file and the section from scratch is a genuine but secondary fallback (a user
//! without this dotfiles deployment, or a stripped-down `~/.config`).
//!
//! # Why a surgical, lossless editor (not a serializer)
//!
//! These files are hand- and script-maintained: they carry a comment (the real
//! files warn that an XSETTINGS/dconf session takes the cursor from `gsettings`
//! instead, reinforcing why R3.4 also sets `gsettings`, not just this file),
//! blank-line grouping, and a deliberate key ordering. The architecture's hard
//! rule for every config parser (architecture §3) is to **never regenerate a
//! file from a model**; instead we keep a lossless line representation and, when
//! asked to change a value, rewrite *only* that value's byte span and re-emit
//! every other byte identically. Two guarantees follow, both covered by tests:
//!
//! - **Round-trip identity**: [`IniFile::parse`] then [`IniFile::emit`] with no
//!   edit reproduces the input byte-for-byte, comments and ordering included.
//! - **Single-span edits**: [`IniFile::set_value`] on an existing key touches
//!   exactly that value's bytes and leaves indentation, the spacing around `=`,
//!   trailing whitespace, comments, and all other lines untouched.
//!
//! # Writing both GTK files identically
//!
//! GTK 3 and GTK 4 read separate files that must carry the *same* cursor/theme
//! values (analysis §6.5, R3.4). This module is deliberately per-file: the Theme
//! page (task 6.4) parses each file into its own [`IniFile`], applies the
//! identical [`IniFile::set_value`] call to each, and emits both. There is no
//! combined "write both" method here — that orchestration belongs to the page /
//! Apply pipeline, which addresses each file by its own runtime path.
//!
//! # Physical file creation is out of scope here
//!
//! The [create-from-scratch path](IniFile::set_value) produces the *bytes* of a
//! new file (a `[Settings]` section with the set keys), but this module does not
//! touch the filesystem — and no caller creates a `settings.ini` on disk today.
//! The atomic writer (task 2.2, `system::writer`) `fs::canonicalize`s its target,
//! so it only rewrites files that already exist, and the Theme model reads
//! whichever of the two GTK files is present rather than creating an absent one.
//! On the target dotfiles machine both files exist, so the edit-existing path is
//! what actually runs; the create path stays implemented and tested so the
//! parser's parse/edit/emit surface is complete for a host missing one of them.

use std::fmt;

/// A parsed GTK `settings.ini` that can re-emit itself byte-for-byte and edit
/// individual values in place.
///
/// Built by [`IniFile::parse`] (or [`IniFile::empty`] for an absent file).
/// Internally it is just the file's lines in order, each classified and — for a
/// `key=value` entry — annotated with the byte span of its value within that
/// line's raw text. Emitting concatenates the raw line texts, so an unedited
/// file reproduces its input exactly; editing a value splices new bytes into a
/// single line's value span and leaves every other line alone.
#[derive(Clone, Debug)]
pub struct IniFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity).
    lines: Vec<Line>,
}

/// One physical line of the file, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of this line **including its terminator**
    /// (`\n` or `\r\n`, or none for a final line with no trailing newline).
    /// This is what [`IniFile::emit`] writes back; it is only ever mutated by an
    /// edit, which splices a value span, or grown by an append/create, which
    /// pushes a wholly new line.
    raw: String,
    /// How this line was classified during parsing.
    kind: LineKind,
}

/// The classification of a single line.
///
/// Only [`LineKind::Section`] and [`LineKind::Entry`] lines take part in
/// addressing; blanks, comments, and malformed lines are preserved verbatim and
/// never matched by an edit (so a commented-out `#gtk-theme-name=…` line can
/// never be mistaken for the real entry).
#[derive(Clone, Debug, PartialEq, Eq)]
enum LineKind {
    /// A line that is empty or only whitespace.
    Blank,
    /// A comment: the first non-whitespace character is `#`. This is GLib's key
    /// file comment marker; a commented-out entry (`# gtk-theme-name=…`) is a
    /// comment, never an entry. (GLib does not treat `;` as a comment, so a
    /// `;`-prefixed line is [`LineKind::Malformed`], not a comment.)
    Comment,
    /// A group header line of the form `[name]`. The bracketed name (trimmed) is
    /// recorded so a caller can address keys within the group.
    Section {
        /// The group name between the brackets, with surrounding whitespace
        /// trimmed (e.g. `Settings`). Used to match a [`IniFile::set_value`]
        /// target section.
        name: String,
    },
    /// A `key=value` assignment with a non-empty key. The value's byte range
    /// within [`Line::raw`] is recorded so it can be read and rewritten in place.
    /// GLib key files do not treat `#` as an inline comment inside a value, so
    /// the value span runs to the end of the line's content (only surrounding
    /// whitespace is excluded).
    Entry {
        /// The assignment's key, with surrounding whitespace trimmed
        /// (e.g. `gtk-theme-name`). Used to match [`IniFile::set_value`] targets.
        key: String,
        /// Byte offset within [`Line::raw`] where the value begins (after the
        /// `=` and any following whitespace).
        value_start: usize,
        /// Byte offset within [`Line::raw`] where the value ends (before any
        /// trailing whitespace and the line terminator). The half-open range
        /// `value_start..value_end` is exactly the bytes an edit replaces.
        value_end: usize,
    },
    /// A non-blank, non-comment line that is neither a well-formed `[section]`
    /// header nor a `key=value` assignment (a bracket line with no closing `]`,
    /// or a line with no `=` and no key). Preserved verbatim and surfaced as a
    /// [`ParseWarningKind::MalformedLine`] warning; never editable.
    Malformed,
}

/// A non-fatal problem noticed while parsing a `settings.ini`.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (task 3.5
/// acceptance: malformed lines are preserved losslessly and surfaced as
/// warnings, not panics). [`IniFile::parse`] returns the collected warnings *and*
/// logs each at `warn`, so the caller can both react programmatically and see
/// them in the journal.
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
                "line {}: not a comment, blank line, `[section]` header, or key=value entry",
                self.line
            ),
        }
    }
}

/// The specific reason a line produced a [`ParseWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseWarningKind {
    /// The line is not blank, not a comment, not a well-formed `[section]`
    /// header, and not a `key=value` assignment with a non-empty key — so it
    /// cannot be interpreted. It is kept byte-for-byte and ignored by edits.
    MalformedLine,
}

/// Which action [`IniFile::set_value`] took, for logging and for tests that need
/// to distinguish an in-place edit from an append or a section creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// The key already existed in the target section; its value span was
    /// rewritten in place.
    Edited,
    /// The section existed but the key was absent; a new `key=value` line was
    /// appended at the end of that section (after its last non-blank line).
    AppendedKey,
    /// The section did not exist; a new `[section]` header and the `key=value`
    /// line were appended at end-of-file. This is the create-from-scratch path
    /// when starting from [`IniFile::empty`] or a file with no such section.
    CreatedSection,
}

/// A failure from [`IniFile::set_value`].
///
/// Every variant leaves the file completely unchanged: the check happens before
/// any byte is written, so a rejected edit can never partially rewrite the file.
// `clippy::enum_variant_names` flags the shared `Invalid` prefix across these
// variants. The `InvalidValue` name matches the sibling parsers (`palette`,
// `monitors`); the sibling `EditError`s do not otherwise share a prefix, so this
// enum is simply the first to carry all three `Invalid*` names and thus the first
// to trip the lint. The shared prefix is intentional — each variant names a
// distinct rejected input (value / key / section).
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The requested value contains a newline or carriage return, which would
    /// split the single `key=value` line into two and corrupt the file. Rejecting
    /// it upholds R8.3 — the app never writes a value that breaks a working
    /// config. (Any other character, including `#`, is fine: GLib key files do
    /// not treat `#` as an inline comment within a value.)
    InvalidValue(String),
    /// A key used to append a new entry is not usable: it is empty or contains a
    /// newline, carriage return, or `=` (which would be read as the assignment
    /// separator). Only checked when appending a new key; editing an existing
    /// key never rewrites the key itself.
    InvalidKey(String),
    /// A section name used to create a new section is not usable: it is empty or
    /// contains a newline, carriage return, `[`, or `]` (which would break the
    /// `[name]` header). Only checked when creating a section.
    InvalidSection(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::InvalidValue(value) => write!(
                f,
                "`{value}` contains a newline or carriage return, which would split the line"
            ),
            EditError::InvalidKey(key) => write!(
                f,
                "`{key}` is not a usable settings key (empty, or contains `=`, a newline, or a \
                 carriage return)"
            ),
            EditError::InvalidSection(section) => write!(
                f,
                "`{section}` is not a usable section name (empty, or contains `[`, `]`, a \
                 newline, or a carriage return)"
            ),
        }
    }
}

impl std::error::Error for EditError {}

impl IniFile {
    /// Parses `settings.ini` text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved, whether or not it is a valid
    /// header or entry, so [`emit`](Self::emit) always reproduces the input
    /// byte-for-byte. Lines that cannot be interpreted (and are not comments or
    /// blanks) are returned as [`ParseWarning`]s and additionally logged at
    /// `warn`; they are *not* errors and do not stop parsing (task 3.5
    /// acceptance).
    ///
    /// The returned warnings carry only line numbers — never the file's
    /// contents, which are not logged at any level (R7.3).
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

        for warning in &warnings {
            // Surface each problem in the journal without dumping file contents
            // (R7.3): the message carries only the line number.
            tracing::warn!(warning = %warning, "settings.ini parse warning");
        }

        (IniFile { lines }, warnings)
    }

    /// An empty file, for the create-from-scratch path when the target
    /// `settings.ini` is absent (a user without the dotfiles deployment).
    ///
    /// The first [`set_value`](Self::set_value) call then creates the `[Settings]`
    /// section and its first key. Emitting an untouched empty file yields the
    /// empty string, so writing one out unchanged is a no-op.
    pub fn empty() -> Self {
        IniFile { lines: Vec::new() }
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After [`set_value`](Self::set_value) edits, the output is identical to the
    /// input except within edited value spans and any lines appended by a new
    /// key or a created section.
    pub fn emit(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.raw);
        }
        out
    }

    /// Returns the current value of `key` within `section`, if present.
    ///
    /// When a key is duplicated within a group, GLib resolves it to the **last**
    /// value, so this reads the *last* matching entry inside the first matching
    /// section — the value GTK actually uses. Comment, blank, and malformed
    /// lines — and entries in other sections — are never considered.
    pub fn value(&self, section: &str, key: &str) -> Option<&str> {
        let (_, body_start, body_end) = self.section_range(section)?;
        // Iterate in reverse so the first hit is the last (GLib-effective)
        // occurrence within the section body.
        for line in self.lines[body_start..body_end].iter().rev() {
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

    /// Sets `key` to `value` within `section`, creating whatever is missing.
    ///
    /// Three cases, reported by the returned [`SetOutcome`]:
    ///
    /// - **Edit** ([`SetOutcome::Edited`]): the key already exists in the section
    ///   — only its value's byte span is rewritten. The key, the spacing around
    ///   `=`, any indentation or trailing whitespace, the terminator, comments,
    ///   and every other line stay byte-identical. This is the common path on the
    ///   dotfiles machine (analysis §6.5).
    /// - **Append** ([`SetOutcome::AppendedKey`]): the section exists but lacks
    ///   the key — a new `key=value` line is inserted at the section's end (after
    ///   its last non-blank line, before any following section), copying the
    ///   indentation and `=`-separator style of a sibling entry in the same
    ///   section when one exists.
    /// - **Create** ([`SetOutcome::CreatedSection`]): the section does not exist
    ///   — a new `[section]` header and the `key=value` line are appended at
    ///   end-of-file (the create-from-scratch path from [`empty`](Self::empty)).
    ///
    /// `value` must not contain a newline or carriage return
    /// ([`EditError::InvalidValue`]); when appending, `key` must be a usable key
    /// ([`EditError::InvalidKey`]); when creating, `section` must be a usable name
    /// ([`EditError::InvalidSection`]). Any rejected edit leaves the file
    /// completely unchanged (R8.3). If a key appears more than once in a section
    /// only the **last** occurrence — the one GLib treats as effective — is
    /// edited; earlier shadowed duplicates stay byte-identical.
    pub fn set_value(
        &mut self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<SetOutcome, EditError> {
        reject_unsafe_value(value)?;

        match self.section_range(section) {
            Some((_, body_start, body_end)) => {
                // Edit-existing: rewrite only the LAST matching key's value span.
                // GLib resolves a duplicated key within a group to its last value,
                // so editing an earlier (shadowed) copy would silently not take
                // effect; `rposition` targets the effective occurrence.
                let target = self.lines[body_start..body_end]
                    .iter()
                    .rposition(|line| {
                        matches!(&line.kind, LineKind::Entry { key: k, .. } if k.as_str() == key)
                    })
                    .map(|relative| body_start + relative);
                if let Some(index) = target {
                    let Line { raw, kind } = &mut self.lines[index];
                    if let LineKind::Entry {
                        value_start,
                        value_end,
                        ..
                    } = kind
                    {
                        raw.replace_range(*value_start..*value_end, value);
                        *value_end = *value_start + value.len();
                        tracing::debug!(section, key, value, "rewrote settings.ini value");
                        return Ok(SetOutcome::Edited);
                    }
                }

                // Key absent within an existing section: append at section end.
                reject_unsafe_key(key)?;
                self.append_entry(body_start, body_end, key, value);
                tracing::debug!(section, key, value, "appended settings.ini key to section");
                Ok(SetOutcome::AppendedKey)
            }
            None => {
                // Section absent: create it (and the key) at end-of-file.
                reject_unsafe_key(key)?;
                reject_unsafe_section(section)?;
                self.create_section(section, key, value);
                tracing::debug!(section, key, value, "created settings.ini section");
                Ok(SetOutcome::CreatedSection)
            }
        }
    }

    /// Locates a section's line range: `(header_index, body_start, body_end)`,
    /// where `body_start..body_end` is the half-open range of the lines *between*
    /// this header and the next section header (or end-of-file).
    ///
    /// Matches the first header whose trimmed name equals `section`. Blank lines
    /// do not end a section, so a section's body may contain them — they are part
    /// of the range and simply skipped when reading or appending.
    fn section_range(&self, section: &str) -> Option<(usize, usize, usize)> {
        let header = self
            .lines
            .iter()
            .position(|line| matches!(&line.kind, LineKind::Section { name } if name == section))?;
        let body_start = header + 1;
        let body_end = self.lines[body_start..]
            .iter()
            .position(|line| matches!(line.kind, LineKind::Section { .. }))
            .map(|relative| body_start + relative)
            .unwrap_or(self.lines.len());
        Some((header, body_start, body_end))
    }

    /// Appends a `key=value` line at the end of an existing section's body.
    ///
    /// The line is inserted right after the section's last non-blank line (so it
    /// lands with the section's content, not after a trailing blank line that
    /// separates it from a following section). If the body is empty it goes
    /// directly after the header.
    fn append_entry(&mut self, body_start: usize, body_end: usize, key: &str, value: &str) {
        let terminator = self.line_terminator().to_string();
        let content = self.entry_line_content(body_start, body_end, key, value);
        let raw = format!("{content}{terminator}");

        // Insert after the last non-blank line of the body; fall back to right
        // after the header (`body_start`) when the body has no content lines.
        let insert_at = self.lines[body_start..body_end]
            .iter()
            .rposition(|line| !matches!(line.kind, LineKind::Blank))
            .map(|relative| body_start + relative + 1)
            .unwrap_or(body_start);

        // Ensure the line we insert after ends with a terminator, so the new line
        // does not run onto it (only an unterminated final line can lack one).
        if let Some(prev) = insert_at.checked_sub(1).and_then(|i| self.lines.get_mut(i)) {
            if !prev.raw.ends_with('\n') {
                prev.raw.push_str(&terminator);
            }
        }

        let mut discard = Vec::new();
        let kind = classify_line(&raw, 0, &mut discard);
        self.lines.insert(insert_at, Line { raw, kind });
    }

    /// Builds the `key=value` text (no terminator) for a newly appended entry,
    /// copying the indentation and `=`-separator style of a sibling entry in the
    /// same section body when one exists.
    ///
    /// GTK's own files use the bare `key=value` form with no spaces, which is the
    /// default when the section has no sibling entry to copy from.
    fn entry_line_content(
        &self,
        body_start: usize,
        body_end: usize,
        key: &str,
        value: &str,
    ) -> String {
        for line in &self.lines[body_start..body_end] {
            if let LineKind::Entry {
                key: sibling_key,
                value_start,
                ..
            } = &line.kind
            {
                // Reconstruct the sibling's prefix as `<indent><key><gap>`, where
                // `gap` is the exact bytes between the key and the value (the
                // spacing around `=`), then swap in the new key. `value_start`
                // indexes into the raw line; the content (terminator stripped)
                // shares that origin, so the slices line up.
                let sibling_content = strip_terminator(&line.raw);
                let indent_len = sibling_content.len() - sibling_content.trim_start().len();
                let key_end = indent_len + sibling_key.len();
                if key_end <= *value_start && *value_start <= sibling_content.len() {
                    let indent = &sibling_content[..indent_len];
                    let gap = &sibling_content[key_end..*value_start];
                    return format!("{indent}{key}{gap}{value}");
                }
            }
        }
        format!("{key}={value}")
    }

    /// Creates a new `[section]` header and its first `key=value` line at
    /// end-of-file.
    ///
    /// A blank separator line is inserted before the new header when the file
    /// already has content and does not already end with a blank line, so the new
    /// section reads as its own block. Starting from [`empty`](Self::empty) there
    /// is no content, so the result is exactly `[section]\nkey=value\n`.
    fn create_section(&mut self, section: &str, key: &str, value: &str) {
        let terminator = self.line_terminator().to_string();

        // Guarantee the current final line ends with a terminator so the new
        // section starts on its own line.
        if let Some(last) = self.lines.last_mut() {
            if !last.raw.ends_with('\n') {
                last.raw.push_str(&terminator);
            }
        }

        let has_content = self
            .lines
            .iter()
            .any(|line| !matches!(line.kind, LineKind::Blank));
        let ends_blank = self
            .lines
            .last()
            .is_some_and(|line| matches!(line.kind, LineKind::Blank));
        if has_content && !ends_blank {
            self.push_classified(terminator.clone());
        }

        self.push_classified(format!("[{section}]{terminator}"));
        self.push_classified(format!("{key}={value}{terminator}"));
    }

    /// Pushes a new line at end-of-file, classifying it so later addressing (e.g.
    /// a follow-up `set_value` for a second key in a just-created section) sees a
    /// real [`LineKind::Section`] / [`LineKind::Entry`] rather than raw text.
    fn push_classified(&mut self, raw: String) {
        let mut discard = Vec::new();
        let kind = classify_line(&raw, 0, &mut discard);
        self.lines.push(Line { raw, kind });
    }

    /// The line terminator to use for appended lines: `\r\n` if the file uses
    /// Windows endings anywhere, otherwise `\n`. The app's real target is LF, so
    /// this mainly keeps an all-CRLF file internally consistent.
    fn line_terminator(&self) -> &'static str {
        if self.lines.iter().any(|line| line.raw.ends_with("\r\n")) {
            "\r\n"
        } else {
            "\n"
        }
    }
}

/// Classifies one raw line (terminator included) and records a parse warning if
/// it is malformed.
///
/// For an entry it computes the value's byte span as offsets into `raw`, so the
/// span can be stored in [`LineKind::Entry`] and later used to splice a
/// replacement value.
fn classify_line(raw: &str, line_number: usize, warnings: &mut Vec<ParseWarning>) -> LineKind {
    // Work against the content without its line terminator so the terminator is
    // never mistaken for part of a value and stays out of the value span. `raw`
    // (with terminator) is what we re-emit; `content` is only for locating tokens.
    let content = strip_terminator(raw);
    let trimmed = content.trim();

    if trimmed.is_empty() {
        return LineKind::Blank;
    }
    if trimmed.starts_with('#') {
        // A comment — including a commented-out entry like `# gtk-theme-name=…`.
        // The `#` check comes before the `=` check so such a line is never treated
        // as an editable entry.
        return LineKind::Comment;
    }
    if trimmed.starts_with('[') {
        // A group header occupies its own line as `[name]`. A `[` line that does
        // not close with `]` is malformed (not a header, and it has no key to
        // assign), preserved and warned.
        if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            return LineKind::Section {
                name: inner.trim().to_string(),
            };
        }
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::MalformedLine,
        });
        return LineKind::Malformed;
    }

    // Otherwise the line must be a `key=value` assignment.
    let Some(eq) = content.find('=') else {
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::MalformedLine,
        });
        return LineKind::Malformed;
    };

    let key = content[..eq].trim();
    if key.is_empty() {
        // `=value` with no key on the left is not an addressable assignment.
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::MalformedLine,
        });
        return LineKind::Malformed;
    }

    // The value is the content after `=` with surrounding whitespace trimmed. The
    // offsets are relative to `content`, which is a prefix of `raw`, so they index
    // into `raw` directly. Internal whitespace (e.g. `Noto Sans  10`) is part of
    // the value and preserved.
    let after_eq = &content[eq + 1..];
    let leading_ws = after_eq.len() - after_eq.trim_start().len();
    let value = after_eq.trim();
    let value_start = eq + 1 + leading_ws;
    let value_end = value_start + value.len();

    LineKind::Entry {
        key: key.to_string(),
        value_start,
        value_end,
    }
}

/// Returns `content` with a trailing `\n` or `\r\n` removed, so value-span
/// computation never runs into the line terminator.
///
/// The terminator is only stripped for locating tokens; the caller keeps the
/// full `raw` (terminator included) for lossless emission.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

/// Rejects a value that would split the `key=value` line: a newline or carriage
/// return. Any other character (including `#`) is allowed, since GLib key files
/// do not treat `#` as an inline comment inside a value.
fn reject_unsafe_value(value: &str) -> Result<(), EditError> {
    if value.chars().any(|c| matches!(c, '\n' | '\r')) {
        Err(EditError::InvalidValue(value.to_string()))
    } else {
        Ok(())
    }
}

/// Rejects a key that could not be written as the left-hand side of a
/// `key=value` assignment: empty, or containing a newline, carriage return, or
/// `=`.
fn reject_unsafe_key(key: &str) -> Result<(), EditError> {
    if key.is_empty() || key.chars().any(|c| matches!(c, '\n' | '\r' | '=')) {
        Err(EditError::InvalidKey(key.to_string()))
    } else {
        Ok(())
    }
}

/// Rejects a section name that could not be written as a `[name]` header: empty,
/// or containing a newline, carriage return, `[`, or `]`.
fn reject_unsafe_section(section: &str) -> Result<(), EditError> {
    if section.is_empty()
        || section
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | '[' | ']'))
    {
        Err(EditError::InvalidSection(section.to_string()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `gtk-3.0/settings.ini` fixture derived from the real dotfiles
    /// (analysis §6.5): a leading comment (the XSETTINGS/dconf caveat), the
    /// `[Settings]` group, and the theme / icon / cursor / font keys the Theme
    /// page edits, with a trailing newline — the shape of a tracked, symlinked
    /// file on the target machine.
    const SETTINGS_INI: &str = "\
# Managed by the dotfiles. In an XSETTINGS/dconf session GTK takes the cursor
# from gsettings (org.gnome.desktop.interface), which may differ from this file.
[Settings]
gtk-theme-name=Everforest-Green-Dark
gtk-icon-theme-name=Everforest-Dark
gtk-cursor-theme-name=Nordic-cursors
gtk-cursor-theme-size=16
gtk-font-name=Noto Sans  10
gtk-application-prefer-dark-theme=1
";

    /// Splits both texts into lines and returns the indices at which they differ,
    /// asserting first that neither an edit added nor removed a line — used to
    /// prove an edit changed exactly one line.
    fn differing_line_indices(before: &str, after: &str) -> Vec<usize> {
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(
            before_lines.len(),
            after_lines.len(),
            "an in-place edit must not add or remove lines"
        );
        before_lines
            .iter()
            .zip(&after_lines)
            .enumerate()
            .filter_map(|(index, (b, a))| (b != a).then_some(index))
            .collect()
    }

    #[test]
    fn round_trip_identity_on_a_realistic_fixture() {
        // Headline guarantee (R6.1, architecture §3): parse → emit with no edit
        // reproduces the input byte-for-byte, comments/order included.
        let (ini, warnings) = IniFile::parse(SETTINGS_INI);
        assert_eq!(
            ini.emit(),
            SETTINGS_INI,
            "emit must reproduce the input byte-for-byte"
        );
        assert!(
            warnings.is_empty(),
            "a clean, well-formed file must yield no warnings, got {warnings:?}"
        );
        // The internal double space in the font name is part of the value.
        assert_eq!(
            ini.value("Settings", "gtk-font-name"),
            Some("Noto Sans  10")
        );
        assert_eq!(
            ini.value("Settings", "gtk-cursor-theme-name"),
            Some("Nordic-cursors")
        );
        // A key that is not present returns None, not a panic.
        assert_eq!(ini.value("Settings", "gtk-xft-dpi"), None);
        // A section that is not present returns None.
        assert_eq!(ini.value("Other", "gtk-theme-name"), None);
    }

    #[test]
    fn round_trips_edge_cases() {
        // The split/emit contract must survive emptiness, a missing trailing
        // newline, a comment-only file, and CRLF endings.
        for input in [
            "",
            "[Settings]",
            "[Settings]\ngtk-theme-name=Adwaita",
            "# only a comment\n",
            "\n\n",
            "[Settings]\r\ngtk-theme-name=Adwaita\r\n",
        ] {
            let (ini, _) = IniFile::parse(input);
            assert_eq!(ini.emit(), input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn editing_an_existing_key_changes_exactly_that_value_span() {
        // Accept criterion: editing an existing key in `[Settings]` preserves
        // comments and order and changes only that one value span.
        let (mut ini, _) = IniFile::parse(SETTINGS_INI);
        let outcome = ini
            .set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("gtk-theme-name exists in [Settings]");
        assert_eq!(outcome, SetOutcome::Edited);

        let edited = ini.emit();
        let differing = differing_line_indices(SETTINGS_INI, &edited);
        let theme_index = SETTINGS_INI
            .lines()
            .position(|line| line == "gtk-theme-name=Everforest-Green-Dark")
            .expect("fixture contains the theme entry");
        assert_eq!(
            differing,
            vec![theme_index],
            "exactly the gtk-theme-name line may change"
        );
        assert_eq!(
            edited.lines().nth(theme_index),
            Some("gtk-theme-name=Nordic"),
            "only the value span changed: the key, `=`, and layout are preserved"
        );
        assert_eq!(ini.value("Settings", "gtk-theme-name"), Some("Nordic"));
    }

    #[test]
    fn editing_preserves_surrounding_whitespace() {
        // The value span excludes indentation, spaces around `=`, and trailing
        // whitespace, so those survive an edit untouched.
        let input = "[Settings]\n  gtk-cursor-theme-size = 16  \n";
        let (mut ini, warnings) = IniFile::parse(input);
        assert!(warnings.is_empty());
        ini.set_value("Settings", "gtk-cursor-theme-size", "24")
            .expect("valid edit");
        assert_eq!(
            ini.emit(),
            "[Settings]\n  gtk-cursor-theme-size = 24  \n",
            "only the value bytes change; all surrounding whitespace is preserved"
        );
    }

    #[test]
    fn appending_a_new_key_lands_at_the_section_end() {
        // Accept criterion: a key absent from an existing `[Settings]` section is
        // appended at the section end, leaving every existing line untouched.
        let (mut ini, _) = IniFile::parse(SETTINGS_INI);
        let outcome = ini
            .set_value("Settings", "gtk-xft-dpi", "96")
            .expect("append into existing section");
        assert_eq!(outcome, SetOutcome::AppendedKey);

        let edited = ini.emit();
        assert!(
            edited.starts_with(SETTINGS_INI),
            "every original byte is preserved and the new key is added after them"
        );
        assert_eq!(
            edited,
            format!("{SETTINGS_INI}gtk-xft-dpi=96\n"),
            "the new key lands at the end of the section, in GTK's bare key=value style"
        );
        assert_eq!(ini.value("Settings", "gtk-xft-dpi"), Some("96"));
    }

    #[test]
    fn appending_lands_before_a_following_section_and_copies_sibling_style() {
        // With a following section, the new key must land at the end of the target
        // section (before the blank line and the next header), and copy the
        // sibling entry's spaced `=` style rather than forcing GTK's bare form.
        let input = "\
[Settings]
gtk-theme-name = Adwaita

[Other]
foo = bar
";
        let (mut ini, _) = IniFile::parse(input);
        ini.set_value("Settings", "gtk-icon-theme-name", "Papirus")
            .expect("append into the first of two sections");
        assert_eq!(
            ini.emit(),
            "\
[Settings]
gtk-theme-name = Adwaita
gtk-icon-theme-name = Papirus

[Other]
foo = bar
",
            "the new key lands within [Settings], before the blank line and [Other], \
             copying the sibling's ` = ` separator"
        );
        // The other section is untouched.
        assert_eq!(ini.value("Other", "foo"), Some("bar"));
    }

    #[test]
    fn create_from_scratch_from_empty_builds_a_settings_section() {
        // Accept criterion: from empty/absent input, setting a key produces a
        // valid `[Settings]` section containing it.
        let mut ini = IniFile::empty();
        assert_eq!(ini.emit(), "", "an untouched empty file emits nothing");

        let outcome = ini
            .set_value("Settings", "gtk-theme-name", "Everforest-Green-Dark")
            .expect("create from scratch");
        assert_eq!(outcome, SetOutcome::CreatedSection);
        assert_eq!(
            ini.emit(),
            "[Settings]\ngtk-theme-name=Everforest-Green-Dark\n",
            "the create path emits a well-formed [Settings] section"
        );

        // A parsed-empty file behaves identically to IniFile::empty().
        let (mut from_parse, _) = IniFile::parse("");
        from_parse
            .set_value("Settings", "gtk-theme-name", "Everforest-Green-Dark")
            .expect("create from parsed-empty");
        assert_eq!(from_parse.emit(), ini.emit());
    }

    #[test]
    fn create_then_add_a_second_key_finds_the_new_section() {
        // The just-created section must be addressable so a page can write several
        // keys (theme, icon, cursor name, cursor size) into one file. The second
        // call must append into the created section, not create a duplicate one.
        let mut ini = IniFile::empty();
        assert_eq!(
            ini.set_value("Settings", "gtk-theme-name", "Nordic"),
            Ok(SetOutcome::CreatedSection)
        );
        assert_eq!(
            ini.set_value("Settings", "gtk-cursor-theme-name", "Nordic-cursors"),
            Ok(SetOutcome::AppendedKey)
        );
        assert_eq!(
            ini.emit(),
            "[Settings]\ngtk-theme-name=Nordic\ngtk-cursor-theme-name=Nordic-cursors\n",
        );
    }

    #[test]
    fn create_section_in_a_non_empty_file_gets_a_blank_separator() {
        // When the file has content but not the section, the new section is
        // appended after a blank separator so it reads as its own block, and a
        // missing final newline on the last line is repaired first.
        let (mut ini, _) = IniFile::parse("[Other]\nfoo=bar");
        ini.set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("create a second section");
        assert_eq!(
            ini.emit(),
            "[Other]\nfoo=bar\n\n[Settings]\ngtk-theme-name=Nordic\n",
        );
    }

    #[test]
    fn both_gtk_files_receive_identical_writes() {
        // Confirms the per-file API supports task 6.4's need to write the same
        // key/value into gtk-3.0 and gtk-4.0 settings.ini identically: the caller
        // applies the same set_value to each file. Even when the two files differ
        // in layout, the resulting value read back is identical.
        let gtk3 = "[Settings]\ngtk-cursor-theme-name=OldCursors\ngtk-cursor-theme-size=24\n";
        let gtk4 = "# gtk4 has its own header\n[Settings]\ngtk-cursor-theme-name=OldCursors\n";
        let (mut ini3, _) = IniFile::parse(gtk3);
        let (mut ini4, _) = IniFile::parse(gtk4);

        for ini in [&mut ini3, &mut ini4] {
            ini.set_value("Settings", "gtk-cursor-theme-name", "Nordic-cursors")
                .expect("cursor theme is set in both files");
            ini.set_value("Settings", "gtk-cursor-theme-size", "16")
                .expect("cursor size is set/appended in both files");
        }

        assert_eq!(
            ini3.value("Settings", "gtk-cursor-theme-name"),
            ini4.value("Settings", "gtk-cursor-theme-name"),
            "both files carry the same cursor theme after identical writes"
        );
        assert_eq!(
            ini3.value("Settings", "gtk-cursor-theme-size"),
            ini4.value("Settings", "gtk-cursor-theme-size"),
        );
        // gtk-3.0 edited both keys in place; gtk-4.0 edited the theme and
        // appended the size.
        assert_eq!(
            ini3.emit(),
            "[Settings]\ngtk-cursor-theme-name=Nordic-cursors\ngtk-cursor-theme-size=16\n"
        );
        assert_eq!(
            ini4.emit(),
            "# gtk4 has its own header\n[Settings]\ngtk-cursor-theme-name=Nordic-cursors\ngtk-cursor-theme-size=16\n"
        );
    }

    #[test]
    fn a_malformed_line_is_preserved_and_warned_without_panicking() {
        // Accept criterion: a malformed line surfaces as a parse warning, not a
        // panic, and is preserved losslessly. Here an unclosed `[` header and a
        // line with no `=` are both malformed.
        let input = "\
[Settings]
gtk-theme-name=Adwaita
[unterminated section
this line has no equals sign
gtk-icon-theme-name=Papirus
";
        let (mut ini, warnings) = IniFile::parse(input);

        // Lossless: every byte is preserved, malformed lines included.
        assert_eq!(ini.emit(), input);

        // Both malformed lines are surfaced on their (1-based) line numbers.
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].line(), 3);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::MalformedLine);
        assert_eq!(warnings[1].line(), 4);
        assert_eq!(warnings[1].kind(), &ParseWarningKind::MalformedLine);

        // The malformed `[unterminated section` line did not open a section, so
        // the entries stay in `[Settings]` and remain editable without panic.
        assert_eq!(ini.value("Settings", "gtk-theme-name"), Some("Adwaita"));
        assert_eq!(
            ini.value("Settings", "gtk-icon-theme-name"),
            Some("Papirus")
        );
        ini.set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("still editable around malformed lines");
        assert_eq!(
            ini.emit(),
            input.replace("gtk-theme-name=Adwaita", "gtk-theme-name=Nordic")
        );
    }

    #[test]
    fn a_commented_out_entry_is_never_treated_as_editable() {
        // A commented-out key must not be matched by an edit; instead the key is
        // treated as absent and appended, leaving the comment byte-identical.
        let input = "[Settings]\n# gtk-theme-name=Disabled\n";
        let (mut ini, _) = IniFile::parse(input);
        assert_eq!(ini.value("Settings", "gtk-theme-name"), None);
        assert_eq!(
            ini.set_value("Settings", "gtk-theme-name", "Nordic"),
            Ok(SetOutcome::AppendedKey)
        );
        assert_eq!(
            ini.emit(),
            "[Settings]\n# gtk-theme-name=Disabled\ngtk-theme-name=Nordic\n",
            "the commented-out line is preserved and the real key is appended"
        );
    }

    #[test]
    fn set_value_rejects_unsafe_input_without_changing_the_file() {
        let (mut ini, _) = IniFile::parse(SETTINGS_INI);

        // A value with a newline would split the line — rejected (R8.3).
        assert_eq!(
            ini.set_value("Settings", "gtk-theme-name", "Nord\nic"),
            Err(EditError::InvalidValue("Nord\nic".to_string()))
        );
        // Appending with an unusable key is rejected.
        assert_eq!(
            ini.set_value("Settings", "bad=key", "x"),
            Err(EditError::InvalidKey("bad=key".to_string()))
        );
        // Creating an unusable section name is rejected.
        assert_eq!(
            ini.set_value("Bad]Section", "gtk-theme-name", "x"),
            Err(EditError::InvalidSection("Bad]Section".to_string()))
        );

        // None of the rejected edits changed the file.
        assert_eq!(ini.emit(), SETTINGS_INI);
    }

    #[test]
    fn crlf_endings_round_trip_and_an_edit_keeps_them() {
        // A file with Windows `\r\n` endings must round-trip byte-for-byte and an
        // edit must change only the value bytes, never dragging the `\r` into the
        // value span or losing it.
        let input = "[Settings]\r\ngtk-theme-name=Adwaita\r\n";
        let (mut ini, warnings) = IniFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(ini.emit(), input, "CRLF endings must be preserved");
        assert_eq!(ini.value("Settings", "gtk-theme-name"), Some("Adwaita"));

        ini.set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("valid edit");
        assert_eq!(ini.emit(), "[Settings]\r\ngtk-theme-name=Nordic\r\n");

        // An append on a CRLF file uses CRLF for the new line too.
        ini.set_value("Settings", "gtk-cursor-theme-size", "16")
            .expect("append keeps CRLF");
        assert_eq!(
            ini.emit(),
            "[Settings]\r\ngtk-theme-name=Nordic\r\ngtk-cursor-theme-size=16\r\n"
        );
    }

    #[test]
    fn value_and_set_value_target_the_last_occurrence_of_a_duplicate_key() {
        // A key repeated within a section is non-standard, but GLib resolves it to
        // the LAST value. So `value()` must read the last (effective) copy and
        // `set_value` must rewrite the last, leaving the earlier shadowed copy
        // byte-identical — mirroring the later-wins rule in the monitors parser.
        let input = "[Settings]\ngtk-theme-name=First\ngtk-theme-name=Second\n";
        let (mut ini, _) = IniFile::parse(input);

        // Reads the effective (last) value, not the shadowed first.
        assert_eq!(ini.value("Settings", "gtk-theme-name"), Some("Second"));

        ini.set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("last occurrence is editable");
        assert_eq!(
            ini.emit(),
            "[Settings]\ngtk-theme-name=First\ngtk-theme-name=Nordic\n",
            "the last occurrence is rewritten; the shadowed first stays byte-identical"
        );
        assert_eq!(ini.value("Settings", "gtk-theme-name"), Some("Nordic"));
    }

    #[test]
    fn a_semicolon_line_is_not_a_glib_comment() {
        // GLib key files use only `#` for comments (unlike classic INI), so a
        // `;`-prefixed line is never a Comment: with an `=` it parses as an
        // (addressable) entry under its literal key, and without one it is
        // malformed. This locks in the divergence from a generic INI parser.
        let (with_eq, warnings) = IniFile::parse("[Settings]\n;gtk-theme-name=Adwaita\n");
        assert!(
            warnings.is_empty(),
            "`;key=value` is a well-formed entry, not a malformed line"
        );
        // The `;` is part of the key (we parse losslessly rather than reject it),
        // so it is addressable under that literal key and the real key is absent.
        assert_eq!(
            with_eq.value("Settings", ";gtk-theme-name"),
            Some("Adwaita")
        );
        assert_eq!(with_eq.value("Settings", "gtk-theme-name"), None);

        // Without an `=`, a `;` line is malformed — still never treated as a
        // comment (which would swallow it silently).
        let (without_eq, warnings) = IniFile::parse("[Settings]\n; just a note\n");
        assert_eq!(without_eq.emit(), "[Settings]\n; just a note\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 2);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::MalformedLine);
    }

    #[test]
    fn a_hash_inside_a_value_is_kept_not_treated_as_a_comment() {
        // Unlike the hyprlang parser (where `#` starts an inline comment), GLib key
        // files treat `#` as a comment only at the start of a line, so a `#` within
        // a value is part of the value: it round-trips intact and `value()` returns
        // it including the `#`.
        let input = "[Settings]\ngtk-font-name=Noto Sans #1\n";
        let (mut ini, warnings) = IniFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(ini.emit(), input, "the `#`-bearing value round-trips");
        assert_eq!(
            ini.value("Settings", "gtk-font-name"),
            Some("Noto Sans #1"),
            "the value includes the literal `#`"
        );

        // A new value may itself contain a `#` — it is a valid value character.
        ini.set_value("Settings", "gtk-font-name", "Cantarell #2")
            .expect("`#` is allowed inside a value");
        assert_eq!(ini.emit(), "[Settings]\ngtk-font-name=Cantarell #2\n");
    }

    #[test]
    fn appending_into_a_header_only_section_without_a_final_newline() {
        // Covers two branches at once: the empty-body fallback (insert right after
        // the header) and the append-time repair of a final line that lacks a
        // terminator. Input `"[Settings]"` has no body and no trailing newline.
        let (mut ini, _) = IniFile::parse("[Settings]");
        let outcome = ini
            .set_value("Settings", "gtk-theme-name", "Nordic")
            .expect("append into an empty, unterminated section");
        assert_eq!(outcome, SetOutcome::AppendedKey);
        assert_eq!(
            ini.emit(),
            "[Settings]\ngtk-theme-name=Nordic\n",
            "the header gains a terminator and the key lands right after it"
        );
    }
}
