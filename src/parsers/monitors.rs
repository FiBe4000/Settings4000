//! Surgical parser, writer and grammar validator for Hyprland's `monitors.conf`
//! (tasks 3.3, 11.9; architecture §3 monitors row; R5.3 item 1, R6.1, R8.3).
//!
//! # What this file is
//!
//! `config/hypr/monitors.conf` is a line-oriented list of `monitor=` records that
//! configures every output. Each record is the key `monitor` followed by
//! comma-separated positional fields:
//!
//! ```text
//! monitor=,preferred,auto,1
//! monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,10
//! monitor=desc:Example Panel Vendor 0x2036,2560x1440,auto,1
//! ```
//!
//! The positional fields, after the `monitor=` key, are:
//!
//! 1. **output** — the output name (`eDP-1`), a `desc:<EDID description>` match,
//!    or empty for the catch-all rule that matches any output;
//! 2. **mode** — `WxH@Hz` (or `preferred`);
//! 3. **position** — `XxY` (or `auto`);
//! 4. **scale** — e.g. `1` or `1.333333`;
//! 5. onward — **extras**, complete `key,value` pairs (`bitdepth,10`, `vrr,1`, …).
//!
//! Rules apply **in file order, a later matching rule winning**. This parser
//! never reorders records, so that precedence is preserved, and a newly created
//! record is appended *after the last existing `monitor=` line* so it wins for
//! its output (architecture §3, R5.3 item 1).
//!
//! # Lossless, surgical editing
//!
//! `monitors.conf` is hand-maintained — comments, blank-line grouping and
//! per-machine notes — so this module follows the
//! [parser contract](crate::parsers#the-parser-contract) shared by every config
//! parser: [`MonitorsFile::parse`] then [`MonitorsFile::emit`] with no edit
//! reproduces the input byte for byte, and an edit rewrites one record.
//!
//! # The frozen grammar and its two downstream parsers (CRITICAL)
//!
//! Hyprland no longer reads this file itself. Two independent parsers in the
//! dotfiles do (analysis §7.2):
//!
//! - the **monitor loader in `hyprland.lua`** strips `#…` to end of line, takes the
//!   body after `monitor=`, splits it on every `,`, trims each field, maps fields
//!   1–4 to `output`/`mode`/`position`/`scale` and turns fields 5, 6, … into
//!   `key = value` pairs of one `hl.monitor({ … })` call per line, in file order;
//! - the **`awk` in `scripts/hypr-display-profile.sh`** looks up the laptop panel's
//!   rule — the first non-comment record whose body starts with the literal
//!   `<key>,`, where `<key>` is `eDP-1` or a per-machine `desc:` string — and reads
//!   the mode from field 2, the scale from field 4 and the extras after them.
//!
//! Neither parser understands quoting, escaping or anything beyond that shape,
//! so the file's own header declares the format **frozen** as
//! `monitor=<output>,<mode>,<position>,<scale>[,<key>,<value>...]`. This module
//! enforces that grammar as named [`GrammarRule`]s, so the app never writes a
//! record either parser would misread (R8.3 "never break a working desktop"):
//!
//! - **at least the four positional fields, in that order** — a shorter record
//!   leaves the loader to fill in defaults and gives the `awk` no scale;
//! - **no legacy `<output>,disable` form** — the loader would pass `disable` as the
//!   *mode*; Hyprland's mode parser rejects it (an "invalid resolution" config
//!   error) and adds the rule with the output still enabled at its preferred mode,
//!   so the record does not do what it says (checked against Hyprland 0.56's
//!   `hl.monitor` binding; disabling is the `disabled` boolean field there);
//! - **extras in complete `key,value` pairs** — an odd count leaves a key without a
//!   value, and an empty key or value is no pair either;
//! - **no `,` or `#` (nor a line break) inside any value**, the `desc:` string and
//!   the output token included — a comma would split the value into an extra field
//!   and shift every later field, a `#` starts a comment both parsers strip;
//! - **the output token followed directly by `,`** — the `awk` matches the literal
//!   `<key>,`, so `monitor=eDP-1 ,…` would not be found;
//! - **at most one record per laptop-panel key** — the `awk` takes the *first*
//!   matching record and Hyprland effectively the *last*, so two records for one
//!   key give the two parsers different eDP values (see
//!   [`GrammarRule::DuplicateLaptopKey`] for which tokens count as such a key);
//! - **the catch-all (`monitor=,…`) as the first record** — rules are issued in
//!   file order and a later rule overrides an earlier one, so a catch-all after a
//!   specific rule would override it.
//!
//! Every edit method re-validates the record it produced and refuses the edit
//! (leaving the file unchanged) when it breaks a rule; [`MonitorsFile::parse`]
//! reports a hand-edited record that breaks one as a [`ParseWarning`]; and
//! [`MonitorsFile::grammar_violations`] checks a whole file, which the Display
//! model runs at stage time and again on the bytes it is about to write.
//!
//! Semantic validation of the *contents* of a field (that a mode really is
//! `WxH@Hz`, that a scale is in range) is the typed settings model's job
//! ([`crate::core::model`]); this module guards only the structure the two
//! downstream parsers depend on.
//!
//! # Relationship to the hyprlang parser
//!
//! `monitors.conf` once was a hyprlang file in which `monitor` is a repeatable
//! top-level key, so it could have ridden on [`crate::parsers::hyprlang`]. It does
//! not, deliberately: hyprlang addresses a repeatable key by its *first*
//! comma-field and can only rewrite the value portion after that field as one
//! opaque blob. The Display page needs the opposite — to match a record by field
//! 1 (the output) and edit *arbitrary positional fields* (mode = 2, position = 3,
//! scale = 4), and to append whole records — and the frozen grammar above is
//! specific to this file. The two share only the lossless line idiom (raw bytes +
//! a `LineKind`), which every parser in this crate re-implements locally for
//! self-containment.

use std::collections::HashMap;
use std::fmt;

use super::line::{self, RawLine, strip_terminator};

/// The characters that may never appear inside a written value. A comma would
/// split the value into an extra positional field, a `#` starts a comment that
/// both downstream parsers strip to end of line, and a line break would split
/// the record (see the module-level grammar section).
const FORBIDDEN_VALUE_CHARACTERS: [char; 4] = [',', '#', '\n', '\r'];

/// The output-token prefix of a laptop panel's connector name. `hyprland.lua`'s
/// loader treats a token starting with exactly this (case-sensitive) prefix as
/// the laptop panel, and the profile script's generic key is `eDP-1`.
const LAPTOP_CONNECTOR_PREFIX: &str = "eDP-";

/// The prefix of an output token that matches a monitor by its EDID
/// description instead of its connector name.
const DESC_PREFIX: &str = "desc:";

