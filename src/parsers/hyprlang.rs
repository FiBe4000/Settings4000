//! Surgical parser and writer for hyprlang config files (task 3.2;
//! architecture §3; R5.3 item 1, R6.1).
//!
//! # What "hyprlang" is here
//!
//! Hyprland and its companion daemons (hypridle, hyprlock, hyprpaper) share a
//! small config language. A file is a sequence of lines, each one of:
//!
//! - a blank line;
//! - a comment (`#` is the first non-whitespace character; a commented-out
//!   assignment like `#env = X,Y` is a comment, never an assignment);
//! - a `key = value` (or `key=value`) assignment, optionally followed by a
//!   trailing inline `# comment`;
//! - a `source = <path>` directive (just an ordinary assignment whose key is
//!   `source`, kept and addressable like any other key);
//! - a section header `name {` that opens a block, closed by a line that is
//!   just `}`. Blocks nest (`input { touchpad { … } }`) and the same section
//!   name may appear more than once (hypridle has three `listener { }` blocks).
//!
//! This module edits the app-owned hyprlang files the settings GUI manages:
//! `config/hypr/input.conf` (the extracted `input { }` block — analysis §6.3,
//! **not** `hyprland.conf`), `hyprland.conf` (only the two cursor `env =`
//! lines), `hypridle.conf`, `hyprlock.conf`, and `hyprpaper.conf`.
//!
//! # Why a surgical, lossless parser (not a serializer)
//!
//! These files are hand-maintained: they carry header comments, inline comments
//! that document magic numbers (`timeout = 150 # 2.5 min`), deliberate blank-line
//! grouping, and commented-out example lines. The architecture's hard rule for
//! every config parser (architecture §3) is to **never regenerate a file from a
//! model**; instead we keep a lossless line/token representation and, when asked
//! to change a value, rewrite *only* that value's byte span and re-emit every
//! other byte identically. The headline guarantee, covered by tests, is
//! **round-trip identity**: [`HyprlangFile::parse`] then [`HyprlangFile::emit`]
//! with no edit reproduces the input byte-for-byte.
//!
//! # Token model
//!
//! A [`HyprlangFile`] is just the file's [`Line`]s in order. Each line keeps its
//! exact original bytes (terminator included) so emission concatenates them back
//! verbatim, plus a [`LineKind`] classification. Only [`LineKind::Entry`] lines
//! are addressable; blanks, comments, section headers, and malformed lines are
//! preserved but never matched by an edit. An `Entry` additionally records the
//! byte span of its value *before any trailing inline comment*, so an edit can
//! splice a new value while leaving the surrounding whitespace and the comment
//! byte-identical.
//!
//! Hyprlang treats `#` as an inline-comment marker, with `##` escaping a literal
//! `#`. This parser never has to resolve that escape: a value's recorded span
//! stops at the first `#`, so [`HyprlangFile::value`] returns the text before it,
//! and writing a value that contains a `#` is rejected outright
//! ([`EditError::InvalidValue`]) — so a literal `#` never has to be produced. The
//! files this app edits carry no `#` inside a value, so this is only a robustness
//! note.
//!
//! A key is the whitespace-free token before the first `=`; it may contain a `.`
//! or start with `$`, so hyprland's dotted keys (`col.active_border`) and
//! variable declarations (`$mainMod = SUPER`) classify as ordinary entries rather
//! than misfiring as malformed lines.
//!
//! # Addressing schemes
//!
//! Two ways to name a value to read or edit:
//!
//! - **By section path** — [`KeyPath`]. `input.touchpad.natural_scroll` is the
//!   key `natural_scroll` inside `touchpad { }` inside `input { }`. Duplicate
//!   sections are disambiguated by 0-based occurrence
//!   ([`SectionStep::nth`]); the default is the first occurrence. This is what
//!   the input page (task 6.6), power/idle page (task 6.8, positional
//!   `listener` matching), and wallpaper/lock page (task 6.5) use.
//! - **By repeatable top-level key + first field** —
//!   [`HyprlangFile::set_repeatable_field_value`]. A top-level key such as
//!   `env` can appear many times, so a specific line is selected by its first
//!   comma-separated field (e.g. `env = XCURSOR_THEME,…` is selected by
//!   `XCURSOR_THEME`) and only the value portion *after that first comma* is
//!   edited, leaving the field name and everything else byte-identical. This is
//!   what the cursor-env writer (task 6.4) uses.
//!
//! # Appending
//!
//! [`HyprlangFile::set_value`] on a key that does not yet exist **appends** a new
//! assignment at the end of its target section (immediately before the section's
//! closing `}`, or at end-of-file for a top-level key). The new line copies the
//! indentation and the `=`-separator style of an existing sibling assignment so
//! it matches the surrounding formatting; when the section has no sibling
//! assignment to copy, it falls back to the section header's indentation plus
//! four spaces and a ` = ` separator. Appending never touches an existing line.

use std::fmt;

/// A parsed hyprlang file that can re-emit itself byte-for-byte and edit
/// individual values in place.
///
/// Built by [`HyprlangFile::parse`]. See the module documentation for the token
/// model and addressing schemes.
#[derive(Clone, Debug)]
pub(crate) struct HyprlangFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity).
    lines: Vec<Line>,
}

/// One physical line of the file, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of this line **including its terminator**
    /// (`\n` or `\r\n`, or none for a final line with no trailing newline).
    /// This is what [`HyprlangFile::emit`] writes back; it is only ever mutated
    /// by an edit, which splices the value span, or by an append, which pushes a
    /// wholly new line.
    raw: String,
    /// How this line was classified during parsing.
    kind: LineKind,
}

/// The classification of a single line.
///
/// Only [`LineKind::Entry`] lines are addressable by the edit and read methods;
/// every other kind is preserved verbatim and never matched (so a commented-out
/// `#env = …` can never be mistaken for a real `env` assignment).
#[derive(Clone, Debug, PartialEq, Eq)]
enum LineKind {
    /// A line that is empty or only whitespace.
    Blank,
    /// A comment: the first non-whitespace character is `#`. A commented-out
    /// assignment (`#kb_layout=us`) is a comment, never an entry.
    Comment,
    /// A section header `name {` that opens a block.
    SectionOpen {
        /// The section name, with surrounding whitespace trimmed (e.g. `input`,
        /// `touchpad`, `listener`). Used to match [`SectionStep`]s.
        name: String,
    },
    /// A line that is just `}` (ignoring surrounding whitespace and a trailing
    /// inline comment): it closes the most recently opened section.
    SectionClose,
    /// A `key = value` assignment. The key is the whitespace-free token before
    /// the first `=` (it may contain a `.` or start with `$`). The value's byte
    /// range within [`Line::raw`] — which stops before any trailing inline
    /// `# comment` — is recorded so it can be read and rewritten in place.
    Entry {
        /// The assignment's key, with surrounding whitespace trimmed
        /// (e.g. `kb_layout`, `natural_scroll`, `env`, `col.active_border`).
        key: String,
        /// Byte offset within [`Line::raw`] where the key begins (i.e. the
        /// number of leading-whitespace bytes). Used, together with `key` and
        /// `value_start`, to copy this line's indentation and separator style
        /// when appending a sibling.
        key_start: usize,
        /// Byte offset within [`Line::raw`] where the value begins (after the
        /// `=` and any following whitespace).
        value_start: usize,
        /// Byte offset within [`Line::raw`] where the value ends: before any
        /// trailing whitespace, inline comment, and the line terminator. The
        /// half-open range `value_start..value_end` is exactly the bytes an edit
        /// replaces, so an inline comment on the same line survives untouched.
        value_end: usize,
    },
    /// A non-blank, non-comment line that is not a section header or a
    /// `key = value` assignment. Preserved verbatim and surfaced as a
    /// [`ParseWarningKind::MalformedLine`] warning; never editable.
    Malformed,
}

