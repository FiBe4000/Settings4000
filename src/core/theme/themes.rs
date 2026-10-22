//! The GTK/icon/cursor theme staging model for the Theme page (task 6.4; R2.2, R3.3,
//! R3.4, R4.2, R4.4) — see [`super`] for how the page's three models fit together.
//!
//! Unlike the palette, these controls *do* edit files: a change is written identically to
//! every place the value is duplicated and applied live with `gsettings set` +
//! `hyprctl setcursor` (R3.3/R3.4). The GTK and icon themes live in both GTK
//! `settings.ini` files; the cursor theme and size additionally in `uwsm/env`, in the two
//! `hl.env("XCURSOR_THEME", …)` / `hl.env("XCURSOR_SIZE", …)` lines of Hyprland's Lua
//! config `hyprland.lua`, and (the theme only) in the XCursor fallback theme
//! `~/.icons/default/index.theme` (analysis §7.7, §8.4). The model handles the
//! `GTK_THEME` override (never fight it, R3.3), gates the live-restyle claim on the
//! settings portal (R2.2), and reports cursor copies that disagree with each other.
//! It also reports two conditions that mean another program is likely to rewrite the
//! GTK settings after the app does (task 11.12, analysis §7.7, §8.4): a `settings.ini`
//! that is a plain file although the dotfiles repo carries a copy of it, and a running
//! `xsettingsd`. An icon-theme change also reaches the notification daemon, notifd,
//! through its `Reload()` method (task 11.14); on a notifd too old to have it, the
//! model supplies the note that notification icons follow at notifd's next start.
//! A GTK-theme change has a third leg beside gsettings and the two `settings.ini`
//! files: GTK 4 and libadwaita apps take their theme from three symlinks
//! `~/.config/gtk-4.0/{gtk.css,gtk-dark.css,assets}` into the installed theme's
//! `gtk-4.0/` directory (analysis §8.4), which the model re-points, removes, or —
//! when one of them is a file of the user's own — leaves alone (task 11.16; see
//! [`ThemesModel::gtk4_links_note`]). Because each value reaches several writers with
//! different character rules, a name or size is checked against all of them when the
//! user picks it, and again in the Apply pipeline's step 1 (task 9.27, R8.3; see
//! [`crate::core::value_writers`]). [`ThemesModel`]'s own docs carry the rest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, LinkAction, LinkWrite, SkippedWrite, WriteMode};
use crate::core::home_path::contract_home;
use crate::core::model::{SettingId, ValidationError, Value, parse_cursor_size};
use crate::core::notifd::NotifdStatus;
use crate::core::reload::{BackingFile, CursorValue, ReloadParams};
use crate::core::value_writers::is_single_directory_name;
use crate::parsers::env::{EnvFile, GtkThemeOverride};
use crate::parsers::ini::IniFile;
use crate::parsers::lua_literal::{LuaConfigFile, LuaEditError};

use super::{BackingSet, BackingText, Selection, log_skipped_writes, skipped_writes_note};

/// The GLib key-file group the app's theme keys live under in `settings.ini`.
const SETTINGS_GROUP: &str = "Settings";
/// `settings.ini` key naming the GTK theme (R3.3).
const KEY_GTK_THEME: &str = "gtk-theme-name";
/// `settings.ini` key naming the icon theme (R3.4).
const KEY_ICON_THEME: &str = "gtk-icon-theme-name";
/// `settings.ini` key naming the cursor theme (R3.4).
const KEY_CURSOR_THEME: &str = "gtk-cursor-theme-name";
/// `settings.ini` key naming the cursor size (R3.4).
const KEY_CURSOR_SIZE: &str = "gtk-cursor-theme-size";
/// Environment variable naming the cursor theme, exported by `uwsm/env` and declared by
/// `hyprland.lua`'s `hl.env` call of the same name (analysis §7.7).
const ENV_CURSOR_THEME: &str = "XCURSOR_THEME";
/// Environment variable naming the cursor size, exported by `uwsm/env` and declared by
/// `hyprland.lua`'s `hl.env` call of the same name (analysis §7.7). In Lua the size is a
/// string argument like every `hl.env` value (`hl.env("XCURSOR_SIZE", "24")`).
const ENV_CURSOR_SIZE: &str = "XCURSOR_SIZE";
/// The key-file group of an icon theme's `index.theme`, which holds the XCursor
/// fallback's `Inherits=` (analysis §8.4).
const ICON_THEME_GROUP: &str = "Icon Theme";
/// The `index.theme` key that names the theme the `default` cursor theme inherits its
/// cursors from — i.e. the cursor theme programs ignoring `XCURSOR_THEME` fall back to.
/// The XCursor format allows a comma-separated list here; the app writes the single
/// chosen theme, which is what the dotfiles ship.
const KEY_INHERITS: &str = "Inherits";
/// The `index.theme` the app creates when `~/.icons/default/index.theme` is absent,
/// before the chosen theme is set as its `Inherits=`: the `[Icon Theme]` group with the
/// two descriptive keys the dotfiles' tracked copy carries (analysis §8.4), so a created
/// file is byte-identical to the one `setup.sh` would deploy for the same theme.
const CURSOR_FALLBACK_TEMPLATE: &str = "[Icon Theme]\nName=Default\nComment=Default Cursor Theme\n";

/// Where the dotfiles repo keeps its copy of `~/.config/gtk-3.0/settings.ini`, relative
/// to the repo root. The dotfiles deploy the live file as a symlink to this copy
/// (analysis §6.5, §8.4); a plain live file beside it means the link was replaced
/// (task 11.12).
const GTK3_SETTINGS_REPO_RELATIVE: &str = "config/gtk-3.0/settings.ini";
/// Where the dotfiles repo keeps its copy of `~/.config/gtk-4.0/settings.ini`, relative
/// to the repo root — see [`GTK3_SETTINGS_REPO_RELATIVE`]. This is the file `nwg-look`
/// replaced with a plain copy on the target machine (analysis §7.7).
const GTK4_SETTINGS_REPO_RELATIVE: &str = "config/gtk-4.0/settings.ini";

/// The entries of `~/.config/gtk-4.0/` that theme GTK 4 and libadwaita apps, in the
/// order the link steps are planned (task 11.16, analysis §8.4).
///
/// GTK 4 loads `gtk.css` from its user configuration directory on top of the theme it
/// was told to use, and libadwaita — which ignores `gtk-theme-name` — loads `gtk.css`
/// or, in dark mode, `gtk-dark.css` from there; `assets` holds the images those
/// stylesheets reference by relative path. The dotfiles' `setup.sh` links each of the
/// three to the entry of the same name in `/usr/share/themes/<gtk-theme-name>/gtk-4.0/`
/// (originally `nwg-look` did), which is the layout the app maintains.
const GTK4_THEME_LINKS: [&str; 3] = ["gtk.css", "gtk-dark.css", "assets"];

/// The subdirectory of an installed GTK theme that holds its GTK 4 part.
const GTK4_THEME_SUBDIR: &str = "gtk-4.0";

/// Cursor pixel sizes offered in the cursor-size drop-down, in ascending order. The
/// currently-configured size is added too (see [`ThemesModel::load`]) so an unusual
/// on-disk value stays selectable — mirroring the Display page's scale drop-down.
const CURATED_CURSOR_SIZES: &[&str] = &["16", "24", "32", "48", "64"];

/// GTK theme names that are always offered because GTK carries the stylesheet inside
/// the library rather than in a theme directory (R3.3: "installed theme dirs … plus the
/// built-in Adwaita variants").
///
/// A built-in theme need not exist under `~/.themes` / `/usr/share/themes` to be
/// selectable, so a directory scan alone can miss it: on a system whose GTK packages
/// ship no `Adwaita` directory (which is the case on plain GTK 4 installs — the
/// stylesheet lives in the library's compiled-in GResource bundle), Adwaita would
/// silently vanish from the drop-down even though setting `gtk-theme-name=Adwaita`
/// works. Merging these names into the scan results keeps the promise of R3.3
/// regardless of packaging.
///
/// Why exactly these two — verified when the list was written on GTK 3.24 and GTK 4.22
/// (the release that renamed the bundled theme to `Default`) by loading each name
/// through GTK's named-theme CSS loader, and re-checked on GTK 4.24 by inspecting the
/// library's resource names (task 9.29); re-verify against `gtk_css_provider_load_named`
/// if a future GTK renames the bundled theme again:
///
/// - `Adwaita` is the long-standing light built-in. GTK 3 ships it as the GResource
///   theme `Adwaita`; GTK 4.22 renamed its bundled resource to `Default`, and loading
///   `Adwaita` there yields byte-identical CSS to `Default`, so the name still selects
///   the built-in look on both. GTK treats these names as deliberate aliases rather
///   than leaving it to chance — `gtk_css_provider_load_named` in GTK's
///   `gtk/gtkcssprovider.c` states that it "accept[s] the names HighContrast,
///   HighContrastInverse, Adwaita and Adwaita-dark as aliases for the variants of the
///   Default theme". Worth re-checking against that function if a future GTK changes
///   the bundled theme's name again.
///   `Adwaita` alone reaches the built-in through the loader's generic
///   unknown-name fallback (the same path a typo takes), so it resolves correctly but
///   is not pinned by a name-specific branch the way the dark variant is.
/// - `Adwaita-dark` selects the dark built-in on GTK 4, whose loader maps that name onto
///   the built-in theme's dark variant (its CSS differs from the light built-in's).
///   Note the asymmetry: GTK 3 has no `Adwaita-dark` resource and falls back to light
///   Adwaita, unless the distribution installs an `Adwaita-dark` theme directory (some
///   do, via `gnome-themes-extra`) — in which case the scan finds it anyway and the name
///   collapses with the built-in entry. Either way the app writes the chosen name
///   verbatim to `gsettings` and both `settings.ini` files, so the copies never desync
///   (R3.3); it is GTK, not this app, that decides how a name resolves. Because the
///   result surprises — dark GTK 4 apps beside light GTK 3 apps, with both files
///   honestly holding the same name — the Theme page says so while the name is
///   selected and GTK 3 has no directory to load it from
///   ([`ThemesModel::one_sided_gtk_theme_note`], task 9.29).
///
/// Each entry also records how GTK 3 resolves the name ([`Gtk3Resolution`]), so a
/// built-in added here later has to be classified for that note too.
///
/// Kept sorted here for readability only; ordering in the drop-down comes from the
/// sorted set the theme scan builds.
const BUILTIN_GTK_THEMES: &[BuiltinGtkTheme] = &[
    // GTK 3 bundles the stylesheet as the GResource theme `Adwaita` itself.
    BuiltinGtkTheme {
        name: "Adwaita",
        gtk3: Gtk3Resolution::BuiltIn,
    },
    // GTK 3 bundles Adwaita's dark stylesheet only as the `dark` *variant* of the
    // `Adwaita` resource, which no theme name selects, so this name needs a directory.
    BuiltinGtkTheme {
        name: "Adwaita-dark",
        gtk3: Gtk3Resolution::NeedsDirectory {
            fallback_look: "the light Adwaita look",
        },
    },
];

/// One GTK theme name offered without a theme directory (see [`BUILTIN_GTK_THEMES`]).
#[derive(Clone, Copy, Debug)]
struct BuiltinGtkTheme {
    /// The name as it is written to `gtk-theme-name` and `gsettings`.
    name: &'static str,
    /// Whether GTK 3 can load the name without a theme directory (task 9.29).
    gtk3: Gtk3Resolution,
}

/// How GTK 3 resolves a [`BuiltinGtkTheme`] name when no theme directory of that name
/// is installed (task 9.29).
///
/// Every built-in name works on GTK 4, whose loader accepts all of them as aliases of
/// its compiled-in theme (see [`BUILTIN_GTK_THEMES`]); GTK 3 is the side that differs.
/// GTK 3.24 looks a theme name up in this order (`_gtk_css_provider_load_named` in its
/// `gtk/gtkcssprovider.c`, which checks the resource itself, then asks
/// `_gtk_css_find_theme` for the theme folders, then falls back):
///
/// 1. a stylesheet compiled into the library as the GResource
///    `/org/gtk/libgtk/theme/<name>/gtk.css` — the GTK 3.24 library carries three such
///    themes: `Adwaita` (with a `dark` variant selected by
///    `gtk-application-prefer-dark-theme`, not by a name), `HighContrast` and
///    `HighContrastInverse`;
/// 2. a stylesheet file in a theme folder: under `$XDG_DATA_HOME/themes`, `~/.themes`,
///    each `$XDG_DATA_DIRS` entry's `themes/` and GTK's own `<prefix>/share/themes`,
///    `_gtk_css_find_theme_dir` probes `<base>/<name>/gtk-3.<N>/gtk.css` (or
///    `gtk-<variant>.css`) with `N` running from the library's minor version (24) down
///    in steps of 2 to 14, then `gtk-3.0`. A `<name>/gtk-3.0/` directory without a
///    `gtk.css` — a keybinding-only package such as `Default` or `Emacs`, which ships
///    nothing but `gtk-keys.css` — is not a theme to this step. The app's scan mirrors
///    the step with [`has_gtk3_stylesheet`] (task 9.30). The app's R3.3 theme roots
///    ([`ThemeRoots::gtk_theme_dirs`], filled by `core::load::theme_roots`) cover only
///    `~/.themes`, `$XDG_DATA_HOME/themes` and `/usr/share/themes`, not the other
///    `$XDG_DATA_DIRS` entries GTK walks: a theme installed under another data
///    directory is invisible to the scan (backlog task 10.6), so the Theme page's
///    one-sided note can be a false positive there;
/// 3. failing both, the same name without the variant, and then the default theme
///    (`DEFAULT_THEME_NAME`, the light `Adwaita` built-in).
///
/// (Checked against the GTK 3.24 and GTK 4 libraries on the target system: the GTK 3
/// library's resource tree has no `Adwaita-dark` entry and the string appears nowhere
/// in it, while the GTK 4 library names it in its theme loader.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gtk3Resolution {
    /// GTK 3 has the name compiled in (step 1), so it resolves the same with or without
    /// a directory.
    BuiltIn,
    /// GTK 3 has no compiled-in theme of this name, so it uses the name only when an
    /// installed `<name>` directory carries a GTK 3 stylesheet (step 2,
    /// [`has_gtk3_stylesheet`]) and otherwise falls back.
    NeedsDirectory {
        /// What GTK 3 apps show instead, phrased to complete the Theme page's sentence
        /// "GTK 3 apps will use …".
        fallback_look: &'static str,
    },
}

/// Whether `name` is one of the [`BUILTIN_GTK_THEMES`].
fn is_builtin_gtk_theme(name: &str) -> bool {
    builtin_gtk_theme(name).is_some()
}

/// The [`BUILTIN_GTK_THEMES`] entry named `name`, if there is one.
fn builtin_gtk_theme(name: &str) -> Option<&'static BuiltinGtkTheme> {
    BUILTIN_GTK_THEMES
        .iter()
        .find(|builtin| builtin.name == name)
}

/// The filesystem roots scanned for installed themes (R3.3/R3.4).
///
/// Injected rather than hardcoded so the scan is unit-tested against a fixture tree
/// (the accept criterion): a test points these at temporary directories. The window's
/// startup loader fills them from the XDG environment (`~/.themes`, the data dirs,
/// `/usr/share/...`).
#[derive(Clone, Debug)]
pub struct ThemeRoots {
    /// Directories that hold GTK theme directories (`~/.themes`,
    /// `~/.local/share/themes`, `/usr/share/themes`). A subdirectory is a GTK theme
    /// when it carries a stylesheet for either GTK version — a `gtk.css` inside a
    /// `gtk-3.<N>/` or the `gtk-4.0/` subdirectory (R3.3, task 9.30); a subdirectory
    /// holding only keybindings (`gtk-keys.css`) does not count.
    ///
    /// They are also the only places the GTK 4 theme links may point into (task
    /// 11.16): a theme's `gtk-4.0/` directory is linked from the first of these roots
    /// that has one, and only when it — and each entry linked — still resolves inside
    /// one of these roots.
    pub gtk_theme_dirs: Vec<PathBuf>,
    /// Directories that hold icon and cursor theme directories (`~/.icons`,
    /// `~/.local/share/icons`, `/usr/share/icons`). A subdirectory with a `cursors/`
    /// subdirectory is a cursor theme; one with an `index.theme` (and no `cursors/`)
    /// is an icon theme (R3.4).
    pub icon_dirs: Vec<PathBuf>,
}

/// The live paths of the files a theme/cursor change writes (R8.5).
///
/// Injected for the same reason as [`ThemeRoots`]: tests point them at a fixture tree,
/// and the writer follows symlinks so a dotfiles-deployed file is handled identically
/// to a plain one. The cursor is duplicated across all five; a GTK/icon theme change
/// touches only the two `settings.ini` files (R3.4, analysis §7.7, §8.4). Two copies are
/// optional:
///
/// - the Hyprland-side copy is guarded by the Hyprland config provider (task 11.2,
///   analysis §8.6): the startup loader leaves it `None` unless the compositor runs its
///   Lua provider, and then no Hyprland-side cursor write exists at all — the app keeps
///   no hyprlang cursor path;
/// - the XCursor fallback theme lives under `$HOME`, not the XDG config directory, so it
///   is `None` only when the home directory is unknown (the app then never guesses a
///   relative path to create a file at).
#[derive(Clone, Debug)]
pub struct ThemesPaths {
    /// `~/.config/gtk-3.0/settings.ini`.
    pub gtk3_settings: PathBuf,
    /// `~/.config/gtk-4.0/settings.ini`. Its directory is also where the GTK 4 theme
    /// links `gtk.css`, `gtk-dark.css` and `assets` live (task 11.16).
    pub gtk4_settings: PathBuf,
    /// `~/.config/hypr/hyprland.lua`, of which only the two cursor `hl.env` lines are
    /// ever edited, or `None` when the Hyprland-side cursor write is hidden because the
    /// compositor does not run its Lua config provider (task 11.2).
    pub hyprland_lua: Option<PathBuf>,
    /// `~/.config/uwsm/env` (the canonical cursor env copy).
    pub uwsm_env: PathBuf,
    /// `~/.icons/default/index.theme`, the XCursor fallback theme (`Inherits=` under
    /// `[Icon Theme]`, analysis §8.4) — created by the cursor write when absent — or
    /// `None` when the home directory is unknown.
    pub cursor_fallback_theme: Option<PathBuf>,
    /// The dotfiles repository root detected behind the deployed config (R8.5,
    /// [`Capabilities::dotfiles_repo_root`](crate::core::detect::Capabilities::dotfiles_repo_root)),
    /// or `None` when there is no repo. Nothing under it is ever read or written by
    /// this model: it is only used to notice a `settings.ini` that is a plain file
    /// while the repo still carries its copy (see
    /// [`ThemesModel::unlinked_gtk_settings`], task 11.12).
    pub dotfiles_repo: Option<PathBuf>,
    /// The user's home directory, or `None` when it is unknown. Used only to show paths
    /// in the Theme page's notes in the familiar `~/…` form
    /// ([`ThemesModel::unlinked_gtk_settings_notes`]); a path outside it, or any path
    /// when it is `None`, is shown absolute. Like `WallpaperPaths::home` (task 11.10) it
    /// is passed in rather than read from `$HOME` here, so the model stays pure.
    pub home: Option<PathBuf>,
}

/// One of the places the cursor theme and size are duplicated (R3.4), named so the
/// Theme page can say *which* copies disagree with the value it shows (see
/// [`ThemesModel::disagreeing_cursor_copies`]).
///
/// The copies are expected to hold the identical value — the app writes every one of
/// them on each cursor change — but other tools (an old `nwg-look` run, a hand edit, a
/// fresh checkout of the dotfiles) can leave them out of step, and since different
/// programs read different copies the result is a cursor that changes depending on
/// which program it hovers over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorCopy {
    /// `gtk-cursor-theme-name`/`-size` in `~/.config/gtk-3.0/settings.ini`.
    Gtk3Settings,
    /// `gtk-cursor-theme-name`/`-size` in `~/.config/gtk-4.0/settings.ini`.
    Gtk4Settings,
    /// `XCURSOR_THEME`/`XCURSOR_SIZE` in `~/.config/uwsm/env`.
    UwsmEnv,
    /// The two `hl.env` cursor lines in `~/.config/hypr/hyprland.lua`.
    HyprlandLua,
    /// `Inherits=` in `~/.icons/default/index.theme` (the theme only; the file has no
    /// size).
    CursorFallbackTheme,
}

impl CursorCopy {
    /// The short, user-facing name of the copy, as the Theme page lists it.
    pub fn label(self) -> &'static str {
        match self {
            CursorCopy::Gtk3Settings => "gtk-3.0/settings.ini",
            CursorCopy::Gtk4Settings => "gtk-4.0/settings.ini",
            CursorCopy::UwsmEnv => "uwsm/env",
            CursorCopy::HyprlandLua => "hyprland.lua",
            CursorCopy::CursorFallbackTheme => "~/.icons/default/index.theme",
        }
    }
}

/// Where an active `GTK_THEME` override was found, so the UI can name it in the banner
/// (R3.3).
///
/// Not to be confused with [`GtkThemeOverride`], the parser's view of the one
/// `GTK_THEME` line in `uwsm/env` (active, commented out, or absent). This type is the
/// resolved answer across both places an override can come from — the app's own
/// environment and `uwsm/env` — and exists only when an override is in effect.
///
/// A set `GTK_THEME` overrides GTK's theme choice entirely, so the app must never
/// fight it: whenever this is `Some`, the Theme page shows a banner and disables the
/// GTK-theme drop-down. The icon and cursor drop-downs stay enabled — `GTK_THEME`
/// overrides only the GTK theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GtkOverrideOrigin {
    /// `GTK_THEME` is set in the app's own process environment — the copy that
    /// actually overrides *this* app's theme. It would come from whatever started the
    /// app, e.g. a wrapper script or a shell export; the dotfiles set it nowhere today
    /// (analysis §7.7), so on the target this variant is not expected.
    AppEnvironment(String),
    /// `GTK_THEME` is uncommented (active) in `uwsm/env`, so it overrides the theme for
    /// the session's apps (analysis §6.3).
    UwsmEnv(String),
}

impl GtkOverrideOrigin {
    /// The override value (the theme name `GTK_THEME` is set to).
    fn value(&self) -> &str {
        match self {
            GtkOverrideOrigin::AppEnvironment(value) | GtkOverrideOrigin::UwsmEnv(value) => value,
        }
    }

    /// A human-readable banner message naming the override and where it comes from, so
    /// the user understands why the GTK-theme drop-down is disabled (R3.3).
    pub fn banner_message(&self) -> String {
        let origin = match self {
            GtkOverrideOrigin::AppEnvironment(_) => "the GTK_THEME environment variable",
            GtkOverrideOrigin::UwsmEnv(_) => "GTK_THEME in uwsm/env",
        };
        format!(
            "The GTK theme is forced to \u{201c}{}\u{201d} by {origin}, so it cannot be changed \
             here. Unset it to choose a theme.",
            self.value()
        )
    }
}

/// One of the four theme values the Theme page stages, used to record a per-value render
/// outcome without repeating the four names at every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeValue {
    /// The GTK theme name (`settings.ini` only).
    GtkTheme,
    /// The icon theme name (`settings.ini` only).
    IconTheme,
    /// The cursor theme name (duplicated across five files, R3.4).
    CursorTheme,
    /// The cursor size in pixels (duplicated across four files, R3.4 — the XCursor
    /// fallback theme carries no size).
    CursorSize,
}

impl ThemeValue {
    /// The [`SettingId`] this value is validated as, at stage time and in the Apply
    /// pipeline's step 1 (task 9.27).
    fn setting_id(self) -> SettingId {
        match self {
            ThemeValue::GtkTheme => SettingId::GtkTheme,
            ThemeValue::IconTheme => SettingId::IconTheme,
            ThemeValue::CursorTheme => SettingId::CursorTheme,
            ThemeValue::CursorSize => SettingId::CursorSize,
        }
    }

    /// The value's user-facing name at the start of a sentence, as a
    /// [`SkippedWrite`] names it.
    fn label(self) -> &'static str {
        match self {
            ThemeValue::GtkTheme => "GTK theme",
            ThemeValue::IconTheme => "Icon theme",
            ThemeValue::CursorTheme => "Cursor theme",
            ThemeValue::CursorSize => "Cursor size",
        }
    }
}

/// A flag per theme value, used for the per-value render outcomes tracked in
/// [`ThemeRenderRecord`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ThemeValueFlags {
    /// The flag for the GTK theme.
    gtk_theme: bool,
    /// The flag for the icon theme.
    icon_theme: bool,
    /// The flag for the cursor theme.
    cursor_theme: bool,
    /// The flag for the cursor size.
    cursor_size: bool,
}

impl ThemeValueFlags {
    /// A mutable handle on one value's flag, so a render can flag the value it is
    /// currently writing without a four-way `match` at each call site.
    fn flag(&mut self, value: ThemeValue) -> &mut bool {
        match value {
            ThemeValue::GtkTheme => &mut self.gtk_theme,
            ThemeValue::IconTheme => &mut self.icon_theme,
            ThemeValue::CursorTheme => &mut self.cursor_theme,
            ThemeValue::CursorSize => &mut self.cursor_size,
        }
    }

    /// Folds another set in (per-value logical OR).
    fn merge(&mut self, other: ThemeValueFlags) {
        self.gtk_theme |= other.gtk_theme;
        self.icon_theme |= other.icon_theme;
        self.cursor_theme |= other.cursor_theme;
        self.cursor_size |= other.cursor_size;
    }

    /// The values flagged here but *not* in `other` (per-value logical AND NOT).
    fn without(self, other: ThemeValueFlags) -> ThemeValueFlags {
        ThemeValueFlags {
            gtk_theme: self.gtk_theme && !other.gtk_theme,
            icon_theme: self.icon_theme && !other.icon_theme,
            cursor_theme: self.cursor_theme && !other.cursor_theme,
            cursor_size: self.cursor_size && !other.cursor_size,
        }
    }
}

