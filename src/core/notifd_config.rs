//! notifd's file-backed settings: `~/.config/notifd/config` read, validated and
//! rewritten for the Notifications page (task 11.15; architecture §3, §6; analysis §8.2;
//! R4.2, R4.4, R5.1, R5.3, R8.3, R6.2).
//!
//! # What this module is
//!
//! The dotfiles' notification daemon, notifd (`scripts/notifd`, version 1.1 and later),
//! reads seven tunables from a flat `key=value` file at `~/.config/notifd/config` and
//! re-reads it when its D-Bus method `Reload()` is called. The Notifications page offers
//! those seven as ordinary staged settings ([`SettingId::NotifdTimeoutLow`] and its
//! siblings): they are staged in the shared store like the Input and Power & Idle
//! settings, written by the normal Apply pipeline, and followed by exactly one
//! `Reload()` call ([`BackingFile::NotifdConfig`]). The page's other half — do not
//! disturb, the unread count, "clear all" — is runtime-only and lives in
//! [`crate::core::notifications`].
//!
//! This module holds everything specific to the file: the key table with notifd's own
//! ranges and defaults ([`NOTIFD_SETTINGS`]), the read rule ([`read_values`]), the
//! surgical writer ([`render_notifd_config`]) and the page's Apply-time write glue
//! ([`NotifdConfigModel`]). It is GTK-free and process-free, so all of it is unit-tested
//! headlessly (R6.2).
//!
//! # The file format and the daemon's reading of it
//!
//! The grammar is the one the palette sources use, so the file goes through the same
//! lossless line model, [`crate::parsers::kv`]: one `key=value` per line, `#` comment
//! lines and blank lines, whitespace around the key and the value ignored, **no inline
//! comments** (everything after the first `=` is the value). The reading rules below are
//! those of notifd's `Config.load` and `Config.parse` (dotfiles commit `8a5b20b`), which
//! this module mirrors exactly, because a value the two disagree about would show one
//! thing on the page while the daemon does another:
//!
//! - **Integers** must be ASCII decimal digits only (no sign, no spaces inside, leading
//!   zeros allowed) and lie in the key's range: `0..=2^31-1` (the largest GLib timer
//!   interval), except `history_max` (at least 1) and `critical_per_app` (`1..=5`, the
//!   number of popup slots).
//! - **Booleans** (`copy_codes`) accept `true`, `false`, `1` and `0`, case-insensitively.
//! - A malformed line, an unknown key or a value that fails its rule is logged by the
//!   daemon and **ignored**; a key without a valid value takes its **default** (the
//!   values the shipped file lists). Nothing in the file can stop the daemon.
//! - **A key assigned on several lines**: the daemon applies the lines in order, so each
//!   valid assignment overwrites the previous one and an invalid one changes nothing —
//!   the **last valid** assignment wins. (Its log line for an invalid value says
//!   "keeping <default>", but the code keeps whatever an earlier line set.) The app reads
//!   with the same rule and writes to the **last** assignment of the key, so the value it
//!   writes is always the one in effect afterwards.
//!
//! # Write safety (R8.3)
//!
//! Because the daemon silently falls back to its default for a value it rejects, an
//! out-of-range value would not break the desktop but would quietly not take effect. So
//! every value is checked against the daemon's rule twice: when it is staged
//! ([`SettingId::validate`] → [`validate_integer`], so the control refuses it on the
//! spot), and again by the writer ([`NotifdRenderError::Rejected`]). Values are written in
//! the canonical spelling: plain decimal integers, `true`/`false`. A key the file lacks —
//! the shipped file lists all seven, but a hand-trimmed one may not — is seeded from the
//! daemon's default on read and appended as a bare `key=value` line at the end of the
//! file when the user changes it.
//!
//! # Files the app refuses to read
//!
//! notifd splits lines with Python's `str.splitlines`, which also breaks on a lone `\r`,
//! form feed, vertical tab, `\x1c`–`\x1e`, `\u{85}`, `\u{2028}` and `\u{2029}`, and strips
//! `\x1f` as whitespace; the shared line model splits on `\n` only. A file containing any
//! of those characters (or any other control character besides tab) could therefore be
//! read differently by the two, so it is refused ([`NotifdConfigUnavailable`]) and the
//! staged rows stay hidden rather than guessing (architecture §10). A file that is not
//! UTF-8 is refused too: the daemon decodes it with replacement characters, and writing
//! such a file back through a lossy decoding would change bytes the app never meant to
//! touch.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, WriteValidation};
use crate::core::model::{SettingId, ValidationError, Value};
use crate::core::notifd::NotifdStatus;
use crate::core::reload::BackingFile;
use crate::core::store_write::{RenderedEdit, StoreBackedFile, StoreWriteError};
use crate::parsers::kv::{KvEditError, KvFile, Occurrence};

