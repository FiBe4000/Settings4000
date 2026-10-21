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
//! [`ThemesModel`]'s own docs carry the rest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, WriteMode, WriteValidation};
use crate::core::home_path::contract_home;
use crate::core::notifd::NotifdStatus;
use crate::core::reload::{BackingFile, CursorValue, ReloadParams};
use crate::parsers::env::{EnvFile, GtkThemeOverride};
use crate::parsers::ini::IniFile;
use crate::parsers::lua_literal::{LuaConfigFile, LuaEditError};

use super::{BackingSet, BackingText, Selection};

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
/// Why exactly these two — checked against the GTK versions on the target system
/// (GTK 3.24 and GTK 4.22) by loading each name through GTK's named-theme CSS loader:
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
///   (R3.3); it is GTK, not this app, that decides how a name resolves.
///
/// Kept sorted here for readability only; ordering in the drop-down comes from the
/// sorted set the discovery scan builds.
const BUILTIN_GTK_THEMES: &[&str] = &["Adwaita", "Adwaita-dark"];

/// The filesystem roots scanned for installed themes (R3.3/R3.4).
///
/// Injected rather than hardcoded so discovery is unit-tested against a fixture tree
/// (the accept criterion): a test points these at temporary directories. The window's
/// startup loader fills them from the XDG environment (`~/.themes`, the data dirs,
/// `/usr/share/...`).
#[derive(Clone, Debug)]
pub struct ThemeRoots {
    /// Directories that hold GTK theme directories (`~/.themes`,
    /// `~/.local/share/themes`, `/usr/share/themes`). A subdirectory is a GTK theme
    /// when it contains a `gtk-3.0/` or `gtk-4.0/` subdirectory (R3.3).
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
    /// `~/.config/gtk-4.0/settings.ini`.
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
    /// The dotfiles repository root discovered behind the deployed config (R8.5,
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
/// A set `GTK_THEME` overrides GTK's theme choice entirely, so the app must never
/// fight it: whenever this is `Some`, the Theme page shows a banner and disables the
/// GTK-theme drop-down. The icon and cursor drop-downs stay enabled — `GTK_THEME`
/// overrides only the GTK theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GtkThemeOverrideSource {
    /// `GTK_THEME` is set in the app's own process environment. On the target this
    /// happens because `scripts/launchhyprland.sh` exports it uncommented when it
    /// starts the session (analysis §6.3), so the app itself inherits it — the copy
    /// that actually overrides *this* app's theme.
    AppEnvironment(String),
    /// `GTK_THEME` is uncommented (active) in `uwsm/env`, so it overrides the theme for
    /// the session's apps (analysis §6.3).
    UwsmEnv(String),
}

impl GtkThemeOverrideSource {
    /// The override value (the theme name `GTK_THEME` is set to).
    fn value(&self) -> &str {
        match self {
            GtkThemeOverrideSource::AppEnvironment(value)
            | GtkThemeOverrideSource::UwsmEnv(value) => value,
        }
    }

