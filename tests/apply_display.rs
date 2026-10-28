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
//! Turning an external monitor off and on through the `disabled,1` extra pair
//! (task 10.4) is asserted here as a whole-file byte diff, alongside the refusal
//! that keeps that pair off the laptop panel's records.
//!
//! The runtime laptop toggle (task 11.8) is covered here too, from detection
//! onwards: the fixture repo's `scripts/hypr-toggle-laptop-display` stub is
//! detected behind the deployed symlinks (R8.5) and run — through the mock
//! runner only, never for real — and a tree without it hides the toggle (R4.2).

use std::fs;

use settings4000::core::apply::{self, ApplyPlan};
use settings4000::core::detect::{Binary, Capabilities, DetectionInputs};
use settings4000::core::display::{DisplayModel, MonitorView, StageRefusal};
use settings4000::core::freshness::FreshnessTracker;
use settings4000::core::notifd::NotifdReplies;
use settings4000::core::reload::ReloadParams;
use settings4000::parsers::monitors::{GrammarRule, MonitorsFile};
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
    assert_eq!(monitor_0(&model).name(), "eDP-1");
    assert!(model.records_editable(), "monitors.conf was readable");
    model
}

/// The model's first monitor — the fixture's laptop panel — failing the test when
/// the model lists none.
fn monitor_0(model: &DisplayModel) -> MonitorView<'_> {
    model.monitor(0).expect("the laptop panel is listed")
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
        links: Vec::new(),
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

