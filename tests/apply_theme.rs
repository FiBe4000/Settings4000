//! End-to-end staged-edit → Apply suite for the **Theme** category (task 7.2;
//! R5.3–R5.6, R6.1).
//!
//! The Theme page is three staging sources folded into one Apply (tasks
//! 6.3–6.5), and this suite drives each against the installed fixture tree
//! (task 7.1):
//!
//! - the **palette** switch, whose source is found by *real* detection
//!   through the deployed `colors.conf` symlink (R8.5) and which runs
//!   `generate-colors <scheme>` as the LAST write step followed by the reload
//!   chain — including the rollback proof that a failed palette apply never
//!   leaves the generated files regenerated (the palette gotcha);
//! - the **GTK/icon/cursor** themes, whose duplicated on-disk copies (both
//!   `settings.ini` files; for the cursor also `uwsm/env`, the two `hl.env`
//!   lines of `hyprland.lua` and the XCursor fallback
//!   `~/.icons/default/index.theme`) must all receive the identical value
//!   (R3.4, analysis §7.7, §8.4);
//! - the **wallpaper / lock background** dual write (`hyprpaper.conf` +
//!   `hyprlock.conf`, same image). The fixture carries the real dotfiles'
//!   home-relative value (`~/Pictures/wallpaper/18.jpg`, analysis §7.4), so the
//!   suite proves the form-preserving write (task 11.10): an image under the
//!   home directory is written `~/…`, one outside it absolute, a file that uses
//!   absolute paths keeps them, and hyprpaper's IPC always gets the absolute
//!   path.
//!
//! Task 11.14 adds the two follow-up reloads: an icon-theme change ends with
//! notifd's `Reload()` D-Bus call (none on a notifd older than 1.1, which gets a
//! page note instead), and a wallpaper change is one `hyprctl hyprpaper
//! wallpaper` request (no `preload`) followed by the fixture's
//! `~/.config/eww/scripts/bar-text-tone --apply` while eww runs.
//!
//! Task 11.16 adds the third leg of a GTK-theme change: the GTK 4 theme links
//! `~/.config/gtk-4.0/{gtk.css,gtk-dark.css,assets}`, which the fixture deploys
//! into a stand-in `/usr/share/themes` like `setup.sh` does (analysis §8.4). A
//! theme with a `gtk-4.0/` directory re-points all three, one without removes
//! them (with a page note), a plain file at any of them blocks the step with a
//! `warn` while the INI and gsettings writes land, and a failure after — or in —
//! the link step rolls the links and the INI writes back together.
//!
//! Like Display, these models own their files' freshness (R5.6): the window
//! checks `check_conflict()` and aborts before building the plan, so the
//! conflict test asserts that guard.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::sys::signal::Signal;

use settings4000::core::apply::WriteMode;
use settings4000::core::apply::{self, ApplyOutcome, ApplyPlan, LinkAction, WriteFailureCause};
use settings4000::core::detect::{Binary, Capabilities, Daemon, DetectionInputs};
use settings4000::core::freshness::FreshnessTracker;
use settings4000::core::model::validate_image_value;
use settings4000::core::notifd::{NotifdReplies, NotifdStatus, NotifdVersion};
use settings4000::core::reload::BackingFile;
use settings4000::core::reload::ReloadParams;
use settings4000::core::theme::ThemesApply;
use settings4000::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use settings4000::parsers::lua_literal::{LuaConfigFile, LuaValue};
use settings4000::parsers::palette::SchemaSource;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::{MockProcessSignaller, SignalCall};
use settings4000::testing::{
    CapturedEvent, FIXTURE_ACTIVE_GTK_THEME, FIXTURE_GTK3_ONLY_THEME, FIXTURE_GTK4_THEME,
    FixtureDotfiles, GTK4_THEME_LINK_NAMES, assert_repo_untouched_except, capture_logs,
    differing_lines, expect_applied, replace_once, repo_snapshot,
};

/// Builds the palette model exactly as the startup worker does (task 6.3): the
/// palette source is found by real detection canonicalizing the deployed
/// `colors.conf` symlink into the fixture repo (R3.2/R8.5), and the active scheme
/// is read from that generated file's header.
fn load_palette(fx: &FixtureDotfiles) -> PaletteModel {
    let anchor = fx.config_path("hypr/colors.conf");
    let inputs = DetectionInputs {
        // Only the filesystem probes matter here; binaries/daemons are irrelevant
        // to source detection.
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: anchor.clone(),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
        bar_text_tone_script: fx.config_path("eww/scripts/bar-text-tone"),
    };
    let source_caps = Capabilities::detect(&inputs);
    let source = source_caps
        .palette_source()
        .expect("the deployed symlink reveals the palette source");
    let model = PaletteModel::load(
        source.colors_dir(),
        &anchor,
        source.generate_colors().to_path_buf(),
    );
    // Task 11.6: the schemes are validated against the fixture generator's own
    // `NAMES` line (18 keys with `shadow`), not the built-in fallback.
    assert_eq!(
        model.schema().source(),
        &SchemaSource::Generator(source.generate_colors().to_path_buf())
    );
    model
}

/// Builds the GTK/icon/cursor model exactly as the startup worker does (task 6.4),
/// with the theme roots pointed at the fixture home's `.themes`/`.icons` (which a
/// test populates first) and the backing paths at the deployed config files.
///
/// No settings portal and no `GTK_THEME` in the app environment: the fixture's
/// `uwsm/env` keeps its override commented, so the GTK drop-down stays enabled.
fn load_themes(fx: &FixtureDotfiles) -> ThemesModel {
    let roots = ThemeRoots {
        gtk_theme_dirs: vec![fx.home().join(".themes")],
        icon_dirs: vec![fx.home().join(".icons")],
    };
    ThemesModel::load(&roots, themes_paths(fx), false, None)
}

/// The deployed backing paths of the GTK/icon/cursor model on a Lua-provider host,
/// as the startup loader builds them: the XDG config files plus the home-level
/// XCursor fallback theme.
fn themes_paths(fx: &FixtureDotfiles) -> ThemesPaths {
    ThemesPaths {
        gtk3_settings: fx.config_path("gtk-3.0/settings.ini"),
        gtk4_settings: fx.config_path("gtk-4.0/settings.ini"),
        hyprland_lua: Some(fx.config_path("hypr/hyprland.lua")),
        uwsm_env: fx.config_path("uwsm/env"),
        cursor_fallback_theme: Some(fx.home().join(".icons/default/index.theme")),
        dotfiles_repo: Some(fx.repo_root().to_path_buf()),
        home: Some(fx.home().to_path_buf()),
    }
}

/// Builds the wallpaper model exactly as the startup worker does (task 6.5), with
/// hyprlock present so the lock-background dual write is in play, and the fixture
/// home as the directory `~/…` paths expand against (task 11.10).
fn load_wallpaper(fx: &FixtureDotfiles) -> WallpaperModel {
    WallpaperModel::load(
        WallpaperPaths {
            hyprpaper_conf: fx.config_path("hypr/hyprpaper.conf"),
            hyprlock_conf: fx.config_path("hypr/hyprlock.conf"),
            home: Some(fx.home().to_path_buf()),
        },
        true,
    )
}

/// The configured wallpaper value of the fixture's `hyprpaper.conf` and
/// `hyprlock.conf` (verbatim from the real dotfiles, analysis §7.4).
const FIXTURE_WALLPAPER: &str = "~/Pictures/wallpaper/18.jpg";

/// Stages `chosen` as the wallpaper of a freshly loaded fixture model, applies it
/// through the real pipeline with a mock runner, and returns the recorded commands.
///
/// Hyprland and hyprpaper run, eww does not, so the bar text-tone follow-up is not
/// part of the sequence; [`apply_wallpaper_with`] takes the capabilities instead.
fn apply_wallpaper(fx: &FixtureDotfiles, chosen: &str) -> Vec<Command> {
    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
    apply_wallpaper_with(fx, chosen, &caps)
}

/// [`apply_wallpaper`] under the given capabilities.
fn apply_wallpaper_with(fx: &FixtureDotfiles, chosen: &str, caps: &Capabilities) -> Vec<Command> {
    let mut wallpaper = load_wallpaper(fx);
    assert!(!wallpaper.override_on(), "the fixture unifies both paths");
    wallpaper
        .stage_wallpaper(chosen)
        .expect("an existing image stages");

    let mut plan = empty_plan();
    let contribution = wallpaper
        .apply_contribution()
        .expect("a dirty wallpaper contributes");
    plan.writes.extend(contribution.writes);
    plan.validations.extend(contribution.validations);
    plan.reload_params = contribution.reload_params;

    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        caps,
        &runner,
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(
        written.len(),
        2,
        "hyprpaper.conf AND hyprlock.conf are written"
    );
    runner.recorded()
}

