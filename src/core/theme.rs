//! The Theme page's GTK-free domain models: palette-scheme switching (task 6.3),
//! GTK/icon/cursor theme selection (task 6.4), and wallpaper / lock-screen background
//! (task 6.5) — architecture §5, §6, §7; R2.2, R3.2, R3.3, R3.4, R4.2, R4.4, R8.3,
//! R8.5, R6.2.
//!
//! # The three models here
//!
//! The Theme page is built from independent sections, each backed by its own GTK-free
//! staging model in this module:
//!
//! - [`PaletteModel`] (task 6.3) — switching the central color palette, which runs
//!   `scripts/generate-colors` rather than editing a file (see its own docs below);
//! - [`ThemesModel`] (task 6.4) — the GTK theme, icon theme, cursor theme, and cursor
//!   size drop-downs. Unlike the palette these *do* edit files: a change is written
//!   identically to every place the value is duplicated (both GTK `settings.ini`
//!   files, and — for the cursor — `hyprland.conf`'s `env` lines and `uwsm/env`) and
//!   applied live with `gsettings set` + `hyprctl setcursor` (R3.3/R3.4, analysis
//!   §6.2). It handles the `GTK_THEME` override (never fight it, R3.3) and gates the
//!   live-restyle claim on the settings portal (R2.2). Its own docs are on the type.
//! - [`WallpaperModel`] (task 6.5) — the desktop wallpaper (`hyprpaper.conf`) and the
//!   lock-screen background (`hyprlock.conf`). Both default to the *same* image in the
//!   dotfiles (analysis §6.2), so the page presents one wallpaper path plus a fit-mode
//!   drop-down and an optional "use a different lock-screen image" override: with no
//!   override the same path is written to both files; with one, the wallpaper goes to
//!   `hyprpaper.conf` and the override to `hyprlock.conf`. A wallpaper change reloads
//!   live via `hyprctl hyprpaper preload`/`wallpaper`, while a hyprlock-only change
//!   issues **no** reload (hyprlock reads its config only at the next lock —
//!   intentional, architecture §6). Chosen image paths are validated (exists +
//!   readable + image extension, R8.3) before staging. Its own docs are on the type.
//!
//! All three are bespoke staging sources (like the Display page's per-monitor model) that
//! the window folds into the shared Apply/Reset chrome and the same
//! [`apply::run`](crate::core::apply::run) pipeline, rather than store-backed
//! [`SettingId`](crate::core::model::SettingId) values.
//!
//! # What the palette model is
//!
//! The Theme page's first section lets the user switch the central color palette.
//! The dotfiles keep one file per scheme under the repo's `colors/` directory
//! (`colors/nord`, `colors/everforest`, …), and switching is not a file edit but a
//! *regeneration*: the app runs `scripts/generate-colors <scheme>`, which rewrites
//! the read-only generated color partials from the chosen source (the palette
//! gotcha — the app never edits `colors/<scheme>` in v1, requirements §9). This
//! module is the GTK-free staging model behind that control:
//!
//! - it **enumerates** the switchable schemes from the discovered `colors/`
//!   directory (skipping dotfiles, subdirectories, and non-palette files so a
//!   `state/active-scheme`-style marker or a `README.md` never appears as a scheme);
//! - it **detects the active scheme** from the deployed generated `colors.conf`
//!   header (task 3.7, R3.2) and preselects it;
//! - it **stages** a pending switch and reports it as an
//!   [`apply::PaletteSwitch`](crate::core::apply::PaletteSwitch) contribution the
//!   Apply pipeline runs last (so `generate-colors` runs after every file write and
//!   a rollback never strands the generated files on the new scheme).
//!
//! # Why a bespoke model, not a `SettingId` in the store
//!
//! Every file-backed setting flows through [`SettingsStore`](crate::core::store) as
//! an `original`/`staged` [`Value`](crate::core::model::Value) keyed by a
//! [`SettingId`](crate::core::model::SettingId), and the store's Apply produces a
//! [`FileWrite`](crate::core::apply::FileWrite). The palette scheme fits neither
//! end of that: its "original" is read from a *generated* file's header (which the
//! app never writes), and its Apply produces **no** file write at all — it runs the
//! generator. Forcing it through the store would mean tracking the generated
//! `colors.conf` for freshness/conflict and then re-baselining a file the app did
//! not write, a poor fit. So — exactly like the Display page's per-monitor model
//! ([`crate::core::display`]) — the palette scheme is a small self-contained staging
//! model that the window folds into the same Apply/Reset chrome and the same
//! [`apply::run`](crate::core::apply::run) pipeline as a second staging source. Its
//! Apply contribution populates [`ApplyPlan::palette`](crate::core::apply::ApplyPlan),
//! not `writes`.
//!
//! # Read-only degrade (R3.2)
//!
//! With fewer than two schemes there is nothing to switch *to*, so the model reports
//! [`is_switchable`](PaletteModel::is_switchable) as `false` and the page shows the
//! active scheme read-only rather than a functional drop-down.
//!
//! It lives in `core/` so the enumeration, preselect, and staging logic are
//! headlessly testable (R6.2) — the layering guard in `tests/module_boundaries.rs`
//! forbids any `gtk`/`relm4` import here. Every path is injected, so tests drive it
//! against a temporary `colors/` directory with no live dotfiles deployment.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, PaletteSwitch};
use crate::core::freshness::FreshnessTracker;
use crate::core::model::{SettingId, ValidationError, Value, validate_image_path};
use crate::core::reload::{BackingFile, CursorValue, ReloadParams};
use crate::parsers::env::{EnvFile, GtkThemeOverride};
use crate::parsers::generated;
use crate::parsers::hyprlang::{HyprlangFile, KeyPath};
use crate::parsers::ini::IniFile;

/// One discovered, switchable palette scheme.
///
/// A scheme is one schema-valid palette file in the `colors/` directory. Its
/// [`preview`](Self::preview) colors are parsed once at load from the file's swatch
/// (task 3.7) so the UI can draw a small preview strip without re-reading the file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scheme {
    /// The scheme's name — its file name under `colors/` (e.g. `nord`), which is also
    /// the argument passed to `generate-colors`.
    name: String,
    /// The scheme's palette colors as RGB components in `0.0..=1.0`, in the palette's
    /// canonical key order, for a preview strip. Empty when no color could be parsed;
    /// a value that is not bare hex is skipped rather than failing the scheme.
    preview: Vec<(f64, f64, f64)>,
}

impl Scheme {
    /// The scheme's name (its `colors/` file name).
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The scheme's preview colors as RGB components in `0.0..=1.0`.
    pub(crate) fn preview(&self) -> &[(f64, f64, f64)] {
        &self.preview
    }
}

/// The palette-scheme staging model for the Theme page (task 6.3).
///
/// Built by [`PaletteModel::load`] from the discovered palette source and folded into
/// the window's Apply/Reset chrome as a second staging source (see the module docs).
#[derive(Clone, Debug)]
pub(crate) struct PaletteModel {
    /// The switchable schemes discovered in `colors/`, sorted by name for a stable
    /// drop-down order.
    schemes: Vec<Scheme>,
    /// The active scheme detected from the deployed generated `colors.conf` header
    /// (R3.2), or `None` when it could not be determined.
    active: Option<String>,
    /// The pending scheme switch, or `None` when nothing is staged. Only ever set to a
    /// scheme that differs from [`active`](Self::active), so `staged.is_some()` is
    /// exactly the dirty condition.
    staged: Option<String>,
    /// The discovered `scripts/generate-colors` path (R8.5), passed verbatim into the
    /// Apply contribution so the pipeline runs it with no shell.
    generate_colors: PathBuf,
}

impl PaletteModel {
    /// Builds the model by enumerating `colors_dir` and detecting the active scheme
    /// from `active_scheme_source` (task 6.3; R3.2, R8.5).
    ///
    /// `colors_dir` and `generate_colors` come from the discovered
    /// [`PaletteSource`](crate::core::detect::PaletteSource); `active_scheme_source` is
    /// the deployed generated color file (`~/.config/hypr/colors.conf`) whose header
    /// names the active scheme (task 3.7). All three are injected so the model can be
    /// built against a temporary fixture in tests. Nothing here fails: an unreadable
    /// directory yields no schemes and an unrecognized header yields an unknown active
    /// scheme, both of which the UI renders as the read-only degrade.
    pub(crate) fn load(
        colors_dir: &Path,
        active_scheme_source: &Path,
        generate_colors: PathBuf,
    ) -> PaletteModel {
        let schemes = enumerate_schemes(colors_dir);
        let active = generated::read_active_scheme(active_scheme_source)
            .name()
            .map(str::to_string);
        tracing::info!(
            schemes = schemes.len(),
            active = ?active,
            "loaded palette schemes for the Theme page (task 6.3, R3.2)"
        );
        PaletteModel {
            schemes,
            active,
            staged: None,
            generate_colors,
        }
    }

    /// The discovered schemes, in drop-down order.
    pub(crate) fn schemes(&self) -> &[Scheme] {
        &self.schemes
    }

    /// The active scheme name detected from the generated header, or `None` when
    /// unknown (R3.2).
    pub(crate) fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Whether the palette control should be an interactive drop-down (R3.2).
    ///
    /// `true` only when there are at least two schemes — with zero or one there is
    /// nothing to switch to, so the UI shows the active scheme read-only instead.
    pub(crate) fn is_switchable(&self) -> bool {
        self.schemes.len() >= 2
    }

    /// The effective selected scheme — the staged switch if one is pending, otherwise
    /// the active scheme — used to preselect the drop-down (R3.2).
    pub(crate) fn selected(&self) -> Option<&str> {
        self.staged.as_deref().or(self.active.as_deref())
    }

    /// The index of the [`selected`](Self::selected) scheme within
    /// [`schemes`](Self::schemes), for preselecting the drop-down.
    ///
    /// `None` when the selected scheme is not among the enumerated schemes — e.g. the
    /// active scheme's file is malformed or absent while others exist — in which case
    /// the UI leaves the drop-down at its default and stages nothing.
    pub(crate) fn selected_index(&self) -> Option<usize> {
        let selected = self.selected()?;
        self.schemes
            .iter()
            .position(|scheme| scheme.name == selected)
    }

    /// Stages a switch to the scheme named `name` (R3.2).
    ///
    /// Re-selecting the active scheme clears any pending switch (so the page is not
    /// dirty), matching the store's rule that re-choosing the current value is not an
    /// edit. A name that is not among the enumerated schemes is ignored — the drop-down
    /// only offers real schemes, so this is a defensive guard against an out-of-band
    /// caller.
    pub(crate) fn stage(&mut self, name: &str) {
        if !self.schemes.iter().any(|scheme| scheme.name == name) {
            tracing::warn!(
                scheme = name,
                "ignoring a palette scheme that is not in the discovered set"
            );
            return;
        }
        if self.active.as_deref() == Some(name) {
            self.staged = None;
        } else {
            self.staged = Some(name.to_string());
        }
    }

    /// Whether a scheme switch is pending (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards a pending scheme switch, returning the page to clean (R5.1).
    pub(crate) fn reset(&mut self) {
        self.staged = None;
    }

    /// Commits a pending switch after a successful Apply: the staged scheme becomes the
    /// active one, so the page is clean and the next Apply is a no-op for the palette.
    ///
    /// Called by the window only after [`apply::run`](crate::core::apply::run) reports
    /// the switch applied. There is no on-disk baseline to re-record: the app does not
    /// write the generated `colors.conf` (the generator does), so it is not tracked for
    /// conflicts.
    pub(crate) fn commit(&mut self) {
        if let Some(scheme) = self.staged.take() {
            self.active = Some(scheme);
        }
    }

    /// The palette switch to contribute to the Apply plan, or `None` when nothing is
    /// staged (task 4.5; R3.2, R8.5).
    ///
    /// The window folds this into [`ApplyPlan::palette`](crate::core::apply::ApplyPlan),
    /// so the pipeline runs the discovered `generate-colors <scheme>` as its last write
    /// step and then the palette reload chain. It carries no file write: a v1 palette
    /// switch edits no file directly.
    pub(crate) fn apply_contribution(&self) -> Option<PaletteSwitch> {
        self.staged.as_ref().map(|scheme| PaletteSwitch {
            scheme: scheme.clone(),
            generate_colors: self.generate_colors.clone(),
        })
    }
}

