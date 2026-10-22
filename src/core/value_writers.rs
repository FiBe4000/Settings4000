//! Which writers each Theme-page value reaches, and which characters those writers
//! refuse — the stage-time half of R8.3 for the theme names, the cursor size and the
//! wallpaper / lock-screen image paths (task 9.27).
//!
//! # Why this module exists
//!
//! A Theme-page value is not written once. The cursor theme alone goes to five files
//! (R3.4), each through a different surgical writer (architecture §3), and each writer
//! refuses the characters that would break *its* format. Those rules differ: the Lua
//! writer refuses a `"` that the `settings.ini` writer accepts, and the hyprlang writer
//! refuses a `#` that the INI and env writers accept. A value that only some writers
//! accept — a directory name holding such a character is legal on Linux, so the theme
//! scan or the file chooser can offer one — would be written to some copies and refused
//! by the others. Task 9.6 stopped the models from claiming such a value as applied,
//! but that left the user with a permanently dirty page and no stated reason.
//!
//! So the value is refused where the user picks it instead: [`check_writable`] holds it
//! against the **union** of the rules of every writer it reaches and, on a refusal,
//! names the offending character and the writers that would reject it. The same check
//! runs again in step 1 of the Apply pipeline through
//! [`SettingId::validate`], so a value that reached the staged state some other way is
//! still caught before any file is touched. The writers keep their own guards as
//! defence in depth.
//!
//! # Where each rule comes from
//!
//! Nothing is restated here: each [`ValueWriter`] asks the writer's own definition.
//!
//! | Writer | Refuses | Defined by |
//! |---|---|---|
//! | both GTK `settings.ini`, `~/.icons/default/index.theme` | line feed, carriage return | [`ini::FORBIDDEN_VALUE_CHARACTERS`] |
//! | `uwsm/env` | anything but one bare shell word: whitespace (line breaks included), `$`, backtick, `;`, `&`, `\|`, `(`, `)`, `<`, `>`, `*`, `?`, `[`, `]`, `{`, `}`, `~`, `'`, `"`, `\`, an empty value | [`env::is_bare_shell_word`] |
//! | the `hl.env` lines of `hyprland.lua` | `"`, `\`, every control character | [`check_string_value`] |
//! | the GTK 4 theme links | anything but one plain directory name | [`is_single_directory_name`] |
//! | `hyprpaper.conf`, `hyprlock.conf` | line feed, carriage return, `#` | [`hyprlang::FORBIDDEN_VALUE_CHARACTERS`] |
//! | the `hyprctl hyprpaper wallpaper` request | `,` | [`HYPRPAPER_ARGUMENT_SEPARATOR`] |
//! | the `hyprctl setcursor <theme> <size>` request | whitespace | [`splits_setcursor_request`] |
//!
//! `uwsm/env` is the strictest writer of a cursor theme: the file is sourced by a shell,
//! so its value is shell code and must be one plain word (the env writer refuses rather
//! than quotes, so the copy reads back identical to the others). The setcursor request
//! adds nothing the shell-word rule does not already refuse, but is listed because it is
//! a writer of its own and a refusal message should name it.
//!
//! Theme values are INI *values*, never keys or group names, so the INI writer's
//! stricter key rule (no `=`) and group rule (no `[` or `]`) do not apply to them.
//!
//! # Which value reaches which writer
//!
//! See [`writers_of`]. In short: a GTK theme reaches `settings.ini` and the GTK 4 theme
//! links (task 11.16); an icon theme only `settings.ini`; a cursor theme `settings.ini`,
//! `uwsm/env`, the `hl.env` lines, the XCursor fallback `index.theme` (task 11.5) and
//! the live `hyprctl setcursor` request; a cursor size the same minus `index.theme`; the wallpaper path `hyprpaper.conf`,
//! `hyprlock.conf` (while the lock screen follows the wallpaper) and the live hyprpaper
//! request; a separate lock-screen image only `hyprlock.conf`.
//!
//! # No current writer refuses `#` in a theme name
//!
//! The cursor theme used to be written into the hyprlang `hyprland.conf` too, whose
//! writer refuses `#` (it would start an inline comment). That copy is gone since the
//! compositor switched to its Lua config (task 11.5), and the Lua string rule permits
//! `#` inside a string literal. A `#` in a GTK, icon or cursor theme name is therefore
//! accepted; only the image paths, which still go through hyprlang, refuse it.
//!
//! # Conservative where the host is unknown
//!
//! The check sees one value, not the host it will be written on. On a host whose
//! Hyprland runs the older hyprlang config the `hl.env` copy is not written at all
//! (task 11.2), and with a separate lock-screen image the wallpaper path does not reach
//! `hyprlock.conf`; the rule still includes those writers. Refusing a value that one
//! particular host could have taken is the safe direction (R8.3: never break a working
//! desktop), and it keeps a staged value valid when the configuration changes under it.
//!
//! Like everything under `core/` this module is GTK-free (R6.2).

