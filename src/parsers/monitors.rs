//! Surgical parser and writer for Hyprland's `monitors.conf` (task 3.3;
//! architecture §3 monitors row; R5.3 item 1, R6.1).
//!
//! # What this file is
//!
//! `config/hypr/monitors.conf` is a line-oriented list of `monitor=` records,
//! `source=`d from `hyprland.conf`, that configures every output. Each record is
//! the key `monitor` followed by comma-separated positional fields:
//!
//! ```text
//! monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10
//! monitor=desc:AU Optronics 0x2036,disable
//! monitor=,preferred,auto,1
//! ```
//!
//! The positional fields, after the `monitor=` key, are:
//!
//! 1. **name / description** — the output name (`eDP-1`), a `desc:<EDID>` match,
//!    or empty for the catch-all rule that matches any output;
//! 2. **mode** — `WxH@Hz` (or `preferred`);
//! 3. **position** — `XxY` (or `auto`);
//! 4. **scale** — e.g. `1` or `1.333333`;
//! 5. onward — any **extras** (`bitdepth,10`, `vrr,1`, `transform,1`, …), each
//!    itself one or more comma fields.
//!
//! A record may instead take the **disable form** `monitor=<name>,disable`,
//! which turns the output off. This module treats the disable form as
//! "enabled = off"; every other record is "enabled = on" with the fields above.
//!
//! Hyprland applies `monitor=` rules **in file order, last matching rule wins**.
//! This parser never reorders records, so that precedence is preserved, and a
//! newly created record is appended *after the last existing `monitor=` line* so
//! it wins for its output (architecture §3, R5.3 item 1).
//!
//! # Why a surgical, lossless parser (not a serializer)
//!
//! `monitors.conf` is hand-maintained: it carries comments, blank-line grouping,
//! and per-machine notes. The architecture's hard rule for every config parser
//! (architecture §3) is to **never regenerate a file from a model**; instead we
//! keep a lossless line representation and, when asked to change a value, rewrite
//! *only* that value's byte span and re-emit every other byte identically. The
//! headline guarantee, covered by tests, is **round-trip identity**:
//! [`MonitorsFile::parse`] then [`MonitorsFile::emit`] with no edit reproduces
//! the input byte-for-byte, comments and ordering included.
//!
//! # Keeping edits awk-parseable (CRITICAL)
//!
//! `scripts/hypr-display-profile.sh` is now the *single source* for the eDP
//! panel's mode and scale: it derives them by `awk`-parsing this file's matching
//! `monitor=` record, splitting it on commas and reading the **name from field 1,
//! the mode from field 2, and the scale from field 4** (analysis §6.2). The app
//! edits the `monitor=` record and never touches that script, so an edit must
//! keep the record parseable by that awk. This module guarantees it by:
//!
//! - **never reflowing or reordering fields** — a value edit replaces exactly one
//!   field's byte span in place, so the leading `monitor=<name>` token stays field
//!   1, the mode stays field 2, and the scale stays field 4;
//! - **rejecting a comma (or newline / `#`) inside any written value**
//!   ([`EditError::InvalidValue`]) — a comma would split into an extra positional
//!   field and shift everything after it, desyncing the awk's field numbering.
//!
//! Semantic validation of the *contents* of a field (that a mode really is
//! `WxH@Hz`, that a scale is in range) is the typed settings model's job (task
//! 4.1); this parser only guards the byte-level positional structure the script
//! depends on (R8.3).
//!
//! # Relationship to the hyprlang parser
//!
//! `monitors.conf` is, strictly, a hyprlang file in which `monitor` is a
//! repeatable top-level key, so it could ride on [`crate::parsers::hyprlang`].
//! It does not, deliberately: hyprlang addresses a repeatable key by its *first*
//! comma-field and can only rewrite the value portion after that field as one
//! opaque blob. The Display page needs the opposite — to match a record by field
//! 1 (the name) and edit *arbitrary positional fields* (mode = 2, position = 3,
//! scale = 4), plus toggle the disable form and append whole records. That
//! positional, awk-shaped model is specific to `monitors.conf`, so it lives here
//! as a focused parser rather than bloating the general hyprlang writer. The two
//! share only the lossless line idiom (raw bytes + a `LineKind`), which every
//! parser in this crate re-implements locally for self-containment.

use std::fmt;

/// A parsed `monitors.conf` that can re-emit itself byte-for-byte and edit
/// individual monitor records in place.
///
/// Built by [`MonitorsFile::parse`]. Internally it is just the file's lines in
/// order, each classified and — for a `monitor=` record — annotated with the
/// byte spans of its comma-separated fields. Emitting concatenates the raw line
/// texts, so an unedited file reproduces its input exactly.
#[derive(Clone, Debug)]
pub(crate) struct MonitorsFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity), and the
    /// order encodes Hyprland's later-rule-wins precedence, so it is never
    /// rearranged.
    lines: Vec<Line>,
}

/// One physical line of the file, kept verbatim for lossless re-emission.
#[derive(Clone, Debug)]
struct Line {
    /// The exact original bytes of this line **including its terminator**
    /// (`\n` or `\r\n`, or none for a final line with no trailing newline).
    /// This is what [`MonitorsFile::emit`] writes back; it is only ever mutated
    /// by an edit, which splices a field span, or by an append, which pushes a
    /// wholly new line.
    raw: String,
    /// How this line was classified during parsing.
    kind: LineKind,
}