/// The largest integer notifd accepts for any key: `2^31 - 1`, the largest interval a
/// GLib timer takes (analysis §8.2).
pub const NOTIFD_INT_MAX: i64 = 2_147_483_647;

/// The number of popup slots notifd draws (fixed in its eww widgets) and therefore the
/// upper bound of `critical_per_app`.
pub const NOTIFD_POPUP_SLOTS: i64 = 5;

/// The value rule of one notifd key, with the daemon's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifdRule {
    /// A whole number in `min..=max`, written as plain decimal.
    Integer {
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
        /// The value notifd uses when the key is missing or invalid.
        default: i64,
    },
    /// A boolean, written as `true`/`false` (read also as `1`/`0`, any case).
    Bool {
        /// The value notifd uses when the key is missing or invalid.
        default: bool,
    },
}

/// One key of `~/.config/notifd/config`: the setting it backs, its name in the file
/// and the daemon's rule for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifdSetting {
    /// The store setting this key backs.
    pub id: SettingId,
    /// The key as written in the file.
    pub key: &'static str,
    /// The daemon's value rule and default.
    pub rule: NotifdRule,
}

/// The seven notifd keys, in the order the shipped file lists them, with the ranges and
/// defaults of notifd's `Config.DEFAULTS`/`MIN`/`MAX` (dotfiles commit `8a5b20b`,
/// analysis §8.2).
///
/// This table is the app's copy of a daemon contract (architecture §10): should notifd
/// change a range, this table must follow, or the app would refuse a value the daemon
/// accepts (or, worse, write one it ignores).
pub const NOTIFD_SETTINGS: [NotifdSetting; 7] = [
    NotifdSetting {
        id: SettingId::NotifdTimeoutLow,
        key: "timeout_low_ms",
        rule: NotifdRule::Integer {
            min: 0,
            max: NOTIFD_INT_MAX,
            default: 5000,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdTimeoutNormal,
        key: "timeout_normal_ms",
        rule: NotifdRule::Integer {
            min: 0,
            max: NOTIFD_INT_MAX,
            default: 10_000,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdTimeoutCritical,
        key: "timeout_critical_ms",
        rule: NotifdRule::Integer {
            min: 0,
            max: NOTIFD_INT_MAX,
            default: 0,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdHistoryMax,
        key: "history_max",
        rule: NotifdRule::Integer {
            min: 1,
            max: NOTIFD_INT_MAX,
            default: 100,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdCriticalPerApp,
        key: "critical_per_app",
        rule: NotifdRule::Integer {
            min: 1,
            max: NOTIFD_POPUP_SLOTS,
            default: 3,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdHoldMin,
        key: "hold_min_ms",
        rule: NotifdRule::Integer {
            min: 0,
            max: NOTIFD_INT_MAX,
            default: 1500,
        },
    },
    NotifdSetting {
        id: SettingId::NotifdCopyCodes,
        key: "copy_codes",
        rule: NotifdRule::Bool { default: true },
    },
];

impl NotifdSetting {
    /// The table entry for `id`, or `None` for a setting notifd's file does not back.
    pub fn for_id(id: SettingId) -> Option<&'static NotifdSetting> {
        NOTIFD_SETTINGS.iter().find(|setting| setting.id == id)
    }

    /// The daemon's default, as the store value the page shows for a key the file
    /// lacks or holds no valid value for.
    pub fn default_value(&self) -> Value {
        match self.rule {
            NotifdRule::Integer { default, .. } => Value::Integer(default),
            NotifdRule::Bool { default } => Value::Bool(default),
        }
    }

    /// Reads a value from the file the way notifd does, or `None` when the daemon
    /// would reject it (see the module docs). `text` is the whitespace-trimmed value.
    pub fn parse(&self, text: &str) -> Option<Value> {
        match self.rule {
            NotifdRule::Integer { min, max, .. } => {
                parse_notifd_integer(text, min, max).map(Value::Integer)
            }
            NotifdRule::Bool { .. } => parse_notifd_bool(text).map(Value::Bool),
        }
    }

    /// The canonical on-disk spelling of `value` for this key, or the reason it may not
    /// be written: the wrong kind, or a number outside the daemon's range (R8.3).
    pub fn render(&self, value: &Value) -> Result<String, NotifdRenderError> {
        match (self.rule, value) {
            (NotifdRule::Integer { min, max, .. }, Value::Integer(number)) => {
                if (min..=max).contains(number) {
                    Ok(number.to_string())
                } else {
                    Err(NotifdRenderError::Rejected {
                        key: self.key,
                        value: number.to_string(),
                    })
                }
            }
            (NotifdRule::Bool { .. }, Value::Bool(flag)) => Ok(flag.to_string()),
            _ => Err(NotifdRenderError::ValueKind {
                key: self.key,
                value: value.clone(),
            }),
        }
    }
}

/// notifd's integer rule: ASCII decimal digits only, then `min..=max` (its
/// `value.isascii() and value.isdecimal()` check followed by the range test).
///
/// An empty string, a sign, inner whitespace, a decimal point or a digit outside ASCII
/// fails. Leading zeros are accepted, as Python's `int()` accepts them. A digit string
/// too long for `i64` is far outside every range and fails too.
pub fn parse_notifd_integer(text: &str, min: i64, max: i64) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number: i64 = text.parse().ok()?;
    (min..=max).contains(&number).then_some(number)
}

/// notifd's boolean rule: `true`/`1` or `false`/`0`, case-insensitively (its
/// `value.lower() in (…)` check).
pub fn parse_notifd_bool(text: &str) -> Option<bool> {
    if text.eq_ignore_ascii_case("true") || text == "1" {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") || text == "0" {
        Some(false)
    } else {
        None
    }
}

/// The stage-time check of a notifd integer setting (R8.3): `value` must lie in the
/// range notifd accepts for `id`'s key, or the daemon would ignore it and use its
/// default. Called by [`SettingId::validate`]; a setting with no notifd key passes.
pub fn validate_integer(id: SettingId, value: i64) -> Result<(), ValidationError> {
    let Some(setting) = NotifdSetting::for_id(id) else {
        return Ok(());
    };
    match setting.rule {
        NotifdRule::Integer { min, max, .. } if !(min..=max).contains(&value) => {
            Err(ValidationError::NotifdOutOfRange {
                key: setting.key,
                value,
                min,
                max,
            })
        }
        NotifdRule::Integer { .. } | NotifdRule::Bool { .. } => Ok(()),
    }
}

/// A line of the file whose value notifd ignores (see [`read_values`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IgnoredValue {
    /// The 1-based line number.
    pub line: usize,
    /// The key the line assigns.
    pub key: &'static str,
}

/// What [`read_values`] found in the file.
#[derive(Clone, Debug, PartialEq)]
pub struct NotifdValues {
    /// One value per notifd setting, in [`NOTIFD_SETTINGS`] order: the value in effect
    /// for the daemon — the last valid assignment, else the default.
    pub values: Vec<(SettingId, Value)>,
    /// Keys with no assignment at all; their value above is the default, and a change
    /// appends them to the file.
    pub missing: Vec<&'static str>,
    /// Assignments the daemon ignores because their value fails the key's rule. Line
    /// numbers and keys only, so they can be logged without quoting the file (R7.3).
    pub ignored: Vec<IgnoredValue>,
}

/// Reads the seven settings from `file` with the daemon's rule (see the module docs):
/// each key's last valid assignment wins, a key with none takes its default. Lines with
/// unknown keys and malformed lines are not settings and are left alone, as the daemon
/// leaves them.
pub fn read_values(file: &KvFile) -> NotifdValues {
    let mut values = Vec::with_capacity(NOTIFD_SETTINGS.len());
    let mut missing = Vec::new();
    let mut ignored = Vec::new();
    for setting in &NOTIFD_SETTINGS {
        let mut seen = false;
        let mut effective = None;
        for entry in file.entries().filter(|entry| entry.key == setting.key) {
            seen = true;
            match setting.parse(entry.value) {
                Some(value) => effective = Some(value),
                None => ignored.push(IgnoredValue {
                    line: entry.line,
                    key: setting.key,
                }),
            }
        }
        if !seen {
            missing.push(setting.key);
        }
        values.push((
            setting.id,
            effective.unwrap_or_else(|| setting.default_value()),
        ));
    }
    NotifdValues {
        values,
        missing,
        ignored,
    }
}

/// Why `~/.config/notifd/config` cannot back the Notifications page's staged rows. Each
/// case hides the rows (the runtime half of the page stays) and is logged at `info` by
/// the startup load (R4.2, R4.4).
#[derive(Debug)]
pub enum NotifdConfigUnavailable {
    /// notifd is not running, or runs a version older than 1.1, which has neither the
    /// config file nor `Reload()` (analysis §8.2). Carries the detected status.
    DaemonCannotReload(NotifdStatus),
    /// The file could not be read (typically: it does not exist).
    Unreadable(io::Error),
    /// The file is not valid UTF-8 (see the module docs).
    NotUtf8,
    /// A line holds a character notifd would read differently from the app — a line
    /// break other than `\n`/`\r\n`, or another control character besides tab (see the
    /// module docs). Carries the 1-based line number only (R7.3).
    UnsupportedCharacter {
        /// The 1-based line number.
        line: usize,
    },
}

impl fmt::Display for NotifdConfigUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotifdConfigUnavailable::DaemonCannotReload(status) => write!(
                f,
                "notifd is {status}; its settings file needs a running notifd 1.1 or later"
            ),
            NotifdConfigUnavailable::Unreadable(error) => {
                write!(f, "the file cannot be read: {error}")
            }
            NotifdConfigUnavailable::NotUtf8 => f.write_str("the file is not valid UTF-8"),
            NotifdConfigUnavailable::UnsupportedCharacter { line } => write!(
                f,
                "line {line} holds a control character or line break notifd would read \
                 differently"
            ),
        }
    }
}

