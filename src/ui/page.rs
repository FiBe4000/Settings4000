//! The declarative page component: it turns a [`RowDescriptor`] list into live GTK
//! controls and runs the one-way `SetValue` → store → render loop (task 5.2;
//! architecture §7; R2.3, R4.2, R5.1, R5.2).
//!
//! # What this module is
//!
//! [`SettingsPage`] is the Relm4 component that renders a page from the declarative
//! descriptors in [`super::row`]. It is the concrete realisation of the framework's
//! contract:
//!
//! - **Build.** From a list of visible [`RowDescriptor`]s it constructs one control
//!   per row — a `GtkDropDown`, `GtkSwitch`, `GtkScale`, or the `GtkListBox`-backed
//!   reorderable editable list (R2.3) — via [`build_control`].
//! - **Emit.** On a user change a control emits a [`SetValue`] as the component's
//!   [`PageMsg::Set`] input. The component relays it to
//!   [`SettingsStore::stage`](crate::core::store::SettingsStore::stage), which either
//!   stages the edit ([`StageOutcome::Staged`], R5.1) or reports a runtime-only
//!   bypass ([`StageOutcome::RuntimeBypass`], R5.2). This is the whole side-effect of
//!   an edit — the control does not change its own state.
//! - **Render.** After every message Relm4 calls [`SettingsPage::update_view`], which
//!   re-renders **every** control purely from `store.value(id)`. So a control always
//!   shows the store's value, whatever changed it: its own edit, a Reset (R5.1), or a
//!   future external-file reload. The store is the single source of truth.
//!
//! # This framework renders file-backed settings only
//!
//! Because a control's displayed value is `store.value(id)`, and the
//! [`SettingsStore`](crate::core::store::SettingsStore) holds a value only for
//! *file-backed* settings, this framework is for **file-backed settings only**. A
//! runtime-only [`SettingId`] (its [`SettingId::backing`] is
//! [`Backing::RuntimeOnly`], e.g. the laptop-display toggle or the Sound page's
//! volume) has no store value — `store.value(id)` is always `None` — so a control
//! built for it would render as its default and, after any edit, snap back to that
//! default on the next `update_view`. Such controls must instead be built by their
//! own §6 page glue, which renders from the live runtime source (the state file,
//! `wpctl`/`pw-dump` state) and applies immediately. [`build_control`] guards this
//! with a debug assertion. Relatedly, the [`StageOutcome::RuntimeBypass`] arm in
//! [`SettingsPage::handle`] only *logs* today; wiring the actual runtime-apply relay
//! (e.g. issuing the `wpctl`/hotplug command) is deferred to §6.1/§6.2.
//!
//! # Why the render loop cannot feed back on itself (the GTK gotcha)
//!
//! Setting a widget property programmatically (e.g. `Switch::set_active`) makes GTK
//! emit the same "changed" signal a user interaction would, which would send another
//! [`SetValue`] and, for a genuinely changing value, could loop. Each scalar control
//! therefore keeps the [`glib::SignalHandlerId`] of its change handler and
//! **blocks** it around the programmatic set in [`BoundControl::render`], so a
//! render never masquerades as a user edit. The reorderable list needs no such
//! guard: it has no persistent change signal (its buttons are rebuilt each render),
//! and [`BoundList::render`] rebuilds only when the stored value actually differs
//! from what is shown.
//!
//! # Interim page content (replaced by §6 / task 5.4)
//!
//! [`category_rows`] returns a small, real descriptor list for the categories whose
//! settings already exist in the model, so the framework is exercised end-to-end and
//! renders in the running app. These lists — and the interim initial values in
//! [`demo_original`] that the window seeds the shared store with (see
//! [`interim_seed_values`]) — are deliberately interim scaffolding, exactly like task
//! 5.1's placeholder pages: the per-category §6 tasks supply the full control set
//! (with real option sources, ranges, and reload wiring), and task 5.4 replaces the
//! seeded values with the values parsed from the real config files. A category with no
//! descriptors yet keeps its task-5.1 placeholder.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Box as GtkBox, Button, DropDown, Label, ListBox, Orientation, Scale,
    SelectionMode, StringList, Switch, Widget, glib,
};
use relm4::{Component, ComponentParts, ComponentSender, Controller, SimpleComponent};

use crate::core::detect::{Binary, Capabilities};
use crate::core::model::{Backing, SettingId, Value, ValueKind};
use crate::core::store::{SettingsStore, StageError, StageOutcome};
use crate::ui::category::SidebarCategory;
use crate::ui::row::{
    DropDownOption, RowCapability, RowDescriptor, SetValue, WidgetKind, dropdown_index_from_value,
    list_items_from_value, list_with_added, list_with_swapped, list_without,
    scale_position_from_value, switch_active_from_value, value_from_dropdown_option,
    value_from_scale, value_from_switch, visible_rows,
};

