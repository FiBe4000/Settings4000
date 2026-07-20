//! End-to-end staged-edit → Apply suite for the **Power & Idle** category
//! (task 7.2; R5.3–R5.6, R6.1).
//!
//! Drives the store-driven pipeline against the installed fixture tree (task
//! 7.1): `hypridle.conf` is loaded through the app's own loader, timeout /
//! lock-command edits are staged like the Power & Idle page stages them
//! (positional `listener` addressing, task 6.8), and [`apply::run`] executes
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
            .hypridle_conf_write(&store.dirty_in_category(Category::PowerAndIdle))
            .expect("the write renders")
            .expect("dirty settings produce a write"),
    );
    plan
}

#[test]
fn a_lock_timeout_edit_touches_one_listener_and_restarts_via_systemd() {
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
fn a_lock_command_edit_restarts_via_the_kill_and_respawn_fallback() {
    // The other restart mechanism (task 4.4): no active systemd unit (`is-active`
    // exits non-zero — the dotfiles' exec-once case), so hypridle is SIGTERMed and
    // respawned with a detached `setsid --fork`. Also covers the lock-command edit,
    // which targets `general.lock_cmd` (not a listener's on-timeout).
    let fx = FixtureDotfiles::install();
    let path = hypridle_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture hypridle.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::LockCommand,
            Value::String("hyprlock --immediate".to_string()),
        )
        .expect("a lock-command edit stages");

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
            "lock_cmd = pidof hyprlock || hyprlock",
            "lock_cmd = hyprlock --immediate",
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
fn a_failed_hypridle_restart_is_non_fatal_and_the_write_stands() {
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
