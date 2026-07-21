//! Surgical, lossless editor for the uwsm session-environment file
//! (`config/uwsm/env`, task 3.6; architecture §3; R3.3, R5.3 item 1, R6.1).
//!
//! # What this file is
//!
//! uwsm reads a shell-sourced environment file that seeds every process in the
//! Hyprland session. It sources the file with **allexport** (`set -a`), so both
//! `export KEY=value` and a bare `KEY=value` line export the variable — they are
//! semantically identical, and this parser treats them the same. In the dotfiles
//! this app manages the file is the **canonical** home of the non-cursor session
//! env (Qt platform theme, Ozone/GTK backend hints, …) and one of the several
//! places the cursor theme/size is declared (analysis §6.3). A representative
//! file (its lines all use the `export` form):
//!
//! ```text
//! export GDK_BACKEND=wayland,x11
//! #export GTK_THEME=Nordic-bluish-accent
//! # Cursor theme/size: canonical value; keep in sync with hyprland.conf
//! export XCURSOR_THEME=Nordic-cursors
//! export XCURSOR_SIZE=16
//! export QT_QPA_PLATFORM="wayland;xcb"
//! ```
//!
//! # What the app edits here, and what it only reads
//!
//! - **Edits `XCURSOR_THEME` / `XCURSOR_SIZE`.** These are duplicated across
//!   `uwsm/env` (canonical), `hyprland.conf`'s env lines, and both
//!   `gtk-{3,4}.0/settings.ini`; the Theme page (task 6.4) writes the *same*
//!   value into every copy so they never desync (R3.4, analysis §6.2). This
//!   module owns only the `uwsm/env` copy — the hyprlang writer (task 3.2) and
//!   the INI editor (task 3.5) own the others. Keeping the edit API value-only
//!   lets the page drive all copies from one value.
//! - **Only reads `GTK_THEME`.** A set `GTK_THEME` overrides GTK's theme choice
//!   entirely, so the app must never fight it (R3.3): if it finds an active
//!   (uncommented) `GTK_THEME=…` — in either the `export` or the bare form — it
//!   shows a banner and disables the GTK-theme drop-down. In the dotfiles this
//!   line is present but **commented out**, which is *not* an override — so the
//!   app must distinguish a commented-out `#export GTK_THEME=…` (inactive,
//!   drop-down stays enabled) from an uncommented one (active override).
//!   [`EnvFile::gtk_theme_override`] reports exactly that. The app's *own
//!   process* environment is the other place an override can come from
//!   (`scripts/launchhyprland.sh` exports it uncommented, analysis §6.3); that
//!   check belongs to the page, not this file-only parser.
//!
//! # Why a surgical, lossless editor (not a serializer)
//!
//! The file is hand-maintained: it carries comments (including the deliberately
//! commented-out `GTK_THEME` line and "keep in sync" notes), blank-line grouping,
//! and a chosen ordering. The architecture's hard rule for every config parser
//! (architecture §3) is to **never regenerate a file from a model**; instead we
//! keep a lossless line representation and, when asked to change a value, rewrite
//! *only* that value's byte span and re-emit every other byte identically. Two
//! guarantees follow, both covered by tests:
//!
//! - **Round-trip identity**: [`EnvFile::parse`] then [`EnvFile::emit`] with no
//!   edit reproduces the input byte-for-byte, comments and ordering included.
//! - **Single-span edits**: [`EnvFile::set_value`] on an existing assignment
//!   touches exactly that value's bytes and leaves the `export` keyword (if
//!   present), the key, the `=`, any surrounding whitespace, comments, and all
//!   other lines untouched.
//!
//! # Quoting
//!
//! The value span is the raw text after `=` (surrounding whitespace excluded) —
//! **quotes included**. The app only ever writes bare cursor tokens
//! (`Nordic-cursors`, `16`), which need no quoting, so a value is spliced in
//! verbatim. A pre-existing quoted value (e.g. `QT_QPA_PLATFORM="wayland;xcb"`)
//! is preserved byte-for-byte on any unrelated edit and, if read back, is
//! returned with its quotes; editing such a key replaces the whole quoted token,
//! so a caller that wants quoting must supply it. This module deliberately does
//! **not** parse or synthesize shell quoting (that would be over-engineering for
//! two bare tokens). The only hard guard is that a written value may not contain
//! a newline or carriage return, which would split the line
//! ([`EditError::InvalidValue`], R8.3).
//!
//! # Physical file creation is out of scope here
//!
//! `uwsm/env` is tracked in the dotfiles and symlinked into place, so on the
//! target machine it always exists and the edit-or-append path is what runs. This
//! module never touches the filesystem; creating an absent file on disk is a
//! writer/Apply concern (the atomic writer, task 2.2, only rewrites existing
//! targets — analysis §6.5), the same as for the sibling INI editor (task 3.5).

use std::fmt;

/// A parsed `uwsm/env` file that can re-emit itself byte-for-byte, edit the value
/// of an assignment in place, and report whether a `GTK_THEME` override is
/// present.
///
/// Built by [`EnvFile::parse`]. Internally it is just the file's lines in order,
/// each classified and — for a `KEY=value` assignment (with or without `export`) —
/// annotated with the byte span of its value within that line's raw text.
/// Emitting concatenates the raw line texts, so an unedited file reproduces its
/// input exactly; editing a value splices new bytes into a single line's value
/// span and leaves every other line alone.
#[derive(Clone, Debug)]
pub struct EnvFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity).
    lines: Vec<Line>,
}

/// One physical line of the file, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of this line **including its terminator**
    /// (`\n` or `\r\n`, or none for a final line with no trailing newline).
    /// This is what [`EnvFile::emit`] writes back; it is only ever mutated by an
    /// edit, which splices a value span, or grown by an append, which pushes a
    /// wholly new line.
    raw: String,
    /// How this line was classified during parsing.
    kind: LineKind,
}

