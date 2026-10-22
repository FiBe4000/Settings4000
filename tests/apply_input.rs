//! End-to-end staged-edit → Apply suite for the **Input** category (tasks 7.2,
//! 11.4; R4.2, R5.3–R5.6, R6.1, R8.3).
//!
//! Drives the full store-driven pipeline against the installed fixture tree
//! (task 7.1): `input.lua` — the app-owned `hl.config({ input = { … } })` table
//! that `hyprland.lua` loads with `dofile` (analysis §7.1) — is loaded into a real
//! [`SettingsStore`] through the app's own loader, edits are staged like the Input
//! page stages them, the plan is assembled by the app's real plan builder plus
//! [`InputModel`]'s write glue, and [`apply::run`] executes it with a
//! [`MockCommandRunner`] — so the suite asserts the exact resulting file bytes at
//! the deployed (symlinked) path, the exact reload command list (`hyprctl reload
//! config-only`), and that every untouched repo file stays byte-identical.
//! Per-setting byte-exact coverage lives in `core::input`'s unit tests; this suite
//! exercises representative edits through real files, symlinks, and the shared
//! harness, plus the two ways the page refuses: hiding for a file it does not fully
//! understand, and aborting an Apply whose edit the Lua-literal editor refuses.
//!
//! Two cases here reach beyond the Input page on purpose: an Input apply made while
//! *another* page's backing file is missing, which is where the conflict check's
//! write-target scoping is observable end to end (task 9.11), and the shared
//! **rollback-on-write-failure** injection (R5.4) — an Input write followed by a
//! Theme write that fails, which must restore the already-written `input.lua`
//! byte-exactly.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use settings4000::core::apply::{self, ApplyOutcome, WriteFailureCause};
use settings4000::core::assemble::{
    ApplySources, FailedWrite, PrepareFailure, assemble_apply_plan,
};
use settings4000::core::detect::{Binary, Capabilities};
use settings4000::core::input::{InputLuaUnavailable, InputModel};
use settings4000::core::model::{Category, SettingId, Value};
use settings4000::core::power::PowerModel;
use settings4000::core::store::SettingsStore;
use settings4000::core::theme::{ThemeRoots, ThemesModel, ThemesPaths};
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, base_apply_plan, differing_lines,
    expect_applied, load_into_store, loaders, replace_once, repo_snapshot,
};

/// The repo-relative path of `input.lua`, for the untouched-repo assertions.
const REPO_INPUT_LUA: &str = "config/hypr/input.lua";

/// The deployed (live XDG) path of `input.lua` — the path the app addresses the
/// file by (R8.5), a symlink into the fixture repo.
fn input_lua(fx: &FixtureDotfiles) -> PathBuf {
    fx.config_path("hypr/input.lua")
}

/// A nonexistent XKB registry: the layout add-list is not under test here, and the
/// model degrades to an empty candidate list (R4.4).
fn no_xkb() -> &'static Path {
    Path::new("/nonexistent/evdev.xml")
}

/// Loads `input.lua` into a fresh store via the app's real loader and builds the
/// Input page's write model, mirroring the startup wiring (tasks 5.4, 6.6, 11.4).
fn store_and_model(fx: &FixtureDotfiles) -> (SettingsStore, InputModel) {
    let path = input_lua(fx);
    let mut store = SettingsStore::new();
    load_into_store(&mut store, &path, loaders::input_lua);
    let model =
        InputModel::load(path, no_xkb()).expect("the fixture input.lua backs the Input page");
    (store, model)
}

/// An Input change reloads only via `hyprctl reload config-only`, so the suite's
/// capabilities are `hyprctl` on `$PATH` plus a live Hyprland IPC socket (task 4.4).
fn caps() -> Capabilities {
    Capabilities::for_tests(&[Binary::Hyprctl], &[], true)
}

/// The one reload an `input.lua` write triggers (task 11.4, architecture §6).
fn config_only_reload() -> Command {
    Command::new("hyprctl").arg("reload").arg("config-only")
}

