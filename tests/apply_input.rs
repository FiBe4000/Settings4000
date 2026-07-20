//! End-to-end staged-edit → Apply suite for the **Input** category (task 7.2;
//! R5.3–R5.6, R6.1).
//!
//! Drives the full store-driven pipeline against the installed fixture tree
//! (task 7.1): `input.conf` is loaded into a real [`SettingsStore`] through the
//! app's own loader, edits are staged like the Input page stages them, the plan
//! is assembled by the app's real plan builder plus [`InputModel`]'s write glue,
//! and [`apply::run`] executes it with a [`MockCommandRunner`] — so the suite
//! asserts the exact resulting file bytes at the deployed (symlinked) path, the
//! exact reload command list, and that every untouched repo file stays
//! byte-identical. Per-setting byte-exact coverage lives in `core::input`'s unit
//! tests (task 6.6); this suite exercises a representative edit through real
//! files, symlinks, and the shared harness.

use std::fs;

use settings4000::core::apply::{self, ApplyOutcome};
use settings4000::core::detect::{Binary, Capabilities};
use settings4000::core::input::InputModel;
use settings4000::core::model::{Category, SettingId, Value};
use settings4000::core::store::SettingsStore;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, base_apply_plan, expect_applied,
    load_into_store, loaders, replace_once, repo_snapshot,
};

/// The deployed (live XDG) path of `input.conf` — the path the app addresses the
/// file by (R8.5), a symlink into the fixture repo.
fn input_conf(fx: &FixtureDotfiles) -> std::path::PathBuf {
    fx.config_path("hypr/input.conf")
}

/// Loads `input.conf` into a fresh store via the app's real loader and builds the
/// Input page's write model, mirroring the startup wiring (task 5.4/6.6).
///
/// The XKB registry path is deliberately nonexistent: the layout add-list is not
/// under test here, and the model degrades to an empty candidate list (R4.4).
fn store_and_model(fx: &FixtureDotfiles) -> (SettingsStore, InputModel) {
    let path = input_conf(fx);
    let mut store = SettingsStore::new();
    load_into_store(&mut store, &path, loaders::input_conf);
    let model = InputModel::load(path, std::path::Path::new("/nonexistent/evdev.xml"));
    (store, model)
}

/// An Input change reloads only via `hyprctl reload`, so the suite's capabilities
/// are `hyprctl` on `$PATH` plus a live Hyprland IPC socket (task 4.4).
fn caps() -> Capabilities {
    Capabilities::for_tests(&[Binary::Hyprctl], &[], true)
}