/// A parsed `monitors.conf` that can re-emit itself byte-for-byte and edit
/// individual monitor records in place.
///
/// Built by [`MonitorsFile::parse`]. Internally it is just the file's lines in
/// order, each classified and — for a `monitor=` record — annotated with the
/// byte spans of its comma-separated fields. Emitting concatenates the raw line
/// texts, so an unedited file reproduces its input exactly.
#[derive(Clone, Debug)]
pub struct MonitorsFile {
    /// The file's lines in original order. Concatenating every line's raw text
    /// reproduces the original input exactly (round-trip identity), and the
    /// order encodes the later-rule-wins precedence, so it is never rearranged.
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

impl RawLine for Line {
    fn raw(&self) -> &str {
        &self.raw
    }
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
        /// Field 1 (the output token), trimmed, used to match a target
        /// (`eDP-1`, `desc:AU Optronics 0x2036`, or `` for the catch-all).
        name: String,
        /// Byte ranges `(start, end)` within [`Line::raw`] of each field's
        /// trimmed value, in order: index 0 = output, 1 = mode (or the literal
        /// `disable`), 2 = position, 3 = scale, 4.. = extras. Always contains at
        /// least the output field. An edit splices bytes into one of these spans.
        fields: Vec<(usize, usize)>,
        /// Whether this is the legacy disable form (`<output>,disable`): exactly
        /// two fields whose second is `disable`. The grammar refuses that form
        /// ([`GrammarRule::DisableForm`]), but a hand-edited file may still carry
        /// it, so it is recognised to report the record as disabled rather than
        /// read `disable` as a mode.
        disabled: bool,
    },
    /// A non-blank, non-comment line that is not a `monitor=` record (some other
    /// directive that may legitimately appear in the file). Preserved verbatim
    /// and never edited; not a warning, since both downstream parsers skip such
    /// lines too.
    Other,
}

/// Which positional field of a record an edit or read targets.
///
/// The numbering matches the frozen grammar (see the module docs): mode is field
/// 2, position field 3, scale field 4 — the positions both the `hyprland.lua`
/// loader and the `awk` in `scripts/hypr-display-profile.sh` read them from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorField {
    /// The mode, `WxH@Hz` or `preferred` (field 2).
    Mode,
    /// The position, `XxY` or `auto` (field 3).
    Position,
    /// The scale, e.g. `1.333333` (field 4).
    Scale,
}

impl MonitorField {
    /// The 0-based index of this field among the record's comma-separated fields
    /// (field 1 = output is index 0, so mode is index 1, and so on).
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

/// The three positional values of an enabled record, written by
/// [`MonitorsFile::set_record`] as `monitor=<output>,<mode>,<position>,<scale>`.
///
/// There is deliberately no "disabled" counterpart: the legacy
/// `<output>,disable` form is outside the frozen grammar (see
/// [`GrammarRule::DisableForm`]), so this module has nothing to write for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorRecord {
    /// The mode (`WxH@Hz` or `preferred`).
    pub mode: String,
    /// The position (`XxY` or `auto`).
    pub position: String,
    /// The scale (e.g. `1.333333`).
    pub scale: String,
}

/// Whether [`MonitorsFile::set_record`] edited an existing record or appended a
/// new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// A record with the target output already existed and was rewritten in place.
    Edited,
    /// No record matched, so a new one was appended after the last `monitor=`
    /// line.
    Appended,
}

/// One rule of the frozen `monitors.conf` grammar that a record breaks (analysis
/// §7.2; see the module-level grammar section for the two downstream parsers
/// each rule protects).
///
/// The variants carry line numbers and counts only, never file contents, so a
/// violation can be logged at `warn` without dumping the file (R7.3); their
/// [`Display`](fmt::Display) text names the rule for the user (R8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrammarRule {
    /// The record has fewer than the four positional fields
    /// `output,mode,position,scale`.
    TooFewFields {
        /// How many comma-separated fields the record has.
        field_count: usize,
    },
    /// The record is the legacy `<output>,disable` form. `hyprland.lua`'s loader
    /// would hand `disable` to Hyprland as the mode, which Hyprland rejects with a
    /// config error while keeping the output enabled at its preferred mode.
    DisableForm,
    /// The fields after the scale are not complete `key,value` pairs: their
    /// count is odd (a key without a value, e.g. a trailing `,bitdepth`), or a
    /// key or value is empty.
    IncompleteExtra,
    /// A value contains a character the grammar cannot carry: a `,` (would split
    /// it into an extra field), a `#` (starts a comment), or a line break.
    ForbiddenCharacter {
        /// The offending character.
        character: char,
    },
    /// The output token is followed by whitespace before its comma. The profile
    /// script's `awk` matches the literal `<output>,`, so it would not find the
    /// record.
    SpaceAfterOutput,
    /// A second record for one laptop-panel key. The profile script's `awk` reads
    /// the *first* record of its key and Hyprland applies the last one over it,
    /// so the two would disagree on the panel's mode and scale.
    ///
    /// A laptop-panel key is an exact output token, compared byte for byte as
    /// both parsers do: every `eDP-…` connector name (the loader's laptop test)
    /// and every `desc:` token. A `desc:` token counts because the profile script
    /// keys a machine's panel on its EDID description through a `case` mapping
    /// the app does not read, so any `desc:` record may be that key. `eDP-1` and
    /// a `desc:` rule for the same physical panel are *different* keys — the
    /// script looks up exactly one of them per machine, and the fixture file
    /// legitimately carries both — so they never conflict with each other.
    DuplicateLaptopKey {
        /// The 1-based line of the first record for the same key.
        first_line: usize,
    },
    /// A catch-all record (empty output, `monitor=,…`) is not the first record,
    /// so it would override every specific rule above it.
    CatchAllNotFirst {
        /// The 1-based line of the first record, which precedes the catch-all.
        first_record_line: usize,
    },
}

impl fmt::Display for GrammarRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrammarRule::TooFewFields { field_count } => write!(
                f,
                "a record needs the four fields output,mode,position,scale but has \
                 {field_count}"
            ),
            GrammarRule::DisableForm => write!(
                f,
                "the `<output>,disable` form is not part of the format: hyprland.lua would \
                 pass `disable` to Hyprland as a mode, which leaves the output on"
            ),
            GrammarRule::IncompleteExtra => write!(
                f,
                "the fields after the scale must be complete key,value pairs, but a key \
                 has no value"
            ),
            GrammarRule::ForbiddenCharacter { character } => {
                let why = match character {
                    ',' => "would split it into an extra field",
                    '#' => "would start a comment",
                    _ => "would split the record across lines",
                };
                write!(
                    f,
                    "a value contains `{}`, which {why}",
                    character.escape_debug()
                )
            }
            GrammarRule::SpaceAfterOutput => write!(
                f,
                "the output name must be followed directly by `,` \
                 (hypr-display-profile.sh matches `<output>,` literally)"
            ),
            GrammarRule::DuplicateLaptopKey { first_line } => write!(
                f,
                "a second record for the laptop-panel key already configured on line \
                 {first_line} (hypr-display-profile.sh reads the first, Hyprland applies \
                 the last)"
            ),
            GrammarRule::CatchAllNotFirst { first_record_line } => write!(
                f,
                "the catch-all record (`monitor=,…`) must come first, but the record on \
                 line {first_record_line} precedes it"
            ),
        }
    }
}

impl std::error::Error for GrammarRule {}

/// A [`GrammarRule`] broken on a specific line of a `monitors.conf`, as reported
/// by [`MonitorsFile::grammar_violations`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrammarViolation {
    /// The 1-based line number of the offending record.
    line: usize,
    /// The rule the record breaks.
    rule: GrammarRule,
}

impl GrammarViolation {
    /// The 1-based line number of the offending record.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The rule the record breaks.
    pub fn rule(&self) -> &GrammarRule {
        &self.rule
    }
}

impl fmt::Display for GrammarViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.rule)
    }
}

impl std::error::Error for GrammarViolation {}