/// Asserts that `relative` (a deployed hypr config) now equals its pre-apply
/// bytes in `before` with exactly the one `old` path value replaced by `new` —
/// and that this is a single-line change.
fn assert_only_the_path_changed(
    fx: &FixtureDotfiles,
    before: &std::collections::BTreeMap<String, Vec<u8>>,
    relative: &str,
    old: &str,
    new: &str,
) {
    let repo_relative = format!("config/{relative}");
    let original =
        String::from_utf8(before[repo_relative.as_str()].clone()).expect("hypr configs are UTF-8");
    let applied = fs::read_to_string(fx.config_path(relative)).expect("read the applied file");
    assert_eq!(
        applied,
        replace_once(&original, old, new),
        "{relative}: only the image path value may change"
    );
    assert_eq!(
        differing_lines(&original, &applied).len(),
        1,
        "{relative}: exactly one line (the path) differs"
    );
}

/// The hyprpaper reload for `absolute` with the fixture's `cover` fit: one
/// `wallpaper` request on every output (task 11.14 — no `preload`, which the
/// installed hyprctl does not know), without the fit field, because `cover` is what
/// hyprpaper's IPC shows when the field is left out.
fn hyprpaper_reload(absolute: &str) -> Vec<Command> {
    let wallpaper_arg = format!(",{absolute}");
    vec![Command::new("hyprctl").args(["hyprpaper", "wallpaper", wallpaper_arg.as_str()])]
}

/// A running notifd reporting `version` (task 11.7's capability).
fn notifd_running(version: &str) -> NotifdStatus {
    NotifdStatus::Running {
        version: NotifdVersion::parse(version),
    }
}

/// notifd's `Reload()` call, exactly as the reload table issues it (task 11.14).
fn notifd_reload() -> Command {
    Command::new("busctl").args([
        "--user",
        "--auto-start=no",
        "call",
        "se.dotfiles.Notifd",
        "/se/dotfiles/Notifd",
        "se.dotfiles.Notifd",
        "Reload",
    ])
}

/// An empty plan to fold a Theme contribution into; the Theme models carry their
/// own validations and reload parameters.
fn empty_plan() -> ApplyPlan {
    ApplyPlan {
        validations: Vec::new(),
        writes: Vec::new(),
        links: Vec::new(),
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

#[test]
fn a_palette_switch_runs_generate_colors_then_the_reload_chain() {
    // The palette happy path (R5.3, task 6.3): switching everforest -> nord runs
    // the fixture's detected `generate-colors nord` (as the only write step —
    // v1 edits no file directly), then the apply-theme reload chain in order,
    // with kitty reloaded by SIGUSR1. The mock runner intercepts the generator,
    // so no repo file — generated partials included — may change.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);

    let mut palette = load_palette(&fx);
    assert_eq!(
        palette.active(),
        Some("everforest"),
        "the active scheme comes from the generated header"
    );
    palette.stage("nord");

    let mut plan = empty_plan();
    plan.palette = palette.apply_contribution();
    assert!(plan.palette.is_some(), "a staged switch contributes");

    // notifd runs and could reload (task 11.14), which pins that a palette switch
    // still asks it for nothing.
    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Eww, Daemon::Kitty], true)
        .with_notifd_for_tests(notifd_running("1.1"));
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![77])]);
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert!(
        written.is_empty(),
        "a palette switch writes no file directly"
    );

    // The exact command sequence: the detected generator path (inside the
    // fixture repo, no shell) with the scheme argument, then the three-step reload
    // chain of `scripts/apply-theme` — `hyprctl reload config-only` (task 11.11),
    // `eww reload` and (below) SIGUSR1 to kitty. There is no notification-daemon
    // step (task 11.7): notifd's popups are eww widgets, recolored by the eww
    // reload.
    let generate_colors = fx.repo_path("scripts/generate-colors");
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new(generate_colors.to_string_lossy()).arg("nord"),
            Command::new("hyprctl").arg("reload").arg("config-only"),
            Command::new("eww").arg("reload"),
        ]
    );
    assert_eq!(
        signaller.calls(),
        vec![SignalCall {
            process_name: "kitty".to_string(),
            signal: Signal::SIGUSR1,
            pids: vec![77],
        }]
    );

    // No repo file changed: the generator was mocked, and the app itself never
    // touches the palette sources or the generated partials (R3.2) — `colors.lua`
    // included, which is why the exception list is empty.
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn a_gtk_theme_change_writes_both_settings_ini_identically() {
    // Task 6.4 (R3.3): a GTK theme switch writes the identical value to BOTH
    // settings.ini duplicates — and nothing else (uwsm/env, hyprland.lua and
    // index.theme are cursor copies only) — then applies it live with one `gsettings set`.
    let fx = FixtureDotfiles::install();
    // Install a GTK theme the scan finds so the drop-down offers it, as on a real
    // machine (a directory with a gtk-3.0/ subdirectory under ~/.themes).
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create the fixture GTK theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes(&fx);
    assert!(
        !themes.gtk_dropdown_disabled(),
        "the fixture's GTK_THEME override is commented, so the drop-down is enabled"
    );
    themes.stage_gtk_theme("Fixture-Theme");

    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;

    // A notifd that could reload runs (task 11.14), so the absence of its `Reload()`
    // below proves a GTK-theme-only change never asks for it.
    let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false)
        .with_notifd_for_tests(notifd_running("1.1"));
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 2, "both settings.ini files are written");

    // (a) Exact bytes: each file is its original with only the gtk-theme-name
    // value span changed — the same value in both (the duplicate contract).
    for relative in ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"] {
        let repo_relative = format!("config/{relative}");
        let original = String::from_utf8(before[repo_relative.as_str()].clone())
            .expect("settings.ini is UTF-8");
        assert_eq!(
            fs::read_to_string(fx.config_path(relative)).expect("read the applied file"),
            replace_once(
                &original,
                "gtk-theme-name=Everforest-Green-Dark",
                "gtk-theme-name=Fixture-Theme",
            ),
            "{relative}: only the GTK theme value may change"
        );
    }

    // (b) The exact reload: one gsettings call, no cursor/icon commands, and no
    // notifd `Reload()`.
    assert_eq!(
        runner.recorded(),
        vec![Command::new("gsettings").args([
            "set",
            "org.gnome.desktop.interface",
            "gtk-theme",
            "Fixture-Theme",
        ])]
    );

    // (c) Everything else untouched — in particular the cursor-only copies.
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/gtk-3.0/settings.ini", "config/gtk-4.0/settings.ini"],
    );
}

