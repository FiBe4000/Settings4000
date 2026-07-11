//! The declarative row framework: descriptors, widget kinds, per-row capability
//! gating, and the pure value conversions the widgets use (task 5.2; architecture
//! §7; R2.3, R4.2, R6.2).
//!
//! # What this module is
//!
//! A category page (§6) is described declaratively as a list of [`RowDescriptor`]s
//! rather than by hand-wiring widgets. Each descriptor names a human [`label`], the
//! kind of control to build ([`WidgetKind`]), the [`SettingId`] it edits, and the
//! [`RowCapability`] that must be present for the row to appear at all (R4.2). The
//! GTK layer (`super::page`) turns a descriptor into a live widget; this module holds
//! everything that is *not* GTK: the descriptor types themselves, the rule that
//! decides which rows are visible, and the small pure functions that translate
//! between a widget's native value (a switch's bool, a drop-down's selected option, a
//! scale's number, a reorderable list's items) and the typed [`Value`] the store
//! keeps.
//!
//! # Why the pure parts live here, separate from the widgets
//!
//! Keeping the descriptors, the gating, and the value conversions GTK-free means they
//! are unit-tested headlessly (R6.2), the same discipline `core/` follows — even
//! though the `ui/` layer is *allowed* to import GTK. The widget construction and the
//! message loop, which genuinely need GTK and a running main loop, live in
//! `super::page` and are exercised by running the app. This split is deliberate: the
//! logic a regression would hide in (which option maps to which token, how a scale
//! rounds to an integer, whether a row is gated out) is all here and testable, while
//! `super::page` stays a thin renderer.
//!
//! # The one-way data flow (thin UI, R2.3 / architecture §7)
//!
//! The framework enforces the project's thin-UI rule: a widget never holds its own
//! source of truth. On a user change it emits a [`SetValue`] message; the page relays
//! that to the [`SettingsStore`](crate::core), and the widget is then re-rendered
//! purely from `store.value(id)`. So a store change from *any* source — a sibling
//! edit, a Reset, an external-file reload — re-renders the widget identically to a
//! direct edit. The conversions here are the two halves of that loop: `value_from_*`
//! turns a widget change into a [`SetValue`]'s [`Value`]; the `*_from_value` helpers
//! turn the store's [`Value`] back into what the widget should display.
//!
//! # Per-row gating and how it composes with the category gate (task 5.1)
//!
//! Task 5.1's [`visible_categories`](super::category::visible_categories) is the
//! *coarse* gate: it decides whether a whole sidebar category could have any content.
//! This module's [`visible_rows`] is the *fine* gate: within a shown category it drops
//! the individual rows whose capability is absent (R4.2). The two compose in the
//! window builder (`super::window`): a category is shown by 5.1, its rows are filtered
//! by [`visible_rows`], and if that leaves **zero** rows the whole category is dropped
//! after all — the "hide the whole category when it is emptied" half of R4.2 that the
//! coarse gate cannot see. A category that simply has no descriptors yet keeps its
//! task-5.1 placeholder until §6 fills it in.

use crate::core::detect::{Binary, Capabilities};
use crate::core::model::{SettingId, Value, ValueKind};

/// A user-originated edit flowing from a widget to the store (architecture §7).
///
/// This is the single message the whole framework emits: on any user change a widget
/// constructs a `SetValue` and hands it to the page, which relays it to
/// [`SettingsStore::stage`](crate::core::store::SettingsStore::stage). Carrying the
/// [`SettingId`] alongside the [`Value`] is what lets one generic message type serve
/// every widget kind — the page does not need to know which widget sent it.
#[derive(Clone, Debug)]
pub(crate) struct SetValue {
    /// The setting the edit targets.
    pub(crate) id: SettingId,
    /// The new value the widget produced, already typed to the setting's kind.
    pub(crate) value: Value,
}