/// The classification of a single line.
///
/// Only [`LineKind::Assignment`] lines are addressable by [`EnvFile::set_value`];
/// blanks, comments, and malformed lines are preserved verbatim and never matched
/// by an edit — so a commented-out `#export GTK_THEME=…` line can never be
/// mistaken for, or rewritten as, an active assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LineKind {
    /// A line that is empty or only whitespace.
    Blank,
    /// A comment: the first non-whitespace character is `#`. This is the shell
    /// comment marker; a commented-out assignment (`#export GTK_THEME=…`,
    /// `# export GTK_THEME=…`, or `# GTK_THEME=…`) is a comment, never an
    /// [`LineKind::Assignment`]. The `#` check comes before the assignment check
    /// so such a line is never treated as an active assignment.
    /// [`EnvFile::gtk_theme_override`] peeks *inside* comments to tell a
    /// commented-out `GTK_THEME` from an absent one, but never promotes it to an
    /// entry or edits it.
    Comment,
    /// A `KEY=value` assignment with a valid shell-identifier key, in either the
    /// `export KEY=value` form or the bare `KEY=value` form. uwsm sources the file
    /// with allexport (`set -a`), so a bare assignment exports the variable too —
    /// both forms are addressable and edited identically. The value's byte range
    /// within [`Line::raw`] is recorded so it can be read and rewritten in place.
    /// Shell does not treat `#` as an inline comment inside an unquoted assignment
    /// word, so the value span runs to the end of the line's content (only
    /// surrounding whitespace is excluded) and may contain a `#`.
    Assignment {
        /// The variable name, e.g. `XCURSOR_THEME`. Used to match
        /// [`EnvFile::set_value`] targets.
        key: String,
        /// Whether the line used the `export` keyword. Not needed for round-trip
        /// (the raw bytes carry the exact text) — recorded so an edit can log
        /// which form it rewrote and to document that both forms are recognized.
        exported: bool,
        /// Byte offset within [`Line::raw`] where the value begins (after the
        /// `=` and any following whitespace).
        value_start: usize,
        /// Byte offset within [`Line::raw`] where the value ends (before any
        /// trailing whitespace and the line terminator). The half-open range
        /// `value_start..value_end` is exactly the bytes an edit replaces.
        value_end: usize,
    },
    /// A non-blank, non-comment line that is not a recognized `KEY=value`
    /// assignment (no `=`, or a key that is not a valid shell identifier).
    /// Preserved verbatim and surfaced as a [`ParseWarningKind::MalformedLine`]
    /// warning; never editable.
    Malformed,
}

/// A non-fatal problem noticed while parsing a `uwsm/env` file.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (task 3.6
/// acceptance: malformed lines are preserved losslessly and surfaced as warnings,
/// not panics). [`EnvFile::parse`] returns the collected warnings *and* logs each
/// at `warn`, so the caller can both react programmatically and see them in the
/// journal.
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
                "line {}: not a comment, blank line, or `KEY=value` assignment",
                self.line
            ),
        }
    }
}

/// The specific reason a line produced a [`ParseWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseWarningKind {
    /// The line is not blank, not a comment, and not a recognized `KEY=value`
    /// assignment — so it cannot be interpreted. It is kept byte-for-byte and
    /// ignored by edits.
    MalformedLine,
}

/// Which action [`EnvFile::set_value`] took, for logging and for tests that need
/// to distinguish an in-place edit from an append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// An assignment for the key already existed (in either the `export KEY=value`
    /// or the bare `KEY=value` form); its value span was rewritten in place. This
    /// is the common path on the dotfiles machine, where `XCURSOR_THEME`/
    /// `XCURSOR_SIZE` are already present (analysis §6.3).
    Edited,
    /// No assignment for the key existed; a new `export KEY=value` line was
    /// appended at end-of-file in the file's established form.
    Appended,
}

/// Whether the file declares a `GTK_THEME` override, and if so whether it is
/// active (R3.3).
///
/// A set `GTK_THEME` env var overrides GTK's theme selection, so the app must not
/// fight it: an [`Active`](Self::Active) override means the Theme page shows a
/// banner and disables the GTK-theme drop-down. A [`Commented`](Self::Commented)
/// line is present but inactive — it does *not* override anything, so the
/// drop-down stays enabled; it is reported so the UI can, for example, mention
/// that the line exists. In the dotfiles the line is deliberately commented out
/// (analysis §6.3), so distinguishing the two is the whole point of this check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GtkThemeOverride {
    /// No `GTK_THEME` line at all, active or commented — no override from this
    /// file.
    Absent,
    /// An active, uncommented `GTK_THEME=<value>` (in either the `export` or the
    /// bare form — both export the variable under allexport). This is a live
    /// override: the app must show a banner and disable the GTK-theme drop-down
    /// (R3.3).
    Active {
        /// The override value as it appears after `=` (quotes, if any, included).
        value: String,
    },
    /// A commented-out `GTK_THEME=<value>` line (`#export GTK_THEME=…`,
    /// `# export GTK_THEME=…`, or `# GTK_THEME=…`). Present but inactive — no
    /// override is in force.
    Commented {
        /// The value from the commented-out line, for display.
        value: String,
    },
}

impl GtkThemeOverride {
    /// Whether a live `GTK_THEME` override is in force in this file — the single
    /// signal the Theme page keys off to disable its GTK-theme drop-down and show
    /// the banner (R3.3). Only [`GtkThemeOverride::Active`] returns `true`; a
    /// commented-out or absent line does not override anything.
    pub fn is_active(&self) -> bool {
        matches!(self, GtkThemeOverride::Active { .. })
    }
}

/// A failure from [`EnvFile::set_value`].
///
/// Both variants leave the file completely unchanged: the check happens before
/// any byte is written, so a rejected edit can never partially rewrite the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The requested value contains a newline or carriage return, which would
    /// split the single assignment line into two and corrupt the file. Rejecting
    /// it upholds R8.3 — the app never writes a value that breaks a working config.
    /// (Any other character is left to higher-level validation; the app writes
    /// bare cursor tokens, which are always safe.)
    InvalidValue(String),
    /// A key used to append a new assignment is not a valid shell identifier
    /// (empty, starts with a digit, or contains a character outside `[A-Za-z0-9_]`),
    /// so it could not be written as `export KEY=value`. Only checked when
    /// appending; an in-place edit never rewrites the key.
    InvalidKey(String),
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
                "`{key}` is not a valid shell identifier (empty, starts with a digit, or contains \
                 a character outside [A-Za-z0-9_])"
            ),
        }
    }
}

