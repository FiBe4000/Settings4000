//! End-to-end staged-edit → Apply suite for the **Display** category (task 7.2;
//! R5.3–R5.6, R6.1).
//!
//! Drives the bespoke Display pipeline against the installed fixture tree (task
//! 7.1): the model is built by [`DisplayModel::load`] from a canned
//! `hyprctl monitors all -j` probe plus the fixture's real `monitors.conf`
//! (through its deployed symlink), edits are staged like the Display page
//! stages them, and the model's contribution is run through [`apply::run`] with
//! mocks — so the suite asserts the exact resulting `monitor=` record bytes
//! (awk-parseability preserved: `hypr-display-profile.sh` derives eDP values by
//! parsing this record, analysis §6.2) and the exact `hyprctl reload`.
//!
//! Unlike the store-driven categories, `monitors.conf` freshness is **owned by
//! the model** (R5.6): the window checks [`DisplayModel::check_conflict`] and
//! aborts *before* building the plan, so the conflict test here asserts that
//! guard rather than an [`ApplyOutcome::Conflicted`].
//!
//! The runtime laptop toggle (task 11.8) is covered here too, from detection
//! onwards: the fixture repo's `scripts/hypr-toggle-laptop-display` stub is
//! discovered behind the deployed symlinks (R8.5) and run — through the mock
//! runner only, never for real — and a tree without it hides the toggle (R4.2).

use std::fs;

use settings4000::core::apply::{self, ApplyPlan};
use settings4000::core::detect::{Binary, Capabilities, DetectionInputs};
use settings4000::core::display::DisplayModel;
use settings4000::core::freshness::FreshnessTracker;
use settings4000::core::notifd::NotifdReplies;
use settings4000::core::reload::ReloadParams;
use settings4000::system::command::{Command, CommandOutput, MockCommandRunner};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{
    FixtureDotfiles, assert_repo_untouched_except, capture_logs, expect_applied, replace_once,
    repo_snapshot,
};

/// A canned `hyprctl monitors all -j` payload for the fixture's laptop panel.
///
/// The description deliberately matches none of the fixture's `desc:` records, so
/// the live eDP-1 pairs with the generic `monitor=eDP-1,…` rule — the record whose
/// edit the suite asserts. The second available mode gives the resolution
/// drop-down a real alternative to stage.
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

/// The fixture's generic eDP-1 record — the exact line the staged edit rewrites.
const EDP_RECORD: &str = "monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,10";

/// Builds the Display model exactly as the startup worker does (task 6.1): a
/// `hyprctl monitors all -j` probe through a runner, plus the deployed
/// `monitors.conf` path.
fn load_model(fx: &FixtureDotfiles) -> DisplayModel {
    let probe_runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
        0,
        HYPRCTL_MONITORS_JSON,
        "",
    ))]);
    let model = DisplayModel::load(&probe_runner, fx.config_path("hypr/monitors.conf"), None)
        .expect("a successful probe yields a model");
    // Sanity-check the probe went through the seam as the app issues it.
    assert_eq!(
        probe_runner.recorded(),
        vec![Command::new("hyprctl").args(["monitors", "all", "-j"])]
    );
    assert_eq!(model.monitor_name(0), "eDP-1");
    assert!(model.records_editable(), "monitors.conf was readable");
    model
}

/// A display change reloads only via `hyprctl reload` (task 4.4).
fn caps() -> Capabilities {
    Capabilities::for_tests(&[Binary::Hyprctl], &[], true)
}