/// What rendering the staged theme edits achieved for each of the four values, so
/// [`ThemesModel::commit`] promotes only the ones that truly landed.
///
/// A surgical parser edit can fail for a single key on its own — the value contains a
/// character that would break the file, or the addressed line/section does not exist — in
/// which case that key is skipped and logged while the remaining keys are still written.
/// Promoting a skipped value would leave the model claiming something the config does not
/// hold, and since it would no longer look changed, no later Apply would write it.
///
/// The two flag sets distinguish the two ways a copy can be skipped, which need opposite
/// treatment:
///
/// - **absent copy** — the file simply does not carry this value (the real case is a
///   `hyprland.lua` without an `hl.env("XCURSOR_*", …)` line: the Lua editor rewrites
///   such calls but never *adds* one, since that file is code the app does not author,
///   see [`ThemesModel::render_hyprland_lua`]). Nothing on disk then disagrees with the
///   value, so this must not block promotion — otherwise the cursor would stay dirty
///   forever on such a host, with every later Apply pointlessly rewriting the other
///   copies.
/// - **refused copy** — the file carries (or may carry) the value but the writer would
///   not put the new one there (e.g. the Lua writer rejects a `"` or `\`, which would
///   end or escape the string literal, while the `settings.ini` and `uwsm/env` writers
///   accept them; or `hyprland.lua`'s structure cannot be followed, so its lines cannot
///   be located safely). This *does* block promotion: the copies on disk now differ, or
///   may, and R3.4 requires every copy to hold the identical value, so the page must stay
///   dirty rather than look applied while one copy lags behind. A refusal over a
///   *character* no longer happens in normal use — the staging methods refuse such a
///   value up front and the Apply pipeline re-checks it (task 9.27) — so for that case
///   this record is defence in depth; a structure the Lua editor cannot follow remains
///   the everyday reason for a refused copy.
///
/// Every refusal is also kept as a [`SkippedWrite`] naming the value, the file and the
/// parser's reason (task 9.28), so the Apply can tell the user what was left out and
/// the page can say why it stays dirty. The two are recorded together by
/// [`Self::refuse`], which is what keeps the invariant that a value is listed in
/// `skipped` exactly when it is flagged `diverged` — the note on the page then explains
/// precisely the values that will not be promoted, no more and no fewer. An absent copy
/// is not listed, for the reason given above.
///
/// The record also says which values reached a `settings.ini` file in particular, since
/// that, not "some file", decides the live `gsettings` push — see
/// [`ThemesModel::reload_params`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ThemeRenderRecord {
    /// Values that reached at least one of the files carrying them.
    written: ThemeValueFlags,
    /// Values that reached at least one of the two `settings.ini` files — a subset of
    /// [`Self::written`].
    in_gtk_settings: ThemeValueFlags,
    /// Values that a file which *does* carry them refused, so the copies on disk diverge.
    diverged: ThemeValueFlags,
    /// One entry per refusal flagged in [`Self::diverged`], in render order.
    skipped: Vec<SkippedWrite>,
}

impl ThemeRenderRecord {
    /// Folds one file's render result into the page-wide record.
    fn merge(&mut self, other: ThemeRenderRecord) {
        self.written.merge(other.written);
        self.in_gtk_settings.merge(other.in_gtk_settings);
        self.diverged.merge(other.diverged);
        self.skipped.extend(other.skipped);
    }

    /// Records that `file`, which carries `value`, refused the staged value for
    /// `reason` (the parser's explanation): the copies on disk now diverge, so the value
    /// must not be promoted, and the user is told which file and why.
    fn refuse(&mut self, value: ThemeValue, file: &'static str, reason: String) {
        *self.diverged.flag(value) = true;
        self.skipped.push(SkippedWrite {
            value: value.label(),
            file,
            reason,
        });
    }

    /// The values [`ThemesModel::commit`] may promote: written somewhere, refused nowhere.
    fn promotable(&self) -> ThemeValueFlags {
        self.written.without(self.diverged)
    }
}

/// The GTK/icon/cursor theme staging model (task 6.4).
///
/// Built by [`ThemesModel::load`] from the scanned [`ThemeRoots`] and the backing
/// [`ThemesPaths`]. It owns the four drop-downs' [`Selection`]s, the backing config
/// texts, the `GTK_THEME` override state, and a freshness baseline for conflict
/// detection (R5.6). Its file edits reach the shared Apply pipeline through
/// [`Self::apply_contribution`]; the window folds them into the same
/// [`apply::run`](crate::core::apply::run) it drives for the store and Display model.
///
/// It stays GTK-free so scanning, staging, the multi-file write, the override
/// decision, and the live-restyle gating are all unit-tested headlessly (R6.2); the
/// layering guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
pub struct ThemesModel {
    /// The GTK theme drop-down.
    gtk_theme: Selection,
    /// The GTK theme names the scan found a GTK 3 stylesheet for, on any root
    /// ([`GtkThemeScan::with_gtk3_stylesheet`]). Decides
    /// [`Self::one_sided_gtk_theme_note`] (task 9.29); [`Self::reload`] scans again,
    /// so it follows the roots on a conflict reload.
    gtk_themes_with_gtk3_stylesheet: BTreeSet<String>,
    /// The icon theme drop-down.
    icon_theme: Selection,
    /// The cursor theme drop-down.
    cursor_theme: Selection,
    /// The cursor size drop-down (values held as strings; parsed when written).
    cursor_size: Selection,
    /// The backing config files this model edits, with their freshness baseline
    /// (R5.6). Reached through the per-file accessors [`Self::gtk3`], [`Self::gtk4`],
    /// [`Self::hyprland_lua`], [`Self::uwsm`] and [`Self::cursor_fallback`], which
    /// document what each one is for.
    backing: BackingSet,
    /// Whether `~/.icons/default/index.theme` did not exist at load (not merely
    /// unreadable), so a cursor theme change *creates* it instead of skipping that copy
    /// (analysis §8.4). Kept separately from [`Self::backing`], which only holds files
    /// that could be read, so the freshness check can also notice the file appearing
    /// before the Apply (see [`Self::check_conflict`]). It stays `true` after a
    /// committed creation; that is harmless, because the commit adds the created file
    /// to [`Self::backing`], and every use checks that entry first.
    cursor_fallback_absent: bool,
    /// The active `GTK_THEME` override (app environment preferred over `uwsm/env`), or
    /// `None`. When `Some`, the GTK-theme drop-down is disabled and a banner shown
    /// (R3.3).
    gtk_override: Option<GtkOverrideOrigin>,
    /// Whether a live theme-restyle path (settings portal or dconf) is available, so
    /// the UI may claim a live GTK-theme restyle rather than "next launch" (R2.2).
    live_restyle: bool,
    /// The theme roots, kept so [`Self::reload`] can scan again on a conflict reload.
    roots: ThemeRoots,
    /// The backing paths, kept so [`Self::reload`] can re-read the files.
    paths: ThemesPaths,
    /// The app-environment `GTK_THEME` value, kept so [`Self::reload`] re-derives the
    /// override (the app's own environment does not change during the session, but the
    /// override is recomputed with the freshly re-read `uwsm/env`).
    app_env_gtk_theme: Option<String>,
    /// The `settings.ini` files that are plain files although the dotfiles repo has a
    /// copy of them, found at load (task 11.12; see [`Self::unlinked_gtk_settings`]).
    unlinked_gtk_settings: Vec<UnlinkedSettingsIni>,
    /// Whether detection found `xsettingsd` running — the trace of an external
    /// GTK-settings writer (task 11.12; see [`Self::with_xsettingsd_running`]).
    xsettingsd_running: bool,
    /// What detection found of the notifd notification daemon, which decides whether
    /// an icon-theme change reaches its notifications live (task 11.14; see
    /// [`Self::with_notifd_status`]).
    notifd: NotifdStatus,
}

/// A live `settings.ini` that is a plain file while the dotfiles repo carries its own
/// copy, so the two have come apart (task 11.12, analysis §7.7).
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnlinkedSettingsIni {
    /// The live XDG path — still the file the app writes, because it is what GTK reads.
    live: PathBuf,
    /// The repo copy the live path used to link to, which the app's writes no longer
    /// reach.
    repo_copy: PathBuf,
}

/// The Theme page's GTK/icon/cursor contribution to an
/// [`ApplyPlan`](crate::core::apply::ApplyPlan): the file writes plus the reload
/// parameters (task 6.4).
///
/// A cursor change contributes writes for *every* file carrying the value (both
/// `settings.ini`, `uwsm/env`, the `hl.env` lines of `hyprland.lua`, and — for the theme
/// — `~/.icons/default/index.theme`) with the identical value, so they never desync
/// (R3.4); a GTK/icon theme change contributes only the two `settings.ini` writes, plus,
/// for a GTK theme, the GTK 4 theme link steps (task 11.16). The reload parameters carry
/// only the values that reached a `settings.ini` file, so the reload table (task 4.4)
/// emits `gsettings set` / `hyprctl setcursor` only for those (task 9.28).
pub struct ThemesApply {
    /// The atomic writes, one per changed backing file. Empty when every staged value
    /// was refused by the files that carry it — the contribution then still exists, to
    /// report [`Self::skipped`] (task 9.28).
    pub writes: Vec<FileWrite>,
    /// The GTK 4 theme link steps, one per link that must change; empty unless the GTK
    /// theme changed (task 11.16). The pipeline runs them after every file write — see
    /// [`LinkWrite`] — and [`ThemesModel::gtk4_links_note`] explains on the page what
    /// they do not cover.
    pub links: Vec<LinkWrite>,
    /// The reload parameters for the changed theme/cursor values that reached a
    /// `settings.ini` file (the pipeline merges these into its plan-wide
    /// [`ReloadParams`]). A staged value no `settings.ini` took is left out, so it is
    /// never pushed into dconf while the files still hold the old value — see
    /// [`ThemesModel::reload_params`] for the rule (task 9.28).
    pub reload_params: ReloadParams,
    /// Every staged theme value, as the [`SettingId`] it is validated by, for the
    /// pipeline's step 1 to re-check before any write (task 9.27, R8.3).
    ///
    /// The staging methods already refused a value any of its writers would reject, so
    /// in normal use these pass; the re-check is what keeps that guarantee from resting
    /// on every path into the staged state going through those methods. Each write
    /// declares the settings it carries ([`FileWrite::validated_by`]), which the
    /// pipeline's plan-drift guard checks against this list.
    pub validations: Vec<(SettingId, Value)>,
    /// The staged values a file that carries them refused, each naming the value, the
    /// file and the reason (task 9.28; R3.4, R5.5). These values stay pending after the
    /// Apply; the assembler collects the list so the UI can say so. A copy that is
    /// merely absent is not listed (see [`SkippedWrite`]).
    pub skipped: Vec<SkippedWrite>,
}

