//! The startup load: installed-app detection plus parsing every backing config file
//! into the store's originals and the bespoke page models, run on a worker thread
//! concurrently with window construction (tasks 5.4, 9.21; architecture §2, §8; R4.3,
//! R4.4, R7.3, R6.2).
//!
//! # What this module is
//!
//! Cold start must stay under the R8.1 budget, so the app builds the window shell
//! immediately on the main thread while the slower work — installed-app detection
//! (task 4.3) and reading/parsing the backing config files (§3 parsers) — happens
//! off-thread (architecture §8). This module is that off-thread work: [`load`]
//! gathers a [`StartupLoad`] (the detected [`Capabilities`], one [`LoadedFile`] per
//! backing config it could read, and the bespoke page models), which the window then
//! applies on the main thread to populate the store and build the pages. The thread
//! hop itself — running [`load`] on a worker and handing its result back to the GTK
//! main loop — is the UI layer's job (`ui::startup`), not this module's.
//!
//! # Why it lives in `core/`
//!
//! Everything here is plain data-in/data-out over the filesystem, the §3 parsers and
//! the [`CommandRunner`](crate::system::command::CommandRunner) probes; it touches no
//! GTK type. Like the rest of `core/` it is therefore unit-tested headlessly (R6.2, see
//! the tests in each submodule) and guarded against toolkit imports by
//! `tests/module_boundaries.rs`, which for this module also forbids `glib` so the
//! worker/main-thread handoff cannot creep in. Living here, rather than beside the
//! window under `ui/`, puts "what parses the config files into settings" next to the
//! models it builds, and makes its items ordinary public API that the integration
//! suites call directly (R6.1).
//!
//! The code is split by what it loads, and everything is re-exported here so callers
//! address it as `core::load::<item>` whichever file it lives in:
//!
//! - submodule `files` — the store-backed config files: [`LoadedFile`],
//!   [`BackingPaths`], [`load_files`], the per-file loaders ([`load_input_lua`],
//!   [`load_hypridle_conf`], [`load_notifd_config`]) and the refresh merge
//!   ([`merge_refreshed_files`]);
//! - submodule `models` — the bespoke page models and write helpers built from the
//!   detected capabilities ([`load_display`], [`load_input`], [`load_palette`],
//!   [`load_themes`], [`load_wallpaper`], [`load_power`], [`load_notifd_config_from`])
//!   and the theme paths they read ([`theme_roots`], [`themes_paths`]);
//! - this file — [`StartupLoad`], the [`load`] orchestration over both, the refresh
//!   adoption rule for bespoke models ([`adopt_refreshed_model`]), and the XDG
//!   config-directory resolution every loader shares.
//!
//! # Everything degrades to "absent"/"skipped" (R4.3, R4.4)
//!
//! A missing binary or daemon degrades inside [`Capabilities::detect`]; here, a
//! backing config that is missing, unreadable, or unparseable simply produces no
//! [`LoadedFile`] (logged at `warn`, never its contents — R7.3) and is skipped. One
//! bad file never aborts the others, and nothing here can fail startup: the window
//! still comes up with whatever loaded. A file that is present but simply lacks a
//! given key contributes only the settings it does hold.
//!
//! # Refresh re-runs the same pass (R4.3)
//!
//! The manual Refresh button re-runs [`load`] on the worker too — detection *and* the
//! backing files *and* the bespoke page models must all be loaded again together, or the
//! refreshed capabilities and the rendered content diverge (e.g. installing hyprpaper
//! and clicking Refresh must make the wallpaper section appear, not wait for an app
//! restart). What differs from startup is how the result is merged into live state:
//! [`merge_refreshed_files`] preserves staged store edits and reports external file
//! edits (R5.6), and [`adopt_refreshed_model`] keeps a bespoke model that holds staged
//! edits rather than clobbering the user's in-progress work.
//!
//! # Scope: the store-backed files
//!
//! The `SettingId` ↔ file ↔ parser ↔ key mapping is complete for the §6 category
//! pages; the store-backed pages exercise three files, so those are what this loads:
//!
//! - `~/.config/hypr/input.lua` (the app-owned `hl.config({ input = { … } })` table,
//!   analysis §7.1) via the Lua-literal editor → the Input page's keyboard layouts,
//!   keyboard options, mouse sensitivity, and touchpad toggles (tasks 6.6, 11.4);
//! - `~/.config/hypr/hypridle.conf` via the hyprlang parser → the Power & Idle page's
//!   dim/lock/DPMS timeouts (by positional `listener` matching, §3.2) and lock command
//!   (task 6.8);
//! - `~/.config/notifd/config` via the shared `key=value` line model → the
//!   Notifications page's seven notifd settings, read with the daemon's own rules
//!   (task 11.15, [`crate::core::notifd_config`]).
//!
//! Each store-backed page contributes its file loader to [`load_files`] in the same
//! shape (read → parse → map keys to `(SettingId, Value)` → build a re-reader), and a
//! new page would add one more. Where a
//! setting maps to more than one on-disk key, the composition is done here for
//! reading; the reverse split for *writing* is the owning page's job, since startup
//! produces no writes.
//!
//! The Notifications page has two halves. Its runtime controls have no startup model
//! (task 11.7): the window builds that model when it builds the page and the first
//! daemon query happens on page entry. Its staged notifd settings (task 11.15) get a
//! write helper here, like Input and Power & Idle, built only when notifd ≥ 1.1 runs and
//! its file is readable ([`load_notifd_config_from`]).