/// One step of a [`KeyPath`]: descend into a named section, choosing which
/// occurrence when several siblings share the name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionStep {
    /// The section name to descend into.
    name: String,
    /// Which occurrence (0-based) of `name` among its same-named siblings to
    /// descend into. `0` selects the first; hypridle's third `listener` block is
    /// occurrence `2`.
    occurrence: usize,
}

impl SectionStep {
    /// Selects the **first** section named `name` at the current level.
    pub(crate) fn first(name: &str) -> Self {
        SectionStep {
            name: name.to_string(),
            occurrence: 0,
        }
    }

    /// Selects the `occurrence`-th (0-based) section named `name` among its
    /// same-named siblings — used to address one of several duplicate sections
    /// such as hypridle's `listener { }` blocks (task 6.8).
    pub(crate) fn nth(name: &str, occurrence: usize) -> Self {
        SectionStep {
            name: name.to_string(),
            occurrence,
        }
    }
}

/// The address of a value reached by descending a (possibly empty) section path
/// and naming a key inside the innermost section.
///
/// An empty section path addresses a top-level key (e.g. `splash` in
/// `hyprpaper.conf`). See the module documentation for the overall scheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyPath {
    /// Section steps from outermost to innermost; empty means top level.
    sections: Vec<SectionStep>,
    /// The key name within the innermost section.
    key: String,
}

impl KeyPath {
    /// Addresses a key at the top level (no enclosing section).
    pub(crate) fn top_level(key: &str) -> Self {
        KeyPath {
            sections: Vec::new(),
            key: key.to_string(),
        }
    }

    /// Addresses a key by a section path, taking the **first** occurrence of
    /// each named section — the common case (e.g. `["input", "touchpad"]` +
    /// `natural_scroll`).
    pub(crate) fn at(section_names: &[&str], key: &str) -> Self {
        KeyPath {
            sections: section_names
                .iter()
                .map(|n| SectionStep::first(n))
                .collect(),
            key: key.to_string(),
        }
    }

    /// Addresses a key by explicit section steps, allowing a specific occurrence
    /// per step (e.g. hypridle's second `listener` block via
    /// `SectionStep::nth("listener", 1)`).
    pub(crate) fn new(sections: Vec<SectionStep>, key: &str) -> Self {
        KeyPath {
            sections,
            key: key.to_string(),
        }
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.sections {
            if step.occurrence == 0 {
                write!(f, "{}.", step.name)?;
            } else {
                write!(f, "{}[{}].", step.name, step.occurrence)?;
            }
        }
        write!(f, "{}", self.key)
    }
}

/// A non-fatal problem noticed while parsing a hyprlang file.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (task 3.2
/// acceptance: malformed/unexpected lines are preserved losslessly and never
/// cause a panic). [`HyprlangFile::parse`] returns the collected warnings *and*
/// logs each at `warn`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseWarning {
    /// 1-based line number the warning concerns, for human-readable diagnostics.
    line: usize,
    /// What was wrong with the line.
    kind: ParseWarningKind,
}

impl ParseWarning {
    /// The 1-based line number this warning concerns.
    pub(crate) fn line(&self) -> usize {
        self.line
    }

    /// What was wrong with the line.
    pub(crate) fn kind(&self) -> &ParseWarningKind {
        &self.kind
    }
}

impl fmt::Display for ParseWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseWarningKind::MalformedLine => write!(
                f,
                "line {}: not a comment, blank line, section header, or key = value entry",
                self.line
            ),
            ParseWarningKind::StrayClosingBrace => {
                write!(
                    f,
                    "line {}: `}}` with no matching section header",
                    self.line
                )
            }
            ParseWarningKind::UnclosedSection { name } => write!(
                f,
                "line {}: section `{name}` opened here is never closed",
                self.line
            ),
        }
    }
}

/// The specific reason a line (or the file's brace structure) produced a
/// [`ParseWarning`].
///
/// Every variant is purely diagnostic: the file still round-trips byte-for-byte
/// and unaffected keys stay addressable. Brace-balance warnings help a caller
/// notice a file it cannot fully address, but the parser tolerates the imbalance
/// gracefully (a stray `}` closes nothing; an unclosed section still lets its
/// keys be addressed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParseWarningKind {
    /// The line is not blank, not a comment, not a section header, and not a
    /// `key = value` assignment (a whitespace-free key token before an `=`) — so
    /// it cannot be interpreted. It is kept byte-for-byte and ignored by edits.
    MalformedLine,
    /// A `}` was found with no open section to close (more closes than opens).
    StrayClosingBrace,
    /// A section was opened but the file ended before it was closed.
    UnclosedSection {
        /// The name of the section left open.
        name: String,
    },
}

/// A failure from an edit method ([`HyprlangFile::set_value`] /
/// [`HyprlangFile::set_repeatable_field_value`]).
///
/// Every variant leaves the file completely unchanged: the check happens before
/// any byte is spliced, so a rejected edit can never partially rewrite a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditError {
    /// The addressed section path does not resolve — some named section (or the
    /// requested occurrence of it) does not exist — so there is no section to
    /// edit a key in or to append to. The string is the rendered [`KeyPath`].
    SectionNotFound(String),
    /// No repeatable top-level entry with `key` whose first comma-field equals
    /// `field` was found (a commented-out line never counts).
    RepeatableKeyNotFound {
        /// The repeatable key that was searched for (e.g. `env`).
        key: String,
        /// The first comma-field that was searched for (e.g. `XCURSOR_THEME`).
        field: String,
    },
    /// A repeatable entry matched by its first field, but its value has no comma,
    /// so there is no value portion after the field to edit.
    NoValuePortion {
        /// The matched repeatable key.
        key: String,
        /// The matched first field.
        field: String,
    },
    /// The requested new value contains a character that would break hyprlang
    /// parsing or the file's line structure: a newline / carriage return (would
    /// split the line) or a `#` (hyprlang would read it as the start of an
    /// inline comment and silently truncate the value). Rejecting it upholds
    /// R8.3 — the app never writes a value that breaks a working config.
    InvalidValue(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::SectionNotFound(path) => {
                write!(f, "no section for `{path}` to edit or append into")
            }
            EditError::RepeatableKeyNotFound { key, field } => {
                write!(f, "no `{key}` entry whose first field is `{field}` to edit")
            }
            EditError::NoValuePortion { key, field } => write!(
                f,
                "the `{key}` entry with first field `{field}` has no comma, so no value portion to edit"
            ),
            EditError::InvalidValue(value) => write!(
                f,
                "`{value}` contains a newline or `#`, which would break hyprlang parsing"
            ),
        }
    }
}

impl std::error::Error for EditError {}

/// Where a [`KeyPath`] resolves: an existing entry, a place to append a new one,
/// or nowhere (the section is missing).
enum Resolution {
    /// The addressed key exists at this line index; edit it in place.
    Entry(usize),
    /// The section exists but the key does not; append a new line per this plan.
    Append(AppendPlan),
    /// The addressed section path does not exist, so nothing can be appended.
    SectionMissing,
}

/// How and where to insert an appended assignment.
struct AppendPlan {
    /// Index in `lines` to insert the new line before. Used only when
    /// `at_eof` is false (an in-section append, inserting before the closing
    /// `}`); for a top-level or unclosed-section append the new line is pushed
    /// at the end instead.
    insert_index: usize,
    /// Line index of an existing direct-child assignment of the target section,
    /// if any, whose indentation and separator style the new line copies.
    sibling_entry: Option<usize>,
    /// Line index of the target section's header, if the target is a real
    /// section (not top level). Used to derive indentation when the section has
    /// no sibling assignment to copy.
    section_open: Option<usize>,
    /// Whether to push at end-of-file (top-level key, or an unclosed target
    /// section) instead of inserting before a closing brace.
    at_eof: bool,
}