#[test]
fn a_staged_input_edit_applies_byte_exactly_and_reloads_hyprland() {
    // The happy path (R5.3): stage a layout reorder and a touchpad toggle, Apply,
    // and assert the exact resulting bytes, the exact reload list, write-through
    // to the repo target, and a clean second apply after commit (R5.6).
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = input_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture input.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("a layout reorder stages");
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    // Assemble the plan exactly as the window's Apply handler does: the base plan
    // (validations from the store's dirty settings) plus the Input page's single
    // surgical FileWrite.
    let mut plan = base_apply_plan(&store);
    let write = model
        .input_conf_write(&store.dirty_in_category(Category::Input))
        .expect("the write renders")
        .expect("dirty settings produce a write");
    plan.writes.push(write);

    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty(), "a clean apply has no failures");
    assert_eq!(written, vec![path.clone()], "the live path is reported");

    // (a) Exact resulting bytes: the original with ONLY the two value spans
    // changed — comments, ordering, and every other byte identical (R5.3).
    let expected = replace_once(&original, "kb_layout=us,se", "kb_layout=se,us");
    let expected = replace_once(&expected, "natural_scroll=true", "natural_scroll=false");
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        expected,
        "the apply must change exactly the two edited value spans"
    );

    // Write-through (R5.4/R8.5): the deployed path is still a symlink and the
    // repo target carries the new bytes.
    assert!(
        fs::symlink_metadata(&path)
            .expect("stat the deployed path")
            .file_type()
            .is_symlink(),
        "the deployment symlink must be preserved by the atomic writer"
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path("config/hypr/input.conf")).expect("read the repo target"),
        expected,
        "the write must land in the repo target behind the symlink"
    );

    // (b) The exact reload list: `hyprctl reload`, nothing else, no signals.
    assert_eq!(
        runner.recorded(),
        vec![Command::new("hyprctl").arg("reload")]
    );
    assert!(signaller.calls().is_empty());

    // (c) Every other repo file is byte-identical.
    assert_repo_untouched_except(&fx, &before, &["config/hypr/input.conf"]);

    // Commit as the window does (task 4.5), then a second staged edit must apply
    // without a self-conflict — the re-baseline contract (R5.6).
    let committed: Vec<(std::path::PathBuf, Vec<u8>)> = plan
        .writes
        .iter()
        .map(|write| (write.path.clone(), write.contents.clone()))
        .collect();
    store.commit_apply(&committed);
    assert!(!store.is_dirty(), "commit returns the store to clean");

    store
        .stage(SettingId::MouseSensitivity, Value::Float(0.5))
        .expect("a sensitivity edit stages");
    let mut second_plan = base_apply_plan(&store);
    second_plan.writes.push(
        model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the second write renders")
            .expect("a dirty setting produces a write"),
    );
    let outcome = apply::run(
        &second_plan,
        store.freshness(),
        &caps(),
        &runner,
        &signaller,
    );
    // `expect_applied` above is what proves the no-self-conflict property: a
    // stale baseline would have produced `Conflicted` and panicked there. This
    // assertion only checks the second apply's reload also ran cleanly.
    let (reload_failures, _) = expect_applied(outcome);
    assert!(
        reload_failures.is_empty(),
        "the second apply's reload must succeed"
    );
    let expected_second = replace_once(&expected, "sensitivity=0.3", "sensitivity=0.5");
    assert_eq!(
        fs::read_to_string(&path).expect("read after the second apply"),
        expected_second,
        "the second apply builds on the first apply's bytes"
    );
}

#[test]
fn an_external_edit_between_load_and_apply_aborts_as_conflicted() {
    // Failure injection (3), R5.6: another program edits input.conf after the store
    // loaded it. The apply must abort as Conflicted with NO writes and NO commands,
    // leaving the external edit standing.
    let fx = FixtureDotfiles::install();
    let path = input_conf(&fx);

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("the edit stages");

    // The external edit, through the deployed path (writes through to the repo).
    let externally_edited = format!(
        "{}# edited by hand while the app was open\n",
        fs::read_to_string(&path).expect("read the fixture")
    );
    fs::write(&path, &externally_edited).expect("apply the external edit");

    let mut plan = base_apply_plan(&store);
    plan.writes.push(
        model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the write renders")
            .expect("a dirty setting produces a write"),
    );

    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);

    match outcome {
        ApplyOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1, "exactly the edited file conflicts");
            assert_eq!(conflicts[0].path(), path.as_path());
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(&path).expect("read the file"),
        externally_edited,
        "the external edit must stand — nothing was written (R5.6)"
    );
    assert!(
        runner.recorded().is_empty(),
        "no command runs on a conflict"
    );
    assert!(signaller.calls().is_empty());
}

#[test]
fn a_failed_hyprctl_reload_is_non_fatal_and_the_write_stands() {
    // Failure injection (2), R5.5: `hyprctl reload` exits non-zero. The apply still
    // completes, the file keeps its new bytes, and the failure is reported.
    let fx = FixtureDotfiles::install();
    let path = input_conf(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture input.conf");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::TouchpadTapToClick, Value::Bool(false))
        .expect("the edit stages");

    let mut plan = base_apply_plan(&store);
    plan.writes.push(
        model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the write renders")
            .expect("a dirty setting produces a write"),
    );

    // The reload fails; the (already completed) write must not be rolled back.
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);

    assert_eq!(reload_failures.len(), 1, "the failed reload is reported");
    assert_eq!(written, vec![path.clone()], "the write is reported applied");
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(&original, "tap-to-click=true", "tap-to-click=false"),
        "the file keeps its new bytes despite the reload failure (R5.5)"
    );
    assert_eq!(
        runner.recorded(),
        vec![Command::new("hyprctl").arg("reload")]
    );
}