use std::path::{Path, PathBuf};

use crate::core::detect::{Capabilities, DetectionInputs};
use crate::core::display::DisplayModel;
use crate::core::input::InputModel;
use crate::core::notifd_config::NotifdConfigModel;
use crate::core::power::PowerModel;
use crate::core::theme::{PaletteModel, ThemesModel, WallpaperModel};
use crate::system::command::SystemCommandRunner;

mod files;
mod models;

pub use files::{
    BackingPaths, LoadedFile, load_files, load_hypridle_conf, load_input_lua, load_notifd_config,
    merge_refreshed_files,
};
pub use models::{
    load_display, load_input, load_input_from, load_notifd_config_from, load_palette, load_power,
    load_themes, load_wallpaper, theme_roots, themes_paths,
};

/// The product of one startup worker pass: detected capabilities plus the backing
/// files that could be read and parsed (task 5.4).
///
/// It is [`Send`] (its fields — [`Capabilities`], `PathBuf`/`Vec<u8>` data, and the
/// [`Send`] [`FileReader`](crate::core::store::FileReader) boxes — all are), so it can
/// be produced on the worker thread and handed back to the main thread for the window
/// to apply.
pub struct StartupLoad {
    /// What detection found (architecture §4). Drives which categories/rows show.
    pub capabilities: Capabilities,
    /// The backing files that loaded, in a fixed order; each is applied to the store.
    /// A file that was missing/unreadable/unparseable is simply absent here.
    pub files: Vec<LoadedFile>,
    /// The Display page's live model (task 6.1), built by probing
    /// `hyprctl monitors -j` and reading `monitors.conf`, or `None` when there is no
    /// live compositor to query (the Display page then shows a placeholder, R4.2).
    pub display: Option<DisplayModel>,
    /// The Input page's helper (tasks 6.6, 11.4): the `input.lua` path and XKB layout
    /// candidates, or `None` when the page cannot be offered — `hyprctl` is absent,
    /// Hyprland does not run its Lua config provider, or `input.lua` is missing or not
    /// fully understood by the editor (the Input category is then hidden entirely,
    /// R4.2). It supplies the layout add-list and renders the store's dirty Input
    /// settings into the `input.lua` write on Apply.
    pub input: Option<InputModel>,
    /// The Notifications page's helper for its staged notifd settings (task 11.15): the
    /// `~/.config/notifd/config` path, or `None` when those rows cannot be offered —
    /// notifd is not running at version 1.1 or later, or the file is missing or not
    /// understood. The page then shows only its runtime controls. It renders the store's
    /// dirty notifd settings into the file's write on Apply, which the pipeline follows
    /// with notifd's `Reload()`.
    pub notifications: Option<NotifdConfigModel>,
    /// The Power & Idle page's helper (task 6.8): the `hypridle.conf` path, or `None` when
    /// hypridle is absent (the Power & Idle category is then hidden entirely, R4.2). It
    /// renders the store's dirty dim/lock/DPMS timeouts and lock command into the
    /// `hypridle.conf` write on Apply, which the pipeline follows with a hypridle restart.
    pub power: Option<PowerModel>,
    /// The Theme page's palette-scheme model (task 6.3), built by scanning the
    /// detected `colors/` directory and detecting the active scheme, or `None` when
    /// there is no dotfiles palette source (the Theme palette section is then hidden,
    /// R4.2/R8.5).
    pub palette: Option<PaletteModel>,
    /// The Theme page's GTK/icon/cursor theme model (task 6.4), built by scanning for
    /// installed themes and reading the backing config, or `None` when `gsettings` is
    /// absent (the appearance section is then hidden, R4.2).
    pub themes: Option<ThemesModel>,
    /// The Theme page's wallpaper / lock-background model (task 6.5), built by reading
    /// `hyprpaper.conf`/`hyprlock.conf`, or `None` when hyprpaper is absent (the
    /// wallpaper section is then hidden, R4.2).
    pub wallpaper: Option<WallpaperModel>,
}