/// One selectable entry in a [`WidgetKind::DropDown`] or the add-control of a
/// [`WidgetKind::ReorderableList`].
///
/// The `token` is what is stored (a [`Value::Enum`] token, or one item of a
/// comma-joined list); the `label` is what the user reads. They are kept separate so
/// a stored token like `top-right` can be shown as a friendlier `Top right` without
/// the store ever seeing the display text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropDownOption {
    /// The value stored when this entry is chosen.
    pub(crate) token: String,
    /// The human-readable text shown for the entry.
    pub(crate) label: String,
}

impl DropDownOption {
    /// Builds an option from a stored `token` and its display `label`.
    pub(crate) fn new(token: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            label: label.into(),
        }
    }
}

/// The kind of control a [`RowDescriptor`] renders as (R2.3, architecture §7).
///
/// These are exactly the widget kinds R2.3 prescribes: a drop-down for a single
/// choice, a switch for a boolean, a scale for a continuous number, and a reorderable
/// editable list for an ordered multi-value setting. Which one a descriptor may use
/// is constrained by the setting's [`ValueKind`] — see [`Self::is_compatible_with`].
#[derive(Clone, Debug)]
pub(crate) enum WidgetKind {
    /// A `GtkDropDown` over a fixed option set, editing a [`Value::Enum`]. The chosen
    /// option's [`DropDownOption::token`] is the stored value.
    DropDown {
        /// The options, in display order.
        options: Vec<DropDownOption>,
    },
    /// A `GtkSwitch`, editing a [`Value::Bool`].
    Switch,
    /// A `GtkScale`, editing a [`Value::Float`] or [`Value::Integer`] over the
    /// documented range. Whether the value is integral is taken from the setting's
    /// [`SettingId::kind`], so an integer setting (e.g. a timeout in whole seconds)
    /// is never given a fractional part (see [`value_from_scale`]).
    Scale {
        /// Inclusive lower bound of the slider.
        min: f64,
        /// Inclusive upper bound of the slider.
        max: f64,
        /// The increment between adjacent slider positions.
        step: f64,
    },
    /// A `GtkListBox`-backed reorderable editable list, editing the comma-joined
    /// [`Value::String`] of an ordered multi-value setting (R2.3), e.g. the keyboard
    /// layout list. `candidates` are the entries the add-control offers.
    ReorderableList {
        /// The entries the user may add to the list.
        candidates: Vec<DropDownOption>,
    },
    /// A `GtkSwitch` that toggles a single `token` in and out of a comma-joined
    /// [`Value::String`] setting, **preserving every other token verbatim** (R4.2). It
    /// presents one flag of a multi-flag setting — e.g. one curated keyboard option
    /// (`caps:escape`) of Hyprland's `kb_options` list (task 6.6) — as a plain on/off
    /// switch, so options the app has no switch for are never dropped by an edit. The
    /// switch is on when the token is present; toggling appends or removes only that
    /// token (see [`value_from_token_toggle`]). Several `TokenSwitch` rows may target the
    /// same String setting, each owning a different token.
    TokenSwitch {
        /// The single comma-token this switch adds or removes (e.g. `caps:escape`).
        token: String,
    },
    /// A `GtkEntry` free-text field, editing a [`Value::String`]. Used for a setting
    /// whose value is arbitrary text with no fixed option set — the hypridle lock command
    /// (task 6.8). The stored value is the entry's text verbatim; validation (if any) is
    /// the setting's concern, not the widget's.
    Entry,
}

impl WidgetKind {
    /// Whether this widget can edit a setting whose value is of `kind`.
    ///
    /// Each widget produces exactly one shape of [`Value`], so a descriptor whose
    /// widget kind does not match its [`SettingId::kind`] is a wiring mistake. The
    /// [`Scale`](Self::Scale) accepts both numeric kinds because a slider drives a
    /// float or a rounded integer equally well. [`RowDescriptor::is_well_formed`]
    /// uses this so tests can assert every descriptor is self-consistent.
    pub(crate) fn is_compatible_with(&self, kind: ValueKind) -> bool {
        match self {
            WidgetKind::DropDown { .. } => kind == ValueKind::Enum,
            WidgetKind::Switch => kind == ValueKind::Bool,
            WidgetKind::Scale { .. } => matches!(kind, ValueKind::Float | ValueKind::Integer),
            WidgetKind::ReorderableList { .. }
            | WidgetKind::TokenSwitch { .. }
            | WidgetKind::Entry => kind == ValueKind::String,
        }
    }
}

