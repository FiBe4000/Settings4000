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
//!
//! Since task 11.1 this file also holds the only tests that execute the fixture
//! shell stubs (`scripts/generate-colors`, `scripts/hypr-toggle-laptop-display`,
//! `scripts/notifctl`) as real subprocesses, to prove the stubs behave as their
//! headers promise. Every other suite drives those scripts through
//! `MockCommandRunner` and must never spawn them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use settings4000::core::detect::{Capabilities, DetectionInputs, HyprConfigProvider};
use settings4000::core::model::validate_image_value;
use settings4000::core::notifd::NotifdReplies;
use settings4000::core::theme::load_palette_schema;
use settings4000::parsers::env::{EnvFile, GtkThemeOverride};
use settings4000::parsers::generated::{
    ActiveScheme, GENERATED_COLOR_PARTIALS, detect_active_scheme, generated_files,
    read_active_scheme,
};
use settings4000::parsers::hyprlang::{HyprlangFile, KeyPath, SectionStep};
use settings4000::parsers::ini::IniFile;
use settings4000::parsers::lua_literal::{LuaConfigFile, LuaValue, TableStatus};
use settings4000::parsers::monitors::{MonitorField, MonitorsFile};
use settings4000::parsers::palette::{PaletteFile, PaletteSchema, SchemaSource};
use settings4000::testing::{FixtureDotfiles, assert_repo_untouched_except, repo_snapshot};