/// Vertical spacing, in pixels, between rows on a page.
const ROW_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a label and its control (and between the
/// buttons of a reorderable-list row).
const CONTROL_SPACING: i32 = 8;

/// Outer margin, in pixels, around a page's rows.
const PAGE_MARGIN: i32 = 18;

/// The initialization payload for a [`SettingsPage`].
pub(crate) struct PageInit {
    /// The shared staging store, already seeded by the window (task 5.3). The page
    /// renders its controls from this store and stages edits into it; every page and
    /// the window chrome share the one store.
    store: Rc<RefCell<SettingsStore>>,
    /// The descriptors to build, already filtered to those whose capability is present
    /// (R4.2). Their interim initial values are seeded into the store from
    /// [`demo_original`].
    rows: Vec<RowDescriptor>,
    /// Invoked after each staged edit so the window refreshes the Apply/Reset chrome
    /// and the per-page markers from the new store state (task 5.3).
    on_changed: Rc<dyn Fn()>,
}

/// A message the page processes (architecture §7).
///
/// Both variants flow through the same `update` → `update_view` loop: [`PageMsg::Set`]
/// stages a user edit, and [`PageMsg::Rerender`] is a no-op update whose only purpose
/// is to make Relm4 re-run `update_view` so the controls re-render from the shared
/// store after the window changed it from outside a page (task 5.3).
#[derive(Debug)]
pub(crate) enum PageMsg {
    /// A control was changed by the user; stage the value in the store (R5.1/R5.2).
    Set(SetValue),
    /// Re-render every control from the shared store without mutating anything. The
    /// window broadcasts this to all pages after it changes the store from outside a
    /// page — a Reset, an applied commit, or a conflict reload — so the controls snap
    /// to the store's new values (task 5.3).
    Rerender,
}

/// The Relm4 component behind one settings page (architecture §7, R6.2).
///
/// The model holds only the [`SettingsStore`]: all rendering state lives in the
/// widgets, and every displayed value is derived from the store, so the model is
/// deliberately thin. The store-mutating logic is factored into [`Self::handle`] so
/// the `SetValue` → store round-trip is unit-tested without launching a GTK runtime.
pub(crate) struct SettingsPage {
    /// The staging store this page reads from and writes to (R5.1). It is the single
    /// store shared by every page and the window chrome (task 5.3): the window seeds
    /// and owns it behind an `Rc<RefCell<…>>`, each page stages edits into it and
    /// renders from it, and the chrome reads its dirty state to drive the Apply/Reset
    /// buttons and the per-page markers. Sharing is what makes an edit on one page
    /// light up the global Apply button and that page's marker (task 5.4 replaces the
    /// interim seed with values parsed from the real config files).
    store: Rc<RefCell<SettingsStore>>,
    /// Called after the page mutates the shared store, so the window can refresh the
    /// Apply/Reset sensitivity and the per-page dirty markers from the new state
    /// (task 5.3). It is a plain callback rather than a Relm4 output because the store
    /// is shared synchronously on the GTK main thread, so the chrome can read it the
    /// moment an edit lands.
    on_changed: Rc<dyn Fn()>,
}

impl SettingsPage {
    /// Applies a [`PageMsg::Set`] to the store, returning the stage result.
    ///
    /// This is the pure, GTK-free heart of the edit loop, called by
    /// [`SimpleComponent::update`] and exercised directly by the tests. A rejected
    /// edit (invalid value, R8.3) is logged and left unstaged; the subsequent
    /// re-render then snaps the control back to the stored value.
    fn handle(&mut self, set: SetValue) -> Result<StageOutcome, StageError> {
        let SetValue { id, value } = set;
        let outcome = self.store.borrow_mut().stage(id, value);
        match &outcome {
            Ok(StageOutcome::Staged) => {
                tracing::debug!(?id, "staged edit from widget");
            }
            Ok(StageOutcome::RuntimeBypass) => {
                // The store stages nothing for a runtime-only setting (R5.2). Today
                // this arm only logs; issuing the actual runtime-apply command (the
                // `wpctl` call, the laptop-display hotplug) is the §6.1/§6.2 page
                // glue's job. This framework does not build runtime-only controls
                // (see the module docs and `build_control`'s guard), so in practice
                // this arm is reached only via a direct `handle` call in tests.
                tracing::debug!(?id, "runtime-only edit bypassed staging (R5.2)");
            }
            Err(error) => {
                tracing::warn!(
                    ?id,
                    %error,
                    "widget edit rejected (R8.3); control will revert to the stored value"
                );
            }
        }
        outcome
    }
}

impl SimpleComponent for SettingsPage {
    type Init = PageInit;
    type Input = PageMsg;
    type Output = ();
    type Root = GtkBox;
    type Widgets = PageWidgets;

    fn init_root() -> Self::Root {
        GtkBox::builder()
            .orientation(Orientation::Vertical)
            .spacing(ROW_SPACING)
            .margin_top(PAGE_MARGIN)
            .margin_bottom(PAGE_MARGIN)
            .margin_start(PAGE_MARGIN)
            .margin_end(PAGE_MARGIN)
            .build()
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The single emit closure every control shares: a user change becomes a
        // `PageMsg::Set` input. Boxing it behind `Rc<dyn Fn>` keeps `super::row`
        // (and the control builders) free of the page's message type.
        let emit: Rc<dyn Fn(SetValue)> = {
            let sender = sender.clone();
            Rc::new(move |set_value| sender.input(PageMsg::Set(set_value)))
        };

        let mut controls = Vec::with_capacity(init.rows.len());
        for descriptor in &init.rows {
            let (row_widget, control) = build_control(descriptor, &emit);
            root.append(&row_widget);
            controls.push(control);
        }

        // Initial render: Relm4 does not call `update_view` after `init`, so render
        // each control from the pre-seeded shared store here (the render loop's
        // starting point).
        {
            let store = init.store.borrow();
            for control in &controls {
                control.render(store.value(control.setting()));
            }
        }

        ComponentParts {
            model: SettingsPage {
                store: init.store,
                on_changed: init.on_changed,
            },
            widgets: PageWidgets { controls },
        }
    }

    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>) {
        match message {
            PageMsg::Set(set) => {
                // The view refresh (`update_view`) reflects the result; the outcome is
                // consumed only by the tests, so it is ignored here.
                let _ = self.handle(set);
                // Tell the window an edit landed so it refreshes the Apply/Reset chrome
                // and this page's dirty marker from the new store state (task 5.3).
                (self.on_changed)();
            }
            // A no-op: `update_view` runs next and re-renders every control from the
            // shared store, which is the whole point of the broadcast (task 5.3).
            PageMsg::Rerender => {}
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        // Render every control from the store — the sole source of truth (R5.1). A
        // control whose stored value did not change re-renders to the same value,
        // which is a cheap no-op (and, for scalar controls, cannot feed back because
        // the change signal is blocked during the set).
        let store = self.store.borrow();
        for control in &widgets.controls {
            control.render(store.value(control.setting()));
        }
    }
}

/// The widgets Relm4 keeps for the page: one bound control per rendered row.
pub(crate) struct PageWidgets {
    /// The bound controls, in row order, each able to re-render itself from the store.
    controls: Vec<BoundControl>,
}

/// A control bound to a setting, holding the GTK widget(s) plus what it needs to
/// re-render itself from a stored [`Value`].
enum BoundControl {
    /// A `GtkSwitch` for a [`Value::Bool`].
    Switch {
        /// The setting this control edits.
        setting: SettingId,
        /// The switch widget.
        widget: Switch,
        /// The `notify::active` handler, blocked while rendering to avoid feedback.
        handler: glib::SignalHandlerId,
    },
    /// A `GtkDropDown` for a [`Value::Enum`].
    DropDown {
        /// The setting this control edits.
        setting: SettingId,
        /// The drop-down widget.
        widget: DropDown,
        /// The options, in display order, used to map a stored token to an index.
        options: Vec<DropDownOption>,
        /// The `notify::selected` handler, blocked while rendering to avoid feedback.
        handler: glib::SignalHandlerId,
    },
    /// A `GtkScale` for a [`Value::Float`] or [`Value::Integer`]. The change handler
    /// captures the setting's value kind, so the variant needs no `kind` field.
    Scale {
        /// The setting this control edits.
        setting: SettingId,
        /// The scale widget.
        widget: Scale,
        /// The `value-changed` handler, blocked while rendering to avoid feedback.
        handler: glib::SignalHandlerId,
    },
    /// A `GtkListBox`-backed reorderable editable list for a comma-joined
    /// [`Value::String`] (R2.3).
    List(BoundList),
}

impl BoundControl {
    /// The setting this control edits (used to look its value up in the store).
    fn setting(&self) -> SettingId {
        match self {
            BoundControl::Switch { setting, .. }
            | BoundControl::DropDown { setting, .. }
            | BoundControl::Scale { setting, .. } => *setting,
            BoundControl::List(list) => list.setting,
        }
    }

    /// Renders the control to show `value` from the store (R5.1).
    ///
    /// Scalar controls block their change signal around the programmatic set so the
    /// render cannot be mistaken for a user edit (see the module docs). A drop-down
    /// or scale with no matching/known value is left unchanged rather than forced to
    /// a default.
    fn render(&self, value: Option<&Value>) {
        match self {
            BoundControl::Switch {
                widget, handler, ..
            } => {
                widget.block_signal(handler);
                widget.set_active(switch_active_from_value(value));
                widget.unblock_signal(handler);
            }
            BoundControl::DropDown {
                widget,
                options,
                handler,
                ..
            } => {
                if let Some(index) = dropdown_index_from_value(options, value) {
                    widget.block_signal(handler);
                    widget.set_selected(index);
                    widget.unblock_signal(handler);
                }
            }
            BoundControl::Scale {
                widget, handler, ..
            } => {
                if let Some(position) = scale_position_from_value(value) {
                    widget.block_signal(handler);
                    widget.set_value(position);
                    widget.unblock_signal(handler);
                }
            }
            BoundControl::List(list) => list.render(value),
        }
    }
}

/// The reorderable editable list control (R2.3).
///
/// # Design: up/down buttons, rendered from the store
///
/// Reordering is done with per-row up/down buttons and a remove button, plus an
/// add-control (a drop-down of candidates and an Add button) — chosen over
/// drag-and-drop for a predictable, accessible interaction that is trivial to render
/// deterministically. Every edit (add/remove/move) reads the currently displayed
/// items, computes the new ordering, and emits a single [`SetValue`] with the
/// comma-joined result; the list does **not** mutate itself. [`Self::render`] then
/// rebuilds the rows from the store, so the store stays the source of truth. The
/// displayed items are cached in [`Self::displayed`] purely so an edit handler can
/// read the current ordering and so [`Self::render`] can skip a rebuild when nothing
/// changed — the cache is always overwritten from the store on render.
struct BoundList {
    /// The setting this list edits.
    setting: SettingId,
    /// The list box holding one row (label + up/down/remove) per item.
    list_box: ListBox,
    /// The shared emit closure, so the per-row buttons can send edits.
    emit: Rc<dyn Fn(SetValue)>,
    /// The items currently shown, kept in sync with the store by [`Self::render`].
    displayed: Rc<RefCell<Vec<String>>>,
}

impl BoundList {
    /// Renders the list to show `value` from the store, rebuilding the rows only when
    /// the ordering actually differs from what is shown.
    fn render(&self, value: Option<&Value>) {
        let items = list_items_from_value(value);
        if *self.displayed.borrow() == items {
            return;
        }
        self.rebuild(&items);
        *self.displayed.borrow_mut() = items;
    }

    /// Rebuilds the list-box rows for `items`, wiring each row's up/down/remove
    /// buttons to emit the resulting edit.
    fn rebuild(&self, items: &[String]) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        let count = items.len();
        for (index, item) in items.iter().enumerate() {
            let row = GtkBox::new(Orientation::Horizontal, CONTROL_SPACING);

            let label = Label::new(Some(item));
            label.set_halign(Align::Start);
            label.set_hexpand(true);
            // Centre each child vertically so the label and its buttons sit on a
            // common line regardless of their differing natural heights.
            label.set_valign(Align::Center);
            row.append(&label);

            // Up/down reorder by swapping with the neighbour; the ends are disabled.
            // Text buttons rather than symbolic-icon buttons: they carry no icon-theme
            // dependency (the app assumes nothing about the installed theme) and read
            // clearly regardless of the theme.
            let up = Button::with_label("Up");
            up.set_valign(Align::Center);
            up.set_sensitive(index > 0);
            self.connect_reorder(&up, index, index.wrapping_sub(1));
            row.append(&up);

            let down = Button::with_label("Down");
            down.set_valign(Align::Center);
            down.set_sensitive(index + 1 < count);
            self.connect_reorder(&down, index, index + 1);
            row.append(&down);

            let remove = Button::with_label("Remove");
            remove.set_valign(Align::Center);
            self.connect_remove(&remove, index);
            row.append(&remove);

            self.list_box.append(&row);
        }
    }

    /// Wires `button` to move the item at `from` to `to` (a neighbour swap) and emit
    /// the new ordering. The ordering math is the pure [`list_with_swapped`], which
    /// returns `None` for a no-op (an end button, or an out-of-range index) so no
    /// redundant edit is emitted.
    fn connect_reorder(&self, button: &Button, from: usize, to: usize) {
        let displayed = self.displayed.clone();
        let emit = self.emit.clone();
        let setting = self.setting;
        button.connect_clicked(move |_| {
            // Compute against the currently displayed items, releasing the borrow
            // before emitting (the emit does not touch `displayed`, but keeping the
            // borrow narrow is clearer).
            let edit = list_with_swapped(&displayed.borrow(), from, to);
            if let Some(value) = edit {
                emit(SetValue { id: setting, value });
            }
        });
    }

    /// Wires `button` to remove the item at `index` and emit the new ordering, via the
    /// pure [`list_without`] (a no-op out-of-range index emits nothing).
    fn connect_remove(&self, button: &Button, index: usize) {
        let displayed = self.displayed.clone();
        let emit = self.emit.clone();
        let setting = self.setting;
        button.connect_clicked(move |_| {
            let edit = list_without(&displayed.borrow(), index);
            if let Some(value) = edit {
                emit(SetValue { id: setting, value });
            }
        });
    }
}

