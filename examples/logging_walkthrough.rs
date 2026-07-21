//! R7.3 logging walkthrough driver (task 8.1).
//!
//! Runs a real staged-edit → Apply cycle — plus a rollback cycle — against an
//! installed **fixture** dotfiles tree ([`FixtureDotfiles::install`], task 7.1)
//! with the app's real tracing subscriber initialized, so
//! `journalctl --user -t settings4000` afterwards contains a genuine Apply
//! cycle to walk the R7.3 checklist against. The walkthrough itself (checklist
//! plus the captured journal lines) is documented in `docs/logging_audit.md`.
//!
//! # Safety boundaries (why parts are mocked)
//!
//! - **Files:** every write targets the fixture tree inside a `TempDir`. The
//!   user's real `~/.config` / `~/.dotfiles` are never read for writing, never
//!   written, and never reloaded. Detection canonicalizes only fixture paths.
//! - **Commands:** the happy-path cycle uses a [`MockCommandRunner`] for the
//!   reload phase, because a real `hyprctl reload` would poke the live desktop
//!   — out of bounds for a diagnostic driver. The reload-*level* logging
//!   (`reload command succeeded` / failure, R5.5) still runs for real, since
//!   it lives above the runner seam. The runner-level invocation + exit-status
//!   log (`ran command`, R7.3) is then demonstrated honestly in the rollback
//!   cycle, which runs the fixture's own `generate-colors` stub (a real
//!   subprocess that exits 2) through the real [`SystemCommandRunner`] —
//!   deliberately, as failure injection: the non-zero exit exercises the
//!   write-failure, rollback, and command-exit logging in one pass.
//! - **Signals:** the write phase fails before any reload in the rollback
//!   cycle and the happy path plans no signal-based reload, so the injected
//!   [`MockProcessSignaller`] is never asked to deliver anything.
//!
//! # Running it
//!
//! ```text
//! cargo run --example logging_walkthrough
//! journalctl --user -t settings4000 --since -2min --output=short
//! ```
//!
//! The example initializes logging at the app's `--log-level debug`
//! equivalent so the R7.3 `debug` items (parsed values, staged diffs) land in
//! the journal too. Like the rest of the test scaffolding (see
//! `src/testing.rs`), it panics loudly on any unexpected state — the crate's
//! "no panics on fallible runtime paths" rule applies to the shipped app, not
//! to a dev-only diagnostic driver.

use std::fs;
use std::path::Path;

use settings4000::core::apply::{self, ApplyOutcome, PaletteSwitch, WriteFailureCause};
use settings4000::core::detect::{Binary, Capabilities, DetectionInputs};
use settings4000::core::input::InputModel;
use settings4000::core::model::{Category, SettingId, Value};
use settings4000::core::store::SettingsStore;
use settings4000::system::command::{MockCommandRunner, SystemCommandRunner};
use settings4000::system::logging::{self, LogLevel};
use settings4000::system::signal::MockProcessSignaller;
use settings4000::testing::{FixtureDotfiles, base_apply_plan, load_into_store, loaders};

