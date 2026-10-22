//! End-to-end staged-edit → Apply suite for the **Power & Idle** category
//! (task 7.2; R5.3–R5.6, R6.1).
//!
//! Drives the store-driven pipeline against the installed fixture tree (task
//! 7.1): `hypridle.conf` is loaded through the app's own loader, timeout /
//! lock-command / lock-grace edits are staged like the Power & Idle page stages
//! them (positional `listener` addressing, task 6.8; the `--grace N` token of
//! `general.lock_cmd`, task 11.13), and [`apply::run`] executes
//! the plan with mocks — so the suite asserts the exact resulting bytes at the
//! deployed path (one listener's value changed, the other listener blocks and
//! their inline comments byte-identical) and the exact hypridle-restart command
//! sequence, for both restart mechanisms (the systemd-unit path and the
//! SIGTERM + `setsid --fork` respawn fallback, task 4.4).

use std::fs;

use nix::sys::signal::Signal;

use settings4000::core::apply::{self, ApplyPlan};
use settings4000::core::detect::{Capabilities, Daemon};
use settings4000::core::model::{Category, SettingId, Value};
use settings4000::core::power::PowerModel;
use settings4000::core::store::SettingsStore;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::{MockProcessSignaller, SignalCall};
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, base_apply_plan, expect_applied,
    load_into_store, loaders, replace_once, repo_snapshot,
};

/// The fixture's `lock_cmd` assignment, verbatim from the real dotfiles (commit
/// `8a5b20b`): hyprlock behind a `pidof` guard, with a 10-second grace period.
const REAL_LOCK_CMD_LINE: &str = "lock_cmd = pidof hyprlock || hyprlock --grace 10";

/// The deployed (live XDG) path of `hypridle.conf` (R8.5).
fn hypridle_conf(fx: &FixtureDotfiles) -> std::path::PathBuf {
    fx.config_path("hypr/hypridle.conf")
}

/// Loads `hypridle.conf` into a fresh store via the app's real loader and builds
/// the Power & Idle write model, mirroring the startup wiring (task 5.4/6.8).
fn store_and_model(fx: &FixtureDotfiles) -> (SettingsStore, PowerModel) {
    let path = hypridle_conf(fx);
    let mut store = SettingsStore::new();
    load_into_store(&mut store, &path, loaders::hypridle_conf);
    let model = PowerModel::load(path);
    (store, model)
}

/// A Power & Idle change restarts hypridle, gated on the daemon being live; no
/// binary capability is involved in that reload (task 4.4).
fn caps() -> Capabilities {
    Capabilities::for_tests(&[], &[Daemon::Hypridle], false)
}

/// Builds the plan the window's Apply handler would: the base plan plus the one
/// `hypridle.conf` write rendered from the store's dirty Power & Idle settings.
fn plan_with_power_write(store: &SettingsStore, model: &PowerModel) -> ApplyPlan {
    let mut plan = base_apply_plan(store);
    plan.writes.push(
        model
            .apply_contribution(&store.dirty_in_category(Category::PowerAndIdle))
            .expect("the write renders")
            .expect("dirty settings produce a write"),
    );
    plan
}

#[test]
fn a_lock_timeout_edit_touches_only_its_listener() {
    // The happy path (R5.3): editing the lock timeout rewrites only listener[1]'s
    // `timeout` value span — the dim/DPMS listeners, every inline comment, and all
    // other bytes stay identical — and hypridle is restarted through its active
    // systemd user unit (`is-active` succeeds, so `restart` follows).
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = hypridle_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture hypridle.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::LockTimeout, Value::Integer(600))
        .expect("a lock-timeout edit stages");

    let plan = plan_with_power_write(&store, &model);
    // MockCommandRunner answers success by default, so `systemctl --user is-active`
    // reports an active unit and the systemd restart path is taken.
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.clone()]);

    // (a) Exact bytes: only the lock listener's timeout value changed. The dim
    // (150) and DPMS (330) listeners appear verbatim, proven by whole-file byte
    // equality against the original with the single span patched.
    let expected = replace_once(&original, "timeout = 300", "timeout = 600");
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        expected,
        "only listener[1].timeout may change; all other listener blocks byte-identical"
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path("config/hypr/hypridle.conf"))
            .expect("read the repo target"),
        expected,
        "the write must land in the repo target behind the deployment symlink"
    );

    // (b) The exact restart sequence: probe the unit, then restart it. No signal
    // is delivered on the systemd path.
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
            Command::new("systemctl").args(["--user", "restart", "hypridle"]),
        ]
    );
    assert!(signaller.calls().is_empty());

    // (c) Every other repo file is byte-identical.
    assert_repo_untouched_except(&fx, &before, &["config/hypr/hypridle.conf"]);
}