/// Builds the labelled row and bound control for `descriptor`, wiring its change to
/// `emit` (R2.3).
fn build_control(
    descriptor: &RowDescriptor,
    emit: &Rc<dyn Fn(SetValue)>,
) -> (GtkBox, BoundControl) {
    // A descriptor whose widget kind does not match its setting's value kind is an
    // authoring bug (the widget would produce a Value of the wrong kind). Catch it in
    // debug builds rather than letting the mismatch reach the store's validation.
    debug_assert!(
        descriptor.is_well_formed(),
        "widget kind of {:?} is incompatible with its value kind",
        descriptor.setting
    );
    // This framework renders from `store.value(id)`, which only exists for file-backed
    // settings (see the module docs). A runtime-only descriptor would render as its
    // default and snap back after any edit, so it must not be built here — its §6 page
    // renders it from the live runtime source instead. Guard it in debug builds.
    debug_assert!(
        descriptor.setting.backing() == Backing::FileBacked,
        "the row framework renders file-backed settings only; {:?} is runtime-only \
         (R5.2) and needs bespoke §6 glue rendering from the runtime source",
        descriptor.setting
    );

    match &descriptor.widget {
        WidgetKind::Switch => build_switch(descriptor, emit),
        WidgetKind::DropDown { options } => build_dropdown(descriptor, options.clone(), emit),
        WidgetKind::Scale { min, max, step } => build_scale(descriptor, *min, *max, *step, emit),
        WidgetKind::ReorderableList { candidates } => {
            build_list(descriptor, candidates.clone(), emit)
        }
    }
}

