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
//!   `settings.ini` files; for the cursor also `hyprland.conf` + `uwsm/env`)
//!   must all receive the identical value (analysis §6.2, R3.4);
//! - the **wallpaper / lock background** dual write (`hyprpaper.conf` +
//!   `hyprlock.conf`, same image), including the real machine's tilde-shaped
//!   original path (`~/Pictures/…`), which the fixture's anonymized absolute
//!   paths would otherwise never exercise.
//!
//! Like Display, these models own their files' freshness (R5.6): the window
//! checks `check_conflict()` and aborts before building the plan, so the
//! conflict test asserts that guard.

use std::fs;

use nix::sys::signal::Signal;

use settings4000::core::apply::{self, ApplyOutcome, ApplyPlan, WriteFailureCause};
use settings4000::core::detect::{Binary, Capabilities, Daemon, DetectionInputs};
use settings4000::core::freshness::FreshnessTracker;
use settings4000::core::reload::ReloadParams;
use settings4000::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::{MockProcessSignaller, SignalCall};
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, expect_applied, replace_once, repo_snapshot,
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
    };
    let source_caps = Capabilities::detect(&inputs);
    let source = source_caps
        .palette_source()
        .expect("the deployed symlink reveals the palette source");
    PaletteModel::load(
        source.colors_dir(),
        &anchor,
        source.generate_colors().to_path_buf(),
    )
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
    let paths = ThemesPaths {
        gtk3_settings: fx.config_path("gtk-3.0/settings.ini"),
        gtk4_settings: fx.config_path("gtk-4.0/settings.ini"),
        hyprland_conf: fx.config_path("hypr/hyprland.conf"),
        uwsm_env: fx.config_path("uwsm/env"),
    };
    ThemesModel::load(&roots, paths, false, None)
}

/// Builds the wallpaper model exactly as the startup worker does (task 6.5), with
/// hyprlock present so the lock-background dual write is in play.
fn load_wallpaper(fx: &FixtureDotfiles) -> WallpaperModel {
    WallpaperModel::load(
        WallpaperPaths {
            hyprpaper_conf: fx.config_path("hypr/hyprpaper.conf"),
            hyprlock_conf: fx.config_path("hypr/hyprlock.conf"),
        },
        true,
    )
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

    let caps = Capabilities::for_tests(
        &[Binary::Hyprctl],
        &[Daemon::Eww, Daemon::Swaync, Daemon::Kitty],
        true,
    );
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
    // fixture repo, no shell) with the scheme argument, then the reload chain.
    let generate_colors = fx.repo_path("scripts/generate-colors");
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new(generate_colors.to_string_lossy()).arg("nord"),
            Command::new("hyprctl").arg("reload"),
            Command::new("eww").arg("reload"),
            Command::new("swaync-client").arg("-rs"),
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
    // touches the palette sources or the generated partials (R3.2).
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn a_gtk_theme_change_writes_both_settings_ini_identically() {
    // Task 6.4 (R3.3): a GTK theme switch writes the identical value to BOTH
    // settings.ini duplicates — and nothing else (uwsm/env and hyprland.conf are
    // cursor copies only) — then applies it live with one `gsettings set`.
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

    // (c) Everything else untouched — in particular uwsm/env and hyprland.conf.
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/gtk-3.0/settings.ini", "config/gtk-4.0/settings.ini"],
    );
}