/// The capability a row requires to be shown (R4.2).
///
/// Every row declares one of these; [`Self::is_present`] queries the detected
/// [`Capabilities`]. A row whose capability is absent is never built (the page skips
/// it), which is how a control for a missing tool is "cleanly hidden" rather than
/// greyed out.
///
/// Only the two variants the current pages need are defined. `Always` means the row
/// carries no requirement beyond its category already being shown (task 5.1's coarse
/// gate); `Binary` requires a specific tool on `$PATH`. The §6 category pages extend
/// this enum with the finer gates they need (e.g. an audio-client or palette-source
/// requirement), matching the capability queries task 5.1's category gate already
/// uses so a row and its category agree on what "present" means.
#[derive(Clone, Debug)]
pub(crate) enum RowCapability {
    /// No requirement of its own: the row is shown whenever its category is (its
    /// category gate is the only condition).
    Always,
    /// A specific binary must be on `$PATH`.
    Binary(Binary),
}

impl RowCapability {
    /// Whether this requirement is satisfied by `capabilities` (R4.2).
    pub(crate) fn is_present(&self, capabilities: &Capabilities) -> bool {
        match self {
            RowCapability::Always => true,
            RowCapability::Binary(binary) => capabilities.has_binary(*binary),
        }
    }
}

/// A declarative description of one settings row (task 5.2, architecture §7).
///
/// A page is built from a `Vec<RowDescriptor>`: the framework renders each into a
/// labelled control, wires it to emit [`SetValue`], and renders it from the store.
/// The descriptor is plain data (no GTK), so a page's shape is defined declaratively
/// and can be reasoned about and tested without a display.
#[derive(Clone, Debug)]
pub(crate) struct RowDescriptor {
    /// The label shown beside the control.
    pub(crate) label: String,
    /// The kind of control to build (R2.3).
    pub(crate) widget: WidgetKind,
    /// The setting this row edits.
    pub(crate) setting: SettingId,
    /// The capability that must be present for the row to appear (R4.2).
    pub(crate) capability: RowCapability,
}

impl RowDescriptor {
    /// Builds a descriptor from its parts.
    pub(crate) fn new(
        label: impl Into<String>,
        widget: WidgetKind,
        setting: SettingId,
        capability: RowCapability,
    ) -> Self {
        Self {
            label: label.into(),
            widget,
            setting,
            capability,
        }
    }

    /// Whether this row should be shown for the detected `capabilities` (R4.2).
    pub(crate) fn is_visible(&self, capabilities: &Capabilities) -> bool {
        self.capability.is_present(capabilities)
    }

    /// Whether the widget kind matches the setting's value kind — a self-consistency
    /// check for a descriptor (see [`WidgetKind::is_compatible_with`]).
    ///
    /// This can only fail through a programming mistake when authoring descriptors,
    /// so it is asserted in tests rather than checked at runtime.
    pub(crate) fn is_well_formed(&self) -> bool {
        self.widget.is_compatible_with(self.setting.kind())
    }
}

/// Filters `descriptors` down to the rows visible for `capabilities`, preserving
/// order (R4.2).
///
/// This is the per-row gate that composes with task 5.1's category gate (see the
/// module docs): the window builds a page from the returned rows, and treats an empty
/// result as "the category is emptied" and drops it. Returning owned clones lets the
/// caller move them into the launched page component.
///
/// Each dropped row is logged at `info` with its label and the absent capability, the
/// same "hidden state is logged at info" convention R4.2 requires (task 5.1 logs
/// hidden categories the same way).
pub(crate) fn visible_rows(
    descriptors: &[RowDescriptor],
    capabilities: &Capabilities,
) -> Vec<RowDescriptor> {
    descriptors
        .iter()
        .filter(|descriptor| {
            let visible = descriptor.is_visible(capabilities);
            if !visible {
                tracing::info!(
                    row = %descriptor.label,
                    capability = ?descriptor.capability,
                    "row hidden: its required capability is absent (R4.2)"
                );
            }
            visible
        })
        .cloned()
        .collect()
}

