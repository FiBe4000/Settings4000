//! The `--grace N` token inside hypridle's `lock_cmd` value: reading it and editing it
//! in place (task 11.13; architecture §3, §6; analysis §7, §8; R8.3, R6.2).
//!
//! # Why a token editor
//!
//! hyprlock ≥ 0.9 dropped its `general:grace` config option: the grace period (how
//! many seconds after locking any input unlocks again without a password) is taken
//! only from its command line, as `--grace N`. The dotfiles therefore keep it inside
//! the command hypridle runs to lock the session, `hypridle.conf`'s
//! `lock_cmd = pidof hyprlock || hyprlock --grace 10`. The Power & Idle page exposes
//! that number as its own control, so the app has to edit one token of a shell
//! command it otherwise treats as free text. Architecture §3 puts the boundary here:
//! the hyprlang parser returns the `lock_cmd` value string (and later writes the new
//! string back into the same value span, keeping the line's trailing `#` comment), and
//! token-level edits inside that string are done by this module. Everything outside
//! the grace token — the `pidof hyprlock ||` guard, other flags, the exact spacing —
//! stays byte-identical.
//!
//! It is pure string logic with no GTK and no side effects, so it lives in `core/` and
//! is unit-tested headlessly (R6.2).
//!
//! # Tokenisation (the contract both directions share)
//!
//! The value is split into **words**: maximal runs of non-whitespace characters,
//! separated by spaces or tabs. No shell quoting is interpreted — hypridle hands the
//! string to `sh -c`, but a grace period never needs quoting, so anything quoted is
//! simply an opaque word.
//!
//! - A **command starts** at the first word and at every word that follows one of the
//!   shell control operators `||`, `&&`, `|`, `&` or `;` written as a word of its own
//!   (surrounded by whitespace, as the dotfiles write them).
//! - The **hyprlock invocation** is a word that starts a command and is exactly
//!   `hyprlock` or a path ending in `/hyprlock`. So in `pidof hyprlock || hyprlock`
//!   the first `hyprlock` is an argument of `pidof`, not the invocation; the second
//!   is. A wrapped launch (`exec hyprlock`, `uwsm app -- hyprlock`, `(hyprlock)`) or
//!   an operator glued to a word (`a||hyprlock`) is not recognised — the command then
//!   reads as "does not run hyprlock" and the grace control is hidden, which is the
//!   safe outcome: the free-text lock-command entry can still edit it.
//! - hyprlock's **arguments** are the words after the invocation up to the next
//!   operator word or the end of the value.
//! - A **grace flag** is an argument word spelled `--grace` or `-g` (the documented
//!   long and short names) — or `--g` / `-grace`, which hyprlock's argument parser
//!   (hyprutils' `CArgumentParser`) also accepts because it matches either name after
//!   either prefix. The **next word** is the number of seconds; it must be ASCII
//!   digits only and fit a `u32`.
//!
//! # What is refused, and why
//!
//! A command the app could only edit by guessing is reported as [`GraceError::Unsupported`]
//! (and the control is hidden) rather than edited:
//!
//! - more than one hyprlock invocation — it would be ambiguous which one locks;
//! - more than one grace flag — hyprlock takes the *last* one, so editing a single
//!   copy might not change the effective value and removing it might not remove it;
//! - a grace flag with no number after it, or a non-numeric one (`--grace ten`,
//!   `--grace 10)`): hyprlock itself would refuse to start;
//! - the `--grace=N` / `-g=N` form: hyprlock rejects it as an invalid argument, so the
//!   app does not pretend to understand it (and must not insert a second flag next to
//!   it).
//!
//! # Absent versus zero
//!
//! A hyprlock invocation without a grace flag behaves exactly like `--grace 0` (an
//! immediate lock; hyprlock's own default), so [`grace_of`] reports both as `0` for
//! display. [`parse_grace`] keeps them apart (`Ok(None)` versus `Ok(Some(0))`) so
//! [`set_grace`] can leave an existing `--grace 0` alone and never adds a token just
//! to say "zero".

use std::fmt;

/// The shell control operators that end one command and start the next, when written
/// as a word of their own (see the module docs on tokenisation).
const COMMAND_SEPARATORS: &[&str] = &["||", "&&", "|", "&", ";"];

/// The program name whose invocation carries the grace flag.
const HYPRLOCK: &str = "hyprlock";

/// Every spelling hyprlock accepts for its grace option: the documented `--grace` and
/// `-g`, plus `--g` and `-grace`, which hyprutils' argument parser also matches because
/// it compares the stripped word against both the long and the short name.
const GRACE_FLAGS: &[&str] = &["--grace", "-g", "--g", "-grace"];