#[test]
fn a_mode_plus_scale_edit_rewrites_only_the_edp_record() {
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
    model
        .stage_resolution(0, "1920x1200".to_string())
        .expect("the edit stages");
    model
        .stage_scale(0, "1.25".to_string())
        .expect("the edit stages");

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
fn a_failed_hyprctl_reload_leaves_the_write_standing() {
    // Failure injection (2), R5.5: `hyprctl reload` exits non-zero after the write
    // succeeded. The apply completes, the record keeps its new bytes, and the
    // failure is reported.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");
    let original = fs::read_to_string(&path).expect("read the fixture monitors.conf");

    let mut model = load_model(&fx);
    model
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");

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
fn an_unrenderable_record_aborts_the_contribution() {
    // Failure injection (4), tasks 9.5 and 11.9: when the writer refuses a dirty edit,
    // the contribution is an Err the window aborts on — so the pipeline never runs and
    // `monitors.conf` keeps its bytes. Without that (a bare `None`), the window would
    // read "nothing to write", apply successfully, and commit the Display model against
    // a file it never wrote.
    //
    // The injected edit is a scale carrying a comma, which would insert a `bitdepth`
    // key without a value into the eDP record and break the frozen grammar both
    // downstream parsers rely on (analysis §7.2). Every real `stage_*` call refuses it
    // at stage time, so it is crafted through the test-only seam to reach the writer's
    // last line of defence (R8.3).
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");
    let original = fs::read_to_string(&path).expect("read the fixture monitors.conf");
    let before = repo_snapshot(&fx);

    let mut model = load_model(&fx);
    model.stage_scale_unchecked_for_tests(0, "1.25,bitdepth");
    assert!(model.is_dirty());

    let error = model
        .apply_contribution()
        .expect_err("an unrenderable edit must abort the apply, not look like a no-op");
    assert!(
        error.to_string().contains("monitors.conf"),
        "the message must name the file the user has to fix: {error}"
    );
    assert!(
        error.to_string().contains("`,`"),
        "the message must name the broken rule: {error}"
    );

    // Nothing reached the disk, and the staged edit is still pending.
    assert_eq!(
        fs::read_to_string(&path).expect("read the file"),
        original,
        "an aborted contribution must leave monitors.conf byte-identical"
    );
    assert_repo_untouched_except(&fx, &before, &[]);
    assert!(model.is_dirty(), "the staged edit must be retained");
}

#[test]
fn a_grammar_breaking_edit_is_refused_at_stage_time() {
    // Task 11.9 acceptance on the live fixture: an edit that would insert `bitdepth`
    // without a value is refused when it is staged, naming the rule (R8.3). Nothing
    // becomes dirty, so nothing can reach the file.
    let fx = FixtureDotfiles::install();
    let mut model = load_model(&fx);

    let refusal = model
        .stage_scale(0, "1.25,bitdepth".to_string())
        .expect_err("a comma in a value is refused");
    assert!(
        refusal.to_string().contains("monitors.conf format: "),
        "the refusal names the grammar: {refusal}"
    );
    assert!(!model.is_dirty());

    // A valid edit on the same record still stages and renders.
    model
        .stage_scale(0, "1.25".to_string())
        .expect("the edit stages");
    assert_eq!(
        plan_from(&model).writes[0].changed_keys,
        vec!["monitor eDP-1 scale"]
    );
}

#[test]
fn a_grammar_breaking_record_makes_the_page_read_only() {
    // Task 11.9's degradation for a hand-edited `monitors.conf`: the eDP record lost
    // the value of its `bitdepth` extra. Loaded through the deployed symlink, the file
    // is logged at `warn` (line and rule, no contents), the per-monitor controls are
    // not offered, every edit is refused naming the line, and the file is never
    // written — the app does not edit around a record it would refuse to write.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");
    let broken = replace_once(
        &fs::read_to_string(&path).expect("read the fixture monitors.conf"),
        EDP_RECORD,
        "monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth",
    );
    fs::write(&path, &broken).expect("install the hand edit");
    let before = repo_snapshot(&fx);
    let broken_line = broken
        .lines()
        .position(|line| line.ends_with(",bitdepth"))
        .expect("the edited line")
        + 1;

    let mut model = None;
    let events = capture_logs(|| {
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_MONITORS_JSON,
            "",
        ))]);
        model = DisplayModel::load(&runner, fx.config_path("hypr/monitors.conf"), None);
    });
    let mut model = model.expect("the probe succeeded");

    let line_field = format!("line {broken_line}");
    let warnings: Vec<_> = events
        .iter()
        .filter(|event| event.level == tracing::Level::WARN)
        .collect();
    assert!(
        warnings
            .iter()
            .any(|event| event.message == "monitors.conf parse warning"
                && event.fields.contains(&line_field)),
        "the parser logs the broken record: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|event| event.message.contains("per-monitor settings are read-only")),
        "the read-only decision is logged: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .all(|event| !event.fields.contains("2880x1800")),
        "no file contents in the log (R7.3)"
    );

    assert!(!model.records_editable());
    let problem = model.records_problem().expect("the problem is reported");
    assert_eq!(problem.line(), broken_line);

    let refusal = model
        .stage_scale(0, "1.25".to_string())
        .expect_err("a read-only page refuses edits");
    assert!(
        refusal.to_string().contains(&line_field),
        "the refusal names the line: {refusal}"
    );
    assert!(!model.is_dirty());
    assert!(matches!(model.apply_contribution(), Ok(None)));
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn an_external_edit_trips_the_conflict_guard() {
    // Failure injection (3), R5.6 — the Display flavour: monitors.conf freshness is
    // model-owned, so the window's guard is `check_conflict()` before the plan is
    // built. An external edit after load must trip it (the window then aborts and
    // reloads the model), and nothing has been written.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");

    let mut model = load_model(&fx);
    model
        .stage_scale(0, "1.5".to_string())
        .expect("the edit stages");
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
/// daemons or compositor — only the filesystem-backed repo detection matters to
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
        bar_text_tone_script: fx.config_path("eww/scripts/bar-text-tone"),
        notif_center_script: fx.config_path("eww/scripts/notif-center"),
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
fn the_laptop_toggle_runs_the_repo_script_between_reads() {
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
    assert!(monitor_0(&model).laptop_toggle_available());
    assert!(monitor_0(&model).laptop_enabled());

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
    assert!(
        !monitor_0(&model).laptop_enabled(),
        "the switch shows the read state"
    );
    assert!(!model.is_dirty(), "the runtime toggle never stages");
    assert_repo_untouched_except(&fx, &before, &[]);
}

#[test]
fn a_tree_without_the_toggle_script_hides_the_toggle() {
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
    assert!(monitor_0(&model).is_laptop());
    assert!(
        !monitor_0(&model).laptop_toggle_available(),
        "no script, no toggle"
    );
    assert!(model.records_editable(), "the file-backed rows stay");
}

/// The fixture's first office monitor record — an external display keyed on its
/// EDID description, the record the disable tests turn off and on (task 10.4).
const OFFICE_RECORD: &str = "monitor=desc:Example Monitor Corp QHD-24 0x0001,2560x1440@60,0x0,1";

/// The exact bytes task 10.4 appends to a record to turn its output off.
const DISABLED_PAIR: &str = ",disabled,1";

/// A `hyprctl monitors all -j` payload with the fixture's laptop panel plus the
/// office monitor that [`OFFICE_RECORD`] configures, both on.
const HYPRCTL_DOCKED_JSON: &str = r#"[{
    "name": "eDP-1",
    "description": "Fixture Internal Panel",
    "width": 2880,
    "height": 1800,
    "refreshRate": 120.0,
    "x": 0,
    "y": 0,
    "scale": 1.333333,
    "disabled": false,
    "availableModes": ["2880x1800@120.00Hz"]
}, {
    "name": "DP-3",
    "description": "Example Monitor Corp QHD-24 0x0001",
    "width": 2560,
    "height": 1440,
    "refreshRate": 59.951,
    "x": 0,
    "y": 0,
    "scale": 1.0,
    "disabled": false,
    "availableModes": ["2560x1440@59.95Hz"]
}]"#;