impl std::error::Error for NotifdConfigUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NotifdConfigUnavailable::Unreadable(error) => Some(error),
            _ => None,
        }
    }
}

/// Decodes the file's bytes and checks that the app and notifd read its lines alike
/// (see the module docs), returning the text.
pub fn decode(bytes: &[u8]) -> Result<&str, NotifdConfigUnavailable> {
    let text = std::str::from_utf8(bytes).map_err(|_| NotifdConfigUnavailable::NotUtf8)?;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if content.chars().any(is_unsupported_character) {
            return Err(NotifdConfigUnavailable::UnsupportedCharacter { line: index + 1 });
        }
    }
    Ok(text)
}

/// Whether `c`, inside a line, would make notifd's reading differ from the app's: any
/// control character but tab (a lone `\r`, form feed, `\x1c`–`\x1f`, …) and the Unicode
/// line separators Python's `str.splitlines` breaks on.
fn is_unsupported_character(c: char) -> bool {
    (c.is_control() && c != '\t') || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Reads and checks the file at `path`, returning its bytes and line model.
///
/// The single read the store loader and [`NotifdConfigModel::load`] share, so the two
/// agree on which files are usable.
pub fn read_notifd_config(path: &Path) -> Result<(Vec<u8>, KvFile), NotifdConfigUnavailable> {
    let bytes = std::fs::read(path).map_err(NotifdConfigUnavailable::Unreadable)?;
    let file = KvFile::parse(decode(&bytes)?);
    Ok((bytes, file))
}

/// Why [`render_notifd_config`] could not write the staged notifd settings. Its message
/// is quoted in the Apply-failure dialog, prefixed with the file name.
#[derive(Clone, Debug, PartialEq)]
pub enum NotifdRenderError {
    /// The file changed into one the app refuses to read since it was loaded (the
    /// conflict check normally catches this first).
    Unreadable(String),
    /// A staged value has the wrong kind for its key — refused on stage, guarded here.
    ValueKind {
        /// The key.
        key: &'static str,
        /// The staged value.
        value: Value,
    },
    /// A staged value is outside the range notifd accepts, so notifd would ignore it —
    /// refused on stage (R8.3), guarded here so it can never be written.
    Rejected {
        /// The key.
        key: &'static str,
        /// The value, formatted.
        value: String,
    },
    /// The line model refused the edit (see [`KvEditError`]).
    Edit(KvEditError),
}

impl fmt::Display for NotifdRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotifdRenderError::Unreadable(reason) => write!(f, "{reason}"),
            NotifdRenderError::ValueKind { key, value } => {
                write!(f, "a {} value cannot be written to `{key}`", value.kind())
            }
            NotifdRenderError::Rejected { key, value } => write!(
                f,
                "`{key}={value}` is outside the range notifd accepts, so notifd would ignore it"
            ),
            NotifdRenderError::Edit(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NotifdRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NotifdRenderError::Edit(error) => Some(error),
            _ => None,
        }
    }
}