impl std::error::Error for EditError {}

impl EnvFile {
    /// Parses `uwsm/env` text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved, whether or not it is a valid
    /// assignment, so [`emit`](Self::emit) always reproduces the input
    /// byte-for-byte. Lines that cannot be interpreted (and are not comments or
    /// blanks) are returned as [`ParseWarning`]s and additionally logged at
    /// `warn`; they are *not* errors and do not stop parsing (task 3.6 acceptance).
    ///
    /// The returned warnings carry only line numbers — never the file's contents,
    /// which are not logged at any level (R7.3).
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
            tracing::warn!(warning = %warning, "uwsm/env parse warning");
        }

        (EnvFile { lines }, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input when
    /// no edit has been made (round-trip identity).
    ///
    /// After [`set_value`](Self::set_value) edits, the output is identical to the
    /// input except within edited value spans and any line appended by a new
    /// assignment.
    pub fn emit(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.raw);
        }
        out
    }

    /// Returns the current value of the variable `key`, if present.
    ///
    /// Matches both the `export KEY=value` and the bare `KEY=value` form. When a
    /// variable is assigned more than once, the shell sources the file top to
    /// bottom so the **last** assignment is the effective one; this reads that
    /// last assignment. Comment lines (including a commented-out `#export KEY=…`),
    /// blanks, and malformed lines are never considered — so a commented-out
    /// `GTK_THEME` reads as absent here. The returned slice is the raw value span,
    /// so a quoted value is returned with its quotes (see the module's quoting
    /// note).
    pub fn value(&self, key: &str) -> Option<&str> {
        // Iterate in reverse so the first hit is the last (shell-effective)
        // assignment.
        for line in self.lines.iter().rev() {
            if let LineKind::Assignment {
                key: entry_key,
                value_start,
                value_end,
                ..
            } = &line.kind
            {
                if entry_key == key {
                    return Some(&line.raw[*value_start..*value_end]);
                }
            }
        }
        None
    }

    /// Sets the variable `key` to `value`, editing in place or appending.
    ///
    /// Two cases, reported by the returned [`SetOutcome`]:
    ///
    /// - **Edit** ([`SetOutcome::Edited`]): an assignment for the key exists (in
    ///   either the `export KEY=value` or the bare `KEY=value` form) — only its
    ///   value's byte span is rewritten. The `export` keyword (if present), the
    ///   key, the `=`, any surrounding whitespace, the terminator, comments, and
    ///   every other line stay byte-identical. If the variable is assigned more
    ///   than once only the **last** (shell-effective) occurrence is edited;
    ///   earlier shadowed copies stay byte-identical.
    /// - **Append** ([`SetOutcome::Appended`]): no assignment for the key exists —
    ///   a new `export KEY=value` line is appended at end-of-file (the file's
    ///   established convention; every line in the real file uses `export`). A
    ///   missing final newline on the previous last line is repaired first so the
    ///   new line does not run onto it.
    ///
    /// A commented-out `#export KEY=…` line is **never** matched (it is a comment,
    /// not an assignment), so calling this for a key that appears only
    /// commented-out appends a fresh active export and leaves the comment
    /// byte-identical. This is why the app never calls `set_value` for
    /// `GTK_THEME`: it reads that key via
    /// [`gtk_theme_override`](Self::gtk_theme_override) and never writes it.
    ///
    /// `value` must not contain a newline or carriage return
    /// ([`EditError::InvalidValue`]); when appending, `key` must be a valid shell
    /// identifier ([`EditError::InvalidKey`]). Any rejected edit leaves the file
    /// completely unchanged (R8.3).
    pub fn set_value(&mut self, key: &str, value: &str) -> Result<SetOutcome, EditError> {
        reject_unsafe_value(value)?;

        // Target the LAST matching assignment: the shell resolves a repeated
        // assignment to its last value, so editing an earlier (shadowed) copy would
        // not take effect. `rposition` finds the effective occurrence, across both
        // the `export` and bare forms.
        let target = self.lines.iter().rposition(
            |line| matches!(&line.kind, LineKind::Assignment { key: k, .. } if k.as_str() == key),
        );

        if let Some(index) = target {
            let Line { raw, kind } = &mut self.lines[index];
            if let LineKind::Assignment {
                exported,
                value_start,
                value_end,
                ..
            } = kind
            {
                raw.replace_range(*value_start..*value_end, value);
                *value_end = *value_start + value.len();
                tracing::debug!(key, value, exported = *exported, "rewrote uwsm/env value");
                return Ok(SetOutcome::Edited);
            }
        }

        // Key absent: append a new export at end-of-file.
        reject_unsafe_key(key)?;
        self.append_export(key, value);
        tracing::debug!(key, value, "appended uwsm/env export");
        Ok(SetOutcome::Appended)
    }

    /// Reports whether the file declares a `GTK_THEME` override, distinguishing an
    /// active (uncommented) one from a commented-out one (R3.3).
    ///
    /// The Theme page (task 6.4) uses this to decide whether to show the
    /// "override active" banner and disable the GTK-theme drop-down. Both the
    /// `export GTK_THEME=…` and the bare `GTK_THEME=…` forms count as active (both
    /// export the variable under allexport). An active assignment wins over a
    /// commented-out line regardless of their order; when several of the same kind
    /// exist the last is reported (matching the shell's last-assignment-wins rule
    /// for the active case). This method only *reads* — it never edits the file,
    /// so a commented-out line stays exactly as written.
    pub fn gtk_theme_override(&self) -> GtkThemeOverride {
        let mut active: Option<String> = None;
        let mut commented: Option<String> = None;

        for line in &self.lines {
            match &line.kind {
                LineKind::Assignment {
                    key,
                    value_start,
                    value_end,
                    ..
                } if key == GTK_THEME_KEY => {
                    active = Some(line.raw[*value_start..*value_end].to_string());
                }
                LineKind::Comment => {
                    if let Some(value) = commented_assignment_value(&line.raw, GTK_THEME_KEY) {
                        commented = Some(value);
                    }
                }
                _ => {}
            }
        }

        // An active override takes precedence: if the variable is assigned live,
        // any commented-out line alongside it is irrelevant.
        if let Some(value) = active {
            GtkThemeOverride::Active { value }
        } else if let Some(value) = commented {
            GtkThemeOverride::Commented { value }
        } else {
            GtkThemeOverride::Absent
        }
    }

    /// Appends `export KEY=value` at end-of-file in the canonical shell form.
    ///
    /// Ensures the current final line ends with a terminator first, so the new
    /// export starts on its own line even when the file lacked a trailing newline.
    fn append_export(&mut self, key: &str, value: &str) {
        let terminator = self.line_terminator().to_string();

        if let Some(last) = self.lines.last_mut() {
            if !last.raw.ends_with('\n') {
                last.raw.push_str(&terminator);
            }
        }

        let raw = format!("export {key}={value}{terminator}");
        let mut discard = Vec::new();
        let kind = classify_line(&raw, 0, &mut discard);
        self.lines.push(Line { raw, kind });
    }

    /// The line terminator to use for an appended line: `\r\n` if the file uses
    /// Windows endings anywhere, otherwise `\n`. The app's real target is LF; this
    /// mainly keeps an all-CRLF file internally consistent.
    fn line_terminator(&self) -> &'static str {
        if self.lines.iter().any(|line| line.raw.ends_with("\r\n")) {
            "\r\n"
        } else {
            "\n"
        }
    }
}