/// Builds the Display model from the installed tree with `hyprctl_json` as the
/// compositor's reply.
fn load_model_with(fx: &FixtureDotfiles, hyprctl_json: &str) -> DisplayModel {
    let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
        0,
        hyprctl_json,
        "",
    ))]);
    let model = DisplayModel::load(&runner, fx.config_path("hypr/monitors.conf"), None)
        .expect("a successful probe yields a model");
    assert!(model.records_editable(), "monitors.conf was readable");
    model
}

/// Runs `model`'s contribution through the Apply pipeline with mocks, asserting it
/// succeeded with exactly the full `hyprctl reload` a `monitors.conf` write needs.
fn apply_and_expect_reload(model: &DisplayModel, path: &std::path::Path) {
    let runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(
        &plan_from(model),
        &FreshnessTracker::new(),
        &caps(),
        &runner,
        &signaller,
    );
    let (reload_failures, written) = expect_applied(outcome);
    assert!(reload_failures.is_empty());
    assert_eq!(written, vec![path.to_path_buf()]);
    assert_eq!(
        runner.recorded(),
        vec![Command::new("hyprctl").arg("reload")]
    );
}

/// `before` with `insertion` inserted directly after the one occurrence of `line`,
/// i.e. the whole-file byte diff of a surgical append to that record.
fn insert_after(before: &str, line: &str, insertion: &str) -> String {
    let at = before.find(line).expect("the record is in the file") + line.len();
    assert_eq!(before.matches(line).count(), 1, "the record is unique");
    format!("{}{insertion}{}", &before[..at], &before[at..])
}