/// Enumerates the switchable schemes in `colors_dir`, skipping non-scheme entries
/// (R3.2, R8.5).
///
/// An entry is a scheme only when it is (1) not a dotfile, (2) a regular file
/// (following symlinks, so subdirectories are excluded), and (3) a schema-valid
/// palette. The schema-validity check is what filters out a `README.md`, an
/// `active-scheme`-style marker, or any other non-palette file: only a file with all
/// the palette's schema keys present parses as valid — the same bar
/// `generate-colors` uses, so every scheme offered here is one it would accept. The
/// result is sorted by name for a deterministic drop-down order regardless of the
/// directory's iteration order.
fn enumerate_schemes(colors_dir: &Path) -> Vec<Scheme> {
    let mut schemes = Vec::new();
    let entries = match std::fs::read_dir(colors_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::info!(
                dir = %colors_dir.display(),
                %error,
                "palette colors/ directory could not be read; no schemes enumerated (R4.4)"
            );
            return schemes;
        }
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // Skip dotfiles. The `state/active-scheme` marker lives outside `colors/`
        // (analysis §6.4), but any hidden file here is likewise not a scheme.
        if name.starts_with('.') {
            continue;
        }
        // Skip anything that is not a regular file. `metadata` follows symlinks, so a
        // scheme deployed as a symlink still counts while a subdirectory is excluded.
        match std::fs::metadata(entry.path()) {
            Ok(metadata) if metadata.is_file() => {}
            _ => continue,
        }
        // A scheme must parse as a schema-valid palette; this is what excludes
        // README.md, a state marker, or any other non-palette file (task 3.7 swatch).
        let Some(swatch) = generated::read_scheme_swatch(&entry.path()) else {
            continue;
        };
        if !swatch.validation().is_valid() {
            tracing::debug!(
                scheme = name,
                "skipping a colors/ entry that is not a complete palette"
            );
            continue;
        }
        // Accepted edge: the bar here is schema validity, not the file name, so a
        // stray but schema-valid file with an incidental name/extension (e.g.
        // `nord.bak`) is trusted and offered as a scheme — the same key-presence bar
        // `generate-colors` uses, which already excludes READMEs, markers, and
        // subdirectories as required.
        let preview = swatch
            .colors()
            .iter()
            .filter_map(|color| parse_hex_rgb(color.value()))
            .collect();
        schemes.push(Scheme {
            name: name.to_string(),
            preview,
        });
    }

    schemes.sort_by(|a, b| a.name.cmp(&b.name));
    schemes
}

/// Parses a bare six-digit hex color (e.g. `83c092`) into RGB components in
/// `0.0..=1.0`, or `None` if it is not well-formed.
///
/// Palette values are bare hex (no `#`), so this expects exactly six hex digits. A
/// scheme can be schema-valid (all keys present) yet carry a malformed value —
/// `generate-colors` checks key presence, not value format (analysis §6.4) — so a bad
/// value is skipped from the preview rather than treated as an error.
fn parse_hex_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    if hex.len() != 6 {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16)
            .ok()
            .map(|value| f64::from(value) / 255.0)
    };
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

// ===========================================================================
// GTK / icon / cursor theme model (task 6.4; R2.2, R3.3, R3.4, R4.2, R4.4)
// ===========================================================================

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
/// `uwsm/env` and `hyprland.conf` variable naming the cursor theme (analysis §6.2).
const ENV_CURSOR_THEME: &str = "XCURSOR_THEME";
/// `uwsm/env` and `hyprland.conf` variable naming the cursor size (analysis §6.2).
const ENV_CURSOR_SIZE: &str = "XCURSOR_SIZE";
/// The repeatable top-level key in `hyprland.conf` that carries the cursor env lines
/// (`env = XCURSOR_THEME,…`), addressed by the hyprlang repeatable-field writer.
const HYPR_ENV_KEY: &str = "env";

/// Cursor pixel sizes offered in the cursor-size drop-down, in ascending order. The
/// currently-configured size is added too (see [`ThemesModel::load`]) so an unusual
/// on-disk value stays selectable — mirroring the Display page's scale drop-down.
const CURATED_CURSOR_SIZES: &[&str] = &["16", "24", "32", "48", "64"];

/// The filesystem roots scanned for installed themes (R3.3/R3.4).
///
/// Injected rather than hardcoded so discovery is unit-tested against a fixture tree
/// (the accept criterion): a test points these at temporary directories. The window's
/// startup loader fills them from the XDG environment (`~/.themes`, the data dirs,
/// `/usr/share/...`).
#[derive(Clone, Debug)]
pub(crate) struct ThemeRoots {
    /// Directories that hold GTK theme directories (`~/.themes`,
    /// `~/.local/share/themes`, `/usr/share/themes`). A subdirectory is a GTK theme
    /// when it contains a `gtk-3.0/` or `gtk-4.0/` subdirectory (R3.3).
    pub(crate) gtk_theme_dirs: Vec<PathBuf>,
    /// Directories that hold icon and cursor theme directories (`~/.icons`,
    /// `~/.local/share/icons`, `/usr/share/icons`). A subdirectory with a `cursors/`
    /// subdirectory is a cursor theme; one with an `index.theme` (and no `cursors/`)
    /// is an icon theme (R3.4).
    pub(crate) icon_dirs: Vec<PathBuf>,
}

/// The live XDG paths of the four config files a theme/cursor change writes (R8.5).
///
/// Injected for the same reason as [`ThemeRoots`]: tests point them at a fixture tree,
/// and the writer follows symlinks so a dotfiles-deployed file is handled identically
/// to a plain one. The cursor is duplicated across all four; a GTK/icon theme change
/// touches only the two `settings.ini` files (analysis §6.2, R3.4).
#[derive(Clone, Debug)]
pub(crate) struct ThemesPaths {
    /// `~/.config/gtk-3.0/settings.ini`.
    pub(crate) gtk3_settings: PathBuf,
    /// `~/.config/gtk-4.0/settings.ini`.
    pub(crate) gtk4_settings: PathBuf,
    /// `~/.config/hypr/hyprland.conf` (only its cursor `env =` lines are edited).
    pub(crate) hyprland_conf: PathBuf,
    /// `~/.config/uwsm/env` (the canonical cursor env copy).
    pub(crate) uwsm_env: PathBuf,
}