/// Task 11.5 (R3.4, the duplicated-values gotcha): a cursor change writes the
/// identical value to every copy — both `settings.ini`, `uwsm/env`, the two
/// `hl.env` lines of `hyprland.lua` and `~/.icons/default/index.theme` — through
/// the deployed symlinks into the repo, then reloads with `hyprctl reload
/// config-only` (the changed Lua config; task 11.11), the two `gsettings set`
/// cursor keys and `hyprctl setcursor`.
///
/// The `hyprland.lua` byte diff is also the proof of the caller contract from task
/// 11.3: the Lua editor can address that file's top-level `hl.config` literals
/// (the real file has e.g. `cursor.inactive_timeout`), but the cursor write may only
/// ever call `LuaConfigFile::set_env`, so exactly the two env line indices may
/// differ and the other 480+ lines must stay byte-identical.
#[test]
fn a_cursor_change_writes_every_duplicated_copy_identically() {
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create the fixture cursor theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes(&fx);
    assert_eq!(
        themes.disagreeing_cursor_copies(),
        Vec::new(),
        "the refreshed fixture's five cursor copies all agree on breeze_cursors/24"
    );
    themes.stage_cursor_theme("Fixture-Cursors");
    themes.stage_cursor_size("32");

    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty cursor contributes");
    assert!(
        contribution
            .writes
            .iter()
            .all(|write| write.mode == WriteMode::Replace),
        "every copy exists in the fixture, so every write is a rewrite"
    );
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;

    let caps = Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true);
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 5, "all five cursor copies are written");

    // (a) hyprland.lua: exactly the two hl.env lines differ, the size as a Lua
    // string. The repo copy behind the deployed symlink carries the change and the
    // link itself is preserved.
    let lua_live = fx.config_path("hypr/hyprland.lua");
    assert!(
        fs::symlink_metadata(&lua_live)
            .expect("hyprland.lua is deployed")
            .file_type()
            .is_symlink(),
        "the deployment link is preserved"
    );
    let lua_before = String::from_utf8(before["config/hypr/hyprland.lua"].clone())
        .expect("hyprland.lua is UTF-8");
    let lua_after =
        fs::read_to_string(fx.repo_path("config/hypr/hyprland.lua")).expect("read the repo copy");
    let env_line = |name: &str| {
        lua_before
            .lines()
            .position(|line| line.starts_with(&format!("hl.env(\"{name}\"")))
            .unwrap_or_else(|| panic!("the fixture has an hl.env line for {name}"))
    };
    assert!(lua_before.lines().count() > 480, "the real, whole file");
    assert_eq!(
        differing_lines(&lua_before, &lua_after),
        vec![env_line("XCURSOR_THEME"), env_line("XCURSOR_SIZE")],
        "only the two hl.env lines may change"
    );
    assert_eq!(
        lua_after,
        replace_once(
            &replace_once(
                &lua_before,
                "hl.env(\"XCURSOR_THEME\", \"breeze_cursors\")",
                "hl.env(\"XCURSOR_THEME\", \"Fixture-Cursors\")",
            ),
            "hl.env(\"XCURSOR_SIZE\", \"24\")",
            "hl.env(\"XCURSOR_SIZE\", \"32\")",
        )
    );
    // The addressable-but-not-owned literal the contract protects is untouched.
    assert_eq!(
        LuaConfigFile::parse(&lua_after).value("cursor.inactive_timeout"),
        Some(&LuaValue::Integer(0))
    );

    // (b) The other copies: each file's own key/format, the identical value.
    let expect_patched = |repo_relative: &str, edits: &[(&str, &str)]| {
        let original = String::from_utf8(before[repo_relative].clone())
            .expect("theme backing files are UTF-8");
        let expected = edits
            .iter()
            .fold(original, |text, (from, to)| replace_once(&text, from, to));
        assert_eq!(
            fs::read_to_string(fx.repo_path(repo_relative)).expect("read the applied file"),
            expected,
            "{repo_relative}: only the cursor values may change"
        );
    };
    for settings in ["config/gtk-3.0/settings.ini", "config/gtk-4.0/settings.ini"] {
        expect_patched(
            settings,
            &[
                (
                    "gtk-cursor-theme-name=breeze_cursors",
                    "gtk-cursor-theme-name=Fixture-Cursors",
                ),
                ("gtk-cursor-theme-size=24", "gtk-cursor-theme-size=32"),
            ],
        );
    }
    expect_patched(
        "config/uwsm/env",
        &[
            (
                "export XCURSOR_THEME=breeze_cursors",
                "export XCURSOR_THEME=Fixture-Cursors",
            ),
            ("export XCURSOR_SIZE=24", "export XCURSOR_SIZE=32"),
        ],
    );
    expect_patched(
        "icons/default/index.theme",
        &[("Inherits=breeze_cursors", "Inherits=Fixture-Cursors")],
    );
    assert!(
        fs::symlink_metadata(fx.home().join(".icons/default/index.theme"))
            .expect("index.theme is deployed")
            .file_type()
            .is_symlink(),
        "the home-level deployment link is preserved"
    );

    // (c) The exact command sequence in the canonical order: `hyprctl reload
    // config-only` (the changed hyprland.lua; task 11.11), the two gsettings
    // cursor keys, then hyprctl setcursor — ONCE despite five files sharing it.
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("hyprctl").arg("reload").arg("config-only"),
            Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "cursor-theme",
                "Fixture-Cursors",
            ]),
            Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "cursor-size",
                "32",
            ]),
            Command::new("hyprctl").args(["setcursor", "Fixture-Cursors", "32"]),
        ]
    );

    // (d) Everything else untouched, and the copies agree again after the commit.
    assert_repo_untouched_except(
        &fx,
        &before,
        &[
            "config/gtk-3.0/settings.ini",
            "config/gtk-4.0/settings.ini",
            "config/hypr/hyprland.lua",
            "config/uwsm/env",
            "icons/default/index.theme",
        ],
    );
    themes.commit();
    assert!(!themes.is_dirty());
    assert_eq!(themes.disagreeing_cursor_copies(), Vec::new());
}

#[test]
fn a_cursor_change_creates_a_missing_xcursor_fallback_theme() {
    // Analysis §8.4: on a host without the dotfiles' `~/.icons/default/index.theme`
    // (here: the deployment link and its directory removed), a cursor theme change
    // creates the file — a plain file with the three standard keys — in a freshly
    // created plain `~/.icons/default`, alongside the rewrites of the other copies.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create the fixture cursor theme");
    let icons_default = fx.home().join(".icons/default");
    fs::remove_dir_all(&icons_default).expect("remove the deployed fallback theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes(&fx);
    themes.stage_cursor_theme("Fixture-Cursors");
    let contribution = themes
        .apply_contribution()
        .expect("a dirty cursor contributes");
    let create = contribution
        .writes
        .iter()
        .find(|write| write.backing == BackingFile::CursorFallbackTheme)
        .expect("the missing fallback theme is part of the write set");
    assert_eq!(create.mode, WriteMode::CreateNew);

    let mut plan = empty_plan();
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;
    let caps = Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true);
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &caps,
        &MockCommandRunner::new(),
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 5, "four rewrites and one creation");

    let created = icons_default.join("index.theme");
    assert_eq!(
        fs::read_to_string(&created).expect("the fallback theme exists"),
        "[Icon Theme]\nName=Default\nComment=Default Cursor Theme\nInherits=Fixture-Cursors\n"
    );
    for path in [&icons_default, &created] {
        let meta = fs::symlink_metadata(path).expect("created");
        assert!(
            !meta.file_type().is_symlink(),
            "{} is plain",
            path.display()
        );
    }
    // The repo's tracked copy is no longer deployed, so it is not touched.
    assert_repo_untouched_except(
        &fx,
        &before,
        &[
            "config/gtk-3.0/settings.ini",
            "config/gtk-4.0/settings.ini",
            "config/hypr/hyprland.lua",
            "config/uwsm/env",
        ],
    );
    themes.commit();
    assert!(!themes.is_dirty());
    assert!(
        !themes.check_conflict(),
        "the file the app created is not a self-conflict"
    );
}

/// Installs an icon theme the scan finds, named `name` under the fixture home's
/// `.icons` (an `index.theme` plus a size directory, which is what the scan
/// requires of an icon theme) and returns a themes model that offers it.
fn themes_with_icon_theme(fx: &FixtureDotfiles, name: &str) -> ThemesModel {
    let theme = fx.home().join(".icons").join(name);
    fs::create_dir_all(theme.join("48x48")).expect("create the icon theme");
    fs::write(theme.join("index.theme"), b"[Icon Theme]\n").expect("write its index");
    let themes = load_themes(fx);
    assert!(
        themes.icon_themes().iter().any(|theme| theme == name),
        "the drop-down offers {name}"
    );
    themes
}

/// Applies `themes`' contribution under `caps` and returns the recorded commands.
fn apply_themes(themes: &ThemesModel, caps: &Capabilities) -> Vec<Command> {
    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;
    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        caps,
        &runner,
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 2, "both settings.ini files are written");
    runner.recorded()
}

#[test]
fn an_icon_theme_change_ends_with_exactly_one_notifd_reload() {
    // Task 11.14 (R5.3, analysis §8.2): notifd reads the icon theme from
    // gtk-3.0/settings.ini at start and on `Reload()`, so after writing both INI
    // files the Apply sets the gsettings key and then calls `Reload()` once — over
    // `busctl --auto-start=no`, never a unit restart.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let mut themes = themes_with_icon_theme(&fx, "Fixture-Icons");
    themes.stage_icon_theme("Fixture-Icons");

    let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false)
        .with_notifd_for_tests(notifd_running("1.1"));
    let recorded = apply_themes(&themes, &caps);

    assert_eq!(
        recorded,
        vec![
            Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "icon-theme",
                "Fixture-Icons",
            ]),
            notifd_reload(),
        ]
    );
    assert_eq!(
        recorded.iter().filter(|c| **c == notifd_reload()).count(),
        1,
        "exactly one Reload call"
    );
    assert!(
        recorded.iter().all(|c| c.program() != "systemctl"),
        "notifd is never restarted"
    );
    // The file notifd re-reads holds the new name.
    let gtk3 = fs::read_to_string(fx.config_path("gtk-3.0/settings.ini")).expect("read gtk-3.0");
    assert!(gtk3.contains("gtk-icon-theme-name=Fixture-Icons"), "{gtk3}");
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/gtk-3.0/settings.ini", "config/gtk-4.0/settings.ini"],
    );
}