#[test]
fn a_lock_command_edit_restarts_via_the_respawn_fallback() {
    // The other restart mechanism (task 4.4): no active systemd unit (`is-active`
    // exits non-zero — the dotfiles' exec-once case), so hypridle is SIGTERMed and
    // respawned with a detached `setsid --fork`. Also covers the lock-command edit,
    // which targets `general.lock_cmd` (not a listener's on-timeout). The new
    // command carries no grace flag, so the store also stages a grace of 0 (task
    // 11.13), which the writer finds already satisfied: the line is the typed text.
    let fx = FixtureDotfiles::install();
    let path = hypridle_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture hypridle.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::LockCommand,
            Value::String("hyprlock --no-fade-in".to_string()),
        )
        .expect("a lock-command edit stages");
    assert_eq!(
        store.value(SettingId::LockGracePeriod),
        Some(&Value::Integer(0)),
        "the grace control follows the typed command"
    );

    let plan = plan_with_power_write(&store, &model);
    // First command (`is-active`) fails -> the fallback path; the respawn succeeds.
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::with_running([("hypridle".to_string(), vec![4242])]);
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, _) = expect_applied(outcome);
    assert!(reload_failures.is_empty());

    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(
            &original,
            REAL_LOCK_CMD_LINE,
            "lock_cmd = hyprlock --no-fade-in"
        ),
        "only general.lock_cmd may change"
    );

    // The exact fallback sequence: the unit probe, then the DETACHED respawn (the
    // detached flag participates in Command equality, pinning the no-capture mode),
    // with the SIGTERM delivered between them through the signaller seam.
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
            Command::new("setsid")
                .args(["--fork", "hypridle"])
                .detached(),
        ]
    );
    assert_eq!(
        signaller.calls(),
        vec![SignalCall {
            process_name: "hypridle".to_string(),
            signal: Signal::SIGTERM,
            pids: vec![4242],
        }]
    );
}

#[test]
fn a_failed_hypridle_restart_leaves_the_write_standing() {
    // Failure injection (2), R5.5: the systemd restart itself fails after the write
    // succeeded. The apply still completes, the new bytes stand, and the failure is
    // reported for the UI to toast.
    let fx = FixtureDotfiles::install();
    let path = hypridle_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture hypridle.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::DimTimeout, Value::Integer(200))
        .expect("a dim-timeout edit stages");

    let plan = plan_with_power_write(&store, &model);
    // `is-active` succeeds (active unit), then `restart` exits non-zero.
    let runner =
        MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(0)), Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);

    assert_eq!(reload_failures.len(), 1, "the failed restart is reported");
    assert_eq!(written, vec![path.clone()]);
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(&original, "timeout = 150", "timeout = 200"),
        "the file keeps its new bytes despite the restart failure (R5.5)"
    );
}

/// The exact systemd restart sequence a Power & Idle Apply records: probe the
/// unit, then restart it — once, however many settings of the file changed.
fn systemd_restart() -> Vec<Command> {
    vec![
        Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
        Command::new("systemctl").args(["--user", "restart", "hypridle"]),
    ]
}

