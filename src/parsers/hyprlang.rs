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
//! This module edits the hyprlang files the settings GUI manages:
//! `hypridle.conf`, `hyprlock.conf` and `hyprpaper.conf`. Hyprland itself now
//! runs its Lua config provider, so the Input page edits `input.lua` and the
//! cursor write edits the two `hl.env` lines of `hyprland.lua` through
//! [`crate::parsers::lua_literal`] instead (analysis §7.1, §8.6; tasks 11.4,
//! 11.5).
//!
//! # Lossless, surgical editing
//!
//! These files are hand-maintained: they carry header comments, inline comments
//! that document magic numbers (`timeout = 150 # 2.5 min`), deliberate blank-line
//! grouping, and commented-out example lines. This module therefore follows the
//! [parser contract](crate::parsers#the-parser-contract) shared by every config
//! parser — [`HyprlangFile::parse`] then [`HyprlangFile::emit`] with no edit
//! reproduces the input byte for byte, and an edit rewrites one value's bytes.
//! What is specific to hyprlang is how a value is located: the token model and
//! the two addressing schemes below.
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
//!   the power/idle page (task 6.8, positional `listener` matching) and the
//!   wallpaper/lock page (task 6.5) use.
//! - **By repeatable top-level key + first field** —
//!   [`HyprlangFile::set_repeatable_field_value`]. A top-level key such as
//!   `env` can appear many times, so a specific line is selected by its first
//!   comma-separated field (e.g. `env = XCURSOR_THEME,…` is selected by
//!   `XCURSOR_THEME`) and only the value portion *after that first comma* is
//!   edited, leaving the field name and everything else byte-identical. The
//!   cursor-env writer (task 6.4) used it on Hyprland's former hyprlang main
//!   config; since task 11.5 moved that write to `hyprland.lua` it is a parser
//!   capability with no production caller.
//!
//! # Duplicated keys, and why sections are not merged
//!
//! Hyprlang applies a file top to bottom, so when a section assigns the same key
//! twice the **last** assignment is the value the daemon ends up holding. Both
//! [`HyprlangFile::value`] and [`HyprlangFile::set_value`] therefore address the
//! **last** matching entry inside the addressed section: the app reads the value
//! that is actually in effect and edits the copy that decides it, instead of
//! rewriting an earlier copy the daemon ignores — the effective-copy rule of the
//! [parser contract](crate::parsers#the-parser-contract). An earlier, shadowed
//! copy is left byte-identical.
//!
//! The **repeatable-key** scheme is unaffected by that rule and cannot conflict
//! with it, because it addresses a different thing: repeated top-level keys such
//! as `exec-once` are *meant* to occur many times, each line being its own
//! directive rather than a copy shadowing the previous one, so a line is picked
//! by identity (key plus first comma-field), not by position. Two lines *can*
//! still share the same first field — a degenerate `env = XCURSOR_THEME,…`
//! written twice — and that case is knowingly unhandled: both reading and
//! writing resolve to the **first** such line. They agree, so the app never
//! displays one copy while editing another, but hyprland's own `env` handling
//! applies each line in turn and ends up with the *later* one's value, so an edit
//! to the first line would not take effect. The parser reports that shape as an
//! [`Ambiguity::DoubledRepeatableField`] (see "Reporting ambiguous addresses"
//! below) and logs a `warn` built from the same report whenever such an address
//! is read or written.
//!
//! Sections are deliberately **not** merged: a [`KeyPath`] always names one
//! section instance, by occurrence. That is required because a repeated block can
//! be a distinct object rather than a continuation — hypridle's three
//! `listener { }` blocks are three separate listeners (hyprlang calls these
//! "anonymous special categories"), so folding them together would let one
//! listener's `timeout` overwrite another's. The trade-off is that when a plain
//! category is written twice in one file, hyprlang merges the blocks and the
//! later block's assignment wins, whereas this parser edits whichever occurrence
//! the caller addressed — so an edit to the first block can still be shadowed by
//! a second; the parser reports that shape as an [`Ambiguity::RepeatedCategory`].
//! All four behaviours behind these rules were verified against libhyprlang
//! 0.6.8: two `kb_layout` lines in one `input { }` block resolve to the second; a
//! `sensitivity` set in a second `input { }` block overrides the first; two
//! anonymous `listener { }` blocks become two instances; and a `timeout` doubled
//! *inside* one such block resolves to the second, so the last-occurrence rule is
//! right there too.
//!
//! Which kind a block is cannot be read from the file: the program that reads
//! the file declares it when it sets up its parser. Three kinds occur in the
//! files this app edits (checked against each daemon's source, see
//! [`SpecialCategory`]):
//!
//! - **plain categories** — hypridle's and hyprlock's `general { }`, hyprlock's
//!   `auth { }`: one set of values, so a second declaration continues the first
//!   and its assignments win;
//! - **anonymous special categories** — hypridle's `listener { }`, hyprlock's
//!   `background { }`, `label { }`, `input-field { }`, `shape { }` and
//!   `image { }`: every block is a new object, so a repeat is expected and is
//!   addressed by occurrence;
//! - **keyed special categories** — hyprpaper's `wallpaper { }`, keyed by its
//!   `monitor` value: blocks for different monitors are different wallpapers,
//!   while two blocks naming the *same* monitor are merged into one, exactly like
//!   a plain category.
//!
//! # Reporting ambiguous addresses
//!
//! Neither shape above is a parse error — the file is valid and round-trips byte
//! for byte — and neither can be fixed by guessing which line the user meant. So
//! [`HyprlangFile::ambiguities`] *reports* them instead, as a read-only list of
//! [`Ambiguity`] values naming the key or category and the line numbers, for the
//! caller to show where the user can see it (task 9.31). Because the block kinds
//! above are part of each daemon's schema rather than of the text, the caller
//! passes a [`HyprlangSchema`] naming its daemon's special categories and
//! repeatable keys; anything not named there is treated as a plain category or
//! an ordinary key. Repeated anonymous blocks are never reported: repetition is
//! their purpose. Like every other diagnostic here, a report carries names and
//! line numbers only, never values ([R7.3][r73]).
//!
//! [r73]: crate::parsers#diagnostics-carry-positions-not-contents-r73
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

use super::line::{self, RawLine, strip_terminator};

/// A parsed hyprlang file that can re-emit itself byte-for-byte and edit
/// individual values in place.
///
/// Built by [`HyprlangFile::parse`]. See the module documentation for the token
/// model and addressing schemes.
#[derive(Clone, Debug)]
pub struct HyprlangFile {
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

impl RawLine for Line {
    fn raw(&self) -> &str {
        &self.raw
    }
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
pub struct SectionStep {
    /// The section name to descend into.
    name: String,
    /// Which occurrence (0-based) of `name` among its same-named siblings to
    /// descend into. `0` selects the first; hypridle's third `listener` block is
    /// occurrence `2`.
    occurrence: usize,
}

impl SectionStep {
    /// Selects the **first** section named `name` at the current level.
    pub fn first(name: &str) -> Self {
        SectionStep {
            name: name.to_string(),
            occurrence: 0,
        }
    }

