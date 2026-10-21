//! End-to-end staged-edit → Apply suite for the **Theme** category (task 7.2;
//! R5.3–R5.6, R6.1).
//!
//! The Theme page is three staging sources folded into one Apply (tasks
//! 6.3–6.5), and this suite drives each against the installed fixture tree
//! (task 7.1):
//!
//! - the **palette** switch, whose source is discovered by *real* detection
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
//! Like Display, these models own their files' freshness (R5.6): the window
//! checks `check_conflict()` and aborts before building the plan, so the
//! conflict test asserts that guard.

use std::fs;

use nix::sys::signal::Signal;

use settings4000::core::apply::WriteMode;
use settings4000::core::apply::{self, ApplyOutcome, ApplyPlan, WriteFailureCause};
use settings4000::core::detect::{Binary, Capabilities, Daemon, DetectionInputs};
use settings4000::core::freshness::FreshnessTracker;
use settings4000::core::model::validate_image_value;
use settings4000::core::notifd::NotifdReplies;
use settings4000::core::reload::BackingFile;
use settings4000::core::reload::ReloadParams;
use settings4000::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use settings4000::parsers::lua_literal::{LuaConfigFile, LuaValue};
use settings4000::parsers::palette::SchemaSource;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::{MockProcessSignaller, SignalCall};
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, differing_lines, expect_applied, replace_once,
    repo_snapshot,
};

/// Builds the palette model exactly as the startup worker does (task 6.3): the
/// palette source is discovered by real detection canonicalizing the deployed
/// `colors.conf` symlink into the fixture repo (R3.2/R8.5), and the active scheme
/// is read from that generated file's header.
fn load_palette(fx: &FixtureDotfiles) -> PaletteModel {
    let anchor = fx.config_path("hypr/colors.conf");
    let inputs = DetectionInputs {
        // Only the filesystem probes matter here; binaries/daemons are irrelevant
        // to source discovery.
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: anchor.clone(),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
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
fn apply_wallpaper(fx: &FixtureDotfiles, chosen: &str) -> Vec<Command> {
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

    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
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

/// The two hyprpaper reload commands for `absolute` with the fixture's `cover` fit.
fn hyprpaper_reload(absolute: &str) -> Vec<Command> {
    let wallpaper_arg = format!(",{absolute},cover");
    vec![
        Command::new("hyprctl").args(["hyprpaper", "preload", absolute]),
        Command::new("hyprctl").args(["hyprpaper", "wallpaper", wallpaper_arg.as_str()]),
    ]
}

/// An empty plan to fold a Theme contribution into; the Theme models carry their
/// own validations and reload parameters.
fn empty_plan() -> ApplyPlan {
    ApplyPlan {
        validations: Vec::new(),
        writes: Vec::new(),
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

#[test]
fn a_palette_switch_runs_generate_colors_then_the_reload_chain() {
    // The palette happy path (R5.3, task 6.3): switching everforest -> nord runs
    // the fixture's discovered `generate-colors nord` (as the only write step —
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

    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Eww, Daemon::Kitty], true);
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

    // The exact command sequence: the discovered generator path (inside the
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
    // Install a discoverable GTK theme so the drop-down offers it, as on a real
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

    let caps = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
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

    // (b) The exact reload: one gsettings call, no cursor/icon commands.
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
    // hyprlock.conf, then reloads hyprpaper live (preload + wallpaper with the
    // fit as the third comma-field). hyprlock gets no reload — it reads its
    // config at launch (intentional).
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
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create the fixture GTK theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes(&fx);
    themes.stage_gtk_theme("Fixture-Theme");
    let mut palette = load_palette(&fx);
    palette.stage("nord");

    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;
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
            // Both already-written files are rolled back (resolved repo targets,
            // newest-first) — proof the writes preceded the generator.
            assert_eq!(
                failure.rolled_back,
                vec![
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
    // palette sources, and every generated partial — is byte-identical (R5.4).
    assert_repo_untouched_except(&fx, &before, &[]);
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
