//! End-to-end staged-edit → Apply suite for the **Notifications** category
//! (task 7.2; R5.3–R5.6, R6.1).
//!
//! Drives the store-driven pipeline against the installed fixture tree (task
//! 7.1): swaync's `config.json` is loaded through the app's own loader, the
//! position/timeout edits are staged like the Notifications page stages them
//! (the combined position token decomposing to `positionY`+`positionX` on
//! write, task 6.7), and [`apply::run`] executes the plan with mocks — so the
//! suite asserts the exact resulting JSON bytes (stable key order) at the
//! deployed path and the exact `swaync-client -rs` reload. It also hosts the
//! shared **rollback-on-write-failure** injection (R5.4): a two-category plan
//! (Input + Notifications) whose second write fails mid-plan must restore the
//! already-written first file byte-exactly. (Do Not Disturb is runtime-only
//! daemon state and never reaches this pipeline — task 6.7.)

use std::fs;
use std::os::unix::fs::PermissionsExt;

use settings4000::core::apply::{self, ApplyOutcome, WriteFailureCause};
use settings4000::core::detect::{Binary, Capabilities, Daemon};
use settings4000::core::input::InputModel;
use settings4000::core::model::{Category, SettingId, Value};
use settings4000::core::notifications::NotificationsModel;
use settings4000::core::store::SettingsStore;
use settings4000::system::command::{Command, MockCommandRunner};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, base_apply_plan, expect_applied,
    load_into_store, loaders, replace_once, repo_snapshot,
};

/// The deployed (live XDG) path of swaync's `config.json` (R8.5).
fn swaync_config(fx: &FixtureDotfiles) -> std::path::PathBuf {
    fx.config_path("swaync/config.json")
}

/// Restores a fixture directory to writable (`0o755`) on drop.
///
/// The rollback test makes a repo directory read-only as its failure injection;
/// that must be undone even when an assertion panics mid-test, or the
/// [`FixtureDotfiles`] `TempDir` cannot delete the directory's contents on drop
/// and the temp tree leaks. An RAII guard runs on unwind too, which a
/// restore statement at the end of the test would not.
struct RestoreWritable<'a> {
    /// The directory whose permissions are restored on drop.
    dir: &'a std::path::Path,
}

impl Drop for RestoreWritable<'_> {
    fn drop(&mut self) {
        // Best-effort: an error here would only mask the original panic, and the
        // worst outcome of a failed restore is a leaked temp directory.
        let _ = fs::set_permissions(self.dir, fs::Permissions::from_mode(0o755));
    }
}

/// A Notifications change reloads via `swaync-client -rs`, gated on the swaync
/// daemon being live (task 4.4).
fn caps() -> Capabilities {
    Capabilities::for_tests(&[Binary::Swaync], &[Daemon::Swaync], false)
}

#[test]
fn position_and_timeout_apply_byte_exactly_and_reload_swaync() {
    // The happy path (R5.3): stage a position move and a timeout change, Apply, and
    // assert the exact JSON bytes — the position decomposed into its two on-disk
    // keys, the key order untouched — plus the exact reload list.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = swaync_config(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture config.json");

    let mut store = SettingsStore::new();
    load_into_store(&mut store, &path, loaders::swaync_config);
    let model = NotificationsModel::load(path.clone());

    store
        .stage(
            SettingId::NotificationPosition,
            Value::Enum("bottom-left".to_string()),
        )
        .expect("a position move stages");
    store
        .stage(SettingId::NotificationTimeout, Value::Integer(5))
        .expect("a timeout change stages");

    let mut plan = base_apply_plan(&store);
    plan.writes.push(
        model
            .swaync_config_write(&store.dirty_in_category(Category::Notifications))
            .expect("the write renders")
            .expect("dirty settings produce a write"),
    );

    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.clone()]);

    // (a) Exact bytes: the combined `bottom-left` token split back into swaync's
    // two keys, the timeout as a bare integer, every other key and the key order
    // byte-identical (the task-3.4 `preserve_order` contract).
    let expected = replace_once(
        &original,
        "\"positionX\": \"right\"",
        "\"positionX\": \"left\"",
    );
    let expected = replace_once(
        &expected,
        "\"positionY\": \"top\"",
        "\"positionY\": \"bottom\"",
    );
    let expected = replace_once(&expected, "\"timeout\": 10", "\"timeout\": 5");
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        expected,
        "only the three edited JSON values may change, with key order preserved"
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path("config/swaync/config.json"))
            .expect("read the repo target"),
        expected,
        "the write must land in the repo target behind the deployment symlink"
    );

    // (b) The exact reload list; DND is runtime-only, so no other command appears.
    assert_eq!(
        runner.recorded(),
        vec![Command::new("swaync-client").arg("-rs")]
    );
    assert!(signaller.calls().is_empty());

    // (c) Every other repo file is byte-identical.
    assert_repo_untouched_except(&fx, &before, &["config/swaync/config.json"]);
}