use std::fmt;
use std::path::{Component, Path};

use crate::core::model::{SettingId, ValidationError};
use crate::core::reload::{HYPRPAPER_ARGUMENT_SEPARATOR, splits_setcursor_request};
use crate::parsers::lua_literal::check_string_value;
use crate::parsers::{env, hyprlang, ini};

/// One place a Theme-page value is written to, through its own writer and therefore
/// under its own character rule (see the module docs for the table).
///
/// Named in a refusal message so the user learns *which* copy could not hold the value,
/// not only that something could not (R8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueWriter {
    /// Both GTK `settings.ini` files (`~/.config/gtk-3.0/` and `~/.config/gtk-4.0/`),
    /// written through the INI writer.
    SettingsIni,
    /// `~/.config/uwsm/env`, the canonical session environment, written through the
    /// env writer.
    UwsmEnv,
    /// The two `hl.env("XCURSOR_THEME", …)` / `hl.env("XCURSOR_SIZE", …)` lines of
    /// `hyprland.lua`, written as Lua string literals through the Lua-literal editor.
    HyprlandLuaEnv,
    /// The XCursor fallback theme `~/.icons/default/index.theme` (its `Inherits=`),
    /// written through the INI writer.
    CursorFallbackTheme,
    /// The GTK 4 theme links `~/.config/gtk-4.0/{gtk.css,gtk-dark.css,assets}`, whose
    /// targets are built by joining the theme name onto a theme root as one path
    /// component.
    Gtk4ThemeLinks,
    /// `hyprpaper.conf`'s `wallpaper.path`, written through the hyprlang writer.
    HyprpaperConf,
    /// `hyprlock.conf`'s `background.path`, written through the hyprlang writer.
    HyprlockConf,
    /// The live `hyprctl hyprpaper wallpaper ,<path>[,<fit>]` request that shows a new
    /// wallpaper without a restart (task 11.14).
    HyprpaperRequest,
    /// The live `hyprctl setcursor <theme> <size>` request that applies a cursor change
    /// without a new login (task 4.4).
    HyprctlSetcursorRequest,
}

