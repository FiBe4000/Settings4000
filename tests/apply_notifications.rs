//! End-to-end staged-edit → Apply suite for the **Notifications** page's staged half
//! (task 11.15; R4.2, R5.3–R5.6, R6.1, R8.3).
//!
//! notifd (the dotfiles' notification daemon, version 1.1 and later) reads seven
//! tunables from `~/.config/notifd/config` and re-reads them on its D-Bus `Reload()`
//! method (analysis §8.2). This suite drives that file through the app's real code
//! against the installed fixture tree (task 7.1): the file is loaded into a
//! [`SettingsStore`] by the app's own loader, edits are staged like the page stages
//! them, the plan is assembled by the app's real assembler with the
//! [`NotifdConfigModel`] write glue, and [`apply::run`] executes it with a
//! [`MockCommandRunner`]. So it asserts the exact bytes written at the deployed
//! (symlinked) path, that the repo copy behind the link received them, the exact
//! command list (one `busctl … Reload`), and that every other repo file is untouched.
//!
//! It also covers the two refusals — values notifd would ignore are refused when staged
//! (R8.3), and the staged rows are not offered without a notifd ≥ 1.1 and a readable
//! file (R4.2) — and the shared `Reload()` with an icon-theme change (task 11.14). The
//! page's runtime half (do-not-disturb, clear all) is covered by `core::notifications`.

use std::fs;
use std::path::PathBuf;

use settings4000::core::apply::{self, ApplyOutcome};
use settings4000::core::assemble::{ApplySources, assemble_apply_plan};
use settings4000::core::detect::{Binary, Capabilities};
use settings4000::core::model::{SettingId, ValidationError, Value};
use settings4000::core::notifd::{NotifdStatus, NotifdVersion};
use settings4000::core::notifd_config::NotifdConfigModel;
use settings4000::core::store::{SettingsStore, StageError};
use settings4000::core::theme::{ThemeRoots, ThemesModel, ThemesPaths};
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{
    CapturedEvent, FixtureDotfiles, assert_repo_untouched_except, capture_logs, differing_lines,
    expect_applied, load_into_store, loaders, notifications, replace_once, repo_snapshot,
};

/// The repo-relative path of notifd's settings file, for the untouched-repo checks.
const REPO_NOTIFD_CONFIG: &str = "config/notifd/config";

/// The deployed (live XDG) path of the file — what the app addresses (R8.5), a symlink
/// into the fixture repo.
fn notifd_config(fx: &FixtureDotfiles) -> PathBuf {
    fx.config_path("notifd/config")
}

/// A running notifd reporting `version`.
fn notifd_running(version: &str) -> NotifdStatus {
    NotifdStatus::Running {
        version: NotifdVersion::parse(version),
    }
}

/// Capabilities with a running notifd 1.1 — the gate of both the staged rows and the
/// `Reload()` call.
fn caps() -> Capabilities {
    Capabilities::for_tests(&[], &[], false).with_notifd_for_tests(notifd_running("1.1"))
}

/// notifd's `Reload()` call, exactly as the reload table issues it.
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

/// Loads the deployed file into a fresh store through the app's loader and builds the
/// page's write glue, mirroring the startup wiring.
fn store_and_model(fx: &FixtureDotfiles) -> (SettingsStore, NotifdConfigModel) {
    let path = notifd_config(fx);
    let mut store = SettingsStore::new();
    load_into_store(&mut store, &path, loaders::notifd_config);
    let model = notifications::offer(&caps(), path).expect("the fixture file backs the rows");
    (store, model)
}

/// Assembles the plan the window's Apply handler would for the store plus the notifd
/// helper, and runs it under `caps` with `runner`.
fn apply_notifd(
    store: &SettingsStore,
    model: &NotifdConfigModel,
    runner: &MockCommandRunner,
) -> ApplyOutcome {
    let assembled = assemble_apply_plan(ApplySources {
        notifications: Some(model),
        ..ApplySources::for_store(store)
    })
    .expect("the plan assembles");
    apply::run(
        &assembled.plan,
        store.freshness(),
        &caps(),
        runner,
        &MockProcessSignaller::new(),
    )
}