/// Checks that `value` — an output token, mode, position, scale or extra about
/// to be written into a record — contains no character the frozen grammar
/// cannot carry (`,`, `#`, or a line break; see [`GrammarRule::ForbiddenCharacter`]).
///
/// This is the value-level half of the grammar, usable before a record exists
/// (a staging layer can refuse a value early); the record- and file-level rules
/// are checked by [`MonitorsFile::grammar_violations`]. A comma inside a value
/// can only be caught here: once written, it is indistinguishable from a field
/// separator.
pub fn check_value(value: &str) -> Result<(), GrammarRule> {
    match line::find_forbidden(value, &FORBIDDEN_VALUE_CHARACTERS) {
        Some(character) => Err(GrammarRule::ForbiddenCharacter { character }),
        None => Ok(()),
    }
}

/// A non-fatal problem noticed while parsing `monitors.conf`.
///
/// Parsing never fails and never loses data: a problematic line is preserved
/// verbatim and reported here instead of aborting or panicking (task 3.3
/// acceptance: malformed / unexpected `monitor=` lines are preserved losslessly
/// and surfaced as warnings, not panics). [`MonitorsFile::parse`] returns the
/// collected warnings *and* logs each at `warn`, so a hand-edited record that
/// breaks the frozen grammar shows up in the journal (task 11.9).
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
            ParseWarningKind::Grammar(rule) => write!(
                f,
                "line {}: `monitor=` record breaks the frozen monitors.conf grammar: {rule}",
                self.line
            ),
        }
    }
}

/// The specific reason a line produced a [`ParseWarning`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseWarningKind {
    /// A `monitor=` record that breaks a rule of the frozen grammar, which
    /// `hyprland.lua`'s loader or the `awk` in `scripts/hypr-display-profile.sh`
    /// would misread. The line is kept byte-for-byte and still enumerated and
    /// readable; whether it may be edited is the caller's policy (the Display
    /// model treats such a file as read-only).
    Grammar(GrammarRule),
}

/// A failure from an edit method ([`MonitorsFile::set_field`] /
/// [`MonitorsFile::set_record`]).
///
/// Every variant leaves the file completely unchanged: the check happens before
/// any byte is spliced, or the splice is undone, so a rejected edit can never
/// partially rewrite a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// No `monitor=` record whose output token equals the target exists.
    /// [`MonitorsFile::set_field`] returns this rather than creating a record,
    /// because a single field is not enough to build a valid one; use
    /// [`MonitorsFile::set_record`], which appends, to create a monitor.
    NoSuchMonitor(String),
    /// The targeted record is in the legacy disable form, so it has no mode /
    /// position / scale field to edit. [`MonitorsFile::set_record`] rewrites it
    /// into a complete record.
    RecordDisabled(String),
    /// The targeted record exists and is enabled but does not have the requested
    /// positional field (it is shorter than a full `output,mode,position,scale`
    /// record — the parser will have warned about its shape).
    FieldMissing {
        /// The monitor's output token.
        monitor: String,
        /// The field that was requested.
        field: MonitorField,
    },
    /// The edit would leave a record that breaks the frozen grammar: a written
    /// value carries a forbidden character, or the resulting record breaks a
    /// record- or file-level rule. Refusing it upholds R8.3 (see the module-level
    /// grammar section).
    Grammar(GrammarRule),
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
            EditError::Grammar(rule) => write!(
                f,
                "the edit would break the frozen monitors.conf format: {rule}"
            ),
        }
    }
}

impl std::error::Error for EditError {}

impl MonitorsFile {
    /// Parses `monitors.conf` text into a lossless, editable representation.
    ///
    /// This never fails: every line is preserved so [`emit`](Self::emit) always
    /// reproduces the input byte-for-byte. Every record that breaks the frozen
    /// grammar ([`Self::grammar_violations`]) is still parsed and kept, and
    /// reported as a [`ParseWarning`] (in line order) that is additionally logged
    /// at `warn`; it is not an error and does not stop parsing. Warnings carry
    /// only line numbers and counts, never file contents ([R7.3][r73]).
    ///
    /// [r73]: crate::parsers#diagnostics-carry-positions-not-contents-r73
    pub fn parse(input: &str) -> (Self, Vec<ParseWarning>) {
        let lines = line::split_raw_lines(input)
            .map(|raw| Line {
                raw: raw.to_string(),
                kind: classify_line(raw),
            })
            .collect();
        let file = MonitorsFile { lines };

        let warnings: Vec<ParseWarning> = file
            .grammar_violations()
            .into_iter()
            .map(|violation| ParseWarning {
                line: violation.line,
                kind: ParseWarningKind::Grammar(violation.rule),
            })
            .collect();
        for warning in &warnings {
            tracing::warn!(warning = %warning, "monitors.conf parse warning");
        }

        (file, warnings)
    }

    /// Re-emits the file as text, byte-for-byte identical to the parsed input
    /// when no edit has been made (round-trip identity).
    ///
    /// After edits, the output is identical to the input except within the edited
    /// field spans and any record appended by [`set_record`](Self::set_record).
    pub fn emit(&self) -> String {
        line::emit(&self.lines)
    }

    /// Every rule of the frozen grammar the file's records break, in line order
    /// (a record breaking two rules is reported twice).
    ///
    /// This is the whole-file check: the record-level rules
    /// ([`GrammarRule::TooFewFields`], [`GrammarRule::DisableForm`],
    /// [`GrammarRule::IncompleteExtra`], [`GrammarRule::SpaceAfterOutput`]) for
    /// each record, plus the file-level ones
    /// ([`GrammarRule::DuplicateLaptopKey`], [`GrammarRule::CatchAllNotFirst`]).
    /// Comments, blank lines and non-`monitor=` lines are never violations. An
    /// empty result means both downstream parsers read every record as intended
    /// (R8.3), within the limits below.
    ///
    /// # Limits
    ///
    /// Two divergences cannot be detected from the file's structure alone:
    ///
    /// - **whitespace inside a mode, scale or extra** (`2560x1440 @60`): the profile
    ///   script's `awk` deletes all whitespace from those fields, while
    ///   `hyprland.lua`'s loader only trims their ends, so the two can read different
    ///   values. The typed validators refuse such a mode or scale when the app writes
    ///   one ([`check_value`] alone does not);
    /// - **a comma inside a `desc:` string that happens to leave an even, non-empty
    ///   number of extras** (`desc:A, B, C 0x1,…` shifts every field by two): the
    ///   record still has the grammar's shape, only with the wrong values in each
    ///   position. Most such commas are caught as a dangling extra or a short record,
    ///   and a value the app writes is checked by [`check_value`] before it exists.
    pub fn grammar_violations(&self) -> Vec<GrammarViolation> {
        let mut violations = Vec::new();
        let mut first_record_line: Option<usize> = None;
        let mut laptop_keys: HashMap<&str, usize> = HashMap::new();

        for (index, line) in self.lines.iter().enumerate() {
            let LineKind::Record {
                name,
                fields,
                disabled,
            } = &line.kind
            else {
                continue;
            };
            let line_number = index + 1;
            let mut report = |rule| {
                violations.push(GrammarViolation {
                    line: line_number,
                    rule,
                })
            };

            if let Some(rule) = record_rule(&line.raw, fields, *disabled) {
                report(rule);
            }

            match first_record_line {
                None => first_record_line = Some(line_number),
                Some(first) if name.is_empty() => report(GrammarRule::CatchAllNotFirst {
                    first_record_line: first,
                }),
                Some(_) => {}
            }

            if is_laptop_key(name) {
                match laptop_keys.get(name.as_str()) {
                    Some(&first_line) => report(GrammarRule::DuplicateLaptopKey { first_line }),
                    None => {
                        laptop_keys.insert(name, line_number);
                    }
                }
            }
        }
        violations
    }