/// Wraps the model's contribution in a plan, as the window's Apply handler does.
///
/// The pipeline's freshness tracker is empty here because `monitors.conf` is not a
/// store-loaded file — its conflict guard is the model's own (see the module docs).
fn plan_from(model: &DisplayModel) -> ApplyPlan {
    let contribution = model
        .apply_contribution()
        .expect("the monitors.conf write renders")
        .expect("dirty monitor edits produce a contribution");
    ApplyPlan {
        validations: contribution.validations,
        writes: vec![contribution.write],
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

#[test]
fn a_mode_and_scale_edit_rewrites_only_the_edp_record_and_reloads() {
    // The happy path (R5.3): stage a resolution and a scale for eDP-1, Apply, and
    // assert the exact bytes — only the eDP-1 record's mode and scale fields
    // change; the catchall, the desc: records, the commented-out records, and
    // every comment stay byte-identical, keeping the record awk-parseable for
    // hypr-display-profile.sh (analysis §6.2).
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let path = fx.config_path("hypr/monitors.conf");
    let original = fs::read_to_string(&path).expect("read the fixture monitors.conf");

    let mut model = load_model(&fx);
    // Staging the second available resolution composes it with that resolution's
    // reported refresh (60), and the new scale replaces the fourth field.
    model.stage_resolution(0, "1920x1200".to_string());
    model.stage_scale(0, "1.25".to_string());

    let plan = plan_from(&model);
    // Empty tracker: monitors.conf freshness is model-owned (see the module docs).
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.clone()]);

    // (a) Exact bytes: one record line changed, field-for-field — mode
    // `1920x1200@60`, scale `1.25`, the `bitdepth,10` extras preserved.
    let expected = replace_once(
        &original,
        EDP_RECORD,
        "monitor=eDP-1,1920x1200@60,auto,1.25,bitdepth,10",
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        expected,
        "only the eDP-1 record's mode/scale fields may change"
    );
    assert_eq!(
        fs::read_to_string(fx.repo_path("config/hypr/monitors.conf"))
            .expect("read the repo target"),
        expected,
        "the write must land in the repo target behind the deployment symlink"
    );
    assert!(
        fs::symlink_metadata(&path)
            .expect("stat the deployed path")
            .file_type()
            .is_symlink(),
        "the deployment symlink must be preserved by the atomic writer"
    );

    // (b) The exact reload list.
    assert_eq!(
        runner.recorded(),
        vec![Command::new("hyprctl").arg("reload")]
    );
    assert!(signaller.calls().is_empty());

    // (c) Every other repo file is byte-identical.
    assert_repo_untouched_except(&fx, &before, &["config/hypr/monitors.conf"]);
}

#[test]
fn a_failed_hyprctl_reload_is_non_fatal_and_the_write_stands() {
    // Failure injection (2), R5.5: `hyprctl reload` exits non-zero after the write
    // succeeded. The apply completes, the record keeps its new bytes, and the
    // failure is reported.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");
    let original = fs::read_to_string(&path).expect("read the fixture monitors.conf");

    let mut model = load_model(&fx);
    model.stage_scale(0, "1.5".to_string());

    let plan = plan_from(&model);
    let freshness = FreshnessTracker::new();
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(&plan, &freshness, &caps(), &runner, &signaller);
    let (reload_failures, written) = expect_applied(outcome);

    assert_eq!(reload_failures.len(), 1, "the failed reload is reported");
    assert_eq!(written, vec![path.clone()]);
    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        replace_once(
            &original,
            EDP_RECORD,
            "monitor=eDP-1,2880x1800@120,auto,1.5,bitdepth,10",
        ),
        "the file keeps its new bytes despite the reload failure (R5.5)"
    );
}

#[test]
fn an_unrenderable_record_aborts_the_contribution_leaving_the_file_untouched() {
    // Failure injection (4), task 9.5: when the writer cannot render a dirty edit, the
    // contribution is an Err the window aborts on — so the pipeline never runs and
    // `monitors.conf` keeps its bytes. Without that (a bare `None`), the window would
    // read "nothing to write", apply successfully, and commit the Display model against
    // a file it never wrote.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");

    // The injected shape: the eDP record shortened to just a name and a mode. It is
    // enabled, so an edit takes the surgical in-place `set_field` path, but it has no
    // scale field to rewrite. Written through the deployed symlink before the load, so
    // the model parses it as its baseline.
    let shortened = replace_once(
        &fs::read_to_string(&path).expect("read the fixture monitors.conf"),
        EDP_RECORD,
        "monitor=eDP-1,2880x1800@120",
    );
    fs::write(&path, &shortened).expect("install the shortened record");
    let before = repo_snapshot(&fx);

    let mut model = load_model(&fx);
    model.stage_scale(0, "1.25".to_string());
    assert!(model.is_dirty());

    let error = model
        .apply_contribution()
        .expect_err("an unrenderable edit must abort the apply, not look like a no-op");
    assert!(
        error.to_string().contains("monitors.conf"),
        "the message must name the file the user has to fix: {error}"
    );

    // Nothing reached the disk, and the staged edit is still pending for a retry.
    assert_eq!(
        fs::read_to_string(&path).expect("read the file"),
        shortened,
        "an aborted contribution must leave monitors.conf byte-identical"
    );
    assert_repo_untouched_except(&fx, &before, &[]);
    assert!(model.is_dirty(), "the staged edit must be retained");
}

#[test]
fn an_external_edit_trips_the_models_conflict_guard_before_any_write() {
    // Failure injection (3), R5.6 — the Display flavour: monitors.conf freshness is
    // model-owned, so the window's guard is `check_conflict()` before the plan is
    // built. An external edit after load must trip it (the window then aborts and
    // reloads the model), and nothing has been written.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");

    let mut model = load_model(&fx);
    model.stage_scale(0, "1.5".to_string());
    assert!(
        !model.check_conflict(),
        "an untouched file is not a conflict"
    );

    // The external edit, through the deployed path.
    let externally_edited = format!(
        "{}# edited by hand while the app was open\n",
        fs::read_to_string(&path).expect("read the fixture")
    );
    fs::write(&path, &externally_edited).expect("apply the external edit");

    assert!(
        model.check_conflict(),
        "the external edit must be detected before any write (R5.6)"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read the file"),
        externally_edited,
        "nothing was written; the external edit stands"
    );
}