// --- Value conversions: widget change -> Value (one half of the loop) ---

/// The [`Value`] a switch produces for its `active` state.
pub(crate) fn value_from_switch(active: bool) -> Value {
    Value::Bool(active)
}

/// The [`Value`] a drop-down produces when `option` is chosen.
pub(crate) fn value_from_dropdown_option(option: &DropDownOption) -> Value {
    Value::Enum(option.token.clone())
}

/// The [`Value`] a scale produces for its raw slider position `raw`, coerced to the
/// setting's `kind`.
///
/// An [`ValueKind::Integer`] setting rounds to the nearest whole number so a timeout
/// slider yields `300`, never `300.0` or an off-by-a-hair `299.999`; any other kind
/// is treated as a [`Value::Float`]. (A scale is only ever built for a numeric
/// setting, so a non-numeric `kind` cannot occur; it is mapped to `Float` as the
/// sole sensible fallback rather than panicking.)
pub(crate) fn value_from_scale(kind: ValueKind, raw: f64) -> Value {
    match kind {
        ValueKind::Integer => Value::Integer(raw.round() as i64),
        _ => Value::Float(raw),
    }
}

/// The [`Value`] a reorderable list produces for its ordered `items` — the
/// comma-joined string an ordered multi-value setting stores (e.g. `us,se`).
pub(crate) fn value_from_list_items(items: &[String]) -> Value {
    Value::String(items.join(","))
}

/// The [`Value`] a text entry produces for its current `text` (task 6.8).
///
/// The text is stored verbatim as a [`Value::String`]; any format rule is the setting's
/// validation concern, not the widget's.
pub(crate) fn value_from_entry(text: &str) -> Value {
    Value::String(text.to_owned())
}

/// The [`Value`] a [`WidgetKind::TokenSwitch`] produces when `token` is switched
/// `active` on or off, starting from the setting's current comma-joined `value`.
///
/// Toggling on appends `token` (only if absent, so it is not duplicated); toggling off
/// removes every occurrence of it. Crucially, **all other tokens are kept in order and
/// verbatim** — including ones the app has no switch for — so a curated switch never
/// drops an unrecognised entry from the list (R4.2, the `kb_options` preserve-unknowns
/// rule of task 6.6). The result is the full comma-joined string to stage.
pub(crate) fn value_from_token_toggle(value: Option<&Value>, token: &str, active: bool) -> Value {
    let mut items = list_items_from_value(value);
    if active {
        if !items.iter().any(|item| item == token) {
            items.push(token.to_owned());
        }
    } else {
        items.retain(|item| item != token);
    }
    value_from_list_items(&items)
}

/// Whether a [`WidgetKind::TokenSwitch`] for `token` should show as on for the stored
/// `value` — i.e. whether `token` is currently present in the comma-joined list.
///
/// A missing or non-string value reads as off, never panicking, so the switch always
/// has a safe state to render.
pub(crate) fn token_switch_active_from_value(value: Option<&Value>, token: &str) -> bool {
    list_items_from_value(value)
        .iter()
        .any(|item| item == token)
}

// The three reorderable-list edit operations, as pure functions so the ordering
// logic is unit-tested rather than buried in a GTK click handler (R6.2). Each
// returns the [`Value`] to emit, or `None` when the operation would be a no-op —
// letting the caller skip a redundant [`SetValue`] (e.g. moving the first item up).

