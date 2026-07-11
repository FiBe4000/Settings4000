//! The seven sidebar categories of the main window and the pure rule that decides
//! which of them are shown (task 5.1; architecture §7; requirements §3; R2.4, R4.2).
//!
//! # What this module is
//!
//! The main window (`super::window`) is a sidebar of top-level categories plus a
//! content stack, one page per category (architecture §7). This module names those
//! categories ([`SidebarCategory`]) and answers the one question the window builder
//! needs before it constructs any widget: *given the detected
//! [`Capabilities`](crate::core::detect::Capabilities), which categories should be
//! shown?* ([`visible_categories`]).
//!
//! # Why the decision lives here, separate from the widgets
//!
//! Deciding visibility is domain logic, not rendering, so it is kept as a pure
//! function with no GTK dependency and unit-tested headlessly (R6.2) — see the tests
//! below. The window builder then turns the returned list into stack pages. A
//! category that is not in the list is never added to the stack or sidebar at all,
//! which is exactly how a category with no available content is "cleanly hidden"
//! rather than greyed out or errored (R4.2).
//!
//! # Category-level gate now; row-level hiding later
//!
//! Task 5.1 establishes only the *shell*. The per-category gate below is
//! deliberately coarse: it asks whether a category could have *any* content, using
//! the presence of the tool or source that its future controls depend on. The
//! finer, per-row hiding (e.g. hiding just the wallpaper control while keeping the
//! GTK-theme control) and the real page content arrive with the declarative row
//! framework (task 5.2) and the individual category pages (§6). Where a category
//! aggregates several independent features (Theme), it is shown when *any* of them
//! is available and hidden only when *all* are gone, so the whole-category-hidden
//! rule (R4.2) holds even before row-level gating exists.

use crate::core::detect::{Binary, Capabilities};

/// One of the seven top-level settings categories shown in the window sidebar
/// (requirements §3, architecture §7).
///
/// This is the UI's notion of a sidebar page, and it is intentionally distinct from
/// [`crate::core::model::Category`], which enumerates only the *file-backed* pages
/// that carry staged, dirty-tracked values. Sound and Network appear here because
/// the sidebar still shows them, but they are runtime-only (R3.1/R5.2) and so have
/// no counterpart in the store's dirty model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarCategory {
    /// Per-monitor resolution/scale/position and the laptop-display toggle (§6.1).
    Display,
    /// Output/input device, volume and mute — runtime-only via `wpctl`/`pactl`
    /// (§6.2).
    Sound,
    /// Palette scheme, GTK/icon/cursor themes, wallpaper and lock background
    /// (§6.3–6.5).
    Theme,
    /// Keyboard layouts/options, mouse sensitivity and touchpad toggles (§6.6).
    Input,
    /// swaync notification position, timeouts and do-not-disturb (§6.7).
    Notifications,
    /// Idle dim/lock/DPMS timeouts and the lock command (§6.8).
    PowerAndIdle,
    /// Read-only NetworkManager connection status (§6.9).
    Network,
}