/// Where an active `GTK_THEME` override was found, so the UI can name it in the banner
/// (R3.3).
///
/// A set `GTK_THEME` overrides GTK's theme choice entirely, so the app must never
/// fight it: whenever this is `Some`, the Theme page shows a banner and disables the
/// GTK-theme drop-down. The icon and cursor drop-downs stay enabled — `GTK_THEME`
/// overrides only the GTK theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GtkThemeOverrideSource {
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
    pub(crate) fn banner_message(&self) -> String {
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

/// One drop-down's staged selection: the discovered options, the current value read
/// from the backing config, and any pending switch.
///
/// Mirrors the store's `original`/`staged` dirty rule (re-selecting the current value
/// clears the pending switch, so it never lights up Apply). Used for all four theme
/// controls; the cursor size holds its numeric value as a string so the same logic
/// serves it (parsed to an integer only when written/reloaded).
#[derive(Clone, Debug)]
struct Selection {
    /// The drop-down's candidate values, in display order. Always includes the current
    /// value (prepended when discovery did not surface it) so it stays selectable.
    options: Vec<String>,
    /// The value read from the backing config, or `None` when the config did not set
    /// it. Selecting any value while this is `None` counts as a change (a write that
    /// appends the key).
    original: Option<String>,
    /// The pending selection, or `None` when nothing is staged. Only ever set to a
    /// value that differs from [`original`](Self::original), so `staged.is_some()` is
    /// exactly the dirty condition.
    staged: Option<String>,
}

impl Selection {
    /// Builds a selection over `options`, ensuring `original` is selectable.
    ///
    /// If the current value is not among the discovered options (e.g. a theme
    /// installed somewhere unusual, or a config value with no matching installed
    /// theme), it is prepended so the drop-down can still preselect it — the same
    /// "keep the configured value selectable" rule the Display page's scale/position
    /// drop-downs follow.
    fn new(mut options: Vec<String>, original: Option<String>) -> Self {
        if let Some(current) = &original {
            if !options.iter().any(|option| option == current) {
                options.insert(0, current.clone());
            }
        }
        Selection {
            options,
            original,
            staged: None,
        }
    }

    /// The effective value — the staged selection if pending, else the current value.
    fn effective(&self) -> Option<&str> {
        self.staged.as_deref().or(self.original.as_deref())
    }

    /// The index of the effective value within [`options`](Self::options), for
    /// preselecting the drop-down. `None` when the effective value is not among the
    /// options (which cannot happen once [`new`](Self::new) has made `original`
    /// selectable, but is handled without panicking).
    fn selected_index(&self) -> Option<usize> {
        let effective = self.effective()?;
        self.options.iter().position(|option| option == effective)
    }

    /// Stages a switch to `value`, clearing the pending switch when it equals the
    /// current value (so re-selecting the current value is not dirty).
    fn stage(&mut self, value: &str) {
        if self.original.as_deref() == Some(value) {
            self.staged = None;
        } else {
            self.staged = Some(value.to_string());
        }
    }

    /// Whether a switch differing from the current value is pending.
    fn is_changed(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards the pending switch.
    fn reset(&mut self) {
        self.staged = None;
    }

    /// Promotes the pending switch to the current value after a committed Apply.
    fn commit(&mut self) {
        if let Some(value) = self.staged.take() {
            self.original = Some(value);
        }
    }
}

/// One backing config file kept for surgical editing: its live path and current text.
#[derive(Clone, Debug)]
struct BackingText {
    /// The live XDG path (the [`FileWrite`] target; the writer follows symlinks, R8.5).
    path: PathBuf,
    /// The file's current text, re-parsed on each write so only the targeted value
    /// spans change (the surgical-edit rule, architecture §3).
    text: String,
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
pub(crate) struct ThemesModel {
    /// The GTK theme drop-down.
    gtk_theme: Selection,
    /// The icon theme drop-down.
    icon_theme: Selection,
    /// The cursor theme drop-down.
    cursor_theme: Selection,
    /// The cursor size drop-down (values held as strings; parsed when written).
    cursor_size: Selection,
    /// `gtk-3.0/settings.ini`, or `None` when it was unreadable (R4.4).
    gtk3: Option<BackingText>,
    /// `gtk-4.0/settings.ini`, or `None` when it was unreadable (R4.4).
    gtk4: Option<BackingText>,
    /// `hyprland.conf`, or `None` when it was unreadable — only its cursor `env =`
    /// lines are edited.
    hyprland: Option<BackingText>,
    /// `uwsm/env`, or `None` when it was unreadable — the canonical cursor env copy,
    /// and the source of the uwsm `GTK_THEME` override reading (R3.3).
    uwsm: Option<BackingText>,
    /// The active `GTK_THEME` override (app environment preferred over `uwsm/env`), or
    /// `None`. When `Some`, the GTK-theme drop-down is disabled and a banner shown
    /// (R3.3).
    gtk_override: Option<GtkThemeOverrideSource>,
    /// Whether a live theme-restyle path (settings portal or dconf) is available, so
    /// the UI may claim a live GTK-theme restyle rather than "next launch" (R2.2).
    live_restyle: bool,
    /// The freshness baseline for the backing files, recorded from the exact bytes
    /// read at load, so a pre-write conflict check catches an external edit and
    /// [`Self::commit`] re-baselines the app's own write (R5.6). Only readable files
    /// are tracked.
    freshness: FreshnessTracker,
    /// The theme roots, kept so [`Self::reload`] can re-discover on a conflict reload.
    roots: ThemeRoots,
    /// The backing paths, kept so [`Self::reload`] can re-read the files.
    paths: ThemesPaths,
    /// The app-environment `GTK_THEME` value, kept so [`Self::reload`] re-derives the
    /// override (the app's own environment does not change during the session, but the
    /// override is recomputed with the freshly re-read `uwsm/env`).
    app_env_gtk_theme: Option<String>,
}

/// The Theme page's GTK/icon/cursor contribution to an
/// [`ApplyPlan`](crate::core::apply::ApplyPlan): the file writes plus the reload
/// parameters (task 6.4).
///
/// A cursor change contributes writes for *all four* files carrying the value (both
/// `settings.ini`, `hyprland.conf`, `uwsm/env`) with the identical value, so they
/// never desync (R3.4); a GTK/icon theme change contributes only the two `settings.ini`
/// writes. The reload parameters carry only the values that changed, so the reload
/// table (task 4.4) emits `gsettings set` / `hyprctl setcursor` only for those.
pub(crate) struct ThemesApply {
    /// The atomic writes, one per changed backing file.
    pub(crate) writes: Vec<FileWrite>,
    /// The reload parameters for the changed theme/cursor values (the pipeline merges
    /// these into its plan-wide [`ReloadParams`]).
    pub(crate) reload_params: ReloadParams,
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
    /// rows via [`Self::themes_editable`], R4.4), and the current values are read from
    /// whichever `settings.ini` is present (with the cursor falling back to `uwsm/env`).
    pub(crate) fn load(
        roots: &ThemeRoots,
        paths: ThemesPaths,
        settings_portal_available: bool,
        app_env_gtk_theme: Option<String>,
    ) -> ThemesModel {
        let gtk3 = read_backing(&paths.gtk3_settings);
        let gtk4 = read_backing(&paths.gtk4_settings);
        let hyprland = read_backing(&paths.hyprland_conf);
        let uwsm = read_backing(&paths.uwsm_env);

        // Read current values from whichever settings.ini is present (prefer gtk-3.0),
        // since both carry the same keys. The cursor theme/size fall back to uwsm/env's
        // XCURSOR_* when settings.ini did not set them.
        let settings_ini = gtk3
            .as_ref()
            .or(gtk4.as_ref())
            .map(|backing| IniFile::parse(&backing.text).0);
        let uwsm_file = uwsm.as_ref().map(|backing| EnvFile::parse(&backing.text).0);

        let current_gtk = settings_value(&settings_ini, KEY_GTK_THEME);
        let current_icon = settings_value(&settings_ini, KEY_ICON_THEME);
        let current_cursor = settings_value(&settings_ini, KEY_CURSOR_THEME)
            .or_else(|| env_value(&uwsm_file, ENV_CURSOR_THEME));
        let current_size = settings_value(&settings_ini, KEY_CURSOR_SIZE)
            .or_else(|| env_value(&uwsm_file, ENV_CURSOR_SIZE));

        let gtk_themes = discover_gtk_themes(&roots.gtk_theme_dirs);
        let (icon_themes, cursor_themes) = discover_icon_and_cursor_themes(&roots.icon_dirs);

        // The app's own environment takes precedence: a GTK_THEME in it overrides this
        // very app, regardless of what uwsm/env says (R3.3).
        let uwsm_override = uwsm_file.as_ref().map(EnvFile::gtk_theme_override);
        let gtk_override = resolve_gtk_override(app_env_gtk_theme.clone(), uwsm_override.as_ref());

        let mut freshness = FreshnessTracker::new();
        for backing in [&gtk3, &gtk4, &hyprland, &uwsm].into_iter().flatten() {
            freshness.record_bytes(backing.path.as_path(), backing.text.as_bytes());
        }

        let curated_sizes: Vec<String> = CURATED_CURSOR_SIZES
            .iter()
            .map(|s| (*s).to_string())
            .collect();

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
            gtk3,
            gtk4,
            hyprland,
            uwsm,
            gtk_override,
            live_restyle: settings_portal_available,
            freshness,
            roots: roots.clone(),
            paths,
            app_env_gtk_theme,
        }
    }

    /// Whether the theme rows should be shown: at least one `settings.ini` was readable
    /// (R4.4).
    ///
    /// The GTK/icon/cursor values are read from — and written to — `settings.ini`, so
    /// when neither GTK 3 nor GTK 4 file can be read there is nothing to preselect or
    /// edit and the rows are hidden (the page shows a note instead), matching the
    /// Display page's "hide the file-backed controls when the config is unreadable"
    /// rule.
    pub(crate) fn themes_editable(&self) -> bool {
        self.gtk3.is_some() || self.gtk4.is_some()
    }

    /// The GTK theme drop-down options (installed GTK themes plus the current value).
    pub(crate) fn gtk_themes(&self) -> &[String] {
        &self.gtk_theme.options
    }

    /// The icon theme drop-down options.
    pub(crate) fn icon_themes(&self) -> &[String] {
        &self.icon_theme.options
    }

    /// The cursor theme drop-down options.
    pub(crate) fn cursor_themes(&self) -> &[String] {
        &self.cursor_theme.options
    }

    /// The cursor size drop-down options (curated sizes plus the current value).
    pub(crate) fn cursor_sizes(&self) -> &[String] {
        &self.cursor_size.options
    }

    /// The preselected index of the GTK theme drop-down.
    pub(crate) fn selected_gtk_index(&self) -> Option<usize> {
        self.gtk_theme.selected_index()
    }

    /// The preselected index of the icon theme drop-down.
    pub(crate) fn selected_icon_index(&self) -> Option<usize> {
        self.icon_theme.selected_index()
    }

    /// The preselected index of the cursor theme drop-down.
    pub(crate) fn selected_cursor_index(&self) -> Option<usize> {
        self.cursor_theme.selected_index()
    }

    /// The preselected index of the cursor size drop-down.
    pub(crate) fn selected_cursor_size_index(&self) -> Option<usize> {
        self.cursor_size.selected_index()
    }

    /// The active `GTK_THEME` override, or `None` (R3.3). When `Some`, the GTK-theme
    /// drop-down is disabled and a banner shown.
    pub(crate) fn gtk_override(&self) -> Option<&GtkThemeOverrideSource> {
        self.gtk_override.as_ref()
    }

    /// Whether the GTK-theme drop-down must be disabled — a live `GTK_THEME` override
    /// is in force, which the app must not fight (R3.3).
    pub(crate) fn gtk_dropdown_disabled(&self) -> bool {
        self.gtk_override.is_some()
    }

    /// Whether a live GTK-theme restyle can be claimed (a settings portal or dconf
    /// backend is available); otherwise a change takes effect at the next launch
    /// (R2.2).
    pub(crate) fn live_restyle(&self) -> bool {
        self.live_restyle
    }

    /// Stages a GTK theme switch (ignored when a `GTK_THEME` override is in force).
    pub(crate) fn stage_gtk_theme(&mut self, name: &str) {
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
    pub(crate) fn stage_icon_theme(&mut self, name: &str) {
        self.icon_theme.stage(name);
    }

    /// Stages a cursor theme switch.
    pub(crate) fn stage_cursor_theme(&mut self, name: &str) {
        self.cursor_theme.stage(name);
    }

    /// Stages a cursor size switch (the value is a pixel size as a string).
    pub(crate) fn stage_cursor_size(&mut self, size: &str) {
        self.cursor_size.stage(size);
    }

    /// Whether any theme/cursor value has a pending change — the page's dirty state,
    /// which the window folds into the global Apply/Reset chrome (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.gtk_theme.is_changed()
            || self.icon_theme.is_changed()
            || self.cursor_theme.is_changed()
            || self.cursor_size.is_changed()
    }

    /// Discards every staged theme/cursor change (R5.1).
    pub(crate) fn reset(&mut self) {
        self.gtk_theme.reset();
        self.icon_theme.reset();
        self.cursor_theme.reset();
        self.cursor_size.reset();
    }

    /// Whether any backing file changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty theme change; a `true` result
    /// means another program edited one of the backing files, so the write must be
    /// aborted and the model reloaded rather than clobbering the stale parse — the same
    /// discipline the Display page follows (the pipeline's own conflict check covers
    /// only the store's files, not these bespoke ones).
    pub(crate) fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Re-reads the backing files and re-discovers themes, returning a fresh model with
    /// a new freshness baseline (R5.6 "warn and re-load").
    ///
    /// Called after [`Self::check_conflict`] detects an external edit: the fresh model
    /// re-parses the current files (discarding the now-stale staged edits) so a
    /// subsequent Apply builds on the current contents.
    pub(crate) fn reload(&self) -> ThemesModel {
        ThemesModel::load(
            &self.roots,
            self.paths.clone(),
            self.live_restyle,
            self.app_env_gtk_theme.clone(),
        )
    }

    /// The Theme page's GTK/icon/cursor contribution to the Apply plan, or `None` when
    /// nothing changed (task 6.4).
    ///
    /// Renders each changed value into the files that carry it through the surgical
    /// parsers (§3) and collects the reload parameters. A cursor change produces writes
    /// for both `settings.ini` files, `hyprland.conf`, and `uwsm/env` with the
    /// **identical** value (R3.4); a GTK/icon theme change produces only the two
    /// `settings.ini` writes.
    pub(crate) fn apply_contribution(&self) -> Option<ThemesApply> {
        if !self.is_dirty() {
            return None;
        }
        let writes = self.build_writes();
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

    /// Commits the staged changes after a successful Apply: re-baselines each written
    /// file's freshness from the exact bytes written, updates the in-memory backing
    /// text, and promotes each staged selection to its current value (R5.6).
    ///
    /// Re-baselining is what stops the app's own write being mistaken for an external
    /// conflict on the next Apply; updating the backing text keeps the in-memory copy
    /// in step so a subsequent edit builds on the current bytes.
    pub(crate) fn commit(&mut self) {
        // Re-render the writes (staged values still present) to capture the exact bytes
        // written, then re-baseline and update the stored text for each.
        for write in self.build_writes() {
            self.freshness
                .record_bytes(write.path.as_path(), &write.contents);
            if let Ok(text) = String::from_utf8(write.contents.clone()) {
                self.set_backing_text(&write.path, text);
            }
        }
        self.gtk_theme.commit();
        self.icon_theme.commit();
        self.cursor_theme.commit();
        self.cursor_size.commit();
    }

    /// Renders the file writes for the current staged changes (used by both
    /// [`Self::apply_contribution`] and [`Self::commit`]).
    fn build_writes(&self) -> Vec<FileWrite> {
        let gtk_changed = self.gtk_theme.is_changed();
        let icon_changed = self.icon_theme.is_changed();
        let cursor_theme_changed = self.cursor_theme.is_changed();
        let cursor_size_changed = self.cursor_size.is_changed();

        let mut writes = Vec::new();

        // Every theme/cursor key lives in settings.ini, so any change writes both files.
        if gtk_changed || icon_changed || cursor_theme_changed || cursor_size_changed {
            for backing in [&self.gtk3, &self.gtk4].into_iter().flatten() {
                if let Some(write) = self.render_settings_ini(
                    backing,
                    gtk_changed,
                    icon_changed,
                    cursor_theme_changed,
                    cursor_size_changed,
                ) {
                    writes.push(write);
                }
            }
        }

        // The cursor is additionally duplicated in hyprland.conf's env lines and
        // uwsm/env; write the identical value there whenever the cursor changed (R3.4).
        if cursor_theme_changed || cursor_size_changed {
            if let Some(backing) = &self.hyprland {
                if let Some(write) =
                    self.render_hyprland_env(backing, cursor_theme_changed, cursor_size_changed)
                {
                    writes.push(write);
                }
            }
            if let Some(backing) = &self.uwsm {
                if let Some(write) =
                    self.render_uwsm_env(backing, cursor_theme_changed, cursor_size_changed)
                {
                    writes.push(write);
                }
            }
        }

        writes
    }

    /// Renders one `settings.ini` write, editing only the keys that changed.
    fn render_settings_ini(
        &self,
        backing: &BackingText,
        gtk_changed: bool,
        icon_changed: bool,
        cursor_theme_changed: bool,
        cursor_size_changed: bool,
    ) -> Option<FileWrite> {
        let (mut ini, _) = IniFile::parse(&backing.text);
        let mut changed_keys = Vec::new();

        let mut set = |key: &str, value: Option<&str>, label: &str| {
            if let Some(value) = value {
                match ini.set_value(SETTINGS_GROUP, key, value) {
                    Ok(_) => changed_keys.push(label.to_string()),
                    Err(error) => {
                        tracing::warn!(key, %error, "could not set a settings.ini theme key");
                    }
                }
            }
        };
        if gtk_changed {
            set(KEY_GTK_THEME, self.gtk_theme.effective(), "GTK theme");
        }
        if icon_changed {
            set(KEY_ICON_THEME, self.icon_theme.effective(), "icon theme");
        }
        if cursor_theme_changed {
            set(
                KEY_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme",
            );
        }
        if cursor_size_changed {
            set(KEY_CURSOR_SIZE, self.cursor_size.effective(), "cursor size");
        }

        if changed_keys.is_empty() {
            return None;
        }
        Some(FileWrite {
            path: backing.path.clone(),
            contents: ini.emit().into_bytes(),
            changed_keys,
            backing: BackingFile::GtkSettings,
        })
    }

    /// Renders the `hyprland.conf` cursor-env write, editing only the repeatable
    /// `env = XCURSOR_*` lines' value portions.
    ///
    /// Each field is applied independently: a `env = XCURSOR_*` line that is absent
    /// (the hyprlang repeatable writer never appends one) is skipped and logged at
    /// `debug`, but a field whose line *does* exist is still written — so if only one
    /// of the two lines is present, hyprland.conf receives that field rather than being
    /// abandoned wholesale, keeping it from drifting out of step with the other copies.
    /// Returns `None` only when neither field could be written (the app-owned invariant
    /// is that both lines are present, so this partial path is a robustness measure).
    fn render_hyprland_env(
        &self,
        backing: &BackingText,
        cursor_theme_changed: bool,
        cursor_size_changed: bool,
    ) -> Option<FileWrite> {
        let (mut file, _) = HyprlangFile::parse(&backing.text);
        let mut changed_keys = Vec::new();

        if cursor_theme_changed {
            if let Some(value) = self.cursor_theme.effective() {
                match file.set_repeatable_field_value(HYPR_ENV_KEY, ENV_CURSOR_THEME, value) {
                    Ok(()) => changed_keys.push("cursor theme (hyprland.conf env)".to_string()),
                    Err(error) => {
                        tracing::debug!(%error, "no XCURSOR_THEME env line in hyprland.conf; skipping that field");
                    }
                }
            }
        }
        if cursor_size_changed {
            if let Some(value) = self.cursor_size.effective() {
                match file.set_repeatable_field_value(HYPR_ENV_KEY, ENV_CURSOR_SIZE, value) {
                    Ok(()) => changed_keys.push("cursor size (hyprland.conf env)".to_string()),
                    Err(error) => {
                        tracing::debug!(%error, "no XCURSOR_SIZE env line in hyprland.conf; skipping that field");
                    }
                }
            }
        }

        if changed_keys.is_empty() {
            return None;
        }
        Some(FileWrite {
            path: backing.path.clone(),
            contents: file.emit().into_bytes(),
            changed_keys,
            backing: BackingFile::HyprlandConf,
        })
    }

    /// Renders the `uwsm/env` cursor-env write, editing (or appending) the
    /// `XCURSOR_*` exports.
    fn render_uwsm_env(
        &self,
        backing: &BackingText,
        cursor_theme_changed: bool,
        cursor_size_changed: bool,
    ) -> Option<FileWrite> {
        let (mut file, _) = EnvFile::parse(&backing.text);
        let mut changed_keys = Vec::new();

        let mut set = |key: &str, value: Option<&str>, label: &str| {
            if let Some(value) = value {
                match file.set_value(key, value) {
                    Ok(_) => changed_keys.push(label.to_string()),
                    Err(error) => {
                        tracing::warn!(key, %error, "could not set a uwsm/env cursor variable");
                    }
                }
            }
        };
        if cursor_theme_changed {
            set(
                ENV_CURSOR_THEME,
                self.cursor_theme.effective(),
                "cursor theme (uwsm/env)",
            );
        }
        if cursor_size_changed {
            set(
                ENV_CURSOR_SIZE,
                self.cursor_size.effective(),
                "cursor size (uwsm/env)",
            );
        }

        if changed_keys.is_empty() {
            return None;
        }
        Some(FileWrite {
            path: backing.path.clone(),
            contents: file.emit().into_bytes(),
            changed_keys,
            backing: BackingFile::UwsmEnv,
        })
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

    /// Updates the in-memory backing text for the file at `path` after a commit, so a
    /// subsequent edit re-parses the current bytes.
    fn set_backing_text(&mut self, path: &Path, text: String) {
        for backing in [
            self.gtk3.as_mut(),
            self.gtk4.as_mut(),
            self.hyprland.as_mut(),
            self.uwsm.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            if backing.path == path {
                backing.text = text;
                return;
            }
        }
    }
}

/// Reads a backing config file into a [`BackingText`], or `None` when it is unreadable
/// (missing, permission-revoked, or non-UTF-8) — logged at `debug`.
///
/// A read failure is not surfaced at `warn` here: the section-level gating
/// ([`ThemesModel::themes_editable`]) reports a missing `settings.ini`, and detection
/// already logs the primary gates (R4.4). A missing secondary cursor copy
/// (`hyprland.conf`/`uwsm/env`) just means that copy is skipped, so `debug` is right.
fn read_backing(path: &Path) -> Option<BackingText> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(BackingText {
            path: path.to_path_buf(),
            text,
        }),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "theme backing file unreadable; that copy is skipped");
            None
        }
    }
}