impl ThemesModel {
    /// Builds the model by scanning for installed themes and reading the backing config
    /// (task 6.4; R3.3, R3.4, R4.4, R2.2).
    ///
    /// `roots` and `paths` are injected (see their docs) so this is exercised against a
    /// fixture tree in tests; `settings_portal_available` gates the live-restyle claim
    /// (R2.2); `app_env_gtk_theme` is the app's own `GTK_THEME` environment value
    /// (`std::env::var("GTK_THEME").ok()`), the copy that overrides *this* app's theme.
    /// Nothing here fails: an unreadable config simply yields no backing text for that
    /// file (its controls degrade — a settings.ini that cannot be read hides the theme
    /// rows via [`Self::themes_editable`], R4.4). The current values are read from
    /// whichever `settings.ini` is present; the cursor theme and size fall back, in
    /// order, to `uwsm/env`, to `hyprland.lua`'s `hl.env` lines, and (the theme only) to
    /// the XCursor fallback theme's `Inherits=`. A `hyprland.lua` without those lines, or
    /// one whose structure the Lua editor cannot follow, simply contributes no value.
    /// The order deliberately starts at `settings.ini`, not at the canonical `uwsm/env`:
    /// `settings.ini` is what GTK (and so this app) actually reads, it is the file whose
    /// absence hides the theme rows ([`Self::themes_editable`]), and any difference
    /// from the canonical copy is surfaced by [`Self::disagreeing_cursor_copies`] — so
    /// do not reorder this to read `uwsm/env` first.
    pub fn load(
        roots: &ThemeRoots,
        paths: ThemesPaths,
        settings_portal_available: bool,
        app_env_gtk_theme: Option<String>,
    ) -> ThemesModel {
        let backing_paths: Vec<&Path> = [
            Some(paths.gtk3_settings.as_path()),
            Some(paths.gtk4_settings.as_path()),
            paths.hyprland_lua.as_deref(),
            Some(paths.uwsm_env.as_path()),
            paths.cursor_fallback_theme.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let backing = BackingSet::load(&backing_paths);
        let cursor_fallback_absent = paths
            .cursor_fallback_theme
            .as_deref()
            .is_some_and(|path| backing.get(path).is_none() && is_absent(path));

        // Read current values from whichever settings.ini is present (prefer gtk-3.0),
        // since both carry the same keys. The cursor theme/size fall back to the other
        // copies when settings.ini did not set them (see the method docs).
        let copies = CursorCopies::read(&backing, &paths);
        let settings_ini = copies.gtk3.as_ref().or(copies.gtk4.as_ref());

        let current_gtk = settings_value(settings_ini, KEY_GTK_THEME);
        let current_icon = settings_value(settings_ini, KEY_ICON_THEME);
        let current_cursor = settings_value(settings_ini, KEY_CURSOR_THEME)
            .or_else(|| env_value(copies.uwsm.as_ref(), ENV_CURSOR_THEME))
            .or_else(|| lua_env_value(copies.hyprland_lua.as_ref(), ENV_CURSOR_THEME))
            .or_else(|| fallback_inherits(copies.cursor_fallback.as_ref()));
        let current_size = settings_value(settings_ini, KEY_CURSOR_SIZE)
            .or_else(|| env_value(copies.uwsm.as_ref(), ENV_CURSOR_SIZE))
            .or_else(|| lua_env_value(copies.hyprland_lua.as_ref(), ENV_CURSOR_SIZE));

        let gtk_scan = scan_gtk_themes(&roots.gtk_theme_dirs);
        let (icon_themes, cursor_themes) = scan_icon_and_cursor_themes(&roots.icon_dirs);

        // The app's own environment takes precedence: a GTK_THEME in it overrides this
        // very app, regardless of what uwsm/env says (R3.3).
        let uwsm_override = copies.uwsm.as_ref().map(EnvFile::gtk_theme_override);
        let gtk_override = resolve_gtk_override(app_env_gtk_theme.clone(), uwsm_override.as_ref());

        let curated_sizes: Vec<String> = CURATED_CURSOR_SIZES
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        let unlinked_gtk_settings = detect_unlinked_gtk_settings(&paths);

        tracing::info!(
            gtk_themes = gtk_scan.offered.len(),
            icon_themes = icon_themes.len(),
            cursor_themes = cursor_themes.len(),
            gtk_override = gtk_override.is_some(),
            live_restyle = settings_portal_available,
            "loaded GTK/icon/cursor themes for the Theme page (task 6.4, R3.3/R3.4)"
        );

        ThemesModel {
            gtk_theme: Selection::new(gtk_scan.offered, current_gtk),
            gtk_themes_with_gtk3_stylesheet: gtk_scan.with_gtk3_stylesheet,
            icon_theme: Selection::new(icon_themes, current_icon),
            cursor_theme: Selection::new(cursor_themes, current_cursor),
            cursor_size: Selection::new(curated_sizes, current_size),
            backing,
            cursor_fallback_absent,
            gtk_override,
            live_restyle: settings_portal_available,
            roots: roots.clone(),
            paths,
            app_env_gtk_theme,
            unlinked_gtk_settings,
            xsettingsd_running: false,
            notifd: NotifdStatus::Absent,
        }
    }

    /// Returns `self` recording whether detection found `xsettingsd` running
    /// ([`Daemon::Xsettingsd`](crate::core::detect::Daemon::Xsettingsd)), which turns on
    /// [`Self::external_gtk_writer_note`] (task 11.12).
    ///
    /// A builder rather than a [`Self::load`] argument because it only feeds a note: it
    /// changes nothing the model reads or writes. [`Self::reload`] carries it over.
    pub fn with_xsettingsd_running(mut self, running: bool) -> ThemesModel {
        self.xsettingsd_running = running;
        self
    }

    /// Returns `self` recording the notifd status detection found
    /// ([`Capabilities::notifd`](crate::core::detect::Capabilities::notifd)), which
    /// decides [`Self::notification_icons_note`] (task 11.14).
    ///
    /// A builder like [`Self::with_xsettingsd_running`], for the same reason: it only
    /// feeds a note. The `Reload()` call itself is planned by the reload table, gated
    /// on the same status. [`Self::reload`] carries it over. Without it the model
    /// assumes notifd is absent and shows no note.
    pub fn with_notifd_status(mut self, notifd: NotifdStatus) -> ThemesModel {
        self.notifd = notifd;
        self
    }

    /// The note shown under the icon-theme control while a running notifd is too old
    /// to reload its icon theme, or `None` otherwise (task 11.14; analysis §8.2).
    ///
    /// notifd looks notification icons up in the theme named by
    /// `gtk-3.0/settings.ini`, which it reads at start and — from version 1.1 on — on
    /// its `Reload()` D-Bus method, which the Apply calls after an icon-theme change.
    /// A running daemon below 1.1, or one that reported no version (treated as
    /// older), has no such method, so its notifications keep the old icons until it
    /// next starts; the app deliberately does not restart it, because a restart drops
    /// the popups on screen. A stopped or absent daemon needs no note: it reads the
    /// new theme when it starts.
    pub fn notification_icons_note(&self) -> Option<String> {
        let too_old = matches!(self.notifd, NotifdStatus::Running { .. })
            && !self.notifd.supports_state_and_reload();
        too_old.then(|| {
            "Notification icons follow an icon-theme change the next time the \
             notification daemon (notifd) starts: this notifd version cannot reload \
             its icon theme while running."
                .to_string()
        })
    }

    /// The live `settings.ini` paths that are plain files although the dotfiles repo
    /// ([`ThemesPaths::dotfiles_repo`]) still carries their copy under
    /// `config/gtk-N.0/settings.ini`, in GTK 3, GTK 4 order (task 11.12; R5.6, R8.5).
    ///
    /// The dotfiles deploy both files as symlinks into the repo, so an edit made through
    /// the app reaches the repo. A tool that writes the file by replacing it — `nwg-look`
    /// did exactly that to `gtk-4.0/settings.ini` on the target machine (analysis §7.7) —
    /// leaves a plain file behind, and from then on the live file and the repo copy drift
    /// apart. The app keeps writing the live file (it is what GTK reads) but says so on
    /// the Theme page ([`Self::unlinked_gtk_settings_notes`]).
    ///
    /// Not reported: a file that is a symlink (wherever it points), a file that is
    /// missing, any file when there is no repo (a plain file is then the normal,
    /// non-dotfiles setup), a file whose repo copy does not exist, and a plain file
    /// that *is* the repo copy, reached through a symlinked parent directory. Determined
    /// once per load, so a conflict reload or a refresh re-checks it.
    ///
    /// Deliberately limited to the two `settings.ini` files. The other repo-linked file
    /// the Theme page writes, `~/.icons/default/index.theme`, is not checked: the app
    /// itself creates that file as a plain file when it is absent (task 11.5), so a plain
    /// file there is not evidence that another tool replaced a link.
    pub fn unlinked_gtk_settings(&self) -> Vec<&Path> {
        self.unlinked_gtk_settings
            .iter()
            .map(|unlinked| unlinked.live.as_path())
            .collect()
    }

    /// One Theme-page note per entry of [`Self::unlinked_gtk_settings`], naming the live
    /// file and the repo copy the app's changes will no longer reach (task 11.12). Paths
    /// under [`ThemesPaths::home`] are shown as `~/…`, any other path absolute.
    pub fn unlinked_gtk_settings_notes(&self) -> Vec<String> {
        self.unlinked_gtk_settings
            .iter()
            .map(|unlinked| {
                format!(
                    "{} is no longer linked to the dotfiles; changes here will not reach \
                     the repo copy at {}.",
                    self.shown_path(&unlinked.live),
                    self.shown_path(&unlinked.repo_copy)
                )
            })
            .collect()
    }

    /// The Theme-page note shown while `xsettingsd` runs, or `None` when it does not
    /// (task 11.12).
    ///
    /// `xsettingsd` is started by KDE's kded6 `gtkconfig` module, which rewrites
    /// `settings.ini` and the GTK gsettings keys from KDE's own settings; the dotfiles
    /// stop both at Hyprland start (`scripts/kded-gtkconfig off`), so a running
    /// `xsettingsd` means that suppression did not hold — for example after a Plasma
    /// login (analysis §7.7). The app still writes as usual; the R5.6 freshness check
    /// catches an external rewrite before it would clobber one. Informational only.
    pub fn external_gtk_writer_note(&self) -> Option<String> {
        self.xsettingsd_running.then(|| {
            "xsettingsd is running, so another program (usually KDE's GTK settings \
             module) is managing GTK settings and may overwrite the theme and cursor \
             changes made here."
                .to_string()
        })
    }

    /// `gtk-3.0/settings.ini`'s current text, or `None` when it was unreadable (R4.4).
    fn gtk3(&self) -> Option<&BackingText> {
        self.backing.get(&self.paths.gtk3_settings)
    }

    /// `gtk-4.0/settings.ini`'s current text, or `None` when it was unreadable (R4.4).
    fn gtk4(&self) -> Option<&BackingText> {
        self.backing.get(&self.paths.gtk4_settings)
    }

    /// `hyprland.lua`'s current text, or `None` when it was unreadable or the
    /// Hyprland-side cursor write is guarded off (task 11.2) — only its two cursor
    /// `hl.env` lines are ever edited.
    fn hyprland_lua(&self) -> Option<&BackingText> {
        self.paths
            .hyprland_lua
            .as_deref()
            .and_then(|path| self.backing.get(path))
    }

    /// `~/.icons/default/index.theme`'s current text, or `None` when it was absent,
    /// unreadable, or the home directory is unknown — see
    /// [`Self::cursor_fallback_absent`] for the case in which the file is created.
    fn cursor_fallback(&self) -> Option<&BackingText> {
        self.paths
            .cursor_fallback_theme
            .as_deref()
            .and_then(|path| self.backing.get(path))
    }

    /// `uwsm/env`'s current text, or `None` when it was unreadable — the canonical cursor
    /// env copy, and the source of the uwsm `GTK_THEME` override reading (R3.3).
    fn uwsm(&self) -> Option<&BackingText> {
        self.backing.get(&self.paths.uwsm_env)
    }

    /// Whether the theme rows should be shown: at least one `settings.ini` was readable
    /// (R4.4).
    ///
    /// The GTK/icon/cursor values are read from — and written to — `settings.ini`, so
    /// when neither GTK 3 nor GTK 4 file can be read there is nothing to preselect or
    /// edit and the rows are hidden (the page shows a note instead), matching the
    /// Display page's "hide the file-backed controls when the config is unreadable"
    /// rule.
    pub fn themes_editable(&self) -> bool {
        self.gtk3().is_some() || self.gtk4().is_some()
    }

    /// The GTK theme drop-down options (installed GTK themes plus the current value).
    pub fn gtk_themes(&self) -> &[String] {
        &self.gtk_theme.options
    }

    /// The icon theme drop-down options.
    pub fn icon_themes(&self) -> &[String] {
        &self.icon_theme.options
    }

    /// The cursor theme drop-down options.
    pub fn cursor_themes(&self) -> &[String] {
        &self.cursor_theme.options
    }

    /// The cursor size drop-down options (curated sizes plus the current value).
    pub fn cursor_sizes(&self) -> &[String] {
        &self.cursor_size.options
    }

    /// The preselected index of the GTK theme drop-down.
    pub fn selected_gtk_index(&self) -> Option<usize> {
        self.gtk_theme.selected_index()
    }

    /// The preselected index of the icon theme drop-down.
    pub fn selected_icon_index(&self) -> Option<usize> {
        self.icon_theme.selected_index()
    }

    /// The preselected index of the cursor theme drop-down.
    pub fn selected_cursor_index(&self) -> Option<usize> {
        self.cursor_theme.selected_index()
    }

    /// The preselected index of the cursor size drop-down.
    pub fn selected_cursor_size_index(&self) -> Option<usize> {
        self.cursor_size.selected_index()
    }

    /// The active `GTK_THEME` override, or `None` (R3.3). When `Some`, the GTK-theme
    /// drop-down is disabled and a banner shown.
    pub fn gtk_override(&self) -> Option<&GtkOverrideOrigin> {
        self.gtk_override.as_ref()
    }

    /// Whether the GTK-theme drop-down must be disabled — a live `GTK_THEME` override
    /// is in force, which the app must not fight (R3.3).
    pub fn gtk_dropdown_disabled(&self) -> bool {
        self.gtk_override.is_some()
    }

    /// Whether a live GTK-theme restyle can be claimed (a settings portal or dconf
    /// backend is available); otherwise a change takes effect at the next launch
    /// (R2.2).
    pub fn live_restyle(&self) -> bool {
        self.live_restyle
    }

    /// The note shown beside the live-restyle note while the GTK theme the page shows
    /// (the staged one, or the current one) works on GTK 4 but not on GTK 3, or `None`
    /// otherwise (task 9.29; R3.3).
    ///
    /// That happens for a built-in name (`BUILTIN_GTK_THEMES`) GTK 3 does not have
    /// compiled in (`Gtk3Resolution::NeedsDirectory` — today `Adwaita-dark`) when the
    /// theme scan found no `<name>` directory carrying a GTK 3 stylesheet (a `gtk.css`
    /// inside a `gtk-3.<N>/` directory, task 9.30) under any theme root: GTK 4 apps then
    /// use the name's built-in look while GTK 3 apps fall back to a different one,
    /// although both `settings.ini` files hold the same name. The app writes the name all the
    /// same — it is a valid choice, and making the copies differ to compensate would
    /// break R3.3's "identical everywhere" — so this is information, never a refusal:
    /// it changes neither [`Self::stage_gtk_theme`] nor the Apply contribution.
    ///
    /// Decided from the scan results held by the model rather than by asking GTK how it
    /// resolves the name, so the check stays headless (R6.2) and costs nothing per
    /// rebuild. A built-in that GTK 3 also has compiled in (`Adwaita`) never gets the
    /// note, nor does a name that is not a built-in (an installed theme is looked up by
    /// both GTK versions in the same theme folders). Nothing is said while a
    /// `GTK_THEME` override is in force: GTK then ignores the `settings.ini` name for
    /// every app the override reaches, and the override banner already says so.
    pub fn one_sided_gtk_theme_note(&self) -> Option<String> {
        if self.gtk_override.is_some() {
            return None;
        }
        let theme = self.gtk_theme.effective()?;
        let Gtk3Resolution::NeedsDirectory { fallback_look } = builtin_gtk_theme(theme)?.gtk3
        else {
            return None;
        };
        if self.gtk_themes_with_gtk3_stylesheet.contains(theme) {
            return None;
        }
        Some(format!(
            "\u{201c}{theme}\u{201d} is built into GTK 4 only; without an \u{201c}{theme}\u{201d} \
             theme directory, GTK 3 apps will use {fallback_look}."
        ))
    }

    /// Stages a GTK theme switch (ignored when a `GTK_THEME` override is in force).
    ///
    /// Refused, with nothing staged, when one of the places the name is written cannot
    /// hold it: both `settings.ini` files refuse a line break, and the GTK 4 theme link
    /// step needs one plain directory name (task 9.27; the rules are listed in
    /// [`crate::core::value_writers`]).
    pub fn stage_gtk_theme(&mut self, name: &str) -> Result<(), ValidationError> {
        if self.gtk_dropdown_disabled() {
            // The drop-down is disabled in the UI, so this is a defensive guard against
            // an out-of-band caller: never stage a GTK theme the override would fight.
            tracing::debug!(
                "ignoring a GTK theme edit while a GTK_THEME override is active (R3.3)"
            );
            return Ok(());
        }
        Self::stage_checked(&mut self.gtk_theme, ThemeValue::GtkTheme, name)
    }

    /// Stages an icon theme switch, refused when `settings.ini` could not hold the name
    /// (a line break; task 9.27).
    pub fn stage_icon_theme(&mut self, name: &str) -> Result<(), ValidationError> {
        Self::stage_checked(&mut self.icon_theme, ThemeValue::IconTheme, name)
    }

    /// Stages a cursor theme switch, refused when one of its five copies could not hold
    /// the name — in practice the `hl.env` line of `hyprland.lua`, whose Lua string
    /// refuses `"`, `\` and every control character, the strictest of the rules (task
    /// 9.27). A `#` is accepted: no current copy refuses it.
    pub fn stage_cursor_theme(&mut self, name: &str) -> Result<(), ValidationError> {
        Self::stage_checked(&mut self.cursor_theme, ThemeValue::CursorTheme, name)
    }

    /// Stages a cursor size switch (the value is a pixel size as a string), refused when
    /// it is not a whole number of pixels greater than zero (task 9.27).
    pub fn stage_cursor_size(&mut self, size: &str) -> Result<(), ValidationError> {
        Self::stage_checked(&mut self.cursor_size, ThemeValue::CursorSize, size)
    }

    /// Stages `value` into `selection` once it passes `which`'s [`SettingId`] validator
    /// (R8.3, task 9.27), returning the refusal — whose message names the offending
    /// character and the files that cannot hold it — and staging nothing otherwise.
    ///
    /// Why the check happens here, where the user picks the value: the writers of one
    /// value disagree on what it may contain (see [`crate::core::value_writers`]), so a
    /// name only some of them accept would reach some copies and not others. Task 9.6
    /// keeps the model from then claiming it as applied, but the user would be left with
    /// a page that never stops being dirty and no reason given; refusing the choice up
    /// front tells them why.
    ///
    /// The theme scan deliberately does **not** filter such names out: the drop-down
    /// lists every installed theme, and picking one that cannot be written explains
    /// itself, whereas a filtered theme would simply be missing from the list with the
    /// reason only in the journal. (The value configured on disk is always listed
    /// anyway — [`Selection::new`] keeps it selectable — so a filter could not keep
    /// every unwritable name out of the list in any case.)
    ///
    /// Re-picking the value the config already holds is a reset, not an edit, and is
    /// never refused: it writes nothing, and refusing it would leave a pending edit the
    /// user is trying to undo.
    fn stage_checked(
        selection: &mut Selection,
        which: ThemeValue,
        value: &str,
    ) -> Result<(), ValidationError> {
        if !selection.is_original(value) {
            which
                .setting_id()
                .validate(&Value::Enum(value.to_string()))?;
        }
        selection.stage(value);
        Ok(())
    }

    /// Stages `value` for the theme setting `id` **without** the stage-time check, so a
    /// test can put the model into a state the real staging methods refuse (task 9.27).
    ///
    /// The seam that proves the Apply pipeline's step 1 still catches such a value before
    /// any writer sees it, and that the writers' own guards (task 9.6) still hold when it
    /// gets past step 1 in a unit test. Compiled only for tests and the `testing`
    /// feature. Panics on a `SettingId` that is not one of the four theme values, so a
    /// test addressing the wrong setting fails loudly.
    #[cfg(any(test, feature = "testing"))]
    pub fn stage_unchecked_for_tests(&mut self, id: SettingId, value: &str) {
        let selection = match id {
            SettingId::GtkTheme => &mut self.gtk_theme,
            SettingId::IconTheme => &mut self.icon_theme,
            SettingId::CursorTheme => &mut self.cursor_theme,
            SettingId::CursorSize => &mut self.cursor_size,
            other => panic!("{other:?} is not a theme setting of this model"),
        };
        selection.stage(value);
    }

    /// Whether any theme/cursor value has a pending change — the page's dirty state,
    /// which the window folds into the global Apply/Reset chrome (R5.1).
    pub fn is_dirty(&self) -> bool {
        self.gtk_theme.is_dirty()
            || self.icon_theme.is_dirty()
            || self.cursor_theme.is_dirty()
            || self.cursor_size.is_dirty()
    }

    /// Discards every staged theme/cursor change (R5.1).
    pub fn reset(&mut self) {
        self.gtk_theme.reset();
        self.icon_theme.reset();
        self.cursor_theme.reset();
        self.cursor_size.reset();
    }

    /// Whether any of the backing files changed on disk since it was loaded, so a
    /// dirty theme change must be reloaded instead of written (R5.6) — see
    /// [`BackingSet::check_conflict`] for the discipline this is part of.
    ///
    /// The one file the model may *create*, `~/.icons/default/index.theme`, is checked
    /// too: if it was absent at load and something now exists at its path, another
    /// program created it, and the staged creation was rendered from a state that no
    /// longer holds. (The writer's exclusive create would refuse it anyway; reporting it
    /// here turns that into the ordinary "reload and retry" conflict instead of a failed
    /// Apply.)
    pub fn check_conflict(&self) -> bool {
        self.backing.check_conflict() || self.cursor_fallback_appeared()
    }

    /// Whether `~/.icons/default/index.theme` was absent at load but not any more, and
    /// the model has not created it itself (a committed creation puts it in
    /// [`Self::backing`]). Logged at `warn` like every other conflict.
    fn cursor_fallback_appeared(&self) -> bool {
        if !self.cursor_fallback_absent || self.cursor_fallback().is_some() {
            return false;
        }
        let Some(path) = self.paths.cursor_fallback_theme.as_deref() else {
            return false;
        };
        let appeared = !is_absent(path);
        if appeared {
            tracing::warn!(
                path = %path.display(),
                "the XCursor fallback theme was created by another program since it was \
                 found absent; the staged creation is not written over it (R5.6)"
            );
        }
        appeared
    }

    /// The cursor copies whose on-disk value differs from the one the Theme page shows,
    /// in a fixed order (R3.4).
    ///
    /// The value shown is the one [`Self::load`] reads first (`settings.ini`, then the
    /// fallbacks listed there), so a copy is listed when it sets the cursor theme or
    /// size to something else. A copy that does not carry the value at all (an
    /// unreadable file, a missing key, a `hyprland.lua` without the `hl.env` lines or
    /// whose structure the Lua editor cannot follow, an absent `index.theme`) is not a
    /// disagreement: the next cursor Apply writes or creates every copy that can carry
    /// the value. The comparison uses the on-disk state, not staged edits, and is
    /// re-derived on each call, so after a committed Apply it reflects what was written.
    pub fn disagreeing_cursor_copies(&self) -> Vec<CursorCopy> {
        let copies = CursorCopies::read(&self.backing, &self.paths);
        let shown_theme = self.cursor_theme.original();
        let shown_size = self.cursor_size.original();
        let differs = |copy: Option<String>, shown: Option<&str>| match (copy, shown) {
            (Some(copy), Some(shown)) => copy != shown,
            _ => false,
        };
        let candidates = [
            (
                CursorCopy::Gtk3Settings,
                settings_value(copies.gtk3.as_ref(), KEY_CURSOR_THEME),
                settings_value(copies.gtk3.as_ref(), KEY_CURSOR_SIZE),
            ),
            (
                CursorCopy::Gtk4Settings,
                settings_value(copies.gtk4.as_ref(), KEY_CURSOR_THEME),
                settings_value(copies.gtk4.as_ref(), KEY_CURSOR_SIZE),
            ),
            (
                CursorCopy::UwsmEnv,
                env_value(copies.uwsm.as_ref(), ENV_CURSOR_THEME),
                env_value(copies.uwsm.as_ref(), ENV_CURSOR_SIZE),
            ),
            (
                CursorCopy::HyprlandLua,
                lua_env_value(copies.hyprland_lua.as_ref(), ENV_CURSOR_THEME),
                lua_env_value(copies.hyprland_lua.as_ref(), ENV_CURSOR_SIZE),
            ),
            (
                CursorCopy::CursorFallbackTheme,
                fallback_inherits(copies.cursor_fallback.as_ref()),
                None,
            ),
        ];
        candidates
            .into_iter()
            .filter(|(_, theme, size)| {
                differs(theme.clone(), shown_theme) || differs(size.clone(), shown_size)
            })
            .map(|(copy, _, _)| copy)
            .collect()
    }

    /// The note the Theme page shows when cursor copies disagree (see
    /// [`Self::disagreeing_cursor_copies`]), or `None` when they all agree.
    pub fn cursor_mismatch_note(&self) -> Option<String> {
        let copies = self.disagreeing_cursor_copies();
        if copies.is_empty() {
            return None;
        }
        let names: Vec<&str> = copies.into_iter().map(CursorCopy::label).collect();
        Some(format!(
            "The cursor is set differently in {}. Applying a cursor change writes the same \
             theme and size to every copy.",
            names.join(", ")
        ))
    }

    /// The note the Theme page shows under the cursor controls about the session
    /// environment copies, or `None` when the model writes none of them.
    ///
    /// `uwsm/env` and `hyprland.lua`'s `hl.env` lines set `XCURSOR_THEME`/`XCURSOR_SIZE`
    /// for the session, and both are read when the session starts (uwsm exports the
    /// first, Hyprland runs the second at startup), so programs already running — and the
    /// session environment new programs inherit — keep the old values until the next
    /// login (architecture §6, analysis §7.7). The cursor itself changes immediately:
    /// `hyprctl setcursor` and `gsettings` apply it live. The note names only the copies
    /// this model actually writes (on a hyprlang host there is no `hyprland.lua` copy).
    pub fn cursor_login_note(&self) -> Option<String> {
        let env_copies: Vec<&str> = [
            self.uwsm().map(|_| "uwsm/env"),
            self.hyprland_lua()
                .map(|_| "the hl.env lines of hyprland.lua"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let copies = match env_copies.as_slice() {
            [] => return None,
            [one] => format!("copy ({one}) takes"),
            many => format!("copies ({}) take", many.join(" and ")),
        };
        Some(format!(
            "A cursor change shows immediately; its session environment {copies} effect \
             at the next login."
        ))
    }

    /// Re-reads the backing files and scans the themes again, returning a fresh model with
    /// a new freshness baseline (R5.6 "warn and re-load").
    ///
    /// Called after [`Self::check_conflict`] detects an external edit: the fresh model
    /// re-parses the current files (discarding the now-stale staged edits) so a
    /// subsequent Apply builds on the current contents.
    pub fn reload(&self) -> ThemesModel {
        ThemesModel::load(
            &self.roots,
            self.paths.clone(),
            self.live_restyle,
            self.app_env_gtk_theme.clone(),
        )
        .with_xsettingsd_running(self.xsettingsd_running)
        .with_notifd_status(self.notifd)
    }

    /// The Theme page's GTK/icon/cursor contribution to the Apply plan, or `None` when
    /// there is nothing to contribute (task 6.4).
    ///
    /// Renders each changed value into the files that carry it through the surgical
    /// parsers (§3) and collects the reload parameters. A cursor change produces writes
    /// for both `settings.ini` files, `uwsm/env`, `hyprland.lua` (its two `hl.env`
    /// lines) and — when the theme changed — `~/.icons/default/index.theme`, all with
    /// the **identical** value (R3.4); a GTK/icon theme change produces only the two
    /// `settings.ini` writes.
    ///
    /// A value a file refused is reported in [`ThemesApply::skipped`] (task 9.28). When
    /// *every* staged value was refused there is no write, but the contribution is still
    /// returned — with its validations, so the pipeline's step 1 still re-checks the
    /// values — because the refusals are the only explanation the user will get for an
    /// Apply that changes nothing. `None` therefore means "not dirty", or dirty with no
    /// file to render into and nothing refused (no readable copy at all, R4.4).
    pub fn apply_contribution(&self) -> Option<ThemesApply> {
        if !self.is_dirty() {
            return None;
        }
        let (writes, record) = self.render_writes();
        log_skipped_writes(&record.skipped);
        if writes.is_empty() && record.skipped.is_empty() {
            // Dirty but no file to write — e.g. no copy of the value was readable.
            // Nothing to apply or to report; the page stays dirty for a retry.
            tracing::warn!(
                "theme change is dirty but no backing file could be written; skipping the theme apply (R4.4)"
            );
            return None;
        }
        // The links follow the `gtk-theme-name` write (task 11.16): only when the GTK
        // theme reached a settings.ini file, never for an icon- or cursor-only change.
        let links = if record.written.gtk_theme {
            self.plan_gtk4_links()
        } else {
            Vec::new()
        };
        let reload_params = self.reload_params(&record);
        Some(ThemesApply {
            writes,
            links,
            reload_params,
            validations: self.validations(),
            skipped: record.skipped,
        })
    }

    /// The Theme-page note naming each staged GTK/icon/cursor value that one of its
    /// files cannot take, with the file and the reason, or `None` when every staged
    /// value can be written everywhere it lives (task 9.28; R3.4, R5.5).
    ///
    /// Such a value stays pending after an Apply, so without this note a page that never
    /// becomes clean would be explained only in the journal. The everyday cause is a
    /// `hyprland.lua` whose structure the Lua editor cannot follow, which refuses both
    /// cursor values (see [`Self::render_hyprland_lua`]).
    ///
    /// Derived from a fresh render of the staged values rather than remembered from the
    /// last Apply: the render is deterministic over the staged values and the backing
    /// texts — which is also what [`Self::commit`] relies on — so it is exactly what the
    /// last Apply skipped and what the next one would. Re-deriving it means the note
    /// cannot go stale after a Reset, a Refresh or a conflict reload, and it already
    /// shows when the value is picked, before the user clicks Apply.
    pub fn skipped_writes_note(&self) -> Option<String> {
        if !self.is_dirty() {
            return None;
        }
        skipped_writes_note(&self.render_writes().1.skipped)
    }

    /// The staged theme values as the [`SettingId`]s step 1 of the Apply pipeline
    /// re-checks (task 9.27, R8.3): every dirty value, whether or not a writer accepted
    /// it — a value that fails here aborts the Apply before any file is touched, which
    /// is the point.
    fn validations(&self) -> Vec<(SettingId, Value)> {
        [
            (ThemeValue::GtkTheme, &self.gtk_theme),
            (ThemeValue::IconTheme, &self.icon_theme),
            (ThemeValue::CursorTheme, &self.cursor_theme),
            (ThemeValue::CursorSize, &self.cursor_size),
        ]
        .into_iter()
        .filter(|(_, selection)| selection.is_dirty())
        .filter_map(|(which, selection)| {
            selection
                .effective()
                .map(|value| (which.setting_id(), Value::Enum(value.to_string())))
        })
        .collect()
    }

    /// The directory holding the GTK 4 theme links: the directory of
    /// [`ThemesPaths::gtk4_settings`] (`~/.config/gtk-4.0`).
    fn gtk4_links_dir(&self) -> Option<&Path> {
        self.paths.gtk4_settings.parent()
    }

    /// The first GTK 4 theme link path that holds a user-managed override, with its
    /// kind — what makes the app leave all three links alone (task 11.16).
    ///
    /// An override is a plain file or directory, a path that cannot be inspected, or a
    /// symbolic link that resolves to an existing location **outside** every theme root
    /// (for instance into `~/custom/`): the user pointed GTK 4 somewhere of their own,
    /// which the app must not silently replace with a theme link, just as it never
    /// replaces a plain file. A dangling link (whose target no longer resolves — say
    /// the theme it pointed into was uninstalled) is not an override: it is a stale
    /// app/`setup.sh` link and gets repaired. The foreign-link rule is applied only
    /// when at least one theme root exists; without any, nothing could be inside a
    /// root, and the Apply plans no link step anyway.
    fn gtk4_links_override(&self) -> Option<(PathBuf, LinkOverride)> {
        let dir = self.gtk4_links_dir()?;
        let roots = resolved_theme_roots(&self.roots.gtk_theme_dirs);
        GTK4_THEME_LINKS.iter().find_map(|name| {
            let path = dir.join(name);
            let kind = match inspect_link(&path) {
                LinkEntry::NotALink => LinkOverride::NotALink,
                LinkEntry::Unreadable => LinkOverride::Unreadable,
                LinkEntry::Symlink(_) if !roots.is_empty() => match std::fs::canonicalize(&path) {
                    Ok(resolved) if !roots.iter().any(|root| resolved.starts_with(root)) => {
                        LinkOverride::ForeignLink
                    }
                    _ => return None,
                },
                LinkEntry::Symlink(_) | LinkEntry::Absent => return None,
            };
            Some((path, kind))
        })
    }

    /// Plans the GTK 4 theme link steps for the staged GTK theme (task 11.16; R3.3,
    /// analysis §8.4), logging why when it plans none.
    ///
    /// The rule, mirroring what the dotfiles' `setup.sh` does after a theme change:
    ///
    /// - When any of `gtk.css`, `gtk-dark.css`, `assets` in `~/.config/gtk-4.0` is a
    ///   plain file or a directory, cannot be inspected, or is a link resolving outside
    ///   every theme root (see `gtk4_links_override`), none of the three is
    ///   touched and a `warn` says so: a real file there is the user's own override —
    ///   like a set `GTK_THEME`, the app does not fight it — and changing only some of
    ///   the three would mix two themes. The `settings.ini`/gsettings writes still land.
    /// - When the theme has a `gtk-4.0/` directory under a theme root
    ///   ([`ThemeRoots::gtk_theme_dirs`]), each link is pointed at the entry of the same
    ///   name inside it; an entry the theme does not ship is not linked (an existing
    ///   link is removed rather than left on the old theme, and never made dangling).
    /// - When no theme root has a `gtk-4.0/` directory for it — a GTK 3-only theme, or
    ///   a built-in like Adwaita that has no theme directory — the links are removed,
    ///   so GTK 4 and libadwaita apps use their default look (the page says so, see
    ///   [`Self::gtk4_links_note`]).
    /// - When the theme's `gtk-4.0/` directory resolves outside every theme root, when
    ///   the name is not a single directory name (only possible for a hand-edited
    ///   `settings.ini` value), or when no theme root exists at all, nothing is planned
    ///   (logged at `warn`, `warn` and `info`).
    ///
    /// A link that already points where it should is not planned again, so a no-op
    /// theme change leaves the links untouched.
    fn plan_gtk4_links(&self) -> Vec<LinkWrite> {
        let (Some(theme), Some(dir)) = (self.gtk_theme.effective(), self.gtk4_links_dir()) else {
            return Vec::new();
        };
        if let Some((path, kind)) = self.gtk4_links_override() {
            tracing::warn!(
                path = %path.display(),
                kind = kind.as_str(),
                "a GTK 4 theme path is a user-managed override, not a theme link; \
                 leaving all three GTK 4 theme links alone, so GTK 4 and libadwaita apps \
                 do not follow this GTK-theme change (analysis §8.4)"
            );
            return Vec::new();
        }
        let wanted = match resolve_gtk4_theme(&self.roots.gtk_theme_dirs, theme) {
            Gtk4ThemeSource::Found { targets } => targets,
            Gtk4ThemeSource::Missing => {
                tracing::info!(
                    theme,
                    "the GTK theme has no gtk-4.0 directory under any theme root; \
                     removing the GTK 4 theme links"
                );
                [None, None, None]
            }
            Gtk4ThemeSource::OutsideRoots { resolved } => {
                tracing::warn!(
                    theme,
                    resolved = %resolved.display(),
                    "the GTK theme's GTK 4 files resolve outside every theme root; \
                     leaving the GTK 4 theme links alone"
                );
                return Vec::new();
            }
            Gtk4ThemeSource::InvalidName => {
                tracing::warn!(
                    theme,
                    "the GTK theme name is not a single directory name; leaving the GTK 4 \
                     theme links alone"
                );
                return Vec::new();
            }
            Gtk4ThemeSource::NoRoots => {
                tracing::info!("no GTK theme root could be found; skipping the GTK 4 theme links");
                return Vec::new();
            }
        };
        GTK4_THEME_LINKS
            .iter()
            .zip(wanted)
            .filter_map(|(name, wanted)| {
                let path = dir.join(name);
                let current = inspect_link(&path);
                let action = match (current, wanted) {
                    (LinkEntry::Symlink(current), Some(target)) if current == target => {
                        return None;
                    }
                    (_, Some(target)) => LinkAction::PointAt(target),
                    (LinkEntry::Symlink(_), None) => LinkAction::Remove,
                    (_, None) => return None,
                };
                Some(LinkWrite {
                    path,
                    action,
                    label: format!("GTK 4 theme link ({name})"),
                })
            })
            .collect()
    }

    /// The Theme-page note about the GTK 4 theme links, or `None` when there is
    /// nothing to say (task 11.16; analysis §8.4).
    ///
    /// - When one of `~/.config/gtk-4.0/{gtk.css,gtk-dark.css,assets}` is a file or
    ///   directory of the user's own, or a link of the user's own into a place outside
    ///   the theme folders, the note says the app leaves them alone, so GTK 4 and
    ///   libadwaita apps do not follow a theme change made here.
    /// - Otherwise it is about the GTK theme the page shows (the staged one, or the
    ///   current one): a theme without a GTK 4 version means those apps fall back to the
    ///   default Adwaita look (an Apply removes the links), and a theme whose GTK 4
    ///   files lie outside the theme folders is not linked. A built-in theme (Adwaita)
    ///   gets no note: the default look is then exactly what was chosen, and neither
    ///   does a name that is not a theme name at all (it cannot come from the
    ///   drop-down's scanned list, only from a hand-edited `settings.ini`).
    ///
    /// Derived from the filesystem on every call, so it reflects the state after an
    /// Apply or an external change without a reload; it never logs (the Apply logs
    /// its decisions when it plans the steps).
    pub fn gtk4_links_note(&self) -> Option<String> {
        if let Some((path, kind)) = self.gtk4_links_override() {
            let what = match kind {
                LinkOverride::NotALink | LinkOverride::Unreadable => {
                    "is a file of its own rather than a link to a theme"
                }
                LinkOverride::ForeignLink => "links to a place outside the theme folders",
            };
            return Some(format!(
                "{} {what}, so GTK 4 and libadwaita apps are left as they are: they do not \
                 follow a GTK-theme change made here.",
                self.shown_path(&path)
            ));
        }
        let theme = self.gtk_theme.effective()?;
        match resolve_gtk4_theme(&self.roots.gtk_theme_dirs, theme) {
            Gtk4ThemeSource::Missing if !is_builtin_gtk_theme(theme) => Some(format!(
                "\u{201c}{theme}\u{201d} has no GTK 4 version, so GTK 4 and libadwaita apps \
                 fall back to the default Adwaita look."
            )),
            Gtk4ThemeSource::OutsideRoots { .. } => Some(format!(
                "The GTK 4 files of \u{201c}{theme}\u{201d} lie outside the theme folders, so \
                 GTK 4 and libadwaita apps are not switched to it."
            )),
            _ => None,
        }
    }

    /// `path` as the Theme page shows it: `~/…` under [`ThemesPaths::home`], absolute
    /// otherwise.
    fn shown_path(&self, path: &Path) -> String {
        self.paths
            .home
            .as_deref()
            .and_then(|home| contract_home(path, home))
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Commits the staged changes after a successful Apply: takes the written bytes back
    /// into the backing set (re-baselining freshness and the in-memory text, see
    /// [`BackingSet::absorb_writes`]) and promotes the staged selections **that actually
    /// reached a file** to their current value (R5.6).
    ///
    /// Promotion is derived from the rendered writes rather than from the dirty flags:
    /// a selection that reached no file, or that one of the files carrying it refused
    /// (see [`ThemeRenderRecord`]), is left staged, so the page stays dirty and the user
    /// can retry. Promoting it would leave the model claiming a value the config does not
    /// hold — a silent model-vs-disk divergence that no later Apply would repair, because
    /// the selection would no longer look changed.
    pub fn commit(&mut self) {
        // Re-render the writes (staged values still present) to capture the exact bytes
        // written. Rendering is deterministic and the backing texts are unchanged since
        // the Apply, so this reproduces both the bytes that were written and which keys
        // were skipped.
        let (writes, record) = self.render_writes();
        self.backing.absorb_writes(&writes);
        let promotable = record.promotable();
        if promotable.gtk_theme {
            self.gtk_theme.commit();
        }
        if promotable.icon_theme {
            self.icon_theme.commit();
        }
        if promotable.cursor_theme {
            self.cursor_theme.commit();
        }
        if promotable.cursor_size {
            self.cursor_size.commit();
        }
    }

    /// Renders the file writes for the current staged changes, together with the per-value
    /// record of what each render achieved — used by [`Self::apply_contribution`] (the
    /// writes, the refusals and the reload parameters), by [`Self::commit`] (which values
    /// may be promoted) and by [`Self::skipped_writes_note`] (the refusals).
    fn render_writes(&self) -> (Vec<FileWrite>, ThemeRenderRecord) {
        // Which files are involved is decided here; *which keys* each file receives is
        // decided by the per-render functions, which read the same per-value dirty flags.
        let cursor_dirty = self.cursor_theme.is_dirty() || self.cursor_size.is_dirty();
        let any_dirty = self.gtk_theme.is_dirty() || self.icon_theme.is_dirty() || cursor_dirty;

        let mut writes = Vec::new();
        let mut record = ThemeRenderRecord::default();

        // Every theme/cursor key lives in settings.ini, so any change writes both files.
        if any_dirty {
            let copies = [
                (self.gtk3(), CursorCopy::Gtk3Settings),
                (self.gtk4(), CursorCopy::Gtk4Settings),
            ];
            for (backing, copy) in copies {
                let Some(backing) = backing else {
                    continue;
                };
                let (write, file_record) = self.render_settings_ini(backing, copy.label());
                record.merge(file_record);
                writes.extend(write);
            }
        }

        // The cursor is additionally duplicated in hyprland.lua's hl.env lines,
        // uwsm/env and the XCursor fallback theme; write the identical value there
        // whenever the cursor changed (R3.4).
        if cursor_dirty {
            if let Some(backing) = self.hyprland_lua() {
                let (write, file_record) = self.render_hyprland_lua(backing);
                record.merge(file_record);
                writes.extend(write);
            }
            if let Some(backing) = self.uwsm() {
                let (write, file_record) = self.render_uwsm_env(backing);
                record.merge(file_record);
                writes.extend(write);
            }
            let (write, file_record) = self.render_cursor_fallback();
            record.merge(file_record);
            writes.extend(write);
        }

        (writes, record)
    }

    /// Renders one `settings.ini` write, editing only the keys that changed, and reports
    /// which of those keys the edit actually landed for.
    ///
    /// A key whose `set_value` fails is skipped and recorded as a refusal by `file` (the
    /// copy's user-facing name); the remaining keys are still written, and the returned
    /// [`ThemeRenderRecord`] tells [`Self::commit`] which selections may be promoted. A
    /// failure here is always a *refusal* by a file that carries the key, never an
    /// absent copy — the INI writer appends the key, and even the `[Settings]` group,
    /// when it is missing — so it blocks promotion. (In practice a
    /// value the INI writer rejects, i.e. one containing a newline, is rejected by the env
    /// and Lua writers too, so nothing is written anywhere; the flag is what keeps
    /// that conclusion from depending on the writers agreeing.)
    fn render_settings_ini(
        &self,
        backing: &BackingText,
        file: &'static str,
    ) -> (Option<FileWrite>, ThemeRenderRecord) {
        let (mut ini, _) = IniFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut validated_by = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, key: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match ini.set_value(SETTINGS_GROUP, key, value) {
                Ok(_) => {
                    changed_keys.push(label.to_string());
                    validated_by.push(value_id.setting_id());
                    *record.written.flag(value_id) = true;
                    *record.in_gtk_settings.flag(value_id) = true;
                }
                Err(error) => record.refuse(value_id, file, error.to_string()),
            }
        };
        if self.gtk_theme.is_dirty() {
            set(
                ThemeValue::GtkTheme,
                KEY_GTK_THEME,
                self.gtk_theme.effective(),
                "GTK theme",
            );
        }
        if self.icon_theme.is_dirty() {
            set(
                ThemeValue::IconTheme,
                KEY_ICON_THEME,
                self.icon_theme.effective(),
                "icon theme",
            );
        }
        if self.cursor_theme.is_dirty() {
            set(
                ThemeValue::CursorTheme,
                KEY_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme",
            );
        }
        if self.cursor_size.is_dirty() {
            set(
                ThemeValue::CursorSize,
                KEY_CURSOR_SIZE,
                self.cursor_size.effective(),
                "cursor size",
            );
        }

        if changed_keys.is_empty() {
            // Nothing was set, so this file contributes no write — only whatever refusals
            // were recorded above.
            return (None, record);
        }
        (
            Some(FileWrite {
                path: backing.path.clone(),
                contents: ini.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::GtkSettings,
                // The settings whose values this write carries; each is in
                // `ThemesApply::validations` (task 9.27), which the plan-drift guard
                // checks.
                validated_by,
                mode: WriteMode::Replace,
            }),
            record,
        )
    }

    /// Renders the `hyprland.lua` cursor write: the second argument of the two
    /// `hl.env("XCURSOR_THEME", "…")` / `hl.env("XCURSOR_SIZE", "…")` calls, and nothing
    /// else (R3.4, architecture §3 "Lua literal" row).
    ///
    /// [`LuaConfigFile::set_env`] is the **only** editing method ever called on this
    /// file. `hyprland.lua` is real code — keybinds, window rules, the monitor loader —
    /// and the Lua editor can address its top-level `hl.config({ … })` literals too, but
    /// the app owns none of them; architecture §3 restricts it to the two env lines, and
    /// task 11.3 documented that restriction as a contract the caller (this function)
    /// keeps, because a parsed file does not know which file it came from. The size is
    /// written as a Lua string (`"24"`) like every `hl.env` value.
    ///
    /// Each field is applied independently, and the three ways a field can fail are
    /// recorded differently in the returned [`ThemeRenderRecord`]:
    ///
    /// - no such call ([`LuaEditError::EnvNotFound`]): this file carries no copy of the
    ///   field, so there is nothing to diverge from; it is skipped (logged at `debug`)
    ///   and the value may still be promoted on the strength of the other copies. The
    ///   editor never *adds* an `hl.env` line — inserting code into a file the app does
    ///   not author would be guessing where it belongs. If only one of the two lines is
    ///   present, that field is still written rather than the file being abandoned.
    /// - a structure the editor cannot follow ([`LuaEditError::UnsafeFile`]): no line can
    ///   be located safely, so nothing is written, but the file may well hold an env line
    ///   with the old value, so it counts as a **refusal** that blocks promotion and is
    ///   reported as a [`SkippedWrite`] with the editor's reason (task 9.28) — the app
    ///   refuses rather than guesses.
    /// - a value the Lua string rule rejects ([`LuaEditError::Literal`] — a `"`, `\` or
    ///   control character): the existing line keeps its old value, a refusal too.
    ///
    /// Returns `None` for the write when neither field could be written.
    fn render_hyprland_lua(&self, backing: &BackingText) -> (Option<FileWrite>, ThemeRenderRecord) {
        let mut file = LuaConfigFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut validated_by = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, name: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match file.set_env(name, value) {
                Ok(()) => {
                    changed_keys.push(label.to_string());
                    validated_by.push(value_id.setting_id());
                    *record.written.flag(value_id) = true;
                }
                Err(LuaEditError::EnvNotFound(_)) => {
                    tracing::debug!(
                        name,
                        "no hl.env line for this variable in hyprland.lua; skipping that field"
                    );
                }
                Err(error) => {
                    record.refuse(value_id, CursorCopy::HyprlandLua.label(), error.to_string())
                }
            }
        };
        if self.cursor_theme.is_dirty() {
            set(
                ThemeValue::CursorTheme,
                ENV_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme (hyprland.lua hl.env)",
            );
        }
        if self.cursor_size.is_dirty() {
            set(
                ThemeValue::CursorSize,
                ENV_CURSOR_SIZE,
                self.cursor_size.effective(),
                "cursor size (hyprland.lua hl.env)",
            );
        }

        if changed_keys.is_empty() {
            return (None, record);
        }
        (
            Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::HyprlandLua,
                // The cursor settings this write carries, as for `settings.ini`.
                validated_by,
                mode: WriteMode::Replace,
            }),
            record,
        )
    }