impl ValueWriter {
    /// How the writer is named in a refusal message.
    pub fn name(self) -> &'static str {
        match self {
            ValueWriter::SettingsIni => "settings.ini",
            ValueWriter::UwsmEnv => "uwsm/env",
            ValueWriter::HyprlandLuaEnv => "the hl.env lines of hyprland.lua",
            ValueWriter::CursorFallbackTheme => "~/.icons/default/index.theme",
            ValueWriter::Gtk4ThemeLinks => "the GTK 4 theme links in ~/.config/gtk-4.0",
            ValueWriter::HyprpaperConf => "hyprpaper.conf",
            ValueWriter::HyprlockConf => "hyprlock.conf",
            ValueWriter::HyprpaperRequest => "the live hyprctl hyprpaper request",
            ValueWriter::HyprctlSetcursorRequest => "the live hyprctl setcursor request",
        }
    }

    /// Whether this writer refuses `character` anywhere in a value.
    ///
    /// Each arm delegates to the writer's own definition. The Lua rule is not a fixed
    /// set (it refuses *every* control character), so the character is run through
    /// [`check_string_value`] itself rather than looked up in a list; likewise the
    /// `uwsm/env` rule is asked of [`env::is_bare_shell_word`] with the character as a
    /// one-character word. The GTK 4 link step has no per-character rule; its
    /// whole-value rule (like the shell rule's "not empty") is [`Self::refuses_value`].
    fn refuses_character(self, character: char) -> bool {
        match self {
            ValueWriter::SettingsIni | ValueWriter::CursorFallbackTheme => {
                ini::FORBIDDEN_VALUE_CHARACTERS.contains(&character)
            }
            ValueWriter::UwsmEnv => !env::is_bare_shell_word(character.encode_utf8(&mut [0; 4])),
            ValueWriter::HyprlandLuaEnv => {
                check_string_value(character.encode_utf8(&mut [0; 4])).is_err()
            }
            ValueWriter::HyprpaperConf | ValueWriter::HyprlockConf => {
                hyprlang::FORBIDDEN_VALUE_CHARACTERS.contains(&character)
            }
            ValueWriter::HyprpaperRequest => character == HYPRPAPER_ARGUMENT_SEPARATOR,
            ValueWriter::HyprctlSetcursorRequest => splits_setcursor_request(character),
            ValueWriter::Gtk4ThemeLinks => false,
        }
    }

    /// Whether this writer refuses `value` as a whole, independently of any single
    /// character: the GTK 4 link step, for a name that is not one plain directory name,
    /// and `uwsm/env`, for a value that is not a bare shell word (after the character
    /// check, only an empty value is left for this to catch).
    fn refuses_value(self, value: &str) -> bool {
        match self {
            ValueWriter::Gtk4ThemeLinks => !is_single_directory_name(value),
            ValueWriter::UwsmEnv => !env::is_bare_shell_word(value),
            _ => false,
        }
    }
}

impl fmt::Display for ValueWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why [`check_writable`] refused a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwritable {
    /// The value contains this character, which at least one of its writers refuses.
    /// It is the first such character in reading order.
    Character(char),
    /// The value is not one plain directory name (it holds a `/`, or is empty, `.` or
    /// `..`), which the GTK 4 theme link step needs.
    NotASingleDirectoryName,
    /// The value is not one bare shell word, which `uwsm/env` needs because a shell
    /// sources it: it holds this character (whitespace or a shell special character,
    /// the first refused one in reading order, reported with every writer refusing it),
    /// or it is empty (`None`).
    NotABareShellWord {
        /// The first refused character, or `None` for an empty value.
        character: Option<char>,
    },
}

/// The writers `id`'s value reaches, in the order a refusal message lists them, or an
/// empty slice for a setting this module has no rule for (every setting outside the
/// Theme page: their own validators in [`crate::core::model`] already mirror their one
/// writer).
///
/// The wallpaper path lists `hyprlock.conf` because the lock screen follows the
/// wallpaper unless the user picked a separate image (analysis §6.2) — see "Conservative
/// where the host is unknown" in the module docs.
pub fn writers_of(id: SettingId) -> &'static [ValueWriter] {
    match id {
        SettingId::GtkTheme => &[ValueWriter::SettingsIni, ValueWriter::Gtk4ThemeLinks],
        SettingId::IconTheme => &[ValueWriter::SettingsIni],
        SettingId::CursorTheme => &[
            ValueWriter::SettingsIni,
            ValueWriter::UwsmEnv,
            ValueWriter::HyprlandLuaEnv,
            ValueWriter::CursorFallbackTheme,
            ValueWriter::HyprctlSetcursorRequest,
        ],
        SettingId::CursorSize => &[
            ValueWriter::SettingsIni,
            ValueWriter::UwsmEnv,
            ValueWriter::HyprlandLuaEnv,
            ValueWriter::HyprctlSetcursorRequest,
        ],
        SettingId::WallpaperPath => &[
            ValueWriter::HyprpaperConf,
            ValueWriter::HyprlockConf,
            ValueWriter::HyprpaperRequest,
        ],
        SettingId::LockBackgroundPath => &[ValueWriter::HyprlockConf],
        _ => &[],
    }
}