    /// Selects the `occurrence`-th (0-based) section named `name` among its
    /// same-named siblings — used to address one of several duplicate sections
    /// such as hypridle's `listener { }` blocks (task 6.8).
    pub fn nth(name: &str, occurrence: usize) -> Self {
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
pub struct KeyPath {
    /// Section steps from outermost to innermost; empty means top level.
    sections: Vec<SectionStep>,
    /// The key name within the innermost section.
    key: String,
}

impl KeyPath {
    /// Addresses a key at the top level (no enclosing section).
    pub fn top_level(key: &str) -> Self {
        KeyPath {
            sections: Vec::new(),
            key: key.to_string(),
        }
    }

    /// Addresses a key by a section path, taking the **first** occurrence of
    /// each named section — the common case (e.g. `["input", "touchpad"]` +
    /// `natural_scroll`).
    pub fn at(section_names: &[&str], key: &str) -> Self {
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
    pub fn new(sections: Vec<SectionStep>, key: &str) -> Self {
        KeyPath {
            sections,
            key: key.to_string(),
        }
    }

    /// The name of the outermost section this path descends into, or `None` for a
    /// top-level key — what a caller compares with
    /// [`Ambiguity::top_level_category`] to tell whether a reported ambiguity
    /// touches the value this path addresses (task 9.31).
    pub fn top_section(&self) -> Option<&str> {
        self.sections.first().map(|step| step.name.as_str())
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
pub enum ParseWarningKind {
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

/// The part of a daemon's configuration schema that a hyprlang file's text does not
/// reveal, which [`HyprlangFile::ambiguities`] needs to tell an expected repeat from
/// an ambiguous one (task 9.31).
///
/// hyprlang leaves it to the program reading a file to declare which top-level
/// blocks are separate objects ([`SpecialCategory`]) and which keys are directives
/// meant to be repeated (`env`, `exec-once`); the file looks the same either way.
/// Each caller therefore describes its daemon's file with one of these. A block name
/// not listed in `special_categories` is treated as a plain category, and a key not
/// listed in `repeatable_keys` is not checked for doubled first fields — so the
/// default (empty) schema reports only repeated plain categories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HyprlangSchema {
    /// The top-level blocks the daemon declares as special categories.
    pub special_categories: &'static [SpecialCategory],
    /// The top-level keys whose lines are separate directives selected by their
    /// first comma-field (see the module's "Addressing schemes"), checked for two
    /// lines sharing that field.
    pub repeatable_keys: &'static [&'static str],
}

impl HyprlangSchema {
    /// The special category declared for the top-level block `name`, if any.
    fn special_category(&self, name: &str) -> Option<SpecialCategory> {
        self.special_categories
            .iter()
            .copied()
            .find(|category| category.name() == name)
    }
}

/// A top-level block that a daemon declares as a hyprlang *special category*: a block
/// that may occur many times, each occurrence (or each key value) being its own object
/// rather than a continuation of the previous one.
///
/// The two kinds mirror the two ways the daemons register them with libhyprlang
/// (`addSpecialCategory`): with `anonymousKeyBased = true` — hypridle's `listener`,
/// hyprlock's `background`, `label`, `input-field`, `shape` and `image` — or with a
/// `key` — hyprpaper's `wallpaper`, keyed by `monitor`. For a keyed category hyprlang
/// looks up an existing instance by the key's value, so two blocks naming the same
/// value are merged into one instance (later assignments win) while different values
/// stay separate objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialCategory {
    /// Every block is a new object (hyprlang's anonymous special category), so a
    /// repeated block is never ambiguous.
    Anonymous(&'static str),
    /// Blocks are objects identified by the value of their `key` assignment; blocks
    /// sharing that value are merged.
    Keyed {
        /// The block name (e.g. `wallpaper`).
        name: &'static str,
        /// The key whose value identifies the object (e.g. `monitor`).
        key: &'static str,
    },
}

impl SpecialCategory {
    /// The block name this declaration applies to.
    pub fn name(&self) -> &'static str {
        match self {
            SpecialCategory::Anonymous(name) | SpecialCategory::Keyed { name, .. } => name,
        }
    }
}

/// An address in a hyprlang file that names one line while the daemon reading the file
/// takes its value from another, reported by [`HyprlangFile::ambiguities`] (task 9.31).
///
/// Both shapes are valid hyprlang and round-trip losslessly; what they break is the
/// promise that an edit made through the app takes effect (R5.3). The report is meant
/// to be shown to the user, so it names the key or category and the 1-based line
/// numbers — never a value ([R7.3][r73]) — and its [`Display`](fmt::Display) form is a
/// plain sentence fragment ("`general` is declared twice (lines 1 and 12)").
///
/// [r73]: crate::parsers#diagnostics-carry-positions-not-contents-r73
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ambiguity {
    /// Two or more top-level lines of a repeatable key share their first comma-field
    /// (`env = XCURSOR_THEME,…` written twice). The repeatable-key accessors read and
    /// write the first of them, while the daemon applies each line in turn and ends up
    /// with the last.
    DoubledRepeatableField {
        /// The repeatable key (e.g. `env`).
        key: String,
        /// The shared first comma-field (e.g. `XCURSOR_THEME`).
        field: String,
        /// The 1-based line numbers of the lines sharing it, in file order.
        lines: Vec<usize>,
    },
    /// A category declared more than once among the same siblings — a plain category,
    /// or a keyed special category naming the same key value — which the daemon merges
    /// so that a later block's assignment wins, while a [`KeyPath`] addresses one of
    /// the blocks.
    RepeatedCategory {
        /// The category's section path from the top level, outermost first (e.g.
        /// `["general"]`, or `["auth", "fingerprint"]` for a nested repeat).
        path: Vec<String>,
        /// For a keyed special category, the key whose value the blocks share (e.g.
        /// `monitor`); `None` for a plain category.
        keyed_by: Option<&'static str>,
        /// The 1-based line numbers of the blocks' headers, in file order.
        lines: Vec<usize>,
    },
}

impl Ambiguity {
    /// The 1-based line numbers involved, in file order.
    pub fn lines(&self) -> &[usize] {
        match self {
            Ambiguity::DoubledRepeatableField { lines, .. }
            | Ambiguity::RepeatedCategory { lines, .. } => lines,
        }
    }

    /// The top-level category a [`Ambiguity::RepeatedCategory`] lies in (the first
    /// element of its path), or `None` for a repeatable key, which lives outside every
    /// category. Lets a caller decide whether the ambiguity touches a value it edits, by
    /// comparing it with the [`KeyPath::top_section`] of each address it uses.
    pub fn top_level_category(&self) -> Option<&str> {
        match self {
            Ambiguity::RepeatedCategory { path, .. } => path.first().map(String::as_str),
            Ambiguity::DoubledRepeatableField { .. } => None,
        }
    }
}

impl fmt::Display for Ambiguity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ambiguity::DoubledRepeatableField { key, field, lines } => write!(
                f,
                "{} `{key}` lines share the first field `{field}` ({})",
                count_word(lines.len()),
                line_list(lines)
            ),
            Ambiguity::RepeatedCategory {
                path,
                keyed_by,
                lines,
            } => {
                write!(f, "`{}` is declared {}", path.join("."), times(lines.len()))?;
                if let Some(key) = keyed_by {
                    write!(f, " for the same `{key}`")?;
                }
                write!(f, " ({})", line_list(lines))
            }
        }
    }
}

/// "Two", "Three", … for the small counts an ambiguity report carries, so a sentence
/// can start with it; larger counts fall back to digits.
fn count_word(count: usize) -> String {
    match count {
        2 => "Two".to_string(),
        3 => "Three".to_string(),
        4 => "Four".to_string(),
        other => other.to_string(),
    }
}

/// "twice" for two, "N times" otherwise.
fn times(count: usize) -> String {
    if count == 2 {
        "twice".to_string()
    } else {
        format!("{count} times")
    }
}

/// Renders 1-based line numbers as English prose: "line 4", "lines 1 and 12",
/// "lines 1, 5 and 12".
fn line_list(lines: &[usize]) -> String {
    match lines {
        [] => "no lines".to_string(),
        [only] => format!("line {only}"),
        [init @ .., last] => {
            let init: Vec<String> = init.iter().map(usize::to_string).collect();
            format!("lines {} and {last}", init.join(", "))
        }
    }
}

/// A failure from an edit method ([`HyprlangFile::set_value`] /
/// [`HyprlangFile::set_repeatable_field_value`]).
///
/// Every variant leaves the file completely unchanged: the check happens before
/// any byte is spliced, so a rejected edit can never partially rewrite a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
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
    /// never file contents ([R7.3][r73]).
    ///
    /// [r73]: crate::parsers#diagnostics-carry-positions-not-contents-r73
    pub fn parse(input: &str) -> (Self, Vec<ParseWarning>) {
        let mut lines = Vec::new();
        let mut warnings = Vec::new();

        for (index, raw) in line::split_raw_lines(input).enumerate() {
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
    pub fn emit(&self) -> String {
        line::emit(&self.lines)
    }

    /// Returns the current value of the key addressed by `path`, if it exists.
    ///
    /// The value excludes any trailing inline comment. Comment, blank, section,
    /// and malformed lines are never considered, so a commented-out assignment is
    /// never read as a value. When the addressed section assigns the key more
    /// than once, this reads the **last** assignment — the one hyprlang resolves
    /// the key to, and the same one [`set_value`](Self::set_value) edits.
    pub fn value(&self, path: &KeyPath) -> Option<&str> {
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
    /// one. When a key appears more than once in the same section, the **last**
    /// occurrence is edited — hyprlang resolves the key to that one, so editing
    /// an earlier, shadowed copy would silently have no effect (task 9.8); the
    /// shadowed copies stay byte-identical.
    ///
    /// Errors:
    /// - [`EditError::InvalidValue`] if `value` contains a newline or `#`
    ///   (rejected before any byte changes, R8.3).
    /// - [`EditError::SectionNotFound`] if the addressed section path does not
    ///   exist, so there is nothing to edit or append into.
    pub fn set_value(&mut self, path: &KeyPath, value: &str) -> Result<(), EditError> {
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
    ///
    /// If several entries share the key *and* the first field, the **first** of
    /// them is read — the same one
    /// [`set_repeatable_field_value`](Self::set_repeatable_field_value) edits, and
    /// not necessarily the one hyprland ends up applying — and a `warn` records the
    /// [`Ambiguity::DoubledRepeatableField`]. See the module section "Duplicated keys,
    /// and why sections are not merged".
    pub fn repeatable_field_value(&self, key: &str, first_field: &str) -> Option<&str> {
        let i = self.find_repeatable(key, first_field)?;
        let value = self.entry_value(i)?;
        value_portion(value).map(|(start, end)| &value[start..end])
    }

    /// Rewrites the value portion (after the first comma) of the repeatable
    /// top-level entry `key` whose first comma-field equals `first_field`,
    /// changing only those bytes and leaving the field name and the rest of the
    /// line byte-identical.
    ///
    /// For example, `set_repeatable_field_value("env", "XCURSOR_THEME", "Bibata")`
    /// rewrites `env = XCURSOR_THEME,Nordic-cursors` to `env = XCURSOR_THEME,Bibata`
    /// without disturbing sibling `env` lines, `source` lines, or comments. (The
    /// cursor write used exactly that on Hyprland's former hyprlang main config until
    /// task 11.5 moved it to `hyprland.lua`; none of the three hyprlang files the app
    /// still edits has repeatable keys as the dotfiles currently write them
    /// (hyprpaper's grammar does allow repeated `preload =`/`wallpaper =`), so this
    /// is general parser capability per architecture §3 rather than a path the app
    /// currently takes.)
    ///
    /// If several entries share the key *and* the first field, the **first** of
    /// them is rewritten (matching what
    /// [`repeatable_field_value`](Self::repeatable_field_value) reads) and a `warn`
    /// records the ambiguity, which [`ambiguities`](Self::ambiguities) also reports
    /// for a caller to show. Unlike a duplicated key inside a section, this is
    /// *not* resolved to the last occurrence: see the module section "Duplicated
    /// keys, and why sections are not merged" for why, and for what hyprland does
    /// with such a file.
    ///
    /// Errors:
    /// - [`EditError::InvalidValue`] if `value` contains a newline or `#`.
    /// - [`EditError::RepeatableKeyNotFound`] if no matching entry exists.
    /// - [`EditError::NoValuePortion`] if the matched entry has no comma.
    ///
    /// Unlike [`set_value`](Self::set_value), this never appends: a repeatable
    /// entry has no unambiguous append location, so a missing target is an error.
    pub fn set_repeatable_field_value(
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
    ///
    /// A duplicated key resolves to its **last** assignment within the addressed
    /// section (see the module documentation), so the walk cannot stop at the
    /// first match: it continues to the section's closing `}`, or to end-of-file
    /// for a top-level key or a target section the file never closes, and reports
    /// the match it saw last.
    fn resolve_path(&self, path: &KeyPath) -> Resolution {
        let target = &path.sections;
        let target_top_level = target.is_empty();

        // The stack of currently open sections. Each frame counts how many child
        // sections of each name it has seen, so a child's occurrence index is the
        // count at the moment it opens. A separate root counter handles top-level
        // sections.
        let mut frames: Vec<WalkFrame> = Vec::new();
        let mut root_counts: Vec<(String, usize)> = Vec::new();

        // The latest assignment of the addressed key seen so far in the target
        // section; the one still recorded when the section ends is the effective
        // (last) occurrence.
        let mut matched_entry: Option<usize> = None;
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
                        // The whole target section has been walked, so any
                        // recorded match is its last assignment of the key.
                        return match matched_entry {
                            Some(index) => Resolution::Entry(index),
                            // The key is absent from the section, so this closing
                            // brace is where a new assignment is appended.
                            None => Resolution::Append(AppendPlan {
                                insert_index: i,
                                sibling_entry: last_child_entry,
                                section_open: target_open,
                                at_eof: false,
                            }),
                        };
                    }
                }
                LineKind::Entry { key, .. } => {
                    if path_matches(&frames, target) {
                        if key == &path.key {
                            // Keep looking: a later assignment of the same key
                            // shadows this one.
                            matched_entry = Some(i);
                        } else {
                            last_child_entry = Some(i);
                        }
                    }
                }
                LineKind::Blank | LineKind::Comment | LineKind::Malformed => {}
            }
        }

        // The walk ran to end-of-file: either the target is a top-level key, or
        // the target section was never closed.
        if let Some(index) = matched_entry {
            Resolution::Entry(index)
        } else if target_top_level {
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
    /// The whole file is scanned rather than stopped at the first hit so that an
    /// ambiguous address — several entries sharing the key *and* the first field,
    /// which for `env` means the app's edit would be shadowed by the later line
    /// (see the module docs) — can be logged as the same
    /// [`Ambiguity::DoubledRepeatableField`] that [`ambiguities`](Self::ambiguities)
    /// reports. The first match is still the one returned, so read and write stay in
    /// agreement.
    fn find_repeatable(&self, key: &str, first_field: &str) -> Option<usize> {
        let matches: Vec<usize> = self
            .repeatable_entries(key)
            .into_iter()
            .filter(|(_, field)| *field == first_field)
            .map(|(index, _)| index)
            .collect();

        if matches.len() > 1 {
            let ambiguity = Ambiguity::DoubledRepeatableField {
                key: key.to_string(),
                field: first_field.to_string(),
                lines: matches.iter().map(|index| index + 1).collect(),
            };
            // The report names the key, the field and line numbers only — no file
            // contents (R7.3).
            tracing::warn!(
                %ambiguity,
                "ambiguous hyprlang address: several entries share this key and first \
                 field; using the first"
            );
        }

        matches.first().copied()
    }

    /// The top-level entries with key `key`, each as its line index and its first
    /// comma-field (trimmed), in file order.
    ///
    /// Only entries outside every section count, and comments never match. Nesting is
    /// tracked by a simple depth counter because repeatable keys (`env`, `exec-once`,
    /// `source`) live at the top level.
    fn repeatable_entries(&self, key: &str) -> Vec<(usize, &str)> {
        let mut entries = Vec::new();
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
                    entries.push((i, field));
                }
                _ => {}
            }
        }
        entries
    }