impl HyprlangFile {
    /// Parses hyprlang source text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved so [`emit`](Self::emit) always
    /// reproduces the input byte-for-byte. Lines that cannot be interpreted, and
    /// a `}`/section-header imbalance, are returned as [`ParseWarning`]s (sorted
    /// by line number) and additionally logged at `warn`; they are not errors and
    /// do not stop parsing. Warnings carry only line numbers and section names,
    /// never file contents (R7.3).
    pub(crate) fn parse(input: &str) -> (Self, Vec<ParseWarning>) {
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

        collect_brace_warnings(&lines, &mut warnings);
        warnings.sort_by_key(|w| w.line);

        for warning in &warnings {
            // Surface each problem in the journal without dumping file contents
            // (R7.3): the message carries only the line number and, for an
            // unclosed section, its name.
            tracing::warn!(warning = %warning, "hyprlang parse warning");
        }

        (HyprlangFile { lines }, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After edits, the output is identical to the input except within the edited
    /// value spans and any lines appended by [`set_value`](Self::set_value).
    pub(crate) fn emit(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.raw);
        }
        out
    }

    /// Returns the current value of the key addressed by `path`, if it exists.
    ///
    /// The value excludes any trailing inline comment. Comment, blank, section,
    /// and malformed lines are never considered, so a commented-out assignment is
    /// never read as a value.
    pub(crate) fn value(&self, path: &KeyPath) -> Option<&str> {
        match self.resolve_path(path) {
            Resolution::Entry(i) => self.entry_value(i),
            Resolution::Append(_) | Resolution::SectionMissing => None,
        }
    }

    /// Rewrites the value of the key addressed by `path`, changing exactly that
    /// one value span — or, when the key does not yet exist, appending a new
    /// assignment at the end of its target section.
    ///
    /// On an in-place edit, only the value's byte span is replaced: the key, the
    /// spacing around `=`, any indentation or trailing whitespace, a trailing
    /// inline comment, the line terminator, and every other line are left
    /// byte-identical. When several siblings share a section name, the default
    /// path takes the first occurrence; use [`SectionStep::nth`] for a specific
    /// one. When a key appears more than once in the same section, the first
    /// occurrence is edited.
    ///
    /// Errors:
    /// - [`EditError::InvalidValue`] if `value` contains a newline or `#`
    ///   (rejected before any byte changes, R8.3).
    /// - [`EditError::SectionNotFound`] if the addressed section path does not
    ///   exist, so there is nothing to edit or append into.
    pub(crate) fn set_value(&mut self, path: &KeyPath, value: &str) -> Result<(), EditError> {
        reject_unsafe_value(value)?;

        match self.resolve_path(path) {
            Resolution::Entry(i) => {
                self.replace_span(i, value);
                tracing::debug!(path = %path, "rewrote hyprlang value");
                Ok(())
            }
            Resolution::Append(plan) => {
                self.append_entry(&path.key, value, &plan);
                tracing::debug!(path = %path, "appended hyprlang key at section end");
                Ok(())
            }
            Resolution::SectionMissing => Err(EditError::SectionNotFound(path.to_string())),
        }
    }

    /// Returns the value portion (after the first comma) of the repeatable
    /// top-level entry `key` whose first comma-field equals `first_field`.
    ///
    /// For `env = XCURSOR_THEME,Nordic-cursors`, `("env", "XCURSOR_THEME")`
    /// yields `Nordic-cursors`. Returns `None` if no such entry exists or the
    /// matched entry has no comma (no distinct value portion). Only top-level
    /// entries are considered, and commented-out lines never match.
    pub(crate) fn repeatable_field_value(&self, key: &str, first_field: &str) -> Option<&str> {
        let i = self.find_repeatable(key, first_field)?;
        let value = self.entry_value(i)?;
        value_portion(value).map(|(start, end)| &value[start..end])
    }

    /// Rewrites the value portion (after the first comma) of the repeatable
    /// top-level entry `key` whose first comma-field equals `first_field`,
    /// changing only those bytes and leaving the field name and the rest of the
    /// line byte-identical.
    ///
    /// This is how the cursor-env writer edits `hyprland.conf`'s
    /// `env = XCURSOR_THEME,…` / `env = XCURSOR_SIZE,…` lines (task 6.4) without
    /// disturbing sibling `env` lines, `source` lines, or comments.
    ///
    /// Errors:
    /// - [`EditError::InvalidValue`] if `value` contains a newline or `#`.
    /// - [`EditError::RepeatableKeyNotFound`] if no matching entry exists.
    /// - [`EditError::NoValuePortion`] if the matched entry has no comma.
    ///
    /// Unlike [`set_value`](Self::set_value), this never appends: a repeatable
    /// entry has no unambiguous append location, so a missing target is an error.
    pub(crate) fn set_repeatable_field_value(
        &mut self,
        key: &str,
        first_field: &str,
        value: &str,
    ) -> Result<(), EditError> {
        reject_unsafe_value(value)?;

        let Some(i) = self.find_repeatable(key, first_field) else {
            return Err(EditError::RepeatableKeyNotFound {
                key: key.to_string(),
                field: first_field.to_string(),
            });
        };

        // Locate the value-portion span relative to the whole line before
        // mutating, so we replace only the bytes after the first comma.
        let Line { raw, kind } = &mut self.lines[i];
        if let LineKind::Entry {
            value_start,
            value_end,
            ..
        } = kind
        {
            let current = &raw[*value_start..*value_end];
            let Some((portion_start, _portion_end)) = value_portion(current) else {
                return Err(EditError::NoValuePortion {
                    key: key.to_string(),
                    field: first_field.to_string(),
                });
            };
            // The value portion always runs from just after the first comma to
            // the end of the value, so its absolute end is `value_end`.
            let abs_start = *value_start + portion_start;
            raw.replace_range(abs_start..*value_end, value);
            *value_end = abs_start + value.len();
            tracing::debug!(key, first_field, "rewrote repeatable hyprlang field value");
            Ok(())
        } else {
            // `find_repeatable` only ever returns the index of an `Entry`; this
            // branch is unreachable in practice but is handled without panicking.
            Err(EditError::RepeatableKeyNotFound {
                key: key.to_string(),
                field: first_field.to_string(),
            })
        }
    }

    /// Reads an `Entry` line's value slice, or `None` if the line is not an
    /// entry.
    fn entry_value(&self, index: usize) -> Option<&str> {
        if let LineKind::Entry {
            value_start,
            value_end,
            ..
        } = &self.lines[index].kind
        {
            Some(&self.lines[index].raw[*value_start..*value_end])
        } else {
            None
        }
    }

    /// Replaces the value span of the `Entry` at `index` with `value`, updating
    /// the recorded end offset. A no-op if the line is not an entry.
    fn replace_span(&mut self, index: usize, value: &str) {
        let Line { raw, kind } = &mut self.lines[index];
        if let LineKind::Entry {
            value_start,
            value_end,
            ..
        } = kind
        {
            raw.replace_range(*value_start..*value_end, value);
            *value_end = *value_start + value.len();
        }
    }