/// Checks that every writer `id`'s value reaches would accept `value` (R8.3), so a
/// value is never written to some copies and refused by others.
///
/// Characters are checked first, in reading order: the first character any writer
/// refuses is reported together with **every** writer that refuses it, so a line break
/// in a cursor theme names all four files and the setcursor request while a `"` names
/// `uwsm/env` and `hyprland.lua`. A value whose characters all pass is then checked
/// against the whole-value rules of the GTK 4 link step and `uwsm/env`. The refused value travels in the error so the message can quote
/// it.
pub fn check_writable(id: SettingId, value: &str) -> Result<(), ValidationError> {
    let writers = writers_of(id);
    let refusal = |problem: Unwritable, refusing: Vec<ValueWriter>| ValidationError::Unwritable {
        value: value.to_string(),
        problem,
        writers: refusing,
    };
    if let Some(character) = value
        .chars()
        .find(|&character| writers.iter().any(|w| w.refuses_character(character)))
    {
        let refusing: Vec<ValueWriter> = writers
            .iter()
            .copied()
            .filter(|writer| writer.refuses_character(character))
            .collect();
        // A character the shell rule refuses gets the shell-word explanation, so the
        // user learns why `uwsm/env` cannot hold e.g. a space; the other writers
        // refusing the same character are still named.
        let problem = if refusing.contains(&ValueWriter::UwsmEnv) {
            Unwritable::NotABareShellWord {
                character: Some(character),
            }
        } else {
            Unwritable::Character(character)
        };
        return Err(refusal(problem, refusing));
    }
    // Whole-value rules. No setting reaches both writers that have one (the GTK theme
    // reaches the links, the cursor values reach `uwsm/env`), so the first is the only.
    let refusing: Vec<ValueWriter> = writers
        .iter()
        .copied()
        .filter(|writer| writer.refuses_value(value))
        .collect();
    match refusing.first() {
        None => Ok(()),
        Some(ValueWriter::UwsmEnv) => Err(refusal(
            Unwritable::NotABareShellWord { character: None },
            refusing,
        )),
        Some(_) => Err(refusal(Unwritable::NotASingleDirectoryName, refusing)),
    }
}

/// Whether `name` is one plain directory name: non-empty, no `/`, and not `.` or `..`.
///
/// The single definition of the rule the GTK 4 theme link step applies before it joins
/// a GTK theme name onto a theme root (task 11.16), so a name read from a hand-edited
/// `settings.ini` can never make a link point outside the theme folders. The `/` is
/// refused explicitly because [`Path::components`] silently drops a trailing one
/// (`"Theme/"` would otherwise count as the single component `Theme`).
pub fn is_single_directory_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    !name.contains('/')
        && matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        )
}

/// Describes `character` for a refusal message: a control or whitespace character by
/// its name or code point (it would be invisible or break the message's own line),
/// anything else quoted as itself.
pub(crate) fn describe_character(character: char) -> String {
    match character {
        '\n' => "a line break (U+000A)".to_string(),
        '\r' => "a carriage return (U+000D)".to_string(),
        '\t' => "a tab (U+0009)".to_string(),
        ' ' => "a space".to_string(),
        // Quoted as itself it would read as an empty code span: ```.
        '`' => "a backtick".to_string(),
        control if control.is_control() => {
            format!("the control character U+{:04X}", u32::from(control))
        }
        // Other whitespace (a no-break space, …) is invisible when quoted as itself.
        space if space.is_whitespace() => {
            format!("the whitespace character U+{:04X}", u32::from(space))
        }
        other => format!("the character `{other}`"),
    }
}