    /// The output token of every `monitor=` record, in file order (which is the
    /// later-rule-wins order). The catch-all rule contributes an empty string.
    /// Lets a caller enumerate the configured outputs without guessing names.
    pub fn record_names(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match &line.kind {
                LineKind::Record { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether the record named `name` is enabled (`Some(true)`) or in the legacy
    /// disable form (`Some(false)`), or `None` if no such record exists.
    pub fn is_enabled(&self, name: &str) -> Option<bool> {
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
    pub fn field(&self, name: &str, field: MonitorField) -> Option<&str> {
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
    /// resolution/refresh or scale (task 6.1): the leading `monitor=<output>`
    /// token, every other field (including trailing extras such as `bitdepth,10`),
    /// the commas, any inline comment, the terminator, and every other line stay
    /// byte-identical — so the record keeps the positional shape both downstream
    /// parsers read.
    ///
    /// If several records share the name the last (effective, later-rule-wins)
    /// one is edited and any shadowed earlier duplicate is left byte-identical.
    ///
    /// Errors (each leaving the file untouched):
    /// - [`EditError::Grammar`] if `value` contains a forbidden character, or the
    ///   edited record would break a record-level grammar rule (the last line of
    ///   defence of R8.3: an edit never leaves a record the downstream parsers
    ///   would misread, whatever the caller validated).
    /// - [`EditError::NoSuchMonitor`] if no record has that name — this method
    ///   never creates a record.
    /// - [`EditError::RecordDisabled`] if the record is in the disable form.
    /// - [`EditError::FieldMissing`] if the record is too short to have the field.
    pub fn set_field(
        &mut self,
        name: &str,
        field: MonitorField,
        value: &str,
    ) -> Result<(), EditError> {
        check_value(value).map_err(EditError::Grammar)?;

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

        self.splice_checked(index, start..end, value)?;
        tracing::debug!(
            monitor = name,
            field = field.label(),
            "rewrote monitor field"
        );
        Ok(())
    }

    /// Writes the record named `name` as `<output>,<mode>,<position>,<scale>`,
    /// creating it if needed.
    ///
    /// If a record with that name exists, its body after the output token is
    /// rewritten to the three values — dropping any trailing extras, since this
    /// replaces the whole body; [`set_field`](Self::set_field) is the way to
    /// change one field while preserving extras — touching only that one line. A
    /// legacy `,disable` record becomes a complete record this way. If no record
    /// matches, a new one is **appended after the last `monitor=` line** so it
    /// wins the later-rule-wins precedence, copying the leading `monitor=`
    /// token's exact style (spacing around `=`) from the last existing record; if
    /// the file has no record at all, the new line is added at end-of-file. The
    /// returned [`SetOutcome`] says which happened.
    ///
    /// The laptop display's on/off switch never comes through here — it runs the
    /// dotfiles' `scripts/hypr-toggle-laptop-display` (task 11.8) and never edits
    /// this file.
    ///
    /// Errors (each leaving the file untouched):
    /// - [`EditError::Grammar`] if any written value (the three values, or the
    ///   output token when appending) contains a forbidden character, or the
    ///   written record would break a grammar rule — e.g. appending a catch-all
    ///   after other records ([`GrammarRule::CatchAllNotFirst`]) or a second
    ///   record for a laptop-panel key ([`GrammarRule::DuplicateLaptopKey`]).
    pub fn set_record(
        &mut self,
        name: &str,
        record: &MonitorRecord,
    ) -> Result<SetOutcome, EditError> {
        for value in [&record.mode, &record.position, &record.scale] {
            check_value(value).map_err(EditError::Grammar)?;
        }
        let body = format!(",{},{},{}", record.mode, record.position, record.scale);

        if let Some(index) = self.find_record(name) {
            // Replace everything after the output field (the comma-separated body)
            // with the new body, preserving the `monitor=<output>` prefix and any
            // trailing inline comment / terminator.
            let (name_end, body_end) = match &self.lines[index].kind {
                LineKind::Record { fields, .. } => match (fields.first(), fields.last()) {
                    (Some(&(_, name_end)), Some(&(_, body_end))) => (name_end, body_end),
                    // A record always has at least the output field; unreachable.
                    _ => return Err(EditError::NoSuchMonitor(name.to_string())),
                },
                _ => return Err(EditError::NoSuchMonitor(name.to_string())),
            };

            self.splice_checked(index, name_end..body_end, &body)?;
            tracing::debug!(monitor = name, "rewrote monitor record body");
            Ok(SetOutcome::Edited)
        } else {
            // Appending writes the output token verbatim, so it too must be
            // structurally safe.
            check_value(name).map_err(EditError::Grammar)?;
            // `append_record` inserts after this line and may give it a terminator
            // first; keep its bytes so a refused append can be undone exactly.
            let anchor = self
                .last_record_index()
                .or(self.lines.len().checked_sub(1))
                .map(|anchor| (anchor, self.lines[anchor].raw.clone()));
            let index = self.append_record(name, &body);
            if let Some(rule) = self.first_rule_on_line(index) {
                self.lines.remove(index);
                if let Some((anchor, raw)) = anchor {
                    self.lines[anchor].raw = raw;
                }
                return Err(EditError::Grammar(rule));
            }
            tracing::debug!(monitor = name, "appended monitor record");
            Ok(SetOutcome::Appended)
        }
    }

    /// Replaces `range` of line `index` with `replacement`, reclassifies the line,
    /// and undoes the splice — returning the broken rule — when the edited record
    /// breaks the grammar. Only rules located on the edited line count: a
    /// violation elsewhere in the file pre-dates this edit and is not its doing.
    fn splice_checked(
        &mut self,
        index: usize,
        range: std::ops::Range<usize>,
        replacement: &str,
    ) -> Result<(), EditError> {
        let original = self.lines[index].clone();
        self.lines[index].raw.replace_range(range, replacement);
        self.lines[index].kind = classify_line(&self.lines[index].raw);
        if let Some(rule) = self.first_rule_on_line(index) {
            self.lines[index] = original;
            return Err(EditError::Grammar(rule));
        }
        Ok(())
    }

    /// The first grammar rule broken by the record at line `index` (0-based),
    /// file-level rules included, or `None` when that record is well-formed.
    fn first_rule_on_line(&self, index: usize) -> Option<GrammarRule> {
        self.grammar_violations()
            .into_iter()
            .find(|violation| violation.line == index + 1)
            .map(|violation| violation.rule)
    }

    /// The index of the *last* `monitor=` record whose output token equals `name`.
    ///
    /// Rules apply in file order with the last matching rule winning, so when
    /// several records share a name the last one is the output's effective
    /// configuration. Reads and edits therefore target it (via `rposition`), not
    /// a shadowed earlier duplicate — mirroring how [`Self::last_record_index`]
    /// appends after the last record so a new rule wins.
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
    /// record's `monitor=` prefix style, and returns the new line's index.
    fn append_record(&mut self, name: &str, body: &str) -> usize {
        let terminator = line::terminator_if_any_crlf(&self.lines);
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
        let kind = classify_line(&raw);
        let new_line = Line { raw, kind };

        match last_record {
            Some(index) => {
                // Guarantee the line we insert after ends with a terminator, so
                // the new record does not run onto it.
                line::ensure_terminated(&mut self.lines[index].raw, terminator);
                self.lines.insert(index + 1, new_line);
                index + 1
            }
            None => {
                if let Some(last) = self.lines.last_mut() {
                    line::ensure_terminated(&mut last.raw, terminator);
                }
                self.lines.push(new_line);
                self.lines.len() - 1
            }
        }
    }
}

/// Whether an output token is a laptop-panel key for
/// [`GrammarRule::DuplicateLaptopKey`]: an `eDP-…` connector name or any `desc:`
/// token (see that variant for why every `desc:` token counts).
fn is_laptop_key(name: &str) -> bool {
    name.starts_with(LAPTOP_CONNECTOR_PREFIX) || name.starts_with(DESC_PREFIX)
}

/// The record-level grammar rule a record breaks, if any. Only the first broken
/// rule is returned, checked in this order: the disable form (a two-field record
/// that would otherwise be reported as too short), the field count, the output
/// token's comma, then the extras.
///
/// `raw` is the record's line and `fields` its trimmed field spans within it.
fn record_rule(raw: &str, fields: &[(usize, usize)], disabled: bool) -> Option<GrammarRule> {
    if disabled {
        return Some(GrammarRule::DisableForm);
    }
    if fields.len() < 4 {
        return Some(GrammarRule::TooFewFields {
            field_count: fields.len(),
        });
    }
    // The output's trimmed span ends where its text ends; with at least two
    // fields a comma follows somewhere after it, and it must follow at once.
    // For the empty catch-all the span sits right before its comma, so leading
    // whitespace after `=` (which both parsers strip) is never flagged.
    let (_, output_end) = fields[0];
    if !raw[output_end..].starts_with(',') {
        return Some(GrammarRule::SpaceAfterOutput);
    }
    let extras = &fields[4..];
    if extras.len() % 2 != 0 || extras.iter().any(|&(start, end)| start == end) {
        return Some(GrammarRule::IncompleteExtra);
    }
    None
}

/// Classifies one raw line (terminator included). For a record it computes the
/// byte span of each comma-separated field, expressed as offsets into `raw`.
fn classify_line(raw: &str) -> LineKind {
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

    // Both downstream parsers treat `#` as a comment marker anywhere on a line,
    // so the "code" part excludes any trailing comment. Field spans are computed
    // from the code part, keeping a trailing comment outside every field span
    // (preserved).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::differing_lines;

    /// A realistic `monitors.conf` fixture derived from the real dotfiles
    /// (analysis §4, §6.2, §7.2) that follows the frozen grammar: header comments,
    /// blank-line grouping, the catch-all first, an eDP-1 record with
    /// mode/position/scale **and trailing extras** (`bitdepth,10`), a
    /// `desc:`-matched external display, and a trailing comment after the last
    /// record.
    const MONITORS_CONF: &str = "\
# monitors.conf — per-output configuration; later rules win.
# Single source of truth for the eDP panel's mode and scale:
# scripts/hypr-display-profile.sh awk-parses the eDP-1 record below, splitting on
# commas into output,mode,position,scale[,key,value...]. The format is frozen.

# Fallback for anything else plugged in.
monitor=,preferred,auto,1

# Built-in laptop panel (10-bit).
monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10

# A docked external display.
monitor=desc:AU Optronics 0x2036,2560x1440@60,auto,1

# End of monitor configuration.
";

    /// The fixture's eDP-1 record line.
    const EDP_LINE: &str = "monitor=eDP-1,2880x1800@120,0x0,1.333333,bitdepth,10";

    /// Returns the single `monitor=` line of `text` whose comma fields start with
    /// `monitor=<name>` — used to inspect one record's emitted bytes.
    fn record_line<'a>(text: &'a str, name: &str) -> &'a str {
        let needle = format!("monitor={name},");
        text.lines()
            .find(|line| line.starts_with(&needle))
            .unwrap_or_else(|| panic!("no record line for `{name}` in:\n{text}"))
    }

    /// The rules `input` breaks, as `(line, rule)` pairs, from the whole-file
    /// check.
    fn violations(input: &str) -> Vec<(usize, GrammarRule)> {
        let (file, _) = MonitorsFile::parse(input);
        file.grammar_violations()
            .into_iter()
            .map(|violation| (violation.line(), violation.rule().clone()))
            .collect()
    }

    /// A `MonitorRecord` from its three values.
    fn record(mode: &str, position: &str, scale: &str) -> MonitorRecord {
        MonitorRecord {
            mode: mode.to_string(),
            position: position.to_string(),
            scale: scale.to_string(),
        }
    }

    #[test]
    fn round_trip_identity_on_a_realistic_fixture() {
        // Headline guarantee (R6.1, architecture §3): parse -> emit with no edit
        // reproduces the input byte-for-byte, comments/blanks/order included. A
        // grammar-clean fixture warns nothing.
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
            vec!["", "eDP-1", "desc:AU Optronics 0x2036"],
        );
    }

    #[test]
    fn edge_case_inputs_round_trip() {
        // Empty input, a final line without a newline, comment-only and refused records.
        for input in [
            "",
            "monitor=eDP-1,preferred,auto,1",
            "# only a comment",
            "\n\n",
            "monitor=,preferred,auto,1\n# tail",
            // Lossless even for records the grammar refuses.
            "monitor=eDP-1 ,disable\nmonitor=,x",
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
        assert_eq!(
            file.field("desc:AU Optronics 0x2036", MonitorField::Mode),
            Some("2560x1440@60")
        );

        // The catch-all is matched by its empty name.
        assert_eq!(file.field("", MonitorField::Mode), Some("preferred"));
        assert_eq!(file.field("", MonitorField::Scale), Some("1"));

        // An unknown monitor is simply absent.
        assert_eq!(file.is_enabled("DP-9"), None);
        assert_eq!(file.field("DP-9", MonitorField::Mode), None);
    }

    // --- The frozen grammar, one test per rule (task 11.9, analysis §7.2) ---

    #[test]
    fn the_grammar_requires_the_four_positional_fields() {
        // Field order/count: output,mode,position,scale. A record with only an
        // output and a mode gives the profile script's awk no scale (field 4) and
        // leaves hyprland.lua's loader to invent position and scale.
        assert_eq!(
            violations("monitor=HDMI-A-1,1920x1080\n"),
            vec![(1, GrammarRule::TooFewFields { field_count: 2 })]
        );
        assert_eq!(
            violations("monitor=HDMI-A-1,1920x1080,auto\n"),
            vec![(1, GrammarRule::TooFewFields { field_count: 3 })]
        );
        // An empty body is one (empty) field.
        assert_eq!(
            violations("monitor=\n"),
            vec![(1, GrammarRule::TooFewFields { field_count: 1 })]
        );
        // Exactly four fields is the minimal valid record.
        assert!(violations("monitor=HDMI-A-1,1920x1080,auto,1\n").is_empty());
    }

    #[test]
    fn the_grammar_refuses_the_legacy_disable_form() {
        // `monitor=<output>,disable`: hyprland.lua's loader would issue
        // `hl.monitor({ output = "DP-1", mode = "disable", … })`, which Hyprland
        // rejects as an invalid resolution while keeping the output enabled — so
        // the record would not disable anything. It is still read as disabled.
        let input = "monitor=,preferred,auto,1\nmonitor=DP-1,disable\n";
        assert_eq!(violations(input), vec![(2, GrammarRule::DisableForm)]);
        let (file, warnings) = MonitorsFile::parse(input);
        assert_eq!(
            warnings
                .iter()
                .map(|w| (w.line(), w.kind().clone()))
                .collect::<Vec<_>>(),
            vec![(2, ParseWarningKind::Grammar(GrammarRule::DisableForm))]
        );
        assert_eq!(file.is_enabled("DP-1"), Some(false));
        assert_eq!(file.field("DP-1", MonitorField::Mode), None);
    }

    #[test]
    fn the_grammar_requires_complete_extra_pairs() {
        // An odd number of extra fields leaves a key without a value: the loader
        // drops the dangling key, and the profile script hands it on as a
        // half pair.
        assert_eq!(
            violations("monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth\n"),
            vec![(1, GrammarRule::IncompleteExtra)]
        );
        assert_eq!(
            violations("monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,10,vrr\n"),
            vec![(1, GrammarRule::IncompleteExtra)]
        );
        // An even count with an empty value (or key) is no pair either.
        assert_eq!(
            violations("monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,\n"),
            vec![(1, GrammarRule::IncompleteExtra)]
        );
        assert_eq!(
            violations("monitor=eDP-1,2880x1800@120,auto,1.333333,,10\n"),
            vec![(1, GrammarRule::IncompleteExtra)]
        );
        // Complete pairs, several of them, are fine.
        assert!(violations("monitor=eDP-1,2880x1800@120,auto,1.5,bitdepth,10,vrr,1\n").is_empty());
    }

    #[test]
    fn the_grammar_refuses_a_comma_in_any_written_value() {
        // A comma in a value would become a field separator once written. At the
        // value level it is caught directly…
        assert_eq!(
            check_value("desc:Example Corp, Inc 0x0001"),
            Err(GrammarRule::ForbiddenCharacter { character: ',' })
        );
        assert_eq!(
            check_value("1920x1080@60,bitdepth"),
            Err(GrammarRule::ForbiddenCharacter { character: ',' })
        );
        assert_eq!(check_value("desc:Example Corp Inc 0x0001"), Ok(()));

        // …and a record that already carries one (here inside a `desc:` string)
        // has its fields shifted, which the field rules usually catch: here the desc
        // fragment becomes a dangling extra (see `grammar_violations`' limits for the
        // case they cannot).
        assert_eq!(
            violations("monitor=desc:Example Corp, Inc 0x0001,2560x1440@60,0x0,1\n"),
            vec![(1, GrammarRule::IncompleteExtra)]
        );
    }

    #[test]
    fn the_grammar_refuses_a_hash_or_line_break_in_values() {
        // A `#` starts a comment both downstream parsers strip to end of line, so
        // everything after it would be lost; a line break splits the record.
        assert_eq!(
            check_value("desc:Example Monitor #2"),
            Err(GrammarRule::ForbiddenCharacter { character: '#' })
        );
        assert_eq!(
            check_value("1.25\n"),
            Err(GrammarRule::ForbiddenCharacter { character: '\n' })
        );
        assert_eq!(
            check_value("1.25\r"),
            Err(GrammarRule::ForbiddenCharacter { character: '\r' })
        );

        // On disk, a `#` inside a desc truncates the record to its output token.
        assert_eq!(
            violations("monitor=desc:Example Monitor #2,2560x1440@60,0x0,1\n"),
            vec![(1, GrammarRule::TooFewFields { field_count: 1 })]
        );
        // A trailing `# comment` after a complete record is fine.
        assert!(violations("monitor=DP-1,2560x1440@60,0x0,1 # desk\n").is_empty());
    }

    #[test]
    fn the_output_token_must_be_followed_directly_by_a_comma() {
        // The profile script's awk matches the literal `eDP-1,` at the start of the
        // body, so a space before the comma hides the record from it.
        assert_eq!(
            violations("monitor=eDP-1 ,2880x1800@120,auto,1.333333\n"),
            vec![(1, GrammarRule::SpaceAfterOutput)]
        );
        assert_eq!(
            violations("monitor=desc:Example Panel\t,2560x1440,auto,1\n"),
            vec![(1, GrammarRule::SpaceAfterOutput)]
        );
        // Whitespace the awk strips — around `=` and after each comma — is fine,
        // and so is a catch-all whose (empty) output follows `= `.
        assert!(violations("monitor = eDP-1, 2880x1800@120 , 0x0 , 1.5\n").is_empty());
        assert!(violations("monitor= ,preferred,auto,1\n").is_empty());
    }

    #[test]
    fn the_grammar_allows_one_record_per_laptop_panel_key() {
        // The awk takes the first record of its key and Hyprland applies the last,
        // so a second `eDP-1` record makes them disagree.
        assert_eq!(
            violations(
                "monitor=eDP-1,2880x1800@120,auto,1.333333\n\
                 monitor=eDP-1,1920x1200@60,auto,1\n"
            ),
            vec![(2, GrammarRule::DuplicateLaptopKey { first_line: 1 })]
        );
        // A `desc:` token may be the profile script's key for a machine's panel,
        // so it is held to the same rule.
        assert_eq!(
            violations(
                "monitor=desc:Example Panel Vendor 0x2036,2560x1440,auto,1\n\
                 # a comment between them\n\
                 monitor=desc:Example Panel Vendor 0x2036,2560x1440,auto,1.25\n"
            ),
            vec![(3, GrammarRule::DuplicateLaptopKey { first_line: 1 })]
        );
        // `eDP-1` and a `desc:` rule for the same physical panel are different
        // keys (the script looks up exactly one of them), and a plain connector
        // name like `DP-1`, which neither parser looks up as a key, may repeat.
        assert!(
            violations(
                "monitor=eDP-1,2880x1800@120,auto,1.333333\n\
                 monitor=desc:Example Panel Vendor 0x2036,2560x1440,auto,1\n\
                 monitor=DP-1,1920x1080@60,0x0,1\n\
                 monitor=DP-1,2560x1440@60,0x0,1\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn the_grammar_requires_the_catch_all_to_be_the_first_record() {
        // Rules are issued in file order and a later one overrides an earlier one,
        // so a catch-all after a specific rule would override it.
        assert_eq!(
            violations(
                "# header\n\
                 monitor=eDP-1,2880x1800@120,auto,1.333333\n\
                 monitor=,preferred,auto,1\n"
            ),
            vec![(
                3,
                GrammarRule::CatchAllNotFirst {
                    first_record_line: 2
                }
            )]
        );
        // A second catch-all is "not first" either.
        assert_eq!(
            violations("monitor=,preferred,auto,1\nmonitor=,preferred,auto,2\n"),
            vec![(
                2,
                GrammarRule::CatchAllNotFirst {
                    first_record_line: 1
                }
            )]
        );
        // Comments, blanks and non-monitor lines before the catch-all are fine, and
        // a file without a catch-all is fine too.
        assert!(
            violations("# header\n\nworkspace=1,monitor:eDP-1\nmonitor=,preferred,auto,1\n")
                .is_empty()
        );
        assert!(violations("monitor=eDP-1,preferred,auto,1\n").is_empty());
    }

    #[test]
    fn every_grammar_rule_names_itself_without_file_contents() {
        // The rule text is what the Display page shows after "refused:" (R8.3) and
        // what the parse warning logs; it carries line numbers and counts, never a
        // value from the file (R7.3).
        let rules = [
            GrammarRule::TooFewFields { field_count: 2 },
            GrammarRule::DisableForm,
            GrammarRule::IncompleteExtra,
            GrammarRule::ForbiddenCharacter { character: ',' },
            GrammarRule::ForbiddenCharacter { character: '\n' },
            GrammarRule::SpaceAfterOutput,
            GrammarRule::DuplicateLaptopKey { first_line: 7 },
            GrammarRule::CatchAllNotFirst {
                first_record_line: 3,
            },
        ];
        let texts: Vec<String> = rules.iter().map(ToString::to_string).collect();
        assert!(texts[0].contains("four fields") && texts[0].contains('2'));
        assert!(texts[1].contains("disable"));
        assert!(texts[2].contains("key,value"));
        assert!(texts[3].contains("`,`"));
        assert!(
            texts[4].contains("`\\n`"),
            "a line break is escaped: {}",
            texts[4]
        );
        assert!(texts[5].contains("followed directly by `,`"));
        assert!(texts[6].contains("line 7"));
        assert!(texts[7].contains("catch-all") && texts[7].contains("line 3"));

        let violation = GrammarViolation {
            line: 4,
            rule: GrammarRule::IncompleteExtra,
        };
        assert!(violation.to_string().starts_with("line 4: "));
    }

    // --- Surgical edits ---

    #[test]
    fn editing_edp_mode_and_scale_touches_only_that_record() {
        // Explicit accept criterion: editing the eDP rule's mode/scale changes
        // only those field(s) and leaves the catch-all, the other record, and the
        // comments byte-identical. Trailing extras (`bitdepth,10`) survive too.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        file.set_field("eDP-1", MonitorField::Mode, "1920x1200@60")
            .expect("eDP-1 mode is editable");
        file.set_field("eDP-1", MonitorField::Scale, "1.0")
            .expect("eDP-1 scale is editable");

        let edited = file.emit();
        let changed = differing_lines(MONITORS_CONF, &edited);
        let target = MONITORS_CONF
            .lines()
            .position(|l| l == EDP_LINE)
            .expect("fixture contains the eDP-1 record");
        assert_eq!(
            changed,
            vec![target],
            "only the eDP-1 record line may change"
        );
        assert_eq!(
            edited.lines().nth(target),
            Some("monitor=eDP-1,1920x1200@60,0x0,1.0,bitdepth,10"),
            "mode (field 2) and scale (field 4) changed; output, position, and extras preserved"
        );
    }

    #[test]
    fn edited_record_stays_awk_parseable() {
        // CRITICAL guarantee: after an edit the record keeps the positional comma
        // structure scripts/hypr-display-profile.sh parses — output in field 1,
        // mode in field 2, scale in field 4, extras after. Replicate that awk field
        // split here (splitting the whole line on commas) on both the original and
        // the edited record.
        let original_fields: Vec<&str> = record_line(MONITORS_CONF, "eDP-1").split(',').collect();
        assert_eq!(
            original_fields[0], "monitor=eDP-1",
            "field 1 is the output token"
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
        assert_eq!(fields[0], "monitor=eDP-1", "field 1 still the output token");
        assert_eq!(fields[1], "1920x1200@60", "field 2 still the mode");
        assert_eq!(fields[3], "1.25", "field 4 still the scale");
        assert_eq!(
            &fields[4..],
            &["bitdepth", "10"],
            "the trailing extras keep their positions after the scale"
        );
        assert!(file.grammar_violations().is_empty());
    }

    #[test]
    fn set_field_refuses_a_forbidden_character_naming_the_rule() {
        // A comma in a value would split into a new positional field and shift the
        // fields both downstream parsers read, so it is refused before any byte is
        // written — here the edit that would insert a `bitdepth` key with no value.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Scale, "1.25,bitdepth"),
            Err(EditError::Grammar(GrammarRule::ForbiddenCharacter {
                character: ','
            }))
        );
        // Newlines and `#` are refused for the same structural reasons.
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Scale, "1.0\n"),
            Err(EditError::Grammar(GrammarRule::ForbiddenCharacter {
                character: '\n'
            }))
        );
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Mode, "preferred # x"),
            Err(EditError::Grammar(GrammarRule::ForbiddenCharacter {
                character: '#'
            }))
        );
        assert_eq!(
            file.emit(),
            MONITORS_CONF,
            "a rejected edit changes nothing"
        );
    }

    #[test]
    fn set_field_refuses_to_leave_a_grammar_violation() {
        // The last line of defence (R8.3): whatever the caller validated, an edit
        // never emits a record the downstream parsers would misread. A hand-edited
        // record with a dangling extra stays broken after a scale edit, so the edit
        // is refused with the rule named and the line restored byte-for-byte.
        let input = "monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth\n";
        let (mut file, _) = MonitorsFile::parse(input);
        assert_eq!(
            file.set_field("eDP-1", MonitorField::Scale, "1.25"),
            Err(EditError::Grammar(GrammarRule::IncompleteExtra))
        );
        assert_eq!(file.emit(), input, "the refused edit is undone");
        assert_eq!(file.field("eDP-1", MonitorField::Scale), Some("1.333333"));
    }

    #[test]
    fn set_field_reports_a_missing_monitor() {
        let input = "monitor=,preferred,auto,1\nmonitor=DP-2,disable\n";
        let (mut file, _) = MonitorsFile::parse(input);

        // No record with that name — set_field never creates one.
        assert_eq!(
            file.set_field("DP-9", MonitorField::Mode, "1920x1080@60"),
            Err(EditError::NoSuchMonitor("DP-9".to_string()))
        );
        assert_eq!(file.emit(), input, "no rejected edit changed the file");
    }

    #[test]
    fn set_field_reports_a_disabled_record() {
        let input = "monitor=,preferred,auto,1\nmonitor=DP-2,disable\n";
        let (mut file, _) = MonitorsFile::parse(input);

        // A legacy disable record has no mode field to edit.
        assert_eq!(
            file.set_field("DP-2", MonitorField::Mode, "1920x1080@60"),
            Err(EditError::RecordDisabled("DP-2".to_string()))
        );
        assert_eq!(file.emit(), input, "no rejected edit changed the file");
    }

    #[test]
    fn appending_a_new_monitor_inserts_after_the_last_monitor_line() {
        // Accept criterion: writing a record for a monitor that does not exist
        // appends a new `monitor=` record after the last `monitor=` line — here
        // after the desc record and before the trailing `# End` comment, proving
        // it is placed after the last record rather than at end-of-file.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        let outcome = file
            .set_record("DP-3", &record("3840x2160@60", "2880x0", "1.5"))
            .expect("appending a valid record succeeds");
        assert_eq!(outcome, SetOutcome::Appended);

        let expected = MONITORS_CONF.replace(
            "monitor=desc:AU Optronics 0x2036,2560x1440@60,auto,1\n",
            "monitor=desc:AU Optronics 0x2036,2560x1440@60,auto,1\n\
             monitor=DP-3,3840x2160@60,2880x0,1.5\n",
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
            .set_record("eDP-1", &record("2880x1800@120", "0x0", "1.333333"))
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
        file.set_record("DP-1", &record("1920x1080@60", "auto", "1"))
            .expect("append succeeds");
        assert_eq!(
            file.emit(),
            "monitor = eDP-1,2560x1440,0x0,1\nmonitor = DP-1,1920x1080@60,auto,1\n"
        );
    }

    #[test]
    fn appending_refuses_a_record_that_breaks_a_file_level_rule() {
        // A record appended after the last one could break a file-level rule: a
        // catch-all appended after specific rules would override them all. The
        // append is refused with the rule named and the file left unchanged.
        let input = "monitor=eDP-1,2880x1800@120,auto,1.333333\n";
        let (mut file, _) = MonitorsFile::parse(input);
        assert_eq!(
            file.set_record("", &record("preferred", "auto", "1")),
            Err(EditError::Grammar(GrammarRule::CatchAllNotFirst {
                first_record_line: 1
            }))
        );
        assert_eq!(file.emit(), input);

        // An output token carrying a comma (a `desc:` string) is refused before
        // anything is appended.
        assert_eq!(
            file.set_record(
                "desc:Example Corp, Inc 0x0001",
                &record("2560x1440@60", "0x0", "1")
            ),
            Err(EditError::Grammar(GrammarRule::ForbiddenCharacter {
                character: ','
            }))
        );
        assert_eq!(file.emit(), input);
    }

    #[test]
    fn a_refused_append_restores_the_missing_terminator() {
        // Appending terminates the last record first when it has no newline; a refused
        // append must undo that too, so the file round-trips byte-for-byte.
        let input = "monitor=eDP-1,2880x1800@120,auto,1.333333";
        let (mut file, _) = MonitorsFile::parse(input);
        assert!(matches!(
            file.set_record("", &record("preferred", "auto", "1")),
            Err(EditError::Grammar(GrammarRule::CatchAllNotFirst { .. }))
        ));
        assert_eq!(file.emit(), input);
        assert_eq!(file.record_names(), vec!["eDP-1"]);
    }

    #[test]
    fn set_record_completes_a_legacy_disable_record() {
        // The only thing the app writes for an output is a complete record, so a
        // hand-written legacy disable record becomes valid when it is rewritten.
        let input = "monitor=,preferred,auto,1\nmonitor=desc:AU Optronics 0x2036,disable\n";
        let (mut file, warnings) = MonitorsFile::parse(input);
        assert_eq!(warnings.len(), 1, "the disable form is reported");

        let outcome = file
            .set_record(
                "desc:AU Optronics 0x2036",
                &record("2560x1440@60", "auto", "1.066667"),
            )
            .expect("rewriting into a complete record succeeds");
        assert_eq!(outcome, SetOutcome::Edited);
        assert_eq!(
            file.emit(),
            "monitor=,preferred,auto,1\n\
             monitor=desc:AU Optronics 0x2036,2560x1440@60,auto,1.066667\n"
        );
        assert_eq!(file.is_enabled("desc:AU Optronics 0x2036"), Some(true));
        assert!(file.grammar_violations().is_empty());
    }

    #[test]
    fn set_record_drops_extras_but_keeps_the_record_valid() {
        // `set_record` rewrites the whole body after the output token, so a
        // record's extras go with it — the result is still a complete record.
        let (mut file, _) = MonitorsFile::parse(MONITORS_CONF);
        file.set_record("eDP-1", &record("1920x1200@60", "auto", "1"))
            .expect("editable");
        assert_eq!(
            record_line(&file.emit(), "eDP-1"),
            "monitor=eDP-1,1920x1200@60,auto,1"
        );
        assert!(file.grammar_violations().is_empty());
    }

    #[test]
    fn a_malformed_monitor_line_is_kept_with_a_warning() {
        // Accept criterion: a `monitor=` line with an unexpected field shape (here
        // only output + mode) surfaces as a grammar warning, not a panic, and is
        // preserved losslessly.
        let input = "\
monitor=,preferred,auto,1
# external head
monitor=HDMI-A-1,badmode
";
        let (file, warnings) = MonitorsFile::parse(input);

        // Lossless: the odd line is kept byte-for-byte.
        assert_eq!(file.emit(), input);

        // Surfaced as a warning on the right (1-based) line, not a panic.
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].line(), 3);
        assert_eq!(
            warnings[0].kind(),
            &ParseWarningKind::Grammar(GrammarRule::TooFewFields { field_count: 2 })
        );
        assert!(
            warnings[0]
                .to_string()
                .contains("frozen monitors.conf grammar")
        );

        // It is still enumerated and enabled; the fields it does have are readable
        // and the ones it lacks are simply absent.
        assert_eq!(file.record_names(), vec!["", "HDMI-A-1"]);
        assert_eq!(file.is_enabled("HDMI-A-1"), Some(true));
        assert_eq!(file.field("HDMI-A-1", MonitorField::Mode), Some("badmode"));
        assert_eq!(file.field("HDMI-A-1", MonitorField::Position), None);
    }