    /// Reports every address in the file that names one line while the daemon takes
    /// its value from another (task 9.31; see the module section "Reporting ambiguous
    /// addresses").
    ///
    /// `schema` says which top-level blocks are special categories and which keys are
    /// repeatable for the daemon that reads this file. Two shapes are reported:
    ///
    /// - [`Ambiguity::DoubledRepeatableField`] — two or more top-level lines of a
    ///   key in [`HyprlangSchema::repeatable_keys`] sharing their first comma-field;
    /// - [`Ambiguity::RepeatedCategory`] — a category declared more than once among
    ///   the same siblings (the top level, or the inside of one block), when the
    ///   daemon merges such blocks: a plain category, or a
    ///   [`SpecialCategory::Keyed`] one whose blocks carry the same key value. A
    ///   keyed block whose first assignment is not its key is left out:
    ///   libhyprlang requires the key to be a special category's first value and
    ///   rejects the block otherwise, so it is never merged. Repeated [`SpecialCategory::Anonymous`]
    ///   blocks are never reported — each is its own object, and callers address
    ///   them by occurrence on purpose (hypridle's `listener { }` blocks).
    ///
    /// Special categories are matched at the top level only, where the daemons
    /// declare them; a block nested inside another is always treated as plain. For a
    /// category repeated at the top level the nested blocks inside the copies are not
    /// reported separately — the outer report already covers them.
    ///
    /// The result is sorted by first line number and is empty for an unambiguous
    /// file. This only reads the parsed lines: it changes nothing, logs nothing, and
    /// does not affect how [`value`](Self::value) or [`set_value`](Self::set_value)
    /// resolve an address — logging the report is the caller's decision, so it can be
    /// done once per load rather than on every call.
    pub fn ambiguities(&self, schema: &HyprlangSchema) -> Vec<Ambiguity> {
        let mut found = Vec::new();
        for key in schema.repeatable_keys {
            found.extend(self.doubled_repeatable_fields(key));
        }
        found.extend(self.repeated_categories(schema));
        found.sort_by_key(|ambiguity| ambiguity.lines().first().copied().unwrap_or(0));
        found
    }