#[test]
fn a_timeout_change_rewrites_one_value_span_through_the_symlink_and_reloads_notifd_once() {
    // The headline acceptance criterion: staging a timeout rewrites exactly that value
    // span — every comment and blank line of the shipped file intact — at the deployed
    // path, whose repo target receives the bytes while the link stays a link, and the
    // only command is notifd's `Reload()`.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = notifd_config(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture file");

    let (mut store, model) = store_and_model(&fx);
    assert_eq!(
        store.value(SettingId::NotifdTimeoutNormal),
        Some(&Value::Integer(10_000)),
        "the shipped value is loaded"
    );
    store
        .stage(SettingId::NotifdTimeoutNormal, Value::Integer(7500))
        .expect("an in-range timeout stages");

    let runner = MockCommandRunner::new();
    let (reload_failures, written) = expect_applied(apply_notifd(&store, &model, &runner));
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.clone()]);

    let applied = fs::read_to_string(&path).expect("read the applied file");
    assert_eq!(
        applied,
        replace_once(
            &original,
            "timeout_normal_ms=10000",
            "timeout_normal_ms=7500"
        ),
        "only the one value span may change"
    );
    assert_eq!(
        differing_lines(&original, &applied).len(),
        1,
        "exactly one line differs"
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path(REPO_NOTIFD_CONFIG)).expect("read the repo copy"),
        applied,
        "the write lands in the repo target behind the deployment symlink"
    );
    assert!(
        fs::symlink_metadata(&path)
            .expect("stat the deployed path")
            .file_type()
            .is_symlink(),
        "the deployed path is still a symlink"
    );

    assert_eq!(
        runner.recorded(),
        vec![notifd_reload()],
        "exactly one Reload"
    );
    assert_repo_untouched_except(&fx, &before, &[REPO_NOTIFD_CONFIG]);
}

#[test]
fn values_notifd_would_ignore_are_refused_when_staged_with_the_rule_named() {
    // R8.3: notifd silently falls back to its default for an out-of-range value, so the
    // app refuses such a value at stage time — the control reverts — and the message
    // names the key and the range.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let (mut store, model) = store_and_model(&fx);

    for (id, value, key, range) in [
        (
            SettingId::NotifdCriticalPerApp,
            6,
            "critical_per_app",
            "1 to 5",
        ),
        (
            SettingId::NotifdHistoryMax,
            0,
            "history_max",
            "1 to 2147483647",
        ),
    ] {
        match store.stage(id, Value::Integer(value)) {
            Err(StageError::Invalid(
                error @ ValidationError::NotifdOutOfRange {
                    key: refused_key, ..
                },
            )) => {
                assert_eq!(refused_key, key);
                let message = error.to_string();
                assert!(
                    message.contains(&format!("{key}={value}")) && message.contains(range),
                    "the rule is named: {message}"
                );
            }
            other => panic!("{key}={value} must be refused, got {other:?}"),
        }
    }
    assert!(!store.is_dirty(), "nothing was staged");

    // Nothing to apply, so nothing is written and notifd is not called.
    let runner = MockCommandRunner::new();
    let (_, written) = expect_applied(apply_notifd(&store, &model, &runner));
    assert!(written.is_empty());
    assert!(runner.recorded().is_empty());
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn an_icon_theme_and_a_notifd_config_change_in_one_apply_reload_notifd_once() {
    // Tasks 11.14/11.15: the icon theme (read by notifd from gtk-3.0/settings.ini) and
    // the notifd config file both ask for `Reload()`; one Apply issues it once, last.
    let fx = FixtureDotfiles::install();
    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::NotifdCopyCodes, Value::Bool(false))
        .expect("the switch stages");

    let icon_theme = fx.home().join(".icons").join("Fixture-Icons");
    fs::create_dir_all(icon_theme.join("48x48")).expect("create the icon theme");
    fs::write(icon_theme.join("index.theme"), b"[Icon Theme]\n").expect("write its index");
    let mut themes = ThemesModel::load(
        &ThemeRoots {
            gtk_theme_dirs: vec![fx.home().join(".themes")],
            icon_dirs: vec![fx.home().join(".icons")],
        },
        ThemesPaths {
            gtk3_settings: fx.config_path("gtk-3.0/settings.ini"),
            gtk4_settings: fx.config_path("gtk-4.0/settings.ini"),
            hyprland_lua: Some(fx.config_path("hypr/hyprland.lua")),
            uwsm_env: fx.config_path("uwsm/env"),
            cursor_fallback_theme: Some(fx.home().join(".icons/default/index.theme")),
            dotfiles_repo: Some(fx.repo_root().to_path_buf()),
            home: Some(fx.home().to_path_buf()),
        },
        false,
        None,
    );
    themes.stage_icon_theme("Fixture-Icons");

    let assembled = assemble_apply_plan(ApplySources {
        notifications: Some(&model),
        themes: Some(&themes),
        ..ApplySources::for_store(&store)
    })
    .expect("the plan assembles");
    let runner = MockCommandRunner::new();
    let capabilities = Capabilities::for_tests(&[Binary::Gsettings], &[], false)
        .with_notifd_for_tests(notifd_running("1.1"));
    let (reload_failures, written) = expect_applied(apply::run(
        &assembled.plan,
        store.freshness(),
        &capabilities,
        &runner,
        &MockProcessSignaller::new(),
    ));
    assert!(reload_failures.is_empty());
    assert_eq!(
        written.len(),
        3,
        "notifd/config and both settings.ini files"
    );

    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("gsettings").args([
                "set",
                "org.gnome.desktop.interface",
                "icon-theme",
                "Fixture-Icons",
            ]),
            notifd_reload(),
        ],
        "one Reload, after the gsettings key"
    );
    assert!(
        fs::read_to_string(notifd_config(&fx))
            .expect("read the applied file")
            .contains("\ncopy_codes=false\n")
    );
}