impl From<KvEditError> for NotifdRenderError {
    fn from(error: KvEditError) -> Self {
        NotifdRenderError::Edit(error)
    }
}

/// Applies the store's dirty notifd `edits` to the file's current `bytes`, returning the
/// complete new bytes and the keys it changed (task 11.15; R5.3).
///
/// Each value is rendered in its canonical spelling after the daemon's rule is checked
/// once more ([`NotifdSetting::render`]), then written into the value span of the key's
/// **last** assignment — the one in effect for the daemon — or, for a key the file does
/// not assign, appended as a bare `key=value` line at the end. Comments, blank lines,
/// other keys and every untouched byte stay identical, and no inline comment is ever
/// produced. A setting that notifd's file does not back is ignored.
pub fn render_notifd_config(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<RenderedEdit, NotifdRenderError> {
    let text = decode(bytes).map_err(|error| NotifdRenderError::Unreadable(error.to_string()))?;
    let mut file = KvFile::parse(text);
    let mut changed_keys = Vec::new();
    for (id, value) in edits {
        let Some(setting) = NotifdSetting::for_id(*id) else {
            continue;
        };
        let rendered = setting.render(value)?;
        match file.set_value(setting.key, Occurrence::Last, &rendered) {
            Ok(()) => {}
            Err(KvEditError::UnknownKey(_)) => file.append_entry(setting.key, &rendered)?,
            Err(error) => return Err(error.into()),
        }
        changed_keys.push(setting.key.to_string());
    }
    Ok(RenderedEdit {
        contents: file.emit().into_bytes(),
        changed_keys,
    })
}

/// The Notifications page's Apply-time write glue for `~/.config/notifd/config`
/// (task 11.15).
///
/// Built by the startup load only when the staged rows may be shown — notifd ≥ 1.1 runs
/// and the file is readable ([`Self::offer`]) — and held by the window. Like the Input
/// and Power & Idle helpers it is **not** a staging model: the seven settings are staged
/// in the shared store, which also baselines the file for the conflict check (R5.6).
/// This only turns the store's dirty Notifications settings into the one write, which the
/// pipeline follows with notifd's `Reload()`.
pub struct NotifdConfigModel {
    /// The write target: the file's live XDG path (R8.5) plus how the shared glue labels
    /// and logs it.
    config: StoreBackedFile,
}

impl NotifdConfigModel {
    /// Builds the model when the staged rows may be shown: `status` must be a running
    /// notifd of version 1.1 or later, and the file at `path` must be readable and
    /// understood ([`read_notifd_config`]). Otherwise returns why not, for the caller to
    /// log at `info` while the page keeps only its runtime rows (R4.2).
    pub fn offer(
        status: NotifdStatus,
        path: PathBuf,
    ) -> Result<NotifdConfigModel, NotifdConfigUnavailable> {
        if !status.supports_state_and_reload() {
            return Err(NotifdConfigUnavailable::DaemonCannotReload(status));
        }
        read_notifd_config(&path)?;
        Ok(NotifdConfigModel {
            config: write_target(path),
        })
    }

    /// The file's live path.
    pub fn path(&self) -> &Path {
        &self.config.path
    }

    /// The Notifications page's contribution to an Apply: the store's dirty
    /// Notifications settings rendered into one surgical write of the file (see
    /// [`StoreBackedFile::render_write`] for the three possible answers). The write's
    /// [`BackingFile::NotifdConfig`] is what makes the pipeline call `Reload()`.
    pub fn apply_contribution(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, StoreWriteError<NotifdRenderError>> {
        self.config.render_write(dirty, render_notifd_config)
    }
}

/// How the shared store-backed write glue addresses, labels and logs notifd's file.
fn write_target(path: PathBuf) -> StoreBackedFile {
    StoreBackedFile {
        path,
        name: "notifd/config",
        backing: BackingFile::NotifdConfig,
        // Rendered from the store's dirty Notifications settings, which the plan carries
        // as its validations.
        validation: WriteValidation::InPlan,
        read_failure_log: "could not read notifd/config to render the Notifications edits; \
                           aborting the apply (R8.3)",
        render_failure_log: "failed to render a notifd/config edit; aborting the apply (R8.3)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::notifd::NotifdVersion;
    use crate::testing::differing_lines;

    /// The shipped `config/notifd/config` from dotfiles commit `8a5b20b`, byte for byte —
    /// the same file as the test fixture tree's copy.
    const SHIPPED: &str = include_str!("../../tests/fixtures/dotfiles/config/notifd/config");

    /// The value read for `id`.
    fn value_of(values: &NotifdValues, id: SettingId) -> Option<&Value> {
        values
            .values
            .iter()
            .find(|(setting, _)| *setting == id)
            .map(|(_, value)| value)
    }

    /// Renders `edits` into `input` and returns the text.
    fn render(input: &str, edits: &[(SettingId, Value)]) -> String {
        let edit = render_notifd_config(input.as_bytes(), edits).expect("the edits render");
        String::from_utf8(edit.contents).expect("UTF-8")
    }

    fn running(version: &str) -> NotifdStatus {
        NotifdStatus::Running {
            version: NotifdVersion::parse(version),
        }
    }

    #[test]
    fn the_shipped_file_round_trips() {
        let file = KvFile::parse(SHIPPED);
        assert_eq!(file.emit(), SHIPPED, "parse → emit must be byte-identical");
    }

    #[test]
    fn the_shipped_file_reads_its_seven_defaults() {
        let file = KvFile::parse(SHIPPED);
        let read = read_values(&file);
        assert!(read.missing.is_empty(), "{:?}", read.missing);
        assert!(read.ignored.is_empty(), "{:?}", read.ignored);
        // The shipped file lists every key at its default.
        let defaults: Vec<(SettingId, Value)> = NOTIFD_SETTINGS
            .iter()
            .map(|setting| (setting.id, setting.default_value()))
            .collect();
        assert_eq!(read.values, defaults);
    }

    #[test]
    fn the_table_matches_the_daemons_ranges_and_defaults() {
        // notifd's Config.DEFAULTS / MIN / MAX at dotfiles commit 8a5b20b.
        let expected = [
            ("timeout_low_ms", 0, NOTIFD_INT_MAX, 5000),
            ("timeout_normal_ms", 0, NOTIFD_INT_MAX, 10_000),
            ("timeout_critical_ms", 0, NOTIFD_INT_MAX, 0),
            ("history_max", 1, NOTIFD_INT_MAX, 100),
            ("critical_per_app", 1, 5, 3),
            ("hold_min_ms", 0, NOTIFD_INT_MAX, 1500),
        ];
        for (key, min, max, default) in expected {
            let setting = NOTIFD_SETTINGS
                .iter()
                .find(|setting| setting.key == key)
                .expect("the key is in the table");
            assert_eq!(
                setting.rule,
                NotifdRule::Integer { min, max, default },
                "{key}"
            );
        }
        assert_eq!(NOTIFD_INT_MAX, (1_i64 << 31) - 1);
        assert_eq!(
            NotifdSetting::for_id(SettingId::NotifdCopyCodes).map(|setting| setting.rule),
            Some(NotifdRule::Bool { default: true })
        );
        assert_eq!(NotifdSetting::for_id(SettingId::DimTimeout), None);
    }

    #[test]
    fn integers_follow_the_daemons_ascii_decimal_rule() {
        assert_eq!(parse_notifd_integer("0", 0, 10), Some(0));
        assert_eq!(parse_notifd_integer("007", 0, 10), Some(7));
        assert_eq!(
            parse_notifd_integer("2147483647", 0, NOTIFD_INT_MAX),
            Some(NOTIFD_INT_MAX)
        );
        for rejected in [
            "",
            "-1",
            "+1",
            "1.5",
            "1 000",
            "0x10",
            "٣",
            "5000 # five seconds",
            "2147483648",
            "99999999999999999999999",
        ] {
            assert_eq!(
                parse_notifd_integer(rejected, 0, NOTIFD_INT_MAX),
                None,
                "{rejected:?}"
            );
        }
        assert_eq!(parse_notifd_integer("6", 1, 5), None);
        assert_eq!(parse_notifd_integer("0", 1, 5), None);
    }

    #[test]
    fn booleans_follow_the_daemons_rule() {
        for (text, expected) in [
            ("true", Some(true)),
            ("TRUE", Some(true)),
            ("True", Some(true)),
            ("1", Some(true)),
            ("false", Some(false)),
            ("FaLsE", Some(false)),
            ("0", Some(false)),
            ("yes", None),
            ("on", None),
            ("", None),
            ("01", None),
        ] {
            assert_eq!(parse_notifd_bool(text), expected, "{text:?}");
        }
    }

    #[test]
    fn the_last_valid_assignment_wins() {
        // The daemon applies lines in order: a valid line overwrites, an invalid one
        // changes nothing.
        let file = KvFile::parse(
            "history_max=10\nhistory_max=20\nhistory_max=0\ncritical_per_app=9\n\
             copy_codes=0\ncopy_codes=maybe\n",
        );
        let read = read_values(&file);
        assert_eq!(
            value_of(&read, SettingId::NotifdHistoryMax),
            Some(&Value::Integer(20))
        );
        // Only invalid assignments: the default.
        assert_eq!(
            value_of(&read, SettingId::NotifdCriticalPerApp),
            Some(&Value::Integer(3))
        );
        assert_eq!(
            value_of(&read, SettingId::NotifdCopyCodes),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            read.ignored,
            vec![
                IgnoredValue {
                    line: 3,
                    key: "history_max"
                },
                IgnoredValue {
                    line: 4,
                    key: "critical_per_app"
                },
                IgnoredValue {
                    line: 6,
                    key: "copy_codes"
                },
            ]
        );
        assert_eq!(
            read.missing,
            vec![
                "timeout_low_ms",
                "timeout_normal_ms",
                "timeout_critical_ms",
                "hold_min_ms"
            ]
        );
    }

    #[test]
    fn spacing_around_the_equals_sign_is_read() {
        let input = "timeout_low_ms = 5000\n  hold_min_ms=\t1500  \n";
        let read = read_values(&KvFile::parse(input));
        assert_eq!(
            value_of(&read, SettingId::NotifdTimeoutLow),
            Some(&Value::Integer(5000))
        );
        assert_eq!(
            value_of(&read, SettingId::NotifdHoldMin),
            Some(&Value::Integer(1500))
        );
    }

    #[test]
    fn spacing_around_the_equals_sign_survives_a_write() {
        let input = "timeout_low_ms = 5000\n  hold_min_ms=\t1500  \n";
        assert_eq!(
            render(
                input,
                &[
                    (SettingId::NotifdTimeoutLow, Value::Integer(2500)),
                    (SettingId::NotifdHoldMin, Value::Integer(800)),
                ]
            ),
            "timeout_low_ms = 2500\n  hold_min_ms=\t800  \n"
        );
    }

    #[test]
    fn a_timeout_change_rewrites_exactly_one_value_span() {
        // Measured against the shipped file.
        let edited = render(
            SHIPPED,
            &[(SettingId::NotifdTimeoutNormal, Value::Integer(7500))],
        );
        let target = SHIPPED
            .lines()
            .position(|line| line == "timeout_normal_ms=10000")
            .expect("the shipped file has the key");
        assert_eq!(differing_lines(SHIPPED, &edited), vec![target]);
        assert_eq!(edited.lines().nth(target), Some("timeout_normal_ms=7500"));
    }

    #[test]
    fn a_boolean_replaces_a_numeric_spelling_with_a_word() {
        // Written as `true`/`false`, even where the file spelled it `1`/`0`.
        assert_eq!(
            render(
                "copy_codes=1\n",
                &[(SettingId::NotifdCopyCodes, Value::Bool(false))]
            ),
            "copy_codes=false\n"
        );
    }

    #[test]
    fn a_duplicated_key_is_written_at_its_last_assignment() {
        assert_eq!(
            render(
                "history_max=10\n# later override\nhistory_max = 20\n",
                &[(SettingId::NotifdHistoryMax, Value::Integer(50))]
            ),
            "history_max=10\n# later override\nhistory_max = 50\n"
        );
    }

    #[test]
    fn a_key_missing_from_a_trimmed_file_is_appended_at_the_end() {
        let trimmed = "# trimmed by hand\ntimeout_low_ms=5000";
        let read = read_values(&KvFile::parse(trimmed));
        assert_eq!(
            value_of(&read, SettingId::NotifdCriticalPerApp),
            Some(&Value::Integer(3)),
            "a missing key is seeded with the daemon default"
        );
        assert_eq!(
            render(
                trimmed,
                &[(SettingId::NotifdCriticalPerApp, Value::Integer(2))]
            ),
            "# trimmed by hand\ntimeout_low_ms=5000\ncritical_per_app=2\n"
        );
    }

    #[test]
    fn a_commented_out_key_is_not_edited_but_appended() {
        assert_eq!(
            render(
                "# hold_min_ms=1500\n",
                &[(SettingId::NotifdHoldMin, Value::Integer(900))]
            ),
            "# hold_min_ms=1500\nhold_min_ms=900\n"
        );
    }

    #[test]
    fn the_writer_refuses_a_value_the_daemon_would_ignore() {
        for (id, value, key) in [
            (
                SettingId::NotifdCriticalPerApp,
                Value::Integer(6),
                "critical_per_app",
            ),
            (
                SettingId::NotifdHistoryMax,
                Value::Integer(0),
                "history_max",
            ),
            (
                SettingId::NotifdTimeoutLow,
                Value::Integer(-1),
                "timeout_low_ms",
            ),
            (
                SettingId::NotifdHoldMin,
                Value::Integer(NOTIFD_INT_MAX + 1),
                "hold_min_ms",
            ),
        ] {
            assert!(
                matches!(
                    render_notifd_config(SHIPPED.as_bytes(), &[(id, value)]),
                    Err(NotifdRenderError::Rejected { key: rejected, .. }) if rejected == key
                ),
                "{key}"
            );
        }
        assert!(matches!(
            render_notifd_config(
                SHIPPED.as_bytes(),
                &[(SettingId::NotifdCopyCodes, Value::Integer(1))]
            ),
            Err(NotifdRenderError::ValueKind {
                key: "copy_codes",
                ..
            })
        ));
    }

    #[test]
    fn stage_time_validation_names_the_key_and_the_range() {
        let error = SettingId::NotifdCriticalPerApp
            .validate(&Value::Integer(6))
            .expect_err("6 is above the slot count");
        assert!(matches!(
            error,
            ValidationError::NotifdOutOfRange {
                key: "critical_per_app",
                value: 6,
                min: 1,
                max: 5
            }
        ));
        assert_eq!(
            error.to_string(),
            "critical_per_app=6 would be ignored by notifd, which then uses its default: \
             it must be a whole number from 1 to 5"
        );
        assert!(
            SettingId::NotifdHistoryMax
                .validate(&Value::Integer(0))
                .is_err()
        );
        assert!(
            SettingId::NotifdTimeoutCritical
                .validate(&Value::Integer(0))
                .is_ok(),
            "0 keeps a popup until it is dismissed"
        );
        assert!(
            SettingId::NotifdTimeoutLow
                .validate(&Value::Integer(NOTIFD_INT_MAX))
                .is_ok()
        );
        assert!(
            SettingId::NotifdTimeoutLow
                .validate(&Value::Integer(NOTIFD_INT_MAX + 1))
                .is_err()
        );
        assert!(
            SettingId::NotifdCopyCodes
                .validate(&Value::Bool(false))
                .is_ok()
        );
    }

    #[test]
    fn a_file_notifd_would_split_differently_is_refused() {
        assert!(decode(SHIPPED.as_bytes()).is_ok());
        assert!(decode(b"a=1\r\nb=2\r\n").is_ok(), "CRLF is fine");
        assert!(decode(b"a=1\tx\n").is_ok(), "a tab is whitespace to both");
        for (bytes, line) in [
            (&b"a=1\nb=2\rc=3\n"[..], 2),
            (b"a=1\x0cb=2\n", 1),
            (b"\n\na=1\x1f\n", 3),
            ("a=1\u{2028}b=2\n".as_bytes(), 1),
        ] {
            assert!(
                matches!(
                    decode(bytes),
                    Err(NotifdConfigUnavailable::UnsupportedCharacter { line: found }) if found == line
                ),
                "{bytes:?}"
            );
        }
        assert!(matches!(
            decode(b"a=\xff\n"),
            Err(NotifdConfigUnavailable::NotUtf8)
        ));
    }

    #[test]
    fn the_model_needs_a_reloadable_daemon_and_a_readable_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        fs::write(&path, SHIPPED).expect("write the file");

        assert!(NotifdConfigModel::offer(running("1.1"), path.clone()).is_ok());
        assert!(NotifdConfigModel::offer(running("1.2"), path.clone()).is_ok());
        for status in [
            running("1.0"),
            NotifdStatus::Running { version: None },
            NotifdStatus::Installed,
            NotifdStatus::Absent,
        ] {
            assert!(
                matches!(
                    NotifdConfigModel::offer(status, path.clone()),
                    Err(NotifdConfigUnavailable::DaemonCannotReload(found)) if found == status
                ),
                "{status}"
            );
        }
        assert!(matches!(
            NotifdConfigModel::offer(running("1.1"), dir.path().join("missing")),
            Err(NotifdConfigUnavailable::Unreadable(_))
        ));
    }

    #[test]
    fn a_clean_notifd_config_contributes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        fs::write(&path, SHIPPED).expect("write the file");
        let model = NotifdConfigModel::offer(running("1.1"), path).expect("offered");

        assert!(
            model
                .apply_contribution(&[])
                .expect("nothing to render")
                .is_none()
        );
    }

    #[test]
    fn a_dirty_notifd_config_contributes_one_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        fs::write(&path, SHIPPED).expect("write the file");
        let model = NotifdConfigModel::offer(running("1.1"), path.clone()).expect("offered");
        assert_eq!(model.path(), path);

        let write = model
            .apply_contribution(&[(SettingId::NotifdTimeoutLow, Value::Integer(3000))])
            .expect("renders")
            .expect("a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::NotifdConfig);
        assert_eq!(write.changed_keys, vec!["timeout_low_ms".to_string()]);
        assert_eq!(
            String::from_utf8(write.contents).expect("UTF-8"),
            SHIPPED.replace("timeout_low_ms=5000", "timeout_low_ms=3000")
        );
    }
}