/// Runs real capability detection over the installed tree, with no binaries,
/// daemons or compositor — only the filesystem-backed repo discovery matters to
/// the laptop toggle (R8.5).
fn detect(fx: &FixtureDotfiles) -> Capabilities {
    Capabilities::detect(&DetectionInputs {
        path: None,
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: Vec::new(),
        hyprctl_status: None,
        hyprland_lua_config: fx.config_path("hypr/hyprland.lua"),
        notifd_replies: NotifdReplies::default(),
        notifd_unit_file: fx.config_path("systemd/user/notifd.service"),
    })
}

/// [`HYPRCTL_MONITORS_JSON`] with the panel switched off, as `monitors all -j`
/// lists a disabled output.
fn monitors_json_disabled() -> String {
    let disabled = HYPRCTL_MONITORS_JSON.replace(r#""disabled": false"#, r#""disabled": true"#);
    assert_ne!(disabled, HYPRCTL_MONITORS_JSON);
    disabled
}

#[test]
fn the_laptop_toggle_runs_the_detected_repo_script_between_two_reads() {
    // Task 11.8 acceptance, end to end from detection: the toggle script is resolved
    // from the fixture repo root behind the deployed symlinks (R8.5); toggling issues
    // exactly `hyprctl monitors all -j`, the script by its repo path with no
    // arguments, and the re-read; the switch shows the re-read state; and no repo
    // file — monitors.conf included — changes.
    let fx = FixtureDotfiles::install();
    let before = repo_snapshot(&fx);
    let caps = detect(&fx);
    let script = fx.repo_path("scripts/hypr-toggle-laptop-display");
    assert_eq!(caps.laptop_toggle_script(), Some(script.as_path()));

    let probe_runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
        0,
        HYPRCTL_MONITORS_JSON,
        "",
    ))]);
    let mut model = DisplayModel::load(
        &probe_runner,
        fx.config_path("hypr/monitors.conf"),
        caps.laptop_toggle_script().map(|path| path.to_path_buf()),
    )
    .expect("a successful probe yields a model");
    assert!(model.laptop_toggle_available(0));
    assert!(model.laptop_enabled(0));

    let runner = MockCommandRunner::with_outcomes([
        Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_MONITORS_JSON,
            "",
        )),
        Ok(CommandOutput::fake(0)),
        Ok(CommandOutput::fake_with_streams(
            0,
            &monitors_json_disabled(),
            "",
        )),
    ]);
    model
        .toggle_laptop(0, false, &runner)
        .expect("the re-read confirms the switch");

    let script_program = script.to_str().expect("the fixture path is UTF-8");
    assert_eq!(
        runner.recorded(),
        vec![
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
            Command::new(script_program),
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
        ]
    );
    assert!(!model.laptop_enabled(0), "the switch shows the read state");
    assert!(!model.is_dirty(), "the runtime toggle never stages");
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn a_tree_without_the_toggle_script_hides_the_laptop_toggle_and_logs_it() {
    // R4.2: with the script gone from the repo, detection reports no toggle script
    // and logs the hidden control at `info`; the model built from that offers no
    // laptop toggle while the panel's mode/scale rows (monitors.conf) stay.
    let fx = FixtureDotfiles::install();
    fs::remove_file(fx.repo_path("scripts/hypr-toggle-laptop-display"))
        .expect("remove the toggle script stub");

    let mut caps = None;
    let events = capture_logs(|| caps = Some(detect(&fx)));
    let caps = caps.expect("detection ran");
    assert!(caps.laptop_toggle_script().is_none());
    assert!(
        events
            .iter()
            .any(|event| event.level == tracing::Level::INFO
                && event.message.contains("laptop-display toggle is hidden")),
        "the hidden toggle must be logged at info, got {events:?}"
    );
    assert!(
        caps.palette_source().is_some(),
        "the rest of the repo-backed detection is unaffected"
    );

    let model = DisplayModel::load(
        &MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_MONITORS_JSON,
            "",
        ))]),
        fx.config_path("hypr/monitors.conf"),
        caps.laptop_toggle_script().map(|path| path.to_path_buf()),
    )
    .expect("a successful probe yields a model");
    assert!(model.is_laptop(0));
    assert!(!model.laptop_toggle_available(0), "no script, no toggle");
    assert!(model.records_editable(), "the file-backed rows stay");
}