/// Reads a file, panicking with the path on failure (test-only convenience).
fn read(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The anonymization rule "no usernames/home paths" (`tests/fixtures/README.md`)
/// holds for every file: none names an absolute path under `/home/`.
///
/// The fixture spells home paths the way the real dotfiles do, home-relative
/// (`~/…`) or built from `$HOME`, so they resolve inside each test's temp home
/// without any install-time rewriting (task 11.10). The parser tests below only
/// read the files they parse, so without this walk a future fixture file copied
/// with a real user's absolute path would go unnoticed — and would point every
/// test at a path outside the isolated tree.
#[test]
fn no_fixture_file_names_an_absolute_home_path() {
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
                    !read(&path).contains("/home/"),
                    "{} names an absolute /home/ path — write it home-relative \
                     (`~/…`) as the real dotfiles do",
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
        "hypr/hyprlock.conf",
        "hypr/hyprpaper.conf",
        "hypr/monitors.conf",
        "gtk-3.0/settings.ini",
        "gtk-4.0/settings.ini",
        "uwsm/env",
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

    let input_lua = fx.config_path("hypr/input.lua");
    let inputs = DetectionInputs {
        // No binaries, daemons, or compositor in a headless test — only the
        // filesystem-backed probes matter here.
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: vec![input_lua.clone()],
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
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
    assert!(caps.config_readable(&input_lua));
    // Task 11.2: with no running session, the deployed `hyprland.lua` makes the
    // host read as a Lua-config host, so the Lua-only controls stay visible.
    assert_eq!(caps.hypr_config_provider(), HyprConfigProvider::Lua);
}

/// The palette scheme files parse, satisfy the schema read from the fixture
/// generator's `NAMES` line (18 keys, `shadow` last), and round-trip
/// byte-identically (R3.2, R8.3; task 11.6).
#[test]
fn palette_schemes_parse_and_satisfy_the_schema() {
    let fx = FixtureDotfiles::install();

    // The stub's header comment mentions `NAMES` as well; only the verbatim
    // script's column-0 assignment may be read.
    let generator = fx.repo_path("scripts/generate-colors");
    let schema = load_palette_schema(&generator);
    assert_eq!(schema.source(), &SchemaSource::Generator(generator));
    assert_eq!(
        schema.keys(),
        PaletteSchema::built_in().keys(),
        "the fixture generator lists the same 18 keys as the built-in fallback"
    );

    for scheme in ["everforest", "nord"] {
        let input = read(&fx.repo_path(&format!("colors/{scheme}")));
        let (palette, warnings) = PaletteFile::parse(&input);
        assert!(warnings.is_empty(), "{scheme}: unexpected {warnings:?}");
        let report = palette.validate(&schema);
        assert!(
            report.is_valid(),
            "{scheme} should satisfy the generator's schema: {report:?}"
        );
        assert!(palette.value("shadow").is_some(), "{scheme} defines shadow");
        assert_eq!(palette.emit(), input, "{scheme} should round-trip");
    }
}

/// Every generator output named by `parsers::generated` exists in the fixture
/// repo, and every color partial's header — `colors.lua`'s `-- # Generated …`
/// included — names the fixture's active scheme, read through its deployed
/// path (analysis §7.6, §8.3; task 11.6).
#[test]
fn every_generated_file_exists_and_every_color_header_names_the_active_scheme() {
    let fx = FixtureDotfiles::install();

    for relative in generated_files() {
        assert!(
            fx.repo_path(relative).is_file(),
            "{relative} is a generator output and must exist in the fixture"
        );
    }
    for relative in GENERATED_COLOR_PARTIALS {
        // Deployed under `$XDG_CONFIG_HOME` (`config/…`) or as `~/.zsh_colors`.
        let live = match relative.strip_prefix("config/") {
            Some(config_relative) => fx.config_path(config_relative),
            None => fx.home().join(".zsh_colors"),
        };
        assert_eq!(
            detect_active_scheme(&read(&live)),
            ActiveScheme::Named("everforest".to_string()),
            "{relative} should name the active scheme in its header"
        );
    }
}

/// Every hyprlang fixture parses warning-free through its deployed symlink,
/// round-trips byte-identically, and yields the documented values at the
/// section paths the app edits (tasks 6.5/6.8).
#[test]
fn hyprlang_configs_parse_losslessly_at_their_deployed_paths() {
    let fx = FixtureDotfiles::install();

    // Round-trip identity + warning-free parse for the whole hyprlang family.
    for relative in [
        "hypr/hypridle.conf",
        "hypr/hyprlock.conf",
        "hypr/hyprpaper.conf",
    ] {
        let input = read(&fx.config_path(relative));
        let (file, warnings) = HyprlangFile::parse(&input);
        assert!(warnings.is_empty(), "{relative}: unexpected {warnings:?}");
        assert_eq!(file.emit(), input, "{relative} should round-trip");
    }

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

    // hyprpaper/hyprlock: the unified wallpaper path (analysis §6.2) is the
    // real dotfiles' home-relative value in both files (analysis §7.4); expanded
    // against the fixture home it names the installed stub and passes R8.3
    // validation (task 11.10).
    let expected = "~/Pictures/wallpaper/18.jpg";
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
    assert_eq!(
        fx.wallpaper_path(),
        fx.home().join("Pictures/wallpaper/18.jpg")
    );
    assert!(validate_image_value(expected, Some(fx.home())).is_ok());
}

/// Both Lua configs parse structurally sound through their deployed symlinks,
/// round-trip byte-identically, and expose exactly what the Input page and the
/// cursor write set address (analysis §7.1, task 11.3): a fully recognised
/// `input` table in `input.lua` with the Lua key names (`tap_to_click`), and the
/// two cursor `hl.env` lines of `hyprland.lua`.
#[test]
fn lua_configs_parse_losslessly_at_their_deployed_paths() {
    let fx = FixtureDotfiles::install();

    for relative in ["hypr/input.lua", "hypr/hyprland.lua"] {
        let input = read(&fx.config_path(relative));
        let file = LuaConfigFile::parse(&input);
        assert_eq!(file.structure_error(), None, "{relative}");
        assert_eq!(file.emit(), input, "{relative} should round-trip");
    }

    let input_lua = LuaConfigFile::parse(&read(&fx.config_path("hypr/input.lua")));
    assert_eq!(input_lua.table_status("input"), TableStatus::Clean);
    assert_eq!(
        input_lua.value("input.kb_layout"),
        Some(&LuaValue::String("us,se".to_string()))
    );
    assert_eq!(
        input_lua.value("input.touchpad.tap_to_click"),
        Some(&LuaValue::Boolean(true))
    );
    assert_eq!(
        input_lua.value("input.kb_variant"),
        None,
        "absent until inserted"
    );

    let hyprland_lua = LuaConfigFile::parse(&read(&fx.config_path("hypr/hyprland.lua")));
    assert_eq!(
        hyprland_lua.env_value("XCURSOR_THEME"),
        Some("breeze_cursors")
    );
    assert_eq!(hyprland_lua.env_value("XCURSOR_SIZE"), Some("24"));
}

/// `monitors.conf` parses warning-free with the awk-shaped eDP record
/// `hypr-display-profile.sh` derives from (analysis §6.2), and validates clean
/// against the frozen grammar both downstream parsers rely on (analysis §7.2,
/// task 11.9) — read at its deployed path, as the app reads it.
#[test]
fn monitors_conf_parses_with_the_awk_shaped_edp_record() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.config_path("hypr/monitors.conf"));
    let (monitors, warnings) = MonitorsFile::parse(&input);
    assert!(warnings.is_empty(), "unexpected {warnings:?}");
    assert_eq!(
        monitors.grammar_violations(),
        Vec::new(),
        "the live fixture follows the frozen grammar"
    );
    assert_eq!(monitors.emit(), input, "monitors.conf should round-trip");
    // The rules the grammar has to allow are all present in the real file: the
    // catch-all first, `bitdepth,10` extras, and `eDP-1` beside the `desc:` rule for
    // the other laptop's panel.
    let names = monitors.record_names();
    assert_eq!(names.first(), Some(&""), "the catch-all comes first");
    assert!(names.contains(&"desc:Example Panel Vendor 0x2036"));

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

/// Both GTK `settings.ini` files parse with the `[Settings]` section holding
/// the unified cursor values (`breeze_cursors`/24, analysis §7.7) and round-trip
/// byte-identically. Neither carries `gtk-xft-dpi`, which the app never writes
/// (analysis §7.7: X11 DPI comes from `xrdb`).
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
            Some("breeze_cursors"),
            "{relative}"
        );
        assert_eq!(
            ini.value("Settings", "gtk-cursor-theme-size"),
            Some("24"),
            "{relative}"
        );
        assert_eq!(ini.value("Settings", "gtk-xft-dpi"), None, "{relative}");
        assert_eq!(
            ini.value("Settings", "gtk-theme-name"),
            Some("Everforest-Green-Dark"),
            "{relative}"
        );
    }
}