#[test]
fn an_older_notifd_gets_no_reload_and_the_page_says_when_icons_follow() {
    // Task 11.14: notifd 1.0, as real detection resolves it from the probe replies,
    // has no `Reload()`. The Apply still writes the icon theme and sets gsettings,
    // issues no notifd call at all, and the Theme page carries the note that
    // notification icons follow at the next notifd start.
    let fx = FixtureDotfiles::install();
    let detected = Capabilities::detect(&DetectionInputs {
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies {
            name_has_owner: Some("b true\n".to_string()),
            server_information: Some("ssss \"notifd\" \"dotfiles\" \"1.0\" \"1.2\"\n".to_string()),
            unit_active: None,
        },
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
        bar_text_tone_script: fx.config_path("eww/scripts/bar-text-tone"),
    });
    assert_eq!(detected.notifd(), notifd_running("1.0"));

    let mut themes =
        themes_with_icon_theme(&fx, "Fixture-Icons").with_notifd_status(detected.notifd());
    let note = themes
        .notification_icons_note()
        .expect("an older running notifd is noted");
    assert!(note.contains("next time"), "got {note}");
    themes.stage_icon_theme("Fixture-Icons");

    let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false)
        .with_notifd_for_tests(detected.notifd());
    let recorded = apply_themes(&themes, &caps);
    assert_eq!(
        recorded,
        vec![Command::new("gsettings").args([
            "set",
            "org.gnome.desktop.interface",
            "icon-theme",
            "Fixture-Icons",
        ])],
        "no busctl Reload and no restart on a 1.0 daemon"
    );

    // A 1.1 daemon gets the reload instead, and no note.
    assert_eq!(
        load_themes(&fx)
            .with_notifd_status(notifd_running("1.1"))
            .notification_icons_note(),
        None
    );
}

#[test]
fn the_fixture_wallpaper_value_loads_as_an_existing_image() {
    // Task 11.10: the configured `~/…` value is read verbatim, and its expanded
    // form is the installed stub — which is what validation and the file
    // chooser use.
    let fx = FixtureDotfiles::install();
    let wallpaper = load_wallpaper(&fx);
    assert_eq!(wallpaper.wallpaper_path(), Some(FIXTURE_WALLPAPER));
    assert_eq!(wallpaper.wallpaper_file(), Some(fx.wallpaper_path()));
    assert!(fx.wallpaper_path().is_file(), "the installed stub exists");
    assert!(!wallpaper.is_dirty(), "a freshly loaded page is clean");
    assert!(
        validate_image_value(FIXTURE_WALLPAPER, Some(fx.home())).is_ok(),
        "the configured value passes R8.3 validation"
    );
}

#[test]
fn a_wallpaper_change_dual_writes_hyprpaper_and_hyprlock_and_reloads() {
    // Task 6.5 (the unified-wallpaper gotcha): with the lock override off, one
    // wallpaper change writes the SAME new path to hyprpaper.conf and
    // hyprlock.conf, then reloads hyprpaper live with one `wallpaper` request
    // (task 11.14: no `preload`; the fixture's `cover` fit is the IPC default, so
    // no fit field). hyprlock gets no reload — it reads its config at launch
    // (intentional).
    //
    // Task 11.10: the image lies under the home directory and both files store
    // `~/…`, so both receive `~/…`, while the reload names the absolute path
    // (hyprpaper's IPC rejects anything not starting with `/`).
    let fx = FixtureDotfiles::install();
    // The new image the user picks; must exist and be readable for R8.3. The file
    // chooser hands back an absolute path.
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper_str = new_wallpaper.to_str().expect("UTF-8 path").to_string();
    let before = repo_snapshot(&fx);

    let recorded = apply_wallpaper(&fx, &new_wallpaper_str);

    // (a) Exact bytes: both files get the identical new home-relative path; every
    // other byte — including the fit mode and hyprlock's many other sections — is
    // untouched.
    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        assert_only_the_path_changed(
            &fx,
            &before,
            relative,
            FIXTURE_WALLPAPER,
            "~/Pictures/wallpaper/next.png",
        );
    }

    // (b) The exact reload, with the absolute path; nothing for hyprlock.
    assert_eq!(recorded, hyprpaper_reload(&new_wallpaper_str));

    // (c) Everything else untouched.
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/hypr/hyprpaper.conf", "config/hypr/hyprlock.conf"],
    );
}

/// The capabilities of a session where Hyprland, hyprpaper and eww run and
/// detection found the fixture's bar text-tone stub at its live path.
fn bar_tone_capabilities(fx: &FixtureDotfiles, daemons: &[Daemon]) -> Capabilities {
    let detected = detect_fixture(fx, &[]);
    let script = detected
        .bar_text_tone_script()
        .expect("real detection finds the fixture's bar-text-tone stub");
    assert_eq!(script, fx.config_path("eww/scripts/bar-text-tone"));
    Capabilities::for_tests(&[Binary::Hyprctl], daemons, true)
        .with_bar_text_tone_script_for_tests(script.to_path_buf())
}

/// Stages and applies a new wallpaper under `caps`, returning the new image's
/// absolute path and the recorded commands.
fn apply_new_wallpaper(fx: &FixtureDotfiles, caps: &Capabilities) -> (String, Vec<Command>) {
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper_str = new_wallpaper.to_str().expect("UTF-8 path").to_string();
    let recorded = apply_wallpaper_with(fx, &new_wallpaper_str, caps);
    (new_wallpaper_str, recorded)
}

#[test]
fn a_wallpaper_apply_pushes_the_bar_text_tone_after_the_single_hyprpaper_request() {
    // Task 11.14 (architecture §6, analysis §7.8): with eww running and the script
    // present, the wallpaper request is followed by `bar-text-tone --apply`, run by
    // its live XDG path. The sequence has no `preload` and exactly one `wallpaper`
    // request, carrying the absolute image path.
    let fx = FixtureDotfiles::install();
    let caps = bar_tone_capabilities(&fx, &[Daemon::Hyprpaper, Daemon::Eww]);
    let (absolute, recorded) = apply_new_wallpaper(&fx, &caps);

    let script = fx.config_path("eww/scripts/bar-text-tone");
    let mut expected = hyprpaper_reload(&absolute);
    expected.push(Command::new(script.to_str().expect("UTF-8 path")).arg("--apply"));
    assert_eq!(recorded, expected);

    assert!(
        recorded
            .iter()
            .all(|c| !c.args_slice().iter().any(|a| a == "preload")),
        "no preload request: {recorded:?}"
    );
    let wallpaper_requests: Vec<&Command> = recorded
        .iter()
        .filter(|c| c.args_slice().get(1).is_some_and(|a| a == "wallpaper"))
        .collect();
    assert_eq!(wallpaper_requests.len(), 1, "{recorded:?}");
    assert_eq!(
        wallpaper_requests[0].args_slice()[2],
        format!(",{absolute}"),
        "the request names the absolute path, never `~/…`"
    );
}

#[test]
fn the_bar_text_tone_is_skipped_without_eww_or_without_the_script() {
    // Task 11.14: eww not running — there is no bar to update, so only the hyprpaper
    // request runs.
    let fx = FixtureDotfiles::install();
    let caps = bar_tone_capabilities(&fx, &[Daemon::Hyprpaper]);
    let (absolute, recorded) = apply_new_wallpaper(&fx, &caps);
    assert_eq!(recorded, hyprpaper_reload(&absolute));

    // The script absent from the deployed tree: real detection reports none, and
    // the follow-up is skipped even though eww runs.
    let fx = FixtureDotfiles::install();
    fs::remove_file(fx.config_path("eww/scripts/bar-text-tone")).expect("remove the stub link");
    let detected = detect_fixture(&fx, &[]);
    assert!(detected.bar_text_tone_script().is_none());
    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper, Daemon::Eww], true);
    let (absolute, recorded) = apply_new_wallpaper(&fx, &caps);
    assert_eq!(recorded, hyprpaper_reload(&absolute));
}