#[test]
fn a_mid_plan_write_failure_rolls_back_the_earlier_category_write() {
    // Failure injection (1), R5.4: a two-category plan — the Input write first, the
    // Notifications write second, in the window's fold order — where the second
    // write fails. The already-written input.conf must be restored to its pre-apply
    // bytes, the failure surfaced, and no reload attempted.
    //
    // The injection: the repo's swaync/ directory is made read-only, so the atomic
    // writer cannot create its temp file beside the resolved config.json target.
    // The file itself stays readable, so the step-2 conflict check (which re-reads,
    // R5.6) still passes and the pipeline genuinely reaches the write phase.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let input_path = fx.config_path("hypr/input.conf");
    let swaync_path = swaync_config(&fx);

    let mut store = SettingsStore::new();
    load_into_store(&mut store, &input_path, loaders::input_conf);
    load_into_store(&mut store, &swaync_path, loaders::swaync_config);
    let input_model = InputModel::load(
        input_path.clone(),
        std::path::Path::new("/nonexistent/evdev.xml"),
    );
    let notifications_model = NotificationsModel::load(swaync_path.clone());

    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("the Input edit stages");
    store
        .stage(SettingId::NotificationTimeout, Value::Integer(5))
        .expect("the Notifications edit stages");

    let mut plan = base_apply_plan(&store);
    plan.writes.push(
        input_model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the input write renders")
            .expect("a dirty setting produces a write"),
    );
    plan.writes.push(
        notifications_model
            .swaync_config_write(&store.dirty_in_category(Category::Notifications))
            .expect("the swaync write renders")
            .expect("a dirty setting produces a write"),
    );

    // Make the second write's directory read-only, with an RAII guard so the
    // permissions are restored even when an assertion below panics (otherwise
    // the fixture's TempDir could not clean the directory up).
    let swaync_dir = fx.repo_path("config/swaync");
    fs::set_permissions(&swaync_dir, fs::Permissions::from_mode(0o555))
        .expect("make the swaync directory read-only");
    let _restore = RestoreWritable { dir: &swaync_dir };

    // Guard: running as root bypasses directory modes, so the injection only
    // works when the directory is genuinely non-writable (probed, mirroring the
    // writer tests). Skipping must be visible in the test output — a silent
    // "ok" here would leave R5.4 with no end-to-end coverage and no signal.
    if fs::File::create(swaync_dir.join(".probe")).is_ok() {
        eprintln!(
            "skipped: directory modes are not enforced (running as root), \
             so the write-failure injection cannot work"
        );
        return;
    }

    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    assert!(
        runner.recorded().is_empty(),
        "a write-phase failure must run no reload (R5.4)"
    );
    assert!(signaller.calls().is_empty());

    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            match &failure.cause {
                WriteFailureCause::File { path, .. } => assert_eq!(
                    path, &swaync_path,
                    "the failure names the write that could not complete"
                ),
                other => panic!("expected a File write failure, got {other:?}"),
            }
            assert_eq!(
                failure.rolled_back,
                vec![fx.repo_path("config/hypr/input.conf")],
                "the already-written input.conf (resolved to its repo target) is rolled back"
            );
            assert!(failure.rollback_failures.is_empty());
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }

    // The rollback restored the desktop exactly as it was: the whole repo tree is
    // byte-identical to the pre-apply snapshot (R5.4).
    assert_repo_untouched_except(&fx, &before, &[]);
}
