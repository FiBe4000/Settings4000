//! Smoke tests for the fixture-dotfiles installer (task 7.1, R6.1).
//!
//! [`FixtureDotfiles::install`] is the shared foundation of the integration
//! suites (the end-to-end Apply suites of task 7.2 build on it), so this file
//! proves the installed tree itself is sound: the deployment symlinks resolve
//! into the repo, repo-root discovery (R8.5) finds the palette source behind
//! them, every parser family reads its fixture file at the *deployed* path
//! (through the symlink, exactly as the app does) with the expected values and
//! byte-identical round-trips, and per-test installs are isolated. Deeper
//! behavior — staging, Apply, rollback, reload sequences — is task 7.2's job,
//! not this file's.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use settings4000::core::detect::{Capabilities, DetectionInputs};
use settings4000::core::model::validate_image_path;
use settings4000::parsers::env::{EnvFile, GtkThemeOverride};
use settings4000::parsers::generated::{ActiveScheme, read_active_scheme};
use settings4000::parsers::hyprlang::{HyprlangFile, KeyPath, SectionStep};
use settings4000::parsers::ini::IniFile;
use settings4000::parsers::monitors::{MonitorField, MonitorsFile};
use settings4000::parsers::palette::PaletteFile;
use settings4000::parsers::swaync::SwayncConfigFile;
use settings4000::testing::{FixtureDotfiles, HOME_PLACEHOLDER};

/// Reads a file, panicking with the path on failure (test-only convenience).
fn read(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Placeholder substitution must be complete: no installed file may still
/// contain the anonymization prefix the fixture uses for home paths.
///
/// The parser tests below only read the files they parse, so without this
/// walk a future fixture file whose `/home/user` placeholder lands somewhere
/// unparsed would install with a dangling, never-substituted path and no test
/// would notice.
#[test]
fn no_installed_file_retains_the_home_placeholder() {
    let fx = FixtureDotfiles::install();

    let mut pending = vec![fx.repo_root().to_path_buf()];
    let mut files_checked = 0usize;
    while let Some(dir) = pending.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                pending.push(path);
            } else {
                assert!(
                    !read(&path).contains(HOME_PLACEHOLDER),
                    "{} still contains the {HOME_PLACEHOLDER} placeholder — \
                     installation should have substituted it",
                    path.display()
                );
                files_checked += 1;
            }
        }
    }
    // Guard the guard: an empty walk would vacuously pass, so make sure the
    // traversal actually visited the installed tree.
    assert!(
        files_checked > 20,
        "expected to check the whole installed repo, saw only {files_checked} files"
    );
}

/// Every deployed config path must be a symlink resolving to the matching file
/// inside the repo — the layout both the repo-root discovery (R8.5) and the
/// symlink-following atomic writer (R5.4) depend on.
#[test]
fn deployed_config_paths_are_symlinks_into_the_repo() {
    let fx = FixtureDotfiles::install();

    let deployed = [
        "hypr/colors.conf",
        "hypr/hypridle.conf",
        "hypr/hyprland.conf",
        "hypr/hyprlock.conf",
        "hypr/hyprpaper.conf",
        "hypr/input.conf",
        "hypr/monitors.conf",
        "gtk-3.0/settings.ini",
        "gtk-4.0/settings.ini",
        "uwsm/env",
        "swaync/config.json",
        "swaync/colors.css",
        "swaync/style.css",
        "kitty/kitty.conf",
        "kitty/colors.conf",
        "kitty/fonts.conf",
        "eww/_colors.scss",
        "eww/_fonts.scss",
        "rofi/colors.rasi",
        "rofi/fonts.rasi",
    ];
    for relative in deployed {
        let live = fx.config_path(relative);
        let meta = fs::symlink_metadata(&live)
            .unwrap_or_else(|e| panic!("missing deployed path {}: {e}", live.display()));
        assert!(
            meta.file_type().is_symlink(),
            "{} should be a deployment symlink",
            live.display()
        );
        let resolved = fs::canonicalize(&live)
            .unwrap_or_else(|e| panic!("dangling symlink {}: {e}", live.display()));
        assert_eq!(
            resolved,
            fx.repo_path(&format!("config/{relative}")),
            "{} should resolve into the repo",
            live.display()
        );
    }

    // The one home-level deployment link.
    let zsh_colors = fx.home().join(".zsh_colors");
    assert_eq!(
        fs::canonicalize(&zsh_colors).expect(".zsh_colors should resolve"),
        fx.repo_path("zsh/colors.zsh")
    );

    // The generator stub must have survived installation executable, since the
    // palette-source capability and the Apply pipeline address it as a program.
    let generator = fx.repo_path("scripts/generate-colors");
    let mode = fs::metadata(&generator)
        .expect("generate-colors should exist")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "generate-colors should be executable (mode {mode:o})"
    );
}