#[test]
fn a_wallpaper_outside_home_is_written_absolute() {
    // Task 11.10: an image outside the home directory has no `~/…` spelling, so
    // it is written absolute into both files even though they store `~/…`.
    let fx = FixtureDotfiles::install();
    let outside_dir = fx
        .home()
        .parent()
        .expect("the fixture home has a parent")
        .join("shared-backgrounds");
    fs::create_dir_all(&outside_dir).expect("create a directory outside the home");
    let outside = outside_dir.join("dunes.jpg");
    fs::write(&outside, b"fixture outside wallpaper").expect("create the image");
    let outside_str = outside.to_str().expect("UTF-8 path").to_string();
    assert!(
        !outside.starts_with(fx.home()),
        "the image is outside the home"
    );
    let before = repo_snapshot(&fx);

    let recorded = apply_wallpaper(&fx, &outside_str);

    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        assert_only_the_path_changed(&fx, &before, relative, FIXTURE_WALLPAPER, &outside_str);
    }
    assert_eq!(recorded, hyprpaper_reload(&outside_str));
}

#[test]
fn an_absolute_wallpaper_original_keeps_its_absolute_form() {
    // Task 11.10, the other form: a host whose configs store absolute paths keeps
    // them, even for an image under the home directory. The originals are
    // rewritten to the absolute spelling of the same stub (fixture setup, not the
    // code under test), and the change is a surgical value-span edit in both.
    let fx = FixtureDotfiles::install();
    let absolute = fx.wallpaper_path();
    let absolute_str = absolute.to_str().expect("UTF-8 path");
    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        let live = fx.config_path(relative);
        let text = fs::read_to_string(&live).expect("read the installed file");
        fs::write(&live, replace_once(&text, FIXTURE_WALLPAPER, absolute_str))
            .expect("rewrite the original to the absolute form");
    }
    let before = repo_snapshot(&fx);
    assert_eq!(
        load_wallpaper(&fx).wallpaper_path(),
        Some(absolute_str),
        "the absolute original is read verbatim"
    );

    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper_str = new_wallpaper.to_str().expect("UTF-8 path").to_string();

    let recorded = apply_wallpaper(&fx, &new_wallpaper_str);

    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        assert_only_the_path_changed(&fx, &before, relative, absolute_str, &new_wallpaper_str);
    }
    assert_eq!(recorded, hyprpaper_reload(&new_wallpaper_str));
}

#[test]
fn a_missing_or_non_image_wallpaper_is_rejected_before_staging() {
    // R8.3 still holds with home-relative values: a missing file and a non-image
    // are rejected whether chosen absolute or spelled `~/…`, and nothing stages.
    let fx = FixtureDotfiles::install();
    let notes = fx.home().join("Pictures/wallpaper/notes.txt");
    fs::write(&notes, b"not an image").expect("create a non-image file");
    let missing = fx.home().join("Pictures/wallpaper/missing.jpg");

    let mut wallpaper = load_wallpaper(&fx);
    for rejected in [
        missing.to_str().expect("UTF-8 path"),
        "~/Pictures/wallpaper/missing.jpg",
        notes.to_str().expect("UTF-8 path"),
        "~/Pictures/wallpaper/notes.txt",
    ] {
        assert!(
            wallpaper.stage_wallpaper(rejected).is_err(),
            "{rejected} must be rejected"
        );
        assert!(
            wallpaper.stage_lock(rejected).is_err(),
            "{rejected} must be rejected as a lock image too"
        );
    }
    assert!(!wallpaper.is_dirty(), "a rejected path is never staged");
    assert_eq!(wallpaper.wallpaper_path(), Some(FIXTURE_WALLPAPER));
}

#[test]
fn a_generate_colors_failure_rolls_back_the_theme_writes_and_reloads_nothing() {
    // Failure injection (1), R5.4 + the palette-gotcha ordering: a combined apply
    // (GTK theme switch + palette switch) whose generator exits non-zero. The two
    // settings.ini writes — which genuinely happened first, proven by their
    // rollback — are restored byte-exactly, generate-colors is the ONLY command
    // ever run (it is the LAST write step, so no reload followed), and the whole
    // repo — the generated partials above all — is byte-identical afterwards.
    // Fixture-Theme ships only gtk-3.0, so the plan also removes the three GTK 4
    // theme links (task 11.16); the rollback re-creates them.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create the fixture GTK theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme("Fixture-Theme");
    let mut palette = load_palette(&fx);
    palette.stage("nord");

    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    assert_eq!(contribution.links.len(), 3, "one removal per link");
    assert!(
        contribution
            .links
            .iter()
            .all(|link| link.action == LinkAction::Remove),
        "a GTK 3-only theme removes the links"
    );
    let mut plan = plan_with_themes(contribution);
    plan.palette = palette.apply_contribution();

    // The generator fails — also the shape of a missing/incomplete theme/fonts,
    // on which generate-colors aborts (the recorded palette gotcha).
    let caps = Capabilities::for_tests(
        &[Binary::Gsettings, Binary::Hyprctl],
        &[Daemon::Eww, Daemon::Kitty],
        true,
    );
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
        1,
        "",
        "generate-colors: theme/fonts is missing or incomplete; aborting\n",
    ))]);
    let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![77])]);
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);

    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            assert!(
                matches!(
                    failure.cause,
                    WriteFailureCause::GenerateColorsExit { code: Some(1), .. }
                ),
                "the cause is the generator's non-zero exit, got {:?}",
                failure.cause
            );
            // Task 9.10: the generator's own reason — here the documented
            // missing/incomplete `theme/fonts` abort — reaches the message the user is
            // shown, not just its exit code.
            assert!(
                failure
                    .cause
                    .to_string()
                    .contains("theme/fonts is missing or incomplete"),
                "the failure message must carry the generator's diagnostic, got {}",
                failure.cause
            );
            // Every completed step is rolled back newest-first — the three link
            // removals (by their own path), then both settings.ini files (resolved
            // repo targets) — proof they all preceded the generator.
            assert_eq!(
                failure.rolled_back,
                vec![
                    gtk4_link(&fx, "assets"),
                    gtk4_link(&fx, "gtk-dark.css"),
                    gtk4_link(&fx, "gtk.css"),
                    fx.repo_path("config/gtk-4.0/settings.ini"),
                    fx.repo_path("config/gtk-3.0/settings.ini"),
                ]
            );
            assert!(failure.rollback_failures.is_empty());
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }

    // generate-colors ran LAST among the write steps and nothing followed it: no
    // reload command, no kitty signal — a failed palette apply can never leave
    // the desktop half-reloaded onto a new scheme.
    let generate_colors = fx.repo_path("scripts/generate-colors");
    assert_eq!(
        runner.recorded(),
        vec![Command::new(generate_colors.to_string_lossy()).arg("nord")]
    );
    assert!(signaller.calls().is_empty());

    // The rollback restored everything: the whole repo — settings.ini files,
    // palette sources, and every generated partial — is byte-identical (R5.4), and
    // the GTK 4 links point at the active theme again.
    assert_repo_untouched_except(&fx, &before, &[]);
    assert_eq!(
        gtk4_link_targets(&fx),
        targets_into(&fx, FIXTURE_ACTIVE_GTK_THEME)
    );
}

#[test]
fn an_external_hyprpaper_edit_trips_the_wallpaper_conflict_guard() {
    // The wallpaper flavour of the model-owned guard (R5.6): an external
    // hyprpaper.conf edit after load must trip `WallpaperModel::check_conflict`,
    // on which the window aborts and reloads before building any plan.
    let fx = FixtureDotfiles::install();
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");

    let mut wallpaper = load_wallpaper(&fx);
    wallpaper
        .stage_wallpaper(new_wallpaper.to_str().expect("UTF-8 path"))
        .expect("an existing image stages");
    assert!(
        !wallpaper.check_conflict(),
        "an untouched file is no conflict"
    );

    let live = fx.config_path("hypr/hyprpaper.conf");
    let edited = format!(
        "{}# edited by hand while the app was open\n",
        fs::read_to_string(&live).expect("read the installed file")
    );
    fs::write(&live, &edited).expect("apply the external edit");

    assert!(
        wallpaper.check_conflict(),
        "the external edit must be detected before any write (R5.6)"
    );
    assert_eq!(
        fs::read_to_string(&live).expect("read the file"),
        edited,
        "nothing was written; the external edit stands"
    );
}