/// Stages the store's dirty Input settings into a plan exactly as the window's Apply
/// handler does: the base plan (validations from the store's dirty settings) plus the
/// Input page's single surgical write.
fn plan_with_input_write(
    store: &SettingsStore,
    model: &InputModel,
) -> settings4000::core::apply::ApplyPlan {
    let mut plan = base_apply_plan(store);
    plan.writes.push(
        model
            .apply_contribution(&store.dirty_in_category(Category::Input))
            .expect("the write renders")
            .expect("dirty settings produce a write"),
    );
    plan
}

#[test]
fn an_input_apply_rewrites_exactly_two_literals() {
    // The task-11.4 acceptance (R5.3): stage natural scrolling off and a third
    // keyboard layout, Apply, and assert that exactly the two literal spans changed
    // in input.lua — every other line byte-identical through the deployed symlink —
    // and that the only command run is `hyprctl reload config-only`. Then a second
    // apply after commit must not self-conflict (R5.6).
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = input_lua(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture input.lua");

    let (mut store, model) = store_and_model(&fx);
    assert_eq!(
        store.value(SettingId::KeyboardLayouts),
        Some(&Value::String("us,se".to_string())),
        "the fixture's layout list loads from its Lua literal"
    );
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("us,se,de".to_string()),
        )
        .expect("a third layout stages");
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    let plan = plan_with_input_write(&store, &model);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty(), "a clean apply has no failures");
    assert_eq!(written, vec![path.clone()], "the live path is reported");

    // Exactly the two literal spans changed; nothing else moved (byte diff).
    let applied = fs::read_to_string(&path).expect("read the applied file");
    let layout_line = original
        .lines()
        .position(|line| line.trim() == "kb_layout = \"us,se\",")
        .expect("the fixture declares kb_layout");
    let scroll_line = original
        .lines()
        .position(|line| line.trim() == "natural_scroll = true,")
        .expect("the fixture declares natural_scroll");
    assert_eq!(
        differing_lines(&original, &applied),
        vec![layout_line, scroll_line],
        "exactly the two edited lines differ"
    );
    let expected = replace_once(&original, "\"us,se\"", "\"us,se,de\"");
    let expected = replace_once(
        &expected,
        "natural_scroll = true,",
        "natural_scroll = false,",
    );
    assert_eq!(
        applied, expected,
        "only the literals changed — key, spacing, separator and line ending stay"
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
        fs::read_to_string(fx.repo_path(REPO_INPUT_LUA)).expect("read the repo target"),
        expected,
        "the write must land in the repo target behind the symlink"
    );

    // The exact reload list: `hyprctl reload config-only`, nothing else, no signals.
    assert_eq!(runner.recorded(), vec![config_only_reload()]);
    assert!(signaller.calls().is_empty());

    // Every other repo file is byte-identical.
    assert_repo_untouched_except(&fx, &before, &[REPO_INPUT_LUA]);

    // Commit as the window does (task 4.5), then a second staged edit must apply
    // without a self-conflict — the re-baseline contract (R5.6).
    let committed: Vec<(PathBuf, Vec<u8>)> = plan
        .writes
        .iter()
        .map(|write| (write.path.clone(), write.contents.clone()))
        .collect();
    store.commit_apply(&committed);
    assert!(!store.is_dirty(), "commit returns the store to clean");

    store
        .stage(SettingId::MouseSensitivity, Value::Float(0.5))
        .expect("a sensitivity edit stages");
    let second_plan = plan_with_input_write(&store, &model);
    let outcome = apply::run(
        &second_plan,
        store.freshness(),
        &caps(),
        &runner,
        &signaller,
    );
    // `expect_applied` is what proves the no-self-conflict property: a stale
    // baseline would have produced `Conflicted` and panicked there.
    let (reload_failures, _) = expect_applied(outcome);
    assert!(
        reload_failures.is_empty(),
        "the second apply's reload must succeed"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read after the second apply"),
        replace_once(&expected, "sensitivity = 0.3,", "sensitivity = 0.5,"),
        "the second apply builds on the first apply's bytes"
    );
}