    /// Resolves a [`KeyPath`] to an existing entry, an append location, or a
    /// missing section, by walking the lines while tracking the open-section
    /// stack and per-section occurrence counts.
    fn resolve_path(&self, path: &KeyPath) -> Resolution {
        let target = &path.sections;
        let target_top_level = target.is_empty();

        // The stack of currently open sections. Each frame counts how many child
        // sections of each name it has seen, so a child's occurrence index is the
        // count at the moment it opens. A separate root counter handles top-level
        // sections.
        let mut frames: Vec<WalkFrame> = Vec::new();
        let mut root_counts: Vec<(String, usize)> = Vec::new();

        // The last direct-child assignment of the target section (for copying
        // append style) and the target section's header line, discovered lazily.
        let mut last_child_entry: Option<usize> = None;
        let mut target_open: Option<usize> = None;

        for (i, line) in self.lines.iter().enumerate() {
            match &line.kind {
                LineKind::SectionOpen { name } => {
                    let occurrence = {
                        let counts = match frames.last_mut() {
                            Some(frame) => &mut frame.child_counts,
                            None => &mut root_counts,
                        };
                        bump_occurrence(counts, name)
                    };
                    frames.push(WalkFrame {
                        name: name.clone(),
                        occurrence,
                        child_counts: Vec::new(),
                    });
                    if !target_top_level && target_open.is_none() && path_matches(&frames, target) {
                        target_open = Some(i);
                        last_child_entry = None;
                    }
                }
                LineKind::SectionClose => {
                    let closing_target = !target_top_level && path_matches(&frames, target);
                    frames.pop();
                    if closing_target {
                        // Reached the end of the target section without finding
                        // the key (a match would have returned already), so this
                        // closing brace is where a new assignment is appended.
                        return Resolution::Append(AppendPlan {
                            insert_index: i,
                            sibling_entry: last_child_entry,
                            section_open: target_open,
                            at_eof: false,
                        });
                    }
                }
                LineKind::Entry { key, .. } => {
                    if path_matches(&frames, target) {
                        if key == &path.key {
                            return Resolution::Entry(i);
                        }
                        last_child_entry = Some(i);
                    }
                }
                LineKind::Blank | LineKind::Comment | LineKind::Malformed => {}
            }
        }

        if target_top_level {
            // A top-level key that does not exist is appended at end-of-file.
            Resolution::Append(AppendPlan {
                insert_index: self.lines.len(),
                sibling_entry: last_child_entry,
                section_open: None,
                at_eof: true,
            })
        } else if target_open.is_some() {
            // The target section opened but never closed (a malformed file).
            // Append at end-of-file as a best effort rather than failing.
            Resolution::Append(AppendPlan {
                insert_index: self.lines.len(),
                sibling_entry: last_child_entry,
                section_open: target_open,
                at_eof: true,
            })
        } else {
            Resolution::SectionMissing
        }
    }