/// Real capability detection over the installed tree: canonicalizing the
/// deployed `hypr/colors.conf` symlink must discover the fixture repo root and
/// its palette source (R3.2/R8.5), and the deployed configs must read as
/// readable (R4.4).
#[test]
fn detection_discovers_the_palette_source_behind_the_deployed_symlink() {
    let fx = FixtureDotfiles::install();

    let input_conf = fx.config_path("hypr/input.conf");
    let inputs = DetectionInputs {
        // No binaries, daemons, or compositor in a headless test — only the
        // filesystem-backed probes matter here.
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: vec![input_conf.clone()],
    };
    let caps = Capabilities::detect(&inputs);

    let source = caps
        .palette_source()
        .expect("the deployed symlink should reveal the palette source");
    assert_eq!(source.repo_root(), fx.repo_root());
    assert_eq!(source.colors_dir(), fx.repo_path("colors"));
    assert_eq!(
        source.generate_colors(),
        fx.repo_path("scripts/generate-colors")
    );
    assert!(caps.config_readable(&input_conf));
}

/// The palette scheme files parse with the full 17-key schema and round-trip
/// byte-identically (R3.2, R8.3).
#[test]
fn palette_schemes_parse_and_satisfy_the_schema() {
    let fx = FixtureDotfiles::install();

    for scheme in ["everforest", "nord"] {
        let input = read(&fx.repo_path(&format!("colors/{scheme}")));
        let (palette, warnings) = PaletteFile::parse(&input);
        assert!(warnings.is_empty(), "{scheme}: unexpected {warnings:?}");
        assert!(
            palette.validate().is_valid(),
            "{scheme} should satisfy the 17-key schema"
        );
        assert_eq!(palette.emit(), input, "{scheme} should round-trip");
    }
}

/// Every hyprlang fixture parses warning-free through its deployed symlink,
/// round-trips byte-identically, and yields the documented values at the
/// section paths the app edits (analysis §6.3, tasks 6.5/6.6/6.8).
#[test]
fn hyprlang_configs_parse_losslessly_at_their_deployed_paths() {
    let fx = FixtureDotfiles::install();

    // Round-trip identity + warning-free parse for the whole hyprlang family.
    for relative in [
        "hypr/input.conf",
        "hypr/hyprland.conf",
        "hypr/hypridle.conf",
        "hypr/hyprlock.conf",
        "hypr/hyprpaper.conf",
    ] {
        let input = read(&fx.config_path(relative));
        let (file, warnings) = HyprlangFile::parse(&input);
        assert!(warnings.is_empty(), "{relative}: unexpected {warnings:?}");
        assert_eq!(file.emit(), input, "{relative} should round-trip");
    }

    // input.conf: the Input page's section paths (task 6.6).
    let (input_conf, _) = HyprlangFile::parse(&read(&fx.config_path("hypr/input.conf")));
    assert_eq!(
        input_conf.value(&KeyPath::at(&["input"], "kb_layout")),
        Some("us,se")
    );
    assert_eq!(
        input_conf.value(&KeyPath::at(&["input", "touchpad"], "natural_scroll")),
        Some("true")
    );

    // hypridle.conf: the positional listener addressing (task 6.8) — dim 150,
    // lock 300, DPMS 330, in file order.
    let (hypridle, _) = HyprlangFile::parse(&read(&fx.config_path("hypr/hypridle.conf")));
    for (occurrence, timeout) in [(0, "150"), (1, "300"), (2, "330")] {
        assert_eq!(
            hypridle.value(&KeyPath::new(
                vec![SectionStep::nth("listener", occurrence)],
                "timeout",
            )),
            Some(timeout),
            "listener[{occurrence}].timeout"
        );
    }

    // hyprpaper/hyprlock: the unified wallpaper path (analysis §6.2) points at
    // the substituted, existing stub in both files and passes R8.3 validation.
    let wallpaper = fx.wallpaper_path();
    let expected = wallpaper.to_str().expect("wallpaper path is UTF-8");
    let (hyprpaper, _) = HyprlangFile::parse(&read(&fx.config_path("hypr/hyprpaper.conf")));
    assert_eq!(
        hyprpaper.value(&KeyPath::at(&["wallpaper"], "path")),
        Some(expected)
    );
    let (hyprlock, _) = HyprlangFile::parse(&read(&fx.config_path("hypr/hyprlock.conf")));
    assert_eq!(
        hyprlock.value(&KeyPath::at(&["background"], "path")),
        Some(expected)
    );
    assert!(validate_image_path(&wallpaper).is_ok());
}

