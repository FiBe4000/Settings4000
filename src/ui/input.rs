//! The Input page's descriptor glue (task 6.6; architecture §7; R2.3, R4.2).
//!
//! # What this module is
//!
//! Unlike the Display, Sound, and Theme pages — which are wholly bespoke because their
//! data is dynamic or runtime-only — the Input page's settings map cleanly onto the
//! fixed [`SettingId`] enum, so they are staged in the shared store and rendered by the
//! declarative row framework (task 5.2). This module supplies the one thing the generic
//! framework path cannot: an Input row list whose keyboard-layout add-control is
//! populated from the **runtime-loaded XKB registry** (task 6.6), plus the curated
//! keyboard-option switches. The window builds a [`SettingsPage`](super::page::SettingsPage)
//! from [`rows`] directly (rather than the pure `page::plan_category`, which has no
//! access to the runtime candidate list).
//!
//! # The controls (requirements §3, R2.3)
//!
//! - **Keyboard layouts** — a reorderable editable list ([`WidgetKind::ReorderableList`])
//!   over [`SettingId::KeyboardLayouts`], whose add-control offers the XKB layouts;
//! - **Keyboard options** — one curated [`WidgetKind::TokenSwitch`] per option in
//!   [`CURATED_KB_OPTIONS`], all editing the single [`SettingId::KeyboardOptions`]
//!   string, so an option the app has no switch for is preserved verbatim (R4.2);
//! - **Mouse sensitivity** — a [`WidgetKind::Scale`] over
//!   [`SettingId::MouseSensitivity`] across the model's [`SENSITIVITY_RANGE`];
//! - **Touchpad** — a [`WidgetKind::Switch`] each for natural scroll and tap-to-click.
//!
//! Every row is [`RowCapability::Always`]: an Input change takes effect via `hyprctl
//! reload`, and the Input category itself is already `hyprctl`-gated (task 5.1), so no
//! row has any requirement *beyond* its category — the whole page appears or disappears
//! with `hyprctl` as one unit (R4.2).

use crate::core::input::LayoutOption;
use crate::core::model::{SENSITIVITY_RANGE, SettingId};
use crate::ui::row::{DropDownOption, RowCapability, RowDescriptor, WidgetKind};

/// The keyboard options the Input page exposes as curated switches, each `(token,
/// label)` (task 6.6, requirements §3).
///
/// These are the two options in use in the target dotfiles (analysis §6.3). Only these
/// get a switch; any *other* `kb_options` token on disk is preserved verbatim because
/// [`SettingId::KeyboardOptions`] holds the whole comma-joined list and a
/// [`WidgetKind::TokenSwitch`] toggles just its own token (R4.2).
const CURATED_KB_OPTIONS: &[(&str, &str)] = &[
    ("caps:escape", "Caps Lock acts as Escape"),
    ("grp:win_space_toggle", "Cycle layouts with Super + Space"),
];

/// The increment between adjacent positions of the sensitivity slider. A twentieth of
/// the range is fine-grained enough for Hyprland's `sensitivity` without overwhelming
/// the slider with steps.
const SENSITIVITY_STEP: f64 = 0.05;

/// Builds the Input page's row descriptors, with the keyboard-layout add-control
/// populated from the XKB `layouts` (task 6.6, R2.3).
///
/// The window filters these through the per-row capability gate
/// ([`visible_rows`](super::row::visible_rows)) and launches a
/// [`SettingsPage`](super::page::SettingsPage) for the survivors, exactly as the generic
/// framework path does — the only difference from `page::plan_category` is that the
/// candidate list is supplied at runtime here rather than hardcoded. An empty `layouts`
/// (no XKB registry, R4.4) simply yields an add-control with no entries; the layout list
/// itself still renders whatever is on disk.
pub(crate) fn rows(layouts: &[LayoutOption]) -> Vec<RowDescriptor> {
    let candidates = layouts
        .iter()
        .map(|layout| {
            // Show the friendly description with the code so two similarly-named
            // layouts are still distinguishable; store the bare code Hyprland expects.
            DropDownOption::new(
                layout.code.clone(),
                format!("{} ({})", layout.description, layout.code),
            )
        })
        .collect();

    let mut rows = vec![
        RowDescriptor::new(
            "Keyboard layouts",
            WidgetKind::ReorderableList { candidates },
            SettingId::KeyboardLayouts,
            RowCapability::Always,
        ),
        RowDescriptor::new(
            "Mouse sensitivity",
            WidgetKind::Scale {
                min: *SENSITIVITY_RANGE.start(),
                max: *SENSITIVITY_RANGE.end(),
                step: SENSITIVITY_STEP,
            },
            SettingId::MouseSensitivity,
            RowCapability::Always,
        ),
        RowDescriptor::new(
            "Touchpad natural scrolling",
            WidgetKind::Switch,
            SettingId::TouchpadNaturalScroll,
            RowCapability::Always,
        ),
        RowDescriptor::new(
            "Touchpad tap to click",
            WidgetKind::Switch,
            SettingId::TouchpadTapToClick,
            RowCapability::Always,
        ),
    ];

    // One curated switch per known keyboard option; all edit the single kb_options
    // string, so an unknown option is never dropped (R4.2, preserve-unknowns).
    for (token, label) in CURATED_KB_OPTIONS {
        rows.push(RowDescriptor::new(
            *label,
            WidgetKind::TokenSwitch {
                token: (*token).to_string(),
            },
            SettingId::KeyboardOptions,
            RowCapability::Always,
        ));
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_well_formed_and_carry_the_xkb_candidates() {
        // Every descriptor pairs a compatible widget and setting kind (so the framework
        // builds the right control), and the layout add-control offers the XKB layouts
        // with the code stored and the description shown (R2.3).
        let layouts = vec![
            LayoutOption {
                code: "us".to_string(),
                description: "English (US)".to_string(),
            },
            LayoutOption {
                code: "se".to_string(),
                description: "Swedish".to_string(),
            },
        ];
        let rows = rows(&layouts);

        for row in &rows {
            assert!(
                row.is_well_formed(),
                "{:?} pairs an incompatible widget and setting kind",
                row.setting
            );
        }

        // The layout list carries the two candidates, code as token, description shown.
        match &rows[0].widget {
            WidgetKind::ReorderableList { candidates } => {
                assert_eq!(
                    candidates,
                    &vec![
                        DropDownOption::new("us", "English (US) (us)"),
                        DropDownOption::new("se", "Swedish (se)"),
                    ]
                );
            }
            other => panic!("expected the first row to be the layout list, got {other:?}"),
        }

        // Both curated options are token-switches over the single KeyboardOptions string.
        let token_switches: Vec<&str> = rows
            .iter()
            .filter_map(|row| match &row.widget {
                WidgetKind::TokenSwitch { token } => {
                    assert_eq!(row.setting, SettingId::KeyboardOptions);
                    Some(token.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(token_switches, vec!["caps:escape", "grp:win_space_toggle"]);
    }

    #[test]
    fn an_empty_xkb_registry_yields_an_empty_add_control() {
        // No XKB layouts (R4.4) still produces the full row set — only the add-control
        // is empty; the layout list itself renders whatever is on disk.
        let rows = rows(&[]);
        match &rows[0].widget {
            WidgetKind::ReorderableList { candidates } => assert!(candidates.is_empty()),
            other => panic!("expected the layout list, got {other:?}"),
        }
        assert_eq!(
            rows.len(),
            6,
            "all Input rows are present regardless of XKB"
        );
    }
}