/// Builds a `GtkSwitch` row.
fn build_switch(descriptor: &RowDescriptor, emit: &Rc<dyn Fn(SetValue)>) -> (GtkBox, BoundControl) {
    let widget = Switch::new();
    widget.set_halign(Align::End);
    widget.set_valign(Align::Center);

    let setting = descriptor.setting;
    let handler = {
        let emit = emit.clone();
        widget.connect_active_notify(move |switch| {
            emit(SetValue {
                id: setting,
                value: value_from_switch(switch.is_active()),
            });
        })
    };

    let row = labelled_row(&descriptor.label, &widget);
    (
        row,
        BoundControl::Switch {
            setting,
            widget,
            handler,
        },
    )
}

/// Builds a `GtkDropDown` row over `options`.
fn build_dropdown(
    descriptor: &RowDescriptor,
    options: Vec<DropDownOption>,
    emit: &Rc<dyn Fn(SetValue)>,
) -> (GtkBox, BoundControl) {
    let model = string_list(&options);
    let widget = DropDown::builder().model(&model).build();
    widget.set_halign(Align::End);
    widget.set_valign(Align::Center);

    let setting = descriptor.setting;
    let handler = {
        let emit = emit.clone();
        let options = options.clone();
        widget.connect_selected_notify(move |drop_down| {
            let index = drop_down.selected() as usize;
            if let Some(option) = options.get(index) {
                emit(SetValue {
                    id: setting,
                    value: value_from_dropdown_option(option),
                });
            }
        })
    };

    let row = labelled_row(&descriptor.label, &widget);
    (
        row,
        BoundControl::DropDown {
            setting,
            widget,
            options,
            handler,
        },
    )
}