#[test]
fn a_first_keyboard_option_inserts_a_kb_options_field() {
    // The insert path through real files: a hand-written input.lua that never set
    // `kb_options` loads an empty seeded value; switching on a curated option inserts
    // one `kb_options = "…",` field before the `input` table's closing brace, indented
    // like its siblings, and leaves every original line in place.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = input_lua(&fx);
    let fixture = fs::read_to_string(&path).expect("read the fixture input.lua");
    let options_line = fixture
        .lines()
        .find(|line| line.trim_start().starts_with("kb_options = "))
        .expect("the fixture declares kb_options")
        .to_string();
    let without_options = fixture.replace(&format!("{options_line}\n"), "");
    // Prepared before the store loads, so it is the baseline rather than a conflict.
    fs::write(&path, &without_options).expect("write the hand-written input.lua");

    let (mut store, model) = store_and_model(&fx);
    assert_eq!(
        store.value(SettingId::KeyboardOptions),
        Some(&Value::String(String::new())),
        "an absent kb_options is seeded empty"
    );
    store
        .stage(
            SettingId::KeyboardOptions,
            Value::String("caps:escape".to_string()),
        )
        .expect("a first option stages");

    let plan = plan_with_input_write(&store, &model);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    expect_applied(apply::run(
        &plan,
        store.freshness(),
        &caps(),
        &runner,
        &signaller,
    ));

    let applied = fs::read_to_string(&path).expect("read the applied file");
    let expected = replace_once(
        &without_options,
        "            scroll_factor = 1.0,\n        },\n    },\n",
        "            scroll_factor = 1.0,\n        },\n        kb_options = \"caps:escape\",\n    },\n",
    );
    assert_eq!(
        applied, expected,
        "one field is inserted, nothing else moves"
    );
    assert_eq!(runner.recorded(), vec![config_only_reload()]);
    assert_repo_untouched_except(&fx, &before, &[REPO_INPUT_LUA]);
}

#[test]
fn an_opaque_line_in_the_input_table_hides_the_page() {
    // Architecture §10 / R4.2: a computed value inside the `input` table means the
    // literals around it may not be the values in effect. The store loader skips the
    // file and no Input helper is built, so the Input page is hidden (the startup-level
    // visibility and logging are covered in `ui::startup`'s tests).
    let fx = FixtureDotfiles::install();
    let path = input_lua(&fx);
    let fixture = fs::read_to_string(&path).expect("read the fixture input.lua");
    let edited = replace_once(
        &fixture,
        "follow_mouse = 1,",
        "follow_mouse = os.getenv(\"DOCKED\") and 0 or 1,",
    );
    fs::write(&path, &edited).expect("write the opaque line");
    let opaque_line = edited
        .lines()
        .position(|line| line.contains("os.getenv"))
        .expect("the opaque line is in the file")
        + 1;

    match InputModel::load(path.clone(), no_xkb()) {
        Err(InputLuaUnavailable::OpaqueLines(lines)) => assert_eq!(lines, vec![opaque_line]),
        Err(other) => panic!("expected OpaqueLines, got {other:?}"),
        Ok(_) => panic!("an opaque input table must not back the Input page"),
    }
    let error = loaders::input_lua(&path).expect_err("the store loader skips the file");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        !error.to_string().contains("getenv"),
        "the reason names lines, never contents (R7.3): {error}"
    );
}

#[test]
fn a_kind_mismatch_refusal_aborts_the_input_apply() {
    // R8.3: input.lua was hand-edited after the app loaded it so that `natural_scroll`
    // now holds a number. The Lua-literal editor never writes a boolean over it, so
    // preparing the Apply fails, names the Input page and input.lua, writes nothing
    // and keeps the staged edit for a retry.
    let fx = FixtureDotfiles::install();
    let path = input_lua(&fx);
    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    let fixture = fs::read_to_string(&path).expect("read the fixture input.lua");
    let retyped = replace_once(&fixture, "natural_scroll = true,", "natural_scroll = 1,");
    fs::write(fx.repo_path(REPO_INPUT_LUA), &retyped).expect("the hand edit");
    let after_edit = repo_snapshot(&fx);

    let failure = assemble_apply_plan(ApplySources {
        input: Some(&model),
        ..ApplySources::for_store(&store)
    })
    .expect_err("a refused Input edit must abort the whole apply");
    match failure {
        PrepareFailure::Write { source, message } => {
            assert_eq!(
                source,
                FailedWrite::Input,
                "the failure names the Input page"
            );
            assert!(
                message.contains("input.lua"),
                "the dialog's message names input.lua: {message}"
            );
            assert!(
                message.contains("number") && message.contains("boolean"),
                "the message says why: {message}"
            );
        }
        other => panic!("expected a write-preparation failure, got {other:?}"),
    }
    assert!(store.is_dirty(), "the staged edit is kept for a retry");
    assert_eq!(
        repo_snapshot(&fx),
        after_edit,
        "nothing was written beyond the hand edit"
    );
}