/// `uwsm/env` parses with the canonical cursor values (`breeze_cursors`/24) and
/// the deliberately commented-out `GTK_THEME` line the override check reads
/// (R3.3).
#[test]
fn uwsm_env_parses_with_the_commented_gtk_theme_override() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.config_path("uwsm/env"));
    let (env, warnings) = EnvFile::parse(&input);
    assert!(warnings.is_empty(), "unexpected {warnings:?}");
    assert_eq!(env.emit(), input, "uwsm/env should round-trip");

    assert_eq!(env.value("XCURSOR_THEME"), Some("breeze_cursors"));
    assert_eq!(env.value("XCURSOR_SIZE"), Some("24"));
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
    let original = read(&fx_b.config_path("hypr/input.lua"));

    // A plain write through tree A's deployed symlink…
    fs::write(fx_a.config_path("hypr/input.lua"), "hl.config({})\n")
        .expect("write through the symlink should succeed");

    // …lands in A's repo file and leaves tree B untouched.
    assert_eq!(
        read(&fx_a.repo_path("config/hypr/input.lua")),
        "hl.config({})\n"
    );
    assert_eq!(read(&fx_b.config_path("hypr/input.lua")), original);
}

/// The files the `wayland-dev` dotfiles added (task 11.1, analysis §7–§8) are
/// deployed at their live paths: the Lua Hyprland config and its two
/// `dofile`d companions, the notifd config and the notifd user unit (whose
/// presence marks notifd as installed, task 11.7) under `$XDG_CONFIG_HOME`, and
/// the XCursor fallback theme under `$HOME/.icons` — each a symlink into the
/// repo, as `setup.sh` links them.
#[test]
fn wayland_dev_files_are_deployed_at_their_live_paths() {
    let fx = FixtureDotfiles::install();

    let mut links: Vec<(std::path::PathBuf, std::path::PathBuf)> = [
        "hypr/hyprland.lua",
        "hypr/input.lua",
        "hypr/colors.lua",
        "notifd/config",
        "systemd/user/notifd.service",
    ]
    .iter()
    .map(|relative| {
        (
            fx.config_path(relative),
            fx.repo_path(&format!("config/{relative}")),
        )
    })
    .collect();
    links.push((
        fx.home().join(".icons/default/index.theme"),
        fx.repo_path("icons/default/index.theme"),
    ));

    for (live, expected_target) in links {
        let meta = fs::symlink_metadata(&live)
            .unwrap_or_else(|e| panic!("missing deployed path {}: {e}", live.display()));
        assert!(
            meta.file_type().is_symlink(),
            "{} should be a deployment symlink",
            live.display()
        );
        assert_eq!(
            fs::canonicalize(&live)
                .unwrap_or_else(|e| panic!("dangling symlink {}: {e}", live.display())),
            expected_target,
            "{} should resolve into the repo",
            live.display()
        );
    }

    // Only the file is linked; its directory is a plain one (`setup.sh` runs
    // `mkdir -p ~/.icons/default`), so a writer can never replace the link's
    // parent with a repo directory.
    let icons_dir = fs::symlink_metadata(fx.home().join(".icons/default"))
        .expect("~/.icons/default should exist");
    assert!(icons_dir.is_dir() && !icons_dir.file_type().is_symlink());
}