/// Builds a `GtkScale` row over the range `min..=max` with increment `step`.
fn build_scale(
    descriptor: &RowDescriptor,
    min: f64,
    max: f64,
    step: f64,
    emit: &Rc<dyn Fn(SetValue)>,
) -> (GtkBox, BoundControl) {
    let setting = descriptor.setting;
    let kind = setting.kind();

    let adjustment = Adjustment::new(min, min, max, step, step, 0.0);
    let widget = Scale::new(Orientation::Horizontal, Some(&adjustment));
    widget.set_hexpand(true);
    widget.set_draw_value(true);
    // Whole-number settings show and snap to integers; fractional ones show two
    // decimals (enough for the sensitivity/scale granularity in use).
    if kind == ValueKind::Integer {
        widget.set_digits(0);
        widget.set_round_digits(0);
    } else {
        widget.set_digits(2);
        widget.set_round_digits(2);
    }

    let handler = {
        let emit = emit.clone();
        widget.connect_value_changed(move |scale| {
            emit(SetValue {
                id: setting,
                value: value_from_scale(kind, scale.value()),
            });
        })
    };

    let row = labelled_row(&descriptor.label, &widget);
    (
        row,
        BoundControl::Scale {
            setting,
            widget,
            handler,
        },
    )
}

/// Builds a reorderable editable list row (R2.3).
///
/// The row is laid out vertically: the label, the list box of current items, then an
/// add-control (a drop-down of `candidates` and an Add button). The rows themselves
/// are filled in by the first [`BoundList::render`].
fn build_list(
    descriptor: &RowDescriptor,
    candidates: Vec<DropDownOption>,
    emit: &Rc<dyn Fn(SetValue)>,
) -> (GtkBox, BoundControl) {
    let setting = descriptor.setting;

    let outer = GtkBox::new(Orientation::Vertical, CONTROL_SPACING);
    let heading = Label::new(Some(&descriptor.label));
    heading.set_halign(Align::Start);
    outer.append(&heading);

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::None);
    outer.append(&list_box);

    let displayed = Rc::new(RefCell::new(Vec::<String>::new()));

    // The add-control: pick a candidate and append it (duplicates are ignored, since
    // an ordered layout list has no meaning with repeats).
    let add_row = GtkBox::new(Orientation::Horizontal, CONTROL_SPACING);
    let add_model = string_list(&candidates);
    let add_dropdown = DropDown::builder().model(&add_model).build();
    add_dropdown.set_hexpand(true);
    let add_button = Button::with_label("Add");
    add_row.append(&add_dropdown);
    add_row.append(&add_button);
    outer.append(&add_row);

    {
        // The add-control owns the candidate list; the list rows only need the
        // current items (read from `displayed`), so `BoundList` keeps no copy. The
        // append + dedup math is the pure [`list_with_added`], which returns `None`
        // for a duplicate so no edit is emitted.
        let displayed = displayed.clone();
        let emit = emit.clone();
        add_button.connect_clicked(move |_| {
            let index = add_dropdown.selected() as usize;
            let Some(candidate) = candidates.get(index) else {
                return;
            };
            let edit = list_with_added(&displayed.borrow(), &candidate.token);
            if let Some(value) = edit {
                emit(SetValue { id: setting, value });
            }
        });
    }

    (
        outer,
        BoundControl::List(BoundList {
            setting,
            list_box,
            emit: emit.clone(),
            displayed,
        }),
    )
}

/// Builds a horizontal row: `label` on the left (taking the free space) and `control`
/// on the right.
fn labelled_row(label: &str, control: &impl IsA<Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, CONTROL_SPACING);
    let label = Label::new(Some(label));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    row.append(&label);
    row.append(control);
    row
}

/// Builds a `GtkStringList` of the options' display labels, in order.
fn string_list(options: &[DropDownOption]) -> StringList {
    let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
    StringList::new(&labels)
}

