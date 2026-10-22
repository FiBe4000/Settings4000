//! The worker-thread glue of the startup load (tasks 5.4, 9.21; architecture §8; R4.3,
//! R8.1).
//!
//! The load itself — detection plus parsing every backing config file into a
//! [`StartupLoad`] — is GTK-free and lives in [`crate::core::load`], where it is tested
//! headlessly (R6.2). What stays here is the one thing `core/` must not do: move that
//! pass onto a worker thread and hand its result back to the GTK main loop, the only
//! thread allowed to touch widgets. The window (see [`super::window`]) calls
//! [`spawn_load`] for the cold-start load and for every manual Refresh.
//!
//! The tests below check what a load result means for the UI — which categories and
//! rows a given set of capabilities and files lets the window show — so they pair the
//! `core::load` builders with the UI layer's visibility rules, which `core/` cannot
//! import.

use crate::core::load::{self, StartupLoad};
use crate::ui::worker::run_off_main_thread;

/// Runs one full [`load::load`] pass — detection, backing-file parsing, and the
/// bespoke page models — on relm4's blocking pool, handing the result to `on_done` on
/// the main thread (architecture §8).
///
/// Shared by the cold-start load (task 5.4) and the manual Refresh (R4.3), which must
/// run the *same* loader path so refreshed capabilities and rebuilt models never
/// diverge; only what each does with the result differs. GTK is single-threaded, so
/// the worker only gathers data — `on_done` is where widgets may be touched. It
/// receives `None` when the worker finished without delivering a result (the window
/// closed while it ran), which every caller treats as harmless. The thread hop itself
/// is [`run_off_main_thread`], which the laptop-display toggle uses too (task 10.3).
pub(super) fn spawn_load(on_done: impl FnOnce(Option<StartupLoad>) + 'static) {
    run_off_main_thread(load::load, on_done);
}

#[cfg(test)]
mod tests {
    use crate::core::detect::{Binary, Capabilities, HyprConfigProvider};
    use crate::core::load::{
        adopt_refreshed_model, load_input_from, load_notifd_config_from, load_power, load_themes,
        themes_paths,
    };
    use crate::testing::input_lua_with_sensitivity;

    #[test]
    fn a_hyprlang_provider_hides_the_lua_only_controls() {
        // Task 11.2 (analysis §8.6): the config provider is a guard. With `hyprctl`
        // present, a Lua host shows the Input page and keeps the Hyprland-side cursor
        // copy; a hyprlang host hides both, with nothing routed to a hyprlang writer.
        use crate::ui::category::{SidebarCategory, visible_categories};

        let dir = tempfile::tempdir().expect("temp dir");
        let input_lua = dir.path().join("input.lua");
        std::fs::write(&input_lua, input_lua_with_sensitivity("0.3")).expect("write input.lua");
        let xkb = dir.path().join("no-evdev.xml");

        let lua = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true);
        assert_eq!(lua.hypr_config_provider(), HyprConfigProvider::Lua);
        assert!(visible_categories(&lua).contains(&SidebarCategory::Input));
        assert!(
            load_input_from(&lua, input_lua.clone(), &xkb).is_some(),
            "the Input helper is built"
        );
        assert!(
            themes_paths(&lua)
                .hyprland_lua
                .is_some_and(|path| path.ends_with("hypr/hyprland.lua")),
            "the Hyprland-side cursor copy is written on a Lua host"
        );