/// The list with the items at `from` and `to` swapped, as the value to store, or
/// `None` when the swap is a no-op.
///
/// A no-op is either `from == to` or an index out of range — including the "move the
/// first item up" (`to` underflows past 0) and "move the last item down" (`to` runs
/// off the end) cases at the list ends, which must not emit an edit.
pub(crate) fn list_with_swapped(items: &[String], from: usize, to: usize) -> Option<Value> {
    if from == to || from >= items.len() || to >= items.len() {
        return None;
    }
    let mut next = items.to_vec();
    next.swap(from, to);
    Some(value_from_list_items(&next))
}

/// The list with the item at `index` removed, as the value to store, or `None` when
/// `index` is out of range.
///
/// Removing the only item yields the empty string — a valid comma-joined value (the
/// empty list), whose acceptability is the setting's validation concern, not this
/// function's.
pub(crate) fn list_without(items: &[String], index: usize) -> Option<Value> {
    if index >= items.len() {
        return None;
    }
    let mut next = items.to_vec();
    next.remove(index);
    Some(value_from_list_items(&next))
}

/// The list with `token` appended, as the value to store, or `None` when the add is a
/// no-op.
///
/// A no-op is an empty `token` or one already present: an ordered layout list has no
/// meaning with duplicate entries, so a repeat add is ignored rather than emitted.
pub(crate) fn list_with_added(items: &[String], token: &str) -> Option<Value> {
    if token.is_empty() || items.iter().any(|item| item == token) {
        return None;
    }
    let mut next = items.to_vec();
    next.push(token.to_owned());
    Some(value_from_list_items(&next))
}

// --- Value conversions: store Value -> widget display (the other half) ---

/// Whether a switch should show as active for the stored `value`.
///
/// A missing value (setting not yet loaded) or a non-boolean value renders as `false`
/// rather than panicking, so the widget always has something safe to show.
pub(crate) fn switch_active_from_value(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

/// The index of the drop-down option matching the stored `value`, or `None` when no
/// option matches (an unknown token, or the setting not yet loaded).
pub(crate) fn dropdown_index_from_value(
    options: &[DropDownOption],
    value: Option<&Value>,
) -> Option<u32> {
    let token = value.and_then(Value::as_enum)?;
    options
        .iter()
        .position(|option| option.token == token)
        .map(|index| index as u32)
}

/// The slider position that shows the stored `value`, or `None` when it is absent.
///
/// Accepts both numeric kinds so the same helper serves a float and an integer scale;
/// an integer is widened to `f64` for display.
pub(crate) fn scale_position_from_value(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Float(number)) => Some(*number),
        Some(Value::Integer(number)) => Some(*number as f64),
        _ => None,
    }
}