/// The interim declarative row list for `category` (empty when §6 has not defined the
/// category's controls yet).
///
/// See the module docs: these are real descriptors driving the framework, but their
/// option sets and ranges are scaffolding that §6 replaces. Only the categories whose
/// settings already exist in the model are populated; the rest fall back to task
/// 5.1's placeholder page.
///
/// The capability each row declares (R4.2) shows both variants: the touchpad toggles
/// are [`RowCapability::Always`] (no requirement beyond the Input category, which
/// task 5.1 already gated on `hyprctl`), while the rows that clearly depend on a
/// specific tool declare it with [`RowCapability::Binary`].
fn category_rows(category: SidebarCategory) -> Vec<RowDescriptor> {
    match category {
        SidebarCategory::Input => vec![
            RowDescriptor::new(
                "Keyboard layouts",
                WidgetKind::ReorderableList {
                    candidates: vec![
                        DropDownOption::new("us", "English (US)"),
                        DropDownOption::new("se", "Swedish"),
                        DropDownOption::new("de", "German"),
                        DropDownOption::new("fr", "French"),
                        DropDownOption::new("no", "Norwegian"),
                        DropDownOption::new("gb", "English (UK)"),
                    ],
                },
                SettingId::KeyboardLayouts,
                RowCapability::Binary(Binary::Hyprctl),
            ),
            RowDescriptor::new(
                "Mouse sensitivity",
                WidgetKind::Scale {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                },
                SettingId::MouseSensitivity,
                RowCapability::Binary(Binary::Hyprctl),
            ),
            RowDescriptor::new(
                "Natural scrolling",
                WidgetKind::Switch,
                SettingId::TouchpadNaturalScroll,
                RowCapability::Always,
            ),
            RowDescriptor::new(
                "Tap to click",
                WidgetKind::Switch,
                SettingId::TouchpadTapToClick,
                RowCapability::Always,
            ),
        ],
        SidebarCategory::Notifications => vec![
            RowDescriptor::new(
                "Position",
                WidgetKind::DropDown {
                    options: vec![
                        DropDownOption::new("top-right", "Top right"),
                        DropDownOption::new("top-left", "Top left"),
                        DropDownOption::new("top-center", "Top centre"),
                        DropDownOption::new("bottom-right", "Bottom right"),
                        DropDownOption::new("bottom-left", "Bottom left"),
                        DropDownOption::new("bottom-center", "Bottom centre"),
                    ],
                },
                SettingId::NotificationPosition,
                RowCapability::Binary(Binary::Swaync),
            ),
            RowDescriptor::new(
                "Auto-dismiss timeout (seconds)",
                WidgetKind::Scale {
                    min: 1.0,
                    max: 60.0,
                    step: 1.0,
                },
                SettingId::NotificationTimeout,
                RowCapability::Binary(Binary::Swaync),
            ),
        ],
        // The remaining categories have no framework rows yet; §6 fills them in, and
        // until then the window shows task 5.1's placeholder for them.
        _ => Vec::new(),
    }
}

/// The interim value a control initially shows for `setting`, before task 5.4 loads
/// the real value parsed from the config file.
///
/// This is placeholder scaffolding paired with [`category_rows`] (see the module
/// docs). The explicit arms cover every setting the interim rows use; the fallback
/// yields a kind-appropriate neutral value so a descriptor added before its seed is
/// still renderable rather than left unloaded.
pub(crate) fn demo_original(setting: SettingId) -> Value {
    match setting {
        SettingId::KeyboardLayouts => Value::String("us".to_string()),
        SettingId::MouseSensitivity => Value::Float(0.0),
        SettingId::TouchpadNaturalScroll => Value::Bool(false),
        SettingId::TouchpadTapToClick => Value::Bool(true),
        SettingId::NotificationPosition => Value::Enum("top-right".to_string()),
        SettingId::NotificationTimeout => Value::Integer(10),
        other => match other.kind() {
            ValueKind::Enum => Value::Enum(String::new()),
            ValueKind::Bool => Value::Bool(false),
            ValueKind::Float => Value::Float(0.0),
            ValueKind::Integer => Value::Integer(0),
            ValueKind::String => Value::String(String::new()),
        },
    }
}

/// Every interim setting the framework pages expose, paired with its
/// [`demo_original`] value, for the window to seed the shared store once at startup
/// (task 5.3).
///
/// It walks [`category_rows`] for every category — ignoring capabilities, so the store
/// holds a value for a setting even while its page is hidden (harmless, and it keeps a
/// later stage from failing `NotLoaded` if the page appears on a manual detection
/// refresh) — and deduplicates by [`SettingId`]. This whole seeding is interim
/// scaffolding: task 5.4 replaces it with values parsed from the real config files.
pub(crate) fn interim_seed_values() -> Vec<(SettingId, Value)> {
    let mut values: Vec<(SettingId, Value)> = Vec::new();
    for &category in SidebarCategory::ALL {
        for descriptor in category_rows(category) {
            if !values.iter().any(|(id, _)| *id == descriptor.setting) {
                values.push((descriptor.setting, demo_original(descriptor.setting)));
            }
        }
    }
    values
}