impl SidebarCategory {
    /// Every category in the fixed top-to-bottom order they appear in the sidebar
    /// (the order of requirements §3).
    ///
    /// [`visible_categories`] iterates this, so the sidebar order is defined in
    /// exactly one place.
    pub(crate) const ALL: &'static [SidebarCategory] = &[
        SidebarCategory::Display,
        SidebarCategory::Sound,
        SidebarCategory::Theme,
        SidebarCategory::Input,
        SidebarCategory::Notifications,
        SidebarCategory::PowerAndIdle,
        SidebarCategory::Network,
    ];

    /// The human-readable label shown in the sidebar for this category.
    ///
    /// `GtkStackSidebar` renders the title of each stack page (see
    /// [`Self::stack_name`]), so this is what the user reads.
    pub(crate) fn title(self) -> &'static str {
        match self {
            SidebarCategory::Display => "Display",
            SidebarCategory::Sound => "Sound",
            SidebarCategory::Theme => "Theme",
            SidebarCategory::Input => "Input",
            SidebarCategory::Notifications => "Notifications",
            SidebarCategory::PowerAndIdle => "Power & Idle",
            SidebarCategory::Network => "Network",
        }
    }

    /// The stable, machine-readable name identifying this category's page inside the
    /// `GtkStack`.
    ///
    /// `GtkStack` addresses its children by this name (distinct from the visible
    /// [`title`](Self::title)); keeping it fixed and lowercase-hyphenated lets later
    /// tasks reference a page (e.g. to select it) without depending on the display
    /// label.
    pub(crate) fn stack_name(self) -> &'static str {
        match self {
            SidebarCategory::Display => "display",
            SidebarCategory::Sound => "sound",
            SidebarCategory::Theme => "theme",
            SidebarCategory::Input => "input",
            SidebarCategory::Notifications => "notifications",
            SidebarCategory::PowerAndIdle => "power-and-idle",
            SidebarCategory::Network => "network",
        }
    }

    /// Whether this category has any content to show given `capabilities` (R4.2).
    ///
    /// The gate is the presence of the tool or source the category's controls depend
    /// on, matching the per-page hidden-when-absent acceptance criteria of the §6
    /// tasks. Presence is checked at the binary/source level rather than daemon
    /// liveness: a page should appear whenever the app it configures is installed,
    /// while gating an actual *reload* on the daemon running is the separate concern
    /// of the reload table (task 4.4).
    pub(crate) fn is_visible(self, capabilities: &Capabilities) -> bool {
        match self {
            // Display edits `monitors.conf` and applies via `hyprctl reload`; with no
            // `hyprctl` there is nothing to drive it (§6.1).
            SidebarCategory::Display => capabilities.has_binary(Binary::Hyprctl),
            // Sound is runtime-only. Its enumeration (`pw-dump`, falling back to
            // `wpctl status`) and all its controls speak only `wpctl`/`pw-dump`, so it
            // is gated on `wpctl` specifically — not the broader `audio_available`
            // (wpctl OR pactl). A `pactl`-only host would render a dead, inert page
            // (no devices, a dead rescan button), which R4.2 forbids; `pactl`-only
            // support is out of v1 scope (§6.2, R3.1).
            SidebarCategory::Sound => capabilities.has_binary(Binary::Wpctl),
            // Theme aggregates four independent features: palette switching (the
            // dotfiles palette source), GTK/icon/cursor themes (`gsettings`), the
            // wallpaper (`hyprpaper`) and the lock background (`hyprlock`). It is
            // shown when ANY of them is available and hidden only when ALL are gone,
            // so the whole-category-hidden rule (R4.2) holds even before row-level
            // gating exists (§6.3–6.5).
            SidebarCategory::Theme => {
                capabilities.palette_source().is_some()
                    || capabilities.has_binary(Binary::Gsettings)
                    || capabilities.has_binary(Binary::Hyprpaper)
                    || capabilities.has_binary(Binary::Hyprlock)
            }
            // Input edits the sourced `input.conf` and applies via `hyprctl reload`
            // (§6.6).
            SidebarCategory::Input => capabilities.has_binary(Binary::Hyprctl),
            // Notifications edits swaync's `config.json` and reloads via
            // `swaync-client` (§6.7).
            SidebarCategory::Notifications => capabilities.has_binary(Binary::Swaync),
            // Power & Idle edits `hypridle.conf` and restarts hypridle on Apply
            // (§6.8).
            SidebarCategory::PowerAndIdle => capabilities.has_binary(Binary::Hypridle),
            // Network reads connection status from NetworkManager's `nmcli` (§6.9).
            SidebarCategory::Network => capabilities.has_binary(Binary::Nmcli),
        }
    }
}