#[test]
fn an_external_edit_after_load_aborts_as_conflicted() {
    // Failure injection (3), R5.6: another program edits input.lua after the store
    // loaded it. The apply must abort as Conflicted with NO writes and NO commands,
    // leaving the external edit standing.
    let fx = FixtureDotfiles::install();
    let path = input_lua(&fx);

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("the edit stages");

    // The external edit, through the deployed path (writes through to the repo).
    let externally_edited = format!(
        "{}-- edited by hand while the app was open\n",
        fs::read_to_string(&path).expect("read the fixture")
    );
    fs::write(&path, &externally_edited).expect("apply the external edit");

    let plan = plan_with_input_write(&store, &model);
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

/// The state both deleted-`hypridle.conf` tests start from (task 9.11): the fixture
/// tree with `input.lua` and the Power & Idle page's `hypridle.conf` loaded into one
/// store, after which the deployed `hypridle.conf` is deleted — the state the store can
/// never re-baseline on its own.
struct DeletedHypridleConf {
    fx: FixtureDotfiles,
    before: BTreeMap<String, Vec<u8>>,
    hypridle_conf: PathBuf,
    store: SettingsStore,
    input: InputModel,
    power: PowerModel,
}

/// Installs the fixture tree, loads both store-backed files, then deletes the deployed
/// `hypridle.conf` — the shared starting point of the two deleted-file tests below.
fn with_hypridle_conf_deleted() -> DeletedHypridleConf {
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let (mut store, input) = store_and_model(&fx);
    let hypridle_conf = fx.config_path("hypr/hypridle.conf");
    load_into_store(&mut store, &hypridle_conf, loaders::hypridle_conf);
    let power = PowerModel::load(hypridle_conf.clone());

    // The deployed path disappears — an undeployed dotfiles link, a `rm`, a half-finished
    // edit in another program. The repo file behind it is untouched, so the assertions
    // that no repo file changed still mean what they say.
    fs::remove_file(&hypridle_conf).expect("delete the deployed hypridle.conf");
    DeletedHypridleConf {
        fx,
        before,
        hypridle_conf,
        store,
        input,
        power,
    }
}

#[test]
fn a_deleted_hypridle_conf_does_not_block_an_input_apply() {
    // Task 9.11 (R5.6), end to end through the real loaders and models: the Input apply
    // must still succeed. It writes only `input.lua`, so it cannot clobber the missing
    // file; before task 9.11 the whole-tracker conflict check made every category's
    // apply return `Conflicted` until the app was restarted.
    let DeletedHypridleConf {
        fx,
        before,
        hypridle_conf,
        mut store,
        input: model,
        ..
    } = with_hypridle_conf_deleted();
    let path = input_lua(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture input.lua");

    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("the Input edit stages");

    let plan = plan_with_input_write(&store, &model);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);

    // (i) Applied, not Conflicted — and the missing file is reported so the user learns
    // about it. Matched directly rather than through `expect_applied`, which asserts the
    // opposite (no unrelated conflicts) for the suites that expect an undisturbed tree.
    match outcome {
        ApplyOutcome::Applied {
            reload_failures,
            written,
            unrelated_conflicts,
        } => {
            assert!(reload_failures.is_empty(), "the mocked reload succeeds");
            assert_eq!(written, vec![path.clone()], "only input.lua was written");
            assert_eq!(
                unrelated_conflicts.len(),
                1,
                "the deleted file is reported, not silently dropped"
            );
            assert_eq!(unrelated_conflicts[0].path(), hypridle_conf.as_path());
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(&original, "\"us,se\"", "\"se,us\""),
        "the Input edit was applied byte-exactly despite the unrelated missing file"
    );
    assert_eq!(
        runner.recorded(),
        vec![config_only_reload()],
        "the apply completed normally, reload included"
    );
    assert_repo_untouched_except(&fx, &before, &[REPO_INPUT_LUA]);
}

#[test]
fn a_deleted_hypridle_conf_cannot_be_written_by_its_page() {
    // Task 9.11 (R8.3), end to end through the real loaders and models: the missing
    // file's own page must still refuse to write. Its write glue reads the file to
    // render the edit, so a Power & Idle apply fails at preparation and the window
    // aborts — the app never writes bytes rendered from contents it could not read.
    // (The pipeline's own guard for the same case — a plan whose write target is
    // unreadable — is unit-tested in `core::apply`.)
    let DeletedHypridleConf {
        fx,
        before,
        mut store,
        power,
        ..
    } = with_hypridle_conf_deleted();

    // The Power & Idle page cannot prepare a write for the file that is gone, so an
    // apply of *its* edits aborts before the pipeline runs — and no repo file changed.
    store
        .stage(SettingId::DimTimeout, Value::Integer(600))
        .expect("the Power & Idle edit stages");
    let error = power
        .apply_contribution(&store.dirty_in_category(Category::PowerAndIdle))
        .expect_err("a deleted hypridle.conf cannot be written");
    assert!(
        error.to_string().contains("hypridle.conf"),
        "the abort names the file the user has to restore: {error}"
    );
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn a_failed_hyprctl_reload_leaves_the_write_standing() {
    // Failure injection (2), R5.5: `hyprctl reload config-only` exits non-zero. The
    // apply still completes, the file keeps its new bytes, and the failure is reported.
    let fx = FixtureDotfiles::install();
    let path = input_lua(&fx);
    let original = fs::read_to_string(&path).expect("read the fixture input.lua");

    let (mut store, model) = store_and_model(&fx);
    store
        .stage(SettingId::TouchpadTapToClick, Value::Bool(false))
        .expect("the edit stages");

    let plan = plan_with_input_write(&store, &model);

    // The reload fails; the (already completed) write must not be rolled back.
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);

    assert_eq!(reload_failures.len(), 1, "the failed reload is reported");
    assert_eq!(written, vec![path.clone()], "the write is reported applied");
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(&original, "tap_to_click = true,", "tap_to_click = false,"),
        "the file keeps its new bytes despite the reload failure (R5.5)"
    );
    assert_eq!(runner.recorded(), vec![config_only_reload()]);
}

#[test]
fn edits_staged_for_a_vanished_input_page_abort_the_apply() {
    // A manual Refresh rebuilds the Input helper from scratch. If input.lua was edited
    // into something the editor does not understand in the meantime, no helper is built
    // and the page hides — but edits staged before the Refresh are still in the store.
    // They have no write to land in, so the Apply must abort rather than commit values
    // that never reached disk (R8.3).
    let fx = FixtureDotfiles::install();
    let path = input_lua(&fx);
    let (mut store, _model) = store_and_model(&fx);
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    let fixture = fs::read_to_string(&path).expect("read the fixture input.lua");
    fs::write(
        &path,
        replace_once(&fixture, "follow_mouse = 1,", "follow_mouse = f(),"),
    )
    .expect("the hand edit");
    assert!(
        InputModel::load(path, no_xkb()).is_err(),
        "the refreshed load builds no Input helper"
    );

    let failure = assemble_apply_plan(ApplySources::for_store(&store))
        .expect_err("orphaned Input edits must abort the apply");
    match failure {
        PrepareFailure::Write { source, message } => {
            assert_eq!(source, FailedWrite::Input);
            assert!(
                message.contains("Reset"),
                "the message says how to get out: {message}"
            );
        }
        other => panic!("expected a write-preparation failure, got {other:?}"),
    }
    assert!(store.is_dirty(), "the staged edit is kept");
}

/// Restores a fixture directory to writable (`0o755`) on drop.
///
/// The rollback test below makes a repo directory read-only as its failure
/// injection; that must be undone even when an assertion panics mid-test, or the
/// [`FixtureDotfiles`] `TempDir` cannot delete the directory's contents on drop and
/// the temp tree leaks. An RAII guard runs on unwind too, which a restore statement
/// at the end of the test would not.
struct RestoreWritable<'a> {
    /// The directory whose permissions are restored on drop.
    dir: &'a Path,
}