/// The XCursor fallback theme reads as INI with the cursor theme in
/// `[Icon Theme]` `Inherits=` and round-trips byte-identically — the shape the
/// R3.4 cursor write set edits through the INI writer (analysis §8.4).
#[test]
fn the_cursor_fallback_theme_parses_as_ini() {
    let fx = FixtureDotfiles::install();

    let input = read(&fx.home().join(".icons/default/index.theme"));
    let (ini, warnings) = IniFile::parse(&input);
    assert!(warnings.is_empty(), "unexpected {warnings:?}");
    assert_eq!(ini.emit(), input, "index.theme should round-trip");
    assert_eq!(ini.value("Icon Theme", "Inherits"), Some("breeze_cursors"));
}

/// The generator stub carries the real script's `NAMES=(…)` schema line
/// verbatim — exactly once, 18 keys with `shadow` last (analysis §7.6) — yet
/// stays inert when executed: it exits 2 before reaching the copied body, so
/// no generated file is touched.
#[test]
fn the_generator_stub_carries_the_real_schema_line_and_stays_inert() {
    let fx = FixtureDotfiles::install();
    let generator = fx.repo_path("scripts/generate-colors");

    let script = read(&generator);
    let names_lines: Vec<&str> = script
        .lines()
        .filter(|line| line.starts_with("NAMES=("))
        .collect();
    assert_eq!(
        names_lines,
        [
            "NAMES=(bg0 bg1 bg2 bg3 fg0 fg1 fg2 accent0 accent1 accent2 accent3 \
          red orange yellow green blue purple shadow)"
        ],
        "the stub must carry exactly one, verbatim, NAMES line"
    );

    let before = repo_snapshot(&fx);
    let output = Command::new(&generator)
        .arg("nord")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", generator.display()));
    assert_eq!(
        output.status.code(),
        Some(2),
        "the stub must keep failing with 2"
    );
    assert!(output.stdout.is_empty(), "no generator output may appear");
    assert_repo_untouched_except(&fx, &before, &[]);
}

/// The two action-script stubs are executable no-ops that succeed and record
/// their argv (task 11.1): one line per call, script name plus shell-quoted
/// arguments, appended to the file named by `SETTINGS4000_FIXTURE_STUB_LOG`.
/// Running them changes nothing in the repo.
#[test]
fn the_action_script_stubs_record_their_argv_and_succeed() {
    let fx = FixtureDotfiles::install();
    // Outside the repo, so the untouched-repo assertion below stays meaningful.
    let log = fx.home().join("stub-calls.log");
    let before = repo_snapshot(&fx);

    let calls: [(&str, &[&str]); 3] = [
        ("hypr-toggle-laptop-display", &[]),
        ("notifctl", &["dnd", "on"]),
        ("notifctl", &["inhibit", "add", "two words"]),
    ];
    for (script, args) in calls {
        let path = fx.repo_path(&format!("scripts/{script}"));
        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{} should exist: {e}", path.display()))
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script} should be executable");

        let output = Command::new(&path)
            .args(args)
            .env("SETTINGS4000_FIXTURE_STUB_LOG", &log)
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()));
        assert!(output.status.success(), "{script} stub should exit 0");
        // The same record goes to stderr, where a command runner logs it.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with(&format!("settings4000 fixture stub: {script}")),
            "{script} should announce itself on stderr, got {stderr:?}"
        );
    }

    assert_eq!(
        read(&log),
        "hypr-toggle-laptop-display\nnotifctl dnd on\nnotifctl inhibit add two\\ words\n"
    );
    assert_repo_untouched_except(&fx, &before, &[]);
}