/// The flag [`set_grace`] writes when it inserts a grace period: the spelling the
/// dotfiles and hyprlock's own help text use.
const INSERTED_FLAG: &str = "--grace";

/// Why a `lock_cmd` value offers no grace period the app can read or edit.
///
/// Both cases hide the Power & Idle page's grace control (task 11.13) and make any
/// attempt to stage a grace period fail validation (R8.3), so the command is never
/// rewritten by guesswork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraceError {
    /// No word of the command is a hyprlock invocation (see the module docs for what
    /// counts as one), so there is nothing to attach a grace period to.
    NoHyprlock,
    /// hyprlock is invoked, but in a shape the editor deliberately does not touch.
    Unsupported(UnsupportedReason),
}

/// The specific shape that made a hyprlock command [`GraceError::Unsupported`]; see the
/// module docs for the reasoning behind each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// hyprlock is invoked more than once, so which invocation to edit is ambiguous.
    SeveralInvocations,
    /// The invocation carries more than one grace flag; hyprlock uses the last.
    SeveralGraceFlags,
    /// A grace flag is the last argument, with no number after it.
    MissingSeconds,
    /// The word after the grace flag is not a whole number of seconds that fits a
    /// `u32`. Carries that word.
    InvalidSeconds(String),
    /// The grace period is written as `--grace=N` (or another `=` spelling), which
    /// hyprlock rejects. Carries that word.
    EqualsForm(String),
}

impl fmt::Display for GraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraceError::NoHyprlock => {
                write!(f, "the lock command does not start hyprlock")
            }
            GraceError::Unsupported(reason) => match reason {
                UnsupportedReason::SeveralInvocations => {
                    write!(f, "the lock command starts hyprlock more than once")
                }
                UnsupportedReason::SeveralGraceFlags => write!(
                    f,
                    "the lock command passes hyprlock more than one grace period"
                ),
                UnsupportedReason::MissingSeconds => write!(
                    f,
                    "the lock command's grace flag has no number of seconds after it"
                ),
                UnsupportedReason::InvalidSeconds(word) => write!(
                    f,
                    "the lock command's grace period `{word}` is not a whole number of seconds"
                ),
                UnsupportedReason::EqualsForm(word) => write!(
                    f,
                    "the lock command writes the grace period as `{word}`, which hyprlock \
                     does not accept (it expects `--grace N`)"
                ),
            },
        }
    }
}

impl std::error::Error for GraceError {}

/// One whitespace-separated word of the value, with its byte span.
struct Word<'a> {
    /// The word's text.
    text: &'a str,
    /// Byte offset of the word's first character.
    start: usize,
    /// Byte offset just past the word's last character.
    end: usize,
}

/// Where the grace period sits in a value, as found by [`locate`].
struct Located {
    /// Byte offset just past the hyprlock invocation word — where a new grace flag is
    /// inserted.
    invocation_end: usize,
    /// The existing grace token, if the invocation has one.
    grace: Option<GraceSpan>,
}

/// The byte layout of an existing `<flag> <seconds>` pair.
struct GraceSpan {
    /// The parsed number of seconds.
    seconds: u32,
    /// Byte offset just past the word *before* the flag. Removing
    /// `removal_start..value_end` drops the flag, the number and the whitespace that
    /// separated them from the preceding word, leaving the rest byte-identical.
    removal_start: usize,
    /// Byte offset of the number's first digit.
    value_start: usize,
    /// Byte offset just past the number's last digit.
    value_end: usize,
}