    #[test]
    fn a_grammar_warning_log_carries_no_file_contents() {
        // A hand-edited record that breaks the grammar must be visible in the
        // journal (task 11.9), naming the line and the rule but not the values.
        let input = "monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth\n";
        let events = crate::testing::capture_logs(|| {
            MonitorsFile::parse(input);
        });
        let warning = events
            .iter()
            .find(|event| event.message == "monitors.conf parse warning")
            .expect("the grammar violation is logged");
        assert_eq!(warning.level, tracing::Level::WARN);
        assert!(warning.fields.contains("line 1"), "{}", warning.fields);
        assert!(warning.fields.contains("key,value"), "{}", warning.fields);
        assert!(
            !warning.fields.contains("2880x1800"),
            "no file contents in the log (R7.3): {}",
            warning.fields
        );
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
        let input = "monitor=,preferred,auto,1\n# monitor=eDP-1,2880x1800@120,0x0,1\n";
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
    fn crlf_line_endings_survive_an_edit() {
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
        // Rules apply in file order, the last matching rule winning, so when two
        // records share a name the LAST is the output's effective configuration.
        // Reads and edits must target it and leave the shadowed earlier duplicate
        // byte-identical. (A plain connector name may repeat under the grammar; a
        // laptop-panel key may not.)
        let input = "\
monitor=DP-1,2880x1800@120,0x0,1.0
monitor=DP-1,1920x1200@60,0x0,1.5
";
        let (mut file, warnings) = MonitorsFile::parse(input);
        assert!(warnings.is_empty());

        // A read returns the last (effective) record's fields, not the first.
        assert_eq!(file.field("DP-1", MonitorField::Mode), Some("1920x1200@60"));
        assert_eq!(file.field("DP-1", MonitorField::Scale), Some("1.5"));

        // An edit rewrites the last record; the shadowed first one is untouched.
        file.set_field("DP-1", MonitorField::Scale, "2.0")
            .expect("editable");
        assert_eq!(
            file.emit(),
            "\
monitor=DP-1,2880x1800@120,0x0,1.0
monitor=DP-1,1920x1200@60,0x0,2.0
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
    fn a_scale_edit_keeps_every_extra_in_position() {
        // A record with several extra pairs: a scale edit must keep scale in field
        // 4 (awk split) and leave every extra field (5..) in its exact position.
        let input = "monitor=eDP-1,2880x1800@120,0x0,1.5,bitdepth,10,vrr,1\n";
        let (mut file, _) = MonitorsFile::parse(input);

        file.set_field("eDP-1", MonitorField::Scale, "1.25")
            .expect("editable");
        let edited = file.emit();

        let fields: Vec<&str> = record_line(&edited, "eDP-1").split(',').collect();
        assert_eq!(fields[0], "monitor=eDP-1", "field 1 still the output token");
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
            .set_record("DP-1", &record("1920x1080@60", "0x0", "1"))
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
    fn set_record_preserves_a_trailing_inline_comment() {
        // set_record rewrites the body after the output token up to the last
        // field, so a trailing inline comment on the record survives untouched.
        let input = "monitor=eDP-1,2880x1800@120,0x0,1.0 # primary panel\n";
        let (mut file, _) = MonitorsFile::parse(input);

        file.set_record("eDP-1", &record("1920x1200@60", "0x0", "1.5"))
            .expect("editable");
        assert_eq!(
            file.emit(),
            "monitor=eDP-1,1920x1200@60,0x0,1.5 # primary panel\n",
            "the body was rewritten but the inline comment is preserved"
        );
    }
}