impl Drop for RestoreWritable<'_> {
    fn drop(&mut self) {
        // Best-effort: an error here would only mask the original panic, and the
        // worst outcome of a failed restore is a leaked temp directory.
        let _ = fs::set_permissions(self.dir, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_mid_plan_write_failure_rolls_back_the_earlier_input_write() {
    // Failure injection, R5.4: a two-page plan in the window's fold order — the
    // store-backed Input write first, then the Theme page's GTK-theme writes — where
    // the second page's first write fails. The already-written input.lua must be
    // restored to its pre-apply bytes, the failure surfaced, and no reload attempted.
    // (This case lived in the Notifications suite until task 11.7 made that page
    // runtime-only; the Theme write is the remaining file write outside
    // `config/hypr/`, which the injection needs.)
    //
    // The injection: the repo's gtk-3.0/ directory is made read-only, so the atomic
    // writer cannot create its temp file beside the resolved settings.ini target. The
    // file itself stays readable, so the pipeline genuinely reaches the write phase.
    let fx = FixtureDotfiles::install();
    fx.install_user_gtk3_theme("Fixture-Theme");
    let before = repo_snapshot(&fx);
    let (mut store, model) = store_and_model(&fx);
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("the Input edit stages");
    let mut plan = plan_with_input_write(&store, &model);

    let gtk3 = fx.config_path("gtk-3.0/settings.ini");
    let mut themes = ThemesModel::load(
        &ThemeRoots {
            gtk_theme_dirs: vec![fx.home().join(".themes")],
            icon_dirs: vec![fx.home().join(".icons")],
        },
        ThemesPaths {
            gtk3_settings: gtk3.clone(),
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
    themes
        .stage_gtk_theme("Fixture-Theme")
        .expect("an installed theme name stages");
    let contribution = themes
        .apply_contribution()
        .expect("a dirty theme contributes");
    plan.writes.extend(contribution.writes);
    plan.validations.extend(contribution.validations);
    plan.reload_params = contribution.reload_params;

    let gtk3_dir = fx.repo_path("config/gtk-3.0");
    fs::set_permissions(&gtk3_dir, fs::Permissions::from_mode(0o555))
        .expect("make the gtk-3.0 directory read-only");
    let _restore = RestoreWritable { dir: &gtk3_dir };

    // Guard: running as root bypasses directory modes, so the injection only works
    // when the directory is genuinely non-writable (probed, mirroring the writer
    // tests). Skipping must be visible in the test output — a silent "ok" here would
    // leave R5.4 with no end-to-end coverage and no signal.
    if fs::File::create(gtk3_dir.join(".probe")).is_ok() {
        eprintln!(
            "skipped: directory modes are not enforced (running as root), \
             so the write-failure injection cannot work"
        );
        return;
    }

    let caps = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true);
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, store.freshness(), &caps, &runner, &signaller);
    assert!(
        runner.recorded().is_empty(),
        "a write-phase failure must run no reload (R5.4)"
    );
    assert!(signaller.calls().is_empty());

    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            match &failure.cause {
                WriteFailureCause::File { path, .. } => assert_eq!(
                    path, &gtk3,
                    "the failure names the write that could not complete"
                ),
                other => panic!("expected a File write failure, got {other:?}"),
            }
            assert_eq!(
                failure.rolled_back,
                vec![fx.repo_path(REPO_INPUT_LUA)],
                "the already-written input.lua (resolved to its repo target) is rolled back"
            );
            assert!(failure.rollback_failures.is_empty());
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }

    // The rollback restored the desktop exactly as it was: the whole repo tree is
    // byte-identical to the pre-apply snapshot (R5.4).
    assert_repo_untouched_except(&fx, &before, &[]);
}