#[test]
fn without_the_file_or_with_notifd_1_0_only_the_runtime_controls_are_shown() {
    // R4.2: the staged rows need notifd >= 1.1 *and* a readable file. Either missing
    // leaves the page with its runtime controls alone, and the reason is logged at
    // `info`; with both present the page offers all seven rows.
    let fx = FixtureDotfiles::install();
    let path = notifd_config(&fx);
    let hidden_reason = |events: &[CapturedEvent]| {
        events
            .iter()
            .find(|event| {
                event.level == tracing::Level::INFO
                    && event
                        .message
                        .contains("only the runtime controls are shown")
            })
            .map(|event| event.fields.clone())
    };

    let shown = notifications::offer(&caps(), path.clone());
    assert_eq!(notifications::staged_rows(shown.as_ref()).len(), 7);

    // A daemon at 1.0 (no config file support, no Reload).
    let older =
        Capabilities::for_tests(&[], &[], false).with_notifd_for_tests(notifd_running("1.0"));
    let mut model = None;
    let events = capture_logs(|| model = notifications::offer(&older, path.clone()));
    assert!(model.is_none());
    assert!(notifications::staged_rows(model.as_ref()).is_empty());
    let fields = hidden_reason(&events).expect("the hide is logged at info");
    assert!(fields.contains("running 1.0"), "{fields}");

    // A tree without the file.
    fs::remove_file(&path).expect("remove the deployed link");
    let events = capture_logs(|| model = notifications::offer(&caps(), path.clone()));
    assert!(model.is_none());
    assert!(notifications::staged_rows(model.as_ref()).is_empty());
    let fields = hidden_reason(&events).expect("the hide is logged at info");
    assert!(fields.contains("cannot be read"), "{fields}");
    assert!(
        loaders::notifd_config(&path).is_err(),
        "the store does not track a missing file"
    );
}

#[test]
fn a_key_missing_from_a_trimmed_file_shows_its_default_and_is_appended_when_changed() {
    // The shipped file lists all seven keys, but a hand-trimmed one must still work:
    // the missing key is seeded with notifd's default and, once changed, appended as a
    // bare `key=value` line at the end; nothing else moves.
    let fx = FixtureDotfiles::install();
    let path = notifd_config(&fx);
    let shipped = fs::read_to_string(&path).expect("read the fixture file");
    let trimmed = replace_once(
        &shipped,
        "# Critical popups one app may hold on screen at once (1..5, the slot count);\n\
         # its oldest makes room for the next.\n\
         critical_per_app=3\n\n",
        "",
    );
    fs::write(&path, &trimmed).expect("trim the file through the link");

    let (mut store, model) = store_and_model(&fx);
    assert_eq!(
        store.value(SettingId::NotifdCriticalPerApp),
        Some(&Value::Integer(3)),
        "the missing key shows notifd's default"
    );
    store
        .stage(SettingId::NotifdCriticalPerApp, Value::Integer(2))
        .expect("an in-range value stages");

    let runner = MockCommandRunner::new();
    let (reload_failures, _) = expect_applied(apply_notifd(&store, &model, &runner));
    assert!(reload_failures.is_empty());
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        format!("{trimmed}critical_per_app=2\n")
    );
    assert_eq!(runner.recorded(), vec![notifd_reload()]);
}

#[test]
fn an_external_edit_between_load_and_apply_aborts_as_conflicted() {
    // R5.6: the store baselines the file at load, so an edit by another program (here:
    // the user editing it by hand) aborts the Apply with nothing written or called.
    let fx = FixtureDotfiles::install();
    let path = notifd_config(&fx);
    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::NotifdHoldMin, Value::Integer(900))
        .expect("the edit stages");

    let edited = format!(
        "{}# edited by hand\n",
        fs::read_to_string(&path).expect("read")
    );
    fs::write(&path, &edited).expect("the external edit");

    let runner = MockCommandRunner::new();
    match apply_notifd(&store, &model, &runner) {
        ApplyOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path(), path.as_path());
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&path).expect("read"), edited);
    assert!(runner.recorded().is_empty());
}

#[test]
fn a_failed_reload_is_non_fatal_and_the_write_stands() {
    // R5.5: a `Reload()` call that fails (the daemon exited in the meantime) is
    // reported, but the written file stays.
    let fx = FixtureDotfiles::install();
    let path = notifd_config(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture file");
    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::NotifdTimeoutLow, Value::Integer(0))
        .expect("0 (until dismissed) stages");

    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let (reload_failures, written) = expect_applied(apply_notifd(&store, &model, &runner));
    assert_eq!(reload_failures.len(), 1, "the failed Reload is reported");
    assert_eq!(written, vec![path.clone()]);
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(&original, "timeout_low_ms=5000", "timeout_low_ms=0")
    );
    assert_eq!(runner.recorded(), vec![notifd_reload()]);
}