/// The variable name whose override the app must detect but never write (R3.3).
const GTK_THEME_KEY: &str = "GTK_THEME";

/// Classifies one raw line (terminator included) and records a parse warning if
/// it is malformed.
///
/// For an assignment it computes the value's byte span as offsets into `raw`, so
/// the span can be stored in [`LineKind::Assignment`] and later used to splice a
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
        // A comment — including a commented-out assignment like
        // `#export GTK_THEME=…`. The `#` check comes before the assignment check
        // so such a line is never treated as an active assignment.
        return LineKind::Comment;
    }

    // The offsets returned by `scan_assignment` are relative to `content`, which
    // is a prefix of `raw` (only the trailing terminator differs), so they index
    // into `raw` directly.
    if let Some(matched) = scan_assignment(content) {
        return LineKind::Assignment {
            key: matched.key,
            exported: matched.exported,
            value_start: matched.value_start,
            value_end: matched.value_end,
        };
    }

    warnings.push(ParseWarning {
        line: line_number,
        kind: ParseWarningKind::MalformedLine,
    });
    LineKind::Malformed
}

/// A recognized `KEY=value` assignment: its key, whether it used the `export`
/// keyword, plus the byte span of its value within the scanned string.
struct AssignmentMatch {
    /// The variable name.
    key: String,
    /// Whether the line began with the `export` keyword (vs a bare assignment).
    exported: bool,
    /// Byte offset (within the scanned slice) where the value begins.
    value_start: usize,
    /// Byte offset (within the scanned slice) where the value ends.
    value_end: usize,
}

/// Tries to read `content` (a single line, terminator already stripped) as a
/// `KEY=value` assignment, returning the key, whether it used `export`, and the
/// value's byte span within `content`.
///
/// Accepts optional leading whitespace, then an **optional** `export` keyword
/// (a whole word followed by whitespace), then `KEY=value` where `KEY` is a valid
/// shell identifier. Both `export KEY=value` and a bare `KEY=value` are accepted
/// because uwsm sources the file with allexport, so a bare assignment exports the
/// variable just the same. Returns `None` for anything else (no `=`, a
/// non-identifier key, an `export` with no assignment, …). All the structural
/// tokens (`export`, `=`, whitespace) are ASCII, so byte-offset arithmetic stays
/// on char boundaries; a multibyte value slices cleanly because its span is
/// delimited by ASCII whitespace and the `=`.
fn scan_assignment(content: &str) -> Option<AssignmentMatch> {
    // Shell allows an assignment to be indented; skip any leading blanks.
    let start = skip_ascii_ws(content, 0);

    // Optionally consume a leading `export` keyword: it must be the whole word
    // `export` followed by at least one space/tab. If `export` is present but not
    // followed by whitespace (e.g. `exported=…` or `export=…`), it is the variable
    // name of a bare assignment, not the keyword.
    const EXPORT: &str = "export";
    let (assign_start, exported) = if content[start..].starts_with(EXPORT) {
        let after_keyword = start + EXPORT.len();
        let after_ws = skip_ascii_ws(content, after_keyword);
        if after_ws > after_keyword {
            (after_ws, true)
        } else {
            (start, false)
        }
    } else {
        (start, false)
    };

    // The remainder must be `KEY=value`.
    let eq_relative = content[assign_start..].find('=')?;
    let eq = assign_start + eq_relative;
    let key = &content[assign_start..eq];
    if !is_shell_identifier(key) {
        return None;
    }

    // The value is the content after `=` with surrounding whitespace trimmed; the
    // offsets are relative to `content`. Internal characters (including `#` and
    // quotes) are part of the value and preserved.
    let after_eq = &content[eq + 1..];
    let leading_ws = after_eq.len() - after_eq.trim_start().len();
    let value = after_eq.trim();
    let value_start = eq + 1 + leading_ws;
    let value_end = value_start + value.len();

    Some(AssignmentMatch {
        key: key.to_string(),
        exported,
        value_start,
        value_end,
    })
}