/// The live path of notifd's settings file under the XDG config base directory
/// `config_home` (R8.5) — one definition for the store loader and the Notifications page
/// helper, which must agree on the file. notifd itself reads
/// `$XDG_CONFIG_HOME/notifd/config` (falling back to `~/.config`), the same rule.
fn notifd_config_path(config_home: &Path) -> PathBuf {
    config_home.join("notifd").join("config")
}

/// The live path of `input.lua` under the XDG config base directory `config_home`
/// (R8.5) — one definition for the store loader and the Input page helper, which must
/// agree on the file.
fn input_lua_path(config_home: &Path) -> PathBuf {
    config_home.join("hypr").join("input.lua")
}

/// The XDG config base directory: `$XDG_CONFIG_HOME`, else `$HOME/.config`, else a
/// bare `.config` relative path (which simply misses on read).
fn config_home() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        if !config_home.is_empty() {
            return PathBuf::from(config_home);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".config");
    }
    PathBuf::from(".config")
}

/// Runs one full startup pass — detection plus parsing every backing file — for the
/// real system (the worker-thread entry point, architecture §8).
///
/// Called off the main thread by the UI layer's worker glue (`ui::startup`). It never
/// panics and never fails: detection degrades every absent probe to "absent" (task
/// 4.3), and each file that cannot be read or parsed is skipped with a `warn` (R4.4).
/// No backing config paths are passed to [`DetectionInputs::from_system`] for a
/// redundant readability check —
/// this module reads and parses them directly, which subsumes readability; the §6
/// pages register their own per-row config paths with detection when they need
/// [`Capabilities::config_readable`].
pub fn load() -> StartupLoad {
    let capabilities = Capabilities::detect(&DetectionInputs::from_system(
        &SystemCommandRunner::new(),
        Vec::new(),
    ));
    let files = load_files(&BackingPaths::from_system());
    let display = load_display(&capabilities);
    let input = load_input(&capabilities);
    // Reads notifd's file for the visibility gate; `load_files` above read it for the
    // store through the same `read_notifd_config` (see the note there), so both agree.
    let notifications = load_notifd_config_from(&capabilities, notifd_config_path(&config_home()));
    let power = load_power(&capabilities);
    let palette = load_palette(&capabilities);
    let themes = load_themes(&capabilities);
    let wallpaper = load_wallpaper(&capabilities);
    StartupLoad {
        capabilities,
        files,
        display,
        input,
        notifications,
        power,
        palette,
        themes,
        wallpaper,
    }
}