#[test]
fn a_cursor_change_writes_all_four_duplicated_copies_identically() {
    // Task 6.4 (R3.4, the duplicated-values gotcha): the cursor theme is declared
    // in both settings.ini files, hyprland.conf's env lines, AND uwsm/env — a
    // change must write every copy identically so a writer can never desync them,
    // then reload via gsettings + `hyprctl setcursor` (plus `hyprctl reload` for
    // the changed hyprland.conf).
    let fx = FixtureDotfiles::install();
    fs::create_dir_all(fx.home().join(".icons/Fixture-Cursors/cursors"))
        .expect("create the fixture cursor theme");
    let before = repo_snapshot(&fx);

    let mut themes = load_themes(&fx);
    themes.stage_cursor_theme("Fixture-Cursors");

    let mut plan = empty_plan();
    let contribution = themes
        .apply_contribution()
        .expect("a dirty cursor contributes");
    plan.writes.extend(contribution.writes);
    plan.reload_params = contribution.reload_params;

    let caps = Capabilities::for_tests(&[Binary::Gsettings, Binary::Hyprctl], &[], true);
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written.len(), 4, "all four cursor copies are written");

    // (a) Exact bytes per copy: each file's own key/format, the identical value.
    let expect_patched = |relative: &str, from: &str, to: &str| {
        let repo_relative = format!("config/{relative}");
        let original = String::from_utf8(before[repo_relative.as_str()].clone())
            .expect("theme backing files are UTF-8");
        assert_eq!(
            fs::read_to_string(fx.config_path(relative)).expect("read the applied file"),
            replace_once(&original, from, to),
            "{relative}: only the cursor theme value may change"
        );
    };
    expect_patched(
        "gtk-3.0/settings.ini",
        "gtk-cursor-theme-name=Nordic-cursors",
        "gtk-cursor-theme-name=Fixture-Cursors",
    );
    expect_patched(
        "gtk-4.0/settings.ini",
        "gtk-cursor-theme-name=Nordic-cursors",
        "gtk-cursor-theme-name=Fixture-Cursors",
    );
    expect_patched(
        "hypr/hyprland.conf",
        "env = XCURSOR_THEME,Nordic-cursors",
        "env = XCURSOR_THEME,Fixture-Cursors",
    );
    expect_patched(
        "uwsm/env",
        "export XCURSOR_THEME=Nordic-cursors",
        "export XCURSOR_THEME=Fixture-Cursors",
    );

    // (b) The exact reload sequence in the canonical order: hyprctl reload (the
    // changed hyprland.conf), the two gsettings cursor keys (the size rides along
    // with the unchanged 16 — setcursor needs both), then hyprctl setcursor. The
    // gsettings/setcursor pair appears ONCE despite three files sharing it.
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("hyprctl").arg("reload"),
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
                "16",
            ]),
            Command::new("hyprctl").args(["setcursor", "Fixture-Cursors", "16"]),
        ]
    );

    // (c) Everything else untouched.
    assert_repo_untouched_except(
        &fx,
        &before,
        &[
            "config/gtk-3.0/settings.ini",
            "config/gtk-4.0/settings.ini",
            "config/hypr/hyprland.conf",
            "config/uwsm/env",
        ],
    );
}

#[test]
fn a_wallpaper_change_dual_writes_hyprpaper_and_hyprlock_and_reloads() {
    // Task 6.5 (the unified-wallpaper gotcha): with the lock override off, one
    // wallpaper change writes the SAME new path to hyprpaper.conf and
    // hyprlock.conf, then reloads hyprpaper live (preload + wallpaper with the
    // fit as the third comma-field). hyprlock gets no reload — it reads its
    // config at launch (intentional).
    let fx = FixtureDotfiles::install();
    // The new image the user picks; must exist and be readable for R8.3.
    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper_str = new_wallpaper.to_str().expect("UTF-8 path").to_string();
    let before = repo_snapshot(&fx);

    let mut wallpaper = load_wallpaper(&fx);
    assert!(!wallpaper.override_on(), "the fixture unifies both paths");
    wallpaper
        .stage_wallpaper(&new_wallpaper_str)
        .expect("an existing image stages");

    let mut plan = empty_plan();
    let contribution = wallpaper
        .apply_contribution()
        .expect("a dirty wallpaper contributes");
    plan.writes.extend(contribution.writes);
    plan.validations.extend(contribution.validations);
    plan.reload_params = contribution.reload_params;

    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(
        written.len(),
        2,
        "hyprpaper.conf AND hyprlock.conf are written"
    );

    // (a) Exact bytes: both files get the identical new path; every other byte —
    // including the fit mode and hyprlock's many other sections — is untouched.
    let old_path = fx.wallpaper_path();
    let old_path_str = old_path.to_str().expect("UTF-8 path");
    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        let repo_relative = format!("config/{relative}");
        let original = String::from_utf8(before[repo_relative.as_str()].clone())
            .expect("hypr configs are UTF-8");
        assert_eq!(
            fs::read_to_string(fx.config_path(relative)).expect("read the applied file"),
            replace_once(&original, old_path_str, &new_wallpaper_str),
            "{relative}: only the image path value may change"
        );
    }

    // (b) The exact reload: preload then set-on-all-outputs with the current fit
    // as the third comma-field; nothing for hyprlock.
    let wallpaper_arg = format!(",{new_wallpaper_str},cover");
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("hyprctl").args(["hyprpaper", "preload", new_wallpaper_str.as_str()]),
            Command::new("hyprctl").args(["hyprpaper", "wallpaper", wallpaper_arg.as_str()]),
        ]
    );

    // (c) Everything else untouched.
    assert_repo_untouched_except(
        &fx,
        &before,
        &["config/hypr/hyprpaper.conf", "config/hypr/hyprlock.conf"],
    );
}