    /// Renders the XCursor fallback write: `Inherits=<cursor theme>` in the
    /// `[Icon Theme]` group of `~/.icons/default/index.theme`, through the INI writer
    /// (R3.4, analysis §8.4). Programs that ignore `XCURSOR_THEME` load the theme named
    /// `default`, which this file points at the chosen cursor theme.
    ///
    /// Only the cursor *theme* lives here (the file has no size), so a size-only change
    /// writes nothing. When the file exists, only the `Inherits=` value span changes
    /// (the key is appended to the group, or the group created, when missing — the INI
    /// writer's normal behaviour). When it was **absent** at load, the write creates it
    /// ([`WriteMode::CreateNew`]) from [`CURSOR_FALLBACK_TEMPLATE`] plus the `Inherits=`
    /// line — the same three keys the dotfiles ship. A file that exists but could not be
    /// read is left alone, like any other unreadable copy.
    ///
    /// A value the INI writer rejects (a newline) is a refusal that blocks promotion
    /// (and is reported, task 9.28) when the file exists; for a file that would have
    /// been created nothing on disk disagrees, so it is merely skipped, like any other
    /// absent copy.
    fn render_cursor_fallback(&self) -> (Option<FileWrite>, ThemeRenderRecord) {
        let mut record = ThemeRenderRecord::default();
        if !self.cursor_theme.is_dirty() {
            return (None, record);
        }
        let Some(theme) = self.cursor_theme.effective() else {
            return (None, record);
        };
        let (path, text, mode) = match (self.cursor_fallback(), &self.paths.cursor_fallback_theme) {
            (Some(backing), _) => (
                backing.path.clone(),
                backing.text.as_str(),
                WriteMode::Replace,
            ),
            (None, Some(path)) if self.cursor_fallback_absent => {
                (path.clone(), CURSOR_FALLBACK_TEMPLATE, WriteMode::CreateNew)
            }
            _ => return (None, record),
        };

        let (mut ini, _) = IniFile::parse(text);
        if let Err(error) = ini.set_value(ICON_THEME_GROUP, KEY_INHERITS, theme) {
            match mode {
                WriteMode::Replace => record.refuse(
                    ThemeValue::CursorTheme,
                    CursorCopy::CursorFallbackTheme.label(),
                    error.to_string(),
                ),
                WriteMode::CreateNew => tracing::debug!(
                    %error,
                    "the XCursor fallback theme would not take the cursor theme; not creating it"
                ),
            }
            return (None, record);
        }
        record.written.cursor_theme = true;
        (
            Some(FileWrite {
                path,
                contents: ini.emit().into_bytes(),
                changed_keys: vec!["cursor theme (index.theme Inherits)".to_string()],
                backing: BackingFile::CursorFallbackTheme,
                // Only the cursor theme lives in this file.
                validated_by: vec![SettingId::CursorTheme],
                mode,
            }),
            record,
        )
    }

    /// Renders the `uwsm/env` cursor-env write, editing (or appending) the
    /// `XCURSOR_*` exports, and reports which fields the edit landed for.
    ///
    /// Like the `settings.ini` render, a failure here is a refusal by a file that carries
    /// the value (the env writer appends a missing export), so it blocks promotion and
    /// is reported (task 9.28).
    fn render_uwsm_env(&self, backing: &BackingText) -> (Option<FileWrite>, ThemeRenderRecord) {
        let (mut file, _) = EnvFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut validated_by = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, key: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match file.set_value(key, value) {
                Ok(_) => {
                    changed_keys.push(label.to_string());
                    validated_by.push(value_id.setting_id());
                    *record.written.flag(value_id) = true;
                }
                Err(error) => {
                    record.refuse(value_id, CursorCopy::UwsmEnv.label(), error.to_string())
                }
            }
        };
        if self.cursor_theme.is_dirty() {
            set(
                ThemeValue::CursorTheme,
                ENV_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme (uwsm/env)",
            );
        }
        if self.cursor_size.is_dirty() {
            set(
                ThemeValue::CursorSize,
                ENV_CURSOR_SIZE,
                self.cursor_size.effective(),
                "cursor size (uwsm/env)",
            );
        }

        if changed_keys.is_empty() {
            return (None, record);
        }
        (
            Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::UwsmEnv,
                // The cursor settings this write carries, as for `settings.ini`.
                validated_by,
                mode: WriteMode::Replace,
            }),
            record,
        )
    }

    /// The reload parameters for the values this Apply actually put into a
    /// `settings.ini` file, as recorded by the render in `record` (tasks 4.4, 9.28).
    ///
    /// The reload table turns these into `gsettings set` calls and `hyprctl setcursor`,
    /// so a value set here is pushed into the live session. The rule:
    ///
    /// - **A value is pushed only when it reached a `settings.ini` file.** Deriving the
    ///   parameters from "the value is staged" instead (as before task 9.28) pushed a
    ///   value every file writer had refused into dconf, leaving the live session and
    ///   the config files disagreeing.
    /// - **When a value reached some files but not others, the push follows
    ///   `settings.ini`.** `settings.ini` is what GTK reads, what this app reads back
    ///   (see [`Self::load`]) and what the `gsettings` keys mirror, so the live value
    ///   should be whatever it now holds. A cursor value refused only by `hyprland.lua`
    ///   is therefore still pushed: GTK and the live Hyprland cursor show the new theme,
    ///   and the copy that lags behind is named in [`ThemesApply::skipped`] and on the
    ///   page. A value refused by `settings.ini` itself is not pushed, even where
    ///   `uwsm/env` took it.
    ///
    /// The GTK and icon themes live only in `settings.ini`, so for them "reached a file"
    /// and "reached `settings.ini`" coincide. The cursor value carries theme *and* size
    /// (`hyprctl setcursor` needs both), so it is present when either reached
    /// `settings.ini`; each half is the staged value when that half was written and the
    /// value already on disk otherwise, so the push never carries a half the files
    /// refused.
    fn reload_params(&self, record: &ThemeRenderRecord) -> ReloadParams {
        let reached = record.in_gtk_settings;
        // The value `settings.ini` holds after this Apply: the staged one if it was
        // written, the loaded one otherwise.
        fn now_in_settings(selection: &Selection, written: bool) -> Option<&str> {
            if written {
                selection.effective()
            } else {
                selection.original()
            }
        }
        let cursor = if reached.cursor_theme || reached.cursor_size {
            match (
                now_in_settings(&self.cursor_theme, reached.cursor_theme),
                now_in_settings(&self.cursor_size, reached.cursor_size).and_then(parse_cursor_size),
            ) {
                (Some(theme), Some(size)) => Some(CursorValue {
                    theme: theme.to_string(),
                    size,
                }),
                _ => {
                    // A cursor change with no usable theme+size (a degenerate config
                    // with neither configured nor selected) cannot drive setcursor; the
                    // file writes still stand, only the live cursor reload is skipped.
                    tracing::debug!(
                        "cursor changed but no usable theme+size; skipping the live cursor reload"
                    );
                    None
                }
            }
        } else {
            None
        };
        ReloadParams {
            wallpaper: None,
            fit: None,
            cursor,
            gtk_theme: reached
                .gtk_theme
                .then(|| self.gtk_theme.effective().map(str::to_string))
                .flatten(),
            icon_theme: reached
                .icon_theme
                .then(|| self.icon_theme.effective().map(str::to_string))
                .flatten(),
        }
    }
}

/// The parsed cursor copies of one [`BackingSet`], each `None` when its file is
/// unreadable or not part of the write set — the read side shared by
/// [`ThemesModel::load`] (the values the page shows) and
/// [`ThemesModel::disagreeing_cursor_copies`] (which copies differ from them).
struct CursorCopies {
    /// `gtk-3.0/settings.ini`.
    gtk3: Option<IniFile>,
    /// `gtk-4.0/settings.ini`.
    gtk4: Option<IniFile>,
    /// `uwsm/env`.
    uwsm: Option<EnvFile>,
    /// `hyprland.lua` (read only through [`LuaConfigFile::env_value`]).
    hyprland_lua: Option<LuaConfigFile>,
    /// `~/.icons/default/index.theme`.
    cursor_fallback: Option<IniFile>,
}

impl CursorCopies {
    /// Parses every readable backing file named in `paths`.
    fn read(backing: &BackingSet, paths: &ThemesPaths) -> CursorCopies {
        let text = |path: Option<&Path>| {
            path.and_then(|path| backing.get(path))
                .map(|backing| backing.text.as_str())
        };
        let ini = |path: Option<&Path>| text(path).map(|text| IniFile::parse(text).0);
        CursorCopies {
            gtk3: ini(Some(&paths.gtk3_settings)),
            gtk4: ini(Some(&paths.gtk4_settings)),
            uwsm: text(Some(&paths.uwsm_env)).map(|text| EnvFile::parse(text).0),
            hyprland_lua: text(paths.hyprland_lua.as_deref()).map(LuaConfigFile::parse),
            cursor_fallback: ini(paths.cursor_fallback_theme.as_deref()),
        }
    }
}

/// Reads a `[Settings]` key from a parsed `settings.ini`, if present.
fn settings_value(ini: Option<&IniFile>, key: &str) -> Option<String> {
    ini.and_then(|file| file.value(SETTINGS_GROUP, key))
        .map(str::to_string)
}

/// Reads a variable from a parsed `uwsm/env`, if present.
fn env_value(env: Option<&EnvFile>, key: &str) -> Option<String> {
    env.and_then(|file| file.value(key)).map(str::to_string)
}

/// Reads the value of `hyprland.lua`'s top-level `hl.env("name", "…")` call, if present.
///
/// A file whose structure the Lua editor cannot follow yields nothing here (every line
/// is opaque to it), the same as a file without the line: the field is reported absent
/// rather than guessed (architecture §10).
fn lua_env_value(lua: Option<&LuaConfigFile>, name: &str) -> Option<String> {
    lua.and_then(|file| file.env_value(name))
        .map(str::to_string)
}

/// Reads the XCursor fallback theme's `[Icon Theme]` `Inherits=`, if present.
fn fallback_inherits(ini: Option<&IniFile>) -> Option<String> {
    ini.and_then(|file| file.value(ICON_THEME_GROUP, KEY_INHERITS))
        .map(str::to_string)
}

/// Detects the live `settings.ini` files that are plain files beside a repo copy (task
/// 11.12) — see [`ThemesModel::unlinked_gtk_settings`] for what counts and why — and
/// logs each at `warn`, naming the paths only (R7.3: never file contents).
fn detect_unlinked_gtk_settings(paths: &ThemesPaths) -> Vec<UnlinkedSettingsIni> {
    let Some(repo) = paths.dotfiles_repo.as_deref() else {
        return Vec::new();
    };
    [
        (&paths.gtk3_settings, GTK3_SETTINGS_REPO_RELATIVE),
        (&paths.gtk4_settings, GTK4_SETTINGS_REPO_RELATIVE),
    ]
    .into_iter()
    .filter_map(|(live, repo_relative)| {
        let repo_copy = repo.join(repo_relative);
        is_plain_file_beside(live, &repo_copy).then(|| UnlinkedSettingsIni {
            live: live.clone(),
            repo_copy,
        })
    })
    .inspect(|unlinked| {
        tracing::warn!(
            live = %unlinked.live.display(),
            repo_copy = %unlinked.repo_copy.display(),
            "GTK settings.ini is a plain file, no longer linked to its dotfiles repo copy; \
             the app keeps writing the live file, which the repo copy will not receive \
             (analysis §7.7)"
        );
    })
    .collect()
}

/// Whether `live` is a regular file (not a symlink) that is a different file from
/// `repo_copy`, and `repo_copy` exists as a file.
///
/// `symlink_metadata` inspects `live` itself without following a final link, so a
/// deployed symlink is never mistaken for a plain file. The identity check covers a
/// setup that links a whole directory instead of each file: there `live` is reached
/// through a symlinked parent, is itself a regular file, and is the very repo copy —
/// still linked, so not reported. Any I/O error answers `false`: an unreadable or
/// vanished path is no evidence of divergence, and this check must never fail a load.
fn is_plain_file_beside(live: &Path, repo_copy: &Path) -> bool {
    let live_is_plain = std::fs::symlink_metadata(live).is_ok_and(|meta| meta.is_file());
    if !live_is_plain || !repo_copy.is_file() {
        return false;
    }
    match (
        std::fs::canonicalize(live),
        std::fs::canonicalize(repo_copy),
    ) {
        (Ok(live), Ok(repo_copy)) => live != repo_copy,
        _ => false,
    }
}

/// What one GTK 4 theme link path holds, inspected without following it (task 11.16).
#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkEntry {
    /// Nothing is there.
    Absent,
    /// A symbolic link with this target text.
    Symlink(PathBuf),
    /// A plain file, a directory, or another non-link entry — a user-managed override.
    NotALink,
    /// The path could not be inspected (e.g. permission denied). Treated like
    /// [`LinkEntry::NotALink`]: what cannot be seen is not changed.
    Unreadable,
}

/// Inspects a GTK 4 theme link path with `symlink_metadata`/`read_link` only.
fn inspect_link(path: &Path) -> LinkEntry {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => match std::fs::read_link(path) {
            Ok(target) => LinkEntry::Symlink(target),
            Err(_) => LinkEntry::Unreadable,
        },
        Ok(_) => LinkEntry::NotALink,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LinkEntry::Absent,
        Err(_) => LinkEntry::Unreadable,
    }
}

/// Why a GTK 4 theme link path counts as a user-managed override (task 11.16; see
/// [`ThemesModel::gtk4_links_override`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkOverride {
    /// A plain file or a directory.
    NotALink,
    /// The path could not be inspected.
    Unreadable,
    /// A symbolic link resolving to an existing place outside every theme root.
    ForeignLink,
}

impl LinkOverride {
    /// The short name logged in the `kind` field of the override `warn`.
    fn as_str(self) -> &'static str {
        match self {
            LinkOverride::NotALink => "plain file or directory",
            LinkOverride::Unreadable => "not inspectable",
            LinkOverride::ForeignLink => "link outside the theme roots",
        }
    }
}

/// The theme roots that exist, canonicalized — the shared basis of both "inside a
/// theme root" checks (the link targets in [`resolve_gtk4_theme`] and the
/// foreign-link override in [`ThemesModel::gtk4_links_override`]). A root that does
/// not exist or cannot be resolved is skipped.
fn resolved_theme_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}

/// Where a GTK theme's GTK 4 part lives, as far as the GTK 4 theme links are concerned
/// (task 11.16).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Gtk4ThemeSource {
    /// The theme has a `gtk-4.0/` directory inside a theme root. Holds the link target
    /// for each of [`GTK4_THEME_LINKS`], in order, or `None` for an entry the theme
    /// does not ship. Every target is `<resolved root>/<theme>/gtk-4.0/<entry>` and has
    /// been checked to resolve inside a theme root.
    Found {
        /// The link targets, aligned with [`GTK4_THEME_LINKS`].
        targets: [Option<PathBuf>; 3],
    },
    /// No theme root has a `gtk-4.0/` directory for the theme (a GTK 3-only theme or
    /// a built-in theme).
    Missing,
    /// The name is not a single plain directory name (it holds a `/`, is `..`, …), so
    /// it is never joined onto a root. Only a hand-edited `settings.ini` can hold one.
    InvalidName,
    /// The theme's `gtk-4.0/` directory, or an entry in it, resolves outside every
    /// theme root (through a symlink), so the app will not link to it.
    OutsideRoots {
        /// Where it resolved to, for the log.
        resolved: PathBuf,
    },
    /// None of the theme roots exists, so there is nowhere a theme could be found.
    NoRoots,
}

/// Finds the GTK 4 part of `theme` under the theme roots `roots` (task 11.16; R3.3).
///
/// Each root is canonicalized first ([`resolved_theme_roots`]; a root that does not
/// exist is skipped), and the
/// first root in the given order holding a `<theme>/gtk-4.0/` directory is used — the
/// same "first theme directory that has it wins" order GTK applies when it looks a
/// theme up. The link targets are built from the *resolved* root, so they are
/// lexically inside it by construction: the theme name must be one plain path
/// component ([`is_single_directory_name`]: no `/`, not `.` or `..`), which a name read
/// from the user's `settings.ini` is not guaranteed to be — anything else is
/// [`Gtk4ThemeSource::InvalidName`]. A name staged through the Theme page has already
/// been held to the same rule (task 9.27). On top of that lexical rule, the `gtk-4.0/`
/// directory and
/// each entry linked must still resolve inside one of the resolved roots, so a theme
/// directory that is itself a symlink to somewhere else is never linked to (the task's
/// "never follow a link outside the theme roots"). An entry the theme does not ship —
/// or a dangling one — gets no target.
fn resolve_gtk4_theme(roots: &[PathBuf], theme: &str) -> Gtk4ThemeSource {
    let roots = resolved_theme_roots(roots);
    if roots.is_empty() {
        return Gtk4ThemeSource::NoRoots;
    }
    if !is_single_directory_name(theme) {
        return Gtk4ThemeSource::InvalidName;
    }
    let inside_a_root = |path: &Path| match std::fs::canonicalize(path) {
        Ok(resolved) if roots.iter().any(|root| resolved.starts_with(root)) => Ok(()),
        Ok(resolved) => Err(resolved),
        Err(_) => Err(path.to_path_buf()),
    };
    for root in &roots {
        let gtk4 = root.join(theme).join(GTK4_THEME_SUBDIR);
        if !gtk4.is_dir() {
            continue;
        }
        if let Err(resolved) = inside_a_root(&gtk4) {
            return Gtk4ThemeSource::OutsideRoots { resolved };
        }
        let mut targets: [Option<PathBuf>; 3] = [None, None, None];
        for (slot, name) in targets.iter_mut().zip(GTK4_THEME_LINKS) {
            let entry = gtk4.join(name);
            if !entry.exists() {
                continue;
            }
            if let Err(resolved) = inside_a_root(&entry) {
                return Gtk4ThemeSource::OutsideRoots { resolved };
            }
            *slot = Some(entry);
        }
        return Gtk4ThemeSource::Found { targets };
    }
    Gtk4ThemeSource::Missing
}

/// Whether nothing at all exists at `path` — not a file, a directory, or a symlink
/// (dangling or not). Only such a path may be *created*; anything else present is
/// either someone's file or a link that must never be replaced.
fn is_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Resolves the active `GTK_THEME` override, preferring the app's own environment over
/// `uwsm/env` (R3.3).
///
/// The app-environment copy is what actually overrides *this* app's theme (inherited
/// from whatever started the app, e.g. a wrapper or a shell export — the dotfiles set
/// it nowhere today), so it takes precedence; a commented-out `uwsm/env` line is not
/// an override and is ignored. Returns `None` when neither is active, in which case
/// the GTK-theme drop-down stays enabled.
fn resolve_gtk_override(
    app_env: Option<String>,
    uwsm: Option<&GtkThemeOverride>,
) -> Option<GtkOverrideOrigin> {
    if let Some(value) = app_env.filter(|value| !value.is_empty()) {
        return Some(GtkOverrideOrigin::AppEnvironment(value));
    }
    if let Some(GtkThemeOverride::Active { value }) = uwsm {
        return Some(GtkOverrideOrigin::UwsmEnv(value.clone()));
    }
    None
}

/// What [`scan_gtk_themes`] found under the GTK theme roots.
#[derive(Debug)]
struct GtkThemeScan {
    /// The names the GTK-theme drop-down offers: every installed theme plus the
    /// [`BUILTIN_GTK_THEMES`], de-duplicated and sorted.
    offered: Vec<String>,
    /// The names whose directory carries a GTK 3 stylesheet ([`has_gtk3_stylesheet`])
    /// under at least one root — the directories GTK 3 itself would load (step 2 of
    /// [`Gtk3Resolution`]). Kept so the model can tell, without touching the filesystem
    /// again, whether a built-in name GTK 3 does not have compiled in is still
    /// installed for it (task 9.29).
    with_gtk3_stylesheet: BTreeSet<String>,
}

/// Scans for the GTK themes the drop-down offers: those installed under `dirs`, plus
/// GTK's built-in ones (R3.3).
///
/// A subdirectory is a GTK theme when it carries a stylesheet for either GTK version
/// ([`has_gtk3_stylesheet`] or [`has_gtk4_stylesheet`]). Either one qualifies (R3.3):
/// themes differ in which version they style, and a GTK 3-only theme is still a valid
/// `gtk-theme-name`. A `gtk-*` subdirectory alone is not enough — keybinding packages
/// such as `Default` and `Emacs` ship a `gtk-3.0/` holding only `gtk-keys.css`, and
/// offering them would write a theme name that styles nothing (task 9.30). The
/// [`BUILTIN_GTK_THEMES`] names are added unconditionally because they
/// are selectable without any theme directory (see that constant for why). Names are
/// de-duplicated (a theme in `~/.themes` shadows a system one of the same name, and a
/// scanned `Adwaita` directory collapses with the built-in entry — only the name
/// matters, since that is what `gsettings`/`settings.ini` store) and returned sorted for
/// a stable drop-down, built-ins and scan results interleaved in one order.
///
/// The same pass records which names carry a GTK 3 stylesheet on some root
/// ([`GtkThemeScan::with_gtk3_stylesheet`]). That is deliberately narrower than
/// "installed": a directory with only a GTK 4 stylesheet is a theme for the drop-down
/// but nothing GTK 3 can load, so it does not make a GTK 4-only built-in name work on
/// GTK 3 (task 9.29). Because both sets come from the same predicate, a
/// keybinding-only `gtk-3.0/` can neither add a name to the drop-down nor silence the
/// one-sided note.
fn scan_gtk_themes(dirs: &[PathBuf]) -> GtkThemeScan {
    let mut offered = BTreeSet::new();
    let mut with_gtk3_stylesheet = BTreeSet::new();
    for (name, path) in theme_dir_entries(dirs) {
        if has_gtk3_stylesheet(&path) {
            with_gtk3_stylesheet.insert(name.clone());
            offered.insert(name);
        } else if has_gtk4_stylesheet(&path) {
            offered.insert(name);
        }
    }
    // The set both de-duplicates against the scan and keeps the single sort order.
    offered.extend(
        BUILTIN_GTK_THEMES
            .iter()
            .map(|builtin| builtin.name.to_string()),
    );
    GtkThemeScan {
        offered: offered.into_iter().collect(),
        with_gtk3_stylesheet,
    }
}

/// The file name of a theme's main stylesheet, which both GTK versions look for
/// inside the theme's version subdirectory (`gtk-dark.css` is only the dark variant
/// of it, never loaded without it).
const THEME_STYLESHEET: &str = "gtk.css";