/// Splits `value` into words with their byte spans (see the module docs).
fn words(value: &str) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, character) in value.char_indices() {
        let is_space = character == ' ' || character == '\t';
        match (start, is_space) {
            (None, false) => start = Some(index),
            (Some(word_start), true) => {
                words.push(Word {
                    text: &value[word_start..index],
                    start: word_start,
                    end: index,
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(word_start) = start {
        words.push(Word {
            text: &value[word_start..],
            start: word_start,
            end: value.len(),
        });
    }
    words
}

/// Whether `word` names the hyprlock binary: exactly `hyprlock`, or a path to it.
fn is_hyprlock(word: &str) -> bool {
    word == HYPRLOCK
        || word
            .strip_suffix(HYPRLOCK)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

/// Whether `word` is a shell control operator that starts a new command.
fn is_separator(word: &str) -> bool {
    COMMAND_SEPARATORS.contains(&word)
}

/// Whether `word` is a grace flag written with `=` (`--grace=10`), which hyprlock
/// rejects.
fn is_equals_form(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(flag, _)| GRACE_FLAGS.contains(&flag))
}

/// Finds the single hyprlock invocation in `value` and its grace token, applying every
/// rule in the module docs.
fn locate(value: &str) -> Result<Located, GraceError> {
    let words = words(value);

    // The invocation: the one command-starting word that names hyprlock.
    let mut invocation = None;
    let mut command_start = true;
    for (index, word) in words.iter().enumerate() {
        if command_start && is_hyprlock(word.text) {
            if invocation.is_some() {
                return Err(GraceError::Unsupported(
                    UnsupportedReason::SeveralInvocations,
                ));
            }
            invocation = Some(index);
        }
        command_start = is_separator(word.text);
    }
    let invocation = invocation.ok_or(GraceError::NoHyprlock)?;

    // Its arguments, up to the next operator, scanned for a grace flag.
    let mut grace = None;
    let arguments_end = words[invocation + 1..]
        .iter()
        .position(|word| is_separator(word.text))
        .map_or(words.len(), |offset| invocation + 1 + offset);
    let mut index = invocation + 1;
    while index < arguments_end {
        let word = &words[index];
        if is_equals_form(word.text) {
            return Err(GraceError::Unsupported(UnsupportedReason::EqualsForm(
                word.text.to_string(),
            )));
        }
        if GRACE_FLAGS.contains(&word.text) {
            if grace.is_some() {
                return Err(GraceError::Unsupported(
                    UnsupportedReason::SeveralGraceFlags,
                ));
            }
            let Some(number) = words[index + 1..arguments_end].first() else {
                return Err(GraceError::Unsupported(UnsupportedReason::MissingSeconds));
            };
            let seconds = parse_seconds(number.text).ok_or_else(|| {
                GraceError::Unsupported(UnsupportedReason::InvalidSeconds(number.text.to_string()))
            })?;
            grace = Some(GraceSpan {
                seconds,
                // The flag always has a preceding word (at least the invocation).
                removal_start: words[index - 1].end,
                value_start: number.start,
                value_end: number.end,
            });
            // Skip the number so it is not inspected as a flag itself.
            index += 2;
        } else {
            index += 1;
        }
    }

    Ok(Located {
        invocation_end: words[invocation].end,
        grace,
    })
}

/// Parses a grace number: ASCII digits only (no sign, no spaces), within `u32`.
fn parse_seconds(word: &str) -> Option<u32> {
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
}

/// Reads the grace period of a `lock_cmd` value, keeping "no grace flag" (`Ok(None)`)
/// apart from an explicit `--grace 0` (`Ok(Some(0))`).
///
/// An `Err` means the command offers no editable grace period: it does not start
/// hyprlock, or it does so in a shape listed under "What is refused" in the module
/// docs.
pub fn parse_grace(lock_cmd: &str) -> Result<Option<u32>, GraceError> {
    locate(lock_cmd).map(|located| located.grace.map(|grace| grace.seconds))
}

/// The grace period a `lock_cmd` value gives hyprlock, in seconds, as the Power & Idle
/// page shows it — or `None` when the command offers no editable grace period (the
/// control is then hidden).
///
/// A hyprlock invocation without a grace flag reads as `0`, because that is what
/// hyprlock does without one (lock immediately). Use [`parse_grace`] where the
/// difference between "absent" and "explicitly 0" matters.
pub fn grace_of(lock_cmd: &str) -> Option<u32> {
    parse_grace(lock_cmd).ok().map(Option::unwrap_or_default)
}

/// Returns `lock_cmd` with hyprlock's grace period set to `seconds`, changing only the
/// grace token (architecture §3).
///
/// - With a grace flag present, its number is replaced in place (the flag keeps its
///   spelling, e.g. `-g`); setting `0` removes the flag, its number and the whitespace
///   before the flag instead, since no flag means the same as `--grace 0`.
/// - Without one, ` --grace N` is inserted immediately after the hyprlock invocation
///   word, before any other hyprlock argument; setting `0` changes nothing.
/// - Setting the value the command already has returns it unchanged — including a
///   differently written equal number (`--grace 010` stays as it is for 10), so a
///   no-op edit can never produce a byte diff.
///
/// Every byte outside that token — the `pidof hyprlock ||` guard, other flags, the
/// spacing — is kept. The result is still free of newlines and `#` if the input was,
/// so it passes the same lock-command validation (R8.3). Fails with the
/// [`GraceError`] of [`parse_grace`] when the command offers no editable grace period.
pub fn set_grace(lock_cmd: &str, seconds: u32) -> Result<String, GraceError> {
    let located = locate(lock_cmd)?;
    let edited = match located.grace {
        Some(grace) if grace.seconds == seconds => lock_cmd.to_string(),
        Some(grace) if seconds == 0 => format!(
            "{}{}",
            &lock_cmd[..grace.removal_start],
            &lock_cmd[grace.value_end..]
        ),
        Some(grace) => format!(
            "{}{seconds}{}",
            &lock_cmd[..grace.value_start],
            &lock_cmd[grace.value_end..]
        ),
        None if seconds == 0 => lock_cmd.to_string(),
        None => format!(
            "{} {INSERTED_FLAG} {seconds}{}",
            &lock_cmd[..located.invocation_end],
            &lock_cmd[located.invocation_end..]
        ),
    };
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real dotfiles' `lock_cmd` value (fixture `hypridle.conf`, dotfiles commit
    /// `8a5b20b`).
    const REAL: &str = "pidof hyprlock || hyprlock --grace 10";

    #[test]
    fn the_real_command_reads_as_ten_seconds() {
        // `pidof hyprlock` is an argument of pidof, not the invocation: only the
        // command after `||` counts, and it carries `--grace 10`.
        assert_eq!(parse_grace(REAL), Ok(Some(10)));
        assert_eq!(grace_of(REAL), Some(10));
    }

    #[test]
    fn a_missing_grace_flag_is_distinct_from_an_explicit_zero() {
        // Both read as a grace of 0.
        assert_eq!(parse_grace("pidof hyprlock || hyprlock"), Ok(None));
        assert_eq!(grace_of("pidof hyprlock || hyprlock"), Some(0));
        assert_eq!(parse_grace("hyprlock --grace 0"), Ok(Some(0)));
        assert_eq!(grace_of("hyprlock --grace 0"), Some(0));
    }

    #[test]
    fn changing_the_number_rewrites_only_the_digits() {
        assert_eq!(
            set_grace(REAL, 30).as_deref(),
            Ok("pidof hyprlock || hyprlock --grace 30")
        );
        // Other flags and unusual spacing on either side survive byte for byte.
        assert_eq!(
            set_grace("hyprlock  --no-fade-in\t--grace   5  -q", 120).as_deref(),
            Ok("hyprlock  --no-fade-in\t--grace   120  -q")
        );
    }

    #[test]
    fn an_absent_flag_is_inserted_right_after_the_hyprlock_word() {
        assert_eq!(
            set_grace("pidof hyprlock || hyprlock", 10).as_deref(),
            Ok("pidof hyprlock || hyprlock --grace 10")
        );
        // Before hyprlock's other arguments, and before a following command.
        assert_eq!(
            set_grace("hyprlock -c ~/lock.conf && notify-send locked", 5).as_deref(),
            Ok("hyprlock --grace 5 -c ~/lock.conf && notify-send locked")
        );
    }

    #[test]
    fn a_zero_grace_removes_the_flag_with_its_leading_space() {
        assert_eq!(
            set_grace(REAL, 0).as_deref(),
            Ok("pidof hyprlock || hyprlock")
        );
        // A flag in the middle: the arguments after it keep their own spacing.
        assert_eq!(
            set_grace("hyprlock --grace 10 --no-fade-in", 0).as_deref(),
            Ok("hyprlock --no-fade-in")
        );
        // The short spelling is removed the same way.
        assert_eq!(
            set_grace("hyprlock -g 3 ; true", 0).as_deref(),
            Ok("hyprlock ; true")
        );
    }

    #[test]
    fn setting_the_current_value_changes_nothing() {
        assert_eq!(set_grace(REAL, 10).as_deref(), Ok(REAL));
        // A zero on an invocation without a flag adds nothing...
        assert_eq!(set_grace("hyprlock", 0).as_deref(), Ok("hyprlock"));
        // ...and an explicit `--grace 0` is left as written.
        assert_eq!(
            set_grace("hyprlock --grace 0", 0).as_deref(),
            Ok("hyprlock --grace 0")
        );
        // An equal number written differently is not normalised.
        assert_eq!(
            set_grace("hyprlock --grace 010", 10).as_deref(),
            Ok("hyprlock --grace 010")
        );
    }

    #[test]
    fn hyprlock_as_the_first_word_is_the_invocation() {
        assert_eq!(parse_grace("hyprlock"), Ok(None));
        assert_eq!(parse_grace("hyprlock --grace 7"), Ok(Some(7)));
        assert_eq!(
            set_grace("hyprlock", 7).as_deref(),
            Ok("hyprlock --grace 7")
        );
        // A path to the binary is an invocation too.
        assert_eq!(parse_grace("/usr/bin/hyprlock -g 4"), Ok(Some(4)));
    }

    #[test]
    fn a_command_without_a_hyprlock_invocation_has_no_grace() {
        for command in [
            "swaylock -f",
            "",
            // Only an argument of pidof, never started.
            "pidof hyprlock",
            // Wrapped or glued launches are deliberately not recognised (module docs).
            "exec hyprlock --grace 5",
            "uwsm app -- hyprlock",
            "pidof hyprlock||hyprlock",
            // A name that merely ends in "hyprlock" is a different program.
            "myhyprlock --grace 5",
        ] {
            assert_eq!(
                parse_grace(command),
                Err(GraceError::NoHyprlock),
                "{command:?}"
            );
            assert_eq!(grace_of(command), None, "{command:?}");
            assert_eq!(
                set_grace(command, 10),
                Err(GraceError::NoHyprlock),
                "{command:?} must not be rewritten"
            );
        }
    }

    #[test]
    fn several_invocations_are_refused() {
        let command = "hyprlock --grace 5 || hyprlock";
        let expected = Err(GraceError::Unsupported(
            UnsupportedReason::SeveralInvocations,
        ));
        assert_eq!(parse_grace(command), expected);
        assert_eq!(set_grace(command, 10).map(|_| ()), expected.map(|_| ()));
        // ...whereas a second `hyprlock` word that is only an argument is fine.
        assert_eq!(
            parse_grace("pidof hyprlock || hyprlock --grace 10"),
            Ok(Some(10))
        );
    }

    #[test]
    fn a_grace_flag_without_a_number_is_refused() {
        assert_eq!(
            parse_grace("hyprlock --grace"),
            Err(GraceError::Unsupported(UnsupportedReason::MissingSeconds))
        );
        // An operator ends hyprlock's arguments, so it is not the number either.
        assert_eq!(
            parse_grace("hyprlock --grace && true"),
            Err(GraceError::Unsupported(UnsupportedReason::MissingSeconds))
        );
        assert_eq!(set_grace("hyprlock --grace", 5).ok(), None);
    }

    #[test]
    fn a_non_numeric_grace_is_refused() {
        for (command, word) in [
            ("hyprlock --grace ten", "ten"),
            ("hyprlock --grace -5", "-5"),
            ("hyprlock --grace 10)", "10)"),
            ("hyprlock --grace 99999999999", "99999999999"),
        ] {
            assert_eq!(
                parse_grace(command),
                Err(GraceError::Unsupported(UnsupportedReason::InvalidSeconds(
                    word.to_string()
                ))),
                "{command:?}"
            );
        }
    }

    #[test]
    fn the_equals_form_is_refused_because_hyprlock_rejects_it() {
        // hyprutils' parser looks the whole `grace=10` up as an option name and fails,
        // so this form never set a grace period; the app neither reads nor rewrites it.
        for word in ["--grace=10", "-g=10"] {
            let command = format!("hyprlock {word}");
            assert_eq!(
                parse_grace(&command),
                Err(GraceError::Unsupported(UnsupportedReason::EqualsForm(
                    word.to_string()
                )))
            );
            assert!(set_grace(&command, 5).is_err());
        }
    }

    #[test]
    fn every_spelling_hyprlock_accepts_is_recognised() {
        for flag in ["--grace", "-g", "--g", "-grace"] {
            let command = format!("hyprlock {flag} 12");
            assert_eq!(parse_grace(&command), Ok(Some(12)), "{flag}");
            // Changing keeps the spelling; nothing is inserted beside it.
            assert_eq!(set_grace(&command, 3), Ok(format!("hyprlock {flag} 3")));
        }
    }

    #[test]
    fn several_grace_flags_are_refused() {
        assert_eq!(
            parse_grace("hyprlock --grace 5 -g 10"),
            Err(GraceError::Unsupported(
                UnsupportedReason::SeveralGraceFlags
            ))
        );
    }

    #[test]
    fn a_grace_flag_of_another_command_is_ignored() {
        // Only hyprlock's own arguments count: a `--grace` after `;` belongs to the next
        // command, so the invocation has no grace and one is inserted after `hyprlock`.
        let command = "hyprlock ; other --grace 5";
        assert_eq!(parse_grace(command), Ok(None));
        assert_eq!(
            set_grace(command, 2).as_deref(),
            Ok("hyprlock --grace 2 ; other --grace 5")
        );
    }

    #[test]
    fn errors_read_as_sentences_for_the_user() {
        assert_eq!(
            GraceError::NoHyprlock.to_string(),
            "the lock command does not start hyprlock"
        );
        assert!(
            GraceError::Unsupported(UnsupportedReason::EqualsForm("--grace=1".into()))
                .to_string()
                .contains("`--grace=1`")
        );
    }
}