/// If `raw` is a comment line whose commented-out content is a `<key>=<value>`
/// assignment (with or without `export`), returns that value; otherwise `None`.
///
/// This is how [`EnvFile::gtk_theme_override`] tells a commented-out `GTK_THEME`
/// (present but inactive) from an absent one, without ever promoting the comment
/// to an editable entry. It strips the leading whitespace and one-or-more `#`
/// markers, then reuses [`scan_assignment`] on the remainder — so `#export
/// GTK_THEME=…`, `# export GTK_THEME=…`, and `# GTK_THEME=…` all match.
fn commented_assignment_value(raw: &str, key: &str) -> Option<String> {
    let content = strip_terminator(raw);
    let after_hashes = content.trim_start().trim_start_matches('#');
    let matched = scan_assignment(after_hashes)?;
    if matched.key == key {
        Some(after_hashes[matched.value_start..matched.value_end].to_string())
    } else {
        None
    }
}

/// Advances `pos` over ASCII space and tab characters in `content`, returning the
/// offset of the first non-whitespace byte at or after `pos`.
///
/// Only spaces and tabs are skipped — the line terminator has already been
/// stripped by [`strip_terminator`], so there is no `\n`/`\r` to consider.
fn skip_ascii_ws(content: &str, mut pos: usize) -> usize {
    let bytes = content.as_bytes();
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    pos
}

/// Returns `content` with a trailing `\n` or `\r\n` removed, so value-span
/// computation never runs into the line terminator.
///
/// The terminator is only stripped for locating tokens; the caller keeps the full
/// `raw` (terminator included) for lossless emission.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

/// Whether `key` is a valid POSIX-shell variable identifier: non-empty, a first
/// character that is an ASCII letter or `_`, and remaining characters that are
/// ASCII alphanumeric or `_`.
///
/// Used both to classify a `KEY=value` line and to reject an unusable key on
/// append. A stray space (`KEY =…`) leaves whitespace in the key candidate and so
/// fails here, correctly rejecting a non-assignment.
fn is_shell_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Rejects a value that would split the assignment line: a newline or carriage
/// return. Any other character is allowed here — value-content validation (e.g.
/// shell-safe tokens) is a higher-level concern (task 4.1); the app only ever
/// writes bare cursor tokens through this path.
fn reject_unsafe_value(value: &str) -> Result<(), EditError> {
    if value.chars().any(|c| matches!(c, '\n' | '\r')) {
        Err(EditError::InvalidValue(value.to_string()))
    } else {
        Ok(())
    }
}