/// Decides which instance of a bespoke page model survives a manual refresh
/// (task 9.3; R4.3).
///
/// `fresh` is what a new [`load`] pass built against the refreshed
/// [`Capabilities`]; `current` is the model the window holds (a staging source with
/// its own dirty state — Display, palette, GTK/icon/cursor themes, or wallpaper).
/// Three rules, in order:
///
/// - **`fresh` is `None`** → the model's capability is gone (its app was uninstalled,
///   or its source removed), so the model is dropped and its section hides (R4.2).
///   Any staged edits it held could no longer be applied or reloaded anyway.
/// - **`current` has staged edits** → keep it wholesale. This mirrors the store's
///   refresh contract (pending edits are never silently discarded, R5.6): replacing
///   the model would drop the user's in-progress work the moment they click Refresh.
///   Its originals stay stale until the next clean Refresh, which is safe for each
///   model on its own terms: the file-writing models (Display, GTK/icon/cursor
///   themes, wallpaper) re-check their backing files at Apply time (re-read + hash),
///   so a stale model still cannot clobber an external edit; and the palette model
///   writes no file itself — its apply runs `scripts/generate-colors`, which
///   regenerates from the *current* on-disk palette sources — so staleness there at
///   worst means an outdated scheme list in the drop-down.
/// - **Otherwise** → adopt `fresh`, so changes to the model's sources since the last
///   load (newly installed themes, an externally switched palette, a hand-edited
///   `monitors.conf`) are reflected in the rebuilt page.
///
/// Generic over the model with an explicit `is_dirty` accessor because the four
/// staging models share no trait; a plain `fn` pointer keeps the call sites to a
/// method reference like `DisplayModel::is_dirty`.
pub fn adopt_refreshed_model<M>(
    current: &mut Option<M>,
    fresh: Option<M>,
    is_dirty: fn(&M) -> bool,
) {
    match fresh {
        None => *current = None,
        Some(fresh) => {
            if current.as_ref().is_some_and(is_dirty) {
                tracing::debug!(
                    "keeping a page model with staged edits across a refresh; its sources \
                     re-sync on the next clean refresh (R4.3/R5.6)"
                );
            } else {
                *current = Some(fresh);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::theme::{WallpaperModel, WallpaperPaths};

    #[test]
    fn a_dirty_model_survives_a_refresh_and_a_clean_one_is_replaced() {
        // The three adopt_refreshed_model rules, on a stand-in staging model (the four
        // real staging models share no trait; the rule is model-agnostic).
        struct Staging {
            label: &'static str,
            dirty: bool,
        }
        fn is_dirty(model: &Staging) -> bool {
            model.dirty
        }

        // Clean -> the fresh load wins, so external source changes are reflected.
        let mut current = Some(Staging {
            label: "stale",
            dirty: false,
        });
        adopt_refreshed_model(
            &mut current,
            Some(Staging {
                label: "fresh",
                dirty: false,
            }),
            is_dirty,
        );
        assert_eq!(current.as_ref().map(|m| m.label), Some("fresh"));

        // Dirty -> the user's staged edits are kept; the fresh load is discarded (R5.6).
        let mut current = Some(Staging {
            label: "dirty",
            dirty: true,
        });
        adopt_refreshed_model(
            &mut current,
            Some(Staging {
                label: "fresh",
                dirty: false,
            }),
            is_dirty,
        );
        assert_eq!(current.as_ref().map(|m| m.label), Some("dirty"));

        // Capability gone -> dropped even while dirty, so the section hides (R4.2).
        adopt_refreshed_model(&mut current, None, is_dirty);
        assert!(current.is_none());
    }

    #[test]
    fn a_real_wallpaper_model_keeps_its_staged_edit_across_a_refresh() {
        // The dirty-keep rule on a real staging model (not a stand-in): a staged
        // fit-mode edit on a fixture-backed WallpaperModel must survive adoption of
        // the clean model a refresh pass loads from the same sources — this is the
        // exact wiring `apply_refresh_load` runs for the four staging models.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprpaper_conf = dir.path().join("hyprpaper.conf");
        std::fs::write(
            &hyprpaper_conf,
            b"wallpaper {\n    path = /tmp/wallpaper.jpg\n    fit_mode = cover\n}\n",
        )
        .expect("write hyprpaper.conf fixture");
        // Both loads read the same fixture paths, exactly like startup and a later
        // Refresh reading the same live config (hyprlock.conf is simply absent).
        let paths = || WallpaperPaths {
            hyprpaper_conf: hyprpaper_conf.clone(),
            hyprlock_conf: dir.path().join("hyprlock.conf"),
            home: None,
        };

        let mut model = WallpaperModel::load(paths(), false);
        model.stage_fit("contain");
        assert!(
            model.is_dirty(),
            "the staged fit mode marks the model dirty"
        );
        let mut current = Some(model);

        // Refresh: adoption must keep the dirty model so the staged edit is not
        // silently discarded (R5.6), discarding the clean fresh load.
        adopt_refreshed_model(
            &mut current,
            Some(WallpaperModel::load(paths(), false)),
            WallpaperModel::is_dirty,
        );
        let kept = current.expect("the dirty model survives the refresh");
        assert!(kept.is_dirty(), "the staged edit is still pending");
        let fit_index = kept.selected_fit_index().expect("a fit mode is selected");
        assert_eq!(
            kept.fit_options()[fit_index],
            "contain",
            "the kept model still carries the staged fit mode"
        );
    }
}