/// Renders `value` for a refusal message with its control characters escaped (`\n`,
/// `\u{1b}`, …), so a refused value cannot split or garble the message that quotes
/// it. Every other character, a `"` or `\` included, is shown as itself.
pub(crate) fn printable(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                character.escape_default().to_string()
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// Joins writer names as English prose: `a`, `a and b`, `a, b and c`.
pub(crate) fn join_writers(writers: &[ValueWriter]) -> String {
    match writers {
        [] => String::new(),
        [only] => only.name().to_string(),
        [init @ .., last] => {
            let init: Vec<&str> = init.iter().map(|writer| writer.name()).collect();
            format!("{} and {}", init.join(", "), last.name())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writers named by a refusal of `value` for `id`, or `None` when it is
    /// accepted — the shape most assertions below need.
    fn refusal(id: SettingId, value: &str) -> Option<(Unwritable, Vec<ValueWriter>)> {
        match check_writable(id, value) {
            Ok(()) => None,
            Err(ValidationError::Unwritable {
                problem, writers, ..
            }) => Some((problem, writers)),
            Err(other) => panic!("unexpected error kind: {other}"),
        }
    }

    #[test]
    fn a_hash_in_a_theme_name_is_accepted() {
        // Task 9.27 corrects the task text's premise: the hyprlang `hyprland.conf` copy
        // that refused `#` was retired in task 11.5, and no writer of a theme name
        // refuses `#` today — not the INI, env or Lua string rule, nor the link step.
        for id in [
            SettingId::GtkTheme,
            SettingId::IconTheme,
            SettingId::CursorTheme,
        ] {
            assert_eq!(refusal(id, "Nord#ic"), None, "{id:?} accepts a `#`");
        }
    }

    #[test]
    fn a_quote_in_a_cursor_theme_names_the_env_and_lua_writers() {
        // A `"` or `\` would end or escape the Lua string, and quote or escape a word in
        // the shell that sources uwsm/env; the INI writers accept both.
        let shell_word = |character| Unwritable::NotABareShellWord {
            character: Some(character),
        };
        assert_eq!(
            refusal(SettingId::CursorTheme, "Nord\"ic"),
            Some((
                shell_word('"'),
                vec![ValueWriter::UwsmEnv, ValueWriter::HyprlandLuaEnv]
            ))
        );
        assert_eq!(
            refusal(SettingId::CursorTheme, "Nord\\ic"),
            Some((
                shell_word('\\'),
                vec![ValueWriter::UwsmEnv, ValueWriter::HyprlandLuaEnv]
            ))
        );
    }

    #[test]
    fn a_tab_in_a_cursor_theme_is_refused_by_three_writers() {
        // A tab is a control character to the Lua string rule and whitespace to the
        // shell-word rule and the setcursor request; the INI writers accept it.
        assert_eq!(
            refusal(SettingId::CursorTheme, "Nord\tic"),
            Some((
                Unwritable::NotABareShellWord {
                    character: Some('\t')
                },
                vec![
                    ValueWriter::UwsmEnv,
                    ValueWriter::HyprlandLuaEnv,
                    ValueWriter::HyprctlSetcursorRequest,
                ]
            ))
        );
    }

    #[test]
    fn a_line_break_names_every_cursor_writer() {
        assert_eq!(
            refusal(SettingId::CursorTheme, "Nord\nic"),
            Some((
                Unwritable::NotABareShellWord {
                    character: Some('\n')
                },
                vec![
                    ValueWriter::SettingsIni,
                    ValueWriter::UwsmEnv,
                    ValueWriter::HyprlandLuaEnv,
                    ValueWriter::CursorFallbackTheme,
                    ValueWriter::HyprctlSetcursorRequest,
                ]
            ))
        );
    }

    #[test]
    fn shell_special_characters_name_uwsm_env() {
        // Task 9.27 review: uwsm/env is sourced by a shell, so a cursor theme must be one
        // bare word there. A space also splits the setcursor request; the others are
        // refused by uwsm/env alone.
        assert_eq!(
            refusal(SettingId::CursorTheme, "My Theme"),
            Some((
                Unwritable::NotABareShellWord {
                    character: Some(' ')
                },
                vec![ValueWriter::UwsmEnv, ValueWriter::HyprctlSetcursorRequest]
            ))
        );
        for (name, character) in [("a;b", ';'), ("a`id`", '`'), ("a(b", '('), ("a$b", '$')] {
            assert_eq!(
                refusal(SettingId::CursorTheme, name),
                Some((
                    Unwritable::NotABareShellWord {
                        character: Some(character)
                    },
                    vec![ValueWriter::UwsmEnv]
                )),
                "{name:?}"
            );
        }
        assert_eq!(
            refusal(SettingId::CursorTheme, ""),
            Some((
                Unwritable::NotABareShellWord { character: None },
                vec![ValueWriter::UwsmEnv]
            ))
        );
        for name in [
            "breeze_cursors",
            "Nordic-cursors",
            "Bibata-Modern.Ice",
            "Nord#ic",
        ] {
            assert_eq!(refusal(SettingId::CursorTheme, name), None, "{name:?}");
        }
        // Neither the GTK nor the icon theme reaches uwsm/env.
        assert_eq!(refusal(SettingId::IconTheme, "My Icons"), None);
    }

    #[test]
    fn the_first_refused_character_is_reported() {
        // Reading order decides, so the message points at the leftmost problem.
        assert_eq!(
            refusal(SettingId::CursorTheme, "a\"b\nc").map(|(problem, _)| problem),
            Some(Unwritable::NotABareShellWord {
                character: Some('"')
            })
        );
    }

    #[test]
    fn icon_and_gtk_themes_accept_lua_only_characters() {
        // Neither reaches hyprland.lua, so a `"` is legal for them.
        assert_eq!(refusal(SettingId::IconTheme, "Pap\"irus"), None);
        assert_eq!(refusal(SettingId::GtkTheme, "Nord\"ic"), None);
        assert_eq!(
            refusal(SettingId::IconTheme, "Pap\rirus"),
            Some((Unwritable::Character('\r'), vec![ValueWriter::SettingsIni]))
        );
    }

    #[test]
    fn a_gtk_theme_must_be_one_directory_name() {
        for name in ["a/b", "Theme/", "..", ".", ""] {
            assert_eq!(
                refusal(SettingId::GtkTheme, name),
                Some((
                    Unwritable::NotASingleDirectoryName,
                    vec![ValueWriter::Gtk4ThemeLinks]
                )),
                "{name:?}"
            );
        }
        // The icon theme has no link step, so the same names pass its rule.
        assert_eq!(refusal(SettingId::IconTheme, "a/b"), None);
    }

    #[test]
    fn single_directory_names() {
        for name in ["Adwaita", "Nord#ic", "..hidden", "a.b", "with space"] {
            assert!(is_single_directory_name(name), "{name:?}");
        }
        for name in ["", ".", "..", "a/b", "/abs", "trailing/", "./a"] {
            assert!(!is_single_directory_name(name), "{name:?}");
        }
    }

    #[test]
    fn a_hash_in_an_image_path_names_both_hyprlang_files() {
        assert_eq!(
            refusal(SettingId::WallpaperPath, "/pics/wall#1.png"),
            Some((
                Unwritable::Character('#'),
                vec![ValueWriter::HyprpaperConf, ValueWriter::HyprlockConf]
            ))
        );
        assert_eq!(
            refusal(SettingId::LockBackgroundPath, "/pics/lock#1.png"),
            Some((Unwritable::Character('#'), vec![ValueWriter::HyprlockConf]))
        );
    }

    #[test]
    fn a_comma_is_refused_only_where_the_request_goes() {
        // The live hyprpaper request carries only the wallpaper; a separate lock image
        // goes to hyprlock.conf alone, whose hyprlang rule allows a comma.
        assert_eq!(
            refusal(SettingId::WallpaperPath, "/pics/a,b.png"),
            Some((
                Unwritable::Character(','),
                vec![ValueWriter::HyprpaperRequest]
            ))
        );
        assert_eq!(
            refusal(SettingId::LockBackgroundPath, "/pics/a,b.png"),
            None
        );
    }

    #[test]
    fn settings_outside_the_theme_page_have_no_writer_rule() {
        assert!(writers_of(SettingId::LockCommand).is_empty());
        assert!(check_writable(SettingId::LockCommand, "a#b\n").is_ok());
    }

    #[test]
    fn message_helpers_render_for_a_reader() {
        assert_eq!(describe_character('\n'), "a line break (U+000A)");
        assert_eq!(describe_character('\u{1b}'), "the control character U+001B");
        assert_eq!(describe_character('"'), "the character `\"`");
        assert_eq!(describe_character(' '), "a space");
        assert_eq!(describe_character('`'), "a backtick");
        assert_eq!(
            describe_character('\u{a0}'),
            "the whitespace character U+00A0"
        );
        assert_eq!(printable("a\"b\nc\u{1b}"), "a\"b\\nc\\u{1b}");
        assert_eq!(join_writers(&[ValueWriter::SettingsIni]), "settings.ini");
        assert_eq!(
            join_writers(&[
                ValueWriter::SettingsIni,
                ValueWriter::UwsmEnv,
                ValueWriter::CursorFallbackTheme
            ]),
            "settings.ini, uwsm/env and ~/.icons/default/index.theme"
        );
    }
}