    /// The [`Ambiguity::DoubledRepeatableField`] reports for one repeatable `key`, one
    /// per first field that more than one line shares, in order of first appearance.
    fn doubled_repeatable_fields(&self, key: &str) -> Vec<Ambiguity> {
        let mut by_field: Vec<(&str, Vec<usize>)> = Vec::new();
        for (index, field) in self.repeatable_entries(key) {
            match by_field.iter_mut().find(|(seen, _)| *seen == field) {
                Some((_, lines)) => lines.push(index + 1),
                None => by_field.push((field, vec![index + 1])),
            }
        }
        by_field
            .into_iter()
            .filter(|(_, lines)| lines.len() > 1)
            .map(|(field, lines)| Ambiguity::DoubledRepeatableField {
                key: key.to_string(),
                field: field.to_string(),
                lines,
            })
            .collect()
    }

    /// The [`Ambiguity::RepeatedCategory`] reports, found by walking the section tree
    /// and comparing each parent's direct child blocks once the parent closes (the top
    /// level last). A section the file never closes is closed at end-of-file, the same
    /// tolerance [`resolve_path`](Self::resolve_path) applies.
    fn repeated_categories(&self, schema: &HyprlangSchema) -> Vec<Ambiguity> {
        let mut found = Vec::new();
        let mut open: Vec<OpenBlock> = Vec::new();
        let mut top_level: Vec<ChildBlock> = Vec::new();

        for (i, line) in self.lines.iter().enumerate() {
            match &line.kind {
                LineKind::SectionOpen { name } => open.push(OpenBlock {
                    name: name.clone(),
                    line: i + 1,
                    key_value: None,
                    seen_entry: false,
                    children: Vec::new(),
                }),
                LineKind::SectionClose => {
                    // A stray `}` closes nothing (already a parse warning).
                    if let Some(block) = open.pop() {
                        close_block(block, &mut open, &mut top_level, &mut found);
                    }
                }
                LineKind::Entry { key, .. } => {
                    // Record a keyed special category's identifying value. libhyprlang
                    // accepts the key only as the block's first assignment ("special
                    // category's first value must be the key"), so only a first entry
                    // directly inside the top-level block counts; a block whose first
                    // assignment is anything else keeps `None` and is left out.
                    if let [block] = open.as_mut_slice() {
                        if let Some(SpecialCategory::Keyed { key: id_key, .. }) =
                            schema.special_category(&block.name)
                        {
                            if !block.seen_entry && key == id_key {
                                block.key_value = self.entry_value(i).map(str::to_string);
                            }
                        }
                        block.seen_entry = true;
                    }
                }
                LineKind::Blank | LineKind::Comment | LineKind::Malformed => {}
            }
        }
        while let Some(block) = open.pop() {
            close_block(block, &mut open, &mut top_level, &mut found);
        }
        found.extend(repeated_siblings(&[], &top_level, Some(schema)));
        found
    }