#[test]
fn a_tilde_shaped_wallpaper_original_is_replaced_surgically() {
    // The real machine's configs use a tilde path (`~/Pictures/…`), not the
    // fixture's anonymized absolute one (the task-7.1 review flag): rewrite the
    // installed originals to the tilde form, then prove a staged absolute path
    // still applies as a surgical value-span edit in both files. Only the staged
    // (new) path is validated (R8.3) — the tilde original never has to resolve.
    let fx = FixtureDotfiles::install();
    let tilde_path = "~/Pictures/wallpaper/wallpaper.jpg";
    let absolute = fx.wallpaper_path();
    let absolute_str = absolute.to_str().expect("UTF-8 path");
    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        let live = fx.config_path(relative);
        let text = fs::read_to_string(&live).expect("read the installed file");
        // A plain write-through here is fixture setup, not the code under test.
        fs::write(&live, replace_once(&text, absolute_str, tilde_path))
            .expect("rewrite the original to the tilde form");
    }
    let before = repo_snapshot(&fx);

    let mut wallpaper = load_wallpaper(&fx);
    assert_eq!(
        wallpaper.wallpaper_path(),
        Some(tilde_path),
        "the tilde original is read verbatim"
    );
    assert!(
        !wallpaper.override_on(),
        "identical tilde paths still read as the unified default"
    );

    let new_wallpaper = fx.home().join("Pictures/wallpaper/next.png");
    fs::write(&new_wallpaper, b"fixture next wallpaper").expect("create the new image");
    let new_wallpaper_str = new_wallpaper.to_str().expect("UTF-8 path").to_string();
    wallpaper
        .stage_wallpaper(&new_wallpaper_str)
        .expect("an existing image stages over a tilde original");

    let mut plan = empty_plan();
    let contribution = wallpaper
        .apply_contribution()
        .expect("a dirty wallpaper contributes");
    plan.writes.extend(contribution.writes);
    plan.validations.extend(contribution.validations);
    plan.reload_params = contribution.reload_params;

    let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);
    let (reload_failures, _) = expect_applied(outcome);
    assert!(reload_failures.is_empty());

    // Exact bytes: the tilde-shaped original with ONLY the tilde span replaced by
    // the new absolute path, in both files.
    for relative in ["hypr/hyprpaper.conf", "hypr/hyprlock.conf"] {
        let repo_relative = format!("config/{relative}");
        let original = String::from_utf8(before[repo_relative.as_str()].clone())
            .expect("hypr configs are UTF-8");
        assert_eq!(
            fs::read_to_string(fx.config_path(relative)).expect("read the applied file"),
            replace_once(&original, tilde_path, &new_wallpaper_str),
            "{relative}: the tilde original must be replaced surgically"
        );
    }
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
        &[Daemon::Eww, Daemon::Swaync, Daemon::Kitty],
        true,
    );
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![77])]);
    let outcome = apply::run(&plan, &freshness, &caps, &runner, &signaller);

    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            assert!(
                matches!(
                    failure.cause,
                    WriteFailureCause::GenerateColorsExit { code: Some(1) }
                ),
                "the cause is the generator's non-zero exit, got {:?}",
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