/// Whether the installed theme directory `theme_dir` carries a stylesheet GTK 3 can
/// load: a regular file `gtk.css` inside a `gtk-3.<N>/` subdirectory (task 9.30;
/// R3.3).
///
/// GTK 3.24 probes `<theme>/gtk-3.<N>/gtk.css` with `N` from its own minor version
/// down in steps of 2 to 14, then `gtk-3.0` (step 2 of [`Gtk3Resolution`]), so a
/// theme may carry its CSS under a version-suffixed directory such as `gtk-3.20/`
/// and none under `gtk-3.0/`. Every `gtk-3.<digits>` directory counts here rather
/// than the exact list one GTK 3 build probes: the GTK 3 minor version differs
/// between hosts and this GTK 4 program has no GTK 3 library to ask. The cost is a
/// slight over-approximation — a directory named for a version GTK 3 skips (an odd
/// minor, or one newer than the installed library) would count — which no theme
/// installed on the target host exercises. A `gtk-3.0/` holding only `gtk-keys.css`
/// (a keybinding package) does not count.
///
/// Symlinks are followed, for the subdirectory and the stylesheet alike, as
/// [`theme_dir_entries`] does for the theme directory: GTK opens the path, so a
/// linked stylesheet is as good as a copied one. An unreadable directory carries no
/// stylesheet as far as the scan can tell.
fn has_gtk3_stylesheet(theme_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(theme_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_str().is_some_and(is_gtk3_subdir_name)
            && entry.path().join(THEME_STYLESHEET).is_file()
    })
}

/// Whether `name` is a GTK 3 version subdirectory name, `gtk-3.` followed by a
/// decimal minor version (`gtk-3.0`, `gtk-3.20`, …) — the shape of every directory
/// GTK 3 probes (see [`has_gtk3_stylesheet`]).
fn is_gtk3_subdir_name(name: &str) -> bool {
    name.strip_prefix("gtk-3.")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the installed theme directory `theme_dir` carries a GTK 4 stylesheet: a
/// regular file `gtk-4.0/gtk.css`, symlinks followed (task 9.30; R3.3).
///
/// GTK 4 itself also probes version-suffixed `gtk-4.<N>/` directories, but only
/// `gtk-4.0/` is checked here: it is the directory this desktop's GTK 4 setup links
/// from `~/.config/gtk-4.0` (task 11.16), and no theme installed on the target host
/// carries its GTK 4 stylesheet anywhere else. The predicate thus agrees with
/// [`resolve_gtk4_theme`] on what the GTK 4 stylesheet is — `gtk-4.0/gtk.css`, the
/// first of the entries that function links.
fn has_gtk4_stylesheet(theme_dir: &Path) -> bool {
    theme_dir
        .join(GTK4_THEME_SUBDIR)
        .join(THEME_STYLESHEET)
        .is_file()
}

/// Scans `dirs` for installed icon and cursor themes in a single pass (R3.4).
///
/// The two classifications are **independent** — a directory can be both — which is a
/// deliberate deviation from R3.4's literal "cursor = has `cursors/`, the rest are
/// icons" partition. Real icon themes routinely bundle a cursor set: the default GNOME
/// **Adwaita** icon theme (and Breeze/Oxygen) ship *both* an `index.theme` and a
/// `cursors/` subdirectory, so a mutually exclusive rule would drop Adwaita from the
/// icon drop-down entirely. Instead:
///
/// - a directory with a `cursors/` subdirectory is a **cursor** theme;
/// - a directory with an `index.theme` file **and real icon content** — at least one
///   subdirectory other than `cursors/` (icon themes always carry size/`scalable`
///   dirs) — is an **icon** theme.
///
/// The "real icon content" gate is what still keeps a *pure* cursor theme out of the
/// icon list: Bibata-style cursor packs ship an `index.theme` and a `cursors/` dir but
/// no size directories, so they classify as cursor-only. Both lists are de-duplicated
/// by name across the roots and returned sorted.
fn scan_icon_and_cursor_themes(dirs: &[PathBuf]) -> (Vec<String>, Vec<String>) {
    let mut icons = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // `metadata` follows symlinks, so a symlinked theme directory still counts.
            if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            // Independent classification (see the doc): a dir may be both a cursor
            // theme and an icon theme.
            if path.join("cursors").is_dir() {
                cursors.insert(name.clone());
            }
            if path.join("index.theme").is_file() && has_non_cursor_subdir(&path) {
                icons.insert(name);
            }
        }
    }
    (icons.into_iter().collect(), cursors.into_iter().collect())
}

/// Whether `path` has at least one subdirectory other than `cursors/` — the "real
/// icon content" signal that distinguishes an icon theme (which carries size dirs like
/// `48x48/` or `scalable/`) from a pure cursor pack (only `cursors/`).
///
/// `metadata` follows symlinks so a symlinked size directory counts. Any read failure
/// is treated as "no icon content" (the directory is then not an icon theme), which is
/// the safe default.
fn has_non_cursor_subdir(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str() == Some("cursors") {
            continue;
        }
        if std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()) {
            return true;
        }
    }
    false
}