#[test]
fn an_external_settings_ini_edit_trips_the_themes_conflict_guard() {
    // Failure injection (3), R5.6 — the Theme flavour: the GTK/icon/cursor backing
    // files' freshness is model-owned, so the window's guard is `check_conflict()`
    // before the plan is built. An external edit after load must trip it.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create the fixture GTK theme");

    let mut themes = load_themes(&fx);
    themes.stage_gtk_theme("Fixture-Theme");
    assert!(!themes.check_conflict(), "an untouched file is no conflict");

    let live = fx.config_path("gtk-3.0/settings.ini");
    let edited = format!(
        "{}# edited by hand while the app was open\n",
        fs::read_to_string(&live).expect("read the installed file")
    );
    fs::write(&live, &edited).expect("apply the external edit");

    assert!(
        themes.check_conflict(),
        "the external edit must be detected before any write (R5.6)"
    );
    assert_eq!(
        fs::read_to_string(&live).expect("read the file"),
        edited,
        "nothing was written; the external edit stands"
    );
}

/// Runs real detection against the fixture tree (task 11.12): the deployed
/// `colors.conf` symlink reveals the repo root exactly as at startup (R8.5), and
/// `running` stands in for the procfs process list.
fn detect_fixture(fx: &FixtureDotfiles, running: &[&str]) -> Capabilities {
    Capabilities::detect(&DetectionInputs {
        path: None,
        running_processes: running.iter().map(|name| (*name).to_string()).collect(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
        bar_text_tone_script: fx.config_path("eww/scripts/bar-text-tone"),
    })
}

/// Builds the GTK/icon/cursor model the way the startup loader does from detected
/// capabilities: the repo root and the `xsettingsd` flag come from `caps`.
fn load_themes_detected(fx: &FixtureDotfiles, caps: &Capabilities) -> ThemesModel {
    let roots = ThemeRoots {
        gtk_theme_dirs: vec![fx.home().join(".themes")],
        icon_dirs: vec![fx.home().join(".icons")],
    };
    let paths = ThemesPaths {
        dotfiles_repo: caps.dotfiles_repo_root().map(Path::to_path_buf),
        ..themes_paths(fx)
    };
    ThemesModel::load(&roots, paths, false, None)
        .with_xsettingsd_running(caps.is_daemon_live(Daemon::Xsettingsd))
}

/// Whether `events` hold the `warn` the themes model logs for an unlinked
/// `settings.ini`.
fn unlinked_warnings(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|event| {
            event.level == tracing::Level::WARN && event.message.contains("no longer linked")
        })
        .collect()
}

#[test]
fn a_de_linked_gtk4_settings_ini_is_reported_and_its_live_file_still_written() {
    // Task 11.12 (analysis §7.7, R5.6/R8.5): the shape `nwg-look` left on the target
    // machine — the deployed gtk-4.0/settings.ini symlink replaced by a plain copy
    // while the repo keeps its own. The model reports exactly that file, the note
    // names it, a `warn` names the paths (never the contents, R7.3), and a GTK-theme
    // Apply still writes the live plain file (it is what GTK reads) while the repo
    // copy stays untouched; the still-linked gtk-3.0 copy is written through its link.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create the fixture GTK theme");
    let live_gtk4 = fx.config_path("gtk-4.0/settings.ini");
    fx.replace_link_with_plain_copy(&live_gtk4);
    let before = repo_snapshot(&fx);
    let original = fs::read_to_string(&live_gtk4).expect("read the plain live copy");

    let caps = detect_fixture(&fx, &[]);
    assert_eq!(caps.dotfiles_repo_root(), Some(fx.repo_root()));

    // One load, inside the capture, so the warn count and the note below come from the
    // same model.
    let mut loaded = None;
    let events = capture_logs(|| loaded = Some(load_themes_detected(&fx, &caps)));
    let mut themes = loaded.expect("the model was loaded");
    let warnings = unlinked_warnings(&events);
    assert_eq!(
        warnings.len(),
        1,
        "one warn for the one plain file: {events:?}"
    );
    let live_text = live_gtk4.display().to_string();
    assert!(warnings[0].fields.contains(&live_text), "{:?}", warnings[0]);
    assert!(
        !warnings[0].fields.contains("gtk-theme-name")
            && !warnings[0].message.contains("[Settings]"),
        "the warning names paths only, never file contents (R7.3): {:?}",
        warnings[0]
    );

    assert_eq!(themes.unlinked_gtk_settings(), vec![live_gtk4.as_path()]);
    // Both paths lie under the fixture home, so the note shows them as `~/…`.
    assert_eq!(
        themes.unlinked_gtk_settings_notes(),
        vec![
            "~/.config/gtk-4.0/settings.ini is no longer linked to the dotfiles; changes \
             here will not reach the repo copy at ~/.dotfiles/config/gtk-4.0/settings.ini."
                .to_string()
        ]
    );
    assert_eq!(themes.external_gtk_writer_note(), None);

    themes.stage_gtk_theme("Fixture-Theme");
    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;
    let apply_caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &apply_caps,
        &MockCommandRunner::new(),
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 2, "both settings.ini files are written");

    // The live plain file got the surgical edit and is still a plain file — the app
    // never re-links it (nor turns a link into a plain file).
    assert_eq!(
        fs::read_to_string(&live_gtk4).expect("read the applied live file"),
        replace_once(
            &original,
            "gtk-theme-name=Everforest-Green-Dark",
            "gtk-theme-name=Fixture-Theme",
        )
    );
    assert!(
        !fs::symlink_metadata(&live_gtk4)
            .expect("lstat the live file")
            .file_type()
            .is_symlink()
    );
    // Only the still-linked gtk-3.0 copy changed in the repo; the gtk-4.0 repo copy
    // is byte-identical, which is exactly what the note warns about.
    assert_repo_untouched_except(&fx, &before, &["config/gtk-3.0/settings.ini"]);
}

#[test]
fn a_fully_linked_fixture_tree_reports_no_unlinked_settings_ini() {
    // Task 11.12: the deployed tree as setup.sh leaves it — both settings.ini are
    // links into the repo — gets no note and no warning.
    let fx = FixtureDotfiles::install();
    let caps = detect_fixture(&fx, &[]);
    assert!(caps.dotfiles_repo_root().is_some());
    let events = capture_logs(|| {
        let themes = load_themes_detected(&fx, &caps);
        assert!(themes.unlinked_gtk_settings().is_empty());
        assert!(themes.unlinked_gtk_settings_notes().is_empty());
        assert_eq!(themes.external_gtk_writer_note(), None);
    });
    assert!(unlinked_warnings(&events).is_empty(), "{events:?}");
}

