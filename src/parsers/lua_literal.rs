//! Lua literal values the app may write into Hyprland's Lua config or send to it
//! live (task 11.2; architecture §3 "Lua literal" row, §6; analysis §7.1, §8.6;
//! R8.3).
//!
//! # Why this module exists
//!
//! Hyprland ≥ 0.55 runs a **Lua** config provider: `~/.config/hypr/hyprland.lua`
//! is executed as Lua code and the app-owned `input.lua` is one
//! `hl.config({ input = { … } })` table constructor (analysis §7.1). Two parts of
//! the app produce Lua source text from user-chosen values:
//!
//! - the live-tweak helper [`HyprlandEval`](crate::core::reload::HyprlandEval),
//!   which renders an option change as `hyprctl eval 'hl.config({ … })'` because
//!   `hyprctl keyword` is rejected on a Lua session (task 11.2), and
//! - the surgical Lua-literal editor that will rewrite `key = <literal>,` lines in
//!   `input.lua` and the two `hl.env(…)` lines in `hyprland.lua` (task 11.3).
//!
//! Both must agree on exactly one thing: **which values can be written as a Lua
//! literal at all, and how**. That rule lives here, in one place, so the live call
//! and the file write can never disagree about a value (a value the file writer
//! refuses must not be applied live, and vice versa). Task 11.3 builds its parser
//! in this same module and reuses [`LuaLiteral`] and [`check_string_value`] for
//! every *new* value it writes.
//!
//! # The forbidden-character rule (R8.3)
//!
//! A string is always written as a double-quoted Lua short string with **no
//! escaping**. Rather than escape awkward characters, the app refuses them:
//!
//! - `"` would close the literal early, and `\` would start an escape sequence;
//!   either changes what Lua reads (and, in a file, can break the whole config);
//! - a newline or carriage return is illegal inside a Lua short string, so it
//!   would be a syntax error that stops `hyprland.lua` from loading;
//! - every other control character (tab, NUL, …) is refused too. Lua would accept
//!   some of them raw, but none is a legitimate part of any value the app edits
//!   (layouts, option lists, cursor names, monitor modes), and a raw control
//!   character in a config line is invisible and confusing to a human editor.
//!
//! Rejecting instead of escaping is the stricter choice, and it keeps the writer
//! lossless: what the user chose is exactly what lands between the quotes, so a
//! re-read yields the same value with no escape decoding. Values that come from
//! the app's own pickers never contain these characters; the rule exists so a
//! hand-typed or externally sourced value can never corrupt a working desktop
//! (R8.3).
//!
//! Like every module under `parsers/`, this one never imports `gtk` (enforced by
//! `tests/module_boundaries.rs`).

use std::fmt;

/// Lua 5.4's reserved words, which are not valid table keys in the bare
/// `key = value` form the app writes (a reserved word would need the
/// `["key"] = value` form, which the app never emits).
const LUA_RESERVED_WORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// A value that can be written verbatim as a Lua literal: a double-quoted string,
/// an integer, a finite float, or a boolean.
///
/// The variants are private so a value can only be built through the checked
/// constructors — a [`LuaLiteral`] that exists is always safe to render, and
/// [`fmt::Display`] (the rendering) cannot fail. Strings are checked by
/// [`check_string_value`]; floats must be finite, because `NaN`/`inf` have no Lua
/// literal spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct LuaLiteral(LiteralKind);

/// The private representation behind [`LuaLiteral`].
#[derive(Clone, Debug, PartialEq)]
enum LiteralKind {
    /// A string already checked by [`check_string_value`].
    String(String),
    /// A Lua integer (Lua 5.4 distinguishes the integer and float subtypes).
    Integer(i64),
    /// A finite Lua float.
    Float(f64),
    /// `true` or `false`.
    Boolean(bool),
}