/// The classification of a single line.
///
/// Only [`LineKind::Record`] lines are addressable by the read and edit methods;
/// every other kind is preserved verbatim and never matched (so a commented-out
/// `# monitor=…` can never be mistaken for a real record).
#[derive(Clone, Debug, PartialEq, Eq)]
enum LineKind {
    /// A line that is empty or only whitespace.
    Blank,
    /// A comment: the first non-whitespace character is `#`. A commented-out
    /// record (`# monitor=eDP-1,…`) is a comment, never a record.
    Comment,
    /// A `monitor=` record. The byte spans of its comma-separated fields within
    /// [`Line::raw`] are recorded so individual fields can be read and rewritten
    /// in place; the fields' trimmed text is what those spans point at.
    Record {
        /// Field 1 (the name / description), trimmed, used to match a target
        /// (`eDP-1`, `desc:AU Optronics 0x2036`, or `` for the catch-all).
        name: String,
        /// Byte ranges `(start, end)` within [`Line::raw`] of each field's
        /// trimmed value, in order: index 0 = name, 1 = mode (or the literal
        /// `disable`), 2 = position, 3 = scale, 4.. = extras. Always contains at
        /// least the name field. An edit splices bytes into one of these spans.
        fields: Vec<(usize, usize)>,
        /// Whether this is the disable form (`<name>,disable`): exactly two
        /// fields whose second is `disable`. Such a record has no mode / position
        /// / scale to read or edit.
        disabled: bool,
    },
    /// A non-blank, non-comment line that is not a `monitor=` record (some other
    /// directive that may legitimately appear in the file). Preserved verbatim
    /// and never edited; not a warning, since only `monitor=` records are this
    /// parser's concern.
    Other,
}

/// Which positional field of an *enabled* record an edit or read targets.
///
/// The numbering matches the awk contract in `scripts/hypr-display-profile.sh`
/// (analysis §6.2): mode is field 2, position field 3, scale field 4 — see the
/// module-level "Keeping edits awk-parseable" note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MonitorField {
    /// The mode, `WxH@Hz` or `preferred` (field 2).
    Mode,
    /// The position, `XxY` or `auto` (field 3).
    Position,
    /// The scale, e.g. `1.333333` (field 4).
    Scale,
}

impl MonitorField {
    /// The 0-based index of this field among the record's comma-separated fields
    /// (field 1 = name is index 0, so mode is index 1, and so on).
    fn index(self) -> usize {
        match self {
            MonitorField::Mode => 1,
            MonitorField::Position => 2,
            MonitorField::Scale => 3,
        }
    }

    /// A short human-readable name, for diagnostics.
    fn label(self) -> &'static str {
        match self {
            MonitorField::Mode => "mode",
            MonitorField::Position => "position",
            MonitorField::Scale => "scale",
        }
    }
}

/// The desired configuration of a monitor, used by
/// [`MonitorsFile::set_state`] to enable, disable, or create a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MonitorState {
    /// The output is on, with an explicit mode, position, and scale. Written as
    /// `monitor=<name>,<mode>,<position>,<scale>`. Any pre-existing trailing
    /// extras on the edited record are dropped, since this rewrites the whole
    /// body after the name; the surgical [`MonitorsFile::set_field`] is the way
    /// to change one field while preserving extras.
    Active {
        /// The mode (`WxH@Hz` or `preferred`).
        mode: String,
        /// The position (`XxY` or `auto`).
        position: String,
        /// The scale (e.g. `1.333333`).
        scale: String,
    },
    /// The output is off, written as `monitor=<name>,disable`.
    Disabled,
}

/// Whether [`MonitorsFile::set_state`] edited an existing record or appended a
/// new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    /// A record with the target name already existed and was rewritten in place.
    Edited,
    /// No record matched, so a new one was appended after the last `monitor=`
    /// line.
    Appended,
}

/// A non-fatal problem noticed while parsing `monitors.conf`.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (task 3.3
/// acceptance: malformed / unexpected `monitor=` lines are preserved losslessly
/// and surfaced as warnings, not panics). [`MonitorsFile::parse`] returns the
/// collected warnings *and* logs each at `warn`.
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
            ParseWarningKind::UnexpectedRecordShape { field_count } => write!(
                f,
                "line {}: `monitor=` record has {field_count} comma field(s); \
                 expected `<name>,disable` or `<name>,mode,position,scale[,extras]` \
                 (at least four fields)",
                self.line
            ),
        }
    }
}

/// The specific reason a line produced a [`ParseWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParseWarningKind {
    /// A `monitor=` line that is neither the disable form (`<name>,disable`) nor
    /// a full record with at least the four `name,mode,position,scale` fields the
    /// awk in `scripts/hypr-display-profile.sh` expects. It is kept byte-for-byte
    /// and still enumerated, but reading its mode / position / scale may return
    /// nothing.
    UnexpectedRecordShape {
        /// How many comma-separated fields the record actually has.
        field_count: usize,
    },
}

/// A failure from an edit method ([`MonitorsFile::set_field`] /
/// [`MonitorsFile::set_state`]).
///
/// Every variant leaves the file completely unchanged: the check happens before
/// any byte is spliced, so a rejected edit can never partially rewrite a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditError {
    /// No `monitor=` record whose name field equals the target exists.
    /// [`MonitorsFile::set_field`] returns this rather than creating a record,
    /// because a single field is not enough to build a valid one; use
    /// [`MonitorsFile::set_state`], which appends, to create a monitor.
    NoSuchMonitor(String),
    /// The targeted record is in the disable form, so it has no mode / position /
    /// scale field to edit. Re-enable it with [`MonitorsFile::set_state`] first.
    RecordDisabled(String),
    /// The targeted record exists and is enabled but does not have the requested
    /// positional field (it is shorter than a full `name,mode,position,scale`
    /// record — the parser will have warned about its shape).
    FieldMissing {
        /// The monitor name.
        monitor: String,
        /// The field that was requested.
        field: MonitorField,
    },
    /// The requested value contains a character that would break the record's
    /// positional structure: a comma (would create an extra field and desync the
    /// awk in `scripts/hypr-display-profile.sh`), or a newline / carriage return /
    /// `#` (would split the line or start an inline comment). Rejecting it upholds
    /// R8.3 and the module-level awk-parseability guarantee.
    InvalidValue(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoSuchMonitor(name) => {
                write!(f, "no `monitor=` record named `{name}` to edit")
            }
            EditError::RecordDisabled(name) => write!(
                f,
                "the `monitor=` record named `{name}` is disabled and has no field to edit"
            ),
            EditError::FieldMissing { monitor, field } => write!(
                f,
                "the `monitor=` record named `{monitor}` has no {} field",
                field.label()
            ),
            EditError::InvalidValue(value) => write!(
                f,
                "`{value}` contains a comma, newline, or `#`, which would break the record's \
                 positional (awk-parseable) structure"
            ),
        }
    }
}