    /// Finds the first repeatable top-level entry with the given key whose first
    /// comma-field equals `first_field`, returning its line index.
    ///
    /// Only entries at the top level (outside every section) are considered, and
    /// comments never match. The nesting is tracked by a simple depth counter
    /// because repeatable keys (`env`, `exec-once`, `source`) live at the top
    /// level.
    fn find_repeatable(&self, key: &str, first_field: &str) -> Option<usize> {
        let mut depth = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            match &line.kind {
                LineKind::SectionOpen { .. } => depth += 1,
                LineKind::SectionClose => depth = depth.saturating_sub(1),
                LineKind::Entry {
                    key: entry_key,
                    value_start,
                    value_end,
                    ..
                } if depth == 0 && entry_key == key => {
                    let value = &line.raw[*value_start..*value_end];
                    let field = value.split(',').next().unwrap_or_default().trim();
                    if field == first_field {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Counts the direct child sections named `name` within the section
    /// identified by `parent` (an empty `parent` means the top level).
    ///
    /// This lets a caller enumerate repeated blocks — e.g. hypridle's
    /// `listener { }` blocks via `section_count(&[], "listener")` (task 6.8) —
    /// without probing occurrence 0, 1, 2, … until a lookup fails. It composes
    /// with nesting: pass the parent path (with occurrences) to count children of
    /// a specific nested section.
    pub(crate) fn section_count(&self, parent: &[SectionStep], name: &str) -> usize {
        let mut frames: Vec<WalkFrame> = Vec::new();
        let mut root_counts: Vec<(String, usize)> = Vec::new();
        let mut count = 0;

        for line in &self.lines {
            match &line.kind {
                LineKind::SectionOpen { name: section_name } => {
                    let occurrence = {
                        let counts = match frames.last_mut() {
                            Some(frame) => &mut frame.child_counts,
                            None => &mut root_counts,
                        };
                        bump_occurrence(counts, section_name)
                    };
                    // A direct child of `parent`: the stack *before* this section
                    // is pushed equals the parent path, and the name matches.
                    if section_name == name && path_matches(&frames, parent) {
                        count += 1;
                    }
                    frames.push(WalkFrame {
                        name: section_name.clone(),
                        occurrence,
                        child_counts: Vec::new(),
                    });
                }
                LineKind::SectionClose => {
                    frames.pop();
                }
                _ => {}
            }
        }

        count
    }

    /// Inserts a new `key <sep> value` assignment per `plan`, choosing
    /// indentation and separator so it matches the target section's existing
    /// style (see the module-level "Appending" documentation).
    fn append_entry(&mut self, key: &str, value: &str, plan: &AppendPlan) {
        let (indent, separator) = match plan.sibling_entry {
            Some(index) => split_indent_separator(&self.lines[index]),
            None => match plan.section_open {
                // A real section with no assignment to copy: indent one level in
                // from the header (four spaces, the hyprlang convention).
                Some(open_index) => (
                    format!("{}    ", leading_whitespace(&self.lines[open_index].raw)),
                    " = ".to_string(),
                ),
                // Top level with no assignment to copy.
                None => (String::new(), " = ".to_string()),
            },
        };

        let terminator = self.line_terminator();
        let raw = format!("{indent}{key}{separator}{value}{terminator}");
        let key_start = indent.len();
        let value_start = key_start + key.len() + separator.len();
        let value_end = value_start + value.len();
        let new_line = Line {
            raw,
            kind: LineKind::Entry {
                key: key.to_string(),
                key_start,
                value_start,
                value_end,
            },
        };

        if plan.at_eof {
            // Ensure the current final line is terminated so the new assignment
            // starts on its own line.
            if let Some(last) = self.lines.last_mut() {
                if !last.raw.ends_with('\n') {
                    last.raw.push_str(terminator);
                }
            }
            self.lines.push(new_line);
        } else {
            self.lines.insert(plan.insert_index, new_line);
        }
    }

    /// The line terminator to use for appended lines: `\r\n` if the file uses
    /// Windows endings anywhere, otherwise `\n`. The app's real targets are LF,
    /// so this mainly keeps an all-CRLF file internally consistent when a key is
    /// appended.
    fn line_terminator(&self) -> &'static str {
        if self.lines.iter().any(|line| line.raw.ends_with("\r\n")) {
            "\r\n"
        } else {
            "\n"
        }
    }
}

/// A frame on the section stack during a [`HyprlangFile::resolve_path`] walk.
struct WalkFrame {
    /// The section's name.
    name: String,
    /// This section's occurrence index among its same-named siblings.
    occurrence: usize,
    /// How many child sections of each name have been seen so far, used to assign
    /// each child its occurrence index as it opens.
    child_counts: Vec<(String, usize)>,
}

/// Returns the current count for `name` in `counts` (its next occurrence index)
/// and increments the stored count.
fn bump_occurrence(counts: &mut Vec<(String, usize)>, name: &str) -> usize {
    if let Some(entry) = counts.iter_mut().find(|(seen, _)| seen == name) {
        let occurrence = entry.1;
        entry.1 += 1;
        occurrence
    } else {
        counts.push((name.to_string(), 1));
        0
    }
}

/// Whether the open-section stack exactly matches a target section path, both
/// name and occurrence, at every level.
fn path_matches(frames: &[WalkFrame], target: &[SectionStep]) -> bool {
    frames.len() == target.len()
        && frames
            .iter()
            .zip(target)
            .all(|(frame, step)| frame.name == step.name && frame.occurrence == step.occurrence)
}

/// Extracts the leading indentation and the `=`-separator bytes of an existing
/// assignment `line`, so an appended sibling can reproduce its style.
///
/// The separator is everything between the end of the key and the start of the
/// value — e.g. `=` for `natural_scroll=true` or ` = ` for `path = /img`.
fn split_indent_separator(line: &Line) -> (String, String) {
    if let LineKind::Entry {
        key,
        key_start,
        value_start,
        ..
    } = &line.kind
    {
        let indent = line.raw[..*key_start].to_string();
        let separator = line.raw[*key_start + key.len()..*value_start].to_string();
        (indent, separator)
    } else {
        (String::new(), " = ".to_string())
    }
}

/// Returns the leading-whitespace prefix of `raw`.
fn leading_whitespace(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    &raw[..raw.len() - trimmed.len()]
}

/// Returns the byte range, within a value, of the portion after the first comma
/// (skipping one run of whitespace immediately after the comma). Returns `None`
/// if the value has no comma. The range always ends at the value's end.
///
/// For `XCURSOR_THEME,Nordic-cursors` this is the span of `Nordic-cursors`.
fn value_portion(value: &str) -> Option<(usize, usize)> {
    let comma = value.find(',')?;
    let after = comma + 1;
    let leading_ws = value[after..].len() - value[after..].trim_start().len();
    Some((after + leading_ws, value.len()))
}

/// Rejects a new value that would break hyprlang parsing or the file's line
/// structure: a newline / carriage return (splits the line) or a `#` (starts an
/// inline comment, silently truncating the value).
///
/// Braces are deliberately *not* rejected: hyprlang tolerates `{`/`}` inside a
/// value, and semantic validation of typed values (hex, `WxH@Hz`, ranges, paths)
/// is the settings model's job (task 4.1), not this byte-level structural guard
/// (R8.3).
fn reject_unsafe_value(value: &str) -> Result<(), EditError> {
    if value.chars().any(|c| matches!(c, '\n' | '\r' | '#')) {
        Err(EditError::InvalidValue(value.to_string()))
    } else {
        Ok(())
    }
}

/// Classifies one raw line (terminator included), recording a parse warning if
/// it is malformed. For an entry it computes the value's byte span — stopping
/// before any trailing inline `# comment` — expressed as offsets into `raw`.
fn classify_line(raw: &str, line_number: usize, warnings: &mut Vec<ParseWarning>) -> LineKind {
    // Work against the content without its line terminator so the terminator is
    // never mistaken for part of a value. `raw` (with terminator) is what we
    // re-emit; `content` is only for locating tokens.
    let content = strip_terminator(raw);

    let trimmed_start = content.trim_start();
    if trimmed_start.is_empty() {
        return LineKind::Blank;
    }
    if trimmed_start.starts_with('#') {
        // A whole-line comment, including a commented-out assignment. Checking
        // `#` first means such a line is never treated as an entry or section.
        return LineKind::Comment;
    }

    // The "code" part of the line excludes any trailing inline comment; hyprlang
    // treats `#` as a comment marker anywhere on a line. Structural detection and
    // the value span are computed from the code part so a trailing comment is
    // preserved outside the value.
    let code = match content.find('#') {
        Some(hash) => &content[..hash],
        None => content,
    };
    let code_trimmed = code.trim();

    if code_trimmed == "}" {
        return LineKind::SectionClose;
    }

    if let Some(before_brace) = code_trimmed.strip_suffix('{') {
        let name = before_brace.trim();
        // A section header is `name {`. Requiring a non-empty name with no `=`
        // avoids misreading a bare `{` or an assignment whose value happens to
        // end in `{` as a section.
        if !name.is_empty() && !name.contains('=') {
            return LineKind::SectionOpen {
                name: name.to_string(),
            };
        }
    }

    // Try to read the line as `key = value` within the code part. The key is the
    // token before the first `=`; accepting any whitespace-free token (rather than
    // a fixed charset) lets real hyprland keys through — dotted names such as
    // `col.active_border` and `$variable` declarations. A key containing a brace
    // is rejected so section syntax is never swallowed as an entry.
    if let Some(eq) = code.find('=') {
        let key = code[..eq].trim();
        if !key.is_empty()
            && !key
                .chars()
                .any(|c| c.is_whitespace() || c == '{' || c == '}')
        {
            // Leading whitespace bytes = where the (trimmed) key begins. Offsets
            // into `content`/`code` index directly into `raw`, since each is a
            // prefix of the previous.
            let key_start = content.len() - trimmed_start.len();
            let after_eq_start = eq + 1;
            let after_eq = &code[after_eq_start..];
            let value_leading = after_eq.len() - after_eq.trim_start().len();
            let value = after_eq.trim();
            let value_start = after_eq_start + value_leading;
            let value_end = value_start + value.len();
            return LineKind::Entry {
                key: key.to_string(),
                key_start,
                value_start,
                value_end,
            };
        }
    }

    warnings.push(ParseWarning {
        line: line_number,
        kind: ParseWarningKind::MalformedLine,
    });
    LineKind::Malformed
}

/// Scans the classified lines for a `}`/section-header imbalance and appends a
/// [`ParseWarning`] for each stray closing brace and each unclosed section.
fn collect_brace_warnings(lines: &[Line], warnings: &mut Vec<ParseWarning>) {
    // Stack of open sections as (1-based open-line number, name).
    let mut open: Vec<(usize, String)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        match &line.kind {
            LineKind::SectionOpen { name } => open.push((index + 1, name.clone())),
            LineKind::SectionClose => {
                // A `}` always closes a section, so the pop is unconditional; a
                // `None` result means there was no matching `section {` open,
                // i.e. this is a stray closing brace. The result is bound to a
                // local first (rather than testing `open.pop().is_none()`
                // directly) so the pop's side effect stays unconditional and
                // the `if` is not folded into a side-effecting `match` guard.
                let closed_section = open.pop();
                if closed_section.is_none() {
                    warnings.push(ParseWarning {
                        line: index + 1,
                        kind: ParseWarningKind::StrayClosingBrace,
                    });
                }
            }
            _ => {}
        }
    }
    for (line, name) in open {
        warnings.push(ParseWarning {
            line,
            kind: ParseWarningKind::UnclosedSection { name },
        });
    }
}

/// Returns `content` with a trailing `\n` or `\r\n` removed, so token and
/// value-span computation never runs into the line terminator.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app-owned `input.conf` (analysis §6.3): a header comment, the
    /// `input { }` block with flat keys, and a nested `touchpad { }` block. Uses
    /// the real `key=value` style (no spaces around `=`).
    const INPUT_CONF: &str = "\
# Input configuration (keyboard layout/options, sensitivity, touchpad)
# Sourced by hyprland.conf. App-owned: written by the settings app; keep it small.
input {
    kb_layout=us,se
    kb_variant=
    kb_model=
    kb_options=grp:win_space_toggle,caps:escape
    kb_rules=

    sensitivity=0.3
    follow_mouse=1

    touchpad {
        natural_scroll=true
        tap-to-click=true
        scroll_factor=1.0
    }
}
";

    /// A `hypridle.conf`: a `general { }` block plus three duplicate
    /// `listener { }` blocks with trailing inline comments and `key = value`
    /// spacing (the shape task 6.8 addresses by positional listener).
    const HYPRIDLE_CONF: &str = "\
general {
    lock_cmd = pidof hyprlock || hyprlock
    before_sleep_cmd = loginctl lock-session
    after_sleep_cmd = hyprctl dispatch dpms on
}

listener {
    timeout = 150          # 2.5 min
    on-timeout = brightnessctl -s set 10   # dim the backlight
    on-resume = brightnessctl -r           # restore the backlight
}

listener {
    timeout = 300          # 5 min
    on-timeout = loginctl lock-session
}

listener {
    timeout = 330          # 5.5 min
    on-timeout = hyprctl dispatch dpms off # screen off
    on-resume = hyprctl dispatch dpms on
}
";