fn main() {
    // The app's real subscriber (journald when reachable, stderr fallback
    // otherwise — R7.1), at the `--log-level debug` directive so the crate's
    // `debug` output (parsed values, staged diffs) is captured (R7.2/R7.3).
    logging::init(Some(LogLevel::Debug));

    tracing::info!(
        "logging walkthrough BEGIN (task 8.1): fixture tree only — no real config is touched"
    );

    let fx = FixtureDotfiles::install();
    let input_conf = fx.config_path("hypr/input.conf");

    // --- Checklist (a): detection results ---------------------------------
    // Real detection against the fixture: the `$PATH` binary scan is a
    // read-only probe of the host, daemon liveness is injected as empty (this
    // driver must not depend on the live desktop), the palette source is
    // discovered through the fixture's deployed `colors.conf` symlink (R8.5),
    // and the three loaded configs are readability-checked.
    let inputs = DetectionInputs {
        path: std::env::var("PATH").ok(),
        running_processes: Vec::new(),
        hyprland_socket: None,
        palette_config_anchor: fx.config_path("hypr/colors.conf"),
        config_paths: vec![
            input_conf.clone(),
            fx.config_path("swaync/config.json"),
            fx.config_path("hypr/hypridle.conf"),
        ],
    };
    let detected = Capabilities::detect(&inputs);
    let generate_colors = detected
        .palette_source()
        .expect("the fixture's deployed symlink must reveal the palette source")
        .generate_colors()
        .to_path_buf();

    // --- Checklist (e): parsed values at debug ----------------------------
    // Load the store through the app's real startup loaders (task 5.4 wiring),
    // which log each file's parsed (setting, value) pairs at `debug`.
    let mut store = SettingsStore::new();
    load_into_store(&mut store, &input_conf, loaders::input_conf);
    load_into_store(
        &mut store,
        &fx.config_path("swaync/config.json"),
        loaders::swaync_config,
    );
    load_into_store(
        &mut store,
        &fx.config_path("hypr/hypridle.conf"),
        loaders::hypridle_conf,
    );

    // --- Checklist (e): staged diffs at debug -----------------------------
    store
        .stage(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )
        .expect("a layout reorder stages");
    store
        .stage(SettingId::TouchpadNaturalScroll, Value::Bool(false))
        .expect("a touchpad toggle stages");

    // --- Checklist (b)+(c): a full happy-path Apply -----------------------
    // The plan is assembled by the app's real builder plus the Input page's
    // write glue, exactly as the window's Apply handler and the task-7.2
    // suites do. Capabilities are pinned to "hyprctl + live IPC" so the reload
    // plan contains `hyprctl reload`; the mock runner answers it with success
    // instead of poking the real compositor (see the module docs).
    let model = InputModel::load(
        input_conf.clone(),
        Path::new("/nonexistent/evdev.xml"), // layout add-list is not under test
    );
    let mut plan = base_apply_plan(&store);
    plan.writes.push(
        model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the write renders")
            .expect("dirty settings produce a write"),
    );

    let apply_caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
    let mock_runner = MockCommandRunner::new();
    let signaller = MockProcessSignaller::new();
    let outcome = apply::run(
        &plan,
        store.freshness(),
        &apply_caps,
        &mock_runner,
        &signaller,
    );
    let written = match outcome {
        ApplyOutcome::Applied {
            reload_failures,
            written,
        } => {
            assert!(reload_failures.is_empty(), "the mocked reload succeeds");
            written
        }
        other => panic!("expected Applied on the happy path, got {other:?}"),
    };

    // Commit as the window does (task 4.5), logging the commit at `debug`.
    let committed: Vec<_> = plan
        .writes
        .iter()
        .map(|write| (write.path.clone(), write.contents.clone()))
        .collect();
    store.commit_apply(&committed);
    assert_eq!(written, vec![input_conf.clone()]);

    // --- Checklist (c)+(d): real command exit status, error, and rollback --
    // A second Apply whose palette generator fails: the input.conf write
    // happens for real, then the fixture's `generate-colors` stub runs as a
    // REAL subprocess through the real runner and exits 2 — so the journal
    // gets the runner's invocation + exit-status line, the write-phase error,
    // and the per-file rollback restore line, all against fixture files.
    let before_rollback = fs::read(&input_conf).expect("read input.conf before the rollback demo");
    store
        .stage(SettingId::MouseSensitivity, Value::Float(0.5))
        .expect("a sensitivity edit stages");
    let mut failing_plan = base_apply_plan(&store);
    failing_plan.writes.push(
        model
            .input_conf_write(&store.dirty_in_category(Category::Input))
            .expect("the second write renders")
            .expect("a dirty setting produces a write"),
    );
    failing_plan.palette = Some(PaletteSwitch {
        scheme: "nord".to_string(),
        generate_colors,
    });

    // Defense-in-depth: all-absent capabilities so that, should the stub ever
    // drift to exiting 0, this real-runner cycle still cannot plan a reload
    // (a real `hyprctl reload` against the live compositor is out of bounds;
    // reload logging is the happy-path cycle's job).
    let no_reload_caps = Capabilities::for_tests(&[], &[], false);
    let real_runner = SystemCommandRunner::new();
    let outcome = apply::run(
        &failing_plan,
        store.freshness(),
        &no_reload_caps,
        &real_runner,
        &signaller,
    );
    match outcome {
        ApplyOutcome::WriteFailed(failure) => {
            assert!(
                matches!(
                    failure.cause,
                    WriteFailureCause::GenerateColorsExit { code: Some(2) }
                ),
                "the fixture stub exits 2, got {:?}",
                failure.cause
            );
            assert!(
                failure.rollback_failures.is_empty(),
                "the rollback must restore cleanly"
            );
        }
        other => panic!("expected WriteFailed from the stub generator, got {other:?}"),
    }
    assert_eq!(
        fs::read(&input_conf).expect("read input.conf after the rollback"),
        before_rollback,
        "the rollback must restore the pre-apply bytes byte-exactly (R5.4)"
    );

    tracing::info!(
        "logging walkthrough END: happy-path apply + rollback both exercised against the fixture"
    );
    println!(
        "walkthrough complete — inspect with: journalctl --user -t settings4000 --since -2min"
    );
}