/// What a category's page should be, once per-row gating (R4.2) is applied to its
/// interim descriptor list — the pure, GTK-free half of the window's page-building
/// decision (task 5.3), split out so the composition is unit-tested headlessly (R6.2).
///
/// This is how the fine, per-row gate composes with task 5.1's coarse category gate
/// (see [`super::row`]): the window (`super::window`) asks [`plan_category`] for this
/// and either builds the framework page, drops the whole category when every row is
/// gated out, or falls back to the task-5.1 placeholder when the category has no
/// framework rows yet.
pub(crate) enum PagePlan {
    /// The category has visible framework rows; build a [`SettingsPage`] for them.
    Framework(Vec<RowDescriptor>),
    /// The category has framework rows, but all of them are gated out for the
    /// detected capabilities, so the whole category should be dropped (R4.2).
    Emptied,
    /// The category has no framework rows yet; the caller should use its task-5.1
    /// placeholder.
    NoSpec,
}

/// Decides how `category`'s page should be built under the detected `capabilities`,
/// composing per-row gating with task 5.1's category gate (R4.2).
///
/// A category with no interim descriptors yields [`PagePlan::NoSpec`]; one whose every
/// row is gated out yields [`PagePlan::Emptied`] (logged at `info` as a hidden
/// category); otherwise the visible rows are returned as [`PagePlan::Framework`] for
/// [`build_page`] to render. Keeping the decision GTK-free is what lets it be tested
/// without a display (R6.2).
pub(crate) fn plan_category(category: SidebarCategory, capabilities: &Capabilities) -> PagePlan {
    let descriptors = category_rows(category);
    if descriptors.is_empty() {
        return PagePlan::NoSpec;
    }

    // The per-row gate (R4.2). Its result composes with task 5.1's category gate: an
    // empty result means the category has been emptied and should be dropped.
    let visible = visible_rows(&descriptors, capabilities);
    if visible.is_empty() {
        tracing::info!(
            category = category.title(),
            "all rows for this category are gated out; hiding the whole category (R4.2)"
        );
        return PagePlan::Emptied;
    }

    PagePlan::Framework(visible)
}