    /// A human-readable banner message naming the override and where it comes from, so
    /// the user understands why the GTK-theme drop-down is disabled (R3.3).
    pub fn banner_message(&self) -> String {
        let source = match self {
            GtkThemeOverrideSource::AppEnvironment(_) => "the GTK_THEME environment variable",
            GtkThemeOverrideSource::UwsmEnv(_) => "GTK_THEME in uwsm/env",
        };
        format!(
            "The GTK theme is forced to \u{201c}{}\u{201d} by {source}, so it cannot be changed \
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

/// A flag per theme value, used for the two per-value render outcomes tracked in
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
///   dirty rather than look applied while one copy lags behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ThemeRenderRecord {
    /// Values that reached at least one of the files carrying them.
    written: ThemeValueFlags,
    /// Values that a file which *does* carry them refused, so the copies on disk diverge.
    diverged: ThemeValueFlags,
}

impl ThemeRenderRecord {
    /// Folds one file's render result into the page-wide record.
    fn merge(&mut self, other: ThemeRenderRecord) {
        self.written.merge(other.written);
        self.diverged.merge(other.diverged);
    }

    /// The values [`ThemesModel::commit`] may promote: written somewhere, refused nowhere.
    fn promotable(self) -> ThemeValueFlags {
        self.written.without(self.diverged)
    }
}

/// The GTK/icon/cursor theme staging model (task 6.4).
///
/// Built by [`ThemesModel::load`] from the discovered [`ThemeRoots`] and the backing
/// [`ThemesPaths`]. It owns the four drop-downs' [`Selection`]s, the backing config
/// texts, the `GTK_THEME` override state, and a freshness baseline for conflict
/// detection (R5.6). Its file edits reach the shared Apply pipeline through
/// [`Self::apply_contribution`]; the window folds them into the same
/// [`apply::run`](crate::core::apply::run) it drives for the store and Display model.
///
/// It stays GTK-free so discovery, staging, the multi-file write, the override
/// decision, and the live-restyle gating are all unit-tested headlessly (R6.2); the
/// layering guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
pub struct ThemesModel {
    /// The GTK theme drop-down.
    gtk_theme: Selection,
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
    gtk_override: Option<GtkThemeOverrideSource>,
    /// Whether a live theme-restyle path (settings portal or dconf) is available, so
    /// the UI may claim a live GTK-theme restyle rather than "next launch" (R2.2).
    live_restyle: bool,
    /// The theme roots, kept so [`Self::reload`] can re-discover on a conflict reload.
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
/// (R3.4); a GTK/icon theme change contributes only the two `settings.ini` writes. The reload parameters carry only the values that changed, so the reload
/// table (task 4.4) emits `gsettings set` / `hyprctl setcursor` only for those.
pub struct ThemesApply {
    /// The atomic writes, one per changed backing file.
    pub writes: Vec<FileWrite>,
    /// The reload parameters for the changed theme/cursor values (the pipeline merges
    /// these into its plan-wide [`ReloadParams`]).
    pub reload_params: ReloadParams,
}

impl ThemesModel {
    /// Builds the model by discovering installed themes and reading the backing config
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

        let gtk_themes = discover_gtk_themes(&roots.gtk_theme_dirs);
        let (icon_themes, cursor_themes) = discover_icon_and_cursor_themes(&roots.icon_dirs);

        // The app's own environment takes precedence: a GTK_THEME in it overrides this
        // very app, regardless of what uwsm/env says (R3.3).
        let uwsm_override = copies.uwsm.as_ref().map(EnvFile::gtk_theme_override);
        let gtk_override = resolve_gtk_override(app_env_gtk_theme.clone(), uwsm_override.as_ref());

        let curated_sizes: Vec<String> = CURATED_CURSOR_SIZES
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        let unlinked_gtk_settings = find_unlinked_gtk_settings(&paths);

        tracing::info!(
            gtk_themes = gtk_themes.len(),
            icon_themes = icon_themes.len(),
            cursor_themes = cursor_themes.len(),
            gtk_override = gtk_override.is_some(),
            live_restyle = settings_portal_available,
            "loaded GTK/icon/cursor themes for the Theme page (task 6.4, R3.3/R3.4)"
        );

        ThemesModel {
            gtk_theme: Selection::new(gtk_themes, current_gtk),
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
        let shown = |path: &Path| {
            self.paths
                .home
                .as_deref()
                .and_then(|home| contract_home(path, home))
                .unwrap_or_else(|| path.display().to_string())
        };
        self.unlinked_gtk_settings
            .iter()
            .map(|unlinked| {
                format!(
                    "{} is no longer linked to the dotfiles; changes here will not reach \
                     the repo copy at {}.",
                    shown(&unlinked.live),
                    shown(&unlinked.repo_copy)
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
    pub fn gtk_override(&self) -> Option<&GtkThemeOverrideSource> {
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

    /// Stages a GTK theme switch (ignored when a `GTK_THEME` override is in force).
    pub fn stage_gtk_theme(&mut self, name: &str) {
        if self.gtk_dropdown_disabled() {
            // The drop-down is disabled in the UI, so this is a defensive guard against
            // an out-of-band caller: never stage a GTK theme the override would fight.
            tracing::debug!(
                "ignoring a GTK theme edit while a GTK_THEME override is active (R3.3)"
            );
            return;
        }
        self.gtk_theme.stage(name);
    }

    /// Stages an icon theme switch.
    pub fn stage_icon_theme(&mut self, name: &str) {
        self.icon_theme.stage(name);
    }

    /// Stages a cursor theme switch.
    pub fn stage_cursor_theme(&mut self, name: &str) {
        self.cursor_theme.stage(name);
    }

    /// Stages a cursor size switch (the value is a pixel size as a string).
    pub fn stage_cursor_size(&mut self, size: &str) {
        self.cursor_size.stage(size);
    }

    /// Whether any theme/cursor value has a pending change — the page's dirty state,
    /// which the window folds into the global Apply/Reset chrome (R5.1).
    pub fn is_dirty(&self) -> bool {
        self.gtk_theme.is_changed()
            || self.icon_theme.is_changed()
            || self.cursor_theme.is_changed()
            || self.cursor_size.is_changed()
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
        let shown_theme = self.cursor_theme.value.original.as_deref();
        let shown_size = self.cursor_size.value.original.as_deref();
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

    /// Re-reads the backing files and re-discovers themes, returning a fresh model with
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
    /// nothing changed (task 6.4).
    ///
    /// Renders each changed value into the files that carry it through the surgical
    /// parsers (§3) and collects the reload parameters. A cursor change produces writes
    /// for both `settings.ini` files, `uwsm/env`, `hyprland.lua` (its two `hl.env`
    /// lines) and — when the theme changed — `~/.icons/default/index.theme`, all with
    /// the **identical** value (R3.4); a GTK/icon theme change produces only the two
    /// `settings.ini` writes.
    pub fn apply_contribution(&self) -> Option<ThemesApply> {
        if !self.is_dirty() {
            return None;
        }
        let (writes, _) = self.render_writes();
        if writes.is_empty() {
            // Dirty but nothing could be written — e.g. both settings.ini files were
            // unreadable. Nothing to apply; the page stays dirty for a retry.
            tracing::warn!(
                "theme change is dirty but no backing file could be written; skipping the theme apply (R4.4)"
            );
            return None;
        }
        Some(ThemesApply {
            writes,
            reload_params: self.reload_params(),
        })
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
    /// record of what each render achieved (used by both [`Self::apply_contribution`],
    /// which needs only the writes, and [`Self::commit`], which needs the record to decide
    /// what may be promoted).
    fn render_writes(&self) -> (Vec<FileWrite>, ThemeRenderRecord) {
        // Which files are involved is decided here; *which keys* each file receives is
        // decided by the per-render functions, which read the same per-value dirty flags.
        let cursor_changed = self.cursor_theme.is_changed() || self.cursor_size.is_changed();
        let any_changed =
            self.gtk_theme.is_changed() || self.icon_theme.is_changed() || cursor_changed;

        let mut writes = Vec::new();
        let mut record = ThemeRenderRecord::default();

        // Every theme/cursor key lives in settings.ini, so any change writes both files.
        if any_changed {
            for backing in [self.gtk3(), self.gtk4()].into_iter().flatten() {
                let (write, file_record) = self.render_settings_ini(backing);
                record.merge(file_record);
                writes.extend(write);
            }
        }

        // The cursor is additionally duplicated in hyprland.lua's hl.env lines,
        // uwsm/env and the XCursor fallback theme; write the identical value there
        // whenever the cursor changed (R3.4).
        if cursor_changed {
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
    /// A key whose `set_value` fails is skipped and logged; the remaining keys are still
    /// written, and the returned [`ThemeRenderRecord`] tells [`Self::commit`] which
    /// selections may be promoted. A failure here is always a *refusal* by a file that
    /// carries the key, never an absent copy — the INI writer appends the key, and even
    /// the `[Settings]` group, when it is missing — so it blocks promotion. (In practice a
    /// value the INI writer rejects, i.e. one containing a newline, is rejected by the env
    /// and Lua writers too, so nothing is written anywhere; the flag is what keeps
    /// that conclusion from depending on the writers agreeing.)
    fn render_settings_ini(&self, backing: &BackingText) -> (Option<FileWrite>, ThemeRenderRecord) {
        let (mut ini, _) = IniFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, key: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match ini.set_value(SETTINGS_GROUP, key, value) {
                Ok(_) => {
                    changed_keys.push(label.to_string());
                    *record.written.flag(value_id) = true;
                }
                Err(error) => {
                    tracing::warn!(key, %error, "could not set a settings.ini theme key");
                    *record.diverged.flag(value_id) = true;
                }
            }
        };
        if self.gtk_theme.is_changed() {
            set(
                ThemeValue::GtkTheme,
                KEY_GTK_THEME,
                self.gtk_theme.effective(),
                "GTK theme",
            );
        }
        if self.icon_theme.is_changed() {
            set(
                ThemeValue::IconTheme,
                KEY_ICON_THEME,
                self.icon_theme.effective(),
                "icon theme",
            );
        }
        if self.cursor_theme.is_changed() {
            set(
                ThemeValue::CursorTheme,
                KEY_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme",
            );
        }
        if self.cursor_size.is_changed() {
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
                // Task 9.10: every value here is a theme name or cursor size chosen from
                // a list this model discovered on disk (or the fixed size list), and
                // `ThemesApply` carries no validations at all — so this write is
                // validation-free by design. Task 9.27 will give these values their own
                // validations; this declaration becomes `InPlan` when it does.
                validation: WriteValidation::NotNeeded,
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
    ///   with the old value, so it counts as a **refusal** that blocks promotion (logged
    ///   at `warn` with the reason) — the app refuses rather than guesses.
    /// - a value the Lua string rule rejects ([`LuaEditError::Literal`] — a `"`, `\` or
    ///   control character): the existing line keeps its old value, a refusal too.
    ///
    /// Returns `None` for the write when neither field could be written.
    fn render_hyprland_lua(&self, backing: &BackingText) -> (Option<FileWrite>, ThemeRenderRecord) {
        let mut file = LuaConfigFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, name: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match file.set_env(name, value) {
                Ok(()) => {
                    changed_keys.push(label.to_string());
                    *record.written.flag(value_id) = true;
                }
                Err(LuaEditError::EnvNotFound(_)) => {
                    tracing::debug!(
                        name,
                        "no hl.env line for this variable in hyprland.lua; skipping that field"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        name,
                        %error,
                        "hyprland.lua's cursor line was not written, so its copy may now \
                         differ from the others (R3.4); not promoting the change"
                    );
                    *record.diverged.flag(value_id) = true;
                }
            }
        };
        if self.cursor_theme.is_changed() {
            set(
                ThemeValue::CursorTheme,
                ENV_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme (hyprland.lua hl.env)",
            );
        }
        if self.cursor_size.is_changed() {
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
                // The cursor copy of the same discovered values as the `settings.ini`
                // write above, so validation-free by design for the same reason.
                validation: WriteValidation::NotNeeded,
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
    /// when the file exists; for a file that would have been created nothing on disk
    /// disagrees, so it is merely skipped.
    fn render_cursor_fallback(&self) -> (Option<FileWrite>, ThemeRenderRecord) {
        let mut record = ThemeRenderRecord::default();
        if !self.cursor_theme.is_changed() {
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
            tracing::warn!(
                %error,
                "could not set Inherits= in the XCursor fallback theme; that copy is skipped"
            );
            if mode == WriteMode::Replace {
                record.diverged.cursor_theme = true;
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
                // The same discovered cursor theme as every other copy, so
                // validation-free by design for the same reason.
                validation: WriteValidation::NotNeeded,
                mode,
            }),
            record,
        )
    }

    /// Renders the `uwsm/env` cursor-env write, editing (or appending) the
    /// `XCURSOR_*` exports, and reports which fields the edit landed for.
    ///
    /// Like the `settings.ini` render, a failure here is a refusal by a file that carries
    /// the value (the env writer appends a missing export), so it blocks promotion.
    fn render_uwsm_env(&self, backing: &BackingText) -> (Option<FileWrite>, ThemeRenderRecord) {
        let (mut file, _) = EnvFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut record = ThemeRenderRecord::default();

        let mut set = |value_id: ThemeValue, key: &str, value: Option<&str>, label: &str| {
            let Some(value) = value else {
                return;
            };
            match file.set_value(key, value) {
                Ok(_) => {
                    changed_keys.push(label.to_string());
                    *record.written.flag(value_id) = true;
                }
                Err(error) => {
                    tracing::warn!(key, %error, "could not set a uwsm/env cursor variable");
                    *record.diverged.flag(value_id) = true;
                }
            }
        };
        if self.cursor_theme.is_changed() {
            set(
                ThemeValue::CursorTheme,
                ENV_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme (uwsm/env)",
            );
        }
        if self.cursor_size.is_changed() {
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
                // The `uwsm/env` copy of the same discovered cursor values, so
                // validation-free by design for the same reason.
                validation: WriteValidation::NotNeeded,
                mode: WriteMode::Replace,
            }),
            record,
        )
    }

    /// The reload parameters for the changed values (task 4.4): a value is set only
    /// when it changed, so the reload table emits `gsettings set` / `hyprctl setcursor`
    /// only for those. The cursor value carries the effective theme *and* size (both
    /// are needed for `hyprctl setcursor`), so it is present when either changed.
    fn reload_params(&self) -> ReloadParams {
        let cursor = if self.cursor_theme.is_changed() || self.cursor_size.is_changed() {
            match (
                self.cursor_theme.effective(),
                self.cursor_size.effective().and_then(parse_cursor_size),
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
            gtk_theme: self
                .gtk_theme
                .is_changed()
                .then(|| self.gtk_theme.effective().map(str::to_string))
                .flatten(),
            icon_theme: self
                .icon_theme
                .is_changed()
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

/// Finds the live `settings.ini` files that are plain files beside a repo copy (task
/// 11.12) — see [`ThemesModel::unlinked_gtk_settings`] for what counts and why — and
/// logs each at `warn`, naming the paths only (R7.3: never file contents).
fn find_unlinked_gtk_settings(paths: &ThemesPaths) -> Vec<UnlinkedSettingsIni> {
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

/// Whether nothing at all exists at `path` — not a file, a directory, or a symlink
/// (dangling or not). Only such a path may be *created*; anything else present is
/// either someone's file or a link that must never be replaced.
fn is_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Parses a cursor size string into a positive pixel size, or `None` when it is not a
/// usable size (non-numeric or zero).
fn parse_cursor_size(size: &str) -> Option<u32> {
    match size.trim().parse::<u32>() {
        Ok(value) if value > 0 => Some(value),
        _ => None,
    }
}

/// Resolves the active `GTK_THEME` override, preferring the app's own environment over
/// `uwsm/env` (R3.3).
///
/// The app-environment copy is what actually overrides *this* app's theme (the target
/// starts the session via `scripts/launchhyprland.sh`, which exports it uncommented),
/// so it takes precedence; a commented-out `uwsm/env` line is not an override and is
/// ignored. Returns `None` when neither is active, in which case the GTK-theme
/// drop-down stays enabled.
fn resolve_gtk_override(
    app_env: Option<String>,
    uwsm: Option<&GtkThemeOverride>,
) -> Option<GtkThemeOverrideSource> {
    if let Some(value) = app_env.filter(|value| !value.is_empty()) {
        return Some(GtkThemeOverrideSource::AppEnvironment(value));
    }
    if let Some(GtkThemeOverride::Active { value }) = uwsm {
        return Some(GtkThemeOverrideSource::UwsmEnv(value.clone()));
    }
    None
}

/// Discovers the GTK themes the drop-down offers: those installed under `dirs`, plus
/// GTK's built-in ones (R3.3).
///
/// A subdirectory is a GTK theme when it contains a `gtk-3.0/` or `gtk-4.0/`
/// subdirectory. The [`BUILTIN_GTK_THEMES`] names are added unconditionally because they
/// are selectable without any theme directory (see that constant for why). Names are
/// de-duplicated (a theme in `~/.themes` shadows a system one of the same name, and a
/// scanned `Adwaita` directory collapses with the built-in entry — only the name
/// matters, since that is what `gsettings`/`settings.ini` store) and returned sorted for
/// a stable drop-down, built-ins and scan results interleaved in one order.
fn discover_gtk_themes(dirs: &[PathBuf]) -> Vec<String> {
    let mut found = collect_theme_dirs(dirs, |path| {
        path.join("gtk-3.0").is_dir() || path.join("gtk-4.0").is_dir()
    });
    // The set both de-duplicates against the scan and keeps the single sort order.
    found.extend(BUILTIN_GTK_THEMES.iter().map(|name| (*name).to_string()));
    found.into_iter().collect()
}

/// Discovers installed icon and cursor themes under `dirs` in a single scan (R3.4).
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
fn discover_icon_and_cursor_themes(dirs: &[PathBuf]) -> (Vec<String>, Vec<String>) {
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

/// Collects the names of subdirectories of each root that satisfy `is_theme`, sorted
/// and de-duplicated by name.
///
/// Shared by the GTK theme scan; `metadata` follows symlinks so a symlinked theme
/// directory counts, and dotfiles are skipped.
fn collect_theme_dirs(dirs: &[PathBuf], is_theme: impl Fn(&Path) -> bool) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
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
            if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            if is_theme(&path) {
                found.insert(name);
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

    /// A loaded model over `paths` with the icon fixture's cursor themes discoverable.
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

    #[test]
    fn discovery_finds_gtk_icon_and_cursor_themes_from_fixture_roots() {
        // Accept criterion (R3.3/R3.4): discovery unit-tested against a fixture tree
        // with injectable roots. A GTK theme is a dir with gtk-3.0/ or gtk-4.0/; a
        // cursor theme has a cursors/ subdir; an icon theme has index.theme and no
        // cursors/.
        let tmp = tempfile::tempdir().expect("temp dir");
        let themes = tmp.path().join("themes");
        fs::create_dir_all(themes.join("Everforest-Green-Dark").join("gtk-4.0")).unwrap();
        // Named Arc rather than Adwaita so this still proves the gtk-3.0 branch found it:
        // the built-in variants are merged in regardless of what the scan returns.
        fs::create_dir_all(themes.join("Arc").join("gtk-3.0")).unwrap();
        fs::create_dir_all(themes.join("NotATheme")).unwrap(); // no gtk-*/ -> skipped
        fs::create_dir_all(themes.join(".hidden").join("gtk-4.0")).unwrap(); // dotfile skipped

        let gtk = discover_gtk_themes(std::slice::from_ref(&themes));
        assert_eq!(
            gtk,
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Arc".to_string(),
                "Everforest-Green-Dark".to_string()
            ],
            "GTK themes are the dirs with a gtk-3.0/ or gtk-4.0/ plus the built-ins, sorted; \
             dotfiles/non-themes skipped"
        );

        let icons = write_icon_root(tmp.path());
        let (icon_themes, cursor_themes) =
            discover_icon_and_cursor_themes(std::slice::from_ref(&icons));
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
    fn discovery_dedups_names_across_multiple_roots() {
        // A theme of the same name in two roots (a user override shadowing a system
        // theme) appears once — only the name matters, since that is what
        // gsettings/settings.ini store.
        let tmp = tempfile::tempdir().expect("temp dir");
        let system = tmp.path().join("system");
        let user = tmp.path().join("user");
        // Deliberately not named Adwaita: the built-in variants are merged into every
        // result, so a fixture using those names would still pass even if the directory
        // scan returned nothing at all.
        fs::create_dir_all(system.join("Nordic").join("gtk-4.0")).unwrap();
        fs::create_dir_all(user.join("Nordic").join("gtk-4.0")).unwrap();
        let gtk = discover_gtk_themes(&[user, system]);
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
    fn builtin_gtk_themes_are_offered_without_a_theme_dir_and_are_never_duplicated() {
        // R3.3 requires the built-in Adwaita variants alongside the installed theme
        // dirs. GTK carries their stylesheets in the library, so a system can offer
        // Adwaita with no /usr/share/themes/Adwaita directory at all — a directory scan
        // alone would silently drop it (task 9.7).
        let tmp = tempfile::tempdir().expect("temp dir");

        // A root with an unrelated theme and deliberately no Adwaita directory.
        let without = tmp.path().join("without-adwaita");
        fs::create_dir_all(without.join("Nordic").join("gtk-3.0")).unwrap();
        assert_eq!(
            discover_gtk_themes(std::slice::from_ref(&without)),
            vec![
                "Adwaita".to_string(),
                "Adwaita-dark".to_string(),
                "Nordic".to_string()
            ],
            "the built-in variants are offered even though no Adwaita dir exists, merged \
             into the scan's single sort order"
        );

        // The same roots plus an on-disk Adwaita (e.g. gnome-themes-extra installing
        // both variants): the names must appear once each, not twice.
        let with = tmp.path().join("with-adwaita");
        fs::create_dir_all(with.join("Adwaita").join("gtk-3.0")).unwrap();
        fs::create_dir_all(with.join("Adwaita-dark").join("gtk-3.0")).unwrap();
        let scanned = discover_gtk_themes(&[with, without]);
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
        model.stage_cursor_theme("Bibata");
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
    fn a_cursor_change_writes_all_five_copies_to_the_same_value_and_reloads() {
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
        model.stage_cursor_theme("Bibata");
        model.stage_cursor_size("24");
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

        // Task 9.10: `ThemesApply` carries no validations, so every cursor copy must
        // declare itself validation-free. Asserted here as well as on the GTK-theme path
        // because this is the test that renders the hyprland.lua, uwsm/env and index.theme
        // copies: without it, one of those sites could drift to `InPlan` on its own and
        // silently make the plan-drift guard warn on every cursor change.
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validation == WriteValidation::NotNeeded),
            "every cursor copy declares that the plan validates nothing for it"
        );

        // Run the writes + reloads through the real pipeline.
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: contribution.writes,
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
    fn a_gtk_theme_change_writes_only_settings_ini_and_sets_gsettings() {
        // A GTK theme change touches only the two settings.ini files (not the env
        // files) and reloads with just `gsettings set … gtk-theme`.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.path().join("themes");
        fs::create_dir_all(themes.join("Adwaita").join("gtk-4.0")).unwrap();
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        model.stage_gtk_theme("Adwaita");

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
        // Task 9.10: `ThemesApply` carries no validations at all, so every write it
        // produces must declare itself validation-free — otherwise the Apply pipeline's
        // plan-drift guard would warn on every theme change. Task 9.27, which gives these
        // values validations of their own, is what should flip this to `InPlan`.
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validation == WriteValidation::NotNeeded),
            "theme writes are validation-free by design until task 9.27"
        );
        assert_eq!(
            contribution.reload_params.gtk_theme,
            Some("Adwaita".to_string())
        );
        assert!(contribution.reload_params.cursor.is_none());

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: contribution.writes,
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
    fn an_icon_theme_change_writes_both_settings_ini_copies_and_sets_gsettings() {
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
        // to the discovered names — the drop-down must always be able to preselect the
        // value the config actually holds.
        assert_eq!(
            model.icon_themes(),
            vec![
                "Everforest-Dark".to_string(),
                "Adwaita".to_string(),
                "Papirus".to_string()
            ],
            "the icon drop-down offers the discovered themes plus the configured value"
        );
        assert_eq!(
            model.selected_icon_index(),
            Some(0),
            "before any edit the drop-down preselects settings.ini's gtk-icon-theme-name"
        );

        model.stage_icon_theme("Papirus");
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
        // Task 9.10: `ThemesApply` carries no validations at all, so every write it
        // produces must declare itself validation-free — otherwise the Apply pipeline's
        // plan-drift guard would warn on every icon change. Task 9.27, which gives these
        // values validations of their own, is what should flip this to `InPlan`.
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validation == WriteValidation::NotNeeded),
            "theme writes are validation-free by design until task 9.27"
        );
        assert_eq!(
            contribution.reload_params.icon_theme,
            Some("Papirus".to_string()),
            "the reload carries the new icon theme name"
        );
        assert!(contribution.reload_params.gtk_theme.is_none());
        assert!(contribution.reload_params.cursor.is_none());

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: contribution.writes,
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
        model.stage_icon_theme("Papirus");
        let first = model
            .apply_contribution()
            .expect("an icon theme change contributes writes");
        let outcome = apply::run(
            &ApplyPlan {
                validations: Vec::new(),
                writes: first.writes,
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
        model.stage_cursor_size("32");
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
            Some(GtkThemeOverrideSource::AppEnvironment("Foo".to_string()))
        );
        assert_eq!(
            resolve_gtk_override(
                Some("Foo".to_string()),
                Some(&GtkThemeOverride::Active {
                    value: "Bar".to_string()
                })
            ),
            Some(GtkThemeOverrideSource::AppEnvironment("Foo".to_string())),
            "the app environment takes precedence over uwsm/env"
        );
        assert_eq!(
            resolve_gtk_override(
                Some(String::new()),
                Some(&GtkThemeOverride::Active {
                    value: "Bar".to_string()
                })
            ),
            Some(GtkThemeOverrideSource::UwsmEnv("Bar".to_string())),
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
    fn an_app_env_override_disables_the_gtk_dropdown_and_ignores_edits() {
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
        assert!(matches!(source, GtkThemeOverrideSource::AppEnvironment(_)));
        assert!(
            source.banner_message().contains("Nordic-bluish-accent"),
            "the banner names the override value"
        );
        // A GTK theme edit under the override is ignored, so the page stays clean.
        model.stage_gtk_theme("Adwaita");
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
            Some(&GtkThemeOverrideSource::UwsmEnv("Nordic".to_string()))
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

    #[test]
    fn reselecting_the_current_value_is_not_dirty_and_reset_commit_work() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.path().join("themes");
        fs::create_dir_all(themes.join("Adwaita").join("gtk-4.0")).unwrap();
        fs::create_dir_all(themes.join("Everforest-Green-Dark").join("gtk-4.0")).unwrap();
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(&roots, paths, false, None);
        // The current GTK theme is Everforest-Green-Dark (from settings.ini).
        model.stage_gtk_theme("Everforest-Green-Dark");
        assert!(
            !model.is_dirty(),
            "re-selecting the current value is not dirty"
        );

        model.stage_gtk_theme("Adwaita");
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending change");

        model.stage_gtk_theme("Adwaita");
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
    fn an_external_edit_is_a_conflict_and_reload_rebaselines() {
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
    fn cursor_preselect_falls_back_to_uwsm_env_when_settings_ini_lacks_it() {
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
        model.stage_cursor_theme("Bibata");
        model.stage_cursor_size("24");
        let contribution = model
            .apply_contribution()
            .expect("a cursor change contributes");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: contribution.writes,
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
    fn a_non_numeric_cursor_size_writes_files_but_skips_the_live_cursor_reload() {
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
        model.stage_cursor_theme("Bibata");
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
        model.stage_cursor_theme("Bibata");
        model.stage_cursor_size("24");
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
        // and NUL are forbidden), so discovery can surface one — but the settings.ini
        // writer rejects it, because writing it would split the `key=value` line. That
        // one key is skipped while the other is still written, and commit must promote
        // only the key that reached a file: promoting the skipped one would leave the
        // model showing a value no file holds, and since it would no longer look changed,
        // no later Apply would write it either.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        let themes = tmp.path().join("themes");
        fs::create_dir_all(themes.join("Adwaita").join("gtk-4.0")).unwrap();
        let roots = ThemeRoots {
            gtk_theme_dirs: vec![themes],
            icon_dirs: Vec::new(),
        };

        let mut model = ThemesModel::load(&roots, paths, false, None);
        model.stage_gtk_theme("Adwaita");
        model.stage_icon_theme("Papirus\ngtk-theme-name=Injected");

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
            model.icon_theme.value.original.as_deref(),
            Some("Everforest-Dark"),
            "the icon baseline still matches what is on disk"
        );
        assert!(
            model.icon_theme.is_changed(),
            "the skipped icon selection stays staged"
        );
        assert!(
            model.is_dirty(),
            "the page stays dirty, so the user sees the change is not applied and can retry"
        );

        // The retry renders nothing at all (the rejected value is the only dirty one), so
        // there is no contribution — and a commit in that state must still promote
        // nothing rather than silently re-baselining.
        assert!(
            model.apply_contribution().is_none(),
            "with only the rejected value staged, no file can be written"
        );
        model.commit();
        assert!(
            model.icon_theme.is_changed() && model.is_dirty(),
            "a commit with no rendered write promotes nothing"
        );
    }

    #[test]
    fn a_value_one_existing_copy_refuses_is_not_promoted() {
        // Task 9.6, the divergence rule (R3.4): the writers do not share a value rule —
        // the Lua writer additionally rejects `"` (it would end the string literal and
        // break hyprland.lua), while the settings.ini, uwsm/env and index.theme writers
        // accept it. A cursor theme directory named `Nord"ic` is legal on Linux and
        // discovery offers it, so the cursor theme can reach four of its five copies
        // while hyprland.lua's hl.env line keeps the old one. The copies on disk then
        // genuinely differ, so the change must NOT be promoted: the page stays dirty
        // rather than looking applied while one copy lags behind.
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
            "discovery surfaces the quote-named cursor theme, so the drop-down offers it"
        );
        model.stage_cursor_theme("Nord\"ic");

        let contribution = model
            .apply_contribution()
            .expect("the copies that accept the value are still written");
        assert_eq!(
            written_paths(&contribution),
            vec![
                paths.gtk3_settings.as_path(),
                paths.gtk4_settings.as_path(),
                paths.uwsm_env.as_path(),
                fallback(&paths),
            ],
            "hyprland.lua refuses the quote, so it contributes no write"
        );

        model.commit();
        assert_eq!(
            model.cursor_theme.value.original.as_deref(),
            Some("Nordic-cursors"),
            "the cursor baseline is not promoted while one existing copy holds the old value"
        );
        assert!(
            model.cursor_theme.is_changed() && model.is_dirty(),
            "the page stays dirty, so the user is not told a half-written change is applied"
        );
    }

    #[test]
    fn a_cursor_change_is_still_promoted_when_hyprland_lua_has_no_env_line() {
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
        model.stage_cursor_size("24");

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
    fn an_unsafe_hyprland_lua_is_read_as_absent_and_refuses_the_write() {
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

        model.stage_cursor_theme("Bibata");
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
        model.commit();
        assert!(model.is_dirty(), "the refused copy blocks promotion");
        assert_eq!(fs::read_to_string(hypr_lua(&paths)).unwrap(), unsafe_lua);
    }

    #[test]
    fn the_consistency_check_names_only_the_lua_copy_when_it_alone_disagrees() {
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
    fn the_consistency_check_names_only_index_theme_when_it_alone_disagrees() {
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
    fn the_cursor_falls_back_to_the_lua_and_index_theme_copies_when_the_others_lack_it() {
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
        model.stage_cursor_theme("Bibata");
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
            validations: Vec::new(),
            writes: contribution.writes,
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
        model.stage_cursor_theme("Nordic-cursors");
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
        model.stage_cursor_theme("Bibata");
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
        model.stage_cursor_theme("Bibata");
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

    /// Theme roots with nothing to discover, for tests that only look at the backing
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
    fn a_plain_settings_ini_beside_its_repo_copy_is_reported_and_a_linked_one_is_not() {
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
    fn a_plain_settings_ini_without_a_repo_or_repo_copy_is_not_reported() {
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
    fn a_settings_ini_reached_through_a_symlinked_directory_is_still_linked() {
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
    fn the_xsettingsd_note_follows_detection_and_survives_a_reload() {
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
    fn the_notification_icons_note_appears_only_for_a_running_notifd_without_reload() {
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
}