impl std::error::Error for EditError {}

impl MonitorsFile {
    /// Parses `monitors.conf` text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved so [`emit`](Self::emit) always
    /// reproduces the input byte-for-byte. A `monitor=` line whose field shape is
    /// unexpected is still parsed and kept, and reported as a [`ParseWarning`]
    /// (sorted by line number) and additionally logged at `warn`; it is not an
    /// error and does not stop parsing. Warnings carry only line numbers and a
    /// field count, never file contents (R7.3).
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

        for warning in &warnings {
            // Surface each problem in the journal without dumping file contents
            // (R7.3): the message carries only the line number and field count.
            tracing::warn!(warning = %warning, "monitors.conf parse warning");
        }

        (MonitorsFile { lines }, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After edits, the output is identical to the input except within the edited
    /// field spans and any record appended by [`set_state`](Self::set_state).
    pub(crate) fn emit(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.raw);
        }
        out
    }

    /// The name field of every `monitor=` record, in file order (which is
    /// Hyprland's later-rule-wins order). The catch-all rule contributes an empty
    /// string. Lets a caller enumerate the configured outputs without guessing
    /// names.
    pub(crate) fn record_names(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match &line.kind {
                LineKind::Record { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether the record named `name` is enabled (`Some(true)`) or in the
    /// disable form (`Some(false)`), or `None` if no such record exists.
    pub(crate) fn is_enabled(&self, name: &str) -> Option<bool> {
        let index = self.find_record(name)?;
        match &self.lines[index].kind {
            LineKind::Record { disabled, .. } => Some(!*disabled),
            _ => None,
        }
    }

    /// Reads the mode / position / scale of the record named `name`.
    ///
    /// Returns `None` if there is no such record, the record is disabled (it has
    /// no such field), or the record is too short to have the requested field.
    /// If several records share the name the last (effective, later-rule-wins)
    /// one is read.
    pub(crate) fn field(&self, name: &str, field: MonitorField) -> Option<&str> {
        let index = self.find_record(name)?;
        match &self.lines[index].kind {
            LineKind::Record {
                fields, disabled, ..
            } => {
                if *disabled {
                    return None;
                }
                let (start, end) = *fields.get(field.index())?;
                Some(&self.lines[index].raw[start..end])
            }
            _ => None,
        }
    }

    /// Rewrites one field (mode, position, or scale) of the enabled record named
    /// `name`, changing exactly that field's byte span and nothing else.
    ///
    /// This is the surgical edit the Display page uses to change a monitor's
    /// resolution/refresh or scale (task 6.1): the leading `monitor=<name>`
    /// token, every other field (including trailing extras such as `bitdepth,10`),
    /// the commas, any inline comment, the terminator, and every other line stay
    /// byte-identical — so the record remains parseable by the awk in
    /// `scripts/hypr-display-profile.sh` (mode = field 2, scale = field 4).
    ///
    /// If several records share the name the last (effective, later-rule-wins)
    /// one is edited and any shadowed earlier duplicate is left byte-identical; a
    /// well-formed `monitors.conf` has one record per output.
    ///
    /// Errors (each leaving the file untouched):
    /// - [`EditError::InvalidValue`] if `value` contains a comma, newline, or `#`.
    /// - [`EditError::NoSuchMonitor`] if no record has that name — this method
    ///   never creates a record.
    /// - [`EditError::RecordDisabled`] if the record is in the disable form.
    /// - [`EditError::FieldMissing`] if the record is too short to have the field.
    pub(crate) fn set_field(
        &mut self,
        name: &str,
        field: MonitorField,
        value: &str,
    ) -> Result<(), EditError> {
        reject_unsafe_value(value)?;

        let Some(index) = self.find_record(name) else {
            return Err(EditError::NoSuchMonitor(name.to_string()));
        };

        // Resolve the field's byte span before mutating, so the immutable borrow
        // of `kind` ends before `raw` is borrowed mutably.
        let (start, end) = match &self.lines[index].kind {
            LineKind::Record {
                fields, disabled, ..
            } => {
                if *disabled {
                    return Err(EditError::RecordDisabled(name.to_string()));
                }
                match fields.get(field.index()) {
                    Some(&span) => span,
                    None => {
                        return Err(EditError::FieldMissing {
                            monitor: name.to_string(),
                            field,
                        });
                    }
                }
            }
            // `find_record` only ever returns the index of a `Record` line.
            _ => return Err(EditError::NoSuchMonitor(name.to_string())),
        };

        self.lines[index].raw.replace_range(start..end, value);
        self.reclassify(index);
        tracing::debug!(
            monitor = name,
            field = field.label(),
            "rewrote monitor field"
        );
        Ok(())
    }

    /// Enables, disables, or creates the record named `name`.
    ///
    /// If a record with that name exists, its body after the name is rewritten to
    /// reflect `state` — `Active` becomes `,<mode>,<position>,<scale>` (dropping
    /// any trailing extras), `Disabled` becomes `,disable` — touching only that
    /// one line. If no record matches, a new one is **appended after the last
    /// `monitor=` line** so it wins Hyprland's later-rule-wins precedence,
    /// copying the leading `monitor=` token's exact style (spacing around `=`)
    /// from the last existing record; if the file has no record at all, the new
    /// line is added at end-of-file. The returned [`SetOutcome`] says which
    /// happened.
    ///
    /// Note that this parser never assumes the disable form is how the *laptop*
    /// display toggle is applied — that uses the hotplug state file (task 6.1) —
    /// but disable-form editing is supported here for round-trip completeness and
    /// for enabling/disabling non-laptop outputs.
    ///
    /// Errors (each leaving the file untouched):
    /// - [`EditError::InvalidValue`] if any written value (the `Active` fields, or
    ///   the name when appending) contains a comma, newline, or `#`.
    pub(crate) fn set_state(
        &mut self,
        name: &str,
        state: &MonitorState,
    ) -> Result<SetOutcome, EditError> {
        let body = match state {
            MonitorState::Active {
                mode,
                position,
                scale,
            } => {
                reject_unsafe_value(mode)?;
                reject_unsafe_value(position)?;
                reject_unsafe_value(scale)?;
                format!(",{mode},{position},{scale}")
            }
            MonitorState::Disabled => ",disable".to_string(),
        };

        if let Some(index) = self.find_record(name) {
            // Replace everything after the name field (the comma-separated body)
            // with the new body, preserving the `monitor=<name>` prefix and any
            // trailing inline comment / terminator.
            let (name_end, body_end) = match &self.lines[index].kind {
                LineKind::Record { fields, .. } => match (fields.first(), fields.last()) {
                    (Some(&(_, name_end)), Some(&(_, body_end))) => (name_end, body_end),
                    // A record always has at least the name field; unreachable.
                    _ => return Err(EditError::NoSuchMonitor(name.to_string())),
                },
                _ => return Err(EditError::NoSuchMonitor(name.to_string())),
            };

            self.lines[index]
                .raw
                .replace_range(name_end..body_end, &body);
            self.reclassify(index);
            tracing::debug!(monitor = name, "rewrote monitor record body");
            Ok(SetOutcome::Edited)
        } else {
            // Appending writes the name verbatim, so it too must be structurally
            // safe.
            reject_unsafe_value(name)?;
            self.append_record(name, &body);
            tracing::debug!(monitor = name, "appended monitor record");
            Ok(SetOutcome::Appended)
        }
    }

    /// The index of the *last* `monitor=` record whose name equals `name`.
    ///
    /// Hyprland applies `monitor=` rules in file order with the last matching
    /// rule winning, so when several records share a name the last one is the
    /// output's effective configuration. Reads and edits therefore target it (via
    /// `rposition`), not a shadowed earlier duplicate — mirroring how
    /// [`Self::last_record_index`] appends after the last record so a new rule
    /// wins. A well-formed `monitors.conf` has one record per output, so this
    /// only matters for a file that carries duplicates.
    fn find_record(&self, name: &str) -> Option<usize> {
        self.lines
            .iter()
            .rposition(|line| matches!(&line.kind, LineKind::Record { name: n, .. } if n == name))
    }

    /// The index of the last `monitor=` record line, if any — where a new record
    /// is appended after.
    fn last_record_index(&self) -> Option<usize> {
        self.lines
            .iter()
            .rposition(|line| matches!(&line.kind, LineKind::Record { .. }))
    }

    /// Appends a new record `<prefix><name><body>` after the last existing
    /// `monitor=` line (or at end-of-file when there is none), matching the last
    /// record's `monitor=` prefix style.
    fn append_record(&mut self, name: &str, body: &str) {
        let terminator = self.line_terminator().to_string();
        let last_record = self.last_record_index();

        // Copy the leading `monitor` + separator (e.g. `monitor=` or `monitor = `)
        // from the last record so the new line matches the file's style; default
        // to the dotfiles' `monitor=` when the file has no record to copy from.
        let prefix = match last_record {
            Some(index) => match &self.lines[index].kind {
                LineKind::Record { fields, .. } => match fields.first() {
                    Some(&(name_start, _)) => self.lines[index].raw[..name_start].to_string(),
                    None => "monitor=".to_string(),
                },
                _ => "monitor=".to_string(),
            },
            None => "monitor=".to_string(),
        };

        let raw = format!("{prefix}{name}{body}{terminator}");
        let mut discard = Vec::new();
        let kind = classify_line(&raw, 0, &mut discard);
        let new_line = Line { raw, kind };

        match last_record {
            Some(index) => {
                // Guarantee the line we insert after ends with a terminator, so
                // the new record does not run onto it.
                if !self.lines[index].raw.ends_with('\n') {
                    self.lines[index].raw.push_str(&terminator);
                }
                self.lines.insert(index + 1, new_line);
            }
            None => {
                if let Some(last) = self.lines.last_mut() {
                    if !last.raw.ends_with('\n') {
                        last.raw.push_str(&terminator);
                    }
                }
                self.lines.push(new_line);
            }
        }
    }

    /// Recomputes a line's [`LineKind`] after its raw bytes were spliced, so the
    /// recorded field spans and `disabled` flag stay consistent with the new
    /// text. Parse warnings are intentionally discarded here: an edit only ever
    /// produces a well-formed record, and re-warning would be noise.
    fn reclassify(&mut self, index: usize) {
        let mut discard = Vec::new();
        let kind = classify_line(&self.lines[index].raw, index + 1, &mut discard);
        self.lines[index].kind = kind;
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

/// Classifies one raw line (terminator included), recording a parse warning if a
/// `monitor=` record has an unexpected field shape. For a record it computes the
/// byte span of each comma-separated field, expressed as offsets into `raw`.
fn classify_line(raw: &str, line_number: usize, warnings: &mut Vec<ParseWarning>) -> LineKind {
    // Work against the content without its line terminator so the terminator is
    // never mistaken for part of a field. `raw` (with terminator) is what we
    // re-emit; `content` is only for locating tokens.
    let content = strip_terminator(raw);

    let trimmed_start = content.trim_start();
    if trimmed_start.is_empty() {
        return LineKind::Blank;
    }
    if trimmed_start.starts_with('#') {
        // A whole-line comment, including a commented-out record. Checking `#`
        // first means such a line is never treated as a record.
        return LineKind::Comment;
    }

    // Hyprland treats `#` as an inline-comment marker anywhere on a line, so the
    // "code" part excludes any trailing comment. Field spans are computed from the
    // code part, keeping a trailing comment outside every field span (preserved).
    let code = match content.find('#') {
        Some(hash) => &content[..hash],
        None => content,
    };

    // A record is the key `monitor` before the first `=`. Anything else
    // (another directive, or a stray line) is preserved but not our concern.
    let Some(eq) = code.find('=') else {
        return LineKind::Other;
    };
    if code[..eq].trim() != "monitor" {
        return LineKind::Other;
    }

    let (name, fields, disabled) = scan_record(code, eq);

    // Flag a record that is neither the disable form nor a full
    // `name,mode,position,scale[,extras]` record: it would give the awk in
    // hypr-display-profile.sh no mode/scale to read.
    if !disabled && fields.len() < 4 {
        warnings.push(ParseWarning {
            line: line_number,
            kind: ParseWarningKind::UnexpectedRecordShape {
                field_count: fields.len(),
            },
        });
    }

    LineKind::Record {
        name,
        fields,
        disabled,
    }
}

/// Splits a `monitor=` record's `code` (line text with any trailing comment
/// already removed) into its comma-separated fields, returning the trimmed name,
/// the trimmed byte span of every field, and whether it is the disable form.
///
/// The returned spans are offsets into `code`, which is a prefix of the line's
/// `raw`, so they index into `raw` directly.
fn scan_record(code: &str, eq: usize) -> (String, Vec<(usize, usize)>, bool) {
    let mut fields = Vec::new();
    let mut start = eq + 1;

    // Split the post-`=` region on commas, recording each field's trimmed span.
    // There is always at least one field (a trailing/leading empty segment still
    // yields an empty field), so `fields[0]` (the name) always exists.
    loop {
        let rest = &code[start..];
        match rest.find(',') {
            Some(offset) => {
                let end = start + offset;
                fields.push(trim_span(code, start, end));
                start = end + 1;
            }
            None => {
                fields.push(trim_span(code, start, code.len()));
                break;
            }
        }
    }

    let (name_start, name_end) = fields[0];
    let name = code[name_start..name_end].to_string();
    let disabled = fields.len() == 2 && {
        let (start, end) = fields[1];
        &code[start..end] == "disable"
    };

    (name, fields, disabled)
}

/// Returns the trimmed byte span of `code[start..end]`: the range with leading
/// and trailing whitespace excluded, as absolute offsets into `code`.
fn trim_span(code: &str, start: usize, end: usize) -> (usize, usize) {
    let segment = &code[start..end];
    let leading = segment.len() - segment.trim_start().len();
    let trimmed = segment.trim();
    let trimmed_start = start + leading;
    (trimmed_start, trimmed_start + trimmed.len())
}

/// Rejects a value that would break a record's positional structure: a comma
/// (would create an extra field and desync the awk in
/// `scripts/hypr-display-profile.sh`), or a newline / carriage return / `#`
/// (would split the line or start an inline comment). See the module-level
/// "Keeping edits awk-parseable" note.
fn reject_unsafe_value(value: &str) -> Result<(), EditError> {
    if value.chars().any(|c| matches!(c, ',' | '\n' | '\r' | '#')) {
        Err(EditError::InvalidValue(value.to_string()))
    } else {
        Ok(())
    }
}

/// Returns `content` with a trailing `\n` or `\r\n` removed, so token and
/// field-span computation never runs into the line terminator.
fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `monitors.conf` fixture derived from the real dotfiles
    /// (analysis §4, §6.2): header comments, blank-line grouping, an eDP-1 record
    /// with mode/position/scale **and trailing extras** (`bitdepth,10`), a
    /// `desc:`-matched external display in the disable form, the catch-all rule,
    /// and a trailing comment after the last record.
    const MONITORS_CONF: &str = "\
# monitors.conf — per-output configuration; later rules win.
# Single source of truth for the eDP panel's mode and scale:
# scripts/hypr-display-profile.sh awk-parses the eDP-1 record below, splitting on
# commas into name,mode,position,scale[,extras]. Keep every record awk-parseable.

# Built-in laptop panel (10-bit).
monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10

# A docked external display, currently disabled.
monitor=desc:AU Optronics 0x2036,disable

# Fallback for anything else plugged in.
monitor=,preferred,auto,1

# End of monitor configuration.
";

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

    /// Returns the single `monitor=` line of `text` whose comma fields start with
    /// `monitor=<name>` — used to inspect one record's emitted bytes.
    fn record_line<'a>(text: &'a str, name: &str) -> &'a str {
        let needle = format!("monitor={name},");
        text.lines()
            .find(|line| line.starts_with(&needle))
            .unwrap_or_else(|| panic!("no record line for `{name}` in:\n{text}"))
    }

    #[test]
    fn round_trip_identity_on_a_realistic_fixture() {
        // Headline guarantee (R6.1, architecture §3): parse -> emit with no edit
        // reproduces the input byte-for-byte, comments/blanks/order and both the
        // disable form and the catch-all included. A clean fixture warns nothing.
        let (file, warnings) = MonitorsFile::parse(MONITORS_CONF);
        assert_eq!(
            file.emit(),
            MONITORS_CONF,
            "emit must reproduce the input byte-for-byte"
        );
        assert!(
            warnings.is_empty(),
            "a well-formed monitors.conf must yield no warnings, got {warnings:?}"
        );

        // The records (and only the records) are enumerated in file order.
        assert_eq!(
            file.record_names(),
            vec!["eDP-1", "desc:AU Optronics 0x2036", ""],
        );
    }

    #[test]
    fn round_trips_empty_input_and_a_final_line_without_a_newline() {
        for input in [
            "",
            "monitor=eDP-1,preferred,auto,1",
            "# only a comment",
            "\n\n",
            "monitor=,preferred,auto,1\n# tail",
        ] {
            let (file, _) = MonitorsFile::parse(input);
            assert_eq!(file.emit(), input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn reads_fields_and_enabled_state() {
        let (file, _) = MonitorsFile::parse(MONITORS_CONF);

        assert_eq!(
            file.field("eDP-1", MonitorField::Mode),
            Some("2880x1800@120")
        );
        assert_eq!(file.field("eDP-1", MonitorField::Position), Some("0x0"));
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.333333"));
        assert_eq!(file.is_enabled("eDP-1"), Some(true));

        // The disabled external display exposes no mode/scale.
        assert_eq!(file.is_enabled("desc:AU Optronics 0x2036"), Some(false));
        assert_eq!(
            file.field("desc:AU Optronics 0x2036", MonitorField::Mode),
            None
        );

        // The catch-all is matched by its empty name.
        assert_eq!(file.field("", MonitorField::Mode), Some("preferred"));
        assert_eq!(file.field("", MonitorField::Scale), Some("1"));

        // An unknown monitor is simply absent.
        assert_eq!(file.is_enabled("DP-9"), None);
        assert_eq!(file.field("DP-9", MonitorField::Mode), None);
    }

    #[test]
    fn editing_edp_mode_and_scale_touches_only_that_record() {
        // Explicit accept criterion: editing the eDP rule's mode/scale changes
        // only those field(s) and leaves the catch-all, the disable record, and
        // the comments byte-identical. Trailing extras (`bitdepth,10`) survive too.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        file.set_field("eDP-1", MonitorField::Mode, "1920x1200@60")
            .expect("eDP-1 mode is editable");
        file.set_field("eDP-1", MonitorField::Scale, "1.0")
            .expect("eDP-1 scale is editable");

        let edited = file.emit();
        let changed = differing_line_indices(MONITORS_CONF, &edited);
        let target = MONITORS_CONF
            .lines()
            .position(|l| l == "monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10")
            .expect("fixture contains the eDP-1 record");
        assert_eq!(
            changed,
            vec![target],
            "only the eDP-1 record line may change"
        );
        assert_eq!(
            edited.lines().nth(target),
            Some("monitor=eDP-1,1920x1200@60,0x0,1.0,bitdepth,10"),
            "mode (field 2) and scale (field 4) changed; name, position, and extras preserved"
        );
    }

    #[test]
    fn edited_record_stays_awk_parseable() {
        // CRITICAL guarantee: after an edit the record keeps the positional comma
        // structure scripts/hypr-display-profile.sh parses — name in field 1, mode
        // in field 2, scale in field 4, extras after. Replicate that awk field
        // split here (splitting the whole line on commas) on both the original and
        // the edited record.
        let original_fields: Vec<&str> = record_line(MONITORS_CONF, "eDP-1").split(',').collect();
        assert_eq!(
            original_fields[0], "monitor=eDP-1",
            "field 1 is the name token"
        );
        assert_eq!(original_fields[1], "2880x1800@120", "field 2 is the mode");
        assert_eq!(original_fields[3], "1.333333", "field 4 is the scale");

        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        file.set_field("eDP-1", MonitorField::Mode, "1920x1200@60")
            .expect("editable");
        file.set_field("eDP-1", MonitorField::Scale, "1.25")
            .expect("editable");
        let edited = file.emit();

        let fields: Vec<&str> = record_line(&edited, "eDP-1").split(',').collect();
        assert_eq!(fields[0], "monitor=eDP-1", "field 1 still the name token");
        assert_eq!(fields[1], "1920x1200@60", "field 2 still the mode");
        assert_eq!(fields[3], "1.25", "field 4 still the scale");
        assert_eq!(
            &fields[4..],
            &["bitdepth", "10"],
            "the trailing extras keep their positions after the scale"
        );
    }

    #[test]
    fn set_field_rejects_a_comma_to_keep_records_awk_parseable() {
        // A comma in a value would split into a new positional field and desync
        // the awk's mode=field2/scale=field4 numbering, so it is rejected before
        // any byte is written.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Mode, "1920x1200@60,extra"),
            Err(EditError::InvalidValue("1920x1200@60,extra".to_string()))
        );
        // Newlines and `#` are rejected for the same structural reasons.
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Scale, "1.0\n"),
            Err(EditError::InvalidValue("1.0\n".to_string()))
        );
        assert_eq!(
            file.emit(),
            MONITORS_CONF,
            "a rejected edit changes nothing"
        );
    }

    #[test]
    fn set_field_reports_missing_and_disabled_targets_without_changing_the_file() {
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);

        // No record with that name — set_field never creates one.
        assert_eq!(
            file.set_field("DP-9", MonitorField::Mode, "1920x1080@60"),
            Err(EditError::NoSuchMonitor("DP-9".to_string()))
        );
        // A disabled record has no mode field to edit.
        assert_eq!(
            file.set_field(
                "desc:AU Optronics 0x2036",
                MonitorField::Mode,
                "1920x1080@60"
            ),
            Err(EditError::RecordDisabled(
                "desc:AU Optronics 0x2036".to_string()
            ))
        );
        assert_eq!(
            file.emit(),
            MONITORS_CONF,
            "no rejected edit changed the file"
        );
    }

    #[test]
    fn appending_a_new_monitor_inserts_after_the_last_monitor_line() {
        // Accept criterion: editing (via set_state) a monitor that does not exist
        // appends a new `monitor=` record after the last `monitor=` line — here
        // after the catch-all, and before the trailing `# End` comment, proving it
        // is placed after the last record rather than at end-of-file.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        let outcome = file
            .set_state(
                "DP-3",
                &MonitorState::Active {
                    mode: "3840x2160@60".to_string(),
                    position: "2880x0".to_string(),
                    scale: "1.5".to_string(),
                },
            )
            .expect("appending a valid record succeeds");
        assert_eq!(outcome, SetOutcome::Appended);

        let expected = MONITORS_CONF.replace(
            "monitor=,preferred,auto,1\n",
            "monitor=,preferred,auto,1\nmonitor=DP-3,3840x2160@60,2880x0,1.5\n",
        );
        assert_eq!(
            file.emit(),
            expected,
            "the new record sits right after the last monitor= line, all else unchanged"
        );
    }

    #[test]
    fn appending_with_no_existing_record_adds_at_end_of_file() {
        // When the file has no `monitor=` line, the new record is added at the end.
        let (mut file, _) = MonitorsFile::parse("# just a comment\n");
        let outcome = file
            .set_state(
                "eDP-1",
                &MonitorState::Active {
                    mode: "2880x1800@120".to_string(),
                    position: "0x0".to_string(),
                    scale: "1.333333".to_string(),
                },
            )
            .expect("append succeeds");
        assert_eq!(outcome, SetOutcome::Appended);
        assert_eq!(
            file.emit(),
            "# just a comment\nmonitor=eDP-1,2880x1800@120,0x0,1.333333\n"
        );
    }

    #[test]
    fn appending_copies_the_last_records_separator_style() {
        // The appended line matches the file's `monitor` + separator style (here
        // `monitor = ` with spaces) rather than forcing the default `monitor=`.
        let (mut file, _) = MonitorsFile::parse("monitor = eDP-1,2560x1440,0x0,1\n");
        file.set_state("DP-1", &MonitorState::Disabled)
            .expect("append succeeds");
        assert_eq!(
            file.emit(),
            "monitor = eDP-1,2560x1440,0x0,1\nmonitor = DP-1,disable\n"
        );
    }

    #[test]
    fn disabling_an_active_record_collapses_it_to_the_disable_form() {
        // set_state(Disabled) turns an active record into `<name>,disable`,
        // touching only that line.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        let outcome = file
            .set_state("eDP-1", &MonitorState::Disabled)
            .expect("editable");
        assert_eq!(outcome, SetOutcome::Edited);

        let edited = file.emit();
        let changed = differing_line_indices(MONITORS_CONF, &edited);
        let target = MONITORS_CONF
            .lines()
            .position(|l| l == "monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10")
            .expect("fixture contains the eDP-1 record");
        assert_eq!(changed, vec![target], "only the eDP-1 line may change");
        assert_eq!(edited.lines().nth(target), Some("monitor=eDP-1,disable"));
        assert_eq!(file.is_enabled("eDP-1"), Some(false));
        assert_eq!(file.field("eDP-1", MonitorField::Mode), None);
    }

    #[test]
    fn enable_then_disable_round_trips_a_disabled_record() {
        // Enabling the disabled external display and then disabling it again with
        // the same name reproduces the original disable-form record byte-for-byte
        // (it has no extras to lose), so the whole file round-trips.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        let name = "desc:AU Optronics 0x2036";

        file.set_state(
            name,
            &MonitorState::Active {
                mode: "2560x1440@60".to_string(),
                position: "auto".to_string(),
                scale: "1.066667".to_string(),
            },
        )
        .expect("enable succeeds");
        assert_eq!(file.is_enabled(name), Some(true));
        assert_eq!(file.field(name, MonitorField::Scale), Some("1.066667"));
        // The name field is preserved verbatim through the enable rewrite.
        assert_eq!(
            record_line(&file.emit(), name),
            "monitor=desc:AU Optronics 0x2036,2560x1440@60,auto,1.066667"
        );

        file.set_state(name, &MonitorState::Disabled)
            .expect("disable succeeds");
        assert_eq!(
            file.emit(),
            MONITORS_CONF,
            "disable/enable/disable returns the file to its original bytes"
        );
    }

    #[test]
    fn a_malformed_monitor_line_is_preserved_and_warned_without_panicking() {
        // Accept criterion: a `monitor=` line with an unexpected field shape (here
        // only name + mode, no position/scale, not the disable form) surfaces as a
        // warning, not a panic, and is preserved losslessly.
        let input = "\
# external head
monitor=HDMI-A-1,badmode
monitor=,preferred,auto,1
";
        let (file, warnings) = MonitorsFile::parse(input);

        // Lossless: the odd line is kept byte-for-byte.
        assert_eq!(file.emit(), input);

        // Surfaced as a warning on the right (1-based) line, not a panic.
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 2);
        assert_eq!(
            warnings[0].kind(),
            &ParseWarningKind::UnexpectedRecordShape { field_count: 2 }
        );

        // It is still enumerated and enabled; the fields it does have are readable
        // and the ones it lacks are simply absent.
        assert_eq!(file.record_names(), vec!["HDMI-A-1", ""]);
        assert_eq!(file.is_enabled("HDMI-A-1"), Some(true));
        assert_eq!(file.field("HDMI-A-1", MonitorField::Mode), Some("badmode"));
        assert_eq!(file.field("HDMI-A-1", MonitorField::Position), None);
    }

    #[test]
    fn non_monitor_lines_are_never_touched() {
        // A directive that is not a `monitor=` record (nor a comment/blank) is
        // preserved verbatim, not warned, and never enumerated as a record.
        let input = "workspace=1,monitor:eDP-1\nmonitor=eDP-1,preferred,auto,1\n";
        let (file, warnings) = MonitorsFile::parse(input);
        assert!(warnings.is_empty(), "a non-monitor line is not a warning");
        assert_eq!(file.emit(), input);
        assert_eq!(file.record_names(), vec!["eDP-1"]);
    }

    #[test]
    fn a_commented_out_record_is_never_matched() {
        // A `# monitor=…` line is a comment: it is not enumerated, not readable,
        // and cannot be edited (so an edit can never revive a disabled example).
        let input = "# monitor=eDP-1,2880x1800@120,0x0,1\nmonitor=,preferred,auto,1\n";
        let (mut file, _) = MonitorsFile::parse(input);
        assert_eq!(file.record_names(), vec![""]);
        assert_eq!(file.is_enabled("eDP-1"), None);
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Mode, "1920x1080@60"),
            Err(EditError::NoSuchMonitor("eDP-1".to_string()))
        );
        assert_eq!(file.emit(), input, "the commented-out line is untouched");
    }

    #[test]
    fn editing_preserves_a_trailing_inline_comment() {
        // An edit rewrites only the field span, so a trailing inline comment on
        // the record survives untouched.
        let input = "monitor=eDP-1,2880x1800@120,0x0,1.0 # primary panel\n";
        let (mut file, _) = MonitorsFile::parse(input);
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.0"));
        file.set_field("eDP-1", MonitorField::Scale, "1.25")
            .expect("editable");
        assert_eq!(
            file.emit(),
            "monitor=eDP-1,2880x1800@120,0x0,1.25 # primary panel\n",
            "only the scale field changed; the inline comment is preserved"
        );
    }

    #[test]
    fn crlf_line_endings_round_trip_and_survive_an_edit() {
        // A file with Windows `\r\n` endings round-trips byte-for-byte, and an
        // edit changes only the field bytes, never dragging in the carriage return.
        let input = "# CRLF\r\nmonitor=eDP-1,2880x1800@120,0x0,1.0\r\n";
        let (mut file, warnings) = MonitorsFile::parse(input);
        assert!(warnings.is_empty());
        assert_eq!(file.emit(), input, "CRLF endings must be preserved");
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.0"));

        file.set_field("eDP-1", MonitorField::Scale, "1.5")
            .expect("editable");
        assert_eq!(
            file.emit(),
            "# CRLF\r\nmonitor=eDP-1,2880x1800@120,0x0,1.5\r\n"
        );
    }

    #[test]
    fn edits_target_the_last_matching_duplicate_record() {
        // Hyprland applies `monitor=` rules in file order, last matching rule
        // winning, so when two records share a name the LAST is the output's
        // effective configuration. Reads and edits must target it and leave the
        // shadowed earlier duplicate byte-identical.
        let input = "\
monitor=eDP-1,2880x1800@120,0x0,1.0
monitor=eDP-1,1920x1200@60,0x0,1.5
";
        let (mut file, _) = MonitorsFile::parse(input);

        // A read returns the last (effective) record's fields, not the first.
        assert_eq!(
            file.field("eDP-1", MonitorField::Mode),
            Some("1920x1200@60")
        );
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.5"));

        // An edit rewrites the last record; the shadowed first one is untouched.
        file.set_field("eDP-1", MonitorField::Scale, "2.0")
            .expect("editable");
        assert_eq!(
            file.emit(),
            "\
monitor=eDP-1,2880x1800@120,0x0,1.0
monitor=eDP-1,1920x1200@60,0x0,2.0
",
            "only the last (effective) duplicate changed; the first stayed identical"
        );
    }

    #[test]
    fn set_field_preserves_whitespace_around_fields() {
        // Field spans are trimmed, so an edit replaces only the value and leaves
        // the spacing around `=` and around every comma — on both the edited field
        // and the untouched ones — byte-identical.
        let input = "monitor = eDP-1, 2880x1800@120 , 0x0 , 1.5\n";
        let (mut file, _) = MonitorsFile::parse(input);
        assert_eq!(
            file.field("eDP-1", MonitorField::Mode),
            Some("2880x1800@120")
        );

        file.set_field("eDP-1", MonitorField::Mode, "1920x1200@60")
            .expect("editable");
        assert_eq!(
            file.emit(),
            "monitor = eDP-1, 1920x1200@60 , 0x0 , 1.5\n",
            "only the mode value changed; all surrounding whitespace is preserved"
        );
        // The untouched fields still read back with their spans trimmed.
        assert_eq!(file.field("eDP-1", MonitorField::Position), Some("0x0"));
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.5"));
    }

    #[test]
    fn multi_extra_record_keeps_every_extra_in_position_after_a_scale_edit() {
        // A record with several extra pairs: a scale edit must keep scale in field
        // 4 (awk split) and leave every extra field (5..) in its exact position.
        let input = "monitor=eDP-1,2880x1800@120,0x0,1.5,bitdepth,10,vrr,1\n";
        let (mut file, _) = MonitorsFile::parse(input);

        file.set_field("eDP-1", MonitorField::Scale, "1.25")
            .expect("editable");
        let edited = file.emit();

        let fields: Vec<&str> = record_line(&edited, "eDP-1").split(',').collect();
        assert_eq!(fields[0], "monitor=eDP-1", "field 1 still the name token");
        assert_eq!(fields[1], "2880x1800@120", "field 2 still the mode");
        assert_eq!(fields[3], "1.25", "field 4 still the scale");
        assert_eq!(
            &fields[4..],
            &["bitdepth", "10", "vrr", "1"],
            "every extra field (5..8) keeps its exact position"
        );
    }

    #[test]
    fn appending_after_a_record_without_a_trailing_newline() {
        // The last existing record has no trailing newline; appending must
        // terminate it first so the new record starts on its own line, and the
        // result must be well-formed (both records parse back).
        let (mut file, _) = MonitorsFile::parse("monitor=eDP-1,preferred,auto,1");
        let outcome = file
            .set_state(
                "DP-1",
                &MonitorState::Active {
                    mode: "1920x1080@60".to_string(),
                    position: "0x0".to_string(),
                    scale: "1".to_string(),
                },
            )
            .expect("append succeeds");
        assert_eq!(outcome, SetOutcome::Appended);
        assert_eq!(
            file.emit(),
            "monitor=eDP-1,preferred,auto,1\nmonitor=DP-1,1920x1080@60,0x0,1\n"
        );

        // Well-formed: re-parsing sees both records with the expected fields.
        let (reparsed, warnings) = MonitorsFile::parse(&file.emit());
        assert!(warnings.is_empty());
        assert_eq!(reparsed.record_names(), vec!["eDP-1", "DP-1"]);
        assert_eq!(
            reparsed.field("DP-1", MonitorField::Mode),
            Some("1920x1080@60")
        );
    }

    #[test]
    fn set_state_preserves_a_trailing_inline_comment() {
        // set_state rewrites the body after the name up to the last field, so a
        // trailing inline comment on the record survives untouched.
        let input = "monitor=eDP-1,2880x1800@120,0x0,1.0 # primary panel\n";
        let (mut file, _) = MonitorsFile::parse(input);

        file.set_state(
            "eDP-1",
            &MonitorState::Active {
                mode: "1920x1200@60".to_string(),
                position: "0x0".to_string(),
                scale: "1.5".to_string(),
            },
        )
        .expect("editable");
        assert_eq!(
            file.emit(),
            "monitor=eDP-1,1920x1200@60,0x0,1.5 # primary panel\n",
            "the body was rewritten but the inline comment is preserved"
        );
    }
}