/// Stages `edits` in a fresh store over the installed fixture, applies them with
/// the systemd restart path, and returns the fixture (to inspect the tree), the
/// original and resulting `hypridle.conf` text, and the recorded commands.
fn apply_power_edits(
    prepare: impl FnOnce(&FixtureDotfiles),
    edits: &[(SettingId, Value)],
) -> (FixtureDotfiles, String, String, Vec<Command>) {
    let fx = FixtureDotfiles::install();
    prepare(&fx);
    let path = hypridle_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture hypridle.conf");

    let (mut store, model) = store_and_model(&fx);
    for (id, value) in edits {
        store
            .stage(*id, value.clone())
            .unwrap_or_else(|error| panic!("{id:?} = {value:?} must stage: {error}"));
    }
    let plan = plan_with_power_write(&store, &model);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.clone()]);
    assert!(signaller.calls().is_empty());

    let applied = fs::read_to_string(&path).expect("read the applied file");
    (fx, original, applied, runner.recorded())
}

#[test]
fn a_grace_edit_changes_only_the_number_in_the_fixture_lock_cmd() {
    // Change (task 11.13): `--grace 10` -> `--grace 30` on the real line. Whole-file
    // equality against the original with that one token patched proves the
    // `pidof hyprlock ||` guard, the explanatory comments above it and every other
    // line (dual-form dpms commands, idle-dim calls, listener comments) are intact.
    let before_fx = FixtureDotfiles::install();
    let before = repo_snapshot(&before_fx);
    drop(before_fx);

    let (fx, original, applied, recorded) =
        apply_power_edits(|_| {}, &[(SettingId::LockGracePeriod, Value::Integer(30))]);
    assert_eq!(
        applied,
        replace_once(
            &original,
            REAL_LOCK_CMD_LINE,
            "lock_cmd = pidof hyprlock || hyprlock --grace 30",
        )
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path("config/hypr/hypridle.conf"))
            .expect("read the repo target"),
        applied,
        "the write lands in the repo target behind the deployment symlink"
    );
    assert_eq!(recorded, systemd_restart());
    assert_repo_untouched_except(&fx, &before, &["config/hypr/hypridle.conf"]);
}

#[test]
fn a_grace_edit_inserts_the_flag_into_a_lock_cmd_without_one() {
    // Insert: the deployed lock command has no `--grace` (the shape before the
    // dotfiles adopted one), so ` --grace 15` goes right after the `hyprlock` word.
    let (_fx, original, applied, recorded) = apply_power_edits(
        |fx| {
            let path = fx.repo_path("config/hypr/hypridle.conf");
            let text = fs::read_to_string(&path).expect("read the repo hypridle.conf");
            fs::write(
                &path,
                replace_once(
                    &text,
                    REAL_LOCK_CMD_LINE,
                    "lock_cmd = pidof hyprlock || hyprlock",
                ),
            )
            .expect("drop the grace flag from the fixture");
        },
        &[(SettingId::LockGracePeriod, Value::Integer(15))],
    );
    assert_eq!(
        applied,
        replace_once(
            &original,
            "lock_cmd = pidof hyprlock || hyprlock\n",
            "lock_cmd = pidof hyprlock || hyprlock --grace 15\n",
        )
    );
    assert_eq!(recorded, systemd_restart());
}

#[test]
fn a_zero_grace_removes_the_flag_from_the_fixture_lock_cmd() {
    // Remove: 0 is hyprlock's default, so the ` --grace 10` token is dropped rather
    // than rewritten as `--grace 0`.
    let (_fx, original, applied, recorded) =
        apply_power_edits(|_| {}, &[(SettingId::LockGracePeriod, Value::Integer(0))]);
    assert_eq!(
        applied,
        replace_once(
            &original,
            REAL_LOCK_CMD_LINE,
            "lock_cmd = pidof hyprlock || hyprlock",
        )
    );
    assert_eq!(recorded, systemd_restart());
}

#[test]
fn a_combined_timeout_and_grace_apply_restarts_hypridle_once() {
    // Two settings of the one file change — the lock listener's timeout and the
    // grace token — yet the reload planner dedups by file: exactly one probe and
    // one restart.
    let (_fx, original, applied, recorded) = apply_power_edits(
        |_| {},
        &[
            (SettingId::LockTimeout, Value::Integer(600)),
            (SettingId::LockGracePeriod, Value::Integer(5)),
        ],
    );
    let expected = replace_once(&original, "timeout = 300", "timeout = 600");
    let expected = replace_once(
        &expected,
        REAL_LOCK_CMD_LINE,
        "lock_cmd = pidof hyprlock || hyprlock --grace 5",
    );
    assert_eq!(applied, expected);
    assert_eq!(
        recorded,
        systemd_restart(),
        "hypridle is restarted exactly once for the combined Apply"
    );
}