/// The categories to show in the sidebar for `capabilities`, in sidebar order
/// (R4.2).
///
/// This is the pure, GTK-free decision the window builder turns into stack pages: a
/// category absent from the returned list is not added to the stack or the sidebar
/// at all, so a category with no available content simply does not appear (R4.2). It
/// is unit-tested headlessly (R6.2) — see below.
pub(crate) fn visible_categories(capabilities: &Capabilities) -> Vec<SidebarCategory> {
    SidebarCategory::ALL
        .iter()
        .copied()
        .filter(|category| category.is_visible(capabilities))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`Capabilities`] with every category's gating binary present and audio
    /// available — the "everything installed" baseline. `hyprland_ipc` is `true`,
    /// though the shell gates on binary presence rather than liveness.
    fn all_present() -> Capabilities {
        Capabilities::for_tests(
            &[
                Binary::Hyprctl,
                Binary::Gsettings,
                Binary::Wpctl,
                Binary::Swaync,
                Binary::Hypridle,
                Binary::Nmcli,
                Binary::Hyprpaper,
                Binary::Hyprlock,
            ],
            &[],
            true,
        )
    }

    #[test]
    fn all_categories_visible_when_everything_is_present() {
        // With every gating tool installed, all seven categories are shown, in the
        // fixed sidebar order (requirements §3).
        let visible = visible_categories(&all_present());
        assert_eq!(
            visible,
            vec![
                SidebarCategory::Display,
                SidebarCategory::Sound,
                SidebarCategory::Theme,
                SidebarCategory::Input,
                SidebarCategory::Notifications,
                SidebarCategory::PowerAndIdle,
                SidebarCategory::Network,
            ],
        );
    }

    #[test]
    fn no_categories_visible_when_nothing_is_present() {
        // The empty-capabilities case (a machine without any of these tools): every
        // category is hidden, so the sidebar would be empty rather than showing
        // greyed-out stubs (R4.2).
        let none = Capabilities::for_tests(&[], &[], false);
        assert!(
            visible_categories(&none).is_empty(),
            "no category should be visible with no capabilities"
        );
    }

    #[test]
    fn visible_categories_returns_a_subset_in_sidebar_order() {
        // An intermediate capability set (neither all-present nor none): only the
        // gating tools for Sound, Notifications and Network are present. The result
        // must be exactly those three, in the canonical sidebar order (Sound, then
        // Notifications, then Network) with the hidden categories' gaps closed up —
        // confirming `visible_categories` filters to the available subset while
        // preserving the `SidebarCategory::ALL` order rather than any input order.
        let caps =
            Capabilities::for_tests(&[Binary::Wpctl, Binary::Swaync, Binary::Nmcli], &[], false);
        assert_eq!(
            visible_categories(&caps),
            vec![
                SidebarCategory::Sound,
                SidebarCategory::Notifications,
                SidebarCategory::Network,
            ],
        );
    }

    #[test]
    fn hyprctl_gates_the_display_and_input_categories() {
        // Present -> both shown; absent -> both hidden. Both edit Hyprland config and
        // apply via `hyprctl reload`, so they share the gate (§6.1/§6.6).
        let with_hyprctl = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        assert!(SidebarCategory::Display.is_visible(&with_hyprctl));
        assert!(SidebarCategory::Input.is_visible(&with_hyprctl));

        let without = Capabilities::for_tests(&[], &[], true);
        assert!(!SidebarCategory::Display.is_visible(&without));
        assert!(!SidebarCategory::Input.is_visible(&without));
    }

    #[test]
    fn wpctl_gates_the_sound_category() {
        // The Sound page speaks only wpctl/pw-dump, so it is gated on `wpctl`
        // specifically (§6.2, R3.1).
        let with_wpctl = Capabilities::for_tests(&[Binary::Wpctl], &[], false);
        assert!(SidebarCategory::Sound.is_visible(&with_wpctl));

        // pactl present but no wpctl -> hidden: a pactl-only host must not render a
        // dead, inert Sound page (R4.2). This is the S1 review fix.
        let pactl_only = Capabilities::for_tests(&[Binary::Pactl], &[], false);
        assert!(!SidebarCategory::Sound.is_visible(&pactl_only));

        let without = Capabilities::for_tests(&[], &[], false);
        assert!(!SidebarCategory::Sound.is_visible(&without));
    }

    #[test]
    fn theme_is_shown_for_any_sub_feature_and_hidden_only_when_all_are_gone() {
        // The whole-category-hidden rule (R4.2) for an aggregate page: each of Theme's
        // independent features alone keeps the category visible, and only when every
        // one is absent is the category hidden. (`for_tests` fixes the palette source
        // to absent, so that branch is exercised by detection's own tests; the three
        // binary-gated features are checked here.)
        for binary in [Binary::Gsettings, Binary::Hyprpaper, Binary::Hyprlock] {
            let caps = Capabilities::for_tests(&[binary], &[], false);
            assert!(
                SidebarCategory::Theme.is_visible(&caps),
                "{binary:?} alone should keep the Theme category visible"
            );
        }

        // None of the theme features present -> the whole category is hidden. The
        // unrelated tools here (e.g. hyprctl) must not accidentally reveal Theme.
        let unrelated = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Nmcli], &[], true);
        assert!(
            !SidebarCategory::Theme.is_visible(&unrelated),
            "Theme must be hidden when none of its features are available"
        );
    }

    #[test]
    fn theme_is_visible_when_only_the_palette_source_is_present() {
        // The palette-source arm of the Theme OR-gate in isolation (§6.3): with the
        // dotfiles palette source present but none of gsettings/hyprpaper/hyprlock,
        // Theme is still shown. This is the arm `for_tests`' default `palette_source:
        // None` cannot reach, so it uses the test-only palette-source builder. Since
        // no other gating tool is present, Theme is the sole visible category.
        let caps = Capabilities::for_tests(&[], &[], false).with_palette_source_for_tests();
        assert!(
            SidebarCategory::Theme.is_visible(&caps),
            "the palette source alone should keep the Theme category visible"
        );
        assert_eq!(
            visible_categories(&caps),
            vec![SidebarCategory::Theme],
            "with only the palette source present, Theme is the one visible category"
        );
    }

    #[test]
    fn swaync_hypridle_and_nmcli_gate_their_own_categories() {
        // Each single-tool category is shown iff its tool is present, and its gate is
        // independent of the others.
        let swaync = Capabilities::for_tests(&[Binary::Swaync], &[], false);
        assert!(SidebarCategory::Notifications.is_visible(&swaync));
        assert!(!SidebarCategory::PowerAndIdle.is_visible(&swaync));
        assert!(!SidebarCategory::Network.is_visible(&swaync));

        let hypridle = Capabilities::for_tests(&[Binary::Hypridle], &[], false);
        assert!(SidebarCategory::PowerAndIdle.is_visible(&hypridle));
        assert!(!SidebarCategory::Notifications.is_visible(&hypridle));

        let nmcli = Capabilities::for_tests(&[Binary::Nmcli], &[], false);
        assert!(SidebarCategory::Network.is_visible(&nmcli));
        assert!(!SidebarCategory::Notifications.is_visible(&nmcli));
    }

    #[test]
    fn title_and_stack_name_are_unique_across_categories() {
        // The stack addresses pages by name and the sidebar shows titles, so both
        // must be unique or pages would collide/shadow. Guards against a copy-paste
        // slip as categories are added.
        let mut titles: Vec<&str> = SidebarCategory::ALL.iter().map(|c| c.title()).collect();
        let count = titles.len();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), count, "category titles must be unique");

        let mut names: Vec<&str> = SidebarCategory::ALL
            .iter()
            .map(|c| c.stack_name())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "stack names must be unique");
    }
}