    /// A `hyprpaper.conf`: a `wallpaper { }` block (with an empty `monitor =`
    /// and comments) plus a top-level `splash` key. The wallpaper path is
    /// anonymized to avoid embedding a real home directory.
    const HYPRPAPER_CONF: &str = "\
wallpaper {
    monitor =
    # Keep in sync with hyprlock.conf's background.path (same image).
    path = ~/Pictures/wallpaper/18.jpg
    fit_mode = cover
}

splash = false
";

    /// An excerpt of `hyprland.conf`: `source =` directives, the two cursor
    /// `env =` lines (repeatable top-level keys), a commented-out decoy `env`
    /// line, a commented-out `exec-once`, and a real `exec-once`.
    const HYPRLAND_ENV: &str = "\
# Hyprland main config (excerpt)
source = ~/.config/hypr/colors.conf
source = ~/.config/hypr/monitors.conf
source = ~/.config/hypr/input.conf

# Cursor env, kept identical to uwsm/env (canonical). No other env lines here.
env = XCURSOR_THEME,Nordic-cursors
env = XCURSOR_SIZE,16

# A commented-out example that must never be matched by an edit:
#env = XCURSOR_THEME,DecoyShouldNeverMatch

#exec-once=dbus-update-activation-environment --systemd WAYLAND_DISPLAY
exec-once = hyprpaper
";

    /// An excerpt of `hyprlock.conf`: a `source =`, a `background { }` block with
    /// the lock-screen `path` (task 6.5) and an inline comment on a numeric key.
    const HYPRLOCK_CONF: &str = "\
source = ~/.config/hypr/colors.conf

background {
    monitor =
    path = ~/Pictures/wallpaper/18.jpg
    blur_passes = 2 # 0 disables blurring
}

label {
    text = Hi
    color = $fg2
    font_family = Noto Sans
}
";

    /// A synthetic file with **nested** duplicate sections, to prove occurrence
    /// indexing composes with nesting (two `group { }` blocks inside `outer`).
    const NESTED_DUPLICATE: &str = "\
outer {
    group {
        item = first
    }
    group {
        item = second
    }
}
";

    /// A live-shaped `hyprland.conf` excerpt exercising the broadened key rule:
    /// `$variable` declarations, a `general { }` block with dotted keys
    /// (`col.active_border`), a nested `decoration { blur { } }` block with an
    /// inline comment, the cursor `env =` lines, and a `bind =` line. None of
    /// these may be misclassified as malformed.
    const HYPRLAND_CONF: &str = "\
# Hyprland config (excerpt)
source = ~/.config/hypr/colors.conf

$mainMod = SUPER
$terminal = kitty

general {
    gaps_in = 5
    gaps_out = 10
    border_size = 2
    col.active_border = rgb(83c092) rgb(a7c080) 45deg
    col.inactive_border = rgb(414b50)
    layout = dwindle
}

decoration {
    rounding = 8

    blur {
        enabled = true
        size = 3
        passes = 3 # minimum 1, more passes = more resource intensive.
    }
}

env = XCURSOR_THEME,Nordic-cursors
env = XCURSOR_SIZE,16