#[test]
fn a_lock_command_and_grace_edit_in_one_apply_keep_both() {
    // The user retypes the lock command, then sets the grace control: the store
    // rewrites the staged command with the token, and the writer writes the new
    // command with its grace — the command first, the token on top (task 11.13).
    let (_fx, original, applied, recorded) = apply_power_edits(
        |_| {},
        &[
            (
                SettingId::LockCommand,
                Value::String("hyprlock --no-fade-in".to_string()),
            ),
            (SettingId::LockGracePeriod, Value::Integer(20)),
        ],
    );
    assert_eq!(
        applied,
        replace_once(
            &original,
            REAL_LOCK_CMD_LINE,
            "lock_cmd = hyprlock --grace 20 --no-fade-in",
        )
    );
    assert_eq!(recorded, systemd_restart());
}

#[test]
fn the_fixture_lock_cmd_seeds_a_ten_second_grace() {
    // Read side: the loader derives the grace control from the real `lock_cmd`.
    let fx = FixtureDotfiles::install();
    let (store, _model) = store_and_model(&fx);
    assert_eq!(
        store.value(SettingId::LockGracePeriod),
        Some(&Value::Integer(10))
    );
    assert_eq!(
        store.value(SettingId::LockCommand),
        Some(&Value::String(
            "pidof hyprlock || hyprlock --grace 10".to_string()
        ))
    );
}

#[test]
fn a_doubled_general_block_is_noted_but_applies_and_restarts_as_usual() {
    // Task 9.31: a hand-edited hypridle.conf that declares `general { }` a second
    // time. The Power & Idle model notes it for the lock rows, and the note is all
    // that changes: a timeout + grace Apply rewrites the same two value spans in
    // the first `general` block and the lock listener, leaves the second block
    // byte-identical, and restarts hypridle once, exactly as on the clean file.
    let second_general = "\ngeneral {\n    before_sleep_cmd = loginctl lock-session\n}\n";
    let (fx, original, applied, recorded) = apply_power_edits(
        |fx| {
            let path = fx.repo_path("config/hypr/hypridle.conf");
            let text = fs::read_to_string(&path).expect("read the repo hypridle.conf");
            fs::write(&path, format!("{text}{second_general}")).expect("add a second general");
        },
        &[
            (SettingId::LockTimeout, Value::Integer(600)),
            (SettingId::LockGracePeriod, Value::Integer(30)),
        ],
    );
    let expected = replace_once(
        &replace_once(&original, "timeout = 300", "timeout = 600"),
        REAL_LOCK_CMD_LINE,
        "lock_cmd = pidof hyprlock || hyprlock --grace 30",
    );
    assert_eq!(applied, expected);
    assert!(applied.ends_with(second_general));
    assert_eq!(recorded, systemd_restart());

    // The note the page shows: once, for the two settings in `general`, naming
    // both header lines.
    let second_line = original
        .lines()
        .enumerate()
        .filter(|(_, line)| *line == "general {")
        .map(|(index, _)| index + 1)
        .last()
        .expect("the second general block");
    let model = PowerModel::load(hypridle_conf(&fx));
    let notes = model.setting_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(
        notes[0].settings,
        vec![SettingId::LockCommand, SettingId::LockGracePeriod]
    );
    assert!(
        notes[0].text.starts_with(&format!(
            "In hypridle.conf, `general` is declared twice (lines 1 and {second_line})."
        )),
        "{}",
        notes[0].text
    );
}

#[test]
fn the_fixture_hypridle_conf_has_no_note() {
    // The real file declares `general` once; its three listeners are positional by
    // design and never noted (task 9.31).
    let fx = FixtureDotfiles::install();
    assert!(
        PowerModel::load(hypridle_conf(&fx))
            .setting_notes()
            .is_empty()
    );
}