/// Launches a [`SettingsPage`] for `rows`, sharing `store` and reporting each edit
/// through `on_changed` (task 5.3).
///
/// The returned [`Controller`] must be **kept alive** by the window: it owns the page's
/// widget (mounted in the stack via [`ComponentController::widget`](relm4::ComponentController::widget))
/// and is the handle the window sends [`PageMsg::Rerender`] through (via its
/// [`sender`](relm4::ComponentController::sender)) to re-render the page after a
/// Reset/commit/conflict-reload. This is why the task-5.2 `detach_runtime` call is
/// gone — the runtime is retained, not detached, so the page keeps processing both
/// user edits and the window's re-render broadcasts.
pub(crate) fn build_page(
    store: Rc<RefCell<SettingsStore>>,
    rows: Vec<RowDescriptor>,
    on_changed: Rc<dyn Fn()>,
) -> Controller<SettingsPage> {
    SettingsPage::builder()
        .launch(PageInit {
            store,
            rows,
            on_changed,
        })
        .detach()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::core::model::Category;
    use crate::core::store::{FileReader, FileValues};

    /// Builds a store loaded with `originals` under a synthetic key, with a reader that
    /// re-serves them. These tests never refresh or write, so the key is never resolved
    /// on disk — mirroring what the window's interim seeding does with a real temp file
    /// at runtime.
    fn test_store(originals: &[(SettingId, Value)]) -> SettingsStore {
        let values = originals.to_vec();
        let reader_values = values.clone();
        let reader: FileReader = Box::new(move |_path: &Path| -> io::Result<FileValues> {
            Ok(FileValues {
                bytes: Vec::new(),
                values: reader_values.clone(),
            })
        });
        let mut store = SettingsStore::new();
        store.load_file(
            PathBuf::from("<page test seed>"),
            FileValues {
                bytes: Vec::new(),
                values,
            },
            reader,
        );
        store
    }

    /// Builds a page model over a shared store seeded with `originals`, without
    /// launching a GTK runtime — so the `SetValue` → store round-trip is tested
    /// headlessly. The `on_changed` callback is a no-op here; the window supplies the
    /// real chrome-refreshing one.
    fn seeded_page(originals: &[(SettingId, Value)]) -> SettingsPage {
        SettingsPage {
            store: Rc::new(RefCell::new(test_store(originals))),
            on_changed: Rc::new(|| {}),
        }
    }

    #[test]
    fn set_message_stages_a_file_backed_edit_in_the_store() {
        // The core acceptance: a widget's SetValue reaches the store and is reflected
        // by `store.value` — the round-trip a rendered control then reads back.
        let mut page = seeded_page(&[(SettingId::NotificationTimeout, Value::Integer(10))]);
        assert!(!page.store.borrow().is_dirty());

        let outcome = page.handle(SetValue {
            id: SettingId::NotificationTimeout,
            value: Value::Integer(30),
        });
        assert_eq!(outcome.ok(), Some(StageOutcome::Staged));
        assert_eq!(
            page.store.borrow().value(SettingId::NotificationTimeout),
            Some(&Value::Integer(30)),
            "the store reflects the staged edit, which is what the control renders"
        );
        assert!(page.store.borrow().is_dirty());
        assert!(
            page.store
                .borrow()
                .is_category_dirty(Category::Notifications)
        );
    }

    #[test]
    fn runtime_only_edit_bypasses_staging_instead_of_being_stored() {
        // A runtime-only setting's SetValue is reported as a bypass (R5.2) and nothing
        // is staged — the store holds no value for it.
        let mut page = seeded_page(&[(SettingId::NotificationTimeout, Value::Integer(10))]);

        let outcome = page.handle(SetValue {
            id: SettingId::LaptopDisplayEnabled,
            value: Value::Bool(true),
        });
        assert_eq!(
            outcome.ok(),
            Some(StageOutcome::RuntimeBypass),
            "a runtime-only edit must bypass staging (R5.2)"
        );
        assert!(!page.store.borrow().is_dirty());
        assert!(
            page.store
                .borrow()
                .value(SettingId::LaptopDisplayEnabled)
                .is_none(),
            "the store holds no value for a runtime-only setting"
        );
    }

    #[test]
    fn the_store_is_the_render_source_across_a_change() {
        // The store's value is what `update_view` renders (R5.1). Prove the render
        // source tracks a change from either direction: a widget edit stages a new
        // value, and a store change from another source (here a reset, which task 5.3's
        // Reset button triggers) reverts it — so the control follows the store, never
        // its own state.
        let mut page = seeded_page(&[(SettingId::NotificationTimeout, Value::Integer(10))]);
        page.handle(SetValue {
            id: SettingId::NotificationTimeout,
            value: Value::Integer(45),
        })
        .expect("a valid edit stages");
        assert_eq!(
            page.store.borrow().value(SettingId::NotificationTimeout),
            Some(&Value::Integer(45)),
            "after an edit the render source is the staged value"
        );

        page.store.borrow_mut().reset();
        assert!(
            !page.store.borrow().is_dirty(),
            "a reset clears staged edits"
        );
        assert_eq!(
            page.store.borrow().value(SettingId::NotificationTimeout),
            Some(&Value::Integer(10)),
            "after a reset the render source — and thus the control — shows the original"
        );
    }

    #[test]
    fn an_invalid_edit_is_rejected_and_leaves_the_store_unchanged() {
        // A value that fails validation (R8.3) is not staged; the control then
        // re-renders to the unchanged stored value on the next `update_view`.
        let mut page = seeded_page(&[(SettingId::MouseSensitivity, Value::Float(0.0))]);

        let outcome = page.handle(SetValue {
            id: SettingId::MouseSensitivity,
            value: Value::Float(5.0), // outside the -1.0..=1.0 sensitivity range
        });
        assert!(outcome.is_err(), "an out-of-range value must be rejected");
        assert!(!page.store.borrow().is_dirty());
        assert_eq!(
            page.store.borrow().value(SettingId::MouseSensitivity),
            Some(&Value::Float(0.0)),
            "a rejected edit leaves the stored value untouched"
        );
    }

    #[test]
    fn category_rows_are_well_formed_with_matching_seeds() {
        // Guards the interim descriptor lists: every row's widget kind matches its
        // setting's value kind (so the framework builds the right control and stages
        // the right Value), and the seed `demo_original` provides for each setting is
        // of that same kind (so the control has a renderable original).
        for category in [SidebarCategory::Input, SidebarCategory::Notifications] {
            let descriptors = category_rows(category);
            assert!(
                !descriptors.is_empty(),
                "{category:?} should have interim rows"
            );
            for descriptor in &descriptors {
                assert!(
                    descriptor.is_well_formed(),
                    "{:?} pairs an incompatible widget and setting kind",
                    descriptor.setting
                );
                assert_eq!(
                    demo_original(descriptor.setting).kind(),
                    descriptor.setting.kind(),
                    "the seeded value kind must match the setting for {:?}",
                    descriptor.setting
                );
            }
        }

        // The categories without interim rows fall back to a placeholder.
        assert!(category_rows(SidebarCategory::Theme).is_empty());
    }

    #[test]
    fn interim_seed_values_cover_every_framework_setting_without_duplicates() {
        // The window seeds the shared store from this once at startup, so every
        // framework row must have a seed (else staging it would fail `NotLoaded`), and
        // no setting may appear twice.
        let seeds = interim_seed_values();
        for category in [SidebarCategory::Input, SidebarCategory::Notifications] {
            for descriptor in category_rows(category) {
                assert!(
                    seeds.iter().any(|(id, _)| *id == descriptor.setting),
                    "{:?} must be seeded",
                    descriptor.setting
                );
            }
        }
        let mut ids: Vec<SettingId> = seeds.iter().map(|(id, _)| *id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            count,
            "interim seed values must not duplicate a setting"
        );
    }

    #[test]
    fn plan_category_composes_the_two_gates_without_launching_gtk() {
        // The assembled three-way decision (task 5.1 category gate + task 5.2 per-row
        // gate). All branches asserted here are pure, so they run headlessly (no
        // display needed).

        // NoSpec: a category with no framework descriptors yet (Theme today) — the
        // window shows its task-5.1 placeholder.
        let any = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Swaync], &[], true);
        assert!(matches!(
            plan_category(SidebarCategory::Theme, &any),
            PagePlan::NoSpec
        ));

        // Framework: a category whose rows are (at least partly) present.
        assert!(matches!(
            plan_category(SidebarCategory::Input, &any),
            PagePlan::Framework(_)
        ));

        // Emptied: Notifications' rows all require swaync; with swaync absent every row
        // is hidden, so the whole category is dropped (R4.2) rather than shown empty.
        let no_swaync = Capabilities::for_tests(&[], &[], false);
        assert!(matches!(
            plan_category(SidebarCategory::Notifications, &no_swaync),
            PagePlan::Emptied
        ));
    }
}