/// `monitors.conf` parses warning-free with the awk-shaped eDP record
/// `hypr-display-profile.sh` derives from (analysis §6.2).
#[test]
fn monitors_conf_parses_with_the_awk_shaped_edp_record() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.config_path("hypr/monitors.conf"));
    let (monitors, warnings) = MonitorsFile::parse(&input);
    assert!(warnings.is_empty(), "unexpected {warnings:?}");
    assert_eq!(monitors.emit(), input, "monitors.conf should round-trip");

    assert!(monitors.record_names().contains(&"eDP-1"));
    assert_eq!(
        monitors.field("eDP-1", MonitorField::Mode),
        Some("2880x1800@120")
    );
    assert_eq!(
        monitors.field("eDP-1", MonitorField::Scale),
        Some("1.333333")
    );
}

/// The swaync config parses as canonical 2-space-pretty JSON (so the adapter's
/// normalization round-trips byte-identically) with the keys the Notifications
/// page edits (task 6.7).
#[test]
fn swaync_config_parses_with_stable_shape() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.config_path("swaync/config.json"));
    let config = SwayncConfigFile::parse(&input).expect("config.json should parse");
    assert_eq!(config.string("positionX"), Some("right"));
    assert_eq!(config.string("positionY"), Some("top"));
    assert_eq!(config.integer("timeout"), Some(10));
    assert_eq!(config.emit(), input, "config.json should round-trip");
}

/// Both GTK `settings.ini` files parse with the `[Settings]` section holding
/// the unified cursor values (analysis §6.5) and round-trip byte-identically.
#[test]
fn gtk_settings_ini_parse_with_the_unified_cursor_values() {
    let fx = FixtureDotfiles::install();

    for relative in ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"] {
        let input = read(&fx.config_path(relative));
        let (ini, warnings) = IniFile::parse(&input);
        assert!(warnings.is_empty(), "{relative}: unexpected {warnings:?}");
        assert_eq!(ini.emit(), input, "{relative} should round-trip");
        assert_eq!(
            ini.value("Settings", "gtk-cursor-theme-name"),
            Some("Nordic-cursors"),
            "{relative}"
        );
        assert_eq!(
            ini.value("Settings", "gtk-cursor-theme-size"),
            Some("16"),
            "{relative}"
        );
        assert_eq!(
            ini.value("Settings", "gtk-theme-name"),
            Some("Everforest-Green-Dark"),
            "{relative}"
        );
    }
}

/// `uwsm/env` parses with the canonical cursor values and the deliberately
/// commented-out `GTK_THEME` line the override check reads (R3.3).
#[test]
fn uwsm_env_parses_with_the_commented_gtk_theme_override() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.config_path("uwsm/env"));
    let (env, warnings) = EnvFile::parse(&input);
    assert!(warnings.is_empty(), "unexpected {warnings:?}");
    assert_eq!(env.emit(), input, "uwsm/env should round-trip");

    assert_eq!(env.value("XCURSOR_THEME"), Some("Nordic-cursors"));
    assert_eq!(env.value("XCURSOR_SIZE"), Some("16"));
    assert_eq!(
        env.gtk_theme_override(),
        GtkThemeOverride::Commented {
            value: "Nordic-bluish-accent".to_string(),
        }
    );
}

/// The generated `colors.conf` header names the active scheme (R3.2) and the
/// repo's `state/active-scheme` marker agrees (analysis §6.4).
#[test]
fn active_scheme_reads_from_the_generated_header() {
    let fx = FixtureDotfiles::install();

    assert_eq!(
        read_active_scheme(&fx.config_path("hypr/colors.conf")),
        ActiveScheme::Named("everforest".to_string())
    );
    assert_eq!(
        read(&fx.repo_path("state/active-scheme")).trim(),
        "everforest"
    );
}

/// Each install is a fully independent tree, and writing through a deployed
/// symlink lands in that tree's repo file — the property the atomic writer's
/// symlink-following contract (R5.4) relies on.
#[test]
fn installs_are_isolated_and_symlinks_write_through_to_the_repo() {
    let fx_a = FixtureDotfiles::install();
    let fx_b = FixtureDotfiles::install();
    let original = read(&fx_b.config_path("hypr/input.conf"));

    // A plain write through tree A's deployed symlink…
    fs::write(fx_a.config_path("hypr/input.conf"), "input {\n}\n")
        .expect("write through the symlink should succeed");

    // …lands in A's repo file and leaves tree B untouched.
    assert_eq!(
        read(&fx_a.repo_path("config/hypr/input.conf")),
        "input {\n}\n"
    );
    assert_eq!(read(&fx_b.config_path("hypr/input.conf")), original);
}