#[test]
fn plain_settings_ini_files_without_a_dotfiles_repo_are_not_reported() {
    // Task 11.12 (R8.5): on a machine without the dotfiles every config is a plain
    // file and there is no repo root; that is the normal case, not a divergence.
    let tmp = tempfile::tempdir().expect("temp dir");
    let config = tmp.path().join("config");
    for relative in [
        "gtk-3.0/settings.ini",
        "gtk-4.0/settings.ini",
        "hypr/colors.conf",
    ] {
        let path = config.join(relative);
        fs::create_dir_all(path.parent().expect("has a parent")).expect("create a config dir");
        fs::write(&path, "[Settings]\ngtk-theme-name=Adwaita\n").expect("write a plain config");
    }
    let caps = Capabilities::detect(&DetectionInputs {
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: config.join("hypr/colors.conf"),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: config.join("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: config.join("systemd/user/notifd.service"),
        bar_text_tone_script: config.join("eww/scripts/bar-text-tone"),
    });
    assert_eq!(caps.dotfiles_repo_root(), None);

    let paths = ThemesPaths {
        gtk3_settings: config.join("gtk-3.0/settings.ini"),
        gtk4_settings: config.join("gtk-4.0/settings.ini"),
        hyprland_lua: None,
        uwsm_env: config.join("uwsm/env"),
        cursor_fallback_theme: None,
        dotfiles_repo: caps.dotfiles_repo_root().map(Path::to_path_buf),
        home: None,
    };
    let roots = ThemeRoots {
        gtk_theme_dirs: Vec::new(),
        icon_dirs: Vec::new(),
    };
    let events = capture_logs(|| {
        let themes = ThemesModel::load(&roots, paths.clone(), false, None);
        assert!(
            themes.themes_editable(),
            "the plain files are read as usual"
        );
        assert!(themes.unlinked_gtk_settings_notes().is_empty());
    });
    assert!(unlinked_warnings(&events).is_empty(), "{events:?}");
}

#[test]
fn a_running_xsettingsd_in_the_process_list_sets_the_external_writer_note() {
    // Task 11.12 (analysis §7.7): detection scans for xsettingsd like any daemon,
    // and a running one turns on the Theme page's external-writer note.
    let fx = FixtureDotfiles::install();
    let caps = detect_fixture(&fx, &["kitty", "xsettingsd"]);
    assert!(caps.is_daemon_live(Daemon::Xsettingsd));
    let note = load_themes_detected(&fx, &caps)
        .external_gtk_writer_note()
        .expect("a running xsettingsd is noted");
    assert!(note.starts_with("xsettingsd is running"), "got {note}");

    let caps = detect_fixture(&fx, &["kitty"]);
    assert_eq!(
        load_themes_detected(&fx, &caps).external_gtk_writer_note(),
        None
    );
}

// --- Task 11.16: the GTK 4 theme links ---------------------------------------------

/// The GTK/icon/cursor model with the stand-in system theme root among its GTK theme
/// roots, as the startup loader includes `/usr/share/themes` (task 11.16).
fn load_themes_with_system_root(fx: &FixtureDotfiles) -> ThemesModel {
    let roots = ThemeRoots {
        gtk_theme_dirs: vec![
            fx.home().join(".themes"),
            fx.system_themes_root().to_path_buf(),
        ],
        icon_dirs: vec![fx.home().join(".icons")],
    };
    ThemesModel::load(&roots, themes_paths(fx), false, None)
}

/// The live path of one GTK 4 theme link, `~/.config/gtk-4.0/<name>`.
fn gtk4_link(fx: &FixtureDotfiles, name: &str) -> PathBuf {
    fx.config_path("gtk-4.0").join(name)
}

/// The target text of each GTK 4 theme link, `None` for one that is not a link.
fn gtk4_link_targets(fx: &FixtureDotfiles) -> Vec<Option<PathBuf>> {
    GTK4_THEME_LINK_NAMES
        .iter()
        .map(|name| fs::read_link(gtk4_link(fx, name)).ok())
        .collect()
}

/// The three link targets into `theme`'s `gtk-4.0/` under the system root.
fn targets_into(fx: &FixtureDotfiles, theme: &str) -> Vec<Option<PathBuf>> {
    let dir = fx.system_theme_gtk4_dir(theme);
    GTK4_THEME_LINK_NAMES
        .iter()
        .map(|name| Some(dir.join(name)))
        .collect()
}

/// Every entry under the system theme root: a file's bytes, `None` for a directory.
/// Panics on a symlink — the stand-in root has none, so one appearing would mean a
/// link step wrote inside the theme root.
fn theme_root_snapshot(fx: &FixtureDotfiles) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).expect("read a theme dir") {
            let path = entry.expect("entry").path();
            let kind = fs::symlink_metadata(&path).expect("lstat").file_type();
            assert!(!kind.is_symlink(), "{} became a link", path.display());
            if kind.is_dir() {
                out.insert(path.clone(), None);
                walk(&path, out);
            } else {
                out.insert(path.clone(), Some(fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(fx.system_themes_root(), &mut out);
    out
}

/// A plan holding exactly the Theme contribution: its writes, its link steps and its
/// reload parameters.
fn plan_with_themes(contribution: ThemesApply) -> ApplyPlan {
    let mut plan = empty_plan();
    plan.writes = contribution.writes;
    plan.links = contribution.links;
    plan.reload_params = contribution.reload_params;
    plan
}

/// The one `gsettings set … gtk-theme <theme>` call a GTK-theme Apply makes.
fn gsettings_gtk_theme(theme: &str) -> Command {
    Command::new("gsettings").args(["set", "org.gnome.desktop.interface", "gtk-theme", theme])
}

#[test]
fn the_fixture_deploys_the_gtk4_links_into_the_active_theme() {
    // The starting state every test below relies on: setup.sh's three links into the
    // theme the fixture settings.ini names, and the page shows no link note for it.
    let fx = FixtureDotfiles::install();
    assert_eq!(
        gtk4_link_targets(&fx),
        targets_into(&fx, FIXTURE_ACTIVE_GTK_THEME)
    );
    let themes = load_themes_with_system_root(&fx);
    assert_eq!(themes.gtk4_links_note(), None);
    assert!(
        themes
            .gtk_themes()
            .iter()
            .any(|name| name == FIXTURE_GTK4_THEME)
    );
}

#[test]
fn a_gtk_theme_with_a_gtk4_dir_re_points_all_three_links() {
    // R3.3, analysis §8.4: the GTK-theme Apply writes both settings.ini, sets
    // gsettings, and re-points the three links at the new theme's gtk-4.0 entries —
    // touching only the links: every theme file stays byte-identical and in place.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let themes_before = theme_root_snapshot(&fx);

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK4_THEME);
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    assert_eq!(contribution.links.len(), 3, "one step per link");
    let plan = plan_with_themes(contribution);

    let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &caps,
        &runner,
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 2, "the links are not file writes");
    assert_eq!(
        runner.recorded(),
        vec![gsettings_gtk_theme(FIXTURE_GTK4_THEME)]
    );

    assert_eq!(
        gtk4_link_targets(&fx),
        targets_into(&fx, FIXTURE_GTK4_THEME)
    );
    for name in GTK4_THEME_LINK_NAMES {
        assert!(
            fs::symlink_metadata(gtk4_link(&fx, name))
                .expect("lstat")
                .file_type()
                .is_symlink(),
            "{name} is still a link"
        );
    }
    assert_eq!(
        fs::read_to_string(gtk4_link(&fx, "gtk.css")).expect("read through the link"),
        format!("/* {FIXTURE_GTK4_THEME} gtk-4.0 */\n")
    );
    assert!(
        fx.system_theme_gtk4_dir(FIXTURE_GTK4_THEME)
            .join("assets")
            .is_dir()
    );
    assert_eq!(
        theme_root_snapshot(&fx),
        themes_before,
        "no theme file changed"
    );
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/gtk-3.0/settings.ini", "config/gtk-4.0/settings.ini"],
    );
    // The 11.12 rule still holds for the settings.ini written beside the links.
    assert!(
        fs::symlink_metadata(fx.config_path("gtk-4.0/settings.ini"))
            .expect("lstat")
            .file_type()
            .is_symlink()
    );

    // After the commit the model plans nothing more for the links.
    themes.commit();
    assert!(!themes.is_dirty());
    assert_eq!(themes.gtk4_links_note(), None);
}

#[test]
fn a_gtk_theme_without_a_gtk4_dir_removes_the_links_and_the_note_says_so() {
    let fx = FixtureDotfiles::install();
    let themes_before = theme_root_snapshot(&fx);

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK3_ONLY_THEME);
    let note = format!(
        "\u{201c}{FIXTURE_GTK3_ONLY_THEME}\u{201d} has no GTK 4 version, so GTK 4 and \
         libadwaita apps fall back to the default Adwaita look."
    );
    assert_eq!(
        themes.gtk4_links_note(),
        Some(note.clone()),
        "said before the Apply"
    );
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    assert!(
        contribution
            .links
            .iter()
            .all(|link| link.action == LinkAction::Remove)
    );
    let plan = plan_with_themes(contribution);

    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &Capabilities::for_tests(&[Binary::Gsettings], &[], false),
        &MockCommandRunner::new(),
        &MockProcessSignaller::new(),
    );
    expect_applied(outcome);
    assert_eq!(gtk4_link_targets(&fx), vec![None, None, None]);
    for name in GTK4_THEME_LINK_NAMES {
        assert!(
            fs::symlink_metadata(gtk4_link(&fx, name)).is_err(),
            "{name} removed"
        );
    }
    assert_eq!(
        theme_root_snapshot(&fx),
        themes_before,
        "removing the links never touches the old theme's files"
    );
    themes.commit();
    assert_eq!(
        themes.gtk4_links_note(),
        Some(note),
        "and still true after it"
    );
}