impl LuaLiteral {
    /// A string literal, rendered double-quoted (`"us,se"`).
    ///
    /// Fails with [`LuaLiteralError::ForbiddenCharacter`] when `value` contains a
    /// character the forbidden-character rule refuses (see [`check_string_value`]).
    pub fn string(value: &str) -> Result<LuaLiteral, LuaLiteralError> {
        check_string_value(value)?;
        Ok(LuaLiteral(LiteralKind::String(value.to_string())))
    }

    /// An integer literal, rendered bare (`10`). Every `i64` is representable.
    pub fn integer(value: i64) -> LuaLiteral {
        LuaLiteral(LiteralKind::Integer(value))
    }

    /// A float literal, rendered bare and always with a fractional part or an
    /// exponent (`1.0`, `0.3`, `1e-7`) so Lua reads it as a float rather than an
    /// integer — matching how the dotfiles write float options (`scroll_factor =
    /// 1.0`).
    ///
    /// Fails with [`LuaLiteralError::NonFiniteNumber`] for `NaN` or an infinity,
    /// which have no Lua literal form.
    pub fn float(value: f64) -> Result<LuaLiteral, LuaLiteralError> {
        if !value.is_finite() {
            return Err(LuaLiteralError::NonFiniteNumber(value));
        }
        Ok(LuaLiteral(LiteralKind::Float(value)))
    }

    /// A boolean literal, rendered bare (`true` / `false`).
    pub fn boolean(value: bool) -> LuaLiteral {
        LuaLiteral(LiteralKind::Boolean(value))
    }
}

impl fmt::Display for LuaLiteral {
    /// Renders the value as Lua source text: strings double-quoted with no
    /// escaping (the constructor already refused every character that would need
    /// one), numbers and booleans bare.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            LiteralKind::String(value) => write!(f, "\"{value}\""),
            LiteralKind::Integer(value) => write!(f, "{value}"),
            // `Debug` (not `Display`) keeps the trailing `.0` of an integral float,
            // and uses an exponent for very large/small magnitudes — both valid Lua.
            LiteralKind::Float(value) => write!(f, "{value:?}"),
            LiteralKind::Boolean(value) => write!(f, "{value}"),
        }
    }
}

/// Checks a string against the forbidden-character rule (see the module docs):
/// `"`, `\`, and every control character (which includes newline and carriage
/// return) are refused.
///
/// This is the single definition of the rule. [`LuaLiteral::string`] applies it,
/// and the Lua-literal file editor (task 11.3) applies it to every new string
/// value it writes, so a value refused for a live `hyprctl eval` is refused for
/// the file too.
pub fn check_string_value(value: &str) -> Result<(), LuaLiteralError> {
    match value
        .chars()
        .find(|&character| character == '"' || character == '\\' || character.is_control())
    {
        Some(character) => Err(LuaLiteralError::ForbiddenCharacter {
            value: value.to_string(),
            character,
        }),
        None => Ok(()),
    }
}

/// Whether `name` can be written as a bare Lua table key (`name = value`): a Lua
/// *Name* (`[A-Za-z_][A-Za-z0-9_]*`) that is not a reserved word.
///
/// Used to validate every segment of a table path such as
/// `input.touchpad.natural_scroll` before it is rendered into Lua source, so a key
/// can never inject code into the generated call.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !LUA_RESERVED_WORDS.contains(&name)
}

/// Why a value or key cannot be written as Lua source text.
#[derive(Clone, Debug, PartialEq)]
pub enum LuaLiteralError {
    /// A string value contains a character the forbidden-character rule refuses
    /// (`"`, `\`, or a control character such as a newline; see the module docs).
    ForbiddenCharacter {
        /// The refused value.
        value: String,
        /// The first refused character found in it.
        character: char,
    },
    /// A float is `NaN` or infinite, which has no Lua literal spelling.
    NonFiniteNumber(f64),
    /// A table key (or a segment of a dotted table path) is not a valid bare Lua
    /// key — empty, not a Lua *Name*, or a reserved word (see [`is_identifier`]).
    InvalidKey(String),
}