/// Rejects a key that could not be written as the name in `export KEY=value`:
/// anything that is not a valid shell identifier.
fn reject_unsafe_key(key: &str) -> Result<(), EditError> {
    if is_shell_identifier(key) {
        Ok(())
    } else {
        Err(EditError::InvalidKey(key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `uwsm/env` fixture derived from the real dotfiles (analysis
    /// §6.3): a header comment, the non-cursor session env, the canonical cursor
    /// keys, a quoted value, and a **commented-out** `GTK_THEME` line — the shape
    /// of the tracked, symlinked file on the target machine, where the override is
    /// present but inactive.
    const UWSM_ENV_COMMENTED: &str = "\
# uwsm session environment (app-owned; analysis §6.3)
export GDK_BACKEND=wayland,x11
#export GTK_THEME=Nordic-bluish-accent
# Cursor theme/size: canonical value; keep in sync with config/hypr/hyprland.conf
export XCURSOR_THEME=Nordic-cursors
export XCURSOR_SIZE=16
export QT_QPA_PLATFORM=\"wayland;xcb\"
export QT_QPA_PLATFORMTHEME=gtk3
";

    /// The same file but with `GTK_THEME` **uncommented** — an active override the
    /// app must detect (R3.3), e.g. because a user turned the line back on.
    const UWSM_ENV_ACTIVE_OVERRIDE: &str = "\
# uwsm session environment (app-owned; analysis §6.3)
export GDK_BACKEND=wayland,x11
export GTK_THEME=Nordic-bluish-accent
# Cursor theme/size: canonical value; keep in sync with config/hypr/hyprland.conf
export XCURSOR_THEME=Nordic-cursors
export XCURSOR_SIZE=16
export QT_QPA_PLATFORM=\"wayland;xcb\"
export QT_QPA_PLATFORMTHEME=gtk3
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
        let (env, warnings) = EnvFile::parse(UWSM_ENV_COMMENTED);
        assert_eq!(
            env.emit(),
            UWSM_ENV_COMMENTED,
            "emit must reproduce the input byte-for-byte"
        );
        assert!(
            warnings.is_empty(),
            "a clean, well-formed file must yield no warnings, got {warnings:?}"
        );
        assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic-cursors"));
        assert_eq!(env.value("XCURSOR_SIZE"), Some("16"));
        // A quoted value is returned with its quotes (see the module's quoting
        // note) — the app never edits it, but reads must not corrupt it.
        assert_eq!(env.value("QT_QPA_PLATFORM"), Some("\"wayland;xcb\""));
        // A commented-out export is not an addressable value.
        assert_eq!(env.value("GTK_THEME"), None);
        // An absent key returns None, not a panic.
        assert_eq!(env.value("NOT_PRESENT"), None);
    }

    #[test]
    fn round_trips_edge_cases() {
        // The split/emit contract must survive emptiness, a missing trailing
        // newline, a comment-only file, blank lines, and CRLF endings.
        for input in [
            "",
            "export XCURSOR_SIZE=16",
            "# only a comment\n",
            "\n\n",
            "export XCURSOR_THEME=Nordic-cursors\r\nexport XCURSOR_SIZE=16\r\n",
        ] {
            let (env, _) = EnvFile::parse(input);
            assert_eq!(env.emit(), input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn editing_xcursor_theme_changes_exactly_that_value_span() {
        // Accept criterion: editing an existing export rewrites only that one
        // value span, leaving the `export ` keyword, key, `=`, and every other
        // line (including the quoted QT line) byte-identical.
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);
        let outcome = env
            .set_value("XCURSOR_THEME", "Adwaita")
            .expect("XCURSOR_THEME exists");
        assert_eq!(outcome, SetOutcome::Edited);

        let edited = env.emit();
        let differing = differing_line_indices(UWSM_ENV_COMMENTED, &edited);
        let theme_index = UWSM_ENV_COMMENTED
            .lines()
            .position(|line| line == "export XCURSOR_THEME=Nordic-cursors")
            .expect("fixture contains the cursor theme export");
        assert_eq!(
            differing,
            vec![theme_index],
            "exactly the XCURSOR_THEME line may change"
        );
        assert_eq!(
            edited.lines().nth(theme_index),
            Some("export XCURSOR_THEME=Adwaita"),
            "only the value span changed: `export `, the key, and `=` are preserved"
        );
        assert_eq!(env.value("XCURSOR_THEME"), Some("Adwaita"));
    }

    #[test]
    fn editing_xcursor_size_changes_exactly_that_value_span() {
        // The size key is edited the same way — value-only, single line.
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);
        env.set_value("XCURSOR_SIZE", "24")
            .expect("XCURSOR_SIZE exists");

        let edited = env.emit();
        let differing = differing_line_indices(UWSM_ENV_COMMENTED, &edited);
        let size_index = UWSM_ENV_COMMENTED
            .lines()
            .position(|line| line == "export XCURSOR_SIZE=16")
            .expect("fixture contains the cursor size export");
        assert_eq!(differing, vec![size_index]);
        assert_eq!(
            edited.lines().nth(size_index),
            Some("export XCURSOR_SIZE=24")
        );
    }

    #[test]
    fn a_bare_assignment_is_recognized_edited_and_not_flagged() {
        // uwsm sources this file with allexport, so a bare `KEY=value` is an
        // exported variable, semantically identical to `export KEY=value`. It must
        // round-trip WITHOUT a warning and be editable value-only, preserving the
        // lack of the `export` keyword.
        let input = "XCURSOR_THEME=Nordic-cursors\nexport XCURSOR_SIZE=16\n";
        let (mut env, warnings) = EnvFile::parse(input);
        assert!(
            warnings.is_empty(),
            "a bare assignment is well-formed, not malformed, got {warnings:?}"
        );
        assert_eq!(env.emit(), input, "the bare assignment round-trips");
        assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic-cursors"));

        let outcome = env
            .set_value("XCURSOR_THEME", "Adwaita")
            .expect("a bare assignment is editable");
        assert_eq!(outcome, SetOutcome::Edited);
        assert_eq!(
            env.emit(),
            "XCURSOR_THEME=Adwaita\nexport XCURSOR_SIZE=16\n",
            "only the value span changed; the bare form (no `export`) is preserved"
        );
    }

    #[test]
    fn editing_preserves_surrounding_whitespace() {
        // The value span excludes indentation, spaces around the value, and
        // trailing whitespace, so those survive an edit untouched. (Leading
        // indentation on an export is unusual but must still round-trip cleanly.)
        let input = "  export XCURSOR_SIZE= 16  \n";
        let (mut env, warnings) = EnvFile::parse(input);
        assert!(warnings.is_empty());
        env.set_value("XCURSOR_SIZE", "24").expect("valid edit");
        assert_eq!(
            env.emit(),
            "  export XCURSOR_SIZE= 24  \n",
            "only the value bytes change; all surrounding whitespace is preserved"
        );
    }

    #[test]
    fn appending_an_absent_export_lands_at_end_of_file() {
        // Accept criterion: setting a key with no existing assignment appends a new
        // `export KEY=value` line at EOF in the file's established form, leaving
        // every original byte intact.
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);
        let outcome = env
            .set_value("MOZ_ENABLE_WAYLAND", "1")
            .expect("append a new export");
        assert_eq!(outcome, SetOutcome::Appended);

        let edited = env.emit();
        assert!(
            edited.starts_with(UWSM_ENV_COMMENTED),
            "every original byte is preserved and the new export is added after them"
        );
        assert_eq!(
            edited,
            format!("{UWSM_ENV_COMMENTED}export MOZ_ENABLE_WAYLAND=1\n"),
            "the new export lands at EOF in the canonical `export KEY=value` form"
        );
        assert_eq!(env.value("MOZ_ENABLE_WAYLAND"), Some("1"));
    }

    #[test]
    fn appending_repairs_a_missing_final_newline() {
        // When the file's last line lacks a terminator, the append must add one
        // first so the new export does not run onto the previous line.
        let (mut env, _) = EnvFile::parse("export XCURSOR_SIZE=16");
        env.set_value("XCURSOR_THEME", "Nordic-cursors")
            .expect("append");
        assert_eq!(
            env.emit(),
            "export XCURSOR_SIZE=16\nexport XCURSOR_THEME=Nordic-cursors\n"
        );
    }

    #[test]
    fn an_uncommented_gtk_theme_is_reported_as_an_active_override() {
        // Accept criterion (R3.3): an uncommented `export GTK_THEME=…` is a live
        // override the app must not fight — reported as Active with its value.
        let (env, warnings) = EnvFile::parse(UWSM_ENV_ACTIVE_OVERRIDE);
        assert!(warnings.is_empty());
        let override_state = env.gtk_theme_override();
        assert_eq!(
            override_state,
            GtkThemeOverride::Active {
                value: "Nordic-bluish-accent".to_string()
            }
        );
        assert!(
            override_state.is_active(),
            "an uncommented GTK_THEME must disable the GTK-theme drop-down"
        );
        // It is also readable as an ordinary assignment value.
        assert_eq!(env.value("GTK_THEME"), Some("Nordic-bluish-accent"));
    }

    #[test]
    fn a_bare_uncommented_gtk_theme_is_an_active_override() {
        // R3.3: because a bare `KEY=value` is exported under allexport, a bare
        // `GTK_THEME=…` (no `export`) is a LIVE override too. Before this was
        // recognized it would have read as Absent, leaving the drop-down enabled
        // and fighting the override — which CLAUDE.md's R3.3 rule forbids.
        let input = "export XCURSOR_SIZE=16\nGTK_THEME=Nordic\n";
        let (env, warnings) = EnvFile::parse(input);
        assert!(warnings.is_empty(), "a bare assignment is not malformed");
        let override_state = env.gtk_theme_override();
        assert_eq!(
            override_state,
            GtkThemeOverride::Active {
                value: "Nordic".to_string()
            }
        );
        assert!(override_state.is_active());
        assert_eq!(env.value("GTK_THEME"), Some("Nordic"));
    }

    #[test]
    fn a_commented_gtk_theme_is_present_but_commented_and_never_edited() {
        // Accept criterion (R3.3): a commented-out `#export GTK_THEME=…` is present
        // but inactive — reported as Commented, `is_active()` false — and a
        // value-set call must NEVER touch the comment. Setting GTK_THEME (which the
        // app never actually does) appends a fresh export and leaves the comment
        // byte-identical.
        let (mut env, warnings) = EnvFile::parse(UWSM_ENV_COMMENTED);
        assert!(warnings.is_empty());
        let override_state = env.gtk_theme_override();
        assert_eq!(
            override_state,
            GtkThemeOverride::Commented {
                value: "Nordic-bluish-accent".to_string()
            }
        );
        assert!(
            !override_state.is_active(),
            "a commented-out GTK_THEME must not disable the drop-down"
        );

        // A value-set never matches the comment: it appends instead, and the
        // commented line stays exactly as written.
        let outcome = env
            .set_value("GTK_THEME", "SomethingElse")
            .expect("append, not edit");
        assert_eq!(outcome, SetOutcome::Appended);
        assert!(
            env.emit()
                .contains("#export GTK_THEME=Nordic-bluish-accent"),
            "the commented-out line must be preserved byte-identically"
        );
        assert_eq!(
            env.emit(),
            format!("{UWSM_ENV_COMMENTED}export GTK_THEME=SomethingElse\n"),
        );
    }

    #[test]
    fn a_spaced_comment_hash_still_detects_the_commented_override() {
        // `# export GTK_THEME=…` (space after `#`) must also be recognized as a
        // commented-out override, not just the `#export …` form.
        let input = "# export GTK_THEME=Nordic-bluish-accent\nexport XCURSOR_SIZE=16\n";
        let (env, _) = EnvFile::parse(input);
        assert_eq!(
            env.gtk_theme_override(),
            GtkThemeOverride::Commented {
                value: "Nordic-bluish-accent".to_string()
            }
        );
    }

    #[test]
    fn a_commented_bare_gtk_theme_is_reported_commented() {
        // A commented-out bare assignment (`# GTK_THEME=…`, no `export`) is present
        // but inactive — detected the same as the `#export …` form.
        let input = "# GTK_THEME=Nordic\nexport XCURSOR_SIZE=16\n";
        let (env, _) = EnvFile::parse(input);
        assert_eq!(
            env.gtk_theme_override(),
            GtkThemeOverride::Commented {
                value: "Nordic".to_string()
            }
        );
    }

    #[test]
    fn an_absent_gtk_theme_is_reported_absent() {
        let input = "export XCURSOR_THEME=Nordic-cursors\nexport XCURSOR_SIZE=16\n";
        let (env, _) = EnvFile::parse(input);
        assert_eq!(env.gtk_theme_override(), GtkThemeOverride::Absent);
        assert!(!env.gtk_theme_override().is_active());
    }

    #[test]
    fn an_active_override_wins_over_a_commented_line() {
        // With a commented-out line BEFORE an active GTK_THEME, the active one
        // wins — the app must treat the override as live.
        let input = "\
#export GTK_THEME=OldValue
export GTK_THEME=LiveValue
";
        let (env, _) = EnvFile::parse(input);
        assert_eq!(
            env.gtk_theme_override(),
            GtkThemeOverride::Active {
                value: "LiveValue".to_string()
            }
        );
    }

    #[test]
    fn an_active_override_wins_over_a_later_commented_line() {
        // Order-independence: an active export appearing BEFORE a commented-out
        // line still wins (the reverse of the commented-then-active case above).
        let input = "\
export GTK_THEME=LiveValue
#export GTK_THEME=OldValue
";
        let (env, _) = EnvFile::parse(input);
        assert_eq!(
            env.gtk_theme_override(),
            GtkThemeOverride::Active {
                value: "LiveValue".to_string()
            }
        );
    }

    #[test]
    fn a_malformed_line_is_preserved_and_warned_without_panicking() {
        // Accept criterion: an uninterpretable line (no `=`) surfaces as a parse
        // warning, not a panic, and is preserved losslessly. A bare `KEY=value`
        // line, by contrast, is a valid (allexport-exported) assignment and must
        // NOT be flagged.
        let input = "\
export XCURSOR_THEME=Nordic-cursors
BARE_ASSIGNMENT=exported_by_allexport
this line is not an assignment
export XCURSOR_SIZE=16
";
        let (mut env, warnings) = EnvFile::parse(input);

        // Lossless: every byte is preserved, the malformed line included.
        assert_eq!(env.emit(), input);

        // Only the no-`=` line is unrecognized; the bare assignment is not flagged.
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 3);
        assert_eq!(warnings[0].kind(), &ParseWarningKind::MalformedLine);

        // The bare assignment is a real, addressable exported variable.
        assert_eq!(env.value("BARE_ASSIGNMENT"), Some("exported_by_allexport"));

        // The real exports around the malformed line stay editable without panic.
        assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic-cursors"));
        env.set_value("XCURSOR_THEME", "Adwaita")
            .expect("still editable around a malformed line");
        assert_eq!(
            env.emit(),
            input.replace(
                "export XCURSOR_THEME=Nordic-cursors",
                "export XCURSOR_THEME=Adwaita"
            )
        );
    }

    #[test]
    fn set_value_rejects_unsafe_input_without_changing_the_file() {
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);

        // A value with a newline would split the line — rejected (R8.3).
        assert_eq!(
            env.set_value("XCURSOR_THEME", "Nord\nic"),
            Err(EditError::InvalidValue("Nord\nic".to_string()))
        );
        // Appending with a key that is not a shell identifier is rejected.
        assert_eq!(
            env.set_value("2BAD", "x"),
            Err(EditError::InvalidKey("2BAD".to_string()))
        );
        assert_eq!(
            env.set_value("BAD-KEY", "x"),
            Err(EditError::InvalidKey("BAD-KEY".to_string()))
        );

        // None of the rejected edits changed the file.
        assert_eq!(env.emit(), UWSM_ENV_COMMENTED);
    }

    #[test]
    fn a_quoted_value_round_trips_and_is_untouched_by_an_unrelated_edit() {
        // A pre-existing quoted value must survive byte-for-byte when a different
        // key is edited (the app never rewrites this key), documenting that this
        // parser does not strip or synthesize shell quoting.
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);
        env.set_value("XCURSOR_SIZE", "24")
            .expect("edit an unrelated key");
        assert!(
            env.emit()
                .contains("export QT_QPA_PLATFORM=\"wayland;xcb\""),
            "the quoted value is preserved verbatim across an unrelated edit"
        );
    }

    #[test]
    fn editing_a_quoted_value_replaces_the_whole_span_including_quotes() {
        // N1: when the existing value is quoted, the value span includes the
        // quotes, so set_value replaces the entire quoted token (this parser does
        // not peel quotes) and touches nothing else on the line or in the file.
        let (mut env, _) = EnvFile::parse(UWSM_ENV_COMMENTED);
        assert_eq!(env.value("QT_QPA_PLATFORM"), Some("\"wayland;xcb\""));

        let outcome = env
            .set_value("QT_QPA_PLATFORM", "wayland")
            .expect("the quoted key is editable");
        assert_eq!(outcome, SetOutcome::Edited);

        let edited = env.emit();
        let differing = differing_line_indices(UWSM_ENV_COMMENTED, &edited);
        let qt_index = UWSM_ENV_COMMENTED
            .lines()
            .position(|line| line == "export QT_QPA_PLATFORM=\"wayland;xcb\"")
            .expect("fixture contains the quoted QT export");
        assert_eq!(
            differing,
            vec![qt_index],
            "exactly the quoted line may change"
        );
        assert_eq!(
            edited.lines().nth(qt_index),
            Some("export QT_QPA_PLATFORM=wayland"),
            "the whole quoted span (including the quotes) is replaced by the new value"
        );
        assert_eq!(env.value("QT_QPA_PLATFORM"), Some("wayland"));
    }

    #[test]
    fn crlf_endings_round_trip_and_edits_keep_them() {
        // A file with Windows `\r\n` endings must round-trip byte-for-byte and an
        // edit must change only the value bytes, never dragging the `\r` into the
        // value span. An append on a CRLF file uses CRLF for the new line too.
        let input = "export XCURSOR_THEME=Nordic-cursors\r\nexport XCURSOR_SIZE=16\r\n";
        let (mut env, warnings) = EnvFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(env.emit(), input, "CRLF endings must be preserved");
        assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic-cursors"));

        env.set_value("XCURSOR_THEME", "Adwaita")
            .expect("valid edit");
        assert_eq!(
            env.emit(),
            "export XCURSOR_THEME=Adwaita\r\nexport XCURSOR_SIZE=16\r\n"
        );

        env.set_value("GDK_BACKEND", "wayland")
            .expect("append keeps CRLF");
        assert_eq!(
            env.emit(),
            "export XCURSOR_THEME=Adwaita\r\nexport XCURSOR_SIZE=16\r\nexport GDK_BACKEND=wayland\r\n"
        );
    }

    #[test]
    fn value_and_set_value_target_the_last_assignment_of_a_duplicate() {
        // A variable assigned twice is resolved by the shell to the LAST
        // assignment, so `value()` reads the last and `set_value` rewrites the
        // last, leaving the earlier shadowed copy byte-identical.
        let input = "export XCURSOR_SIZE=16\nexport XCURSOR_SIZE=24\n";
        let (mut env, _) = EnvFile::parse(input);
        assert_eq!(env.value("XCURSOR_SIZE"), Some("24"));

        env.set_value("XCURSOR_SIZE", "32")
            .expect("last occurrence is editable");
        assert_eq!(
            env.emit(),
            "export XCURSOR_SIZE=16\nexport XCURSOR_SIZE=32\n",
            "the last assignment is rewritten; the shadowed first stays byte-identical"
        );
        assert_eq!(env.value("XCURSOR_SIZE"), Some("32"));
    }

    #[test]
    fn the_export_keyword_must_be_a_whole_word() {
        // `exported=value` is a bare assignment to a variable literally named
        // `exported`, not an `export` statement exporting a variable named `value`.
        // The `export` keyword is only consumed when it is a whole word followed by
        // whitespace, so this parses as (and is editable as) `exported`.
        let (mut env, warnings) = EnvFile::parse("exported=value\n");
        assert!(warnings.is_empty(), "a bare assignment is well-formed");
        assert_eq!(env.value("exported"), Some("value"));
        assert_eq!(env.value("value"), None);
        env.set_value("exported", "changed")
            .expect("the bare assignment is editable");
        assert_eq!(env.emit(), "exported=changed\n");
    }

    #[test]
    fn an_export_separated_by_a_tab_is_recognized_and_editable() {
        // N2: the `export` keyword may be followed by a tab, not just a space —
        // `skip_ascii_ws` handles both. The value span must be located correctly
        // and the tab preserved on edit.
        let input = "export\tXCURSOR_SIZE=16\n";
        let (mut env, warnings) = EnvFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(env.value("XCURSOR_SIZE"), Some("16"));
        env.set_value("XCURSOR_SIZE", "24")
            .expect("a tab-separated export is editable");
        assert_eq!(env.emit(), "export\tXCURSOR_SIZE=24\n");
    }

    #[test]
    fn a_hash_inside_a_value_is_kept_as_part_of_the_value() {
        // Shell does not treat `#` as an inline comment inside an unquoted
        // assignment word, so a `#` after the `=` is part of the value: it
        // round-trips and is included when read and when written.
        let input = "export XCURSOR_THEME=Nordic#dark\n";
        let (mut env, warnings) = EnvFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(env.emit(), input);
        assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic#dark"));

        env.set_value("XCURSOR_THEME", "Other#value")
            .expect("`#` is allowed inside a value");
        assert_eq!(env.emit(), "export XCURSOR_THEME=Other#value\n");
    }
}