#[test]
fn a_plain_gtk_css_blocks_the_link_step_with_a_warning_and_the_ini_writes_still_land() {
    // A real gtk.css is a user-managed override (like GTK_THEME): none of the three
    // links is changed — mixing two themes would be worse — a `warn` says so, the page
    // notes it, and the settings.ini/gsettings legs of the change still land.
    let fx = FixtureDotfiles::install();
    let gtk_css = gtk4_link(&fx, "gtk.css");
    fs::remove_file(&gtk_css).expect("remove the link");
    fs::write(&gtk_css, b"/* my own GTK 4 tweaks */\n").expect("a plain gtk.css");

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK4_THEME);
    let mut contribution = None;
    let events = capture_logs(|| contribution = themes.apply_contribution());
    let contribution = contribution.expect("a dirty theme contributes");
    let warnings: Vec<&CapturedEvent> = events
        .iter()
        .filter(|event| {
            event.level == tracing::Level::WARN && event.message.contains("user-managed override")
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{events:?}");
    assert!(
        warnings[0].fields.contains(&gtk_css.display().to_string()),
        "{:?}",
        warnings[0]
    );
    assert!(
        contribution.links.is_empty(),
        "all three links are left alone"
    );
    assert_eq!(
        themes.gtk4_links_note().as_deref(),
        Some(
            "~/.config/gtk-4.0/gtk.css is a file of its own rather than a link to a theme, \
             so GTK 4 and libadwaita apps are left as they are: they do not follow a \
             GTK-theme change made here."
        )
    );

    let plan = plan_with_themes(contribution);
    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &Capabilities::for_tests(&[Binary::Gsettings], &[], false),
        &runner,
        &MockProcessSignaller::new(),
    );
    let (_, written) = expect_applied(outcome);
    assert_eq!(written.len(), 2, "both settings.ini files are written");
    assert_eq!(
        runner.recorded(),
        vec![gsettings_gtk_theme(FIXTURE_GTK4_THEME)]
    );
    for relative in ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"] {
        assert!(
            fs::read_to_string(fx.config_path(relative))
                .expect("read")
                .contains(&format!("gtk-theme-name={FIXTURE_GTK4_THEME}")),
            "{relative}"
        );
    }
    assert_eq!(
        fs::read(&gtk_css).expect("read"),
        b"/* my own GTK 4 tweaks */\n"
    );
    let active = targets_into(&fx, FIXTURE_ACTIVE_GTK_THEME);
    assert_eq!(
        gtk4_link_targets(&fx),
        vec![None, active[1].clone(), active[2].clone()]
    );
}

#[test]
fn a_failure_after_the_link_step_restores_the_previous_link_targets() {
    // R5.4: the link steps are part of the write transaction. A GTK-theme change and a
    // palette switch whose generator fails (it runs after the links): the links go back
    // to the old theme and the settings.ini writes are restored, all newest first.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let themes_before = theme_root_snapshot(&fx);

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK4_THEME);
    let mut palette = load_palette(&fx);
    palette.stage("nord");
    let mut plan = plan_with_themes(themes.apply_contribution().expect("dirty"));
    plan.palette = palette.apply_contribution();

    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true),
        &runner,
        &MockProcessSignaller::new(),
    );
    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            assert!(matches!(
                failure.cause,
                WriteFailureCause::GenerateColorsExit { code: Some(1), .. }
            ));
            assert_eq!(
                failure.rolled_back,
                vec![
                    gtk4_link(&fx, "assets"),
                    gtk4_link(&fx, "gtk-dark.css"),
                    gtk4_link(&fx, "gtk.css"),
                    fx.repo_path("config/gtk-4.0/settings.ini"),
                    fx.repo_path("config/gtk-3.0/settings.ini"),
                ]
            );
            assert!(failure.rollback_failures.is_empty());
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }
    assert_eq!(
        gtk4_link_targets(&fx),
        targets_into(&fx, FIXTURE_ACTIVE_GTK_THEME)
    );
    assert_eq!(theme_root_snapshot(&fx), themes_before);
    assert_repo_untouched_except(&fx, &before, &[]);
}

/// Restores a directory's mode on drop, so a test that made it read-only cleans up
/// even when an assertion fails first (the pattern of `tests/apply_input.rs`).
struct RestoreWritable<'a> {
    /// The directory whose permissions are restored on drop.
    dir: &'a Path,
}

impl Drop for RestoreWritable<'_> {
    fn drop(&mut self) {
        // Best-effort: an error here would only mask the original panic.
        let _ = fs::set_permissions(self.dir, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_failing_link_step_rolls_back_the_ini_writes_and_runs_no_reload() {
    // R5.4 the other way round: `~/.config/gtk-4.0` is read-only, so the first link
    // step cannot place its temporary link. The settings.ini writes before it (which
    // land in the repo through their links, so they succeed) are rolled back, gsettings
    // is never called, and the links are as they were.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK4_THEME);
    let plan = plan_with_themes(themes.apply_contribution().expect("dirty"));

    let links_dir = fx.config_path("gtk-4.0");
    fs::set_permissions(&links_dir, fs::Permissions::from_mode(0o555))
        .expect("make the GTK 4 config directory read-only");
    let _restore = RestoreWritable { dir: &links_dir };
    // Running as root bypasses directory modes; say so rather than pass silently.
    if fs::File::create(links_dir.join(".probe")).is_ok() {
        eprintln!(
            "skipped: directory modes are not enforced (running as root), so the \
             link-failure injection cannot work"
        );
        return;
    }

    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &Capabilities::for_tests(&[Binary::Gsettings], &[], false),
        &runner,
        &MockProcessSignaller::new(),
    );
    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            match &failure.cause {
                WriteFailureCause::Link { path, .. } => {
                    assert_eq!(path, &gtk4_link(&fx, "gtk.css"));
                }
                other => panic!("expected a Link failure, got {other:?}"),
            }
            assert_eq!(
                failure.rolled_back,
                vec![
                    fx.repo_path("config/gtk-4.0/settings.ini"),
                    fx.repo_path("config/gtk-3.0/settings.ini"),
                ]
            );
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }
    assert!(
        runner.recorded().is_empty(),
        "no reload after a failed write phase"
    );
    assert_eq!(
        gtk4_link_targets(&fx),
        targets_into(&fx, FIXTURE_ACTIVE_GTK_THEME)
    );
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn icon_and_cursor_only_applies_never_touch_the_gtk4_links() {
    // Task 11.16 item 3: the step follows a gtk-theme-name change only.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create the fixture cursor theme");
    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_cursor_theme("Fixture-Cursors");
    themes.stage_cursor_size("32");
    let contribution = themes
        .apply_contribution()
        .expect("a dirty cursor contributes");
    assert!(contribution.links.is_empty());
}

#[test]
fn a_gtk4_link_into_a_folder_of_the_users_own_is_left_alone_like_a_plain_file() {
    // A link that resolves outside every theme root (here `~/custom/gtk.css`) is a
    // user-managed override: all three links stay, a `warn` names the path, the page
    // notes it, and the INI and gsettings writes still land. (A dangling link would
    // instead be repaired — see the unit tests.)
    let fx = FixtureDotfiles::install();
    let custom = fx.home().join("custom");
    fs::create_dir_all(&custom).expect("create the user's folder");
    fs::write(custom.join("gtk.css"), b"/* mine */\n").expect("the user's css");
    let gtk_css = gtk4_link(&fx, "gtk.css");
    fs::remove_file(&gtk_css).expect("remove the theme link");
    std::os::unix::fs::symlink(custom.join("gtk.css"), &gtk_css).expect("link to custom");
    let targets_before = gtk4_link_targets(&fx);

    let mut themes = load_themes_with_system_root(&fx);
    themes.stage_gtk_theme(FIXTURE_GTK4_THEME);
    let mut contribution = None;
    let events = capture_logs(|| contribution = themes.apply_contribution());
    let contribution = contribution.expect("a dirty theme contributes");
    let warnings: Vec<&CapturedEvent> = events
        .iter()
        .filter(|event| {
            event.level == tracing::Level::WARN && event.message.contains("user-managed override")
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{events:?}");
    assert!(
        warnings[0].fields.contains(&gtk_css.display().to_string())
            && warnings[0].fields.contains("link outside the theme roots"),
        "{:?}",
        warnings[0]
    );
    assert!(
        contribution.links.is_empty(),
        "all three links are left alone"
    );
    assert_eq!(
        themes.gtk4_links_note().as_deref(),
        Some(
            "~/.config/gtk-4.0/gtk.css links to a place outside the theme folders, so GTK 4 \
             and libadwaita apps are left as they are: they do not follow a GTK-theme \
             change made here."
        )
    );

    let plan = plan_with_themes(contribution);
    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &plan,
        &FreshnessTracker::new(),
        &Capabilities::for_tests(&[Binary::Gsettings], &[], false),
        &runner,
        &MockProcessSignaller::new(),
    );
    let (_, written) = expect_applied(outcome);
    assert_eq!(written.len(), 2, "both settings.ini files are written");
    assert_eq!(
        runner.recorded(),
        vec![gsettings_gtk_theme(FIXTURE_GTK4_THEME)]
    );
    assert_eq!(gtk4_link_targets(&fx), targets_before);
    assert_eq!(
        fs::read(custom.join("gtk.css")).expect("read"),
        b"/* mine */\n"
    );
}