/// Reads a `[Settings]` key from a parsed `settings.ini`, if present.
fn settings_value(ini: &Option<IniFile>, key: &str) -> Option<String> {
    ini.as_ref()
        .and_then(|file| file.value(SETTINGS_GROUP, key))
        .map(str::to_string)
}

/// Reads a variable from a parsed `uwsm/env`, if present.
fn env_value(env: &Option<EnvFile>, key: &str) -> Option<String> {
    env.as_ref()
        .and_then(|file| file.value(key))
        .map(str::to_string)
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

/// Discovers installed GTK themes under `dirs` (R3.3).
///
/// A subdirectory is a GTK theme when it contains a `gtk-3.0/` or `gtk-4.0/`
/// subdirectory. Names are de-duplicated across the roots (a theme in `~/.themes`
/// shadows a system one of the same name — only the name matters, since that is what
/// `gsettings`/`settings.ini` store) and returned sorted for a stable drop-down.
fn discover_gtk_themes(dirs: &[PathBuf]) -> Vec<String> {
    collect_theme_dirs(dirs, |path| {
        path.join("gtk-3.0").is_dir() || path.join("gtk-4.0").is_dir()
    })
    .into_iter()
    .collect()
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

// ===========================================================================
// Wallpaper / lock-screen background model (task 6.5; R3.x, R4.2, R4.4, R5.6,
// R8.3, R6.2)
// ===========================================================================

/// The `hyprpaper.conf` section holding the wallpaper (`wallpaper { path = … }`).
const WALLPAPER_SECTION: &str = "wallpaper";
/// The key naming the wallpaper image path inside the `wallpaper` section.
const WALLPAPER_PATH_KEY: &str = "path";
/// The key naming the wallpaper fit mode inside the `wallpaper` section (analysis §4:
/// `fit_mode = cover`).
const WALLPAPER_FIT_KEY: &str = "fit_mode";
/// The `hyprlock.conf` section holding the lock-screen background
/// (`background { path = … }`).
const LOCK_SECTION: &str = "background";
/// The key naming the lock-screen background image path inside the `background` section.
const LOCK_PATH_KEY: &str = "path";

/// Fit modes offered in the fit-mode drop-down, in a sensible order.
///
/// These are hyprpaper's genuine wallpaper rendering modes: `cover` (fill the output,
/// preserving aspect ratio and cropping — the dotfiles' documented value, analysis §4),
/// `contain` (fit the whole image, letterboxing), `fill` (stretch to fill, ignoring
/// aspect ratio), and `tile` (repeat). The currently-configured value is always kept
/// selectable (see [`WallpaperModel::load`]), mirroring the cursor-size and
/// monitor-scale drop-downs, so an unusual on-disk value is never silently dropped.
const CURATED_FIT_MODES: &[&str] = &["cover", "contain", "fill", "tile"];

/// A free-form path setting with an original (on-disk) value and an optional staged
/// edit.
///
/// Mirrors [`Selection`]'s dirty rule for a value that is *not* a fixed drop-down set:
/// re-staging the current value clears the pending edit, so it never lights up Apply.
/// Used for the wallpaper and lock-screen image paths, which the user picks with a file
/// chooser rather than from a list.
#[derive(Clone, Debug)]
struct PathField {
    /// The path read from the backing config, or `None` when the config did not set it.
    original: Option<String>,
    /// The pending path, or `None` when nothing is staged. Only ever set to a value
    /// that differs from [`original`](Self::original).
    staged: Option<String>,
}

impl PathField {
    /// Builds a path field over a current value.
    fn new(original: Option<String>) -> Self {
        PathField {
            original,
            staged: None,
        }
    }

    /// The effective value — the staged path if pending, else the current one.
    fn effective(&self) -> Option<&str> {
        self.staged.as_deref().or(self.original.as_deref())
    }

    /// Stages `value`, clearing the pending edit when it equals the current value.
    fn stage(&mut self, value: &str) {
        if self.original.as_deref() == Some(value) {
            self.staged = None;
        } else {
            self.staged = Some(value.to_string());
        }
    }

    /// Whether a pending edit differing from the current value exists.
    fn is_changed(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards the pending edit.
    fn reset(&mut self) {
        self.staged = None;
    }

    /// Promotes the pending edit to the current value after a committed Apply.
    fn commit(&mut self) {
        if let Some(value) = self.staged.take() {
            self.original = Some(value);
        }
    }
}

/// The live XDG paths of the two config files a wallpaper / lock-background change
/// writes (R8.5).
///
/// Injected (like [`ThemesPaths`]) so the model is exercised against a fixture tree in
/// tests; the writer follows symlinks, so a dotfiles-deployed file is handled
/// identically to a plain one.
#[derive(Clone, Debug)]
pub(crate) struct WallpaperPaths {
    /// `~/.config/hypr/hyprpaper.conf` (the wallpaper `path` and `fit_mode`).
    pub(crate) hyprpaper_conf: PathBuf,
    /// `~/.config/hypr/hyprlock.conf` (the lock-screen background `path`).
    pub(crate) hyprlock_conf: PathBuf,
}

/// The wallpaper / lock-screen background staging model (task 6.5).
///
/// Built by [`WallpaperModel::load`] from the backing [`WallpaperPaths`]. It owns the
/// wallpaper path, the fit-mode [`Selection`], the optional lock-screen override state
/// and its path, the two backing config texts, and a freshness baseline for conflict
/// detection (R5.6). Its file edits reach the shared Apply pipeline through
/// [`Self::apply_contribution`]; the window folds them into the same
/// [`apply::run`](crate::core::apply::run) it drives for the store and the other Theme
/// models.
///
/// # Single wallpaper, optional lock override (analysis §6.2)
///
/// The dotfiles unify the wallpaper and the lock-screen background to the same image,
/// so the UI presents *one* wallpaper path (plus a fit mode) and an optional "use a
/// different lock-screen image" toggle. The path actually written to `hyprlock.conf` is
/// the override path when the toggle is on, otherwise the wallpaper path — so with the
/// toggle off the two files stay in sync automatically.
///
/// # Reload asymmetry (the load-bearing hyprlock rule)
///
/// A change that writes `hyprpaper.conf` reloads live (`hyprctl hyprpaper preload` +
/// `wallpaper`); a change that writes **only** `hyprlock.conf` issues no reload command
/// at all — hyprlock reads its config only at launch, so a lock-background change takes
/// effect at the next lock (intentional, architecture §6). This is enforced by the
/// reload table ([`BackingFile::HyprlockConf`] maps to no action) and by
/// [`Self::reload_params`] setting the wallpaper reload parameter only when
/// `hyprpaper.conf` is written.
///
/// It stays GTK-free so the read, staging, the surgical writes, and the reload decision
/// are unit-tested headlessly (R6.2); the layering guard in
/// `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
pub(crate) struct WallpaperModel {
    /// `hyprpaper.conf`, or `None` when it was unreadable (R4.4) — the wallpaper rows
    /// are then hidden.
    hyprpaper: Option<BackingText>,
    /// `hyprlock.conf`, or `None` when it was unreadable (R4.4) — the lock override is
    /// then unavailable.
    hyprlock: Option<BackingText>,
    /// The wallpaper image path (`hyprpaper.conf` `wallpaper.path`).
    wallpaper: PathField,
    /// The wallpaper fit mode (`hyprpaper.conf` `wallpaper.fit_mode`).
    fit: Selection,
    /// The lock-screen override image path (`hyprlock.conf` `background.path`). Only
    /// consulted when [`override_on`](Self::override_on) is set.
    lock: PathField,
    /// Whether the lock screen uses a *different* image than the wallpaper (the current
    /// state of the override toggle). Seeded from whether the two on-disk paths already
    /// differ (see [`Self::load`]).
    override_on: bool,
    /// The override toggle's baseline, so [`Self::reset`] restores it and
    /// [`Self::commit`] re-baselines it.
    override_initial: bool,
    /// Whether hyprlock is present at all (its binary was detected). When `false` the
    /// lock override control is hidden and `hyprlock.conf` is never written (R4.2), even
    /// if the file happens to be readable.
    lock_available: bool,
    /// The freshness baseline for the two backing files, recorded from the exact bytes
    /// read at load, so a pre-write conflict check catches an external edit and
    /// [`Self::commit`] re-baselines the app's own write (R5.6). Only readable files are
    /// tracked.
    freshness: FreshnessTracker,
    /// The backing paths, kept so [`Self::reload`] can re-read the files.
    paths: WallpaperPaths,
}

/// The Theme page's wallpaper / lock-background contribution to an
/// [`ApplyPlan`](crate::core::apply::ApplyPlan): the file writes, the reload
/// parameters, and the value validations to re-check (task 6.5).
///
/// A wallpaper-path or fit-mode change contributes a `hyprpaper.conf` write and sets
/// the wallpaper reload parameter; the lock-screen background (the same wallpaper path,
/// or the override) contributes a `hyprlock.conf` write with **no** reload. The
/// validations re-check the chosen image paths at apply time (R8.3), so a path deleted
/// between staging and Apply is caught before any write.
pub(crate) struct WallpaperApply {
    /// The atomic writes, one per changed backing file.
    pub(crate) writes: Vec<FileWrite>,
    /// The reload parameters — only the wallpaper path, and only when `hyprpaper.conf`
    /// is written (the pipeline merges these into its plan-wide [`ReloadParams`]).
    pub(crate) reload_params: ReloadParams,
    /// The chosen image paths to validate before writing (R8.3), reusing the
    /// [`SettingId`] image-path validator.
    pub(crate) validations: Vec<(SettingId, Value)>,
}

impl WallpaperModel {
    /// Builds the model by reading the backing config (task 6.5; R4.4, R8.5).
    ///
    /// `paths` is injected (see [`WallpaperPaths`]); `lock_available` is whether the
    /// hyprlock binary was detected — when `false` the lock override is hidden and
    /// `hyprlock.conf` is never written (R4.2). Nothing here fails: an unreadable file
    /// simply yields no backing text (its controls degrade, R4.4). The override toggle
    /// starts on only when the lock-screen path already differs from the wallpaper path
    /// on disk (i.e. the two are not the unified default).
    pub(crate) fn load(paths: WallpaperPaths, lock_available: bool) -> WallpaperModel {
        let hyprpaper = read_backing(&paths.hyprpaper_conf);
        let hyprlock = read_backing(&paths.hyprlock_conf);

        let hyprpaper_file = hyprpaper
            .as_ref()
            .map(|backing| HyprlangFile::parse(&backing.text).0);
        let hyprlock_file = hyprlock
            .as_ref()
            .map(|backing| HyprlangFile::parse(&backing.text).0);

        let wallpaper_path = hyprpaper_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_PATH_KEY)))
            .map(str::to_string);
        let fit = hyprpaper_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_FIT_KEY)))
            .map(str::to_string);
        let lock_path = hyprlock_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[LOCK_SECTION], LOCK_PATH_KEY)))
            .map(str::to_string);

        // The override is on iff the lock-screen path is set and differs from the
        // wallpaper path — i.e. the two are not the unified same-image default.
        let override_initial =
            lock_path.is_some() && lock_path.as_deref() != wallpaper_path.as_deref();

        let fit_options: Vec<String> = CURATED_FIT_MODES.iter().map(|s| (*s).to_string()).collect();

        let mut freshness = FreshnessTracker::new();
        for backing in [&hyprpaper, &hyprlock].into_iter().flatten() {
            freshness.record_bytes(backing.path.as_path(), backing.text.as_bytes());
        }

        tracing::info!(
            wallpaper = wallpaper_path.is_some(),
            lock = lock_path.is_some(),
            override_on = override_initial,
            lock_available,
            "loaded wallpaper / lock background for the Theme page (task 6.5)"
        );

        WallpaperModel {
            hyprpaper,
            hyprlock,
            wallpaper: PathField::new(wallpaper_path),
            fit: Selection::new(fit_options, fit),
            lock: PathField::new(lock_path),
            override_on: override_initial,
            override_initial,
            lock_available,
            freshness,
            paths,
        }
    }

    /// Whether the wallpaper rows should be shown: `hyprpaper.conf` was readable, so a
    /// path/fit edit can be written (R4.4).
    pub(crate) fn wallpaper_editable(&self) -> bool {
        self.hyprpaper.is_some()
    }

    /// Whether the lock-screen override control should be shown: hyprlock is present and
    /// `hyprlock.conf` is readable, so an override can be written (R4.2/R4.4).
    pub(crate) fn lock_editable(&self) -> bool {
        self.lock_available && self.hyprlock.is_some()
    }

    /// The effective wallpaper image path (staged or current), or `None` when unset.
    pub(crate) fn wallpaper_path(&self) -> Option<&str> {
        self.wallpaper.effective()
    }

    /// The fit-mode drop-down options (the curated modes plus the current value).
    pub(crate) fn fit_options(&self) -> &[String] {
        &self.fit.options
    }

    /// The preselected index of the fit-mode drop-down.
    pub(crate) fn selected_fit_index(&self) -> Option<usize> {
        self.fit.selected_index()
    }

    /// Whether the lock-screen override is on (a different image than the wallpaper).
    pub(crate) fn override_on(&self) -> bool {
        self.override_on
    }

    /// The effective lock-screen override image path (staged or current), or `None`.
    /// Only meaningful while [`Self::override_on`] is set; the UI shows it in the
    /// override chooser.
    pub(crate) fn lock_path(&self) -> Option<&str> {
        self.lock.effective()
    }

    /// Stages a wallpaper image path after validating it (R8.3).
    ///
    /// The path must exist, be readable, and have an image extension; an invalid path is
    /// rejected (returned as an [`Err`] the UI surfaces) and nothing is staged, so a
    /// broken wallpaper can never be written.
    pub(crate) fn stage_wallpaper(&mut self, path: &str) -> Result<(), ValidationError> {
        validate_image_path(Path::new(path))?;
        self.wallpaper.stage(path);
        Ok(())
    }

    /// Stages a lock-screen override image path after validating it (R8.3), like
    /// [`Self::stage_wallpaper`].
    pub(crate) fn stage_lock(&mut self, path: &str) -> Result<(), ValidationError> {
        validate_image_path(Path::new(path))?;
        self.lock.stage(path);
        Ok(())
    }

    /// Stages a fit mode (a value from the fit-mode drop-down; no path validation).
    pub(crate) fn stage_fit(&mut self, fit: &str) {
        self.fit.stage(fit);
    }

    /// Sets the lock-screen override toggle. Turning it off makes the lock screen follow
    /// the wallpaper again; turning it on makes it follow the (separately chosen) lock
    /// path.
    pub(crate) fn set_override(&mut self, on: bool) {
        self.override_on = on;
    }

    /// Whether `hyprpaper.conf` needs writing — a wallpaper path or fit-mode change,
    /// and the file is editable.
    fn hyprpaper_write_needed(&self) -> bool {
        self.wallpaper_editable() && (self.wallpaper.is_changed() || self.fit.is_changed())
    }

    /// The image path that should be written to `hyprlock.conf`: the override path when
    /// the override is on, otherwise the wallpaper path (so the two stay in sync).
    fn effective_lock(&self) -> Option<&str> {
        if self.override_on {
            self.lock.effective()
        } else {
            self.wallpaper.effective()
        }
    }

    /// Whether `hyprlock.conf` needs writing — the effective lock path differs from
    /// what is on disk, hyprlock is editable, and there is a path to write.
    fn hyprlock_write_needed(&self) -> bool {
        if !self.lock_editable() {
            return false;
        }
        match self.effective_lock() {
            Some(path) => Some(path) != self.lock.original.as_deref(),
            None => false,
        }
    }

    /// Whether any wallpaper / lock-background value has a pending change — the page's
    /// dirty state, which the window folds into the global Apply/Reset chrome (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.hyprpaper_write_needed() || self.hyprlock_write_needed()
    }

    /// Discards every staged change, returning the toggle to its baseline (R5.1).
    pub(crate) fn reset(&mut self) {
        self.wallpaper.reset();
        self.fit.reset();
        self.lock.reset();
        self.override_on = self.override_initial;
    }

    /// Whether either backing file changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty change; a `true` result means
    /// another program edited one of the files, so the write must be aborted and the
    /// model reloaded rather than clobbering the stale parse — the same discipline the
    /// Display and GTK/icon/cursor models follow (the pipeline's own conflict check
    /// covers only the store's files, not these bespoke ones).
    pub(crate) fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Re-reads the backing files, returning a fresh model with a new freshness baseline
    /// (R5.6 "warn and re-load").
    pub(crate) fn reload(&self) -> WallpaperModel {
        WallpaperModel::load(self.paths.clone(), self.lock_available)
    }

    /// The Theme page's wallpaper / lock-background contribution to the Apply plan, or
    /// `None` when nothing changed (task 6.5).
    pub(crate) fn apply_contribution(&self) -> Option<WallpaperApply> {
        if !self.is_dirty() {
            return None;
        }
        let writes = self.build_writes();
        if writes.is_empty() {
            // Dirty but nothing could be written (e.g. a parser edit error). Nothing to
            // apply; the page stays dirty for a retry.
            tracing::warn!(
                "wallpaper change is dirty but no backing file could be written; skipping the wallpaper apply (R4.4)"
            );
            return None;
        }
        Some(WallpaperApply {
            writes,
            reload_params: self.reload_params(),
            validations: self.validations(),
        })
    }

    /// Commits the staged changes after a successful Apply: re-baselines each written
    /// file's freshness from the exact bytes written, updates the in-memory backing
    /// text, and promotes each staged value to its current value (R5.6).
    pub(crate) fn commit(&mut self) {
        for write in self.build_writes() {
            self.freshness
                .record_bytes(write.path.as_path(), &write.contents);
            if let Ok(text) = String::from_utf8(write.contents.clone()) {
                self.set_backing_text(&write.path, text);
            }
        }
        // Capture the effective lock path (which depends on the still-staged wallpaper
        // value when the override is off) before promoting the wallpaper edit.
        let wrote_lock = self.hyprlock_write_needed();
        let new_lock = self.effective_lock().map(str::to_string);
        self.wallpaper.commit();
        self.fit.commit();
        if wrote_lock {
            self.lock.original = new_lock;
        }
        self.lock.staged = None;
        self.override_initial = self.override_on;
    }

    /// Renders the file writes for the current staged changes (used by both
    /// [`Self::apply_contribution`] and [`Self::commit`]).
    fn build_writes(&self) -> Vec<FileWrite> {
        let mut writes = Vec::new();
        if self.hyprpaper_write_needed() {
            if let Some(backing) = &self.hyprpaper {
                if let Some(write) = self.render_hyprpaper(backing) {
                    writes.push(write);
                }
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(backing) = &self.hyprlock {
                if let Some(write) = self.render_hyprlock(backing) {
                    writes.push(write);
                }
            }
        }
        writes
    }

    /// Renders the `hyprpaper.conf` write, editing only the `wallpaper.path` and/or
    /// `wallpaper.fit_mode` value spans that changed (surgical, R5.3).
    fn render_hyprpaper(&self, backing: &BackingText) -> Option<FileWrite> {
        let (mut file, _) = HyprlangFile::parse(&backing.text);
        let mut changed_keys = Vec::new();

        if self.wallpaper.is_changed() {
            if let Some(path) = self.wallpaper.effective() {
                match file.set_value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_PATH_KEY), path) {
                    Ok(()) => changed_keys.push("wallpaper path".to_string()),
                    Err(error) => {
                        tracing::warn!(%error, "could not set the wallpaper path in hyprpaper.conf");
                    }
                }
            }
        }
        if self.fit.is_changed() {
            if let Some(fit) = self.fit.effective() {
                match file.set_value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_FIT_KEY), fit) {
                    Ok(()) => changed_keys.push("fit mode".to_string()),
                    Err(error) => {
                        tracing::warn!(%error, "could not set the fit mode in hyprpaper.conf");
                    }
                }
            }
        }

        if changed_keys.is_empty() {
            return None;
        }
        Some(FileWrite {
            path: backing.path.clone(),
            contents: file.emit().into_bytes(),
            changed_keys,
            backing: BackingFile::HyprpaperConf,
        })
    }

    /// Renders the `hyprlock.conf` write, editing only the `background.path` value span
    /// to the effective lock path (surgical, R5.3).
    fn render_hyprlock(&self, backing: &BackingText) -> Option<FileWrite> {
        let path = self.effective_lock()?;
        let (mut file, _) = HyprlangFile::parse(&backing.text);
        match file.set_value(&KeyPath::at(&[LOCK_SECTION], LOCK_PATH_KEY), path) {
            Ok(()) => Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys: vec!["lock background path".to_string()],
                backing: BackingFile::HyprlockConf,
            }),
            Err(error) => {
                tracing::warn!(%error, "could not set the lock background path in hyprlock.conf");
                None
            }
        }
    }

    /// The reload parameters for the change (task 4.4).
    ///
    /// The wallpaper path **and** the fit mode are set only when `hyprpaper.conf` is
    /// written, so the live `hyprctl hyprpaper wallpaper <monitor>,<path>,<fit>` applies
    /// the current fit (a fit-only change re-sets the same image with the new fit). A
    /// hyprlock-only change carries no wallpaper parameter — and, combined with
    /// [`BackingFile::HyprlockConf`] mapping to no action, issues no reload command at
    /// all (the intentional hyprlock behaviour, architecture §6).
    fn reload_params(&self) -> ReloadParams {
        let (wallpaper, fit) = if self.hyprpaper_write_needed() {
            (
                self.wallpaper.effective().map(str::to_string),
                self.fit.effective().map(str::to_string),
            )
        } else {
            (None, None)
        };
        ReloadParams {
            wallpaper,
            fit,
            cursor: None,
            gtk_theme: None,
            icon_theme: None,
        }
    }

    /// The chosen image paths to validate before writing (R8.3): the wallpaper path only
    /// when it is actually being (re)written (a fit-only change never rewrites — nor
    /// should it re-validate — the unchanged wallpaper path), and the effective lock
    /// path when `hyprlock.conf` is written.
    fn validations(&self) -> Vec<(SettingId, Value)> {
        let mut validations = Vec::new();
        if self.wallpaper.is_changed() {
            if let Some(path) = self.wallpaper.effective() {
                validations.push((SettingId::WallpaperPath, Value::String(path.to_string())));
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(path) = self.effective_lock() {
                validations.push((
                    SettingId::LockBackgroundPath,
                    Value::String(path.to_string()),
                ));
            }
        }
        validations
    }

    /// Updates the in-memory backing text for the file at `path` after a commit, so a
    /// subsequent edit re-parses the current bytes.
    fn set_backing_text(&mut self, path: &Path, text: String) {
        for backing in [self.hyprpaper.as_mut(), self.hyprlock.as_mut()]
            .into_iter()
            .flatten()
        {
            if backing.path == path {
                backing.text = text;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use nix::sys::signal::Signal;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities, Daemon};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::reload::ReloadParams;
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::{MockProcessSignaller, SignalCall};

    /// A complete, schema-valid `colors/<scheme>` source with all 17 keys.
    const VALID_SCHEME: &str = "\
bg0=272e33
bg1=2e383c
bg2=374145
bg3=414b50
fg0=d3c6aa
fg1=9da9a0
fg2=859289
accent0=83c092
accent1=a7c080
accent2=7fbbb3
accent3=d699b6
red=e67e80
orange=e69875
yellow=dbbc7f
green=a7c080
blue=7fbbb3
purple=d699b6
";

    /// Writes a `colors/` directory with the given scheme files, plus the non-scheme
    /// entries the enumeration must skip, and returns the directory path.
    ///
    /// Every named scheme is written as a complete valid palette. In addition it
    /// writes a `README.md` (a non-palette file), a `.hidden` dotfile (whose *content*
    /// is a valid palette, to prove the dotfile is skipped by name, not content), an
    /// `active-scheme`-style single-line marker, and a subdirectory — none of which
    /// may appear as a scheme.
    fn write_colors_dir(dir: &Path, schemes: &[&str]) {
        fs::create_dir_all(dir).expect("create colors dir");
        for scheme in schemes {
            fs::write(dir.join(scheme), VALID_SCHEME).expect("write a scheme file");
        }
        // Non-palette file: markdown, not a scheme.
        fs::write(dir.join("README.md"), b"# Palette schemes\n").expect("write README");
        // A dotfile whose content is a valid palette — must still be skipped by name.
        fs::write(dir.join(".hidden"), VALID_SCHEME).expect("write a hidden file");
        // A `state/active-scheme`-style marker: a single scheme name on one line. It
        // lives outside colors/ in the real repo, but if one appeared here it must not
        // be surfaced as a scheme.
        fs::write(dir.join("active-scheme"), b"nord\n").expect("write a marker");
        // A subdirectory must be skipped.
        fs::create_dir_all(dir.join("subdir")).expect("create a subdir");
        fs::write(dir.join("subdir").join("nested"), VALID_SCHEME).expect("write nested file");
    }

    /// A generated `colors.conf` naming `scheme` in its header (task 3.7).
    fn write_active_source(path: &Path, scheme: &str) {
        fs::write(
            path,
            format!(
                "# Generated from colors/{scheme} — do not edit manually\n$bg0 = rgb(272e33)\n"
            ),
        )
        .expect("write generated colors.conf");
    }

    #[test]
    fn enumeration_skips_dotfiles_subdirs_and_non_palette_files() {
        // Accept criterion: the scheme list is exactly the two palette files, with the
        // README, the dotfile, the state-marker, and the subdirectory all skipped —
        // so a marker never appears as a scheme.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);

        let schemes = enumerate_schemes(&colors);
        let names: Vec<&str> = schemes.iter().map(Scheme::name).collect();
        assert_eq!(
            names,
            vec!["everforest", "nord"],
            "only the two palette files are schemes, sorted by name"
        );
    }

    #[test]
    fn a_valid_scheme_carries_a_parsed_preview() {
        // The swatch parse (task 3.7) feeds a preview strip: a complete palette yields
        // all 17 colors as RGB, and the first entry (`bg0=272e33`) parses correctly.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["nord"]);

        let schemes = enumerate_schemes(&colors);
        let nord = schemes.iter().find(|s| s.name() == "nord").expect("nord");
        assert_eq!(
            nord.preview().len(),
            17,
            "a complete palette previews all 17"
        );
        let (r, g, b) = nord.preview()[0];
        assert!((r - f64::from(0x27) / 255.0).abs() < 1e-9);
        assert!((g - f64::from(0x2e) / 255.0).abs() < 1e-9);
        assert!((b - f64::from(0x33) / 255.0).abs() < 1e-9);
    }

    #[test]
    fn active_scheme_is_detected_and_preselected() {
        // Accept criterion: the active scheme is detected from the generated header
        // (task 3.7) and preselected in the drop-down.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert_eq!(model.active(), Some("nord"));
        assert_eq!(
            model.selected(),
            Some("nord"),
            "the active scheme is preselected"
        );
        // schemes are [everforest, nord]; nord is at index 1.
        assert_eq!(model.selected_index(), Some(1));
        assert!(!model.is_dirty(), "no switch is staged at load");
    }

    #[test]
    fn an_undetectable_active_scheme_preselects_nothing() {
        // Degraded path (task 6.3 review S2): the generated header does not name a
        // recognizable scheme, so detection degrades to `Unknown`. With no active
        // scheme there is nothing to preselect even though the schemes exist — the
        // UI must NOT fall back to GTK's index-0 default and present the first scheme
        // as if it were active.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        // A file with no `# Generated from colors/<scheme>` header degrades to
        // `ActiveScheme::Unknown`, so the model has no active scheme.
        let active_source = dir.path().join("colors.conf");
        fs::write(&active_source, "$bg0 = rgb(272e33)\n").expect("write a headerless file");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert!(model.is_switchable(), "two schemes are still switchable");
        assert_eq!(model.active(), None, "an unrecognized header is unknown");
        assert_eq!(
            model.selected(),
            None,
            "nothing is selected when the active scheme is unknown"
        );
        assert_eq!(
            model.selected_index(),
            None,
            "no drop-down index is preselected when the active scheme is unknown"
        );
    }

    #[test]
    fn an_active_scheme_absent_from_colors_preselects_nothing() {
        // Degraded path (task 6.3 review S2): the generated header names a scheme
        // that is not among the enumerated `colors/` files (e.g. its source file was
        // deleted or renamed). The active name is still reported, but it maps to no
        // drop-down index, so the UI preselects nothing rather than the first scheme.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "midnight");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert_eq!(
            model.active(),
            Some("midnight"),
            "the header's scheme name is reported even when its file is absent"
        );
        assert_eq!(
            model.selected(),
            Some("midnight"),
            "the selected scheme is the active one"
        );
        assert_eq!(
            model.selected_index(),
            None,
            "an active scheme absent from colors/ maps to no drop-down index"
        );
    }

    #[test]
    fn fewer_than_two_schemes_degrades_to_read_only() {
        // Accept criterion (R3.2): with zero or one scheme there is nothing to switch
        // to, so the control is not switchable (the UI shows a read-only display).
        let dir = tempfile::tempdir().expect("temp dir");
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        // One scheme -> read-only.
        let one = dir.path().join("one");
        write_colors_dir(&one, &["nord"]);
        let model = PaletteModel::load(&one, &active_source, PathBuf::from("/gen"));
        assert_eq!(model.schemes().len(), 1);
        assert!(!model.is_switchable(), "one scheme is not switchable");

        // Zero valid schemes (only non-palette entries) -> read-only.
        let none = dir.path().join("none");
        write_colors_dir(&none, &[]);
        let model = PaletteModel::load(&none, &active_source, PathBuf::from("/gen"));
        assert!(
            model.schemes().is_empty(),
            "no valid palette files -> no schemes"
        );
        assert!(!model.is_switchable());

        // Two schemes -> switchable.
        let two = dir.path().join("two");
        write_colors_dir(&two, &["everforest", "nord"]);
        let model = PaletteModel::load(&two, &active_source, PathBuf::from("/gen"));
        assert!(model.is_switchable(), "two schemes are switchable");
    }

    #[test]
    fn staging_a_different_scheme_is_dirty_and_reselecting_active_is_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let mut model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));

        model.stage("everforest");
        assert!(model.is_dirty(), "switching to a different scheme is dirty");
        assert_eq!(model.selected(), Some("everforest"));

        // Re-selecting the active scheme clears the pending switch (not dirty).
        model.stage("nord");
        assert!(
            !model.is_dirty(),
            "re-selecting the active scheme is not dirty"
        );
        assert_eq!(model.selected(), Some("nord"));

        // An unknown scheme is ignored.
        model.stage("does-not-exist");
        assert!(!model.is_dirty());
    }

    #[test]
    fn reset_discards_and_commit_promotes_the_staged_scheme() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let mut model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));

        model.stage("everforest");
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending switch");
        assert_eq!(
            model.selected(),
            Some("nord"),
            "reset reverts to the active scheme"
        );

        model.stage("everforest");
        model.commit();
        assert!(!model.is_dirty(), "commit clears the dirty state");
        assert_eq!(
            model.active(),
            Some("everforest"),
            "commit promotes the staged scheme to active"
        );
    }

    #[test]
    fn apply_contribution_is_none_when_clean_and_carries_the_generator_path_when_dirty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");
        let generate_colors = PathBuf::from("/repo/scripts/generate-colors");

        let mut model = PaletteModel::load(&colors, &active_source, generate_colors.clone());
        assert!(
            model.apply_contribution().is_none(),
            "nothing staged -> no contribution"
        );

        model.stage("everforest");
        let switch = model
            .apply_contribution()
            .expect("a staged switch contributes a PaletteSwitch");
        assert_eq!(switch.scheme, "everforest");
        assert_eq!(switch.generate_colors, generate_colors);
    }

    #[test]
    fn applying_a_scheme_switch_runs_generate_colors_then_the_reload_chain() {
        // Accept criterion: feeding the model's contribution through the Apply pipeline
        // produces the exact command sequence — `generate-colors <scheme>` (the last
        // write step) then the palette reload chain — with NO colors/<scheme> file
        // write. This is the end-to-end proof the Theme page's palette switch drives
        // the pipeline correctly.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");
        let generate_colors = PathBuf::from("/repo/scripts/generate-colors");

        let mut model = PaletteModel::load(&colors, &active_source, generate_colors);
        model.stage("everforest");

        let plan = ApplyPlan {
            validations: Vec::new(),
            // The switch contributes NO file write: v1 never edits colors/<scheme>.
            writes: Vec::new(),
            palette: model.apply_contribution(),
            reload_params: ReloadParams::default(),
        };
        // A palette switch writes no tracked file, so an empty tracker is correct.
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![4242])]);
        let caps = Capabilities::for_tests(
            &[Binary::Hyprctl],
            &[Daemon::Eww, Daemon::Swaync, Daemon::Kitty],
            true,
        );

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        match outcome {
            ApplyOutcome::Applied { written, .. } => {
                assert!(
                    written.is_empty(),
                    "a palette switch writes no backing file"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // generate-colors runs FIRST (the last write step), then the apply-theme chain.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/repo/scripts/generate-colors").arg("everforest"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
            ]
        );
        // The palette reload chain finishes by delivering SIGUSR1 to the running
        // kitty (seeded above), which re-reads its colors — asserting it here proves
        // the full reload chain from the model level, not just the subprocess steps.
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![4242],
            }]
        );
    }

    #[test]
    fn parse_hex_rgb_reads_bare_hex_and_rejects_malformed_values() {
        assert_eq!(parse_hex_rgb("000000"), Some((0.0, 0.0, 0.0)));
        assert_eq!(parse_hex_rgb("ffffff"), Some((1.0, 1.0, 1.0)));
        // Wrong length and non-hex characters are rejected (skipped from the preview).
        assert_eq!(parse_hex_rgb("fff"), None);
        assert_eq!(parse_hex_rgb("gggggg"), None);
        assert_eq!(parse_hex_rgb("#ffffff"), None);
    }

    // =======================================================================
    // GTK / icon / cursor theme model (task 6.4)
    // =======================================================================

    /// A realistic `settings.ini` with all four theme keys the page edits.
    const SETTINGS_INI: &str = "\
[Settings]
gtk-theme-name=Everforest-Green-Dark
gtk-icon-theme-name=Everforest-Dark
gtk-cursor-theme-name=Nordic-cursors
gtk-cursor-theme-size=16
";

    /// The two cursor `env =` lines the app owns in `hyprland.conf`.
    const HYPRLAND_ENV: &str = "\
# Cursor env, kept identical to uwsm/env.
env = XCURSOR_THEME,Nordic-cursors
env = XCURSOR_SIZE,16
";

    /// A `uwsm/env` with the canonical cursor exports and a commented-out `GTK_THEME`.
    const UWSM_ENV: &str = "\
#export GTK_THEME=Nordic-bluish-accent
export XCURSOR_THEME=Nordic-cursors
export XCURSOR_SIZE=16
";

    /// Writes the four backing files into `dir` and returns their [`ThemesPaths`]. The
    /// `uwsm` text is supplied so a test can flip the `GTK_THEME` override on.
    fn write_backing_fixture(dir: &Path, uwsm: &str) -> ThemesPaths {
        let gtk3 = dir.join("gtk-3.0");
        let gtk4 = dir.join("gtk-4.0");
        let hypr = dir.join("hypr");
        let uwsm_dir = dir.join("uwsm");
        for sub in [&gtk3, &gtk4, &hypr, &uwsm_dir] {
            fs::create_dir_all(sub).expect("create a config subdir");
        }
        let gtk3_settings = gtk3.join("settings.ini");
        let gtk4_settings = gtk4.join("settings.ini");
        let hyprland_conf = hypr.join("hyprland.conf");
        let uwsm_env = uwsm_dir.join("env");
        fs::write(&gtk3_settings, SETTINGS_INI).expect("write gtk-3.0 settings.ini");
        // gtk-4.0 carries a different layout but the same keys, to prove identical
        // writes regardless of surrounding formatting.
        fs::write(
            &gtk4_settings,
            "# gtk4\n[Settings]\ngtk-theme-name = Everforest-Green-Dark\ngtk-cursor-theme-name = Nordic-cursors\ngtk-cursor-theme-size = 16\n",
        )
        .expect("write gtk-4.0 settings.ini");
        fs::write(&hyprland_conf, HYPRLAND_ENV).expect("write hyprland.conf");
        fs::write(&uwsm_env, uwsm).expect("write uwsm/env");
        ThemesPaths {
            gtk3_settings,
            gtk4_settings,
            hyprland_conf,
            uwsm_env,
        }
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
        fs::create_dir_all(themes.join("Adwaita").join("gtk-3.0")).unwrap();
        fs::create_dir_all(themes.join("NotATheme")).unwrap(); // no gtk-*/ -> skipped
        fs::create_dir_all(themes.join(".hidden").join("gtk-4.0")).unwrap(); // dotfile skipped

        let gtk = discover_gtk_themes(std::slice::from_ref(&themes));
        assert_eq!(
            gtk,
            vec!["Adwaita".to_string(), "Everforest-Green-Dark".to_string()],
            "GTK themes are the dirs with a gtk-3.0/ or gtk-4.0/, sorted; dotfiles/non-themes skipped"
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
        fs::create_dir_all(system.join("Adwaita").join("gtk-4.0")).unwrap();
        fs::create_dir_all(user.join("Adwaita").join("gtk-4.0")).unwrap();
        let gtk = discover_gtk_themes(&[user, system]);
        assert_eq!(
            gtk,
            vec!["Adwaita".to_string()],
            "the duplicate name collapses"
        );
    }

    #[test]
    fn a_cursor_change_writes_all_four_files_to_the_same_value_and_reloads() {
        // Accept criterion: a cursor apply writes BOTH settings.ini files AND both env
        // files to the SAME value, and drives the exact gsettings/hyprctl setcursor
        // command sequence through the apply pipeline (R3.4).
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
        // All four copies are written: both settings.ini, hyprland.conf, uwsm/env.
        assert_eq!(
            contribution.writes.len(),
            4,
            "a cursor change writes both settings.ini plus hyprland.conf and uwsm/env"
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

        // The exact reload sequence: hyprctl reload (from hyprland.conf) then the
        // cursor gsettings keys then setcursor, deduped across the four files.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").arg("reload"),
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
        let hypr = fs::read_to_string(&paths.hyprland_conf).unwrap();
        let uwsm = fs::read_to_string(&paths.uwsm_env).unwrap();
        assert!(gtk3.contains("gtk-cursor-theme-name=Bibata"));
        assert!(gtk3.contains("gtk-cursor-theme-size=24"));
        assert!(gtk4.contains("gtk-cursor-theme-name = Bibata"));
        assert!(gtk4.contains("gtk-cursor-theme-size = 24"));
        assert!(hypr.contains("env = XCURSOR_THEME,Bibata"));
        assert!(hypr.contains("env = XCURSOR_SIZE,24"));
        assert!(uwsm.contains("export XCURSOR_THEME=Bibata"));
        assert!(uwsm.contains("export XCURSOR_SIZE=24"));
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
        let paths = ThemesPaths {
            gtk3_settings: tmp.path().join("gtk-3.0/settings.ini"),
            gtk4_settings: tmp.path().join("gtk-4.0/settings.ini"),
            hyprland_conf: tmp.path().join("hypr/hyprland.conf"),
            uwsm_env: tmp.path().join("uwsm/env"),
        };
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
        let paths = ThemesPaths {
            gtk3_settings: config.join("gtk-3.0/settings.ini"),
            gtk4_settings: config.join("gtk-4.0/settings.ini"),
            hyprland_conf: config.join("hypr/hyprland.conf"),
            uwsm_env: config.join("uwsm/env"),
        };
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
        fs::write(&paths.hyprland_conf, HYPRLAND_ENV).unwrap();
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
        let paths = ThemesPaths {
            gtk3_settings: config.join("gtk-3.0/settings.ini"),
            gtk4_settings: config.join("gtk-4.0/settings.ini"),
            hyprland_conf: config.join("hypr/hyprland.conf"),
            uwsm_env: config.join("uwsm/env"),
        };
        let ini = "[Settings]\ngtk-cursor-theme-name=Nordic-cursors\ngtk-cursor-theme-size=big\n";
        fs::write(&paths.gtk3_settings, ini).unwrap();
        fs::write(&paths.gtk4_settings, ini).unwrap();
        fs::write(&paths.hyprland_conf, HYPRLAND_ENV).unwrap();
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
        // N1: with only one XCURSOR_* env line present, the present field is still
        // written to hyprland.conf rather than the whole file being abandoned — so the
        // copies do not drift out of step.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_backing_fixture(&config, UWSM_ENV);
        // Leave only the XCURSOR_THEME env line; drop XCURSOR_SIZE.
        fs::write(&paths.hyprland_conf, "env = XCURSOR_THEME,Nordic-cursors\n").unwrap();
        let roots = ThemeRoots {
            gtk_theme_dirs: Vec::new(),
            icon_dirs: vec![write_icon_root(tmp.path())],
        };

        let mut model = ThemesModel::load(&roots, paths.clone(), false, None);
        model.stage_cursor_theme("Bibata");
        model.stage_cursor_size("24");
        let contribution = model
            .apply_contribution()
            .expect("a cursor change contributes");

        let hypr_write = contribution
            .writes
            .iter()
            .find(|write| write.path == paths.hyprland_conf)
            .expect("hyprland.conf is still written despite the missing XCURSOR_SIZE line");
        let text = String::from_utf8(hypr_write.contents.clone()).unwrap();
        assert!(
            text.contains("env = XCURSOR_THEME,Bibata"),
            "the present theme field is written"
        );
        assert!(
            !text.contains("XCURSOR_SIZE"),
            "the absent size line is not fabricated (the repeatable writer never appends)"
        );
        assert_eq!(
            hypr_write.changed_keys,
            vec!["cursor theme (hyprland.conf env)".to_string()],
            "only the present field is recorded for the hyprland.conf write"
        );
    }

    // --- Wallpaper / lock background (task 6.5) ------------------------------

    /// A `hyprpaper.conf` fixture: a `wallpaper { }` block with a monitor line, a
    /// comment, the wallpaper `path`, and `fit_mode`, plus a top-level `splash` key —
    /// the shape from the hyprlang parser fixtures (analysis §6.2).
    const HYPRPAPER_CONF: &str = "\
wallpaper {
    monitor =
    # Keep in sync with hyprlock.conf's background.path (same image).
    path = ~/Pictures/wallpaper/18.jpg
    fit_mode = cover
}

splash = false
";

    /// A `hyprlock.conf` fixture: a `source =`, a `background { }` block with the
    /// lock-screen `path` and an inline comment, and an unrelated `label { }` block.
    const HYPRLOCK_CONF: &str = "\
source = ~/.config/hypr/colors.conf

background {
    monitor =
    path = ~/Pictures/wallpaper/18.jpg
    blur_passes = 2 # 0 disables blurring
}

label {
    text = Hi
}
";

    /// Writes the two backing files into `dir/hypr` and returns their
    /// [`WallpaperPaths`].
    fn write_wallpaper_fixture(dir: &Path) -> WallpaperPaths {
        let hypr = dir.join("hypr");
        fs::create_dir_all(&hypr).expect("create hypr config subdir");
        let hyprpaper_conf = hypr.join("hyprpaper.conf");
        let hyprlock_conf = hypr.join("hyprlock.conf");
        fs::write(&hyprpaper_conf, HYPRPAPER_CONF).expect("write hyprpaper.conf");
        fs::write(&hyprlock_conf, HYPRLOCK_CONF).expect("write hyprlock.conf");
        WallpaperPaths {
            hyprpaper_conf,
            hyprlock_conf,
        }
    }

    /// Writes a real (present, readable) file with an image extension at `dir/name` and
    /// returns its path string, so `validate_image_path` accepts it (it checks the
    /// extension and readability, not the bytes — task 4.1).
    fn write_image(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, b"img").expect("write image file");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_wallpaper_and_fit_change_writes_both_files_and_reloads_hyprpaper() {
        // Accept criterion: a staged wallpaper path + fit-mode edit produces the exact
        // surgical hyprpaper.conf/hyprlock.conf FileWrite diffs AND the hyprctl hyprpaper
        // preload/wallpaper command sequence. With no override, the same path is written
        // to both files (the single-wallpaper UX, analysis §6.2).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "wall.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(
            !model.override_on(),
            "the unified same-image default means no override"
        );
        model
            .stage_wallpaper(&wall)
            .expect("a real image validates");
        model.stage_fit("contain");
        assert!(model.is_dirty());

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "both hyprpaper.conf and hyprlock.conf are written (same path to both)"
        );
        // The chosen paths are re-validated at apply time (R8.3).
        assert_eq!(
            contribution.validations,
            vec![
                (SettingId::WallpaperPath, Value::String(wall.clone())),
                (SettingId::LockBackgroundPath, Value::String(wall.clone())),
            ]
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the wallpaper apply must succeed, got {outcome:?}"
        );

        // hyprpaper.conf: ONLY the path and fit_mode value spans changed — building the
        // expected text by replacing exactly those two spans proves every other byte
        // (the comment, the `monitor =` line, `splash`) is untouched.
        let expected_hyprpaper = HYPRPAPER_CONF
            .replace("~/Pictures/wallpaper/18.jpg", &wall)
            .replace("fit_mode = cover", "fit_mode = contain");
        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            expected_hyprpaper
        );
        // hyprlock.conf: ONLY background.path changed, to the SAME wallpaper path.
        let expected_hyprlock = HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &wall);
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            expected_hyprlock
        );

        // The reload: preload the image, then set it on all outputs (empty monitor
        // field before the comma) with the staged fit as the third comma-field, so the
        // fit is applied live (task 6.5). No hyprlock reload (see the dedicated test).
        let set_arg = format!(",{wall},contain");
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args(["hyprpaper", "preload", wall.as_str()]),
                Command::new("hyprctl").args(["hyprpaper", "wallpaper", set_arg.as_str()]),
            ]
        );
    }

    #[test]
    fn a_fit_only_change_reloads_hyprpaper_with_the_new_fit() {
        // MAJOR-fix (task 6.5): a fit-only change still reloads hyprpaper — it re-sets
        // the (unchanged) wallpaper image with the NEW fit as the third comma-field, so
        // the fit takes effect live rather than only on the next hyprpaper restart.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        let mut model = WallpaperModel::load(paths, true);
        // Change ONLY the fit; the wallpaper path stays the on-disk value.
        model.stage_fit("tile");
        assert!(model.is_dirty());

        let contribution = model
            .apply_contribution()
            .expect("a fit change contributes");
        // Only hyprpaper.conf is written (the lock path is unchanged), and the reload
        // carries the current wallpaper path AND the new fit. The unchanged wallpaper
        // path is not re-validated (a fit change should not fail on it, R8.3).
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprpaper.conf is written"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);
        assert!(
            contribution.validations.is_empty(),
            "a fit-only change re-validates no path (the wallpaper is unchanged)"
        );
        assert_eq!(
            contribution.reload_params.wallpaper.as_deref(),
            Some("~/Pictures/wallpaper/18.jpg")
        );
        assert_eq!(contribution.reload_params.fit.as_deref(), Some("tile"));

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let runner = MockCommandRunner::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(
            &plan,
            &FreshnessTracker::new(),
            &caps,
            &runner,
            &MockProcessSignaller::new(),
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the fit-only apply must succeed, got {outcome:?}"
        );
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args([
                    "hyprpaper",
                    "preload",
                    "~/Pictures/wallpaper/18.jpg"
                ]),
                Command::new("hyprctl").args([
                    "hyprpaper",
                    "wallpaper",
                    ",~/Pictures/wallpaper/18.jpg,tile"
                ]),
            ]
        );
    }

    #[test]
    fn a_lock_override_only_change_writes_hyprlock_with_no_reload() {
        // Accept criterion: a change that touches ONLY hyprlock.conf (the lock-screen
        // override) issues NO reload command — hyprlock reads its config at the next
        // lock (intentional, architecture §6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let lock_image = write_image(tmp.path(), "lock.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        // Turn on the override and pick a different lock image; leave the wallpaper and
        // fit unchanged, so only hyprlock.conf is written.
        model.set_override(true);
        model
            .stage_lock(&lock_image)
            .expect("a real image validates");
        assert!(model.is_dirty());

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprlock.conf is written for a lock-override-only change"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprlockConf);
        assert!(
            contribution.reload_params.wallpaper.is_none(),
            "a hyprlock-only change carries no wallpaper reload parameter"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        // hyprpaper is fully live, yet a hyprlock-only change still plans no reload.
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the lock apply must succeed, got {outcome:?}"
        );
        assert!(
            runner.recorded().is_empty(),
            "a hyprlock-only change must issue no reload command"
        );

        // hyprpaper.conf is untouched; only hyprlock's background.path changed.
        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF
        );
        let expected_hyprlock = HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &lock_image);
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            expected_hyprlock
        );
    }

    #[test]
    fn staging_rejects_a_non_existent_or_non_image_path() {
        // R8.3: a chosen path is validated (exists + readable + image extension) before
        // staging; an invalid path is rejected and nothing is staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let mut model = WallpaperModel::load(paths, true);

        // A path that does not exist.
        let missing = tmp.path().join("no-such.png");
        assert!(
            model.stage_wallpaper(&missing.to_string_lossy()).is_err(),
            "a non-existent path is rejected"
        );
        // A real file with a non-image extension.
        let text = tmp.path().join("notes.txt");
        fs::write(&text, b"hi").unwrap();
        assert!(
            model.stage_wallpaper(&text.to_string_lossy()).is_err(),
            "a non-image file is rejected"
        );
        // The same guard applies to the lock override path.
        assert!(
            model.stage_lock(&missing.to_string_lossy()).is_err(),
            "the lock override path is validated too"
        );
        assert!(!model.is_dirty(), "a rejected path is never staged");
    }

    #[test]
    fn controls_hide_when_hyprlock_is_absent_or_config_is_unreadable() {
        // R4.2/R4.4: the lock-override control is hidden when hyprlock is absent, and the
        // wallpaper rows are hidden when hyprpaper.conf is unreadable.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        // hyprlock absent: the wallpaper is editable, but the lock override is not.
        let mut no_lock = WallpaperModel::load(paths.clone(), false);
        assert!(no_lock.wallpaper_editable());
        assert!(
            !no_lock.lock_editable(),
            "no hyprlock -> no lock-override control (R4.2)"
        );
        // A wallpaper change then writes ONLY hyprpaper.conf — hyprlock.conf is left
        // alone when the lock is not editable.
        let wall = write_image(tmp.path(), "wall.png");
        no_lock.stage_wallpaper(&wall).unwrap();
        let contribution = no_lock
            .apply_contribution()
            .expect("a wallpaper change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprpaper.conf is written"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);

        // hyprpaper.conf unreadable: the wallpaper rows hide (R4.4).
        let unreadable = WallpaperModel::load(
            WallpaperPaths {
                hyprpaper_conf: config.join("hypr").join("does-not-exist.conf"),
                hyprlock_conf: paths.hyprlock_conf.clone(),
            },
            true,
        );
        assert!(
            !unreadable.wallpaper_editable(),
            "an unreadable hyprpaper.conf hides the wallpaper rows (R4.4)"
        );
    }

    #[test]
    fn reset_restores_the_override_toggle_and_commit_is_not_a_self_conflict() {
        // Reset restores the toggle baseline and clears staged paths (R5.1); and after a
        // committed apply the app's own write is not seen as an external conflict (R5.6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "wall.png");
        let lock_image = write_image(tmp.path(), "lock.png");

        let mut model = WallpaperModel::load(paths, true);
        // Toggle the override on and stage a distinct lock image, then reset.
        model.set_override(true);
        model.stage_lock(&lock_image).unwrap();
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending override");
        assert!(
            !model.override_on(),
            "reset restores the toggle baseline (off)"
        );

        // Now stage a wallpaper change, apply it, and commit.
        model.stage_wallpaper(&wall).unwrap();
        let contribution = model.apply_contribution().expect("a change contributes");
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));
        assert!(
            model.check_conflict(),
            "before commit the write differs from the load-time baseline"
        );
        model.commit();
        assert!(!model.is_dirty(), "commit clears the dirty state");
        assert!(
            !model.check_conflict(),
            "after commit the app's own write is not a self-conflict (R5.6)"
        );
    }

    /// Writes a fixture whose `hyprpaper.conf` and `hyprlock.conf` start with two
    /// **different** on-disk image paths (both real files, so validation accepts them),
    /// returning the paths plus the wallpaper (`A`) and lock (`B`) path strings. This
    /// exercises the override-on branches that the unified same-path fixture cannot.
    fn write_distinct_wallpaper_fixture(
        root: &Path,
        config: &Path,
    ) -> (WallpaperPaths, String, String) {
        let wallpaper_a = write_image(root, "wall_a.png");
        let lock_b = write_image(root, "lock_b.png");
        let hypr = config.join("hypr");
        fs::create_dir_all(&hypr).expect("create hypr config subdir");
        let hyprpaper_conf = hypr.join("hyprpaper.conf");
        let hyprlock_conf = hypr.join("hyprlock.conf");
        fs::write(
            &hyprpaper_conf,
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &wallpaper_a),
        )
        .expect("write hyprpaper.conf");
        fs::write(
            &hyprlock_conf,
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &lock_b),
        )
        .expect("write hyprlock.conf");
        (
            WallpaperPaths {
                hyprpaper_conf,
                hyprlock_conf,
            },
            wallpaper_a,
            lock_b,
        )
    }

    #[test]
    fn distinct_on_disk_paths_start_with_the_override_on() {
        // When the two on-disk paths differ (not the unified default), the lock-screen
        // override toggle starts on, preselecting each file's own path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, wallpaper_a, lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);

        let model = WallpaperModel::load(paths, true);
        assert!(
            model.override_on(),
            "distinct on-disk paths -> the override starts on"
        );
        assert_eq!(model.wallpaper_path(), Some(wallpaper_a.as_str()));
        assert_eq!(model.lock_path(), Some(lock_b.as_str()));
        assert!(!model.is_dirty(), "no edit is staged at load");
    }

    #[test]
    fn a_distinct_lock_override_writes_both_files_to_their_two_paths() {
        // With the override on, changing both the wallpaper and the lock image writes
        // each file to its own (different) path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, _wallpaper_a, _lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);
        let wallpaper_c = write_image(tmp.path(), "wall_c.png");
        let lock_d = write_image(tmp.path(), "lock_d.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(model.override_on());
        model
            .stage_wallpaper(&wallpaper_c)
            .expect("a real image validates");
        model.stage_lock(&lock_d).expect("a real image validates");

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "both files are written, to their two different paths"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));

        assert!(
            fs::read_to_string(&paths.hyprpaper_conf)
                .unwrap()
                .contains(&format!("path = {wallpaper_c}")),
            "hyprpaper.conf points at the new wallpaper"
        );
        assert!(
            fs::read_to_string(&paths.hyprlock_conf)
                .unwrap()
                .contains(&format!("path = {lock_d}")),
            "hyprlock.conf points at the distinct override image"
        );
    }

    #[test]
    fn turning_the_override_off_resyncs_the_lock_to_the_wallpaper_with_no_reload() {
        // Turning the override off resyncs the lock-screen background to the wallpaper:
        // a single hyprlock.conf write equal to the wallpaper path, with no reload
        // (hyprlock reads its config at the next lock).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, wallpaper_a, _lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);

        let mut model = WallpaperModel::load(paths, true);
        assert!(model.override_on());
        model.set_override(false);
        assert!(
            model.is_dirty(),
            "resyncing the lock back to the wallpaper is a pending change"
        );

        let contribution = model.apply_contribution().expect("the resync contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprlock.conf is written on a resync"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprlockConf);
        assert!(
            contribution.reload_params.wallpaper.is_none(),
            "a hyprlock-only resync carries no wallpaper reload parameter -> no reload"
        );
        let text = String::from_utf8(contribution.writes[0].contents.clone()).unwrap();
        assert!(
            text.contains(&format!("path = {wallpaper_a}")),
            "the lock background is resynced to the wallpaper path"
        );
    }

    #[test]
    fn an_unusual_on_disk_fit_mode_stays_selectable() {
        // NIT (task 6.5): a non-curated on-disk fit_mode value is kept as the preselected
        // option (Selection::new prepends it), so an unusual value is never dropped.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("fit_mode = cover", "fit_mode = stretch"),
        )
        .expect("write an unusual fit_mode");

        let model = WallpaperModel::load(paths, true);
        let selected = model
            .selected_fit_index()
            .and_then(|index| model.fit_options().get(index))
            .map(String::as_str);
        assert_eq!(
            selected,
            Some("stretch"),
            "an unusual on-disk fit_mode stays the preselected option"
        );
        assert!(
            model.fit_options().contains(&"stretch".to_string()),
            "the unusual value is present among the options"
        );
    }
}