        let hyprlang = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true)
            .with_hypr_config_provider_for_tests(HyprConfigProvider::Hyprlang);
        assert!(
            !visible_categories(&hyprlang).contains(&SidebarCategory::Input),
            "the Input page is hidden on a hyprlang host"
        );
        assert!(
            load_input_from(&hyprlang, input_lua, &xkb).is_none(),
            "no Input helper is built"
        );
        assert!(
            themes_paths(&hyprlang).hyprland_lua.is_none(),
            "the Hyprland-side cursor write is hidden on a hyprlang host"
        );
        // The rest of the Theme page is unaffected: gsettings still gates it.
        assert!(visible_categories(&hyprlang).contains(&SidebarCategory::Theme));
        assert!(load_themes(&hyprlang).is_some());
    }

    #[test]
    fn an_unusable_input_lua_hides_the_input_page() {
        // Task 11.4 (architecture §10, R4.2): on a Lua host with `hyprctl`, the Input
        // page still needs an `input.lua` the editor fully understands. A missing file
        // and one with an opaque line inside the `input` table each build no helper —
        // which leaves the page without rows, so the window drops the category — and
        // the reason is logged at `info`, by line number for the opaque case and never
        // with file contents (R7.3).
        use crate::testing::capture_logs;
        use crate::ui::input::page_rows;

        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let dir = tempfile::tempdir().expect("temp dir");
        let xkb = dir.path().join("no-evdev.xml");
        let hidden_reason = |events: &[crate::testing::CapturedEvent]| {
            events
                .iter()
                .find(|event| {
                    event.level == tracing::Level::INFO
                        && event
                            .message
                            .contains("input.lua cannot back the Input page")
                })
                .map(|event| event.fields.clone())
        };

        // No input.lua at all.
        let missing = dir.path().join("input.lua");
        let mut model = None;
        let events = capture_logs(|| model = load_input_from(&caps, missing.clone(), &xkb));
        assert!(model.is_none(), "no helper without input.lua");
        assert!(
            page_rows(model.as_ref(), &caps).is_empty(),
            "the page has no rows"
        );
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(
            fields.contains("unreadable"),
            "the reason is given: {fields}"
        );

        // An input.lua with a computed value inside the `input` table (line 3).
        std::fs::write(
            &missing,
            "hl.config({\n    input = {\n        sensitivity = secret_value(),\n    },\n})\n",
        )
        .expect("write input.lua");
        let events = capture_logs(|| model = load_input_from(&caps, missing.clone(), &xkb));
        assert!(model.is_none(), "no helper for an opaque input table");
        assert!(page_rows(model.as_ref(), &caps).is_empty());
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(
            fields.contains("(lines 3)"),
            "the opaque line is named: {fields}"
        );
        assert!(
            !fields.contains("secret_value"),
            "no file contents: {fields}"
        );

        // The same tree with a usable file shows the page.
        std::fs::write(&missing, input_lua_with_sensitivity("0.3")).expect("write input.lua");
        let model = load_input_from(&caps, missing, &xkb);
        assert!(model.is_some());
        assert!(!page_rows(model.as_ref(), &caps).is_empty());
    }

    #[test]
    fn the_notifd_rows_need_a_reloadable_daemon_and_a_readable_file() {
        // Task 11.15 (R4.2): the staged notifd rows are offered only for notifd >= 1.1
        // and a readable file; otherwise the page keeps its runtime controls alone and
        // the reason is logged at `info`.
        use crate::core::notifd::{NotifdStatus, NotifdVersion};
        use crate::testing::{CapturedEvent, capture_logs};
        use crate::ui::notifications::page_rows;

        let with_notifd = |version: &str| {
            Capabilities::for_tests(&[], &[], false).with_notifd_for_tests(NotifdStatus::Running {
                version: NotifdVersion::parse(version),
            })
        };
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
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");

        // notifd 1.1 but no file.
        let mut model = None;
        let events =
            capture_logs(|| model = load_notifd_config_from(&with_notifd("1.1"), path.clone()));
        assert!(model.is_none());
        assert!(page_rows(model.as_ref()).is_empty());
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(fields.contains("cannot be read"), "{fields}");

        // The file exists, but the daemon is 1.0.
        std::fs::write(&path, "timeout_low_ms=5000\n").expect("write the file");
        let events =
            capture_logs(|| model = load_notifd_config_from(&with_notifd("1.0"), path.clone()));
        assert!(model.is_none());
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(fields.contains("1.1 or later"), "{fields}");

        // Both conditions hold: the seven rows.
        let model = load_notifd_config_from(&with_notifd("1.1"), path);
        assert_eq!(page_rows(model.as_ref()).len(), 7);
    }

    #[test]
    fn refresh_rebuilds_a_model_as_its_capability_changes() {
        // The task-9.3 accept scenario at the capability seam: hypridle is installed
        // between startup and a Refresh. The startup pass built no Power & Idle model
        // (category hidden, R4.2); the refresh pass runs the *same* loader against the
        // new capabilities, builds the model, and the category becomes visible.
        use crate::ui::category::{SidebarCategory, visible_categories};

        let before = Capabilities::for_tests(&[], &[], false);
        let mut current = load_power(&before);
        assert!(current.is_none(), "absent at startup");
        assert!(
            !visible_categories(&before).contains(&SidebarCategory::PowerAndIdle),
            "the category is hidden while the capability is absent (R4.2)"
        );

        // hypridle installed -> Refresh: the loader now builds the model and the
        // adoption swaps it in (no staged edits to protect).
        let after = Capabilities::for_tests(&[Binary::Hypridle], &[], false);
        adopt_refreshed_model(&mut current, load_power(&after), |_| false);
        assert!(current.is_some(), "the model is rebuilt on Refresh (R4.3)");
        assert!(
            visible_categories(&after).contains(&SidebarCategory::PowerAndIdle),
            "the category appears with the refreshed capabilities"
        );

        // hypridle uninstalled -> Refresh: the fresh pass yields None and the model is
        // dropped so the section hides again (R4.2).
        adopt_refreshed_model(&mut current, load_power(&before), |_| false);
        assert!(
            current.is_none(),
            "the model is dropped when its app is gone"
        );
    }
}