bind = $mainMod, Return, exec, $terminal
";

    /// All well-formed fixtures, for the round-trip identity sweep.
    fn well_formed_fixtures() -> Vec<(&'static str, &'static str)> {
        vec![
            ("input.conf", INPUT_CONF),
            ("hypridle.conf", HYPRIDLE_CONF),
            ("hyprpaper.conf", HYPRPAPER_CONF),
            ("hyprland.conf env", HYPRLAND_ENV),
            ("hyprland.conf", HYPRLAND_CONF),
            ("hyprlock.conf", HYPRLOCK_CONF),
            ("nested duplicate", NESTED_DUPLICATE),
        ]
    }

    /// Splits emitted text into lines and returns the indices at which `before`
    /// and `after` differ — used to assert an edit changed exactly one line.
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
            .filter_map(|(i, (b, a))| (b != a).then_some(i))
            .collect()
    }

    #[test]
    fn round_trip_identity_on_realistic_fixtures() {
        // Headline guarantee (R6.1, architecture §3): parse -> emit with no edit
        // reproduces each input byte-for-byte, comments/blanks/order/nesting and
        // commented-out lines included. Well-formed fixtures yield no warnings.
        for (name, fixture) in well_formed_fixtures() {
            let (file, warnings) = HyprlangFile::parse(fixture);
            assert_eq!(file.emit(), fixture, "round-trip failed for {name}");
            assert!(
                warnings.is_empty(),
                "well-formed fixture {name} should yield no warnings, got {warnings:?}"
            );
        }
    }

    #[test]
    fn round_trips_empty_input_and_a_final_line_without_a_newline() {
        for input in [
            "",
            "splash = false",
            "# only a comment",
            "\n\n",
            "input {\n    kb_layout=us\n}",
        ] {
            let (file, _) = HyprlangFile::parse(input);
            assert_eq!(file.emit(), input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn editing_a_deeply_nested_value_changes_only_that_span() {
        // A section-path edit deep in a nested block changes exactly one value
        // span and leaves every other byte untouched.
        let (mut file, _) = HyprlangFile::parse(INPUT_CONF);
        let path = KeyPath::at(&["input", "touchpad"], "natural_scroll");
        assert_eq!(file.value(&path), Some("true"));

        file.set_value(&path, "false").expect("editable entry");
        let edited = file.emit();

        let changed = differing_line_indices(INPUT_CONF, &edited);
        let target = INPUT_CONF
            .lines()
            .position(|l| l == "        natural_scroll=true")
            .expect("fixture contains the natural_scroll entry");
        assert_eq!(changed, vec![target], "exactly one line may change");
        assert_eq!(
            edited.lines().nth(target),
            Some("        natural_scroll=false"),
            "only the value span changed: indentation, key, and `=` are preserved"
        );
    }

    #[test]
    fn input_path_addresses_input_conf_not_hyprland() {
        // The `input.*` addressing resolves in input.conf (the extracted block,
        // analysis §6.3) and NOT in hyprland.conf, which no longer has an input
        // block. Same path: found in one file, absent (SectionNotFound) in the
        // other.
        let path = KeyPath::at(&["input", "touchpad"], "natural_scroll");

        let (input_conf, _) = HyprlangFile::parse(INPUT_CONF);
        assert_eq!(input_conf.value(&path), Some("true"));

        let (mut hyprland, _) = HyprlangFile::parse(HYPRLAND_ENV);
        assert_eq!(hyprland.value(&path), None);
        assert_eq!(
            hyprland.set_value(&path, "false"),
            Err(EditError::SectionNotFound(
                "input.touchpad.natural_scroll".to_string()
            )),
            "an input.* edit must not fall back to some other section"
        );
        assert_eq!(
            hyprland.emit(),
            HYPRLAND_ENV,
            "the rejected edit changed nothing"
        );
    }

    #[test]
    fn duplicate_sections_are_addressed_by_occurrence() {
        // Duplicate-section fixture: the three `listener { }` blocks are read and
        // edited by 0-based occurrence, and editing one leaves the others
        // byte-identical (task 6.8 acceptance).
        let (mut file, _) = HyprlangFile::parse(HYPRIDLE_CONF);

        let first = KeyPath::new(vec![SectionStep::nth("listener", 0)], "timeout");
        let second = KeyPath::new(vec![SectionStep::nth("listener", 1)], "timeout");
        let third = KeyPath::new(vec![SectionStep::nth("listener", 2)], "timeout");
        assert_eq!(file.value(&first), Some("150"));
        assert_eq!(file.value(&second), Some("300"));
        assert_eq!(file.value(&third), Some("330"));

        file.set_value(&second, "600").expect("editable entry");
        let edited = file.emit();

        let changed = differing_line_indices(HYPRIDLE_CONF, &edited);
        let target = HYPRIDLE_CONF
            .lines()
            .position(|l| l == "    timeout = 300          # 5 min")
            .expect("fixture contains the second listener's timeout");
        assert_eq!(
            changed,
            vec![target],
            "only the second listener's timeout line may change"
        );
        assert_eq!(
            edited.lines().nth(target),
            Some("    timeout = 600          # 5 min"),
            "the value changed and the inline comment is preserved"
        );
    }

    #[test]
    fn nested_duplicate_sections_compose_occurrence_indexing() {
        // Occurrence indexing works at depth: two `group { }` blocks nested in
        // `outer` are told apart, and editing one leaves the other untouched.
        let (mut file, _) = HyprlangFile::parse(NESTED_DUPLICATE);

        let first = KeyPath::new(
            vec![SectionStep::first("outer"), SectionStep::nth("group", 0)],
            "item",
        );
        let second = KeyPath::new(
            vec![SectionStep::first("outer"), SectionStep::nth("group", 1)],
            "item",
        );
        assert_eq!(file.value(&first), Some("first"));
        assert_eq!(file.value(&second), Some("second"));

        file.set_value(&second, "edited").expect("editable entry");
        assert_eq!(
            file.emit(),
            NESTED_DUPLICATE.replace("item = second", "item = edited"),
            "only the second nested group's item changed"
        );
    }

    #[test]
    fn repeatable_key_edit_touches_only_the_matched_line() {
        // Repeatable-key fixture: editing `env:XCURSOR_THEME` changes only that
        // line's value portion, leaving the other `env` line, the `source` lines,
        // and the commented-out decoy byte-identical.
        let (mut file, _) = HyprlangFile::parse(HYPRLAND_ENV);
        assert_eq!(
            file.repeatable_field_value("env", "XCURSOR_THEME"),
            Some("Nordic-cursors")
        );
        assert_eq!(
            file.repeatable_field_value("env", "XCURSOR_SIZE"),
            Some("16")
        );

        file.set_repeatable_field_value("env", "XCURSOR_THEME", "Bibata-Modern-Ice")
            .expect("matched env line");
        let edited = file.emit();

        let changed = differing_line_indices(HYPRLAND_ENV, &edited);
        let target = HYPRLAND_ENV
            .lines()
            .position(|l| l == "env = XCURSOR_THEME,Nordic-cursors")
            .expect("fixture contains the cursor-theme env line");
        assert_eq!(
            changed,
            vec![target],
            "only the matched env line may change"
        );
        assert_eq!(
            edited.lines().nth(target),
            Some("env = XCURSOR_THEME,Bibata-Modern-Ice"),
            "the first field is preserved; only the value after the comma changed"
        );
        // The sibling env line and the commented-out decoy are untouched.
        assert!(edited.contains("env = XCURSOR_SIZE,16"));
        assert!(edited.contains("#env = XCURSOR_THEME,DecoyShouldNeverMatch"));
    }

    #[test]
    fn commented_out_lines_are_never_matched() {
        // A commented-out `#exec-once=...` must not be read as the value of the
        // real `exec-once`, and a commented-out `#env = ...` must not be matched
        // by the repeatable-field selector.
        let (file, _) = HyprlangFile::parse(HYPRLAND_ENV);
        assert_eq!(
            file.value(&KeyPath::top_level("exec-once")),
            Some("hyprpaper"),
            "the real exec-once is read, not the commented-out one"
        );
        // The decoy env line is a comment, so selecting XCURSOR_THEME must match
        // the real, uncommented line (Nordic-cursors), never the decoy.
        assert_eq!(
            file.repeatable_field_value("env", "XCURSOR_THEME"),
            Some("Nordic-cursors")
        );
    }

    #[test]
    fn reads_a_top_level_and_a_section_value() {
        let (hyprpaper, _) = HyprlangFile::parse(HYPRPAPER_CONF);
        assert_eq!(
            hyprpaper.value(&KeyPath::top_level("splash")),
            Some("false")
        );
        assert_eq!(
            hyprpaper.value(&KeyPath::at(&["wallpaper"], "path")),
            Some("~/Pictures/wallpaper/18.jpg")
        );

        let (hypridle, _) = HyprlangFile::parse(HYPRIDLE_CONF);
        assert_eq!(
            hypridle.value(&KeyPath::at(&["general"], "lock_cmd")),
            Some("pidof hyprlock || hyprlock"),
            "a value containing `||` is read whole, up to the end of the value"
        );
    }

    #[test]
    fn editing_a_value_with_a_trailing_inline_comment_preserves_the_comment() {
        // The lock command lives on a listener line with a trailing comment in
        // some setups; here we edit a plain listener command and, separately, a
        // commented one, to prove the inline comment survives.
        let (mut file, _) = HyprlangFile::parse(HYPRIDLE_CONF);
        let path = KeyPath::new(vec![SectionStep::nth("listener", 2)], "on-timeout");
        assert_eq!(file.value(&path), Some("hyprctl dispatch dpms off"));

        file.set_value(&path, "systemctl suspend")
            .expect("editable entry");
        assert!(
            file.emit()
                .contains("    on-timeout = systemctl suspend # screen off"),
            "the value changed and the trailing inline comment is preserved"
        );
    }

    #[test]
    fn appending_a_missing_key_lands_at_the_end_of_its_section() {
        // Appending: a new key inside `touchpad { }` lands immediately before the
        // touchpad closing brace, copying the sibling entries' indent (8 spaces)
        // and `=` separator.
        let (mut file, _) = HyprlangFile::parse(INPUT_CONF);
        let path = KeyPath::at(&["input", "touchpad"], "disable_while_typing");
        assert_eq!(file.value(&path), None);

        file.set_value(&path, "true").expect("appends a new key");
        let expected = INPUT_CONF.replace(
            "        scroll_factor=1.0\n    }",
            "        scroll_factor=1.0\n        disable_while_typing=true\n    }",
        );
        assert_eq!(
            file.emit(),
            expected,
            "the new key is inserted just before the touchpad closing brace"
        );
        // It is now addressable at the same path.
        assert_eq!(file.value(&path), Some("true"));
    }

    #[test]
    fn appending_a_top_level_key_lands_at_end_of_file() {
        // A top-level key that does not exist is appended at EOF, copying the
        // ` = ` separator of the existing top-level `splash` entry.
        let (mut file, _) = HyprlangFile::parse(HYPRPAPER_CONF);
        file.set_value(&KeyPath::top_level("ipc"), "off")
            .expect("appends a new top-level key");
        assert_eq!(
            file.emit(),
            format!("{HYPRPAPER_CONF}ipc = off\n"),
            "the new top-level key is appended at end-of-file"
        );
    }

    #[test]
    fn appending_into_a_section_without_siblings_indents_one_level_in() {
        // With no sibling assignment to copy, an appended key uses the section
        // header's indentation plus four spaces and a ` = ` separator.
        let (mut file, _) = HyprlangFile::parse("region {\n}\n");
        file.set_value(&KeyPath::at(&["region"], "key"), "val")
            .expect("appends into an empty section");
        assert_eq!(file.emit(), "region {\n    key = val\n}\n");
    }

    #[test]
    fn a_malformed_line_is_preserved_and_warned_without_panicking() {
        // A malformed line surfaces as a warning on its 1-based line, is kept
        // byte-for-byte, and is not addressable.
        let input = "input {\n    this line has no equals or brace\n    kb_layout=us\n}\n";
        let (file, warnings) = HyprlangFile::parse(input);
        assert_eq!(file.emit(), input, "the malformed line is preserved");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 2);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::MalformedLine);
        // The valid neighbor is still addressable.
        assert_eq!(
            file.value(&KeyPath::at(&["input"], "kb_layout")),
            Some("us")
        );
    }

    #[test]
    fn unbalanced_braces_are_reported_but_preserved() {
        // A stray closing brace and an unclosed section are each reported without
        // failing, and the file still round-trips.
        let stray = "outer {\n    a = 1\n}\n}\n";
        let (file, warnings) = HyprlangFile::parse(stray);
        assert_eq!(file.emit(), stray);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 4);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::StrayClosingBrace);

        let unclosed = "outer {\n    a = 1\n";
        let (file, warnings) = HyprlangFile::parse(unclosed);
        assert_eq!(file.emit(), unclosed);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 1);
        assert_eq!(
            warnings[0].kind(),
            &ParseWarningKind::UnclosedSection {
                name: "outer".to_string()
            }
        );
    }

    #[test]
    fn edits_reject_values_that_would_break_hyprlang() {
        // A newline or `#` in a value is rejected before any byte changes (R8.3).
        let (mut file, _) = HyprlangFile::parse(INPUT_CONF);
        let path = KeyPath::at(&["input"], "kb_layout");
        for bad in ["us\nse", "us # comment", "us\rse"] {
            assert_eq!(
                file.set_value(&path, bad),
                Err(EditError::InvalidValue(bad.to_string()))
            );
        }
        assert_eq!(file.emit(), INPUT_CONF, "no rejected edit changed the file");

        let (mut env, _) = HyprlangFile::parse(HYPRLAND_ENV);
        assert_eq!(
            env.set_repeatable_field_value("env", "XCURSOR_THEME", "a#b"),
            Err(EditError::InvalidValue("a#b".to_string()))
        );
        assert_eq!(env.emit(), HYPRLAND_ENV);
    }

    #[test]
    fn repeatable_edit_reports_a_missing_target() {
        let (mut file, _) = HyprlangFile::parse(HYPRLAND_ENV);
        assert_eq!(
            file.set_repeatable_field_value("env", "GDK_BACKEND", "wayland"),
            Err(EditError::RepeatableKeyNotFound {
                key: "env".to_string(),
                field: "GDK_BACKEND".to_string(),
            })
        );
        assert_eq!(file.emit(), HYPRLAND_ENV);
    }

    #[test]
    fn crlf_line_endings_round_trip_and_edits_never_pull_in_the_carriage_return() {
        // Windows `\r\n` endings survive round-trip (the `\r` is part of the
        // terminator, not the value), and an edit changes only the value bytes.
        let input = "input {\r\n    kb_layout=us,se\r\n}\r\n";
        let (mut file, warnings) = HyprlangFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(file.emit(), input, "CRLF endings must be preserved");

        let path = KeyPath::at(&["input"], "kb_layout");
        assert_eq!(file.value(&path), Some("us,se"));
        file.set_value(&path, "us,se,no").expect("valid edit");
        assert_eq!(
            file.emit(),
            "input {\r\n    kb_layout=us,se,no\r\n}\r\n",
            "only the value changed; the carriage return and newline are untouched"
        );
    }

    #[test]
    fn edits_only_the_first_occurrence_of_a_duplicate_key_in_a_section() {
        // Two keys with the same name in one section: set_value edits the first
        // and leaves the second byte-identical (documented first-match behavior).
        let input = "s {\n    k = one\n    k = two\n}\n";
        let (mut file, _) = HyprlangFile::parse(input);
        file.set_value(&KeyPath::at(&["s"], "k"), "edited")
            .expect("first k is editable");
        assert_eq!(file.emit(), "s {\n    k = edited\n    k = two\n}\n");
    }

    #[test]
    fn key_path_display_renders_occurrences() {
        assert_eq!(
            KeyPath::at(&["input", "touchpad"], "natural_scroll").to_string(),
            "input.touchpad.natural_scroll"
        );
        assert_eq!(
            KeyPath::new(vec![SectionStep::nth("listener", 2)], "timeout").to_string(),
            "listener[2].timeout"
        );
        assert_eq!(KeyPath::top_level("splash").to_string(), "splash");
    }

    #[test]
    fn live_hyprland_conf_classifies_dotted_and_variable_keys_as_entries() {
        // M1: real hyprland lines must not misfire as malformed. Round-trip is
        // byte-identical, there are zero parse warnings, and dotted keys and
        // `$variable` declarations are addressable ordinary entries.
        let (file, warnings) = HyprlangFile::parse(HYPRLAND_CONF);
        assert_eq!(file.emit(), HYPRLAND_CONF);
        assert!(
            warnings.is_empty(),
            "a live hyprland.conf must yield no warnings, got {warnings:?}"
        );
        assert_eq!(
            file.value(&KeyPath::top_level("$mainMod")),
            Some("SUPER"),
            "a $variable declaration is an addressable top-level entry"
        );
        assert_eq!(
            file.value(&KeyPath::at(&["general"], "col.active_border")),
            Some("rgb(83c092) rgb(a7c080) 45deg"),
            "a dotted key with an rgb() value is read whole"
        );
        assert_eq!(
            file.value(&KeyPath::at(&["decoration", "blur"], "passes")),
            Some("3"),
            "a nested value with a trailing inline comment reads without the comment"
        );
    }

    #[test]
    fn repeatable_key_edit_preserves_whitespace_after_the_comma() {
        // N3: the production cursor-env line may have a space after the comma;
        // only the value portion after the comma+whitespace is read and rewritten.
        let input = "env = XCURSOR_THEME, Nordic-cursors\n";
        let (mut file, _) = HyprlangFile::parse(input);
        assert_eq!(
            file.repeatable_field_value("env", "XCURSOR_THEME"),
            Some("Nordic-cursors")
        );
        file.set_repeatable_field_value("env", "XCURSOR_THEME", "Bibata-Modern-Ice")
            .expect("matched env line");
        assert_eq!(
            file.emit(),
            "env = XCURSOR_THEME, Bibata-Modern-Ice\n",
            "the field, the comma, and the space after it are all preserved"
        );
    }

    #[test]
    fn appending_terminates_a_final_line_that_lacks_a_newline() {
        // N2: appending to a file whose last line has no trailing newline first
        // terminates that line, so the result stays well-formed.
        let (mut file, _) = HyprlangFile::parse("splash = false");
        file.set_value(&KeyPath::top_level("ipc"), "off")
            .expect("appends a top-level key");
        assert_eq!(file.emit(), "splash = false\nipc = off\n");
    }

    #[test]
    fn appending_into_a_crlf_file_uses_crlf() {
        // N2: an appended line matches the file's Windows terminator so a CRLF
        // file stays internally consistent.
        let input = "wallpaper {\r\n    path = ~/a.png\r\n}\r\n";
        let (mut file, _) = HyprlangFile::parse(input);
        file.set_value(&KeyPath::at(&["wallpaper"], "fit_mode"), "cover")
            .expect("appends into the wallpaper section");
        assert_eq!(
            file.emit(),
            "wallpaper {\r\n    path = ~/a.png\r\n    fit_mode = cover\r\n}\r\n"
        );
    }

    #[test]
    fn section_count_enumerates_repeated_blocks() {
        // N5: counting repeated sections lets task 6.8 enumerate listeners
        // without probing occurrences until one is missing.
        let (hypridle, _) = HyprlangFile::parse(HYPRIDLE_CONF);
        assert_eq!(hypridle.section_count(&[], "listener"), 3);
        assert_eq!(hypridle.section_count(&[], "general"), 1);
        assert_eq!(hypridle.section_count(&[], "absent"), 0);

        // It composes with nesting: two `group` blocks inside `outer`.
        let (nested, _) = HyprlangFile::parse(NESTED_DUPLICATE);
        assert_eq!(
            nested.section_count(&[SectionStep::first("outer")], "group"),
            2
        );
    }
}