/// Lists the candidate theme directories under each root as `(name, path)` pairs, in
/// root order and not de-duplicated — the same name on two roots yields two entries,
/// so a caller can test each copy.
///
/// Used by the GTK theme scan; `metadata` follows symlinks so a symlinked theme
/// directory counts, and dotfiles and non-directories are skipped. A root that cannot
/// be read contributes nothing.
fn theme_dir_entries(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                found.push((name, path));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities};
    use crate::core::freshness::FreshnessTracker;
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;
    use crate::testing::replace_once;

    /// A realistic `settings.ini` with all four theme keys the page edits.
    const SETTINGS_INI: &str = "\
[Settings]
gtk-theme-name=Everforest-Green-Dark
gtk-icon-theme-name=Everforest-Dark
gtk-cursor-theme-name=Nordic-cursors
gtk-cursor-theme-size=16
";

    /// A `hyprland.lua` excerpt: the two cursor `hl.env` lines the app owns, plus a
    /// top-level `hl.config` table the Lua editor could address but the app must never
    /// touch (the `set_env`-only caller contract, task 11.3).
    const HYPRLAND_LUA: &str = "\
-- Cursor env, kept identical to uwsm/env.
hl.env(\"XCURSOR_THEME\", \"Nordic-cursors\")
hl.env(\"XCURSOR_SIZE\", \"16\")

hl.config({
    general = {
        border_size = 2,
    },
})
";

    /// The XCursor fallback theme as the dotfiles ship it, pointing at the same cursor
    /// theme as the other fixture copies.
    const CURSOR_FALLBACK: &str = "\
[Icon Theme]
Name=Default
Comment=Default Cursor Theme
Inherits=Nordic-cursors
";

    /// A `uwsm/env` with the canonical cursor exports and a commented-out `GTK_THEME`.
    const UWSM_ENV: &str = "\
#export GTK_THEME=Nordic-bluish-accent
export XCURSOR_THEME=Nordic-cursors
export XCURSOR_SIZE=16
";

    /// The Hyprland-side cursor copy of a test's [`ThemesPaths`], which every fixture
    /// below sets (only the provider-guard test leaves it `None`).
    fn hypr_lua(paths: &ThemesPaths) -> &Path {
        paths
            .hyprland_lua
            .as_deref()
            .expect("the fixture sets the hyprland.lua path")
    }

    /// The XCursor fallback theme path of a test's [`ThemesPaths`], which every fixture
    /// below sets.
    fn fallback(paths: &ThemesPaths) -> &Path {
        paths
            .cursor_fallback_theme
            .as_deref()
            .expect("the fixture sets the index.theme path")
    }

    /// Backing paths under `config` (plus the fallback theme under `home/.icons`) that
    /// a test fills in itself.
    fn paths_under(config: &Path, home: &Path) -> ThemesPaths {
        ThemesPaths {
            gtk3_settings: config.join("gtk-3.0/settings.ini"),
            gtk4_settings: config.join("gtk-4.0/settings.ini"),
            hyprland_lua: Some(config.join("hypr/hyprland.lua")),
            uwsm_env: config.join("uwsm/env"),
            cursor_fallback_theme: Some(home.join(".icons/default/index.theme")),
            dotfiles_repo: None,
            home: Some(home.to_path_buf()),
        }
    }

    /// Writes the five backing files into `dir` (the fallback theme under
    /// `dir/home/.icons/default`) and returns their [`ThemesPaths`]. The `uwsm` text is
    /// supplied so a test can flip the `GTK_THEME` override on.
    fn write_backing_fixture(dir: &Path, uwsm: &str) -> ThemesPaths {
        let gtk3 = dir.join("gtk-3.0");
        let gtk4 = dir.join("gtk-4.0");
        let hypr = dir.join("hypr");
        let uwsm_dir = dir.join("uwsm");
        let icons_default = dir.join("home/.icons/default");
        for sub in [&gtk3, &gtk4, &hypr, &uwsm_dir, &icons_default] {
            fs::create_dir_all(sub).expect("create a config subdir");
        }
        let gtk3_settings = gtk3.join("settings.ini");
        let gtk4_settings = gtk4.join("settings.ini");
        let hyprland_lua = hypr.join("hyprland.lua");
        let uwsm_env = uwsm_dir.join("env");
        let cursor_fallback_theme = icons_default.join("index.theme");
        fs::write(&gtk3_settings, SETTINGS_INI).expect("write gtk-3.0 settings.ini");
        // gtk-4.0 carries a different layout but the same keys, to prove identical
        // writes regardless of surrounding formatting.
        fs::write(
            &gtk4_settings,
            "# gtk4\n[Settings]\ngtk-theme-name = Everforest-Green-Dark\ngtk-cursor-theme-name = Nordic-cursors\ngtk-cursor-theme-size = 16\n",
        )
        .expect("write gtk-4.0 settings.ini");
        fs::write(&hyprland_lua, HYPRLAND_LUA).expect("write hyprland.lua");
        fs::write(&uwsm_env, uwsm).expect("write uwsm/env");
        fs::write(&cursor_fallback_theme, CURSOR_FALLBACK).expect("write index.theme");
        ThemesPaths {
            gtk3_settings,
            gtk4_settings,
            hyprland_lua: Some(hyprland_lua),
            uwsm_env,
            cursor_fallback_theme: Some(cursor_fallback_theme),
            dotfiles_repo: None,
            home: Some(dir.join("home")),
        }
    }

    /// The paths of a contribution's writes, in order.
    fn written_paths(contribution: &ThemesApply) -> Vec<&Path> {
        contribution
            .writes
            .iter()
            .map(|write| write.path.as_path())
            .collect()
    }

    /// A loaded model over `paths` whose scanned icon roots hold the fixture's cursor themes.
    fn load_with_cursors(tmp: &Path, paths: &ThemesPaths) -> ThemesModel {
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp)],
        };
        ThemesModel::load(&roots, paths.clone(), false, None)
    }

    /// Writes an icon-dir fixture exercising the independent icon/cursor classification,
    /// returning the icon root:
    ///
    /// - `Papirus` — `index.theme` + a `scalable/` size dir, no cursors → icon only;
    /// - `Bibata` — only a `cursors/` dir → cursor only;
    /// - `Nordic-cursors` — `index.theme` + `cursors/` but no size dir (a pure cursor
    ///   pack that ships an `index.theme`) → cursor only;
    /// - `Adwaita` — `index.theme` + a `16x16/` size dir + `cursors/` (a real icon
    ///   theme that bundles cursors, like GNOME's default) → appears in **both** lists.
    fn write_icon_root(dir: &Path) -> PathBuf {
        let icons = dir.join("icons");
        let papirus = icons.join("Papirus");
        fs::create_dir_all(papirus.join("scalable")).expect("create Papirus size dir");
        fs::write(papirus.join("index.theme"), b"[Icon Theme]\n").expect("write Papirus index");
        fs::create_dir_all(icons.join("Bibata").join("cursors")).expect("create Bibata cursors");
        let nordic = icons.join("Nordic-cursors");
        fs::create_dir_all(nordic.join("cursors")).expect("create Nordic-cursors cursors");
        fs::write(nordic.join("index.theme"), b"[Icon Theme]\n").expect("write Nordic index");
        let adwaita = icons.join("Adwaita");
        fs::create_dir_all(adwaita.join("16x16")).expect("create Adwaita size dir");
        fs::create_dir_all(adwaita.join("cursors")).expect("create Adwaita cursors");
        fs::write(adwaita.join("index.theme"), b"[Icon Theme]\n").expect("write Adwaita index");
        icons
    }

    /// Installs a GTK theme the scan accepts: `<root>/<theme>/<subdir>/gtk.css`, where
    /// `subdir` names the GTK version it styles (`gtk-3.0`, `gtk-3.24`, `gtk-4.0`).
    /// Since task 9.30 a bare `gtk-*` directory is no longer a theme, so every fixture
    /// that needs a theme offered goes through here.
    fn install_gtk_theme(root: &Path, theme: &str, subdir: &str) {
        let dir = root.join(theme).join(subdir);
        fs::create_dir_all(&dir).expect("create the theme's version subdirectory");
        fs::write(dir.join("gtk.css"), format!("/* {theme} {subdir} */\n"))
            .expect("write the theme stylesheet");
    }

    #[test]
    fn scan_finds_gtk_themes_in_fixture_roots() {
        // Accept criterion (R3.3): the theme scan unit-tested against a fixture tree
        // with injectable roots. A GTK theme is a dir whose gtk-3.<N>/ or gtk-4.0/ carries
        // a gtk.css (task 9.30).
        let tmp = tempfile::tempdir().expect("temp dir");
        let themes = tmp.path().join("themes");
        install_gtk_theme(&themes, "Everforest-Green-Dark", "gtk-4.0");
        // Named Arc rather than Adwaita so this still proves the gtk-3.0 branch found it:
        // the built-in variants are merged in regardless of what the scan returns.
        install_gtk_theme(&themes, "Arc", "gtk-3.0");
        fs::create_dir_all(themes.join("NotATheme")).unwrap(); // no gtk-*/ -> skipped
        install_gtk_theme(&themes, ".hidden", "gtk-4.0"); // dotfile skipped

        let gtk = scan_gtk_themes(std::slice::from_ref(&themes)).offered;
        assert_eq!(
            gtk,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Arc".to_string(),
                "Everforest-Green-Dark".to_string()
            ],
            "GTK themes are the dirs whose gtk-3.<N>/ or gtk-4.0/ carries a gtk.css, plus the \
             built-ins, sorted; \
             dotfiles/non-themes skipped"
        );
    }

    #[test]
    fn scan_classifies_icon_and_cursor_themes_in_fixture_roots() {
        // Accept criterion (R3.3/R3.4): the icon/cursor scan unit-tested against a
        // fixture tree with injectable roots. A cursor theme has a cursors/ subdir; an
        // icon theme has index.theme and real icon content.
        let tmp = tempfile::tempdir().expect("temp dir");
        let icons = write_icon_root(tmp.path());
        let (icon_themes, cursor_themes) =
            scan_icon_and_cursor_themes(std::slice::from_ref(&icons));
        assert_eq!(
            icon_themes,
            vec!["Adwaita".to_string(), "Papirus".to_string()],
            "a dir with index.theme AND real icon content is an icon theme — including \
             Adwaita, which also ships cursors (the independent-classification deviation)"
        );
        assert_eq!(
            cursor_themes,
            vec![
                "Adwaita".to_string(),
                "Bibata".to_string(),
                "Nordic-cursors".to_string()
            ],
            "a cursors/ subdir marks a cursor theme; Adwaita is in both lists, and a pure \
             cursor pack with an index.theme (Nordic-cursors) stays cursor-only"
        );
    }

    #[test]
    fn scan_dedups_names_across_multiple_roots() {
        // A theme of the same name in two roots (a user override shadowing a system
        // theme) appears once — only the name matters, since that is what
        // gsettings/settings.ini store.
        let tmp = tempfile::tempdir().expect("temp dir");
        let system = tmp.path().join("system");
        let user = tmp.path().join("user");
        // Deliberately not named Adwaita: the built-in variants are merged into every
        // result, so a fixture using those names would still pass even if the directory
        // scan returned nothing at all.
        install_gtk_theme(&system, "Nordic", "gtk-4.0");
        install_gtk_theme(&user, "Nordic", "gtk-4.0");
        let gtk = scan_gtk_themes(&[user, system]).offered;
        assert_eq!(
            gtk,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Nordic".to_string()
            ],
            "the duplicate name collapses to one entry alongside the built-in variants"
        );
    }

    #[test]
    fn builtin_gtk_themes_are_offered_without_a_theme_dir() {
        // R3.3 requires the built-in Adwaita variants alongside the installed theme
        // dirs. GTK carries their stylesheets in the library, so a system can offer
        // Adwaita with no /usr/share/themes/Adwaita directory at all — a directory scan
        // alone would silently drop it (task 9.7).
        let tmp = tempfile::tempdir().expect("temp dir");

        // A root with an unrelated theme and deliberately no Adwaita directory.
        let without = tmp.path().join("without-adwaita");
        install_gtk_theme(&without, "Nordic", "gtk-3.0");
        assert_eq!(
            scan_gtk_themes(std::slice::from_ref(&without)).offered,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Nordic".to_string()
            ],
            "the built-in variants are offered even though no Adwaita dir exists, merged \
             into the scan's single sort order"
        );
    }

    #[test]
    fn a_scanned_adwaita_dir_does_not_duplicate_the_builtin() {
        // The built-in Adwaita variants are merged into the scan (task 9.7), so a root
        // that also has them on disk (e.g. gnome-themes-extra installing both variants)
        // must still yield each name once, not twice.
        let tmp = tempfile::tempdir().expect("temp dir");
        let without = tmp.path().join("without-adwaita");
        install_gtk_theme(&without, "Nordic", "gtk-3.0");
        let with = tmp.path().join("with-adwaita");
        install_gtk_theme(&with, "Adwaita", "gtk-3.0");
        install_gtk_theme(&with, "Adwaita-dark", "gtk-3.0");
        let scanned = scan_gtk_themes(&[with, without]).offered;
        assert_eq!(
            scanned,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Nordic".to_string()
            ],
            "a scanned Adwaita dir collapses with the built-in entry of the same name"
        );
    }

    #[test]
    fn a_guarded_off_hyprland_copy_gets_no_cursor_write() {
        // Task 11.2 (analysis §8.6): on a host whose compositor does not run the Lua
        // config provider, the startup loader passes no Hyprland-side path. A cursor
        // change then writes the other copies only, and hyprland.lua is untouched.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let mut paths = write_backing_fixture(&config, UWSM_ENV);
        let hyprland_lua = hypr_lua(&paths).to_path_buf();
        paths.hyprland_lua = None;

        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("a cursor change still contributes the other writes");
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                paths.uwsm_env.as_path(),
                fallback(&paths),
            ],
            "both settings.ini, uwsm/env and index.theme are written, the Hyprland copy is not"
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.backing != BackingFile::HyprlandLua),
            "no write is attributed to the Hyprland config"
        );

        model.commit();
        assert!(
            !model.is_dirty(),
            "the change is promoted from the remaining copies"
        );
        assert_eq!(
            fs::read_to_string(&hyprland_lua).unwrap(),
            HYPRLAND_LUA,
            "the guarded-off Hyprland file is never touched"
        );
        assert_eq!(
            model.cursor_login_note().as_deref(),
            Some(
                "A cursor change shows immediately; its session environment copy \
                 (uwsm/env) takes effect at the next login."
            ),
            "the login note names only the env copy this host writes"
        );
    }

    #[test]
    fn a_cursor_apply_writes_five_identical_copies() {
        // Accept criterion: a cursor apply writes BOTH settings.ini files, both env
        // copies (uwsm/env and hyprland.lua's hl.env lines) AND the XCursor fallback
        // theme to the SAME value, and drives the exact gsettings/hyprctl setcursor
        // command sequence through the apply pipeline (R3.4, task 11.5).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        model
            .stage_cursor_size("24")
            .expect("the value passes the stage-time check");
        assert!(model.is_dirty());

        let contribution = model
            .apply_contribution()
            .expect("a cursor change contributes writes");
        // All five copies are written, in the model's fixed order.
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                hypr_lua(&paths),
                paths.uwsm_env.as_path(),
                fallback(&paths),
            ],
            "a cursor change writes both settings.ini, hyprland.lua, uwsm/env and index.theme"
        );
        assert_eq!(
            contribution.reload_params.cursor,
            Some(CursorValue {
                theme: "Bibata".to_string(),
                size: 24
            }),
            "the reload carries the new cursor theme+size"
        );
        assert!(contribution.reload_params.gtk_theme.is_none());
        assert!(contribution.reload_params.icon_theme.is_none());

        // Task 9.27: the contribution carries both staged cursor values for step 1, and
        // each copy declares exactly the settings it carries — asserted here because this
        // is the test that renders the hyprland.lua, uwsm/env and index.theme copies, so
        // a site that declared the wrong settings would make the plan-drift guard warn
        // on every cursor change (or miss a dropped validation).
        assert_eq!(
            contribution.validations,
            vec![
                (SettingId::CursorTheme, Value::Enum("Bibata".to_string())),
                (SettingId::CursorSize, Value::Enum("24".to_string())),
            ]
        );
        let both = vec![SettingId::CursorTheme, SettingId::CursorSize];
        let declared: Vec<&[SettingId]> = contribution
            .writes
            .iter()
            .map(|write| write.validated_by.as_slice())
            .collect();
        assert_eq!(
            declared,
            vec![
                both.as_slice(),
                both.as_slice(),
                both.as_slice(),
                both.as_slice(),
                &[SettingId::CursorTheme],
            ],
            "settings.ini ×2, hyprland.lua and uwsm/env carry both; index.theme the theme"
        );

        // Run the writes + reloads through the real pipeline.
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        // The themes files are not in the store's tracker (they are conflict-checked by
        // the model), so an empty tracker is correct here.
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true);

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the cursor apply must succeed, got {outcome:?}"
        );

        // The exact reload sequence: `hyprctl reload config-only` (from
        // hyprland.lua; task 11.11) then the cursor gsettings keys then setcursor,
        // deduped across the five files.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").arg("reload").arg("config-only"),
                Command::new("gsettings").args([
                    "set",
                    "org.gnome.desktop.interface",
                    "cursor-theme",
                    "Bibata",
                ]),
                Command::new("gsettings").args([
                    "set",
                    "org.gnome.desktop.interface",
                    "cursor-size",
                    "24",
                ]),
                Command::new("hyprctl").args(["setcursor", "Bibata", "24"]),
            ]
        );

        // Every copy on disk now holds the identical new value.
        let gtk3 = fs::read_to_string(&paths.gtk3_settings).unwrap();
        let gtk4 = fs::read_to_string(&paths.gtk4_settings).unwrap();
        let uwsm = fs::read_to_string(&paths.uwsm_env).unwrap();
        assert!(gtk3.contains("gtk-cursor-theme-name=Bibata"));
        assert!(gtk3.contains("gtk-cursor-theme-size=24"));
        assert!(gtk4.contains("gtk-cursor-theme-name = Bibata"));
        assert!(gtk4.contains("gtk-cursor-theme-size = 24"));
        assert!(uwsm.contains("export XCURSOR_THEME=Bibata"));
        assert!(uwsm.contains("export XCURSOR_SIZE=24"));
        // hyprland.lua: exactly the two hl.env lines change, the size as a Lua string,
        // and the addressable hl.config table stays byte-identical.
        let expected_lua = replace_once(
            &replace_once(
                HYPRLAND_LUA,
                "hl.env(\"XCURSOR_THEME\", \"Nordic-cursors\")",
                "hl.env(\"XCURSOR_THEME\", \"Bibata\")",
            ),
            "hl.env(\"XCURSOR_SIZE\", \"16\")",
            "hl.env(\"XCURSOR_SIZE\", \"24\")",
        );
        assert_eq!(fs::read_to_string(hypr_lua(&paths)).unwrap(), expected_lua);
        // index.theme: only the Inherits= value (it carries no size).
        assert_eq!(
            fs::read_to_string(fallback(&paths)).unwrap(),
            replace_once(
                CURSOR_FALLBACK,
                "Inherits=Nordic-cursors",
                "Inherits=Bibata"
            )
        );
        // The model promotes the change, and every copy now agrees.
        model.commit();
        assert!(!model.is_dirty());
        assert_eq!(model.disagreeing_cursor_copies(), Vec::new());
    }

    #[test]
    fn a_gtk_theme_apply_writes_only_the_settings_ini_pair() {
        // A GTK theme change touches only the two settings.ini files (not the env
        // files) and reloads with just `gsettings set … gtk-theme`.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.path().join("themes");
        install_gtk_theme(&themes, "Adwaita", "gtk-4.0");
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");

        let contribution = model.apply_contribution().expect("a GTK theme write");
        assert_eq!(
            contribution.writes.len(),
            2,
            "only the two settings.ini files"
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.backing == BackingFile::GtkSettings)
        );
        // Task 9.27: the staged name is handed to step 1, and both writes declare it.
        assert_eq!(
            contribution.validations,
            vec![(SettingId::GtkTheme, Value::Enum("Adwaita".to_string()))]
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validated_by == [SettingId::GtkTheme]),
            "each settings.ini write declares the GTK theme it carries"
        );
        assert_eq!(
            contribution.reload_params.gtk_theme,
            Some("Adwaita".to_string())
        );
        assert!(contribution.reload_params.cursor.is_none());

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
        let outcome = apply::run(&plan, &FreshnessTracker::new(), &caps, &runner, &signaller);
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        assert_eq!(
            runner.recorded(),
            vec![Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "gtk-theme",
                "Adwaita",
            ])],
            "a GTK theme change reloads with only the gtk-theme gsettings key"
        );
        assert!(
            fs::read_to_string(&paths.gtk3_settings)
                .unwrap()
                .contains("gtk-theme-name=Adwaita")
        );
        assert!(
            fs::read_to_string(&paths.gtk4_settings)
                .unwrap()
                .contains("gtk-theme-name = Adwaita")
        );
    }

    #[test]
    fn an_icon_theme_apply_writes_both_settings_ini() {
        // The icon theme's sibling of the GTK-theme test above (task 9.12). Like the GTK
        // theme, the icon theme is declared only in the two settings.ini files — the env
        // files carry cursor copies only — so an icon change must leave both settings.ini
        // copies holding the identical value (R3.4) and take live effect through exactly
        // one `gsettings set … icon-theme` (R2.2/R3.4).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };
        // Captured before the apply, so the expected post-apply bytes below can be built
        // from the originals: the surgical-edit contract (§3) says everything outside the
        // edited span stays byte-identical, so the expectation *is* the original patched.
        let gtk3_before = fs::read_to_string(&paths.gtk3_settings).expect("read gtk-3.0 original");
        let gtk4_before = fs::read_to_string(&paths.gtk4_settings).expect("read gtk-4.0 original");

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        // The configured icon theme is not one the fixture roots offer, so it is prepended
        // to the scanned names — the drop-down must always be able to preselect the
        // value the config actually holds.
        assert_eq!(
            model.icon_themes(),
            vec![
                "Everforest-Dark".to_string(),
                "Adwaita".to_string(),
                "Papirus".to_string()
            ],
            "the icon drop-down offers the scanned themes plus the configured value"
        );
        assert_eq!(
            model.selected_icon_index(),
            Some(0),
            "before any edit the drop-down preselects settings.ini's gtk-icon-theme-name"
        );

        model
            .stage_icon_theme("Papirus")
            .expect("the value passes the stage-time check");
        assert!(model.is_dirty());
        assert_eq!(
            model.selected_icon_index(),
            Some(2),
            "the drop-down shows the staged selection while the change is pending"
        );

        let contribution = model
            .apply_contribution()
            .expect("an icon theme change contributes writes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "only the two settings.ini files carry the icon theme"
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.backing == BackingFile::GtkSettings)
        );
        // Task 9.27: the staged name is handed to step 1, and both writes declare it.
        assert_eq!(
            contribution.validations,
            vec![(SettingId::IconTheme, Value::Enum("Papirus".to_string()))]
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validated_by == [SettingId::IconTheme]),
            "each settings.ini write declares the icon theme it carries"
        );
        assert_eq!(
            contribution.reload_params.icon_theme,
            Some("Papirus".to_string()),
            "the reload carries the new icon theme name"
        );
        assert!(contribution.reload_params.gtk_theme.is_none());
        assert!(contribution.reload_params.cursor.is_none());

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        // Only gsettings, since that is all an icon change needs. Note what this does and
        // does not prove: reload actions are capability-filtered, so an absent hyprctl
        // would *hide* a spurious `hyprctl setcursor` rather than fail the test. The
        // reload_params assertions above are what rule that out — they show the
        // contribution never asked for a cursor or GTK-theme reload in the first place.
        let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
        let outcome = apply::run(&plan, &FreshnessTracker::new(), &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the icon apply must succeed, got {outcome:?}"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "icon-theme",
                "Papirus",
            ])],
            "an icon theme change reloads with only the icon-theme gsettings key"
        );

        // The gtk-3.0 copy holds the icon key, so it is patched in place: its original
        // bytes with only that value span rewritten — every other key, the comments and
        // the ordering byte-identical.
        assert_eq!(
            fs::read_to_string(&paths.gtk3_settings).expect("read the applied gtk-3.0 file"),
            replace_once(
                &gtk3_before,
                "gtk-icon-theme-name=Everforest-Dark",
                "gtk-icon-theme-name=Papirus",
            ),
            "gtk-3.0/settings.ini: only the icon theme value may change"
        );
        // The gtk-4.0 fixture deliberately has no icon key — the case a writer that only
        // rewrites existing value spans would silently skip, leaving GTK 4 applications on
        // the old icon theme while GTK 3 ones moved (exactly the R3.4 desync the app must
        // not create). The INI writer appends the key instead, at the end of the
        // `[Settings]` body and in that file's own `key = value` separator style, so both
        // copies end up holding the identical value.
        assert_eq!(
            fs::read_to_string(&paths.gtk4_settings).expect("read the applied gtk-4.0 file"),
            format!("{gtk4_before}gtk-icon-theme-name = Papirus\n"),
            "gtk-4.0/settings.ini: the absent icon key is appended, not skipped"
        );

        // Task 9.6: the value reached both files, so commit may promote it — the page goes
        // clean and the drop-down's baseline becomes the applied theme. (A value no file
        // accepted stays staged instead; that path is covered by
        // `a_skipped_settings_ini_key_leaves_only_that_selection_dirty`.)
        model.commit();
        assert!(
            !model.is_dirty(),
            "a fully written icon change leaves the page clean"
        );
        let selected = model
            .selected_icon_index()
            .and_then(|index| model.icon_themes().get(index))
            .map(String::as_str);
        assert_eq!(
            selected,
            Some("Papirus"),
            "commit promotes the applied icon theme to the current value"
        );
    }

    #[test]
    fn a_second_apply_builds_on_the_bytes_the_first_one_wrote() {
        // The post-commit bookkeeping both Theme models share has two jobs:
        // re-baselining freshness, so the app's own write is not later read as an external
        // conflict (covered by `a_committed_theme_apply_is_not_a_self_conflict`), and
        // refreshing the in-memory backing text, so the *next* edit re-parses the bytes
        // now on disk. This covers the second job, which no other test pinned down: a
        // model still holding its pre-Apply text would render the next write from it and
        // silently revert the first change — the applied value is no longer staged, so
        // nothing would put it back into the second write.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };
        // Only gsettings, so the icon reload has a runner to reach; the cursor reload of
        // the second apply is capability-filtered away, which this test does not care
        // about — it is about the bytes, not the reloads.
        let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
        let signaller = MockProcessSignaller::new();
        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);

        // First apply: the icon theme, which lives only in the two settings.ini files.
        model
            .stage_icon_theme("Papirus")
            .expect("the value passes the stage-time check");
        let first = model
            .apply_contribution()
            .expect("an icon theme change contributes writes");
        let outcome = apply::run(
            &ApplyPlan {
                validations: Vec::new(),
                writes: first.writes,
                links: Vec::new(),
                palette: None,
                reload_params: first.reload_params,
            },
            &FreshnessTracker::new(),
            &caps,
            &MockCommandRunner::new(),
            &signaller,
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the first apply must succeed, got {outcome:?}"
        );
        model.commit();

        // Second apply: the cursor size — a different key in the same files. Its
        // gtk-3.0 write must carry the icon theme the first apply wrote, which only the
        // refreshed in-memory text can supply.
        model
            .stage_cursor_size("32")
            .expect("the value passes the stage-time check");
        let second = model
            .apply_contribution()
            .expect("a cursor size change contributes writes");
        let expected_gtk3 = replace_once(
            &replace_once(
                SETTINGS_INI,
                "gtk-icon-theme-name=Everforest-Dark",
                "gtk-icon-theme-name=Papirus",
            ),
            "gtk-cursor-theme-size=16",
            "gtk-cursor-theme-size=32",
        );
        let rendered_gtk3 = second
            .writes
            .iter()
            .find(|write| write.path == paths.gtk3_settings)
            .map(|write| String::from_utf8(write.contents.clone()).expect("the write is text"))
            .expect("the cursor size is written to gtk-3.0/settings.ini");
        assert_eq!(
            rendered_gtk3, expected_gtk3,
            "the second write carries both the applied icon theme and the new cursor size"
        );

        // And through the pipeline, so the file on disk really ends up holding both.
        let outcome = apply::run(
            &ApplyPlan {
                validations: Vec::new(),
                writes: second.writes,
                links: Vec::new(),
                palette: None,
                reload_params: second.reload_params,
            },
            &FreshnessTracker::new(),
            &caps,
            &MockCommandRunner::new(),
            &signaller,
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the second apply must succeed, got {outcome:?}"
        );
        model.commit();
        assert_eq!(
            fs::read_to_string(&paths.gtk3_settings).expect("read the twice-applied gtk-3.0 file"),
            expected_gtk3,
            "two applies in a row accumulate: neither change is lost"
        );
        assert!(
            !model.is_dirty(),
            "both values reached every file that carries them, so the page is clean"
        );
    }

    #[test]
    fn resolve_gtk_override_prefers_app_env_then_uwsm_then_none() {
        // Pure/headless decision (R3.3): the app's own environment wins; an empty value
        // is not an override; a commented-out uwsm line is not an override.
        assert_eq!(
            resolve_gtk_override(Some("Foo".to_string()), None),
            Some(GtkOverrideOrigin::AppEnvironment("Foo".to_string()))
        );
        assert_eq!(
            resolve_gtk_override(
                Some("Foo".to_string()),
                Some(&GtkThemeOverride::Active {
                    value: "Bar".to_string()
                })
            ),
            Some(GtkOverrideOrigin::AppEnvironment("Foo".to_string())),
            "the app environment takes precedence over uwsm/env"
        );
        assert_eq!(
            resolve_gtk_override(
                Some(String::new()),
                Some(&GtkThemeOverride::Active {
                    value: "Bar".to_string()
                })
            ),
            Some(GtkOverrideOrigin::UwsmEnv("Bar".to_string())),
            "an empty app-env value falls through to uwsm/env"
        );
        assert_eq!(
            resolve_gtk_override(
                None,
                Some(&GtkThemeOverride::Commented {
                    value: "Bar".to_string()
                })
            ),
            None,
            "a commented-out uwsm GTK_THEME is not an override"
        );
        assert_eq!(resolve_gtk_override(None, None), None);
    }

    #[test]
    fn an_app_env_override_locks_the_gtk_theme() {
        // Accept criterion (R3.3): a GTK_THEME set in the app's own environment shows a
        // banner and disables the GTK-theme drop-down; the app must not fight it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(
            &roots,
            paths,
            false,
            Some("Nordic-bluish-accent".to_string()),
        );
        assert!(model.gtk_dropdown_disabled());
        let source = model.gtk_override().expect("override present");
        assert!(matches!(source, GtkOverrideOrigin::AppEnvironment(_)));
        assert!(
            source.banner_message().contains("Nordic-bluish-accent"),
            "the banner names the override value"
        );
        // A GTK theme edit under the override is ignored, so the page stays clean.
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        assert!(
            !model.is_dirty(),
            "a GTK theme edit must not stage while a GTK_THEME override is active"
        );
    }

    #[test]
    fn an_active_uwsm_gtk_theme_disables_the_gtk_dropdown() {
        // R3.3: an uncommented GTK_THEME in uwsm/env (with none in the app env) is a
        // live override too, from the uwsm source.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let uwsm = "export GTK_THEME=Nordic\nexport XCURSOR_THEME=Nordic-cursors\nexport XCURSOR_SIZE=16\n";
        let paths = write_backing_fixture(&config, uwsm);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };

        let model = ThemesModel::load(&roots, paths, false, None);
        assert!(model.gtk_dropdown_disabled());
        assert_eq!(
            model.gtk_override(),
            Some(&GtkOverrideOrigin::UwsmEnv("Nordic".to_string()))
        );
    }

    #[test]
    fn live_restyle_claim_follows_the_settings_portal() {
        // Accept criterion (R2.2): the live-restyle claim is gated on the settings
        // portal; without it the UI must say "next launch" instead.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };
        assert!(ThemesModel::load(&roots, paths.clone(), true, None).live_restyle());
        assert!(!ThemesModel::load(&roots, paths, false, None).live_restyle());
    }

    #[test]
    fn a_missing_settings_ini_hides_the_theme_rows() {
        // R4.4: with no settings.ini readable there is nothing to preselect or write,
        // so the rows are hidden and nothing can be applied.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = paths_under(tmp.path(), tmp.path());
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };
        let model = ThemesModel::load(&roots, paths, false, None);
        assert!(!model.themes_editable(), "no settings.ini -> rows hidden");
        assert!(!model.is_dirty());
        assert!(model.apply_contribution().is_none());
    }

    /// Loads a [`ThemesModel`] whose settings.ini selects Everforest-Green-Dark and whose
    /// theme root also offers Adwaita, for the staging-lifecycle tests below.
    fn two_gtk_themes_model(tmp: &Path) -> ThemesModel {
        let config = tmp.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.join("themes");
        install_gtk_theme(&themes, "Adwaita", "gtk-4.0");
        install_gtk_theme(&themes, "Everforest-Green-Dark", "gtk-4.0");
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };
        ThemesModel::load(&roots, paths, false, None)
    }

    #[test]
    fn reselecting_the_current_gtk_theme_is_not_dirty() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = two_gtk_themes_model(tmp.path());
        // The current GTK theme is Everforest-Green-Dark (from settings.ini).
        model
            .stage_gtk_theme("Everforest-Green-Dark")
            .expect("the value passes the stage-time check");
        assert!(
            !model.is_dirty(),
            "re-selecting the current value is not dirty"
        );
    }

    #[test]
    fn reset_discards_a_staged_gtk_theme() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = two_gtk_themes_model(tmp.path());
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending change");
    }

    #[test]
    fn commit_promotes_a_staged_gtk_theme() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = two_gtk_themes_model(tmp.path());
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        model.commit();
        assert!(!model.is_dirty(), "commit clears the dirty state");
        let selected = model
            .selected_gtk_index()
            .and_then(|index| model.gtk_themes().get(index))
            .map(String::as_str);
        assert_eq!(
            selected,
            Some("Adwaita"),
            "commit promotes the staged theme to the current value"
        );
    }

    #[test]
    fn an_external_edit_conflicts_until_a_reload() {
        // R5.6: an external edit to a backing file is detected as a conflict, and
        // reload re-baselines against the current bytes.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };

        let model = ThemesModel::load(&roots, paths.clone(), false, None);
        assert!(
            !model.check_conflict(),
            "unchanged files are not a conflict"
        );

        fs::write(&paths.gtk3_settings, b"[Settings]\ngtk-theme-name=Hacked\n")
            .expect("external edit");
        assert!(
            model.check_conflict(),
            "an external edit since load must be a conflict"
        );

        let reloaded = model.reload();
        assert!(
            !reloaded.check_conflict(),
            "reload re-baselines against the current bytes"
        );
    }

    #[test]
    fn cursor_preselect_falls_back_to_uwsm_env() {
        // N2(a): when settings.ini does not carry the cursor keys, the cursor theme and
        // size are preselected from uwsm/env's XCURSOR_* instead.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        for sub in ["gtk-3.0", "gtk-4.0", "hypr", "uwsm"] {
            fs::create_dir_all(config.join(sub)).unwrap();
        }
        let paths = paths_under(&config, tmp.path());
        // settings.ini has a GTK theme but no cursor keys.
        fs::write(
            &paths.gtk3_settings,
            "[Settings]\ngtk-theme-name=Everforest-Green-Dark\n",
        )
        .unwrap();
        fs::write(
            &paths.gtk4_settings,
            "[Settings]\ngtk-theme-name=Everforest-Green-Dark\n",
        )
        .unwrap();
        fs::write(hypr_lua(&paths), HYPRLAND_LUA).unwrap();
        fs::write(&paths.uwsm_env, UWSM_ENV).unwrap(); // XCURSOR_THEME=Nordic-cursors, SIZE=16
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        };

        let model = ThemesModel::load(&roots, paths, false, None);
        let cursor = model
            .selected_cursor_index()
            .and_then(|index| model.cursor_themes().get(index))
            .map(String::as_str);
        assert_eq!(
            cursor,
            Some("Nordic-cursors"),
            "cursor theme preselect falls back to uwsm/env's XCURSOR_THEME"
        );
        let size = model
            .selected_cursor_size_index()
            .and_then(|index| model.cursor_sizes().get(index))
            .map(String::as_str);
        assert_eq!(
            size,
            Some("16"),
            "cursor size preselect falls back to uwsm/env's XCURSOR_SIZE"
        );
    }

    #[test]
    fn a_committed_theme_apply_is_not_a_self_conflict() {
        // N2(b): end-to-end conflict re-baseline (mirrors the store's
        // `a_second_apply_after_commit_is_not_a_self_conflict`). The pipeline writes the
        // backing files; before commit the model's load-time baseline sees them as
        // changed, and after commit the app's own write is no longer a conflict (R5.6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };

        let mut model = ThemesModel::load(&roots, paths, false, None);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        model
            .stage_cursor_size("24")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("a cursor change contributes");

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        // The store's tracker does not track the theme files (the model does), so an
        // empty tracker is correct for the pipeline's own conflict check here.
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true);
        assert!(matches!(
            apply::run(&plan, &tracker, &caps, &runner, &signaller),
            ApplyOutcome::Applied { .. }
        ));

        assert!(
            model.check_conflict(),
            "before commit, the on-disk write differs from the load-time baseline"
        );
        model.commit();
        assert!(
            !model.check_conflict(),
            "after commit the app's own write is not a self-conflict (R5.6)"
        );
    }

    #[test]
    fn a_non_numeric_cursor_size_skips_the_live_reload() {
        // N2(c): an unparseable on-disk cursor size degrades gracefully — a cursor
        // theme change is still written to the files, but the live cursor reload
        // (`gsettings set cursor-*` + `hyprctl setcursor`) is skipped because
        // `setcursor` needs a numeric size.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        for sub in ["gtk-3.0", "gtk-4.0", "hypr", "uwsm"] {
            fs::create_dir_all(config.join(sub)).unwrap();
        }
        let paths = paths_under(&config, tmp.path());
        let ini = "[Settings]\ngtk-cursor-theme-name=Nordic-cursors\ngtk-cursor-theme-size=big\n";
        fs::write(&paths.gtk3_settings, ini).unwrap();
        fs::write(&paths.gtk4_settings, ini).unwrap();
        fs::write(hypr_lua(&paths), HYPRLAND_LUA).unwrap();
        fs::write(&paths.uwsm_env, UWSM_ENV).unwrap();
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };

        let mut model = ThemesModel::load(&roots, paths, false, None);
        // Change only the cursor theme; the size stays the garbage on-disk value.
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("a cursor theme change contributes writes");
        assert!(
            !contribution.writes.is_empty(),
            "the cursor theme is still written to the backing files"
        );
        assert!(
            contribution.reload_params.cursor.is_none(),
            "a non-numeric cursor size skips the live cursor reload (setcursor needs a number)"
        );
    }

    #[test]
    fn a_missing_hyprland_env_field_still_writes_the_present_one() {
        // N1: with only one cursor hl.env line present, the present field is still
        // written to hyprland.lua rather than the whole file being abandoned — so the
        // copies do not drift out of step. The missing line is never invented.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        fs::write(
            hypr_lua(&paths),
            "hl.env(\"XCURSOR_THEME\", \"Nordic-cursors\")\n",
        )
        .unwrap();

        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        model
            .stage_cursor_size("24")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("a cursor change contributes");

        let hypr_write = contribution
            .writes
            .iter()
            .find(|write| write.path == hypr_lua(&paths))
            .expect("hyprland.lua is still written despite the missing XCURSOR_SIZE line");
        assert_eq!(
            String::from_utf8(hypr_write.contents.clone()).unwrap(),
            "hl.env(\"XCURSOR_THEME\", \"Bibata\")\n",
            "the present theme field is written and no size line is fabricated"
        );
        assert_eq!(
            hypr_write.changed_keys,
            vec!["cursor theme (hyprland.lua hl.env)".to_string()],
            "only the present field is recorded for the hyprland.lua write"
        );
    }

    #[test]
    fn a_skipped_settings_ini_key_leaves_only_that_selection_dirty() {
        // Task 9.6 accept criterion: a failed render leaves the affected selection dirty.
        // A theme name containing a newline is a legal directory name on Linux (only `/`
        // and NUL are forbidden), so the scan can surface one — but the settings.ini
        // writer rejects it, because writing it would split the `key=value` line. That
        // one key is skipped while the other is still written, and commit must promote
        // only the key that reached a file: promoting the skipped one would leave the
        // model showing a value no file holds, and since it would no longer look changed,
        // no later Apply would write it either.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.path().join("themes");
        install_gtk_theme(&themes, "Adwaita", "gtk-4.0");
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(&roots, paths, false, None);
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        // Since task 9.27 the stage-time check refuses this name, so the test seam puts
        // it in place to keep the writer's own guard (defence in depth) covered.
        model.stage_unchecked_for_tests(SettingId::IconTheme, "Papirus\ngtk-theme-name=Injected");

        let contribution = model
            .apply_contribution()
            .expect("the writable GTK theme change still contributes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "both settings.ini copies are written (so the loop below is not vacuous)"
        );
        for write in &contribution.writes {
            assert_eq!(
                write.changed_keys,
                vec!["GTK theme".to_string()],
                "the rejected icon value is skipped; only the GTK theme is written"
            );
            assert!(
                !String::from_utf8(write.contents.clone())
                    .expect("settings.ini stays UTF-8")
                    .contains("Injected"),
                "the rejected value never reaches the file contents"
            );
        }

        model.commit();
        let selected_gtk = model
            .selected_gtk_index()
            .and_then(|index| model.gtk_themes().get(index))
            .map(String::as_str);
        assert_eq!(
            selected_gtk,
            Some("Adwaita"),
            "the GTK theme reached both settings.ini files, so it is promoted"
        );
        assert_eq!(
            model.icon_theme.original(),
            Some("Everforest-Dark"),
            "the icon baseline still matches what is on disk"
        );
        assert!(
            model.icon_theme.is_dirty(),
            "the skipped icon selection stays staged"
        );
        assert!(
            model.is_dirty(),
            "the page stays dirty, so the user sees the change is not applied and can retry"
        );

        // The retry renders nothing at all (the rejected value is the only dirty one), so
        // the contribution carries no write — only the refusal, which is what tells the
        // user why the Apply changes nothing (task 9.28) — and a commit in that state
        // must still promote nothing rather than silently re-baselining.
        let retry = model
            .apply_contribution()
            .expect("a refused value still contributes its refusal");
        assert!(
            retry.writes.is_empty(),
            "with only the rejected value staged, no file can be written"
        );
        assert_eq!(
            retry
                .skipped
                .iter()
                .map(|skip| (skip.value, skip.file))
                .collect::<Vec<_>>(),
            vec![
                ("Icon theme", "gtk-3.0/settings.ini"),
                ("Icon theme", "gtk-4.0/settings.ini"),
            ],
            "each settings.ini copy that refused the value is named"
        );
        model.commit();
        assert!(
            model.icon_theme.is_dirty() && model.is_dirty(),
            "a commit with no rendered write promotes nothing"
        );
    }

    #[test]
    fn a_value_one_existing_copy_refuses_is_not_promoted() {
        // Task 9.6, the divergence rule (R3.4): the writers do not share a value rule —
        // the Lua writer rejects `"` (it would end the string literal and break
        // hyprland.lua) and the uwsm/env writer rejects it as not a bare shell word,
        // while the settings.ini and index.theme writers accept it. A cursor theme
        // directory named `Nord"ic` is legal on Linux and the scan offers it, so the
        // cursor theme can reach three of its five copies while the other two keep the
        // old one. The copies on disk then genuinely differ, so the change must NOT be
        // promoted: the page stays dirty rather than looking applied while copies lag
        // behind.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let icons = write_icon_root(tmp.path());
        fs::create_dir_all(icons.join("Nord\"ic").join("cursors"))
            .expect("create a cursor theme dir whose name contains a quote");
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![icons],
        };

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        assert!(
            model.cursor_themes().contains(&"Nord\"ic".to_string()),
            "the scan surfaces the quote-named cursor theme, so the drop-down offers it"
        );
        // Since task 9.27 `stage_cursor_theme` refuses this name (see
        // `a_quote_in_a_cursor_theme_is_refused_at_stage_time`), so the test seam puts it
        // in place to keep the 9.6 divergence rule covered for a value that gets past
        // staging.
        model.stage_unchecked_for_tests(SettingId::CursorTheme, "Nord\"ic");

        let contribution = model
            .apply_contribution()
            .expect("the copies that accept the value are still written");
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                fallback(&paths),
            ],
            "hyprland.lua and uwsm/env refuse the quote, so they contribute no write"
        );

        model.commit();
        assert_eq!(
            model.cursor_theme.original(),
            Some("Nordic-cursors"),
            "the cursor baseline is not promoted while one existing copy holds the old value"
        );
        assert!(
            model.cursor_theme.is_dirty() && model.is_dirty(),
            "the page stays dirty, so the user is not told a half-written change is applied"
        );
    }

    #[test]
    fn a_quote_in_a_cursor_theme_is_refused_at_stage_time() {
        // Task 9.27 accept criterion: the scan offers a `Nord"ic` cursor theme, but the
        // hl.env line of hyprland.lua cannot hold a `"` (it would end the Lua string), so
        // picking it is refused with a message naming the character and that writer, and
        // nothing is staged — instead of the four-of-five-copies split 9.6 had to guard.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let icons = write_icon_root(tmp.path());
        fs::create_dir_all(icons.join("Nord\"ic").join("cursors"))
            .expect("create a cursor theme dir whose name contains a quote");
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![icons],
        };
        let mut model = ThemesModel::load(&roots, paths, false, None);
        assert!(
            model.cursor_themes().contains(&"Nord\"ic".to_string()),
            "the scan still lists the theme; staging is where it is refused"
        );

        let message = model
            .stage_cursor_theme("Nord\"ic")
            .expect_err("a `\"` is refused")
            .to_string();
        assert!(
            message.contains("the character `\"`") && message.contains("hl.env"),
            "the message names the character and the Lua writer: {message}"
        );
        assert!(
            !message.contains("settings.ini"),
            "writers that accept the character are not blamed: {message}"
        );
        assert!(!model.is_dirty(), "nothing was staged");
        assert!(model.apply_contribution().is_none());
    }

    #[test]
    fn a_control_character_in_a_cursor_theme_is_refused() {
        // The Lua string rule refuses every control character, not only the line breaks
        // the INI and env writers refuse; a tab therefore names hl.env alone.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let mut model = load_with_cursors(tmp.path(), &paths);

        let message = model
            .stage_cursor_theme("Nord\tic")
            .expect_err("a tab is refused")
            .to_string();
        assert!(
            message.contains("a tab (U+0009)") && message.contains("hl.env"),
            "{message}"
        );
        assert!(
            message.contains("`Nord\\tic`"),
            "the quoted value shows the tab escaped, not raw: {message}"
        );
        assert!(!model.is_dirty());
    }

    #[test]
    fn a_shell_unsafe_cursor_theme_is_refused_naming_uwsm_env() {
        // Task 9.27 review: uwsm/env is sourced by the login shell, so a cursor theme
        // with a space would be split there and one with `;`, a backtick or `(` would run
        // as code. Each is refused when picked, with a message naming uwsm/env and the
        // character, and nothing is staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let mut model = load_with_cursors(tmp.path(), &paths);
        for (name, character) in [
            ("My Theme", "a space"),
            ("a;b", "the character `;`"),
            ("a`id`", "a backtick"),
            ("a(b", "the character `(`"),
        ] {
            let message = model
                .stage_cursor_theme(name)
                .expect_err("not a bare shell word")
                .to_string();
            assert!(
                message.contains("uwsm/env") && message.contains("login shell"),
                "{name:?}: {message}"
            );
            assert!(message.contains(character), "{message}");
        }
        assert!(!model.is_dirty());
        model
            .stage_cursor_theme("Bibata")
            .expect("a shell-safe name still stages");
    }

    #[test]
    fn a_hash_in_a_cursor_theme_is_accepted_and_written_everywhere() {
        // Task 9.27 corrects the premise it was filed under: no current writer of a
        // cursor theme refuses `#`. The hyprlang `hyprland.conf` copy that did is gone
        // (task 11.5), and inside a Lua string a `#` is plain text. So the name stages
        // and reaches all five copies, hyprland.lua included.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let mut model = load_with_cursors(tmp.path(), &paths);

        model
            .stage_cursor_theme("Nord#ic")
            .expect("no writer of a cursor theme refuses `#`");
        let contribution = model.apply_contribution().expect("a cursor change");
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                hypr_lua(&paths),
                paths.uwsm_env.as_path(),
                fallback(&paths),
            ]
        );
        let lua = contribution
            .writes
            .iter()
            .find(|write| write.path == hypr_lua(&paths))
            .expect("hyprland.lua is written");
        assert!(
            String::from_utf8_lossy(&lua.contents)
                .contains("hl.env(\"XCURSOR_THEME\", \"Nord#ic\")"),
            "the `#` is written inside the Lua string"
        );
        model.commit();
        assert!(!model.is_dirty(), "every copy took it, so it is promoted");
    }

    #[test]
    fn a_non_numeric_cursor_size_is_refused() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let mut model = load_with_cursors(tmp.path(), &paths);
        for size in ["big", "0", "-24", "24.5"] {
            let message = model
                .stage_cursor_size(size)
                .expect_err("not a positive whole number")
                .to_string();
            assert!(message.contains("not a cursor size"), "{size}: {message}");
        }
        assert!(!model.is_dirty());
    }

    #[test]
    fn a_gtk_theme_that_is_not_one_directory_name_is_refused() {
        // The GTK 4 link step joins the name onto a theme root as one path component, so
        // a name with a `/` (only possible from a hand-edited value) is refused.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = two_gtk_themes_model(tmp.path());
        let message = model
            .stage_gtk_theme("../escape")
            .expect_err("not a single directory name")
            .to_string();
        assert!(message.contains("GTK 4 theme links"), "{message}");
        assert!(!model.is_dirty());
    }

    #[test]
    fn re_picking_an_unwritable_on_disk_name_is_not_refused() {
        // A value already on disk is a reset target, not an edit: re-picking it clears a
        // pending edit even when it would be refused as a new choice.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let quoted = SETTINGS_INI.replace("Nordic-cursors", "Nord\"ic");
        fs::write(&paths.gtk3_settings, &quoted).expect("rewrite gtk-3.0 settings.ini");
        fs::write(&paths.gtk4_settings, &quoted).expect("rewrite gtk-4.0 settings.ini");
        let mut model = load_with_cursors(tmp.path(), &paths);

        model
            .stage_cursor_theme("Bibata")
            .expect("a writable name stages");
        assert!(model.is_dirty());
        model
            .stage_cursor_theme("Nord\"ic")
            .expect("going back to the configured name is never refused");
        assert!(!model.is_dirty(), "the pending edit is cleared");
    }

    #[test]
    fn a_corrupted_staged_value_fails_pipeline_step_one() {
        // Task 9.27 accept criterion, in-crate half: a value that got past staging (put
        // there through the test seam) is caught by the pipeline's validation step, not by
        // a writer — the outcome is `ValidationFailed` naming the setting, and no file is
        // written and no command runs.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let mut model = load_with_cursors(tmp.path(), &paths);
        model.stage_unchecked_for_tests(SettingId::CursorTheme, "Nord\"ic");

        let contribution = model.apply_contribution().expect("a cursor change");
        assert_eq!(
            contribution.validations,
            vec![(SettingId::CursorTheme, Value::Enum("Nord\"ic".to_string()))]
        );
        let before = fs::read(hypr_lua(&paths)).expect("read hyprland.lua");
        let gtk3_before = fs::read(&paths.gtk3_settings).expect("read settings.ini");
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: contribution.links,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let runner = MockCommandRunner::new();
        let outcome = apply::run(
            &plan,
            &FreshnessTracker::new(),
            &Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true),
            &runner,
            &MockProcessSignaller::new(),
        );
        match outcome {
            ApplyOutcome::ValidationFailed(invalid) => {
                assert_eq!(invalid.len(), 1);
                assert_eq!(invalid[0].id, SettingId::CursorTheme);
                assert!(invalid[0].error.to_string().contains("hl.env"));
            }
            other => panic!("expected step 1 to refuse the value, got {other:?}"),
        }
        assert_eq!(fs::read(hypr_lua(&paths)).unwrap(), before);
        assert_eq!(fs::read(&paths.gtk3_settings).unwrap(), gtk3_before);
        assert!(runner.recorded().is_empty(), "nothing is reloaded");
    }

    #[test]
    fn a_missing_lua_cursor_line_does_not_block_promotion() {
        // Task 9.6, the other side of the divergence rule: an ABSENT copy must not block
        // promotion. Here hyprland.lua carries only the XCURSOR_THEME line, and the Lua
        // editor rewrites hl.env calls but never adds one, so a cursor *size* change can
        // only reach the two settings.ini files and uwsm/env (index.theme has no size).
        // Nothing on disk then disagrees with the new size, so the commit promotes it and
        // the page goes clean. Were promotion to require every copy instead, the page
        // would stay dirty forever on such a host and every later Apply would rewrite the
        // other copies for nothing.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        fs::write(
            hypr_lua(&paths),
            "hl.env(\"XCURSOR_THEME\", \"Nordic-cursors\")\n",
        )
        .expect("write a hyprland.lua with no XCURSOR_SIZE line");

        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_size("24")
            .expect("the value passes the stage-time check");

        let contribution = model
            .apply_contribution()
            .expect("the three copies carrying the size are written");
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                paths.uwsm_env.as_path(),
            ],
            "with no XCURSOR_SIZE line to edit, hyprland.lua contributes no write, and a \
             size-only change never touches index.theme"
        );

        model.commit();
        let size = model
            .selected_cursor_size_index()
            .and_then(|index| model.cursor_sizes().get(index))
            .map(String::as_str);
        assert_eq!(
            size,
            Some("24"),
            "the size is promoted on the strength of the copies that carry it"
        );
        assert!(
            !model.is_dirty(),
            "an absent copy does not keep the page dirty"
        );
    }

    #[test]
    fn an_unsafe_hyprland_lua_is_left_untouched() {
        // A hyprland.lua whose bracket structure the Lua editor cannot follow (here an
        // unclosed table) has no locatable lines: its cursor values read as absent — the
        // page still loads and shows the other copies' value, and no disagreement is
        // claimed — but the write is refused with a logged reason rather than guessed,
        // and the refusal keeps the change from being promoted (the file may well hold
        // the old value). The file is left byte-identical.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let unsafe_lua = "hl.env(\"XCURSOR_THEME\", \"Other\")\nhl.config({\n    general = {\n";
        fs::write(hypr_lua(&paths), unsafe_lua).unwrap();

        let mut model = load_with_cursors(tmp.path(), &paths);
        let shown = model
            .selected_cursor_index()
            .and_then(|index| model.cursor_themes().get(index))
            .map(String::as_str);
        assert_eq!(shown, Some("Nordic-cursors"), "the page still loads");
        assert_eq!(
            model.disagreeing_cursor_copies(),
            Vec::new(),
            "an unreadable Lua copy is absent, not a disagreement"
        );

        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("the other copies are still written");
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.backing != BackingFile::HyprlandLua),
            "nothing is written to a hyprland.lua whose structure cannot be followed"
        );
        // Task 9.28: the refusal is reported with the Lua editor's reason, and the live
        // push follows settings.ini, which did take the new theme.
        assert_eq!(contribution.skipped.len(), 1, "{:?}", contribution.skipped);
        let skip = &contribution.skipped[0];
        assert_eq!((skip.value, skip.file), ("Cursor theme", "hyprland.lua"));
        assert!(
            skip.reason.contains("structure cannot be followed"),
            "the reason is the Lua editor's: {}",
            skip.reason
        );
        assert_eq!(
            contribution.reload_params.cursor,
            Some(CursorValue {
                theme: "Bibata".to_string(),
                size: 16,
            }),
            "settings.ini holds the new theme, so gsettings and setcursor get it"
        );
        model.commit();
        assert!(model.is_dirty(), "the refused copy blocks promotion");
        assert_eq!(fs::read_to_string(hypr_lua(&paths)).unwrap(), unsafe_lua);

        // The page says why it stays dirty, and stops saying so once the change is
        // discarded.
        let note = model
            .skipped_writes_note()
            .expect("the pending change is explained on the page");
        assert!(
            note.contains("Cursor theme could not be written to hyprland.lua"),
            "{note}"
        );
        model.reset();
        assert_eq!(model.skipped_writes_note(), None);
    }

    #[test]
    fn a_missing_lua_cursor_line_is_not_reported_as_skipped() {
        // Task 9.28 keeps task 9.6's absent-copy rule: a hyprland.lua with no hl.env
        // cursor lines carries no copy to disagree with, so nothing is reported — a
        // warning there would be one the user can do nothing about, shown on every
        // cursor change.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        fs::write(
            hypr_lua(&paths),
            "hl.config({\n    general = {\n    },\n})\n",
        )
        .unwrap();

        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model
            .apply_contribution()
            .expect("the other copies are written");
        assert_eq!(contribution.skipped, Vec::new());
        assert_eq!(model.skipped_writes_note(), None);
    }

    #[test]
    fn a_value_no_settings_ini_took_is_never_pushed_live() {
        // Task 9.28: the gsettings push is driven by what reached settings.ini, not by
        // what is staged. Here the icon theme is refused by both settings.ini files (a
        // line break, which since task 9.27 only the test seam can stage) while the GTK
        // theme is written: gsettings must set the GTK theme and must not push the icon
        // theme, which no file holds.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let themes = tmp.path().join("themes");
        install_gtk_theme(&themes, "Adwaita", "gtk-4.0");
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };
        let mut model = ThemesModel::load(&roots, paths, false, None);
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        model.stage_unchecked_for_tests(SettingId::IconTheme, "Papirus\nInjected");

        let contribution = model
            .apply_contribution()
            .expect("the GTK theme change contributes");
        assert_eq!(
            contribution.reload_params.gtk_theme.as_deref(),
            Some("Adwaita")
        );
        assert_eq!(
            contribution.reload_params.icon_theme, None,
            "the refused icon theme is not pushed into dconf"
        );

        // Through the pipeline. Step 1 would reject the corrupted icon value first —
        // the primary defence — so its validation is left out here to reach the reload
        // step and pin this second one; no write carries the icon theme, so the
        // plan-drift guard has nothing to object to.
        let plan = ApplyPlan {
            validations: contribution
                .validations
                .into_iter()
                .filter(|(id, _)| *id != SettingId::IconTheme)
                .collect(),
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], true);
        let runner = MockCommandRunner::new();
        let outcome = apply::run(
            &plan,
            &FreshnessTracker::new(),
            &caps,
            &runner,
            &MockProcessSignaller::new(),
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "gtk-theme",
                "Adwaita",
            ])],
            "only the value settings.ini took is pushed live"
        );
    }

    #[test]
    fn the_mismatch_note_names_a_disagreeing_lua_copy() {
        // R3.4: the Theme page reports which cursor copies differ from the value it
        // shows. With every copy at Nordic-cursors/16 except hyprland.lua's size line,
        // exactly the Lua copy is named.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let model = load_with_cursors(tmp.path(), &paths);
        assert_eq!(model.disagreeing_cursor_copies(), Vec::new());
        assert_eq!(model.cursor_mismatch_note(), None, "all copies agree");

        fs::write(
            hypr_lua(&paths),
            replace_once(
                HYPRLAND_LUA,
                "hl.env(\"XCURSOR_SIZE\", \"16\")",
                "hl.env(\"XCURSOR_SIZE\", \"24\")",
            ),
        )
        .unwrap();
        let model = load_with_cursors(tmp.path(), &paths);
        assert_eq!(
            model.disagreeing_cursor_copies(),
            vec![CursorCopy::HyprlandLua]
        );
        assert_eq!(
            model.cursor_mismatch_note().as_deref(),
            Some(
                "The cursor is set differently in hyprland.lua. Applying a cursor change \
                 writes the same theme and size to every copy."
            )
        );
    }

    #[test]
    fn the_mismatch_note_names_a_disagreeing_index_theme() {
        // The XCursor fallback theme's Inherits= is a cursor copy too (analysis §8.4).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        fs::write(
            fallback(&paths),
            replace_once(
                CURSOR_FALLBACK,
                "Inherits=Nordic-cursors",
                "Inherits=Adwaita",
            ),
        )
        .unwrap();

        let model = load_with_cursors(tmp.path(), &paths);
        assert_eq!(
            model.disagreeing_cursor_copies(),
            vec![CursorCopy::CursorFallbackTheme]
        );
        assert!(
            model
                .cursor_mismatch_note()
                .is_some_and(|note| note.contains("~/.icons/default/index.theme")),
            "the note names the fallback theme"
        );
    }

    #[test]
    fn the_cursor_preselect_falls_back_to_lua_then_index_theme() {
        // The value the page shows comes from settings.ini, then uwsm/env, then
        // hyprland.lua, then (the theme only) index.theme — so a host whose only cursor
        // copies are the Lua lines and the fallback theme still preselects them.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let no_cursor = "[Settings]\ngtk-theme-name=Everforest-Green-Dark\n";
        fs::write(&paths.gtk3_settings, no_cursor).unwrap();
        fs::write(&paths.gtk4_settings, no_cursor).unwrap();
        fs::write(&paths.uwsm_env, "export GDK_BACKEND=wayland\n").unwrap();

        let model = load_with_cursors(tmp.path(), &paths);
        let selected = |options: &[String], index: Option<usize>| {
            index.and_then(|index| options.get(index)).cloned()
        };
        assert_eq!(
            selected(model.cursor_themes(), model.selected_cursor_index()).as_deref(),
            Some("Nordic-cursors"),
            "the theme comes from hyprland.lua"
        );
        assert_eq!(
            selected(model.cursor_sizes(), model.selected_cursor_size_index()).as_deref(),
            Some("16"),
            "the size comes from hyprland.lua"
        );

        fs::write(hypr_lua(&paths), "-- no cursor lines\n").unwrap();
        let model = load_with_cursors(tmp.path(), &paths);
        assert_eq!(
            selected(model.cursor_themes(), model.selected_cursor_index()).as_deref(),
            Some("Nordic-cursors"),
            "the theme comes from index.theme's Inherits= as the last resort"
        );
    }

    #[test]
    fn a_missing_index_theme_is_created_with_the_standard_keys() {
        // Analysis §8.4: when ~/.icons/default/index.theme is absent (and so is its
        // directory), a cursor theme change creates it — the three keys the dotfiles
        // ship, the chosen theme as Inherits= — through the exclusive-create write
        // mode. After the commit the created file is an ordinary backing file: the next
        // change rewrites it in place and no self-conflict is reported.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let icons_default = fallback(&paths).parent().unwrap().to_path_buf();
        fs::remove_dir_all(&icons_default).unwrap();

        let mut model = load_with_cursors(tmp.path(), &paths);
        assert!(!model.check_conflict(), "an absent file is not a conflict");
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("a cursor change");
        let create = contribution
            .writes
            .iter()
            .find(|write| write.backing == BackingFile::CursorFallbackTheme)
            .expect("the fallback theme is part of the write set");
        assert_eq!(create.mode, WriteMode::CreateNew);
        assert_eq!(create.path, fallback(&paths));
        assert_eq!(
            String::from_utf8(create.contents.clone()).unwrap(),
            "[Icon Theme]\nName=Default\nComment=Default Cursor Theme\nInherits=Bibata\n"
        );
        assert!(
            contribution
                .writes
                .iter()
                .filter(|write| write.backing != BackingFile::CursorFallbackTheme)
                .all(|write| write.mode == WriteMode::Replace),
            "every other copy is rewritten, never created"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
        let outcome = apply::run(
            &plan,
            &FreshnessTracker::new(),
            &caps,
            &MockCommandRunner::new(),
            &MockProcessSignaller::new(),
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(fallback(&paths)).unwrap(),
            "[Icon Theme]\nName=Default\nComment=Default Cursor Theme\nInherits=Bibata\n"
        );
        assert!(icons_default.is_dir(), "the directory is created as well");

        model.commit();
        assert!(!model.is_dirty());
        assert!(
            !model.check_conflict(),
            "the app's own creation is not a conflict"
        );
        model
            .stage_cursor_theme("Nordic-cursors")
            .expect("the value passes the stage-time check");
        let second = model.apply_contribution().expect("a second cursor change");
        let rewrite = second
            .writes
            .iter()
            .find(|write| write.backing == BackingFile::CursorFallbackTheme)
            .expect("the created file is written again");
        assert_eq!(
            rewrite.mode,
            WriteMode::Replace,
            "once created, the file is rewritten in place"
        );
    }

    #[test]
    fn an_index_theme_created_by_another_program_is_a_conflict() {
        // R5.6: the staged creation was rendered from "absent". A file appearing at the
        // path before the Apply is an external change, reported like any other conflict
        // so the page reloads instead of writing over it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        fs::remove_file(fallback(&paths)).unwrap();

        let model = load_with_cursors(tmp.path(), &paths);
        assert!(!model.check_conflict());
        fs::write(fallback(&paths), CURSOR_FALLBACK).unwrap();
        assert!(
            model.check_conflict(),
            "the newly appeared file is a conflict"
        );
        assert!(
            !model.reload().check_conflict(),
            "a reload adopts the new file as an ordinary backing file"
        );
    }

    #[test]
    fn an_unreadable_or_dangling_index_theme_is_never_created_over() {
        // Only a path with nothing at it is created. A dangling symlink (a deployment
        // link whose repo file is missing) is a link the user made; it is skipped like
        // any unreadable copy, never replaced by a plain file. And with the home
        // directory unknown there is no path at all, so nothing is created anywhere.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let mut paths = write_backing_fixture(&config, UWSM_ENV);
        fs::remove_file(fallback(&paths)).unwrap();
        symlink(tmp.path().join("missing-repo-file"), fallback(&paths)).unwrap();

        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("the other copies");
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.backing != BackingFile::CursorFallbackTheme),
            "a dangling link is not written through or replaced"
        );

        paths.cursor_fallback_theme = None;
        let mut model = load_with_cursors(tmp.path(), &paths);
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("the other copies");
        assert_eq!(
            contribution.writes.len(),
            4,
            "no index.theme write without a home"
        );
    }

    #[test]
    fn the_login_note_names_both_session_env_copies_on_a_lua_host() {
        // Architecture §6: env-file copies take effect at the next login, which the page
        // says. On a Lua host the hl.env lines are such a copy too.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let model = load_with_cursors(tmp.path(), &paths);
        assert_eq!(
            model.cursor_login_note().as_deref(),
            Some(
                "A cursor change shows immediately; its session environment copies \
                 (uwsm/env and the hl.env lines of hyprland.lua) take effect at the next \
                 login."
            )
        );
    }

    /// Theme roots with nothing to scan, for tests that only look at the backing
    /// files.
    fn no_theme_roots() -> ThemeRoots {
        ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: Vec::new(),
        }
    }

    /// The repo copy and live path layout of the unlinked-`settings.ini` tests (task
    /// 11.12): a repo at `tmp/repo` with both `config/gtk-N.0/settings.ini` copies, and
    /// a live config root at `tmp/config` whose `gtk-3.0/settings.ini` is a deployment
    /// symlink into the repo. The caller decides what the live `gtk-4.0` path is.
    /// Returns `(paths, repo)` with the repo root already set on the paths.
    #[cfg(unix)]
    fn linked_gtk3_fixture(tmp: &Path) -> (ThemesPaths, PathBuf) {
        use std::os::unix::fs::symlink;
        let repo = tmp.join("repo");
        for version in ["gtk-3.0", "gtk-4.0"] {
            let dir = repo.join("config").join(version);
            fs::create_dir_all(&dir).expect("create a repo gtk dir");
            fs::write(dir.join("settings.ini"), SETTINGS_INI).expect("write a repo copy");
        }
        let config = tmp.join("config");
        fs::create_dir_all(config.join("gtk-3.0")).expect("create the live gtk-3.0 dir");
        symlink(
            repo.join(GTK3_SETTINGS_REPO_RELATIVE),
            config.join("gtk-3.0/settings.ini"),
        )
        .expect("link the live gtk-3.0 settings.ini into the repo");
        let mut paths = paths_under(&config, &tmp.join("home"));
        paths.dotfiles_repo = Some(repo.clone());
        (paths, repo)
    }

    #[cfg(unix)]
    #[test]
    fn a_plain_settings_ini_beside_its_repo_copy_is_reported() {
        // Task 11.12 (analysis §7.7): the nwg-look shape — gtk-4.0/settings.ini
        // replaced by a plain copy while the repo still has its own, gtk-3.0 still
        // linked. Only the plain one is reported, with a note naming both paths.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (paths, repo) = linked_gtk3_fixture(tmp.path());
        fs::create_dir_all(paths.gtk4_settings.parent().expect("has a parent"))
            .expect("create the live gtk-4.0 dir");
        fs::write(&paths.gtk4_settings, SETTINGS_INI).expect("write the plain live copy");

        let model = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);

        assert_eq!(
            model.unlinked_gtk_settings(),
            vec![paths.gtk4_settings.as_path()]
        );
        // Both paths lie outside the fixture's home (`tmp/home`), so they are shown
        // absolute.
        let absolute_note = format!(
            "{} is no longer linked to the dotfiles; changes here will not reach the \
             repo copy at {}.",
            paths.gtk4_settings.display(),
            repo.join(GTK4_SETTINGS_REPO_RELATIVE).display()
        );
        assert_eq!(
            model.unlinked_gtk_settings_notes(),
            vec![absolute_note.clone()]
        );

        // With `tmp` as the home, both are shown in the `~/…` form.
        let mut home_paths = paths.clone();
        home_paths.home = Some(tmp.path().to_path_buf());
        let model = ThemesModel::load(&no_theme_roots(), home_paths, false, None);
        assert_eq!(
            model.unlinked_gtk_settings_notes(),
            vec![
                "~/config/gtk-4.0/settings.ini is no longer linked to the dotfiles; changes \
                 here will not reach the repo copy at ~/repo/config/gtk-4.0/settings.ini."
                    .to_string()
            ]
        );

        // An unknown home falls back to absolute paths.
        let mut homeless = paths;
        homeless.home = None;
        let model = ThemesModel::load(&no_theme_roots(), homeless, false, None);
        assert_eq!(model.unlinked_gtk_settings_notes(), vec![absolute_note]);
    }

    #[test]
    fn a_plain_settings_ini_without_a_repo_copy_is_not_reported() {
        // Task 11.12: plain files are the normal, non-dotfiles setup, so without a
        // repo — or with a repo that has no copy of the file — nothing is reported.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut paths = write_backing_fixture(tmp.path(), "");
        assert_eq!(paths.dotfiles_repo, None);
        let model = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);
        assert!(model.unlinked_gtk_settings().is_empty());
        assert!(model.unlinked_gtk_settings_notes().is_empty());

        let repo = tmp.path().join("repo-without-gtk-copies");
        fs::create_dir_all(&repo).expect("create an empty repo");
        paths.dotfiles_repo = Some(repo);
        let model = ThemesModel::load(&no_theme_roots(), paths, false, None);
        assert!(model.unlinked_gtk_settings().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_settings_ini_in_a_linked_directory_counts_as_linked() {
        // Task 11.12: a setup that links the whole gtk-4.0 directory into the repo
        // makes the live file a regular file that *is* the repo copy — not a
        // divergence. A dangling or missing live file is not one either.
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("temp dir");
        let (paths, repo) = linked_gtk3_fixture(tmp.path());
        let live_dir = paths.gtk4_settings.parent().expect("has a parent");
        symlink(repo.join("config/gtk-4.0"), live_dir).expect("link the gtk-4.0 directory");

        let model = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);
        assert!(model.unlinked_gtk_settings().is_empty());

        fs::remove_file(live_dir).expect("remove the directory link");
        let model = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);
        assert!(
            model.unlinked_gtk_settings().is_empty(),
            "a missing live file is not a divergence"
        );

        fs::create_dir_all(live_dir).expect("recreate the live gtk-4.0 dir");
        symlink(tmp.path().join("nowhere"), &paths.gtk4_settings).expect("dangling link");
        let model = ThemesModel::load(&no_theme_roots(), paths, false, None);
        assert!(
            model.unlinked_gtk_settings().is_empty(),
            "a dangling live link is not reported"
        );
    }

    #[test]
    fn the_xsettingsd_note_follows_detection() {
        // Task 11.12 (analysis §7.7): a running xsettingsd turns on the
        // external-writer note; it gates nothing and a conflict reload keeps it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(tmp.path(), "");
        let quiet = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);
        assert_eq!(quiet.external_gtk_writer_note(), None);

        let noted =
            ThemesModel::load(&no_theme_roots(), paths, false, None).with_xsettingsd_running(true);
        let note = noted
            .external_gtk_writer_note()
            .expect("a running xsettingsd is noted");
        assert!(note.starts_with("xsettingsd is running"), "got {note}");
        assert!(noted.themes_editable(), "the note gates no control");
        assert_eq!(noted.reload().external_gtk_writer_note(), Some(note));
    }

    #[test]
    fn the_icons_note_appears_only_for_a_pre_reload_notifd() {
        // Task 11.14 (analysis §8.2): only a running notifd older than 1.1 (or one that
        // reported no version) cannot pick up an icon-theme change live; a 1.1 daemon
        // is reloaded by the Apply, and a stopped or absent one reads the theme when it
        // starts. The note gates nothing and survives a conflict reload.
        use crate::core::notifd::NotifdVersion;
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(tmp.path(), "");
        let with = |notifd| {
            ThemesModel::load(&no_theme_roots(), paths.clone(), false, None)
                .with_notifd_status(notifd)
        };
        let running = |version| NotifdStatus::Running {
            version: NotifdVersion::parse(version),
        };

        let default = ThemesModel::load(&no_theme_roots(), paths.clone(), false, None);
        assert_eq!(default.notification_icons_note(), None);
        for quiet in [
            running("1.1"),
            running("2.0"),
            NotifdStatus::Installed,
            NotifdStatus::Absent,
        ] {
            assert_eq!(with(quiet).notification_icons_note(), None, "{quiet}");
        }

        for old in [running("1.0"), NotifdStatus::Running { version: None }] {
            let model = with(old);
            let note = model
                .notification_icons_note()
                .unwrap_or_else(|| panic!("{old} must be noted"));
            assert!(note.contains("next time"), "got {note}");
            assert!(model.themes_editable(), "the note gates no control");
            assert_eq!(model.reload().notification_icons_note(), Some(note));
        }
    }

    // --- Task 11.16: the GTK 4 theme links ---------------------------------------

    /// Creates `<root>/<theme>/gtk-4.0/` with the given entries (`assets` as a
    /// directory, the rest as files) plus a GTK 3 part (`gtk-3.0/gtk.css`), returning
    /// the `gtk-4.0` dir. The GTK 3 stylesheet keeps the theme offered by the scan
    /// even when `entries` leaves out the GTK 4 `gtk.css` (task 9.30).
    fn gtk4_theme(root: &Path, theme: &str, entries: &[&str]) -> PathBuf {
        let gtk4 = root.join(theme).join("gtk-4.0");
        fs::create_dir_all(&gtk4).expect("create gtk-4.0");
        install_gtk_theme(root, theme, "gtk-3.0");
        for entry in entries {
            if *entry == "assets" {
                fs::create_dir_all(gtk4.join(entry)).expect("create assets");
            } else {
                fs::write(gtk4.join(entry), format!("/* {theme} */\n")).expect("write css");
            }
        }
        gtk4
    }

    #[test]
    fn the_first_root_with_a_gtk4_dir_provides_link_targets() {
        // GTK's own lookup order: the first root holding `<theme>/gtk-4.0` wins, a root
        // with only `gtk-3.0` for that name does not shadow it, and the targets are
        // built from the resolved root. An entry the theme does not ship gets none.
        let tmp = tempfile::tempdir().expect("temp dir");
        let user = tmp.path().join("user-themes");
        let system = tmp.path().join("system-themes");
        install_gtk_theme(&user, "Shared", "gtk-3.0"); // a GTK 3-only shadow
        let gtk4 = gtk4_theme(&system, "Shared", &["gtk.css", "assets"]);
        let missing_root = tmp.path().join("does-not-exist");

        let source = resolve_gtk4_theme(&[missing_root, user, system.clone()], "Shared");
        let resolved = fs::canonicalize(&gtk4).expect("canonicalize");
        assert_eq!(
            source,
            Gtk4ThemeSource::Found {
                targets: [
                    Some(resolved.join("gtk.css")),
                    None,
                    Some(resolved.join("assets")),
                ]
            }
        );
    }

    #[test]
    fn a_path_shaped_theme_name_never_escapes_a_root() {
        // A name read from the user's settings.ini could hold a path; it must never be
        // joined onto a root to escape it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path().join("themes");
        gtk4_theme(&root, "Real", &["gtk.css"]);
        gtk4_theme(tmp.path(), "Escaped", &["gtk.css"]);
        let roots = [root];
        for name in ["../Escaped", "Real/../../Escaped", "/abs", "", ".", ".."] {
            assert_eq!(
                resolve_gtk4_theme(&roots, name),
                Gtk4ThemeSource::InvalidName,
                "{name:?}"
            );
        }
        assert!(matches!(
            resolve_gtk4_theme(&roots, "Real"),
            Gtk4ThemeSource::Found { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn gtk4_files_resolving_outside_the_roots_are_not_linked() {
        // "Never follow a link outside the theme roots": a theme directory, or one of
        // its entries, that is a symlink to somewhere else is refused.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path().join("themes");
        fs::create_dir_all(&root).expect("create root");
        let elsewhere = gtk4_theme(&tmp.path().join("elsewhere"), "Linked", &["gtk.css"]);
        symlink(tmp.path().join("elsewhere/Linked"), root.join("Linked")).expect("link");
        let partly = gtk4_theme(&root, "Partly", &["gtk.css"]);
        symlink(elsewhere.join("gtk.css"), partly.join("gtk-dark.css")).expect("link");

        for theme in ["Linked", "Partly"] {
            assert!(
                matches!(
                    resolve_gtk4_theme(std::slice::from_ref(&root), theme),
                    Gtk4ThemeSource::OutsideRoots { .. }
                ),
                "{theme}"
            );
        }
        assert_eq!(
            resolve_gtk4_theme(&[tmp.path().join("nothing")], "Linked"),
            Gtk4ThemeSource::NoRoots
        );
    }

    /// A model over a backing fixture whose `gtk-4.0` directory holds the three GTK 4
    /// links into `<tmp>/themes/Old/gtk-4.0`, with themes `Old` and `New` (both with a
    /// GTK 4 part) and `Plain` (GTK 3 only) installed there.
    #[cfg(unix)]
    fn linked_model(tmp: &Path) -> (ThemesModel, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let root = tmp.join("themes");
        let all = ["gtk.css", "gtk-dark.css", "assets"];
        let old = gtk4_theme(&root, "Old", &all);
        let new = gtk4_theme(&root, "New", &all);
        install_gtk_theme(&root, "Plain", "gtk-3.0"); // a GTK 3-only theme
        let paths = write_backing_fixture(&tmp.join("config"), UWSM_ENV);
        let links_dir = paths.gtk4_settings.parent().expect("dir").to_path_buf();
        for name in all {
            symlink(old.join(name), links_dir.join(name)).expect("deploy a link");
        }
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![root],
            icon_dirs: vec![write_icon_root(tmp)],
        };
        let model = ThemesModel::load(&roots, paths, false, None);
        (
            model,
            links_dir,
            fs::canonicalize(new).expect("canonicalize"),
        )
    }

    #[cfg(unix)]
    #[test]
    fn icon_or_cursor_changes_plan_no_gtk4_link_steps() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, _, _) = linked_model(tmp.path());

        model
            .stage_icon_theme("Papirus")
            .expect("the value passes the stage-time check");
        model
            .stage_cursor_theme("Bibata")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("dirty");
        assert!(
            contribution.links.is_empty(),
            "icon/cursor changes never touch the GTK 4 links"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_gtk_theme_change_plans_one_point_step_per_link() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, links_dir, new) = linked_model(tmp.path());

        model
            .stage_gtk_theme("New")
            .expect("the value passes the stage-time check");
        let links = model.apply_contribution().expect("dirty").links;
        let expected: Vec<LinkWrite> = GTK4_THEME_LINKS
            .iter()
            .map(|name| LinkWrite {
                path: links_dir.join(name),
                action: LinkAction::PointAt(new.join(name)),
                label: format!("GTK 4 theme link ({name})"),
            })
            .collect();
        assert_eq!(links, expected);
        assert_eq!(model.gtk4_links_note(), None, "New has a GTK 4 version");
    }

    #[cfg(unix)]
    #[test]
    fn a_gtk3_only_theme_plans_link_removals_with_a_note() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, links_dir, _) = linked_model(tmp.path());

        model
            .stage_gtk_theme("Plain")
            .expect("the value passes the stage-time check");
        let links = model.apply_contribution().expect("dirty").links;
        assert_eq!(
            links.iter().map(|link| &link.action).collect::<Vec<_>>(),
            vec![&LinkAction::Remove; 3]
        );
        assert_eq!(links[0].path, links_dir.join("gtk.css"));
        assert_eq!(
            model.gtk4_links_note().as_deref(),
            Some(
                "\u{201c}Plain\u{201d} has no GTK 4 version, so GTK 4 and libadwaita apps \
                 fall back to the default Adwaita look."
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_builtin_theme_plans_link_removals_without_a_note() {
        // A built-in theme has no directory either, so the links go too — but the
        // default look is then what was asked for, so there is nothing to explain.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, _, _) = linked_model(tmp.path());
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        assert_eq!(model.apply_contribution().expect("dirty").links.len(), 3);
        assert_eq!(model.gtk4_links_note(), None);
    }

    /// [`linked_model`] with only one GTK 4 link deployed: `gtk.css`, already pointing
    /// at the `New` theme; `gtk-dark.css` and `assets` are missing.
    #[cfg(unix)]
    fn partly_linked_model(tmp: &Path) -> (ThemesModel, PathBuf) {
        let (model, links_dir, new) = linked_model(tmp);
        for name in GTK4_THEME_LINKS {
            fs::remove_file(links_dir.join(name)).expect("remove the old link");
        }
        std::os::unix::fs::symlink(new.join("gtk.css"), links_dir.join("gtk.css"))
            .expect("one link already on New");
        (model, links_dir)
    }

    #[cfg(unix)]
    #[test]
    fn links_already_on_the_chosen_theme_plan_nothing() {
        // Re-applying the theme a link already points at is a no-op for that link.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, links_dir) = partly_linked_model(tmp.path());

        model
            .stage_gtk_theme("New")
            .expect("the value passes the stage-time check");
        let links = model.apply_contribution().expect("dirty").links;
        assert_eq!(
            links
                .iter()
                .map(|link| link.path.clone())
                .collect::<Vec<_>>(),
            vec![links_dir.join("gtk-dark.css"), links_dir.join("assets")],
            "only the two links that are not on New yet are planned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_link_plans_no_removal() {
        // Switching to a theme without a GTK 4 version removes only the links that exist.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, links_dir) = partly_linked_model(tmp.path());

        model
            .stage_gtk_theme("Plain")
            .expect("the value passes the stage-time check");
        let links = model.apply_contribution().expect("dirty").links;
        assert_eq!(
            links,
            vec![LinkWrite {
                path: links_dir.join("gtk.css"),
                action: LinkAction::Remove,
                label: "GTK 4 theme link (gtk.css)".to_string(),
            }],
            "only the one existing link needs removing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_directory_at_a_link_path_blocks_all_link_steps() {
        // All three links are left alone, and the GTK 4 links note names the blocking path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, links_dir, _) = linked_model(tmp.path());
        fs::remove_file(links_dir.join("assets")).expect("remove the link");
        fs::create_dir(links_dir.join("assets")).expect("a real assets directory");

        model
            .stage_gtk_theme("New")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("dirty");
        assert!(contribution.links.is_empty(), "all three are left alone");
        assert_eq!(
            contribution.writes.len(),
            2,
            "the settings.ini writes still land"
        );
        let note = model.gtk4_links_note().expect("the override is noted");
        assert!(
            note.contains("/gtk-4.0/assets is a file of its own"),
            "{note}"
        );
    }

    /// [`linked_model`] with `gtk-dark.css` re-pointed at a file of the user's own
    /// outside every theme root. Returns the model, the resolved `New` theme directory,
    /// and the user's folder holding the link target.
    #[cfg(unix)]
    fn foreign_linked_model(tmp: &Path) -> (ThemesModel, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let (model, links_dir, new) = linked_model(tmp);
        let custom = tmp.join("custom");
        fs::create_dir_all(&custom).expect("create the user's folder");
        fs::write(custom.join("gtk-dark.css"), b"/* mine */\n").expect("write css");
        fs::remove_file(links_dir.join("gtk-dark.css")).expect("remove the link");
        symlink(custom.join("gtk-dark.css"), links_dir.join("gtk-dark.css"))
            .expect("a link of the user's own");
        (model, new, custom)
    }

    #[cfg(unix)]
    #[test]
    fn a_link_outside_the_theme_roots_blocks_all_link_steps() {
        // A link of the user's own is an override: all three links are left alone and
        // the note says why, while the settings.ini writes still land.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, _, _) = foreign_linked_model(tmp.path());

        model
            .stage_gtk_theme("New")
            .expect("the value passes the stage-time check");
        let contribution = model.apply_contribution().expect("dirty");
        assert!(contribution.links.is_empty(), "all three are left alone");
        assert_eq!(
            contribution.writes.len(),
            2,
            "the settings.ini writes still land"
        );
        let note = model.gtk4_links_note().expect("the override is noted");
        assert!(
            note.contains("/gtk-4.0/gtk-dark.css links to a place outside the theme folders"),
            "{note}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_foreign_link_is_repaired() {
        // The same link dangling (its target gone) is a stale link, not an override.
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, new, custom) = foreign_linked_model(tmp.path());
        model
            .stage_gtk_theme("New")
            .expect("the value passes the stage-time check");
        fs::remove_file(custom.join("gtk-dark.css")).expect("delete the target");
        assert_eq!(model.gtk4_links_note(), None);
        let links = model.apply_contribution().expect("dirty").links;
        assert_eq!(
            links.len(),
            3,
            "the dangling link is repaired with the others"
        );
        assert_eq!(
            links[1].action,
            LinkAction::PointAt(new.join("gtk-dark.css"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_shaped_gtk_theme_name_plans_no_link_step() {
        // Only a hand-edited settings.ini can hold such a name; it is neither joined
        // onto a root nor described as a theme "without a GTK 4 version".
        let tmp = tempfile::tempdir().expect("temp dir");
        let (mut model, _, _) = linked_model(tmp.path());
        model.gtk_theme.stage("../x");
        assert_eq!(model.gtk4_links_note(), None);
        assert!(model.apply_contribution().expect("dirty").links.is_empty());
    }

    // --- Task 9.29: a GTK theme name only GTK 4 resolves ---------------------------

    /// The note [`ThemesModel::one_sided_gtk_theme_note`] shows for `Adwaita-dark`.
    const ADWAITA_DARK_NOTE: &str = "\u{201c}Adwaita-dark\u{201d} is built into GTK 4 only; \
        without an \u{201c}Adwaita-dark\u{201d} theme directory, GTK 3 apps will use the \
        light Adwaita look.";

    /// Two GTK theme roots under `tmp` laid out like a real host — a user root
    /// `<tmp>/home/.themes` (empty) and a system root `<tmp>/usr/share/themes` holding an
    /// ordinary installed theme, `Nordic` — plus, when `adwaita_dark_file` is given, an
    /// `Adwaita-dark` directory on the *second* (system) root holding that one file
    /// (a path relative to the theme directory, such as `gtk-3.0/gtk.css`), so a test
    /// proves any root counts, not only the first, and chooses exactly what the
    /// directory ships.
    fn one_sided_roots(tmp: &Path, adwaita_dark_file: Option<&str>) -> ThemeRoots {
        let user = tmp.join("home/.themes");
        let system = tmp.join("usr/share/themes");
        fs::create_dir_all(&user).expect("create the user theme root");
        install_gtk_theme(&system, "Nordic", "gtk-3.0");
        if let Some(file) = adwaita_dark_file {
            let path = system.join("Adwaita-dark").join(file);
            fs::create_dir_all(path.parent().expect("a file inside the theme"))
                .expect("create an Adwaita-dark theme directory");
            fs::write(&path, "/* Adwaita-dark */\n").expect("write the Adwaita-dark file");
        }
        ThemeRoots {
            gtk_theme_dirs: vec![user, system],
            icon_dirs: Vec::new(),
        }
    }

    /// A model over [`write_backing_fixture`]'s files (current GTK theme
    /// `Everforest-Green-Dark`) and the [`one_sided_roots`] under `tmp`.
    fn one_sided_model(tmp: &Path, adwaita_dark_file: Option<&str>) -> ThemesModel {
        let roots = one_sided_roots(&tmp.join("roots"), adwaita_dark_file);
        let paths = write_backing_fixture(&tmp.join("config"), UWSM_ENV);
        ThemesModel::load(&roots, paths, false, None)
    }

    #[test]
    fn scan_records_which_names_gtk3_can_load_from_a_directory() {
        // Only a GTK 3 stylesheet is something GTK 3 loads; a gtk-4.0-only theme is
        // offered but not recorded, and a name is recorded whichever root holds it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        install_gtk_theme(&first, "Gtk4Only", "gtk-4.0");
        install_gtk_theme(&first, "Both", "gtk-4.0");
        install_gtk_theme(&second, "Both", "gtk-3.0");
        install_gtk_theme(&second, "Gtk3Only", "gtk-3.0");

        let scan = scan_gtk_themes(&[first, second]);
        assert_eq!(
            scan.with_gtk3_stylesheet.into_iter().collect::<Vec<_>>(),
            vec!["Both".to_string(), "Gtk3Only".to_string()]
        );
        assert!(scan.offered.contains(&"Gtk4Only".to_string()));
    }

    #[test]
    fn a_gtk4_only_builtin_without_a_gtk3_dir_is_noted() {
        // The accept criterion: Adwaita-dark picked on a host whose scan found no
        // Adwaita-dark directory is one-sided — and the choice is not blocked.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), None);

        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("a one-sided name is still accepted");
        assert_eq!(
            model.one_sided_gtk_theme_note().as_deref(),
            Some(ADWAITA_DARK_NOTE)
        );
        let selected = model.selected_gtk_index().expect("a selection");
        assert_eq!(model.gtk_themes()[selected], "Adwaita-dark");
    }

    #[test]
    fn a_scanned_gtk3_stylesheet_makes_the_builtin_two_sided() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), Some("gtk-3.0/gtk.css"));
        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        assert_eq!(
            model.one_sided_gtk_theme_note(),
            None,
            "GTK 3 loads the installed Adwaita-dark directory"
        );
    }

    #[test]
    fn a_gtk4_only_dir_leaves_the_builtin_one_sided() {
        // An Adwaita-dark directory with only gtk-4.0/ is nothing GTK 3 can load.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), Some("gtk-4.0/gtk.css"));
        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        assert_eq!(
            model.one_sided_gtk_theme_note().as_deref(),
            Some(ADWAITA_DARK_NOTE)
        );
    }

    #[test]
    fn adwaita_is_never_one_sided() {
        // GTK 3 has the light Adwaita compiled in, so no directory is needed.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), None);
        model
            .stage_gtk_theme("Adwaita")
            .expect("the value passes the stage-time check");
        assert_eq!(model.one_sided_gtk_theme_note(), None);
        model
            .stage_gtk_theme("Nordic")
            .expect("the value passes the stage-time check");
        assert_eq!(
            model.one_sided_gtk_theme_note(),
            None,
            "an installed theme is never one-sided"
        );
    }

    #[test]
    fn the_one_sided_note_changes_neither_staging_nor_the_contribution() {
        // The same pick on a host with and without the GTK 3 directory: staging and the
        // Apply contribution are identical, only the note differs.
        let without_dir = tempfile::tempdir().expect("temp dir");
        let with_dir = tempfile::tempdir().expect("temp dir");
        let paths = write_backing_fixture(&without_dir.path().join("config"), UWSM_ENV);
        let load = |roots: &ThemeRoots| ThemesModel::load(roots, paths.clone(), false, None);
        let mut one_sided = load(&one_sided_roots(without_dir.path(), None));
        let mut two_sided = load(&one_sided_roots(with_dir.path(), Some("gtk-3.0/gtk.css")));

        one_sided
            .stage_gtk_theme("Adwaita-dark")
            .expect("accepted although GTK 3 cannot load it");
        two_sided
            .stage_gtk_theme("Adwaita-dark")
            .expect("accepted with the GTK 3 directory installed");
        assert!(one_sided.one_sided_gtk_theme_note().is_some());
        assert!(two_sided.one_sided_gtk_theme_note().is_none());

        let one = one_sided.apply_contribution().expect("dirty");
        let two = two_sided.apply_contribution().expect("dirty");
        let rendered = |apply: &ThemesApply| {
            apply
                .writes
                .iter()
                .map(|write| (write.path.clone(), write.contents.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(rendered(&one), rendered(&two));
        assert_eq!(one.writes.len(), 2, "both settings.ini files are written");
        assert_eq!(one.links, two.links);
        assert_eq!(one.reload_params, two.reload_params);
        assert_eq!(one.validations, two.validations);
        assert_eq!(one.skipped, two.skipped);
    }

    #[test]
    fn the_one_sided_note_follows_the_staged_value() {
        // Like the other page notes it is derived on every rebuild: it appears when the
        // name is picked (before any Apply) and goes with Reset or another pick.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), None);
        assert_eq!(
            model.one_sided_gtk_theme_note(),
            None,
            "Everforest is current"
        );

        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        assert!(model.one_sided_gtk_theme_note().is_some());
        model.reset();
        assert_eq!(model.one_sided_gtk_theme_note(), None, "gone after Reset");

        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        model
            .stage_gtk_theme("Nordic")
            .expect("the value passes the stage-time check");
        assert_eq!(
            model.one_sided_gtk_theme_note(),
            None,
            "gone after a new pick"
        );
    }

    #[test]
    fn a_current_one_sided_theme_is_noted_until_a_gtk3_stylesheet_appears() {
        // The value already on disk counts too, and a conflict reload scans again, so
        // installing the directory clears the note on the next rebuild.
        let tmp = tempfile::tempdir().expect("temp dir");
        let roots = one_sided_roots(&tmp.path().join("roots"), None);
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let gtk3 = fs::read_to_string(&paths.gtk3_settings).expect("read settings.ini");
        fs::write(
            &paths.gtk3_settings,
            replace_once(
                &gtk3,
                "gtk-theme-name=Everforest-Green-Dark",
                "gtk-theme-name=Adwaita-dark",
            ),
        )
        .expect("write settings.ini");
        let model = ThemesModel::load(&roots, paths, false, None);
        assert_eq!(
            model.one_sided_gtk_theme_note().as_deref(),
            Some(ADWAITA_DARK_NOTE)
        );
        assert_eq!(
            model.reload().one_sided_gtk_theme_note().as_deref(),
            Some(ADWAITA_DARK_NOTE)
        );

        install_gtk_theme(&roots.gtk_theme_dirs[0], "Adwaita-dark", "gtk-3.0");
        assert_eq!(model.reload().one_sided_gtk_theme_note(), None);
    }

    #[test]
    fn a_gtk_theme_override_silences_the_one_sided_note() {
        // Under GTK_THEME the settings.ini name is not what GTK uses; the override
        // banner explains the page instead.
        let tmp = tempfile::tempdir().expect("temp dir");
        let roots = one_sided_roots(&tmp.path().join("roots"), None);
        let paths = write_backing_fixture(&tmp.path().join("config"), UWSM_ENV);
        let gtk3 = fs::read_to_string(&paths.gtk3_settings).expect("read settings.ini");
        fs::write(
            &paths.gtk3_settings,
            replace_once(
                &gtk3,
                "gtk-theme-name=Everforest-Green-Dark",
                "gtk-theme-name=Adwaita-dark",
            ),
        )
        .expect("write settings.ini");
        let overridden =
            ThemesModel::load(&roots, paths.clone(), false, Some("Nordic".to_string()));
        assert!(overridden.gtk_override().is_some());
        assert_eq!(overridden.one_sided_gtk_theme_note(), None);
        assert!(
            ThemesModel::load(&roots, paths, false, None)
                .one_sided_gtk_theme_note()
                .is_some(),
            "the same files without the override are noted"
        );
    }

    // --- Task 9.30: a theme needs a stylesheet, not just a gtk-* directory ----------

    /// Writes `<root>/<theme>/<relative>` with placeholder contents, creating its
    /// directories — for theme directories that ship something other than a
    /// `gtk.css` ([`install_gtk_theme`]).
    fn write_theme_file(root: &Path, theme: &str, relative: &str) {
        let path = root.join(theme).join(relative);
        fs::create_dir_all(path.parent().expect("a file inside the theme"))
            .expect("create the theme subdirectory");
        fs::write(&path, "/* fixture */\n").expect("write the theme file");
    }

    #[test]
    fn scan_offers_only_directories_that_carry_a_stylesheet() {
        // The accept criterion (R3.3): over one fixture root, a keybinding-only package
        // and a theme with empty gtk-3.0/ and gtk-4.0/ directories are skipped, while a
        // stylesheet in gtk-3.0/, in gtk-4.0/ or in a version-suffixed gtk-3.<N>/ alone
        // is enough.
        let tmp = tempfile::tempdir().expect("temp dir");
        let themes = tmp.path().join("themes");
        write_theme_file(&themes, "Emacs", "gtk-3.0/gtk-keys.css");
        install_gtk_theme(&themes, "Gtk3Only", "gtk-3.0");
        install_gtk_theme(&themes, "Gtk4Only", "gtk-4.0");
        install_gtk_theme(&themes, "Versioned", "gtk-3.24");
        fs::create_dir_all(themes.join("Empty/gtk-3.0")).unwrap();
        fs::create_dir_all(themes.join("Empty/gtk-4.0")).unwrap();

        let scan = scan_gtk_themes(std::slice::from_ref(&themes));
        assert_eq!(
            scan.offered,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Gtk3Only".to_string(),
                "Gtk4Only".to_string(),
                "Versioned".to_string()
            ],
            "Emacs (keybindings only) and Empty (no stylesheet) are not themes"
        );
        assert_eq!(
            scan.with_gtk3_stylesheet.into_iter().collect::<Vec<_>>(),
            vec!["Gtk3Only".to_string(), "Versioned".to_string()],
            "a gtk-3.24/gtk.css is a GTK 3 stylesheet; a gtk-4.0 one is not"
        );
    }

    #[test]
    fn only_a_gtk_css_file_is_a_stylesheet() {
        // Neither the dark variant alone nor a directory that happens to be called
        // gtk.css is what GTK loads as a theme's stylesheet.
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path();
        write_theme_file(root, "DarkOnly", "gtk-4.0/gtk-dark.css");
        write_theme_file(root, "DarkOnly", "gtk-3.0/gtk-dark.css");
        fs::create_dir_all(root.join("CssDir/gtk-3.0/gtk.css")).unwrap();
        fs::create_dir_all(root.join("CssDir/gtk-4.0/gtk.css")).unwrap();
        for theme in ["DarkOnly", "CssDir"] {
            assert!(!has_gtk3_stylesheet(&root.join(theme)), "{theme}");
            assert!(!has_gtk4_stylesheet(&root.join(theme)), "{theme}");
        }
    }

    #[test]
    fn gtk3_version_directories_are_gtk_3_dot_a_number() {
        for name in ["gtk-3.0", "gtk-3.20", "gtk-3.24"] {
            assert!(is_gtk3_subdir_name(name), "{name}");
        }
        for name in [
            "gtk-3.",
            "gtk-3",
            "gtk-3.x",
            "gtk-30",
            "gtk-3.24-old",
            "gtk-4.0",
            "gtk-2.0",
        ] {
            assert!(!is_gtk3_subdir_name(name), "{name}");
        }
    }

    #[test]
    fn a_gtk4_stylesheet_counts_only_in_gtk_4_0() {
        // GTK 4 also probes gtk-4.<N>/, but the app's GTK 4 side is the gtk-4.0/
        // directory the link step points into, so the two predicates agree on it.
        let tmp = tempfile::tempdir().expect("temp dir");
        install_gtk_theme(tmp.path(), "Suffixed", "gtk-4.12");
        assert!(!has_gtk4_stylesheet(&tmp.path().join("Suffixed")));
        assert!(!has_gtk3_stylesheet(&tmp.path().join("Suffixed")));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_stylesheet_counts_and_a_dangling_one_does_not() {
        // GTK opens the path, so a stylesheet linked in from elsewhere styles the
        // theme exactly like a copy; a link to nothing styles nothing.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("temp dir");
        let shared = tmp.path().join("shared.css");
        fs::write(&shared, "/* shared */\n").unwrap();
        let themes = tmp.path().join("themes");
        for (theme, target) in [
            ("Linked", shared.clone()),
            ("Dangling", tmp.path().join("missing.css")),
        ] {
            for subdir in ["gtk-3.0", "gtk-4.0"] {
                let dir = themes.join(theme).join(subdir);
                fs::create_dir_all(&dir).unwrap();
                symlink(&target, dir.join("gtk.css")).unwrap();
            }
        }
        // A whole version directory linked in counts too.
        fs::create_dir_all(themes.join("LinkedDir")).unwrap();
        fs::create_dir_all(tmp.path().join("real-gtk-3.0")).unwrap();
        fs::write(tmp.path().join("real-gtk-3.0/gtk.css"), "/* real */\n").unwrap();
        symlink(
            tmp.path().join("real-gtk-3.0"),
            themes.join("LinkedDir/gtk-3.0"),
        )
        .unwrap();

        assert!(has_gtk3_stylesheet(&themes.join("Linked")));
        assert!(has_gtk4_stylesheet(&themes.join("Linked")));
        assert!(!has_gtk3_stylesheet(&themes.join("Dangling")));
        assert!(!has_gtk4_stylesheet(&themes.join("Dangling")));
        assert!(has_gtk3_stylesheet(&themes.join("LinkedDir")));
    }

    #[test]
    fn a_keybinding_only_gtk3_dir_leaves_the_builtin_one_sided() {
        // The 9.29 hand-off: an Adwaita-dark directory whose gtk-3.0/ holds only
        // keybindings gives GTK 3 nothing to load, so the note must stay.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), Some("gtk-3.0/gtk-keys.css"));
        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        assert_eq!(
            model.one_sided_gtk_theme_note().as_deref(),
            Some(ADWAITA_DARK_NOTE)
        );
    }

    #[test]
    fn a_version_suffixed_gtk3_stylesheet_makes_the_builtin_two_sided() {
        // GTK 3.24 probes gtk-3.24/ before gtk-3.0/, so a theme that ships its CSS
        // only there is still loaded by GTK 3.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = one_sided_model(tmp.path(), Some("gtk-3.24/gtk.css"));
        model
            .stage_gtk_theme("Adwaita-dark")
            .expect("the value passes the stage-time check");
        assert_eq!(model.one_sided_gtk_theme_note(), None);
    }
}
