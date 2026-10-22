//! Headless suite for the Apply-plan **assembler** (task 9.16; R5.3–R5.6, R6.2, R8.3).
//!
//! [`assemble_apply_plan`] is the front half of the window's Apply button: it asks every
//! staging source — the shared [`SettingsStore`] plus the six bespoke page models — for
//! its contribution, folds them into one [`ApplyPlan`] in a fixed order, and refuses to
//! plan the Apply at all when a source cannot be prepared. Until task 9.16 that logic sat
//! inside the button's click handler, so the only way to reach it was to click the button;
//! the decisions it makes are asserted here instead:
//!
//! - **the conflict guards run first, and only for a dirty model** (R5.6) — a
//!   model-owned file changed on disk aborts before anything is built, while the same
//!   change behind a *clean* model must not block an unrelated page's Apply;
//! - **a write that cannot be prepared aborts the whole Apply**, keeping the staged edits
//!   — never a silent skip, which would let the caller commit values against a file that
//!   was never written;
//! - **the commit snapshot is captured before the bespoke models contribute**, so the
//!   store re-baselines exactly the files whose freshness it owns;
//! - **plan composition per category**: which write, validation, palette switch and
//!   reload parameter each source adds, and which models the caller must commit.
//!
//! Everything runs against the installed fixture dotfiles tree (task 7.1) with the models
//! built through their production `load` entry points, so the paths, symlinks and file
//! formats are the real ones. The suite deliberately does **not** re-assert the per-page
//! byte-exactness the `apply_*` suites own; it asserts what the assembler decides, plus
//! one end-to-end pass that runs the assembled plan and reconciles the sources the way the
//! window does.

use std::fs;
use std::path::{Path, PathBuf};

use settings4000::core::apply::{self, ApplyOutcome, ApplyPlan};
use settings4000::core::assemble::{
    ApplySources, AssembledPlan, ConflictedSource, FailedWrite, ModelCommits, PrepareFailure,
    assemble_apply_plan,
};
use settings4000::core::detect::{Binary, Capabilities, Daemon, DetectionInputs};
use settings4000::core::display::DisplayModel;
use settings4000::core::input::InputModel;
use settings4000::core::model::{SettingId, Value};
use settings4000::core::notifd::{NotifdReplies, NotifdStatus, NotifdVersion};
use settings4000::core::power::PowerModel;
use settings4000::core::store::SettingsStore;
use settings4000::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use settings4000::parsers::generated::generated_files;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::{MockProcessSignaller, SignalCall};
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, expect_applied, load_into_store, loaders,
    replace_once, repo_snapshot,
};

/// A canned `hyprctl monitors all -j` payload for the fixture's laptop panel, matching
/// the one `tests/apply_display.rs` uses: the description matches none of the fixture's
/// `desc:` records, so the live output pairs with the generic `monitor=eDP-1,…` rule.
const HYPRCTL_MONITORS_JSON: &str = r#"[{
    "name": "eDP-1",
    "description": "Fixture Internal Panel",
    "width": 2880,
    "height": 1800,
    "refreshRate": 120.0,
    "x": 0,
    "y": 0,
    "scale": 1.333333,
    "disabled": false,
    "availableModes": ["2880x1800@120.00Hz", "1920x1200@60.00Hz"]
}]"#;

/// The store loaded with both store-backed backing files through the app's own loaders,
/// mirroring the startup wiring (task 5.4).
fn loaded_store(fx: &FixtureDotfiles) -> SettingsStore {
    let mut store = SettingsStore::new();
    load_into_store(
        &mut store,
        &fx.config_path("hypr/input.lua"),
        loaders::input_lua,
    );
    load_into_store(
        &mut store,
        &fx.config_path("hypr/hypridle.conf"),
        loaders::hypridle_conf,
    );
    store
}

/// The Input page's write helper. The XKB registry path is deliberately nonexistent: the
/// layout candidates play no part in assembly, and the model degrades to none (R4.4).
fn input_model(fx: &FixtureDotfiles) -> InputModel {
    InputModel::load(
        fx.config_path("hypr/input.lua"),
        Path::new("/nonexistent/evdev.xml"),
    )
    .expect("the fixture input.lua backs the Input page")
}

/// The Display model, built through its production probe-and-read entry point.
fn display_model(fx: &FixtureDotfiles) -> DisplayModel {
    let probe = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
        0,
        HYPRCTL_MONITORS_JSON,
        "",
    ))]);
    DisplayModel::load(&probe, fx.config_path("hypr/monitors.conf"), None)
        .expect("a successful probe yields a model")
}