#[test]
fn disabling_an_external_desc_monitor_appends_exactly_the_disabled_pair() {
    // Task 10.4 acceptance on the fixture tree: turning the office monitor off
    // inserts exactly `,disabled,1` at the end of its `desc:` record — the whole
    // file differs from the original by that insertion and nothing else — and
    // turning it back on removes exactly that pair, leaving the file byte-identical
    // to the original. Both run the full pipeline (validate → atomic write through
    // the deployed symlink → full `hyprctl reload`).
    let fx = FixtureDotfiles::install();
    let before_repo = repo_snapshot(&fx);
    let path = fx.config_path("hypr/monitors.conf");
    let original = fs::read_to_string(&path).expect("read the fixture monitors.conf");

    let mut model = load_model_with(&fx, HYPRCTL_DOCKED_JSON);
    let office = 1;
    let view = model.monitor(office).expect("the office monitor is listed");
    assert_eq!(view.name(), "DP-3");
    assert!(!view.is_laptop() && view.effective_enabled());

    model
        .stage_enabled(office, false)
        .expect("an external monitor can be turned off");
    apply_and_expect_reload(&model, &path);
    model.commit();

    let off = fs::read_to_string(&path).expect("read the applied file");
    assert_eq!(off, insert_after(&original, OFFICE_RECORD, DISABLED_PAIR));
    assert_eq!(off.len(), original.len() + DISABLED_PAIR.len());
    assert!(
        !off.contains("disabled,true"),
        "the pair's value is the number 1, never `true`"
    );
    // The written file is inside the frozen grammar: the validator accepts the pair.
    let (parsed, warnings) = MonitorsFile::parse(&off);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        parsed.is_enabled("desc:Example Monitor Corp QHD-24 0x0001"),
        Some(false)
    );
    assert_repo_untouched_except(&fx, &before_repo, &["config/hypr/monitors.conf"]);

    // Back on: exactly the pair goes again.
    assert!(!model.monitor(office).expect("listed").effective_enabled());
    model
        .stage_enabled(office, true)
        .expect("an off monitor can be turned back on");
    apply_and_expect_reload(&model, &path);
    assert_eq!(
        fs::read_to_string(&path).expect("read the re-applied file"),
        original,
        "re-enabling restores the file byte for byte"
    );
    assert_repo_untouched_except(&fx, &before_repo, &[]);
}

#[test]
fn disabling_keeps_an_existing_bitdepth_extra() {
    // A record that already carries `bitdepth,10` keeps it: the pair is appended
    // after it rather than the record body being rewritten.
    let fx = FixtureDotfiles::install();
    let path = fx.config_path("hypr/monitors.conf");
    let with_bitdepth = format!("{OFFICE_RECORD},bitdepth,10");
    let original = replace_once(
        &fs::read_to_string(&path).expect("read the fixture monitors.conf"),
        OFFICE_RECORD,
        &with_bitdepth,
    );
    fs::write(&path, &original).expect("install the 10-bit office record");

    let mut model = load_model_with(&fx, HYPRCTL_DOCKED_JSON);
    model.stage_enabled(1, false).expect("stages");
    apply_and_expect_reload(&model, &path);

    assert_eq!(
        fs::read_to_string(&path).expect("read the applied file"),
        insert_after(&original, &with_bitdepth, DISABLED_PAIR),
    );
}

#[test]
fn disabling_a_laptop_panel_key_is_refused_naming_the_rule() {
    // The pair is never written on an eDP key: neither the generic `eDP-1` record
    // nor the `desc:` record the profile script maps a panel to (here the fixture's
    // laptop-B rule, matched by the live panel's description). Both are refused at
    // stage time with the rule named, and nothing is staged or written.
    let fx = FixtureDotfiles::install();
    let before_repo = repo_snapshot(&fx);

    let laptop_b =
        HYPRCTL_MONITORS_JSON.replace("Fixture Internal Panel", "Example Panel Vendor 0x2036");
    for hyprctl in [HYPRCTL_MONITORS_JSON, laptop_b.as_str()] {
        let mut model = load_model_with(&fx, hyprctl);
        let refusal = model
            .stage_enabled(0, false)
            .expect_err("the laptop panel's record never carries the pair");
        assert!(
            matches!(
                refusal,
                StageRefusal::Grammar(GrammarRule::DisabledOnLaptopKey)
            ),
            "{refusal:?}"
        );
        assert!(
            refusal
                .to_string()
                .contains("the laptop panel's record may not carry `disabled,1`"),
            "the refusal names the rule: {refusal}"
        );
        assert!(!model.is_dirty());
    }
    assert_repo_untouched_except(&fx, &before_repo, &[]);
}