impl fmt::Display for LuaLiteralError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LuaLiteralError::ForbiddenCharacter { value, character } => write!(
                f,
                "`{}` contains {character:?}, which cannot appear in a Lua string value \
                 (double quotes, backslashes and control characters such as newlines are refused)",
                value.escape_debug()
            ),
            LuaLiteralError::NonFiniteNumber(value) => {
                write!(
                    f,
                    "{value} is not a finite number and has no Lua literal form"
                )
            }
            LuaLiteralError::InvalidKey(key) => write!(
                f,
                "`{}` is not a valid Lua table key (it must match [A-Za-z_][A-Za-z0-9_]* \
                 and not be a reserved word)",
                key.escape_debug()
            ),
        }
    }
}

impl std::error::Error for LuaLiteralError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_render_double_quoted_and_verbatim() {
        let layouts = LuaLiteral::string("us,se").expect("a plain layout list is allowed");
        assert_eq!(layouts.to_string(), "\"us,se\"");
        // Single quotes, `#`, `--` and non-ASCII text need no escaping inside a
        // double-quoted Lua string, so they are allowed and kept verbatim.
        let odd = LuaLiteral::string("it's #1 -- ok ö").expect("allowed characters");
        assert_eq!(odd.to_string(), "\"it's #1 -- ok ö\"");
        assert_eq!(LuaLiteral::string("").unwrap().to_string(), "\"\"");
    }

    #[test]
    fn numbers_and_booleans_render_bare() {
        assert_eq!(LuaLiteral::integer(10).to_string(), "10");
        assert_eq!(LuaLiteral::integer(-3).to_string(), "-3");
        assert_eq!(LuaLiteral::float(0.3).unwrap().to_string(), "0.3");
        assert_eq!(
            LuaLiteral::float(1.0).unwrap().to_string(),
            "1.0",
            "an integral float keeps its fractional part so Lua reads a float"
        );
        assert_eq!(LuaLiteral::float(-0.5).unwrap().to_string(), "-0.5");
        assert_eq!(LuaLiteral::boolean(true).to_string(), "true");
        assert_eq!(LuaLiteral::boolean(false).to_string(), "false");
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(
                    LuaLiteral::float(value),
                    Err(LuaLiteralError::NonFiniteNumber(_))
                ),
                "{value} has no Lua literal form"
            );
        }
    }

    #[test]
    fn the_forbidden_character_rule_refuses_quotes_backslashes_and_control_characters() {
        for (value, character) in [
            ("a\"b", '"'),
            ("C:\\path", '\\'),
            ("two\nlines", '\n'),
            ("carriage\rreturn", '\r'),
            ("tab\there", '\t'),
            ("nul\0byte", '\0'),
        ] {
            assert_eq!(
                check_string_value(value),
                Err(LuaLiteralError::ForbiddenCharacter {
                    value: value.to_string(),
                    character,
                }),
                "{value:?} must be refused"
            );
            assert!(
                LuaLiteral::string(value).is_err(),
                "the string constructor applies the same rule to {value:?}"
            );
        }
        assert_eq!(
            check_string_value("grp:win_space_toggle,caps:escape"),
            Ok(())
        );
    }

    #[test]
    fn identifiers_follow_the_lua_name_rule_and_exclude_reserved_words() {
        for name in ["input", "natural_scroll", "_private", "kb_layout2"] {
            assert!(is_identifier(name), "{name} is a valid Lua key");
        }
        for name in [
            "",
            "2fast",
            "tap-to-click",
            "a.b",
            "a b",
            "end",
            "true",
            "ö",
        ] {
            assert!(!is_identifier(name), "{name:?} is not a valid bare Lua key");
        }
    }

    #[test]
    fn errors_describe_the_problem_for_a_reader() {
        let error = check_string_value("a\nb").unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("a\\nb"),
            "the value is shown escaped: {message}"
        );
        assert!(message.contains("newlines"), "{message}");
        assert!(
            LuaLiteralError::InvalidKey("tap-to-click".to_string())
                .to_string()
                .contains("tap-to-click")
        );
    }
}