    /// Counts the direct child sections named `name` within the section
    /// identified by `parent` (an empty `parent` means the top level).
    ///
    /// This lets a caller enumerate repeated blocks — e.g. hypridle's
    /// `listener { }` blocks via `section_count(&[], "listener")` (task 6.8) —
    /// without probing occurrence 0, 1, 2, … until a lookup fails. It composes
    /// with nesting: pass the parent path (with occurrences) to count children of
    /// a specific nested section.
    pub fn section_count(&self, parent: &[SectionStep], name: &str) -> usize {
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

        let terminator = line::terminator_if_any_crlf(&self.lines);
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
                line::ensure_terminated(&mut last.raw, terminator);
            }
            self.lines.push(new_line);
        } else {
            self.lines.insert(plan.insert_index, new_line);
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

/// A section still open during the [`HyprlangFile::ambiguities`] walk.
struct OpenBlock {
    /// The section's name.
    name: String,
    /// The 1-based line number of its header.
    line: usize,
    /// For a top-level keyed special category, the value of its key assignment when
    /// that is the block's first assignment (see [`SpecialCategory::Keyed`]); `None`
    /// otherwise.
    key_value: Option<String>,
    /// Whether a direct-child assignment of this block has been seen yet, so only the
    /// first one can supply `key_value`.
    seen_entry: bool,
    /// The direct child sections closed so far.
    children: Vec<ChildBlock>,
}

/// A closed section, as its parent sees it when comparing its children.
struct ChildBlock {
    /// The section's name.
    name: String,
    /// The 1-based line number of its header.
    line: usize,
    /// The keyed special category's identifying value, if any (see [`OpenBlock`]).
    key_value: Option<String>,
}

/// Finishes `block` in the ambiguity walk: reports repeats among its own children,
/// then hands it to its parent (the innermost section still open, or the top level).
fn close_block(
    block: OpenBlock,
    open: &mut [OpenBlock],
    top_level: &mut Vec<ChildBlock>,
    found: &mut Vec<Ambiguity>,
) {
    let mut path: Vec<String> = open.iter().map(|parent| parent.name.clone()).collect();
    path.push(block.name.clone());
    // Special categories exist only at the top level, so a nested parent's children
    // are compared as plain categories (no schema).
    found.extend(repeated_siblings(&path, &block.children, None));

    let child = ChildBlock {
        name: block.name,
        line: block.line,
        key_value: block.key_value,
    };
    match open.last_mut() {
        Some(parent) => parent.children.push(child),
        None => top_level.push(child),
    }
}

/// The [`Ambiguity::RepeatedCategory`] reports among one parent's direct children:
/// every group of blocks the daemon would merge that has more than one member.
///
/// `parent` is the parent's section path (empty for the top level). `schema` is given
/// only for the top level; a block it declares anonymous is skipped, a keyed one is
/// grouped by its key value (skipped without one), and every other block is grouped by
/// name alone.
fn repeated_siblings(
    parent: &[String],
    children: &[ChildBlock],
    schema: Option<&HyprlangSchema>,
) -> Vec<Ambiguity> {
    let mut groups: Vec<MergeGroup<'_>> = Vec::new();
    for child in children {
        let (key_value, keyed_by) =
            match schema.and_then(|schema| schema.special_category(&child.name)) {
                Some(SpecialCategory::Anonymous(_)) => continue,
                Some(SpecialCategory::Keyed { key, .. }) => match child.key_value.as_deref() {
                    Some(value) => (Some(value), Some(key)),
                    None => continue,
                },
                None => (None, None),
            };
        match groups
            .iter_mut()
            .find(|group| group.name == child.name && group.key_value == key_value)
        {
            Some(group) => group.lines.push(child.line),
            None => groups.push(MergeGroup {
                name: &child.name,
                key_value,
                keyed_by,
                lines: vec![child.line],
            }),
        }
    }
    groups
        .into_iter()
        .filter(|group| group.lines.len() > 1)
        .map(|group| {
            let mut path = parent.to_vec();
            path.push(group.name.to_string());
            Ambiguity::RepeatedCategory {
                path,
                keyed_by: group.keyed_by,
                lines: group.lines,
            }
        })
        .collect()
}

/// Sibling blocks the daemon merges into one, collected by [`repeated_siblings`].
struct MergeGroup<'a> {
    /// The blocks' shared name.
    name: &'a str,
    /// For a keyed special category, the key value the blocks share; `None` for a
    /// plain category.
    key_value: Option<&'a str>,
    /// For a keyed special category, the key that value belongs to.
    keyed_by: Option<&'static str>,
    /// The 1-based header line numbers, in file order.
    lines: Vec<usize>,
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

/// The characters the hyprlang writer refuses in a new value: a newline /
/// carriage return (splits the line) or a `#` (starts an inline comment, silently
/// truncating the value).
///
/// Braces are deliberately *not* in the set: hyprlang tolerates `{`/`}` inside a
/// value, and semantic validation of typed values (hex, `WxH@Hz`, ranges, paths)
/// is the settings model's job (task 4.1), not this byte-level structural guard
/// (R8.3).
///
/// Public so a caller can check a value against exactly this writer's rule *before*
/// staging it — the wallpaper model's stage-time validation does, see
/// [`crate::core::value_writers`] — instead of restating the set.
pub const FORBIDDEN_VALUE_CHARACTERS: [char; 3] = ['\n', '\r', '#'];

/// Rejects a new value that would break hyprlang parsing or the file's line
/// structure — one containing any of the [`FORBIDDEN_VALUE_CHARACTERS`].
fn reject_unsafe_value(value: &str) -> Result<(), EditError> {
    if line::find_forbidden(value, &FORBIDDEN_VALUE_CHARACTERS).is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::differing_lines;

    /// A two-level nested block in the shape of Hyprland's former hyprlang input
    /// configuration: a header comment, an `input { }` block with flat keys
    /// (some with empty values), and a nested `touchpad { }` block with a hyphenated
    /// key. Uses the compact `key=value` style (no spaces around `=`).
    const NESTED_BLOCK: &str = "\
# Keyboard layout/options, sensitivity, touchpad.
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

    /// A hyprlang file with repeatable top-level keys, shaped like Hyprland's former
    /// hyprlang main config: `source =` directives, two `env =` lines, a
    /// commented-out decoy `env` line, a commented-out `exec-once`, and a real
    /// `exec-once`.
    const REPEATABLE_KEYS: &str = "\
# Hyprland main config (excerpt)
source = ~/.config/hypr/colors.conf
source = ~/.config/hypr/monitors.conf

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

    /// A Hyprland-shaped hyprlang excerpt exercising the broadened key rule:
    /// `$variable` declarations, a `general { }` block with dotted keys
    /// (`col.active_border`), a nested `decoration { blur { } }` block with an
    /// inline comment, the cursor `env =` lines, and a `bind =` line. None of
    /// these may be misclassified as malformed.
    const DOTTED_AND_VARIABLE_KEYS: &str = "\
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
            ("nested block", NESTED_BLOCK),
            ("hypridle.conf", HYPRIDLE_CONF),
            ("hyprpaper.conf", HYPRPAPER_CONF),
            ("repeatable keys", REPEATABLE_KEYS),
            ("dotted and variable keys", DOTTED_AND_VARIABLE_KEYS),
            ("hyprlock.conf", HYPRLOCK_CONF),
            ("nested duplicate", NESTED_DUPLICATE),
        ]
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
    fn edge_case_inputs_round_trip() {
        // Empty input, a missing final newline, comment-only and blank-only files.
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
        let (mut file, _) = HyprlangFile::parse(NESTED_BLOCK);
        let path = KeyPath::at(&["input", "touchpad"], "natural_scroll");
        assert_eq!(file.value(&path), Some("true"));

        file.set_value(&path, "false").expect("editable entry");
        let edited = file.emit();

        let changed = differing_lines(NESTED_BLOCK, &edited);
        let target = NESTED_BLOCK
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

        let changed = differing_lines(HYPRIDLE_CONF, &edited);
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
        let (mut file, _) = HyprlangFile::parse(REPEATABLE_KEYS);
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

        let changed = differing_lines(REPEATABLE_KEYS, &edited);
        let target = REPEATABLE_KEYS
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
        let (file, _) = HyprlangFile::parse(REPEATABLE_KEYS);
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
    fn an_edit_keeps_a_trailing_inline_comment() {
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
        let (mut file, _) = HyprlangFile::parse(NESTED_BLOCK);
        let path = KeyPath::at(&["input", "touchpad"], "disable_while_typing");
        assert_eq!(file.value(&path), None);

        file.set_value(&path, "true").expect("appends a new key");
        let expected = NESTED_BLOCK.replace(
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
    fn appending_into_an_empty_section_indents_one_level() {
        // With no sibling assignment to copy, an appended key uses the section
        // header's indentation plus four spaces and a ` = ` separator.
        let (mut file, _) = HyprlangFile::parse("region {\n}\n");
        file.set_value(&KeyPath::at(&["region"], "key"), "val")
            .expect("appends into an empty section");
        assert_eq!(file.emit(), "region {\n    key = val\n}\n");
    }

    #[test]
    fn a_malformed_line_is_kept_with_a_warning() {
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
        let (mut file, _) = HyprlangFile::parse(NESTED_BLOCK);
        let path = KeyPath::at(&["input"], "kb_layout");
        for bad in ["us\nse", "us # comment", "us\rse"] {
            assert_eq!(
                file.set_value(&path, bad),
                Err(EditError::InvalidValue(bad.to_string()))
            );
        }
        assert_eq!(
            file.emit(),
            NESTED_BLOCK,
            "no rejected edit changed the file"
        );

        let (mut env, _) = HyprlangFile::parse(REPEATABLE_KEYS);
        assert_eq!(
            env.set_repeatable_field_value("env", "XCURSOR_THEME", "a#b"),
            Err(EditError::InvalidValue("a#b".to_string()))
        );
        assert_eq!(env.emit(), REPEATABLE_KEYS);
    }

    #[test]
    fn repeatable_edit_reports_a_missing_target() {
        let (mut file, _) = HyprlangFile::parse(REPEATABLE_KEYS);
        assert_eq!(
            file.set_repeatable_field_value("env", "GDK_BACKEND", "wayland"),
            Err(EditError::RepeatableKeyNotFound {
                key: "env".to_string(),
                field: "GDK_BACKEND".to_string(),
            })
        );
        assert_eq!(file.emit(), REPEATABLE_KEYS);
    }

    #[test]
    fn crlf_line_endings_survive_an_edit() {
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
    fn edits_the_last_occurrence_of_a_duplicate_key_in_a_section() {
        // Hyprlang applies a file top to bottom, so a key assigned twice in one
        // section resolves to the second assignment. Reading and editing both
        // target that one (task 9.8) and the shadowed first copy stays
        // byte-identical, so the app never displays or rewrites a value the
        // daemon ignores.
        let input = "s {\n    k = one\n    k = two\n}\n";
        let (mut file, _) = HyprlangFile::parse(input);
        let path = KeyPath::at(&["s"], "k");
        assert_eq!(
            file.value(&path),
            Some("two"),
            "the effective (last) value is read"
        );

        file.set_value(&path, "edited").expect("last k is editable");
        assert_eq!(file.emit(), "s {\n    k = one\n    k = edited\n}\n");
    }

    #[test]
    fn a_duplicated_top_level_key_resolves_to_the_last() {
        // The same rule outside any section, where the walk runs to end-of-file
        // instead of stopping at a closing brace.
        let input = "splash = false\nipc = on\nsplash = true\n";
        let (mut file, _) = HyprlangFile::parse(input);
        let path = KeyPath::top_level("splash");
        assert_eq!(file.value(&path), Some("true"));

        file.set_value(&path, "false")
            .expect("last splash is editable");
        assert_eq!(file.emit(), "splash = false\nipc = on\nsplash = false\n");
    }

    #[test]
    fn the_last_key_rule_stays_inside_the_addressed_section() {
        // A `KeyPath` names one section *instance*, so a key repeated in another
        // occurrence of the same section name is not treated as a shadowing copy:
        // editing occurrence 0 rewrites the last `k` inside that block and leaves
        // the second block byte-identical. This is deliberate — a repeated block
        // can be a distinct object (hypridle's `listener { }` blocks), which
        // merging would corrupt — even though hyprlang itself merges two plain
        // `s { }` blocks and lets the later one win (see the module docs).
        let input = "s {\n    k = one\n    k = two\n}\ns {\n    k = three\n}\n";
        let (mut file, _) = HyprlangFile::parse(input);
        let first_block = KeyPath::new(vec![SectionStep::nth("s", 0)], "k");
        assert_eq!(file.value(&first_block), Some("two"));
        assert_eq!(
            file.value(&KeyPath::new(vec![SectionStep::nth("s", 1)], "k")),
            Some("three")
        );

        file.set_value(&first_block, "edited")
            .expect("the first block's last k is editable");
        assert_eq!(
            file.emit(),
            "s {\n    k = one\n    k = edited\n}\ns {\n    k = three\n}\n"
        );
    }

    #[test]
    fn a_nested_key_does_not_shadow_its_parents_key() {
        // A same-named key in a child block belongs to the child, so it must
        // neither be read as, nor edited instead of, the parent section's own last
        // assignment (the last-occurrence walk only considers direct children).
        let input = "input {\n    k = outer\n    touchpad {\n        k = inner\n    }\n}\n";
        let (mut file, _) = HyprlangFile::parse(input);
        let parent = KeyPath::at(&["input"], "k");
        assert_eq!(file.value(&parent), Some("outer"));
        assert_eq!(
            file.value(&KeyPath::at(&["input", "touchpad"], "k")),
            Some("inner")
        );

        file.set_value(&parent, "edited")
            .expect("the parent's k is editable");
        assert_eq!(
            file.emit(),
            "input {\n    k = edited\n    touchpad {\n        k = inner\n    }\n}\n"
        );

        // The nested-block case where the two rules interact: the parent's
        // *last* assignment comes after the child block has closed, so descending
        // into the child and coming back out must not lose track of the parent
        // level. The old first-match walk answered `one` here.
        let interleaved =
            "input {\n    k = one\n    touchpad {\n        k = inner\n    }\n    k = two\n}\n";
        let (mut file, _) = HyprlangFile::parse(interleaved);
        assert_eq!(file.value(&parent), Some("two"));

        file.set_value(&parent, "edited")
            .expect("the parent's last k is editable");
        assert_eq!(
            file.emit(),
            "input {\n    k = one\n    touchpad {\n        k = inner\n    }\n    k = edited\n}\n"
        );
    }

    #[test]
    fn a_duplicate_key_in_an_unclosed_section_resolves_to_the_last() {
        // A file whose target section is never closed still addresses its keys
        // (the parser tolerates the imbalance and only warns), so the
        // last-occurrence walk must apply on the end-of-file path too, and the
        // missing terminator must survive the edit.
        let input = "s {\n    k = one\n    k = two\n";
        let (mut file, warnings) = HyprlangFile::parse(input);
        assert_eq!(
            warnings,
            vec![ParseWarning {
                line: 1,
                kind: ParseWarningKind::UnclosedSection {
                    name: "s".to_string()
                },
            }]
        );

        let path = KeyPath::at(&["s"], "k");
        assert_eq!(file.value(&path), Some("two"));

        file.set_value(&path, "edited").expect("last k is editable");
        assert_eq!(file.emit(), "s {\n    k = one\n    k = edited\n");
    }

    #[test]
    fn a_duplicate_key_inside_one_listener_resolves_to_the_last() {
        // The shape of the highest-risk consumer (`core/power.rs`, task 6.8):
        // hypridle's `listener { }` blocks are addressed positionally, and a
        // duplicated `timeout` *within* one block resolves to that block's last
        // assignment. Pins both rules at once — the second listener is edited (not
        // the first or third), at its second `timeout` (not its first).
        let input = "listener {\n    timeout = 150\n}\n\nlistener {\n    timeout = 300\n    timeout = 330\n}\n";
        let (mut file, _) = HyprlangFile::parse(input);
        let second = KeyPath::new(vec![SectionStep::nth("listener", 1)], "timeout");
        assert_eq!(file.value(&second), Some("330"));
        assert_eq!(
            file.value(&KeyPath::new(
                vec![SectionStep::nth("listener", 0)],
                "timeout"
            )),
            Some("150"),
            "the first listener is unaffected by the second's duplicate"
        );

        file.set_value(&second, "600")
            .expect("the second listener's last timeout is editable");
        assert_eq!(
            file.emit(),
            "listener {\n    timeout = 150\n}\n\nlistener {\n    timeout = 300\n    timeout = 600\n}\n"
        );
    }

    #[test]
    fn a_doubled_repeatable_field_resolves_to_the_first_line() {
        // Two `env` lines sharing the same first field are an ambiguous address:
        // the parser resolves both reading and writing to the first line — so it
        // never displays one copy while editing another — and logs a `warn` built
        // from the same report `ambiguities` returns (hyprland itself would apply
        // the *later* line). This test pins that documented behaviour, including
        // the log the docs promise.
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing::subscriber::with_default;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct LogCapture {
            events: Arc<Mutex<Vec<(Level, String)>>>,
        }

        struct MessageVisitor<'a>(&'a mut String);
        impl tracing::field::Visit for MessageVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                if field.name() == "message" {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{value:?}");
                }
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for LogCapture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut message = String::new();
                event.record(&mut MessageVisitor(&mut message));
                self.events
                    .lock()
                    .expect("log capture mutex should not be poisoned")
                    .push((*event.metadata().level(), message));
            }
        }

        let input =
            "env = XCURSOR_THEME,First\nenv = XCURSOR_SIZE,16\nenv = XCURSOR_THEME,Second\n";
        let (mut file, _) = HyprlangFile::parse(input);

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        with_default(subscriber, || {
            // `tracing` caches each callsite's interest process-globally, so a
            // parallel test that reached this `warn!` under the no-op default
            // subscriber could have cached it as "never" and suppressed the event.
            // Touch the callsite once, rebuild the cache so it is re-evaluated
            // against this thread's subscriber, then measure (the same guard
            // `core::detect`'s log test uses).
            let _ = file.repeatable_field_value("env", "XCURSOR_THEME");
            tracing::callsite::rebuild_interest_cache();
            capture
                .events
                .lock()
                .expect("log capture mutex should not be poisoned")
                .clear();

            assert_eq!(
                file.repeatable_field_value("env", "XCURSOR_THEME"),
                Some("First"),
                "the first of the two matching lines is read"
            );
            file.set_repeatable_field_value("env", "XCURSOR_THEME", "Edited")
                .expect("the first matching line is editable");
            // The ordinary shape the cursor-env writer works on — one line per
            // first field — must stay silent, so the warning cannot become noise
            // on a healthy file.
            assert_eq!(
                file.repeatable_field_value("env", "XCURSOR_SIZE"),
                Some("16"),
                "an unambiguous field reads normally"
            );
        });

        assert_eq!(
            file.emit(),
            "env = XCURSOR_THEME,Edited\nenv = XCURSOR_SIZE,16\nenv = XCURSOR_THEME,Second\n",
            "only the first matching line changed"
        );

        let events = capture
            .events
            .lock()
            .expect("log capture mutex should not be poisoned");
        let warnings: Vec<&String> = events
            .iter()
            .filter(|(level, _)| *level == Level::WARN)
            .map(|(_, message)| message)
            .collect();
        assert_eq!(
            warnings.len(),
            2,
            "exactly the ambiguous read and the ambiguous write warn — the \
             unambiguous XCURSOR_SIZE read must not — got {events:?}"
        );
        assert!(
            warnings
                .iter()
                .all(|message| message.contains("share this key and first field")),
            "the warning names the ambiguity, got {warnings:?}"
        );
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
    fn dotted_and_variable_keys_classify_as_entries() {
        // Real hyprland lines must not misfire as malformed. Round-trip is
        // byte-identical, there are zero parse warnings, and dotted keys and
        // `$variable` declarations are addressable ordinary entries.
        let (file, warnings) = HyprlangFile::parse(DOTTED_AND_VARIABLE_KEYS);
        assert_eq!(file.emit(), DOTTED_AND_VARIABLE_KEYS);
        assert!(
            warnings.is_empty(),
            "a Hyprland-shaped hyprlang file must yield no warnings, got {warnings:?}"
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
        // The production cursor-env line may have a space after the comma;
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

    /// hypridle's schema as the tests below need it: `listener` is an anonymous
    /// special category, nothing is repeatable.
    const LISTENERS_ARE_OBJECTS: HyprlangSchema = HyprlangSchema {
        special_categories: &[SpecialCategory::Anonymous("listener")],
        repeatable_keys: &[],
    };

    /// A schema in which `env` is a repeatable key, as on Hyprland's former hyprlang
    /// main config.
    const ENV_IS_REPEATABLE: HyprlangSchema = HyprlangSchema {
        special_categories: &[],
        repeatable_keys: &["env"],
    };

    /// hyprpaper's schema: `wallpaper` is a special category keyed by `monitor`.
    const WALLPAPERS_BY_MONITOR: HyprlangSchema = HyprlangSchema {
        special_categories: &[SpecialCategory::Keyed {
            name: "wallpaper",
            key: "monitor",
        }],
        repeatable_keys: &[],
    };

    #[test]
    fn a_doubled_repeatable_first_field_is_reported_with_key_field_and_lines() {
        // Task 9.31, shape (a): two `env` lines sharing `XCURSOR_THEME` around an
        // unrelated `env` line. The report names the key, the field and both line
        // numbers, and its sentence form says the same.
        let input =
            "env = XCURSOR_THEME,First\nenv = XCURSOR_SIZE,16\nenv = XCURSOR_THEME,Second\n";
        let (file, _) = HyprlangFile::parse(input);
        let report = file.ambiguities(&ENV_IS_REPEATABLE);
        assert_eq!(
            report,
            vec![Ambiguity::DoubledRepeatableField {
                key: "env".to_string(),
                field: "XCURSOR_THEME".to_string(),
                lines: vec![1, 3],
            }]
        );
        assert_eq!(
            report[0].to_string(),
            "Two `env` lines share the first field `XCURSOR_THEME` (lines 1 and 3)"
        );
        assert_eq!(report[0].top_level_category(), None);

        // Whether a key is repeatable is the schema's call: without it the same
        // file reports nothing (a plain top-level key resolves to its last copy).
        assert!(file.ambiguities(&HyprlangSchema::default()).is_empty());
    }

    #[test]
    fn a_commented_out_copy_of_a_repeatable_field_is_not_a_duplicate() {
        // The fixture's decoy `#env = XCURSOR_THEME,…` is a comment, so the one
        // real XCURSOR_THEME line is unambiguous.
        let (file, _) = HyprlangFile::parse(REPEATABLE_KEYS);
        assert!(file.ambiguities(&ENV_IS_REPEATABLE).is_empty());
    }

    #[test]
    fn a_twice_declared_plain_category_is_reported_with_its_lines() {
        // Task 9.31, shape (b): hypridle's `general { }` written twice. hyprlang
        // merges the blocks (the later assignment wins) while a KeyPath addresses
        // one of them, so the repeat is reported by category and header lines. The
        // three listeners beside it are not.
        let input = "\
general {
    lock_cmd = hyprlock
}

listener {
    timeout = 150
}

general {
    lock_cmd = swaylock
}
";
        let (file, _) = HyprlangFile::parse(input);
        let report = file.ambiguities(&LISTENERS_ARE_OBJECTS);
        assert_eq!(
            report,
            vec![Ambiguity::RepeatedCategory {
                path: vec!["general".to_string()],
                keyed_by: None,
                lines: vec![1, 9],
            }]
        );
        assert_eq!(
            report[0].to_string(),
            "`general` is declared twice (lines 1 and 9)"
        );
        assert_eq!(report[0].top_level_category(), Some("general"));
        // The report is read-only: the file still emits byte for byte, and the
        // address still resolves to the first block as before.
        assert_eq!(file.emit(), input);
        assert_eq!(
            file.value(&KeyPath::at(&["general"], "lock_cmd")),
            Some("hyprlock")
        );
    }

    #[test]
    fn repeated_anonymous_blocks_are_not_ambiguous() {
        // Three `listener { }` blocks are three listeners, addressed by occurrence
        // on purpose: no report under hypridle's schema. The same file under an
        // empty schema does report them, which proves it is the declaration — not
        // an accident of the walk — that excludes them.
        let (file, _) = HyprlangFile::parse(HYPRIDLE_CONF);
        assert!(file.ambiguities(&LISTENERS_ARE_OBJECTS).is_empty());
        assert_eq!(
            file.ambiguities(&HyprlangSchema::default()),
            vec![Ambiguity::RepeatedCategory {
                path: vec!["listener".to_string()],
                keyed_by: None,
                lines: vec![7, 13, 18],
            }]
        );
        assert_eq!(
            file.ambiguities(&HyprlangSchema::default())[0].to_string(),
            "`listener` is declared 3 times (lines 7, 13 and 18)"
        );
    }

    #[test]
    fn keyed_blocks_are_ambiguous_only_when_they_share_the_key_value() {
        // hyprpaper merges `wallpaper` blocks naming the same monitor, and keeps
        // blocks for different monitors apart.
        let same = "wallpaper {\n    monitor =\n    path = /a.png\n}\nwallpaper {\n    monitor =\n    path = /b.png\n}\n";
        let (file, _) = HyprlangFile::parse(same);
        let report = file.ambiguities(&WALLPAPERS_BY_MONITOR);
        assert_eq!(
            report,
            vec![Ambiguity::RepeatedCategory {
                path: vec!["wallpaper".to_string()],
                keyed_by: Some("monitor"),
                lines: vec![1, 5],
            }]
        );
        assert_eq!(
            report[0].to_string(),
            "`wallpaper` is declared twice for the same `monitor` (lines 1 and 5)"
        );

        let different = "wallpaper {\n    monitor = DP-1\n    path = /a.png\n}\nwallpaper {\n    monitor = eDP-1\n    path = /b.png\n}\n";
        let (file, _) = HyprlangFile::parse(different);
        assert!(file.ambiguities(&WALLPAPERS_BY_MONITOR).is_empty());

        // A block without its key is rejected by the daemon rather than merged, so
        // it is not reported either.
        let keyless = "wallpaper {\n    path = /a.png\n}\nwallpaper {\n    path = /b.png\n}\n";
        let (file, _) = HyprlangFile::parse(keyless);
        assert!(file.ambiguities(&WALLPAPERS_BY_MONITOR).is_empty());

        // Nor is one whose key comes after another assignment: libhyprlang requires
        // the key to be the first value, so such a block is rejected too.
        let key_not_first = "wallpaper {\n    path = /a.png\n    monitor =\n}\nwallpaper {\n    path = /b.png\n    monitor =\n}\n";
        let (file, _) = HyprlangFile::parse(key_not_first);
        assert!(file.ambiguities(&WALLPAPERS_BY_MONITOR).is_empty());

        // The real file's single block is clean.
        let (file, _) = HyprlangFile::parse(HYPRPAPER_CONF);
        assert!(file.ambiguities(&WALLPAPERS_BY_MONITOR).is_empty());
    }

    #[test]
    fn a_nested_repeat_is_reported_by_its_full_path() {
        // Two `fingerprint { }` blocks inside one `auth { }` merge like any plain
        // category; the report carries the dotted path. Special categories are a
        // top-level notion, so a nested block named like one is still plain.
        let input = "auth {\n    fingerprint {\n        enabled = true\n    }\n    fingerprint {\n        enabled = false\n    }\n}\n";
        let (file, _) = HyprlangFile::parse(input);
        let report = file.ambiguities(&HyprlangSchema::default());
        assert_eq!(
            report,
            vec![Ambiguity::RepeatedCategory {
                path: vec!["auth".to_string(), "fingerprint".to_string()],
                keyed_by: None,
                lines: vec![2, 5],
            }]
        );
        assert_eq!(
            report[0].to_string(),
            "`auth.fingerprint` is declared twice (lines 2 and 5)"
        );
        assert_eq!(report[0].top_level_category(), Some("auth"));

        // Blocks under *different* parent instances are not siblings: a repeated
        // `input` with a `touchpad` in each is reported once, at `input`.
        let outer = "input {\n    touchpad {\n    }\n}\ninput {\n    touchpad {\n    }\n}\n";
        let (file, _) = HyprlangFile::parse(outer);
        assert_eq!(
            file.ambiguities(&HyprlangSchema::default()),
            vec![Ambiguity::RepeatedCategory {
                path: vec!["input".to_string()],
                keyed_by: None,
                lines: vec![1, 5],
            }]
        );
    }

    #[test]
    fn an_unclosed_repeat_is_still_reported() {
        // A second `general {` the file never closes still merges in hyprlang's
        // reading; the walk closes it at end-of-file like `resolve_path` does.
        let (file, _) = HyprlangFile::parse("general {\n    a = 1\n}\ngeneral {\n    b = 2\n");
        assert_eq!(
            file.ambiguities(&HyprlangSchema::default()),
            vec![Ambiguity::RepeatedCategory {
                path: vec!["general".to_string()],
                keyed_by: None,
                lines: vec![1, 4],
            }]
        );
    }

    #[test]
    fn reports_are_sorted_by_first_line() {
        // Both shapes in one file come back in file order, whichever walk found them.
        let input = "\
env = A,1
general {
}
general {
}
env = A,2
";
        let (file, _) = HyprlangFile::parse(input);
        let lines: Vec<Vec<usize>> = file
            .ambiguities(&ENV_IS_REPEATABLE)
            .iter()
            .map(|ambiguity| ambiguity.lines().to_vec())
            .collect();
        assert_eq!(lines, vec![vec![1, 6], vec![2, 4]]);
    }

    #[test]
    fn the_well_formed_fixtures_are_unambiguous_under_their_schemas() {
        // Every realistic fixture here declares each plain category once. Listeners
        // (hypridle) and labels/backgrounds (hyprlock) are special categories.
        let hyprlock = HyprlangSchema {
            special_categories: &[
                SpecialCategory::Anonymous("background"),
                SpecialCategory::Anonymous("label"),
            ],
            repeatable_keys: &[],
        };
        for (name, input, schema) in [
            ("hypridle.conf", HYPRIDLE_CONF, LISTENERS_ARE_OBJECTS),
            ("hyprpaper.conf", HYPRPAPER_CONF, WALLPAPERS_BY_MONITOR),
            ("hyprlock.conf", HYPRLOCK_CONF, hyprlock),
            ("repeatable keys", REPEATABLE_KEYS, ENV_IS_REPEATABLE),
            ("nested block", NESTED_BLOCK, HyprlangSchema::default()),
            (
                "dotted and variable keys",
                DOTTED_AND_VARIABLE_KEYS,
                ENV_IS_REPEATABLE,
            ),
        ] {
            let (file, _) = HyprlangFile::parse(input);
            assert!(
                file.ambiguities(&schema).is_empty(),
                "{name}: {:?}",
                file.ambiguities(&schema)
            );
        }
    }
}