/// The palette model, its source found by real detection canonicalizing the deployed
/// `colors.conf` symlink into the fixture repo (R3.2/R8.5) — the same route the startup
/// worker takes.
fn palette_model(fx: &FixtureDotfiles) -> PaletteModel {
    let anchor = fx.config_path("hypr/colors.conf");
    let inputs = DetectionInputs {
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
    let source = Capabilities::detect(&inputs);
    let source = source
        .palette_source()
        .expect("the deployed symlink reveals the palette source");
    PaletteModel::load(
        source.colors_dir(),
        &anchor,
        source.generate_colors().to_path_buf(),
    )
}

/// The GTK/icon/cursor model, with the theme roots pointed at the fixture home (a test
/// creates the theme directory it stages) and no settings portal or `GTK_THEME` override.
fn themes_model(fx: &FixtureDotfiles) -> ThemesModel {
    let roots = ThemeRoots {
        gtk_theme_dirs: vec![fx.home().join(".themes")],
        icon_dirs: vec![fx.home().join(".icons")],
    };
    let paths = ThemesPaths {
        gtk3_settings: fx.config_path("gtk-3.0/settings.ini"),
        gtk4_settings: fx.config_path("gtk-4.0/settings.ini"),
        hyprland_lua: Some(fx.config_path("hypr/hyprland.lua")),
        uwsm_env: fx.config_path("uwsm/env"),
        cursor_fallback_theme: Some(fx.home().join(".icons/default/index.theme")),
        dotfiles_repo: Some(fx.repo_root().to_path_buf()),
        home: Some(fx.home().to_path_buf()),
    };
    ThemesModel::load(&roots, paths, false, None)
}

/// The wallpaper / lock-background model, with hyprlock present so the dual write is in
/// play.
fn wallpaper_model(fx: &FixtureDotfiles) -> WallpaperModel {
    WallpaperModel::load(
        WallpaperPaths {
            hyprpaper_conf: fx.config_path("hypr/hyprpaper.conf"),
            hyprlock_conf: fx.config_path("hypr/hyprlock.conf"),
            home: Some(fx.home().to_path_buf()),
        },
        true,
    )
}

/// The paths of a plan's writes, in plan order — the assembly assertion most tests make.
fn write_paths(plan: &ApplyPlan) -> Vec<PathBuf> {
    plan.writes.iter().map(|write| write.path.clone()).collect()
}

/// Asserts the failure is a [`PrepareFailure::Write`] from `source`, returning its
/// message.
#[track_caller]
fn expect_write_failure(failure: PrepareFailure, source: FailedWrite) -> String {
    match failure {
        PrepareFailure::Write {
            source: actual,
            message,
        } => {
            assert_eq!(actual, source, "the failure must name the failing page");
            message
        }
        other => panic!("expected a write-preparation failure, got {other:?}"),
    }
}

#[test]
fn a_clean_desktop_assembles_an_empty_plan() {
    // The overwhelmingly common case: Apply is not even clickable with nothing dirty, but
    // the assembler must still answer with an empty plan rather than, say, a write
    // rendered from no edits.
    let fx = FixtureDotfiles::install();
    let store = loaded_store(&fx);
    let input = input_model(&fx);
    let display = display_model(&fx);
    let themes = themes_model(&fx);
    let wallpaper = wallpaper_model(&fx);
    let palette = palette_model(&fx);

    let assembled = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        power: Some(&PowerModel::load(fx.config_path("hypr/hypridle.conf"))),
        palette: Some(&palette),
        themes: Some(&themes),
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect("a clean desktop can always be planned");

    assert!(assembled.plan.writes.is_empty(), "nothing to write");
    assert!(assembled.plan.validations.is_empty(), "nothing to validate");
    assert!(assembled.plan.palette.is_none(), "no scheme switch");
    assert!(assembled.store_writes.is_empty(), "nothing to commit");
    assert_eq!(
        assembled.commits,
        ModelCommits::default(),
        "no model contributed, so none may be committed"
    );
}

#[test]
fn the_store_backed_pages_fold_one_write_each() {
    // Plan composition, store side (tasks 6.6–6.8): one surgical write per store-backed
    // page, in a fixed order, with the store's dirty values carried as the pipeline's
    // R8.3 validations.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    store
        .stage(SettingId::LockTimeout, Value::Integer(600))
        .expect("a lock-timeout change stages");

    let input = input_model(&fx);
    let power = PowerModel::load(fx.config_path("hypr/hypridle.conf"));
    let assembled = assemble_apply_plan(ApplySources {
        input: Some(&input),
        power: Some(&power),
        ..ApplySources::for_store(&store)
    })
    .expect("two readable files render two writes");

    assert_eq!(
        write_paths(&assembled.plan),
        vec![
            fx.config_path("hypr/input.lua"),
            fx.config_path("hypr/hypridle.conf"),
        ],
        "one write per page, Input then Power & Idle"
    );
    let validated: Vec<SettingId> = assembled
        .plan
        .validations
        .iter()
        .map(|(id, _)| *id)
        .collect();
    for id in [SettingId::TouchpadNaturalScroll, SettingId::LockTimeout] {
        assert!(
            validated.contains(&id),
            "every dirty store value must reach the R8.3 gate: {id:?} missing"
        );
    }
    assert_eq!(
        assembled.store_writes.len(),
        2,
        "both files are store-tracked, so both are committed"
    );
    assert_eq!(
        assembled.commits,
        ModelCommits::default(),
        "no bespoke model contributed"
    );
}

#[test]
fn the_commit_snapshot_covers_the_stores_own_files_only() {
    // The load-bearing capture point: `store_writes` is taken *before* the bespoke models
    // fold their writes in, because `SettingsStore::commit_apply` re-baselines every file
    // it is handed. Listing a model-owned file here would have the store re-baseline a
    // file it never loaded — and the model would re-baseline it too, from its own commit.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");

    let assembled = assemble_apply_plan(ApplySources {
        input: Some(&input),
        display: Some(&display),
        ..ApplySources::for_store(&store)
    })
    .expect("both writes render");

    assert_eq!(
        write_paths(&assembled.plan),
        vec![
            fx.config_path("hypr/input.lua"),
            fx.config_path("hypr/monitors.conf"),
        ],
        "the plan writes both files"
    );
    assert_eq!(
        assembled
            .store_writes
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>(),
        vec![fx.config_path("hypr/input.lua")],
        "only the store-tracked file is handed to commit_apply"
    );
    assert!(
        assembled.commits.display,
        "the Display model contributed, so it commits itself"
    );
}

#[test]
fn the_bespoke_models_compose_into_one_plan() {
    // Plan composition, model side (tasks 6.1, 6.3–6.5): four sources contribute in one
    // Apply — the Display write plus its validations, the palette switch, the theme
    // writes with their reload parameters, and the wallpaper writes with theirs — and the
    // two reload-parameter sets merge instead of overwriting each other.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create a GTK theme the scan finds");
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");

    let store = loaded_store(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");
    let mut palette = palette_model(&fx);
    palette.stage("nord");
    let mut themes = themes_model(&fx);
    themes
        .stage_gtk_theme("Fixture-Theme")
        .expect("the value passes the stage-time check");
    let mut wallpaper = wallpaper_model(&fx);
    wallpaper
        .stage_wallpaper(new_wallpaper.to_str().expect("UTF-8 path"))
        .expect("an existing image stages");

    let assembled = assemble_apply_plan(ApplySources {
        display: Some(&display),
        palette: Some(&palette),
        themes: Some(&themes),
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect("every contribution renders");

    let paths = write_paths(&assembled.plan);
    assert_eq!(
        paths.first(),
        Some(&fx.config_path("hypr/monitors.conf")),
        "the Display write is folded first among the models"
    );
    for expected in [
        "gtk-3.0/settings.ini",
        "gtk-4.0/settings.ini",
        "hypr/hyprpaper.conf",
        "hypr/hyprlock.conf",
    ] {
        assert!(
            paths.contains(&fx.config_path(expected)),
            "{expected} must be in the plan; got {paths:?}"
        );
    }
    assert!(
        assembled.store_writes.is_empty(),
        "no store-backed page is dirty, so nothing goes to commit_apply"
    );
    assert_eq!(
        assembled.plan.palette.as_ref().map(|s| s.scheme.as_str()),
        Some("nord"),
        "the staged scheme becomes the plan's last write step"
    );
    // Both Theme sub-features fill different fields of the one ReloadParams; a merge that
    // simply assigned would drop whichever contribution folded first.
    assert_eq!(
        assembled.plan.reload_params.gtk_theme.as_deref(),
        Some("Fixture-Theme"),
        "the GTK theme reload parameter survives the wallpaper contribution"
    );
    assert!(
        assembled.plan.reload_params.wallpaper.is_some(),
        "the wallpaper reload parameter is present"
    );
    // The Display monitor values and the chosen image path are both re-checked by the
    // pipeline's R8.3 gate.
    assert!(
        assembled
            .plan
            .validations
            .iter()
            .any(|(id, _)| matches!(id, SettingId::MonitorScale)),
        "the Display contribution carries its validations"
    );
    assert!(
        assembled
            .plan
            .validations
            .iter()
            .any(|(id, _)| matches!(id, SettingId::WallpaperPath)),
        "the wallpaper contribution carries its validations"
    );
    assert!(
        assembled
            .plan
            .validations
            .iter()
            .any(|(id, _)| matches!(id, SettingId::GtkTheme)),
        "the GTK/icon/cursor contribution carries its validations (task 9.27)"
    );
    assert_eq!(
        assembled.commits,
        ModelCommits {
            display: true,
            palette: true,
            themes: true,
            wallpaper: true,
        },
        "all four models contributed, so all four must be committed on success"
    );
}

#[test]
fn no_assembled_write_ever_targets_a_generated_file() {
    // Generated files are read-only inputs (R3.2, architecture §3): only
    // `generate-colors` changes them. The likeliest accident is the Lua palette
    // partial `colors.lua` (task 11.6), which sits beside the cursor setting's
    // `hyprland.lua` target. Stage every write-producing control at once — an Input
    // edit, a monitor scale, a palette switch, GTK/icon/cursor themes and a
    // wallpaper — and prove no planned write resolves to any generator output, and
    // that applying the plan (generator mocked) leaves every one byte-identical.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create a GTK theme the scan finds");
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create a cursor theme the scan finds");
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let before = repo_snapshot(&fx);

    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");
    let mut palette = palette_model(&fx);
    palette.stage("nord");
    let mut themes = themes_model(&fx);
    themes
        .stage_gtk_theme("Fixture-Theme")
        .expect("the value passes the stage-time check");
    themes
        .stage_cursor_theme("Fixture-Cursors")
        .expect("the value passes the stage-time check");
    themes
        .stage_cursor_size("32")
        .expect("the value passes the stage-time check");
    let mut wallpaper = wallpaper_model(&fx);
    wallpaper
        .stage_wallpaper(new_wallpaper.to_str().expect("UTF-8 path"))
        .expect("an existing image stages");

    let assembled = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        palette: Some(&palette),
        themes: Some(&themes),
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect("every contribution renders");
    assert!(
        write_paths(&assembled.plan).contains(&fx.config_path("hypr/hyprland.lua")),
        "the cursor write to colors.lua's neighbour is in the plan"
    );
    assert!(
        assembled.plan.palette.is_some(),
        "the palette switch is planned"
    );

    // Compare resolved targets, so a write reaching a generated file through any
    // symlink would be caught too.
    let generated: Vec<PathBuf> = generated_files()
        .map(|relative| {
            fs::canonicalize(fx.repo_path(relative))
                .unwrap_or_else(|e| panic!("{relative} should exist in the fixture: {e}"))
        })
        .collect();
    for write in &assembled.plan.writes {
        let target = fs::canonicalize(&write.path).unwrap_or_else(|_| write.path.clone());
        assert!(
            !generated.contains(&target),
            "{} is a generated file and must never be a write target",
            write.path.display()
        );
    }

    let caps = Capabilities::for_tests(
        &[Binary::Hyprctl, Binary::Gsettings],
        &[Daemon::Eww, Daemon::Kitty],
        true,
    );
    let outcome = apply::run(
        &assembled.plan,
        store.freshness(),
        &caps,
        &MockCommandRunner::new(),
        &MockProcessSignaller::new(),
    );
    // Reload replies from the bare mock may be refused; only the writes matter here.
    let (_, written) = expect_applied(outcome);
    assert!(!written.is_empty(), "the plan's writes were made");
    for relative in generated_files() {
        assert_eq!(
            fs::read(fx.repo_path(relative)).expect("read a generated file"),
            before[relative],
            "{relative} must be byte-identical after an app Apply"
        );
    }
}

#[test]
fn a_dirty_models_externally_changed_file_aborts_the_apply() {
    // R5.6, the Display flavour: `monitors.conf` freshness is the model's own, so the
    // assembler asks the model before building anything. An external edit after load
    // aborts with the source named — the window reloads that model and asks the user to
    // re-apply — and every staged edit is still pending.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = fx.config_path("hypr/monitors.conf");

    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");

    fs::write(
        &path,
        format!(
            "{}# edited by hand while the app was open\n",
            fs::read_to_string(&path).expect("read the fixture")
        ),
    )
    .expect("apply the external edit");

    let failure = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a stale model-owned file must abort the Apply");

    assert_eq!(failure, PrepareFailure::Conflict(ConflictedSource::Display));
    assert!(display.is_dirty(), "the staged monitor edit is retained");
    assert!(store.is_dirty(), "the staged Input edit is retained");
    assert_repo_untouched_except(&fx, &before, &["config/hypr/monitors.conf"]);
}

#[test]
fn a_stale_theme_or_wallpaper_file_aborts_naming_its_model() {
    // The same guard for the two Theme models, whose files are likewise outside the
    // store's tracker. Both are asserted here because the assembler checks them in a
    // fixed order and each maps to a different recovery in the window (which model to
    // reload); a copy-paste that reported the wrong one would be invisible otherwise.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".themes/Fixture-Theme/gtk-3.0"))
        .expect("create a GTK theme the scan finds");
    let store = loaded_store(&fx);

    let mut themes = themes_model(&fx);
    themes
        .stage_gtk_theme("Fixture-Theme")
        .expect("the value passes the stage-time check");
    let gtk3 = fx.config_path("gtk-3.0/settings.ini");
    fs::write(
        &gtk3,
        format!(
            "{}# edited by hand\n",
            fs::read_to_string(&gtk3).expect("read the fixture")
        ),
    )
    .expect("apply the external edit");

    let failure = assemble_apply_plan(ApplySources {
        themes: Some(&themes),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a stale settings.ini must abort the Apply");
    assert_eq!(failure, PrepareFailure::Conflict(ConflictedSource::Themes));
    assert!(themes.is_dirty(), "the staged theme is retained");

    // The wallpaper model, independently: a fresh tree, an image staged, then its
    // hyprpaper.conf changed behind it.
    let fx = FixtureDotfiles::install();
    let store = loaded_store(&fx);
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let mut wallpaper = wallpaper_model(&fx);
    wallpaper
        .stage_wallpaper(new_wallpaper.to_str().expect("UTF-8 path"))
        .expect("an existing image stages");
    let hyprpaper = fx.config_path("hypr/hyprpaper.conf");
    fs::write(
        &hyprpaper,
        format!(
            "{}# edited by hand\n",
            fs::read_to_string(&hyprpaper).expect("read the fixture")
        ),
    )
    .expect("apply the external edit");

    let failure = assemble_apply_plan(ApplySources {
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a stale hyprpaper.conf must abort the Apply");
    assert_eq!(
        failure,
        PrepareFailure::Conflict(ConflictedSource::Wallpaper)
    );
    assert!(wallpaper.is_dirty(), "the staged image path is retained");
}

#[test]
fn a_changed_file_behind_a_clean_model_never_blocks_the_apply() {
    // The `is_dirty()` half of the guard. A model with nothing staged writes nothing, so
    // an external edit to its file cannot be clobbered — blocking on it would leave the
    // user unable to apply *any* page until they fixed a file they never touched. (The
    // pipeline reports such untouched-but-changed store files non-blockingly, task 9.11;
    // this is the model-owned counterpart.)
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let display = display_model(&fx);

    let path = fx.config_path("hypr/monitors.conf");
    fs::write(
        &path,
        format!(
            "{}# edited by hand while the app was open\n",
            fs::read_to_string(&path).expect("read the fixture")
        ),
    )
    .expect("apply the external edit");
    assert!(
        display.check_conflict(),
        "the file did change; only the model being clean makes it harmless"
    );

    let assembled = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect("a clean model's stale file must not block another page's Apply");
    assert_eq!(
        write_paths(&assembled.plan),
        vec![fx.config_path("hypr/input.lua")],
        "the Input write is planned and no monitors.conf write is"
    );
}

#[test]
fn the_conflict_guards_run_before_any_write_is_prepared() {
    // Guard *ordering*: with a stale Display model AND an unreadable input.lua, both
    // failure paths are live. The conflict must win, because it is the one with a
    // recovery: the window reloads the model and the user re-applies. Preparing writes
    // first would report the unreadable file and leave the stale model in place, so the
    // next Apply would report the conflict anyway — two dialogs for one problem.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");

    let monitors = fx.config_path("hypr/monitors.conf");
    fs::write(
        &monitors,
        format!(
            "{}# edited by hand\n",
            fs::read_to_string(&monitors).expect("read the fixture")
        ),
    )
    .expect("apply the external edit");
    // The write the assembler would otherwise prepare first now cannot be rendered at
    // all: the file (and the repo target behind its symlink) is gone.
    fs::remove_file(fx.repo_path("config/hypr/input.lua")).expect("remove the repo target");
    fs::remove_file(fx.config_path("hypr/input.lua")).expect("remove the deployed symlink");

    let failure = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect_err("both a conflict and an unpreparable write are pending");
    assert_eq!(
        failure,
        PrepareFailure::Conflict(ConflictedSource::Display),
        "the conflict guards run before any write is rendered"
    );
}

#[test]
fn an_unreadable_input_lua_aborts_the_whole_apply() {
    // The abort-not-skip contract (task 6.6): with Input edits pending but its file
    // unreadable, the assembler refuses to plan at all. Skipping the write would let the
    // Apply succeed for the *other* dirty page and then commit the Input values against
    // an untouched file — the store and disk would disagree from then on, silently.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    store
        .stage(SettingId::LockTimeout, Value::Integer(600))
        .expect("a lock-timeout change stages");
    let input = input_model(&fx);
    let power = PowerModel::load(fx.config_path("hypr/hypridle.conf"));

    fs::remove_file(fx.repo_path("config/hypr/input.lua")).expect("remove the repo target");
    fs::remove_file(fx.config_path("hypr/input.lua")).expect("remove the deployed symlink");

    let failure = assemble_apply_plan(ApplySources {
        input: Some(&input),
        power: Some(&power),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a pending Input edit with no readable input.lua must abort");
    let message = expect_write_failure(failure, FailedWrite::Input);
    assert!(
        !message.is_empty(),
        "the message is quoted in the dialog, so it must say something"
    );
    assert!(
        store.is_dirty(),
        "both pages' staged edits are kept for a retry"
    );
}

#[test]
fn an_unreadable_hypridle_conf_aborts_the_whole_apply() {
    // The same contract for the Power & Idle page (task 6.8).
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::LockTimeout, Value::Integer(600))
        .expect("a lock-timeout change stages");
    let power = PowerModel::load(fx.config_path("hypr/hypridle.conf"));

    fs::remove_file(fx.repo_path("config/hypr/hypridle.conf")).expect("remove the repo target");
    fs::remove_file(fx.config_path("hypr/hypridle.conf")).expect("remove the deployed symlink");

    let failure = assemble_apply_plan(ApplySources {
        power: Some(&power),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a pending Power & Idle edit with no readable file must abort");
    expect_write_failure(failure, FailedWrite::Power);
    assert!(store.is_dirty(), "the staged edit is kept for a retry");
}

#[test]
fn orphaned_power_edits_abort_the_whole_apply() {
    // The generic orphaned-edits branch of the store-backed fold, through a page other
    // than Input: a Refresh dropped the Power & Idle helper (hypridle uninstalled, say)
    // while a lock-timeout edit was still staged. There is no write for it to land in,
    // so the Apply aborts naming the page instead of committing an unwritten value.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::LockTimeout, Value::Integer(600))
        .expect("a lock-timeout change stages");

    let failure = assemble_apply_plan(ApplySources::for_store(&store))
        .expect_err("orphaned Power & Idle edits must abort");
    let message = expect_write_failure(failure, FailedWrite::Power);
    assert!(
        message.contains("Reset"),
        "the message says how to get out: {message}"
    );
    assert!(store.is_dirty(), "the staged edit is kept");
}

#[test]
fn orphaned_notifd_edits_abort_the_whole_apply() {
    // Task 11.15, the same branch for the Notifications page: the store tracks
    // notifd/config and holds a staged timeout, but a Refresh found no notifd >= 1.1 (so
    // no helper), leaving the edit with no write to land in.
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    load_into_store(
        &mut store,
        &fx.config_path("notifd/config"),
        loaders::notifd_config,
    );
    store
        .stage(SettingId::NotifdTimeoutLow, Value::Integer(2500))
        .expect("an in-range timeout stages");

    let failure = assemble_apply_plan(ApplySources::for_store(&store))
        .expect_err("orphaned Notifications edits must abort");
    expect_write_failure(failure, FailedWrite::Notifications);
    assert!(store.is_dirty(), "the staged edit is kept");
}

#[test]
fn an_unrenderable_monitor_record_aborts_the_whole_apply() {
    // The Display flavour of the same contract (task 9.5): the file is perfectly
    // readable, but the staged monitor edit is one the writer refuses — a scale carrying
    // a comma, which would insert a `bitdepth` key without a value into the record and
    // break the frozen grammar (task 11.9). Every real `stage_*` call refuses that at
    // stage time, so the state is crafted through the test-only seam to reach the
    // writer's last line of defence. The abort must come before the pipeline runs — and
    // in particular before the Input write that was already folded into the plan reaches
    // disk, which is what "abort the whole Apply" means.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);

    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display.stage_scale_unchecked_for_tests(0, "1.25,bitdepth");

    let failure = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect_err("an unrenderable record must abort the Apply, not look like a no-op");
    let message = expect_write_failure(failure, FailedWrite::Display);
    assert!(
        message.contains("monitors.conf"),
        "the message must name the file the user has to fix: {message}"
    );

    assert!(display.is_dirty(), "the staged monitor edit is retained");
    assert!(store.is_dirty(), "the staged Input edit is retained");
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn an_assembled_plan_applies_then_reconciles_every_source() {
    // The whole seam end to end: assemble one plan carrying a store-backed write and a
    // model write, run it, then reconcile the way the window does — the store from
    // `store_writes`, each model whose flag `commits` set. Afterwards nothing is dirty and
    // a second assembly is empty, which is what proves the returned snapshot and flags are
    // the ones the commit step needs (a snapshot missing the store's file would leave it
    // dirty; a flag never set would leave the model dirty).
    let fx = FixtureDotfiles::install();
    let mut store = loaded_store(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");
    let input = input_model(&fx);
    let mut display = display_model(&fx);
    display
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");

    let AssembledPlan {
        plan,
        store_writes,
        commits,
        skipped,
    } = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect("both writes render");
    assert!(skipped.is_empty(), "neither page model refused a value");

    // Hyprland alone is reloaded for these two files (task 4.4); the runner is mocked so
    // nothing pokes a live compositor.
    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty(), "a clean apply has no failures");
    assert_eq!(
        written,
        vec![
            fx.config_path("hypr/input.lua"),
            fx.config_path("hypr/monitors.conf"),
        ],
        "both files were written, at their live paths"
    );
    // Task 11.11 (architecture §6): input.lua alone would take `hyprctl reload
    // config-only`, monitors.conf the full `hyprctl reload`. With both written in one
    // Apply, Hyprland is reloaded exactly once, with the full form, which re-applies
    // everything the lighter form would.
    assert_eq!(
        runner.recorded(),
        vec![Command::new("hyprctl").arg("reload")],
        "an input + monitors Apply runs the full reload once and nothing else"
    );

    store.commit_apply(&store_writes);
    assert!(commits.display, "the Display model contributed");
    display.commit();

    assert!(!store.is_dirty(), "the store promoted its staged edits");
    assert!(!display.is_dirty(), "the model promoted its staged edits");

    // A second Apply has nothing to do — and, crucially, does not report the app's own
    // writes as an external conflict (R5.6): both the store and the model re-baselined
    // the files they wrote.
    let assembled = assemble_apply_plan(ApplySources {
        display: Some(&display),
        input: Some(&input),
        ..ApplySources::for_store(&store)
    })
    .expect("the second assembly sees no conflict from the app's own writes");
    assert!(
        assembled.plan.writes.is_empty(),
        "nothing is dirty, so nothing is planned"
    );
    // The pipeline agrees: an empty plan applies cleanly with no writes at all.
    let outcome = apply::run(
        &assembled.plan,
        store.freshness(),
        &caps,
        &MockCommandRunner::new(),
        &MockProcessSignaller::new(),
    );
    assert!(matches!(
        outcome,
        ApplyOutcome::Applied { ref written, .. } if written.is_empty()
    ));
}

#[test]
fn an_assembled_theme_apply_orders_its_follow_up_reloads() {
    // Task 11.14 end to end (architecture §6): one assembled Apply switches the
    // palette, the icon theme and the wallpaper, with every component running. The
    // reloads come out in the canonical order — the apply-theme chain around the
    // single hyprpaper request, the bar text tone after `eww reload` (which
    // re-initialises the bar's `bar_tone` poll to its `:initial` value) and notifd's
    // `Reload()` once, last.
    let fx = FixtureDotfiles::install();
    let icons = fx.home().join(".icons/Fixture-Icons");
    fs::create_dir_all(icons.join("48x48")).expect("create an icon theme the scan finds");
    fs::write(icons.join("index.theme"), b"[Icon Theme]\n").expect("write its index");
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper = new_wallpaper.to_str().expect("UTF-8 path").to_string();

    let store = loaded_store(&fx);
    let mut palette = palette_model(&fx);
    palette.stage("nord");
    let mut themes = themes_model(&fx);
    themes
        .stage_icon_theme("Fixture-Icons")
        .expect("the value passes the stage-time check");
    let mut wallpaper = wallpaper_model(&fx);
    wallpaper
        .stage_wallpaper(&new_wallpaper)
        .expect("an existing image stages");

    let assembled = assemble_apply_plan(ApplySources {
        palette: Some(&palette),
        themes: Some(&themes),
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect("every contribution renders");

    let script = fx.config_path("eww/scripts/bar-text-tone");
    let caps = Capabilities::for_tests(
        &[Binary::Hyprctl, Binary::Gsettings],
        &[Daemon::Eww, Daemon::Kitty, Daemon::Hyprpaper],
        true,
    )
    .with_notifd_for_tests(NotifdStatus::Running {
        version: NotifdVersion::parse("1.1"),
    })
    .with_bar_text_tone_script_for_tests(script.clone());
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![77])]);
    let outcome = apply::run(
        &assembled.plan,
        store.freshness(),
        &caps,
        &runner,
        &signaller,
    );
    let (reload_failures, _) = expect_applied(outcome);
    assert!(reload_failures.is_empty(), "every reload succeeded");

    let wallpaper_arg = format!(",{new_wallpaper}");
    assert_eq!(
        runner.recorded(),
        vec![
            // The palette generator is the last write step, before every reload.
            Command::new(fx.repo_path("scripts/generate-colors").to_string_lossy()).arg("nord"),
            Command::new("hyprctl").args(["reload", "config-only"]),
            Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "icon-theme",
                "Fixture-Icons",
            ]),
            Command::new("hyprctl").args(["hyprpaper", "wallpaper", wallpaper_arg.as_str()]),
            Command::new("eww").arg("reload"),
            Command::new(script.to_str().expect("UTF-8 path")).arg("--apply"),
            // kitty's SIGUSR1 goes through the signaller, between these two.
            Command::new("busctl").args([
                "--user",
                "--auto-start=no",
                "call",
                "se.dotfiles.Notifd",
                "/se/dotfiles/Notifd",
                "se.dotfiles.Notifd",
                "Reload",
            ]),
        ]
    );
    assert_eq!(
        signaller.calls(),
        vec![SignalCall {
            process_name: "kitty".to_string(),
            signal: nix::sys::signal::Signal::SIGUSR1,
            pids: vec![77],
        }]
    );
}

/// Makes the fixture's `hyprland.lua` structurally unsafe for the Lua editor — a stray
/// closing brace at the end, as a half-finished hand edit leaves it — and returns the
/// bytes it now holds, so a test can prove the app left them alone.
fn break_hyprland_lua(fx: &FixtureDotfiles) -> String {
    let path = fx.config_path("hypr/hyprland.lua");
    let mut text = fs::read_to_string(&path).expect("read the fixture hyprland.lua");
    text.push_str("}\n");
    fs::write(&path, &text).expect("write the broken hyprland.lua");
    text
}

#[test]
fn a_refused_hyprland_lua_copy_is_reported_while_the_rest_applies() {
    // Task 9.28 (R3.4, R5.5): a hyprland.lua the Lua editor cannot follow refuses the
    // cursor theme, while the other four copies take it. The Apply still writes those
    // four and pushes the theme live — the push follows settings.ini, which took it —
    // but the assembled plan names the refused value, the file and the editor's reason,
    // so the UI can say so; and the page keeps the change pending with a note.
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create a cursor theme the scan finds");
    let lua_before = break_hyprland_lua(&fx);

    let store = loaded_store(&fx);
    let mut themes = themes_model(&fx);
    themes
        .stage_cursor_theme("Fixture-Cursors")
        .expect("the value passes the stage-time check");

    let assembled = assemble_apply_plan(ApplySources {
        themes: Some(&themes),
        ..ApplySources::for_store(&store)
    })
    .expect("the refusal does not stop the Apply");
    let skipped: Vec<String> = assembled.skipped.iter().map(ToString::to_string).collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(
        skipped[0].starts_with(
            "Cursor theme could not be written to hyprland.lua: the file's structure cannot \
             be followed (line "
        ),
        "the warning names the value, the file and the reason: {}",
        skipped[0]
    );
    assert!(!assembled.plan.changes_nothing());
    assert!(assembled.commits.themes, "the other copies were written");
    assert!(
        !write_paths(&assembled.plan).contains(&fx.config_path("hypr/hyprland.lua")),
        "nothing is planned for the refused file"
    );

    let caps = Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true);
    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &assembled.plan,
        store.freshness(),
        &caps,
        &runner,
        &MockProcessSignaller::new(),
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(
        written.len(),
        4,
        "both settings.ini, uwsm/env and index.theme"
    );
    assert_eq!(
        runner.recorded(),
        vec![
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
                "24",
            ]),
            Command::new("hyprctl").args(["setcursor", "Fixture-Cursors", "24"]),
        ],
        "settings.ini took the theme, so it is pushed live; hyprland.lua was not \
         written, so Hyprland's config is not reloaded"
    );
    assert_eq!(
        fs::read_to_string(fx.config_path("hypr/hyprland.lua")).expect("read hyprland.lua"),
        lua_before,
        "the refused file is byte-identical"
    );

    themes.commit();
    assert!(themes.is_dirty(), "the refused change stays pending");
    let note = themes
        .skipped_writes_note()
        .expect("the page explains why it stays dirty");
    assert!(note.contains(&skipped[0]), "{note}");
}

#[test]
fn an_apply_whose_every_value_is_refused_reports_it_instead_of_nothing() {
    // Task 9.28: a lock-image change against a hyprlock.conf whose `background { }`
    // block was removed by hand is the only staged value, and the file refuses it. The
    // assembled plan therefore changes nothing — the pipeline reports `Applied` with no
    // write and runs no command — and the skip list is the only thing that says why.
    // The UI turns exactly this combination into a warning (see `ui::chrome`).
    let fx = FixtureDotfiles::install();
    let hyprlock = fx.config_path("hypr/hyprlock.conf");
    let original = fs::read_to_string(&hyprlock).expect("read the fixture hyprlock.conf");
    fs::write(
        &hyprlock,
        replace_once(&original, "background {", "shape {"),
    )
    .expect("write a hyprlock.conf without a background block");
    let lock_image = fx.home().join("Pictures/lock.png");
    fs::write(&lock_image, b"fixture lock image").expect("create the lock image");

    let store = loaded_store(&fx);
    let mut wallpaper = wallpaper_model(&fx);
    wallpaper.set_override(true);
    wallpaper
        .stage_lock(lock_image.to_str().expect("UTF-8 path"))
        .expect("an existing image stages");

    let assembled = assemble_apply_plan(ApplySources {
        wallpaper: Some(&wallpaper),
        ..ApplySources::for_store(&store)
    })
    .expect("the refusal does not stop the Apply");
    assert!(assembled.plan.changes_nothing(), "nothing can be written");
    assert!(
        !assembled.commits.wallpaper,
        "a model that wrote nothing is not committed"
    );
    let skipped: Vec<String> = assembled.skipped.iter().map(ToString::to_string).collect();
    assert_eq!(
        skipped,
        vec![
            "Lock-screen background could not be written to hyprlock.conf: no section for \
             `background.path` to edit or append into"
                .to_string()
        ]
    );

    let runner = MockCommandRunner::new();
    let outcome = apply::run(
        &assembled.plan,
        store.freshness(),
        &Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true),
        &runner,
        &MockProcessSignaller::new(),
    );
    let (_, written) = expect_applied(outcome);
    assert!(written.is_empty());
    assert!(runner.recorded().is_empty(), "nothing ran");
    let note = wallpaper
        .skipped_writes_note()
        .expect("the page explains why the change stays pending");
    assert!(note.contains(&skipped[0]), "{note}");
}