/// The text a [`WidgetKind::Entry`] should show for the stored `value` (task 6.8).
///
/// A missing value (setting not yet loaded) or a non-string value renders as an empty
/// field rather than panicking, so the widget always has something safe to show.
pub(crate) fn entry_text_from_value(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// The ordered items a reorderable list should show for the stored `value`.
///
/// Splits the comma-joined string, trimming each item and dropping empties, so a
/// stored `us, se` or a trailing comma degrades to a clean item list rather than
/// producing blank entries. A missing value yields an empty list.
pub(crate) fn list_items_from_value(value: Option<&Value>) -> Vec<String> {
    match value.and_then(Value::as_str) {
        Some(text) => text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions() -> Vec<DropDownOption> {
        vec![
            DropDownOption::new("top-right", "Top right"),
            DropDownOption::new("top-left", "Top left"),
            DropDownOption::new("bottom-right", "Bottom right"),
        ]
    }

    #[test]
    fn widget_kind_compatibility_matches_the_setting_value_kind() {
        // A descriptor's widget kind must match the shape of Value its setting stores;
        // the framework relies on this so a widget's output is always the right kind.
        assert!(WidgetKind::Switch.is_compatible_with(ValueKind::Bool));
        assert!(!WidgetKind::Switch.is_compatible_with(ValueKind::Enum));

        let drop_down = WidgetKind::DropDown {
            options: positions(),
        };
        assert!(drop_down.is_compatible_with(ValueKind::Enum));
        assert!(!drop_down.is_compatible_with(ValueKind::String));

        let scale = WidgetKind::Scale {
            min: 0.0,
            max: 1.0,
            step: 0.1,
        };
        // A scale drives both numeric kinds.
        assert!(scale.is_compatible_with(ValueKind::Float));
        assert!(scale.is_compatible_with(ValueKind::Integer));
        assert!(!scale.is_compatible_with(ValueKind::Bool));

        let list = WidgetKind::ReorderableList {
            candidates: positions(),
        };
        assert!(list.is_compatible_with(ValueKind::String));
        assert!(!list.is_compatible_with(ValueKind::Enum));

        // A token-switch edits one flag of a comma-joined String setting.
        let token_switch = WidgetKind::TokenSwitch {
            token: "caps:escape".to_string(),
        };
        assert!(token_switch.is_compatible_with(ValueKind::String));
        assert!(!token_switch.is_compatible_with(ValueKind::Bool));

        // A text entry edits a free-text String setting (the lock command, task 6.8).
        assert!(WidgetKind::Entry.is_compatible_with(ValueKind::String));
        assert!(!WidgetKind::Entry.is_compatible_with(ValueKind::Integer));
    }

    #[test]
    fn descriptor_is_well_formed_when_widget_and_setting_agree() {
        // The representative descriptors used by the pages are self-consistent: the
        // widget kind matches the setting's value kind. A mismatch would be an
        // authoring bug this catches.
        let switch = RowDescriptor::new(
            "Natural scroll",
            WidgetKind::Switch,
            SettingId::TouchpadNaturalScroll,
            RowCapability::Always,
        );
        assert!(switch.is_well_formed());

        // MonitorMode is an Enum setting, so pairing it with a Switch is malformed.
        let malformed = RowDescriptor::new(
            "Mode",
            WidgetKind::Switch,
            SettingId::MonitorMode,
            RowCapability::Always,
        );
        assert!(!malformed.is_well_formed());
    }

    #[test]
    fn row_capability_gates_on_the_detected_capabilities() {
        let with_hyprctl = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let without = Capabilities::for_tests(&[], &[], false);

        // `Always` ignores capabilities; `Binary` follows the PATH scan.
        assert!(RowCapability::Always.is_present(&without));
        assert!(RowCapability::Binary(Binary::Hyprctl).is_present(&with_hyprctl));
        assert!(!RowCapability::Binary(Binary::Hyprctl).is_present(&without));
    }

    #[test]
    fn visible_rows_drops_gated_rows_and_keeps_order() {
        // Per-row gating (R4.2): a row whose capability is absent is filtered out; the
        // rest keep their declared order.
        let descriptors = vec![
            RowDescriptor::new(
                "Always shown",
                WidgetKind::Switch,
                SettingId::TouchpadNaturalScroll,
                RowCapability::Always,
            ),
            RowDescriptor::new(
                "Needs gsettings",
                WidgetKind::Switch,
                SettingId::TouchpadTapToClick,
                RowCapability::Binary(Binary::Gsettings),
            ),
        ];

        // gsettings absent: only the ungated row survives.
        let without = Capabilities::for_tests(&[], &[], false);
        let visible = visible_rows(&descriptors, &without);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].setting, SettingId::TouchpadNaturalScroll);

        // gsettings present: both rows, in order.
        let with = Capabilities::for_tests(&[Binary::Gsettings], &[], false);
        let visible = visible_rows(&descriptors, &with);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].setting, SettingId::TouchpadNaturalScroll);
        assert_eq!(visible[1].setting, SettingId::TouchpadTapToClick);
    }

    #[test]
    fn all_rows_gated_out_yields_an_empty_page_for_the_category_drop() {
        // The composition with task 5.1 (R4.2 "hide the whole category when emptied"):
        // when every row of a shown category is gated out, `visible_rows` is empty, and
        // the window builder treats that as an emptied category to drop.
        let descriptors = vec![RowDescriptor::new(
            "Needs swaync",
            WidgetKind::Switch,
            SettingId::TouchpadNaturalScroll,
            RowCapability::Binary(Binary::Swaync),
        )];
        let without = Capabilities::for_tests(&[], &[], false);
        assert!(visible_rows(&descriptors, &without).is_empty());
    }

    #[test]
    fn switch_value_round_trips() {
        assert_eq!(value_from_switch(true), Value::Bool(true));
        assert!(switch_active_from_value(Some(&Value::Bool(true))));
        assert!(!switch_active_from_value(Some(&Value::Bool(false))));
        // A missing or wrong-kind value renders as off, never panics.
        assert!(!switch_active_from_value(None));
        assert!(!switch_active_from_value(Some(&Value::Integer(1))));
    }

    #[test]
    fn dropdown_value_round_trips_through_the_token() {
        let options = positions();
        // Choosing an option stores its token, not its label.
        assert_eq!(
            value_from_dropdown_option(&options[2]),
            Value::Enum("bottom-right".to_string())
        );
        // Rendering finds the option index for the stored token.
        assert_eq!(
            dropdown_index_from_value(&options, Some(&Value::Enum("top-left".to_string()))),
            Some(1)
        );
        // An unknown token or a missing value selects nothing.
        assert_eq!(
            dropdown_index_from_value(&options, Some(&Value::Enum("centre".to_string()))),
            None
        );
        assert_eq!(dropdown_index_from_value(&options, None), None);
    }

    #[test]
    fn scale_value_coerces_to_the_setting_kind() {
        // A float setting keeps the fractional value.
        assert_eq!(value_from_scale(ValueKind::Float, 0.35), Value::Float(0.35));
        // An integer setting rounds to the nearest whole number — no `300.0`.
        assert_eq!(
            value_from_scale(ValueKind::Integer, 299.6),
            Value::Integer(300)
        );
        assert_eq!(
            value_from_scale(ValueKind::Integer, 300.4),
            Value::Integer(300)
        );

        // Rendering widens either kind back to a slider position.
        assert_eq!(
            scale_position_from_value(Some(&Value::Float(0.5))),
            Some(0.5)
        );
        assert_eq!(
            scale_position_from_value(Some(&Value::Integer(120))),
            Some(120.0)
        );
        assert_eq!(scale_position_from_value(None), None);
    }

    /// Three layouts, as owned `String`s, for the list-edit helper tests.
    fn layouts() -> Vec<String> {
        vec!["us".to_string(), "se".to_string(), "de".to_string()]
    }

    #[test]
    fn list_swap_reorders_and_is_a_no_op_at_the_ends() {
        let items = layouts();

        // A middle swap (move index 1 up to 0) reorders the two entries.
        assert_eq!(
            list_with_swapped(&items, 1, 0),
            Some(Value::String("se,us,de".to_string()))
        );
        // Move index 1 down to 2.
        assert_eq!(
            list_with_swapped(&items, 1, 2),
            Some(Value::String("us,de,se".to_string()))
        );

        // No-ops must return None so no redundant SetValue is emitted (N2): moving the
        // first item up (`to` underflows to usize::MAX), the last item down (`to`
        // runs off the end), and a swap with itself.
        assert_eq!(list_with_swapped(&items, 0, 0usize.wrapping_sub(1)), None);
        assert_eq!(list_with_swapped(&items, 2, 3), None);
        assert_eq!(list_with_swapped(&items, 1, 1), None);
        // An out-of-range `from` is also a no-op.
        assert_eq!(list_with_swapped(&items, 9, 0), None);
    }

    #[test]
    fn list_remove_deletes_by_index_and_can_empty_the_list() {
        let items = layouts();

        assert_eq!(
            list_without(&items, 1),
            Some(Value::String("us,de".to_string()))
        );
        // Removing the only remaining item yields the empty string (the empty list).
        assert_eq!(
            list_without(&["us".to_string()], 0),
            Some(Value::String(String::new()))
        );
        // An out-of-range index is a no-op.
        assert_eq!(list_without(&items, 9), None);
    }

    #[test]
    fn list_add_appends_and_ignores_duplicates() {
        let items = layouts();

        // A new token is appended at the end, preserving order.
        assert_eq!(
            list_with_added(&items, "fr"),
            Some(Value::String("us,se,de,fr".to_string()))
        );
        // A token already present is ignored (no duplicate, no emit), as is an empty
        // token.
        assert_eq!(list_with_added(&items, "se"), None);
        assert_eq!(list_with_added(&items, ""), None);
        // Adding to an empty list starts it.
        assert_eq!(
            list_with_added(&[], "us"),
            Some(Value::String("us".to_string()))
        );
    }

    #[test]
    fn token_switch_toggles_one_token_and_preserves_the_rest() {
        // The curated keyboard-option switch (task 6.6): toggling one token in/out of a
        // comma list must keep every other token — including one the app has no switch
        // for (`compose:ralt`) — in place and in order (R4.2 preserve-unknowns).
        let current = Value::String("grp:win_space_toggle,caps:escape,compose:ralt".to_string());

        // The `caps:escape` switch is on (present) and `numlock:on` is off (absent).
        assert!(token_switch_active_from_value(
            Some(&current),
            "caps:escape"
        ));
        assert!(!token_switch_active_from_value(
            Some(&current),
            "numlock:on"
        ));
        // A missing value reads as off, never panics.
        assert!(!token_switch_active_from_value(None, "caps:escape"));

        // Toggling `caps:escape` OFF removes only it; the unknown option is preserved.
        assert_eq!(
            value_from_token_toggle(Some(&current), "caps:escape", false),
            Value::String("grp:win_space_toggle,compose:ralt".to_string())
        );
        // Toggling a new token ON appends it, keeping the existing (unknown) tokens.
        assert_eq!(
            value_from_token_toggle(Some(&current), "numlock:on", true),
            Value::String("grp:win_space_toggle,caps:escape,compose:ralt,numlock:on".to_string())
        );
        // Toggling an already-present token ON is idempotent (no duplicate).
        assert_eq!(
            value_from_token_toggle(Some(&current), "caps:escape", true),
            current
        );
        // Toggling ON from an empty/absent value starts the list with just that token.
        assert_eq!(
            value_from_token_toggle(None, "caps:escape", true),
            Value::String("caps:escape".to_string())
        );
    }

    #[test]
    fn entry_value_round_trips_as_a_string() {
        // The free-text entry (task 6.8): the typed text is stored verbatim as a String,
        // and rendering shows the stored text back — a missing or non-string value is an
        // empty field, never a panic.
        assert_eq!(
            value_from_entry("pidof hyprlock || hyprlock"),
            Value::String("pidof hyprlock || hyprlock".to_string())
        );
        assert_eq!(value_from_entry(""), Value::String(String::new()));

        assert_eq!(
            entry_text_from_value(Some(&Value::String("hyprlock".to_string()))),
            "hyprlock"
        );
        assert_eq!(entry_text_from_value(None), "");
        assert_eq!(
            entry_text_from_value(Some(&Value::Integer(5))),
            "",
            "a non-string value renders as an empty field"
        );
    }

    #[test]
    fn reorderable_list_value_round_trips_as_a_comma_string() {
        // The ordered items serialize to Hyprland's comma-joined form and back,
        // preserving order.
        let items = vec!["us".to_string(), "se".to_string(), "de".to_string()];
        assert_eq!(
            value_from_list_items(&items),
            Value::String("us,se,de".to_string())
        );
        assert_eq!(
            list_items_from_value(Some(&Value::String("us,se,de".to_string()))),
            items
        );
        // Whitespace and empty fields are cleaned up; a missing value is an empty list.
        assert_eq!(
            list_items_from_value(Some(&Value::String("us, se ,,".to_string()))),
            vec!["us".to_string(), "se".to_string()]
        );
        assert!(list_items_from_value(None).is_empty());
    }
}
