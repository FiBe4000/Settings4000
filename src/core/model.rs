//! Typed settings model and per-setting validators (task 4.1; architecture §3
//! "Write safety", §6 step 1; R8.3, R6.2).
//!
//! # What this module is
//!
//! Every value the app can edit is modelled here as a typed [`Value`] addressed
//! by a stable [`SettingId`], plus the validators that decide whether a proposed
//! value is safe to write. It is the shared vocabulary the rest of `core/` builds
//! on: the [`SettingsStore`](crate::core) (task 4.2) keeps an `original` and a
//! `staged` [`Value`] per [`SettingId`], and the Apply pipeline (task 4.5) calls
//! [`SettingId::validate`] on every staged value *before* it writes anything to
//! disk.
//!
//! # Why validation lives here (R8.3)
//!
//! The overriding non-functional requirement is that the app must **never break a
//! working desktop**: an invalid value must be rejected in memory, never written
//! into a live config. R8.3 names the concrete checks — hex color format, monitor
//! mode strings (`WxH@Hz`), numeric ranges for timeouts/sensitivity/scale, and
//! that a wallpaper/lock-background path exists, is readable, and is an image.
//! This module implements exactly those, each as a small pure function returning a
//! [`ValidationError`] whose [`Display`](std::fmt::Display) is a message the UI can
//! show verbatim. Because it is part of `core/` it is GTK-free and headlessly
//! testable (R6.2) — the layering guard in `tests/module_boundaries.rs` forbids any
//! `gtk`/`relm4` import here.
//!
//! # How a category page adds a setting
//!
//! [`SettingId`] is the single, central registry of every editable setting. A
//! category page (§6) that needs a new setting adds one [`SettingId`] variant and
//! extends the three per-setting mappings to describe it:
//!
//! - [`SettingId::kind`] — the [`ValueKind`] the setting stores (so the store and
//!   UI know which widget to build and how to coerce input);
//! - [`SettingId::category`] — which [`Category`] it belongs to (so the store can
//!   roll dirty state up per page);
//! - [`SettingId::backing`] — whether the setting is staged to a file until Apply
//!   (R5.1) or applied to the live session immediately, bypassing staging (R5.2),
//!   so the store and the UI dispatcher route the edit correctly;
//! - the match in [`SettingId::validate`] — which validator (if any) guards it.
//!
//! Keeping this in one enum, rather than scattering ad-hoc string keys across
//! pages, is what lets the store use a [`SettingId`] as a map key and the pipeline
//! validate any staged value uniformly by its id.
//!
//! # Scope of the hex validator
//!
//! [`validate_hex_color`] accepts only the palette's **bare** six-hex-digit form
//! (no `#`, no `rgb()`), reusing [`crate::parsers::palette::is_bare_hex`] so there
//! is a single definition of "a palette color". The `#rrggbb` and `rgb(...)` forms
//! that appear elsewhere in the dotfiles live only in the *generated* color files
//! (analysis §2), which the app never writes — so no writable value ever needs
//! those forms and they are intentionally out of scope here.

use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::core::home_path::{self, expand_home, is_home_relative};
use crate::core::lock_grace::GraceError;
use crate::core::notifd_config;
use crate::core::value_writers::{self, Unwritable, ValueWriter};
use crate::parsers::palette::is_bare_hex;

/// A top-level settings category — one entry per sidebar page (R2.4, requirements
/// §3).
///
/// [`SettingId::category`] maps each setting to its page so the store (task 4.2)
/// can compute a per-page "modified" indicator by grouping dirty settings.
///
/// Only pages that own at least one staged, validated setting appear here. Sound
/// and Network are deliberately absent: they are runtime-only (R3.1, R5.2) — their
/// controls apply immediately via system commands and hold no staged [`Value`], so
/// there is no whole page for this model to represent. A page adds its variant here
/// when it introduces its first staged [`SettingId`].
///
/// A page can still be file-backed *and* carry an individual runtime-only control:
/// the laptop-display toggle lives on the file-backed [`Display`](Self::Display)
/// page yet is itself runtime-only (see [`SettingId::backing`]), so it is modelled
/// as a [`SettingId`] for routing but never contributes a staged value or dirty
/// state to its category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Per-monitor display settings backed by `monitors.conf` (task 6.1).
    Display,
    /// Palette, GTK/icon/cursor themes, wallpaper and lock background (tasks
    /// 6.3–6.5).
    Theme,
    /// Keyboard, mouse, and touchpad settings backed by `input.lua` (tasks 6.6, 11.4).
    Input,
    /// notifd's popup timeouts, history size, critical-popup cap, hold time and 2FA
    /// copy switch, backed by `~/.config/notifd/config` (task 11.15). The page's
    /// do-not-disturb, count and clear controls are runtime-only and are not part of
    /// this category's staged state (R5.2).
    Notifications,
    /// Idle timeouts, the lock command and its grace period, backed by `hypridle.conf`
    /// (tasks 6.8, 11.13).
    PowerAndIdle,
}

/// Whether a setting's value is staged to a config file or applied to the live
/// session immediately.
///
/// This is the routing marker that the [`SettingsStore`](crate::core) (task 4.2)
/// and the UI dispatcher use to decide, for a given edit, between *staging* it until
/// the user clicks Apply (R5.1) and *applying it immediately*, bypassing staging
/// entirely (R5.2). Because it is an intrinsic, unchanging property of the setting
/// (audio volume is always live; a monitor mode is always file-backed), it is
/// declared once on [`SettingId`] via [`SettingId::backing`] rather than being
/// decided per edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// The value is written to a config file and only takes effect on Apply. The
    /// store keeps it as an `original`/`staged` pair, and it contributes to dirty
    /// state and the per-page rollup (R5.1).
    FileBacked,
    /// The value is pushed to the running session immediately via a system command
    /// and touches no config file, so it is never staged and never dirty (R5.2) —
    /// e.g. audio volume/mute or the laptop-display toggle. The store holds no
    /// value for such a setting.
    RuntimeOnly,
}

/// A stable identifier for a single editable setting.
///
/// This is the key type the whole `core/` layer is organised around: the store
/// (task 4.2) maps each id to its `original`/`staged` [`Value`], and the Apply
/// pipeline (task 4.5) validates a staged value by looking its id up here. It is a
/// fieldless enum so it is cheap to copy, hash, and order — the properties a map
/// key and deterministic iteration need.
///
/// The variants below are a representative cross-section covering every
/// [`ValueKind`] and every validator, not an exhaustive list of the final UI. Each
/// category page (§6) extends the enum with its remaining settings; see the module
/// docs for the three mappings a new variant must be added to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingId {
    // --- Display (task 6.1) ---
    /// A monitor's mode, e.g. `2880x1800@120` — chosen from the modes the output
    /// reports, so it is a [`ValueKind::Enum`] validated as a `WxH@Hz` string.
    MonitorMode,
    /// A monitor's fractional scale factor (e.g. `1.333333`).
    MonitorScale,
    /// A monitor's position in the multi-monitor layout: either an `auto…` token
    /// (Hyprland places the output relative to the others) or an explicit `XxY`
    /// pixel coordinate pair, e.g. `0x0` or `-1920x0`. A [`ValueKind::Enum`] because
    /// the Display page offers it as a drop-down, but validated as a position string
    /// all the same — the model, not the drop-down's option list, is the layer that
    /// guarantees a writable value (R8.3).
    MonitorPosition,
    /// Whether the laptop's internal display is enabled. Runtime-only (R5.2): the
    /// toggle runs the dotfiles' `scripts/hypr-toggle-laptop-display`, which drives
    /// the existing hotplug mechanism (the `/tmp/hypr-laptop-display-forced` state
    /// file) itself (task 11.8), and applies immediately, so it is never staged and
    /// never dirty. It is modelled as a
    /// [`SettingId`] purely so the UI can route it by [`SettingId::backing`]; the
    /// store (task 4.2) never holds an `original`/`staged` value for it.
    LaptopDisplayEnabled,

    // --- Theme (tasks 6.3–6.5) ---
    /// A single palette color as bare hex. Not surfaced by the v1 UI — palette
    /// *switching* runs `generate-colors` rather than editing individual colors
    /// (requirements §9) — but modelled and validated so the task-3.1 palette
    /// write path and any future per-key editor have a typed, guarded home.
    PaletteColor,
    /// The desktop wallpaper image path (`hyprpaper.conf`).
    WallpaperPath,
    /// The lock-screen background image path (`hyprlock.conf`).
    LockBackgroundPath,
    /// The GTK theme name (`gtk-theme-name` in both GTK `settings.ini` files; also the
    /// directory the GTK 4 theme links point into, task 11.16).
    ///
    /// This and the next three are staged by the Theme page's own model rather than the
    /// store (see [`crate::core::theme`] for why). They are ids nonetheless so that the
    /// model's Apply contribution can hand them to the pipeline's step 1 like every
    /// other staged value, and so that one validator serves both the stage-time and the
    /// Apply-time check (task 9.27, R8.3). A [`ValueKind::Enum`]: the value is picked
    /// from a drop-down of scanned themes.
    GtkTheme,
    /// The icon theme name (`gtk-icon-theme-name` in both GTK `settings.ini` files). See
    /// [`Self::GtkTheme`] on why a bespoke-model value has an id.
    IconTheme,
    /// The cursor theme name, written identically to both `settings.ini` files,
    /// `uwsm/env`, the `hl.env("XCURSOR_THEME", …)` line of `hyprland.lua` and the
    /// XCursor fallback `~/.icons/default/index.theme` (R3.4). See [`Self::GtkTheme`] on
    /// why a bespoke-model value has an id.
    CursorTheme,
    /// The cursor size in pixels, held as the drop-down's text (`"24"`) and written to
    /// the same copies as [`Self::CursorTheme`] except `index.theme`, which has no size.
    /// See [`Self::GtkTheme`] on why a bespoke-model value has an id.
    CursorSize,

    // --- Input (task 6.6) ---
    /// The ordered keyboard layout list, serialized to Hyprland's comma-joined
    /// `kb_layout` value (e.g. `us,se`). Modelled as a [`ValueKind::String`] because
    /// that is the on-disk shape; the Input page edits it through the declarative
    /// reorderable list widget (task 5.2, R2.3), which splits/joins on commas. The
    /// order is significant — it is the layout switch order — so this cannot be a
    /// plain set. Validated to require **at least one entry** (an empty `kb_layout`
    /// would make Hyprland fall back to its default layout, R8.3); per-item XKB-validity
    /// is not checked (the Input page sources candidates from the XKB registry, so an
    /// added item is a real layout, and an unknown-but-present code is left to Hyprland).
    KeyboardLayouts,
    /// The keyboard option list, kept as Hyprland's raw comma-joined `kb_options`
    /// value (e.g. `grp:win_space_toggle,caps:escape`). Modelled as the whole
    /// [`ValueKind::String`] rather than one setting per option so that options the
    /// Input page (task 6.6) has no curated switch for are **preserved verbatim** on
    /// an edit: the page's curated switches (e.g. `caps:escape`) toggle a single
    /// token in and out of this string while leaving every other token untouched.
    /// Free-text like [`SettingId::KeyboardLayouts`], so it is unvalidated beyond its
    /// kind — an unrecognised option token is left to Hyprland to accept or ignore.
    KeyboardOptions,
    /// Mouse/touchpad sensitivity — Hyprland's `sensitivity`, clamped to
    /// `-1.0..=1.0`.
    MouseSensitivity,
    /// Touchpad natural (reverse) scrolling.
    TouchpadNaturalScroll,
    /// Touchpad tap-to-click.
    TouchpadTapToClick,

    // --- Notifications (task 11.15) ---
    //
    // The seven keys of `~/.config/notifd/config`. Their key names, ranges and defaults
    // are notifd's own and live in one table, `core::notifd_config::NOTIFD_SETTINGS`.
    /// How long a low-urgency popup stays, in milliseconds; `0` keeps it until it is
    /// dismissed (`timeout_low_ms`). An application's own expiry always wins.
    NotifdTimeoutLow,
    /// How long a normal-urgency popup stays, in milliseconds; `0` = until dismissed
    /// (`timeout_normal_ms`).
    NotifdTimeoutNormal,
    /// How long a critical popup stays, in milliseconds; `0` = until dismissed
    /// (`timeout_critical_ms`, `0` by default).
    NotifdTimeoutCritical,
    /// How many notifications the history (the notification center) keeps; at least 1
    /// (`history_max`).
    NotifdHistoryMax,
    /// How many critical popups one application may hold on screen at once, `1..=5` —
    /// the number of popup slots (`critical_per_app`).
    NotifdCriticalPerApp,
    /// How long a popup stays at least after the pointer leaves it, in milliseconds
    /// (`hold_min_ms`).
    NotifdHoldMin,
    /// Whether a notification carrying a two-factor code offers a "Copy" action
    /// (`copy_codes`).
    NotifdCopyCodes,

    // --- Power & Idle (task 6.8) ---
    /// Idle seconds before the screen dims (`hypridle.conf` listener).
    DimTimeout,
    /// Idle seconds before the session locks.
    LockTimeout,
    /// Idle seconds before displays are switched off (DPMS).
    DpmsTimeout,
    /// The command hypridle runs to lock the session (`general.lock_cmd`) — free text,
    /// validated only against bytes that would break the hyprlang line.
    LockCommand,
    /// hyprlock's grace period in whole seconds: how long after locking any input
    /// unlocks again without a password, `0` locking immediately (task 11.13).
    ///
    /// hyprlock ≥ 0.9 reads it only from its command line, so on disk it is not a key
    /// of its own but the `--grace N` token inside [`Self::LockCommand`]'s value
    /// (analysis §7, §8). It is therefore a second view of the same `general.lock_cmd`
    /// value: the loader derives it with [`grace_of`](crate::core::lock_grace::grace_of),
    /// the store keeps the two in step whenever either is staged (see
    /// [`SettingsStore::stage`](crate::core::store::SettingsStore::stage)), and the
    /// writer applies it as a token edit on the lock command
    /// ([`render_hypridle_conf`](crate::core::power::render_hypridle_conf)). Validated
    /// against [`LOCK_GRACE_SECONDS_RANGE`]. A grace already on disk beyond that range
    /// loads verbatim and blocks lock-command edits until it is lowered. The store's
    /// `linked_edit` documents this corner case.
    LockGracePeriod,
}

impl SettingId {
    /// The [`ValueKind`] this setting stores.
    ///
    /// A single source of truth for the "expected" kind: [`Self::validate`]
    /// reports a [`ValidationError::WrongKind`] against exactly this value, and the
    /// UI/store use it to build the right widget and coerce input.
    pub fn kind(&self) -> ValueKind {
        match self {
            SettingId::MonitorMode
            | SettingId::MonitorPosition
            | SettingId::GtkTheme
            | SettingId::IconTheme
            | SettingId::CursorTheme
            | SettingId::CursorSize => ValueKind::Enum,
            SettingId::MonitorScale | SettingId::MouseSensitivity => ValueKind::Float,
            SettingId::DimTimeout
            | SettingId::LockTimeout
            | SettingId::DpmsTimeout
            | SettingId::LockGracePeriod
            | SettingId::NotifdTimeoutLow
            | SettingId::NotifdTimeoutNormal
            | SettingId::NotifdTimeoutCritical
            | SettingId::NotifdHistoryMax
            | SettingId::NotifdCriticalPerApp
            | SettingId::NotifdHoldMin => ValueKind::Integer,
            SettingId::TouchpadNaturalScroll
            | SettingId::TouchpadTapToClick
            | SettingId::LaptopDisplayEnabled
            | SettingId::NotifdCopyCodes => ValueKind::Bool,
            SettingId::PaletteColor
            | SettingId::WallpaperPath
            | SettingId::LockBackgroundPath
            | SettingId::KeyboardLayouts
            | SettingId::KeyboardOptions
            | SettingId::LockCommand => ValueKind::String,
        }
    }

    /// The [`Category`] (sidebar page) this setting belongs to.
    ///
    /// Used by the store (task 4.2) to roll per-setting dirty state up into a
    /// per-page "modified" marker.
    pub fn category(&self) -> Category {
        match self {
            SettingId::MonitorMode
            | SettingId::MonitorScale
            | SettingId::MonitorPosition
            | SettingId::LaptopDisplayEnabled => Category::Display,
            SettingId::PaletteColor
            | SettingId::WallpaperPath
            | SettingId::LockBackgroundPath
            | SettingId::GtkTheme
            | SettingId::IconTheme
            | SettingId::CursorTheme
            | SettingId::CursorSize => Category::Theme,
            SettingId::KeyboardLayouts
            | SettingId::KeyboardOptions
            | SettingId::MouseSensitivity
            | SettingId::TouchpadNaturalScroll
            | SettingId::TouchpadTapToClick => Category::Input,
            SettingId::NotifdTimeoutLow
            | SettingId::NotifdTimeoutNormal
            | SettingId::NotifdTimeoutCritical
            | SettingId::NotifdHistoryMax
            | SettingId::NotifdCriticalPerApp
            | SettingId::NotifdHoldMin
            | SettingId::NotifdCopyCodes => Category::Notifications,
            SettingId::DimTimeout
            | SettingId::LockTimeout
            | SettingId::DpmsTimeout
            | SettingId::LockCommand
            | SettingId::LockGracePeriod => Category::PowerAndIdle,
        }
    }

    /// Whether this setting is staged to a file or applied to the live session
    /// immediately (R5.1/R5.2).
    ///
    /// See [`Backing`]. The store (task 4.2) calls this at the point an edit is
    /// staged: a [`Backing::RuntimeOnly`] setting bypasses staging and is applied
    /// at once, so it never becomes dirty. The match is exhaustive on purpose —
    /// adding a [`SettingId`] variant without classifying it fails to compile, which
    /// forces every new setting to declare how it reaches the system.
    pub fn backing(&self) -> Backing {
        match self {
            // Runtime-only: applied to the live session immediately, never staged
            // (R5.2). The laptop-display toggle drives the hotplug state file
            // directly rather than editing a config.
            SettingId::LaptopDisplayEnabled => Backing::RuntimeOnly,
            // Everything else is written to a config file and staged until Apply.
            SettingId::MonitorMode
            | SettingId::MonitorScale
            | SettingId::MonitorPosition
            | SettingId::PaletteColor
            | SettingId::WallpaperPath
            | SettingId::LockBackgroundPath
            | SettingId::GtkTheme
            | SettingId::IconTheme
            | SettingId::CursorTheme
            | SettingId::CursorSize
            | SettingId::KeyboardLayouts
            | SettingId::KeyboardOptions
            | SettingId::MouseSensitivity
            | SettingId::TouchpadNaturalScroll
            | SettingId::TouchpadTapToClick
            | SettingId::NotifdTimeoutLow
            | SettingId::NotifdTimeoutNormal
            | SettingId::NotifdTimeoutCritical
            | SettingId::NotifdHistoryMax
            | SettingId::NotifdCriticalPerApp
            | SettingId::NotifdHoldMin
            | SettingId::NotifdCopyCodes
            | SettingId::DimTimeout
            | SettingId::LockTimeout
            | SettingId::DpmsTimeout
            | SettingId::LockCommand
            | SettingId::LockGracePeriod => Backing::FileBacked,
        }
    }

    /// Validates `value` against this setting's rules, returning a described error
    /// if it is unsafe to write (R8.3).
    ///
    /// This is the entry point the Apply pipeline (task 4.5) calls for every staged
    /// value before any file is touched. It first requires `value` to be the
    /// [`kind`](Self::kind) this setting expects — a mismatch is a
    /// [`ValidationError::WrongKind`], which should not happen if the UI stages the
    /// right widget's output but is guarded rather than assumed — and then applies
    /// the setting-specific validator. Settings that are constrained purely by the
    /// UI (a boolean switch, or a drop-down whose options are all valid) have no
    /// value-format rule and pass once their kind matches.
    ///
    /// It never panics: an invalid value, a wrong kind, or a filesystem error while
    /// checking a path all return an [`Err`], never an unwind.
    pub fn validate(&self, value: &Value) -> Result<(), ValidationError> {
        // Matching the (id, value) pair together dispatches to the right validator
        // and, in the final arm, turns any leftover (i.e. wrong-kind) pairing into a
        // WrongKind error — so the kind check and the dispatch cannot drift apart.
        match (self, value) {
            (SettingId::PaletteColor, Value::String(text)) => validate_hex_color(text),
            // The value may be the config's own `~/…` form (task 11.10). The wallpaper
            // model already hands over expanded paths against its injected home; reading
            // `$HOME` here covers any other caller with the same rule.
            (SettingId::WallpaperPath, Value::String(text)) => {
                validate_wallpaper_value(text, home_path::home_dir().as_deref())
            }
            (SettingId::LockBackgroundPath, Value::String(text)) => {
                validate_lock_background_value(text, home_path::home_dir().as_deref())
            }
            // Theme names: whatever the scan offered, as long as every file the name is
            // written to can hold it (task 9.27). The Theme model runs this same check
            // when the user picks a name, so step 1 only catches a value that reached
            // the staged state some other way.
            (
                SettingId::GtkTheme | SettingId::IconTheme | SettingId::CursorTheme,
                Value::Enum(name),
            ) => value_writers::check_writable(*self, name),
            (SettingId::CursorSize, Value::Enum(size)) => validate_cursor_size(size),
            (SettingId::MonitorMode, Value::Enum(token)) => validate_monitor_mode(token),
            (SettingId::MonitorPosition, Value::Enum(token)) => validate_monitor_position(token),
            (SettingId::MonitorScale, Value::Float(scale)) => {
                validate_float_range(*scale, &SCALE_RANGE)
            }
            (SettingId::MouseSensitivity, Value::Float(sensitivity)) => {
                validate_float_range(*sensitivity, &SENSITIVITY_RANGE)
            }
            (
                SettingId::DimTimeout | SettingId::LockTimeout | SettingId::DpmsTimeout,
                Value::Integer(seconds),
            ) => validate_int_range(*seconds, &TIMEOUT_SECONDS_RANGE),
            (SettingId::LockGracePeriod, Value::Integer(seconds)) => {
                validate_int_range(*seconds, &LOCK_GRACE_SECONDS_RANGE)
            }
            // notifd ignores a value outside its range and silently uses the default, so
            // the daemon's own range is enforced here, at stage time (task 11.15, R8.3).
            (
                SettingId::NotifdTimeoutLow
                | SettingId::NotifdTimeoutNormal
                | SettingId::NotifdTimeoutCritical
                | SettingId::NotifdHistoryMax
                | SettingId::NotifdCriticalPerApp
                | SettingId::NotifdHoldMin,
                Value::Integer(number),
            ) => notifd_config::validate_integer(*self, *number),
            // No value-format rule: a boolean switch, a free-text command, or a
            // drop-down whose every option is valid. The kind match above is the
            // only constraint. `LaptopDisplayEnabled` is runtime-only (R5.2) and
            // never reaches the Apply pipeline, but is validated for its kind here
            // so the whole enum has uniform, guarded validation.
            (
                SettingId::TouchpadNaturalScroll
                | SettingId::TouchpadTapToClick
                | SettingId::LaptopDisplayEnabled
                | SettingId::NotifdCopyCodes,
                Value::Bool(_),
            ) => Ok(()),
            // The keyboard-layout list must have at least one entry (R8.3): writing an
            // empty `kb_layout=` makes Hyprland silently fall back to its default
            // layout, discarding the user's configured layouts — so an empty list (the
            // user removed every entry) is rejected and the control reverts. The XKB
            // per-item validity check is not done here (it needs the registry); an
            // unknown-but-present layout code is left to Hyprland.
            (SettingId::KeyboardLayouts, Value::String(text)) => validate_layout_list(text),
            // The hypridle lock command is free text, but it is written into a hyprlang
            // value, so it must not contain a byte that would break that config — caught
            // here at stage time (R8.3) so the free-text Entry (task 6.8) reverts the
            // control on the offending keystroke, rather than only failing the whole Apply.
            (SettingId::LockCommand, Value::String(text)) => validate_command(text),
            // The keyboard-option list is not free text: it is fed from curated tokens and
            // on-disk values (task 6.6), an opaque comma-joined string whose unknown entries
            // are preserved verbatim and which may legitimately be empty (no options set),
            // so it has no format rule beyond its kind.
            (SettingId::KeyboardOptions, Value::String(_)) => Ok(()),
            // Anything left over is a value whose kind does not match the setting.
            (id, value) => Err(ValidationError::WrongKind {
                expected: id.kind(),
                found: value.kind(),
            }),
        }
    }
}

/// The kind of data a [`Value`] carries — the type-level discriminant of the
/// value union.
///
/// Kept separate from [`Value`] so a [`SettingId`] can declare its expected kind
/// (see [`SettingId::kind`]) without carrying data, and so a
/// [`ValidationError::WrongKind`] can name the expected and found kinds in a
/// human-readable way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// A choice from a fixed set, presented as a drop-down (e.g. a monitor mode or
    /// position). Distinguished from [`ValueKind::String`] to signal
    /// that only a member of a known set is meaningful, even though it is carried
    /// as text.
    Enum,
    /// A boolean, presented as a toggle switch.
    Bool,
    /// A genuinely fractional number, e.g. a monitor scale or input sensitivity.
    Float,
    /// A whole number, e.g. an idle timeout in seconds. Kept distinct from
    /// [`ValueKind::Float`] so values that must be integral cannot acquire a
    /// fractional part and are emitted as `300`, never `300.0`, into configs that
    /// expect an integer (analysis §4, §6.4).
    Integer,
    /// Free-form text, e.g. a file path or a shell command.
    String,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ValueKind::Enum => "choice",
            ValueKind::Bool => "boolean",
            ValueKind::Float => "decimal number",
            ValueKind::Integer => "whole number",
            ValueKind::String => "text",
        };
        f.write_str(name)
    }
}

/// A typed setting value.
///
/// The store (task 4.2) holds one of these per [`SettingId`] for both the
/// `original` (as read from disk) and the `staged` (edited) state, and computes
/// dirtiness by comparing them. It derives [`PartialEq`] but not [`Eq`] because
/// [`Value::Float`] wraps an [`f64`], which is only partially ordered; equality
/// comparison is all the store needs.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A choice token from a fixed set (see [`ValueKind::Enum`]).
    Enum(String),
    /// A boolean.
    Bool(bool),
    /// A fractional number.
    Float(f64),
    /// A whole number.
    Integer(i64),
    /// Free-form text.
    String(String),
}

impl Value {
    /// The [`ValueKind`] discriminant of this value.
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Enum(_) => ValueKind::Enum,
            Value::Bool(_) => ValueKind::Bool,
            Value::Float(_) => ValueKind::Float,
            Value::Integer(_) => ValueKind::Integer,
            Value::String(_) => ValueKind::String,
        }
    }

    /// The choice token if this is a [`Value::Enum`], else `None`.
    pub fn as_enum(&self) -> Option<&str> {
        match self {
            Value::Enum(token) => Some(token),
            _ => None,
        }
    }

    /// The boolean if this is a [`Value::Bool`], else `None`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// The number if this is a [`Value::Float`], else `None`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(number) => Some(*number),
            _ => None,
        }
    }

    /// The number if this is a [`Value::Integer`], else `None`.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(number) => Some(*number),
            _ => None,
        }
    }

    /// The text if this is a [`Value::String`], else `None`.
    ///
    /// Note this is deliberately *not* satisfied by [`Value::Enum`]: an enum token
    /// is retrieved with [`Self::as_enum`], keeping the two kinds distinct.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }
}

/// Why a proposed [`Value`] was rejected, with a [`Display`](std::fmt::Display)
/// message suitable for showing directly in the UI (R8.3).
///
/// Does not derive [`Clone`]/[`PartialEq`] because [`ImagePathProblem::Unreadable`]
/// carries a [`std::io::Error`], which implements neither. Callers match on the
/// variant (with [`matches!`]) or render the message; they do not need to clone or
/// compare errors.
#[derive(Debug)]
pub enum ValidationError {
    /// The value's kind does not match what the setting expects. A guard against a
    /// programming error (the UI staging the wrong widget's output), not something
    /// a user can normally trigger.
    WrongKind {
        /// The kind the setting requires.
        expected: ValueKind,
        /// The kind that was supplied.
        found: ValueKind,
    },
    /// A color value is not the palette's bare six-hex-digit form.
    NotBareHexColor {
        /// The offending value as supplied.
        value: String,
    },
    /// A list-valued setting that requires at least one entry is empty. Today only the
    /// keyboard-layout list uses this: an empty `kb_layout` would make Hyprland fall
    /// back to its default layout, silently dropping the user's configuration (R8.3).
    EmptyKeyboardLayouts,
    /// A free-text command destined for a hyprlang value contains a byte that would break
    /// that config: a newline/carriage return (splits the line) or a `#` (hyprlang reads
    /// it as an inline comment and silently truncates the value). Today only the hypridle
    /// lock command uses this (task 6.8, R8.3).
    UnsafeCommand {
        /// The offending value as supplied.
        value: String,
    },
    /// A grace period was staged, but the lock command it would be written into offers
    /// no `--grace N` token the app can edit — it does not start hyprlock, or starts it
    /// in a shape the token editor refuses (task 11.13; see
    /// [`crate::core::lock_grace`]). Raised by the store, which knows the lock command,
    /// rather than by [`SettingId::validate`], which sees one value at a time.
    LockGraceUnavailable(GraceError),
    /// A monitor mode string is not a recognised `WxH@Hz` mode (or special token),
    /// or its numbers are outside the plausible range.
    InvalidMonitorMode {
        /// The offending mode string as supplied.
        value: String,
        /// A human-readable explanation of what is wrong with it.
        detail: String,
    },
    /// A monitor position string is neither one of Hyprland's `auto…` placement
    /// tokens nor an `XxY` pixel coordinate pair within the plausible range.
    InvalidMonitorPosition {
        /// The offending position string as supplied.
        value: String,
        /// A human-readable explanation of what is wrong with it.
        detail: String,
    },
    /// A numeric value falls outside the setting's allowed range. The bounds are
    /// pre-formatted so an integer setting reports `1`/`86400` and a float setting
    /// reports `0.5`/`3` without spurious decimals.
    OutOfRange {
        /// The offending value, formatted.
        value: String,
        /// The inclusive lower bound, formatted.
        min: String,
        /// The inclusive upper bound, formatted.
        max: String,
    },
    /// A notifd setting is outside the range notifd accepts for its key, so the daemon
    /// would ignore the line and silently use its default (task 11.15, R8.3). Names the
    /// key and the rule, because the reason (e.g. `critical_per_app` is capped by the
    /// number of popup slots) is the daemon's, not an app preference.
    NotifdOutOfRange {
        /// The key in `~/.config/notifd/config`.
        key: &'static str,
        /// The refused value.
        value: i64,
        /// The smallest value notifd accepts for the key.
        min: i64,
        /// The largest value notifd accepts for the key.
        max: i64,
    },
    /// A Theme-page value contains something at least one of the files (or the live
    /// request) it is written to cannot hold, so writing it would leave some copies on
    /// the new value and others on the old one (task 9.27, R8.3, R3.4). Raised by
    /// [`value_writers::check_writable`], which names the writers that refuse it.
    Unwritable {
        /// The refused value, as supplied.
        value: String,
        /// What about it is refused.
        problem: Unwritable,
        /// Every writer that refuses it, in the order the message lists them.
        writers: Vec<ValueWriter>,
    },
    /// A cursor size is not a whole number of pixels greater than zero. GTK reads
    /// `gtk-cursor-theme-size` and Xcursor reads `XCURSOR_SIZE` as integers, and
    /// `hyprctl setcursor` needs one, so any other text would be written to four files
    /// and ignored by all of them (task 9.27).
    InvalidCursorSize {
        /// The refused size, as supplied.
        value: String,
    },
    /// A wallpaper/lock-background path is not a readable image file.
    ImagePath {
        /// The path that was checked.
        path: PathBuf,
        /// The specific problem with it.
        problem: ImagePathProblem,
    },
}

/// The specific reason an image path failed [`validate_image_path`].
///
/// Split out so the UI can phrase an accurate message — "does not exist" versus
/// "not an image" versus "cannot be read" are different fixes for the user.
#[derive(Debug)]
pub enum ImagePathProblem {
    /// Nothing exists at the path.
    DoesNotExist,
    /// Something exists there, but it is not a regular file (e.g. a directory).
    NotAFile,
    /// The file exists but does not have a supported image extension. Carries the
    /// lower-cased extension found, or `None` when the path has no extension.
    UnsupportedExtension {
        /// The extension found (lower-cased), or `None` if there was none.
        extension: Option<String>,
    },
    /// The file exists but could not be opened for reading (e.g. permission
    /// denied). Carries the underlying OS error.
    Unreadable(io::Error),
    /// The path is home-relative (`~/…`) but the home directory is unknown (`$HOME`
    /// unset), so there is no file to check. Reported instead of probing the literal
    /// `~/…` as a path relative to the working directory (task 11.10).
    HomeUnknown,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::WrongKind { expected, found } => {
                write!(f, "expected a {expected} value but got a {found} value")
            }
            ValidationError::NotBareHexColor { value } => write!(
                f,
                "`{value}` is not a valid color: expected six hexadecimal digits, e.g. 83c092"
            ),
            ValidationError::EmptyKeyboardLayouts => {
                write!(f, "at least one keyboard layout is required")
            }
            ValidationError::UnsafeCommand { value } => write!(
                f,
                "`{value}` cannot be used as a command: it must not contain a line break or `#`"
            ),
            ValidationError::LockGraceUnavailable(error) => {
                write!(f, "no grace period can be set: {error}")
            }
            ValidationError::InvalidMonitorMode { value, detail } => {
                write!(f, "`{value}` is not a valid display mode: {detail}")
            }
            ValidationError::InvalidMonitorPosition { value, detail } => {
                write!(f, "`{value}` is not a valid display position: {detail}")
            }
            ValidationError::OutOfRange { value, min, max } => {
                write!(f, "{value} is outside the allowed range {min} to {max}")
            }
            ValidationError::NotifdOutOfRange {
                key,
                value,
                min,
                max,
            } => write!(
                f,
                "{key}={value} would be ignored by notifd, which then uses its default: it \
                 must be a whole number from {min} to {max}"
            ),
            ValidationError::Unwritable {
                value,
                problem,
                writers,
            } => {
                let value = value_writers::printable(value);
                let writers_text = value_writers::join_writers(writers);
                match problem {
                    Unwritable::Character(character) => write!(
                        f,
                        "`{value}` cannot be used: it contains {}, which {writers_text} \
                         cannot hold",
                        value_writers::describe_character(*character)
                    ),
                    Unwritable::NotABareShellWord {
                        character: Some(character),
                    } => write!(
                        f,
                        "`{value}` cannot be used: it contains {}, which {writers_text} \
                         cannot hold — uwsm/env is read by the login shell, so the value \
                         must be one plain word without spaces or shell special characters",
                        value_writers::describe_character(*character)
                    ),
                    Unwritable::NotABareShellWord { character: None } => write!(
                        f,
                        "`{value}` cannot be used: {writers_text} need a non-empty word, \
                         because uwsm/env is read by the login shell"
                    ),
                    Unwritable::NotASingleDirectoryName => write!(
                        f,
                        "`{value}` cannot be used: {writers_text} need a single directory \
                         name (no `/`, and not `.` or `..`)"
                    ),
                }
            }
            ValidationError::InvalidCursorSize { value } => write!(
                f,
                "`{}` is not a cursor size: expected a whole number of pixels greater \
                 than zero, for example 24",
                value_writers::printable(value)
            ),
            ValidationError::ImagePath { path, problem } => {
                let path = path.display();
                match problem {
                    ImagePathProblem::DoesNotExist => write!(f, "no file exists at {path}"),
                    ImagePathProblem::NotAFile => write!(f, "{path} is not a regular file"),
                    ImagePathProblem::UnsupportedExtension { extension } => match extension {
                        Some(extension) => write!(
                            f,
                            "{path} is a `.{extension}` file, not a supported image \
                             ({})",
                            supported_image_extensions()
                        ),
                        None => write!(
                            f,
                            "{path} has no file extension; expected a supported image ({})",
                            supported_image_extensions()
                        ),
                    },
                    ImagePathProblem::Unreadable(error) => {
                        write!(f, "{path} cannot be read: {error}")
                    }
                    ImagePathProblem::HomeUnknown => write!(
                        f,
                        "{path} is relative to the home directory, but the home directory \
                         is unknown ($HOME is not set)"
                    ),
                }
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Hyprland clamps input `sensitivity` to `-1.0..=1.0`; a value outside that is
/// silently clamped by the compositor, so we reject it up front to keep the stored
/// value and the effective value in agreement (analysis §4).
pub const SENSITIVITY_RANGE: RangeInclusive<f64> = -1.0..=1.0;

/// Plausible monitor scale factors. Hyprland fractional scaling is used at values
/// such as `1.066667` and `1.333333` in the real dotfiles (analysis §4). This is a
/// sanity bound to reject obviously-broken input (a zero, negative, or absurd
/// scale) before it reaches a working config, not an exact per-monitor limit.
/// Hyprland's stricter rule — that a scale must divide the resolution into a whole
/// number of logical pixels — needs the monitor's resolution, so it is deferred to
/// the Display page (task 6.1); passing this range check is necessary but not
/// sufficient.
pub const SCALE_RANGE: RangeInclusive<f64> = 0.5..=3.0;

/// Allowed idle timeouts, in whole seconds: strictly positive (R8.3 "timeout
/// ranges"), up to one day. The upper bound is a sanity ceiling for an idle timeout
/// (dim/lock/DPMS in `hypridle.conf`); the
/// requirement asks only that timeouts be positive with a sensible maximum.
pub const TIMEOUT_SECONDS_RANGE: RangeInclusive<i64> = 1..=86_400;

/// Allowed hyprlock grace periods, in whole seconds (task 11.13, R8.3).
///
/// `0` is valid and means "lock immediately" (hyprlock's behaviour without a grace
/// flag). The ceiling of ten minutes is a safety bound rather than a hyprlock limit:
/// during the grace period *any* mouse movement or key press unlocks the session
/// without a password, so a longer grace would quietly leave an apparently locked
/// session open. The Power & Idle page's spin control covers exactly this range, and
/// a grace period typed into the free-text lock command is held to it as well.
pub const LOCK_GRACE_SECONDS_RANGE: RangeInclusive<i64> = 0..=600;

/// Plausible pixels per axis for a monitor resolution. `16384` is generous
/// headroom beyond current panels (8K is 7680×4320); the point is to reject a
/// zero, negative, or absurd dimension, not to encode a hardware table.
const RESOLUTION_DIMENSION_RANGE: RangeInclusive<u32> = 1..=16_384;

/// Plausible refresh rates in hertz. Real panels reach a few hundred hertz today;
/// `1000` is headroom. Like the resolution range this is a sanity bound, not an
/// exact limit.
const REFRESH_HZ_RANGE: RangeInclusive<f64> = 1.0..=1_000.0;

/// The non-numeric monitor mode tokens Hyprland accepts in place of an explicit
/// `WxH@Hz`. The Display page (task 6.1) may offer these as drop-down choices, so
/// the mode validator accepts them verbatim rather than trying to parse them as a
/// resolution.
const SPECIAL_MODE_TOKENS: &[&str] = &["preferred", "highres", "highrr"];

/// The placement tokens Hyprland accepts in a `monitor=` rule's position field in
/// place of explicit `XxY` coordinates, per its monitor documentation: plain `auto`
/// (place the output next to the existing ones, anchored at their top-left corners),
/// the four directional forms, and the four centre-anchored forms.
///
/// The Display page (task 6.1) only *offers* `auto` and `0x0`, but a hand-written
/// `monitors.conf` may legitimately carry any of these — and an edit to any field of a
/// monitor re-validates its whole staged record — so the position validator must accept
/// them all rather than fail an Apply over a value the app never chose.
const MONITOR_POSITION_TOKENS: &[&str] = &[
    "auto",
    "auto-right",
    "auto-left",
    "auto-up",
    "auto-down",
    "auto-center-right",
    "auto-center-left",
    "auto-center-up",
    "auto-center-down",
];

/// Plausible pixel coordinates for one axis of a monitor position. A position may be
/// negative (an output placed left of or above the origin), and the layout canvas can
/// legitimately be several 8K panels wide, so this is a generous sanity bound whose
/// only job is to reject an implausible typo — not to encode a hardware limit.
///
/// The bound is an [`i64`] range so that a wildly out-of-range coordinate is reported by
/// this check (with the range in the message) rather than by the axis parser as a
/// spurious "not a whole number".
const POSITION_AXIS_RANGE: RangeInclusive<i64> = -32_768..=32_768;

/// File extensions accepted for a wallpaper/lock-background image (matched
/// case-insensitively).
///
/// These are the common raster formats hyprpaper and hyprlock load. The set is
/// intentionally conservative — additional formats (e.g. `bmp`) can be added here
/// if a backend is confirmed to support them — so that a path with a clearly
/// non-image extension is rejected before Apply (R8.3).
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// The supported image extensions rendered as a human-readable list for error
/// messages, e.g. `png, jpg, jpeg, webp`.
fn supported_image_extensions() -> String {
    IMAGE_EXTENSIONS.join(", ")
}

/// Validates that `value` is the palette's bare six-hex-digit color form
/// (R8.3, analysis §2).
///
/// Reuses [`crate::parsers::palette::is_bare_hex`] so the model validator and the
/// palette writer agree on exactly what a color is. See the module docs for why
/// `#rrggbb`/`rgb(...)` forms are out of scope.
pub fn validate_hex_color(value: &str) -> Result<(), ValidationError> {
    if is_bare_hex(value) {
        Ok(())
    } else {
        Err(ValidationError::NotBareHexColor {
            value: value.to_string(),
        })
    }
}

/// Validates the keyboard-layout list: it must contain at least one non-empty entry
/// (R8.3).
///
/// The value is Hyprland's comma-joined `kb_layout` string (e.g. `us,se`). A value that
/// is empty, or only whitespace and separators (`""`, `","`, `" , "`), has no real
/// layout and would write a bare `kb_layout=`, which Hyprland replaces with its default
/// — silently losing the user's configured layouts. Splitting on commas and requiring a
/// non-empty trimmed field matches how the Input page's reorderable list tokenises the
/// value, so the two agree on what "at least one layout" means.
pub fn validate_layout_list(value: &str) -> Result<(), ValidationError> {
    if value.split(',').any(|item| !item.trim().is_empty()) {
        Ok(())
    } else {
        Err(ValidationError::EmptyKeyboardLayouts)
    }
}

/// Validates a free-text command that will be written into a hyprlang value (R8.3).
///
/// Rejects a value containing a newline/carriage return (which would split the config
/// line) or a `#` (which hyprlang reads as the start of an inline comment, silently
/// truncating the value). This mirrors the hyprlang writer's own `reject_unsafe_value`
/// guard ([`crate::parsers::hyprlang`]) so an unsafe value is caught at **stage** time —
/// when the row framework's free-text entry (task 6.8) reverts the control on the
/// offending keystroke — rather than only surfacing as an aborted Apply. The writer keeps
/// its guard as defense-in-depth. Today the hypridle lock command is the only setting
/// that reaches this validator; a command is otherwise unconstrained (any program and
/// arguments are allowed).
pub fn validate_command(value: &str) -> Result<(), ValidationError> {
    if value.contains(['\n', '\r', '#']) {
        Err(ValidationError::UnsafeCommand {
            value: value.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Validates a monitor mode string (R8.3).
///
/// Accepts any of:
///
/// - one of the [`SPECIAL_MODE_TOKENS`] (`preferred`, `highres`, `highrr`)
///   verbatim;
/// - a **bare** `WIDTHxHEIGHT` resolution such as `2560x1440`. Hyprland accepts a
///   mode with no refresh and picks a default rate, and the live `monitors.conf`
///   carries exactly this form (analysis §4:
///   `monitor=desc:AU Optronics 0x2036,2560x1440,auto,1.066667`). Rejecting it
///   would block a legitimate Apply — e.g. a scale-only edit on the Display page
///   (task 6.1) whose mode field is already a bare `WxH` on disk;
/// - a full `WIDTHxHEIGHT@REFRESH` mode such as `2880x1800@120`.
///
/// In every resolution form the width and height must be plain positive integers
/// within [`RESOLUTION_DIMENSION_RANGE`]. When (and only when) an `@REFRESH` suffix
/// is present, the refresh must be a finite positive number (integer or decimal,
/// e.g. `59.951`) within [`REFRESH_HZ_RANGE`]; a bare `WxH` skips the refresh
/// check entirely.
///
/// The numeric bounds are deliberately loose plausibility checks (see the range
/// constants): the goal is to reject a broken string (`0x0`, a typo, or a non-mode)
/// before it reaches a working `monitors.conf`, not to reproduce the monitor's
/// exact capability list.
pub fn validate_monitor_mode(value: &str) -> Result<(), ValidationError> {
    if SPECIAL_MODE_TOKENS.contains(&value) {
        return Ok(());
    }

    // Builds an `InvalidMonitorMode` carrying the offending value and a specific
    // explanation; used at each failure point below.
    let bad = |detail: &str| ValidationError::InvalidMonitorMode {
        value: value.to_string(),
        detail: detail.to_string(),
    };

    // A concrete mode is `WIDTHxHEIGHT` with an *optional* `@REFRESH` suffix. Split
    // the refresh off first (if any), so a bare `WxH` — which Hyprland accepts and
    // which appears in the live monitors.conf — is handled with `refresh = None`.
    let (resolution, refresh) = match value.split_once('@') {
        Some((resolution, refresh)) => (resolution, Some(refresh)),
        None => (value, None),
    };

    let Some((width, height)) = resolution.split_once('x') else {
        return Err(bad(
            "the resolution must be WIDTHxHEIGHT, for example 2880x1800",
        ));
    };

    let width =
        parse_dimension(width).ok_or_else(|| bad("the width must be a whole number of pixels"))?;
    let height = parse_dimension(height)
        .ok_or_else(|| bad("the height must be a whole number of pixels"))?;
    if !RESOLUTION_DIMENSION_RANGE.contains(&width) || !RESOLUTION_DIMENSION_RANGE.contains(&height)
    {
        return Err(bad(&format!(
            "resolution {width}x{height} is outside the plausible range \
             ({}-{} pixels per axis)",
            RESOLUTION_DIMENSION_RANGE.start(),
            RESOLUTION_DIMENSION_RANGE.end()
        )));
    }

    // Validate the refresh only when the mode actually carried an `@REFRESH`
    // suffix; a bare `WxH` leaves the rate to Hyprland and needs no check.
    if let Some(refresh) = refresh {
        let refresh: f64 = refresh
            .parse()
            .map_err(|_| bad("the refresh rate must be a number, for example 120 or 59.951"))?;
        if !refresh.is_finite() || !REFRESH_HZ_RANGE.contains(&refresh) {
            return Err(bad(&format!(
                "refresh rate {refresh} Hz is outside the plausible range ({}-{} Hz)",
                REFRESH_HZ_RANGE.start(),
                REFRESH_HZ_RANGE.end()
            )));
        }
    }

    Ok(())
}

/// Parses one resolution axis: a plain positive integer with no sign, whitespace,
/// or decimal point. Returns `None` for anything else (including on overflow),
/// leaving the caller to raise a specific error.
fn parse_dimension(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok()
}

/// Validates a monitor position field (R8.3).
///
/// Accepts either one of the [`MONITOR_POSITION_TOKENS`] verbatim, or an explicit
/// `XxY` coordinate pair whose axes are whole numbers (optionally negative) within
/// [`POSITION_AXIS_RANGE`] — `0x0`, `2880x0`, `-1920x-1080`.
///
/// Two properties matter beyond "Hyprland understands it". The position is written
/// into a `monitor=` record whose positional fields are comma-separated and which
/// `scripts/hypr-display-profile.sh` parses with `awk` to derive the laptop panel's
/// mode and scale (analysis §6.2): a comma in the value would shift every later field
/// and desync that script, and a `#` would turn the rest of the line into a hyprlang
/// inline comment, truncating the record. Both are impossible for a value that passes
/// this check, so validating the *format* is what upholds the awk-parseability
/// guarantee. The [`monitors`](crate::parsers::monitors) writer keeps its own
/// character guard as defense in depth; catching the value here means the Display
/// page's control reverts at **stage** time instead of the whole Apply aborting.
pub fn validate_monitor_position(value: &str) -> Result<(), ValidationError> {
    if MONITOR_POSITION_TOKENS.contains(&value) {
        return Ok(());
    }

    // Builds an `InvalidMonitorPosition` carrying the offending value and a specific
    // explanation; used at each failure point below.
    let bad = |detail: &str| ValidationError::InvalidMonitorPosition {
        value: value.to_string(),
        detail: detail.to_string(),
    };

    let Some((x, y)) = value.split_once('x') else {
        return Err(bad("expected `auto` or XxY coordinates, for example 0x0"));
    };
    let x =
        parse_axis(x).ok_or_else(|| bad("the X coordinate must be a whole number of pixels"))?;
    let y =
        parse_axis(y).ok_or_else(|| bad("the Y coordinate must be a whole number of pixels"))?;
    if !POSITION_AXIS_RANGE.contains(&x) || !POSITION_AXIS_RANGE.contains(&y) {
        return Err(bad(&format!(
            "position {x}x{y} is outside the plausible range ({} to {} pixels per axis)",
            POSITION_AXIS_RANGE.start(),
            POSITION_AXIS_RANGE.end()
        )));
    }
    Ok(())
}

/// Parses one position axis: a whole number with an optional leading `-` and nothing
/// else — no `+`, whitespace, decimal point, or thousands separator. Returns `None` only
/// when the text is not a whole number at all, leaving the caller to raise a specific
/// error.
///
/// The width is [`i64`] so that "is it a number?" and "is it in range?" stay separate
/// questions: a coordinate far outside [`POSITION_AXIS_RANGE`] still parses here and is
/// rejected by the range check, which can name the bounds, instead of being misreported
/// as not a number. Digits beyond [`i64`]'s own range are the one remaining overflow
/// case, and they are so far out of range that either message is accurate.
fn parse_axis(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok()
}

/// Validates that a fractional `value` lies within the inclusive `range` (R8.3).
///
/// A non-finite value (`NaN`, infinity) is outside every finite range and so is
/// rejected — [`RangeInclusive::contains`] compares with `NaN` as `false` and
/// infinity against the finite bounds, both of which fail the check.
pub fn validate_float_range(
    value: f64,
    range: &RangeInclusive<f64>,
) -> Result<(), ValidationError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::OutOfRange {
            value: value.to_string(),
            min: range.start().to_string(),
            max: range.end().to_string(),
        })
    }
}

/// Validates that a whole-number `value` lies within the inclusive `range` (R8.3).
pub fn validate_int_range(value: i64, range: &RangeInclusive<i64>) -> Result<(), ValidationError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::OutOfRange {
            value: value.to_string(),
            min: range.start().to_string(),
            max: range.end().to_string(),
        })
    }
}

/// Validates a wallpaper/lock-background image path: it must **exist**, be a
/// **regular file**, have a **supported image extension**, and be **readable**
/// (R8.3).
///
/// Metadata is read with [`std::fs::metadata`], which follows symlinks, so a path
/// that points at an image through a symlink validates on the real target — the
/// same live-path philosophy the writer uses (R8.5). The distinct failure modes
/// are reported separately (see [`ImagePathProblem`]) so the UI can tell the user
/// precisely what to fix. Any filesystem error is turned into an [`Err`]; this
/// function never panics.
pub fn validate_image_path(path: &Path) -> Result<(), ValidationError> {
    let image_error = |problem: ImagePathProblem| ValidationError::ImagePath {
        path: path.to_path_buf(),
        problem,
    };

    // Existence + file-type. `metadata` follows symlinks; a missing target is the
    // common "you deleted/renamed the wallpaper" case and gets its own message,
    // while any other stat failure is surfaced as unreadable.
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(image_error(ImagePathProblem::DoesNotExist));
        }
        Err(error) => return Err(image_error(ImagePathProblem::Unreadable(error))),
    };
    if !metadata.is_file() {
        // A directory (or other non-file) — checked before the extension so a
        // chosen folder reports "not a regular file" rather than a confusing
        // extension complaint.
        return Err(image_error(ImagePathProblem::NotAFile));
    }

    // Extension. Compared case-insensitively against the supported set.
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let extension_ok = extension
        .as_deref()
        .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension));
    if !extension_ok {
        return Err(image_error(ImagePathProblem::UnsupportedExtension {
            extension,
        }));
    }

    // Readability. Stat succeeding does not prove the contents are readable (e.g. a
    // file with its read bit cleared), so actually open it — the definitive test.
    if let Err(error) = File::open(path) {
        return Err(image_error(ImagePathProblem::Unreadable(error)));
    }

    Ok(())
}

/// Validates a wallpaper/lock-background **config value** as an image path (R8.3,
/// task 11.10): like [`validate_image_path`], after expanding a home-relative value
/// (`~` or `~/…`) against `home`.
///
/// The dotfiles store these paths as `~/Pictures/wallpaper/18.jpg` (analysis §7.4),
/// which names no file until the `~` is expanded — hyprpaper and hyprlock expand it
/// themselves when they read their configs, so the app must check the same file they
/// will load. Which forms count as home-relative, and why `~user/…` does not, is
/// documented in [`crate::core::home_path`]. A home-relative value with no known
/// `home` is rejected as [`ImagePathProblem::HomeUnknown`] rather than probed as a
/// relative path. Errors report the original value, so the user sees the path as
/// written in the config.
pub fn validate_image_value(value: &str, home: Option<&Path>) -> Result<(), ValidationError> {
    if is_home_relative(value) && home.is_none() {
        return Err(ValidationError::ImagePath {
            path: PathBuf::from(value),
            problem: ImagePathProblem::HomeUnknown,
        });
    }
    validate_image_path(&expand_home(value, home)).map_err(|error| match error {
        ValidationError::ImagePath { problem, .. } => ValidationError::ImagePath {
            path: PathBuf::from(value),
            problem,
        },
        other => other,
    })
}

/// Validates a desktop-wallpaper **config value** (R8.3): every place the path is
/// written must be able to hold it, and it must name a readable image
/// ([`validate_image_value`]).
///
/// The character rule is the union of the `hyprpaper.conf` and `hyprlock.conf` writers
/// (line breaks and `#`) and the live `hyprctl hyprpaper` request (`,`); see
/// [`value_writers`] for where each comes from (task 9.27). It is checked on the path
/// expanded against `home`, because that is the form the live request carries and it
/// contains every character of the `~/…` form a file may store instead — so the check
/// is the strictest of the forms, and the wallpaper model's stage-time call and the
/// Apply pipeline's step 1 (which receives the expanded path) reach the same verdict.
/// The characters are checked before the filesystem, so a refused path is reported by
/// its character even when it also names no file.
pub fn validate_wallpaper_value(value: &str, home: Option<&Path>) -> Result<(), ValidationError> {
    check_image_path_writable(SettingId::WallpaperPath, value, home)?;
    validate_image_value(value, home)
}

/// Validates a separate lock-screen image **config value** (R8.3), like
/// [`validate_wallpaper_value`] but against `hyprlock.conf`'s writer alone (line breaks
/// and `#`): a separate lock image is written nowhere else and never reaches the live
/// hyprpaper request.
pub fn validate_lock_background_value(
    value: &str,
    home: Option<&Path>,
) -> Result<(), ValidationError> {
    check_image_path_writable(SettingId::LockBackgroundPath, value, home)?;
    validate_image_value(value, home)
}

/// Runs [`value_writers::check_writable`] for an image path on its home-expanded form,
/// reporting the refusal against `value` as the user supplied it.
///
/// Checking the expanded form is over-strict in one corner: the `#` rule belongs to the
/// hyprlang files (`hyprpaper.conf`, `hyprlock.conf`), which store the `~/…` form
/// without the home prefix, so a `#` inside `$HOME` itself is refused although those
/// files would never contain it. (A `,` in `$HOME` is refused rightly: the live
/// hyprpaper request does carry the expanded path.) Accepted as the price of one check
/// that agrees with the Apply pipeline's step 1, which only sees the expanded path.
fn check_image_path_writable(
    id: SettingId,
    value: &str,
    home: Option<&Path>,
) -> Result<(), ValidationError> {
    let expanded = expand_home(value, home);
    let expanded = expanded.to_string_lossy();
    value_writers::check_writable(id, &expanded).map_err(|error| match error {
        ValidationError::Unwritable {
            problem, writers, ..
        } => ValidationError::Unwritable {
            value: value.to_string(),
            problem,
            writers,
        },
        other => other,
    })
}

/// Parses a cursor size into a pixel count, or `None` when it is not a whole number
/// greater than zero (surrounding whitespace is ignored, as the INI reader trims it).
///
/// The one definition of a usable cursor size, shared by [`validate_cursor_size`] and
/// by the Theme model, which needs the number for `hyprctl setcursor`.
pub fn parse_cursor_size(size: &str) -> Option<u32> {
    match size.trim().parse::<u32>() {
        Ok(value) if value > 0 => Some(value),
        _ => None,
    }
}

/// Validates a cursor size (R8.3, task 9.27): a whole number of pixels greater than
/// zero ([`parse_cursor_size`]).
///
/// A number this check accepts consists of digits and surrounding spaces only, so it
/// also passes the character rules of every file the size is written to
/// ([`value_writers::writers_of`]); those are checked anyway so the guarantee does not
/// rest on that observation.
pub fn validate_cursor_size(size: &str) -> Result<(), ValidationError> {
    value_writers::check_writable(SettingId::CursorSize, size)?;
    match parse_cursor_size(size) {
        Some(_) => Ok(()),
        None => Err(ValidationError::InvalidCursorSize {
            value: size.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`SettingId`] variant, so tests can exhaustively check the
    /// `kind`/`category`/`validate` mappings. A `match` in a helper below keeps
    /// this honest: adding a variant without listing it here fails to compile.
    const ALL_SETTING_IDS: &[SettingId] = &[
        SettingId::MonitorMode,
        SettingId::MonitorScale,
        SettingId::MonitorPosition,
        SettingId::LaptopDisplayEnabled,
        SettingId::PaletteColor,
        SettingId::WallpaperPath,
        SettingId::LockBackgroundPath,
        SettingId::GtkTheme,
        SettingId::IconTheme,
        SettingId::CursorTheme,
        SettingId::CursorSize,
        SettingId::KeyboardLayouts,
        SettingId::KeyboardOptions,
        SettingId::MouseSensitivity,
        SettingId::TouchpadNaturalScroll,
        SettingId::TouchpadTapToClick,
        SettingId::NotifdTimeoutLow,
        SettingId::NotifdTimeoutNormal,
        SettingId::NotifdTimeoutCritical,
        SettingId::NotifdHistoryMax,
        SettingId::NotifdCriticalPerApp,
        SettingId::NotifdHoldMin,
        SettingId::NotifdCopyCodes,
        SettingId::DimTimeout,
        SettingId::LockTimeout,
        SettingId::DpmsTimeout,
        SettingId::LockCommand,
        SettingId::LockGracePeriod,
    ];

    /// Fails to compile if [`ALL_SETTING_IDS`] omits a variant, guaranteeing the
    /// exhaustive tests below really are exhaustive as the enum grows.
    #[allow(dead_code)]
    fn assert_all_setting_ids_listed(id: SettingId) {
        match id {
            SettingId::MonitorMode
            | SettingId::MonitorScale
            | SettingId::MonitorPosition
            | SettingId::LaptopDisplayEnabled
            | SettingId::PaletteColor
            | SettingId::WallpaperPath
            | SettingId::LockBackgroundPath
            | SettingId::GtkTheme
            | SettingId::IconTheme
            | SettingId::CursorTheme
            | SettingId::CursorSize
            | SettingId::KeyboardLayouts
            | SettingId::KeyboardOptions
            | SettingId::MouseSensitivity
            | SettingId::TouchpadNaturalScroll
            | SettingId::TouchpadTapToClick
            | SettingId::NotifdTimeoutLow
            | SettingId::NotifdTimeoutNormal
            | SettingId::NotifdTimeoutCritical
            | SettingId::NotifdHistoryMax
            | SettingId::NotifdCriticalPerApp
            | SettingId::NotifdHoldMin
            | SettingId::NotifdCopyCodes
            | SettingId::DimTimeout
            | SettingId::LockTimeout
            | SettingId::DpmsTimeout
            | SettingId::LockCommand
            | SettingId::LockGracePeriod => {}
        }
    }

    /// A [`Value`] of the kind each setting expects, valid for that setting — used
    /// to prove every id has a working happy path and correct kind pairing.
    fn valid_value_for(id: SettingId) -> Value {
        match id {
            SettingId::MonitorMode => Value::Enum("2880x1800@120".to_string()),
            SettingId::MonitorScale => Value::Float(1.333_333),
            SettingId::MonitorPosition => Value::Enum("0x0".to_string()),
            SettingId::LaptopDisplayEnabled => Value::Bool(true),
            SettingId::PaletteColor => Value::String("83c092".to_string()),
            // A path validity test needs a real file, so these two are exercised
            // separately in the image-path tests; here just use a plausible-kind
            // value that the dispatch will forward to the path validator.
            SettingId::WallpaperPath | SettingId::LockBackgroundPath => {
                Value::String("/nonexistent.png".to_string())
            }
            SettingId::GtkTheme => Value::Enum("Adwaita-dark".to_string()),
            SettingId::IconTheme => Value::Enum("Papirus".to_string()),
            SettingId::CursorTheme => Value::Enum("breeze_cursors".to_string()),
            SettingId::CursorSize => Value::Enum("24".to_string()),
            SettingId::KeyboardLayouts => Value::String("us,se".to_string()),
            SettingId::KeyboardOptions => {
                Value::String("grp:win_space_toggle,caps:escape".to_string())
            }
            SettingId::MouseSensitivity => Value::Float(0.3),
            SettingId::TouchpadNaturalScroll | SettingId::TouchpadTapToClick => Value::Bool(true),
            SettingId::DimTimeout | SettingId::LockTimeout | SettingId::DpmsTimeout => {
                Value::Integer(300)
            }
            SettingId::LockCommand => Value::String("hyprlock".to_string()),
            SettingId::LockGracePeriod => Value::Integer(10),
            SettingId::NotifdTimeoutLow
            | SettingId::NotifdTimeoutNormal
            | SettingId::NotifdTimeoutCritical
            | SettingId::NotifdHoldMin => Value::Integer(5000),
            SettingId::NotifdHistoryMax => Value::Integer(100),
            SettingId::NotifdCriticalPerApp => Value::Integer(3),
            SettingId::NotifdCopyCodes => Value::Bool(true),
        }
    }

    #[test]
    fn typed_accessors_agree_with_the_value_kind() {
        // Each accessor returns Some for its own kind and None for the others, and
        // `kind()` reports the matching discriminant. This is the typed-accessor
        // contract the store relies on.
        let enum_value = Value::Enum("top".to_string());
        assert_eq!(enum_value.kind(), ValueKind::Enum);
        assert_eq!(enum_value.as_enum(), Some("top"));
        assert_eq!(enum_value.as_str(), None, "an enum is not a plain string");
        assert_eq!(enum_value.as_bool(), None);

        let bool_value = Value::Bool(true);
        assert_eq!(bool_value.kind(), ValueKind::Bool);
        assert_eq!(bool_value.as_bool(), Some(true));
        assert_eq!(bool_value.as_float(), None);

        let float_value = Value::Float(1.5);
        assert_eq!(float_value.kind(), ValueKind::Float);
        assert_eq!(float_value.as_float(), Some(1.5));
        assert_eq!(float_value.as_integer(), None);

        let integer_value = Value::Integer(300);
        assert_eq!(integer_value.kind(), ValueKind::Integer);
        assert_eq!(integer_value.as_integer(), Some(300));
        assert_eq!(integer_value.as_float(), None);

        let string_value = Value::String("hyprlock".to_string());
        assert_eq!(string_value.kind(), ValueKind::String);
        assert_eq!(string_value.as_str(), Some("hyprlock"));
        assert_eq!(string_value.as_enum(), None, "a string is not an enum");
    }

    #[test]
    fn every_setting_id_has_a_consistent_example_value() {
        // Exhaustive smoke test: for every id, the kind matches its example value
        // and that value validates. Also touches `category()` for every id so the
        // whole mapping surface is exercised.
        for &id in ALL_SETTING_IDS {
            let value = valid_value_for(id);
            assert_eq!(
                value.kind(),
                id.kind(),
                "the example value for {id:?} must match its declared kind"
            );
            let _ = id.category();

            // Wallpaper/lock validate against the filesystem; their happy path is
            // covered by the image-path tests, so skip the Apply-level validate
            // here (the example path intentionally does not exist).
            if matches!(id, SettingId::WallpaperPath | SettingId::LockBackgroundPath) {
                continue;
            }
            assert!(
                id.validate(&value).is_ok(),
                "the example value for {id:?} should validate, got {:?}",
                id.validate(&value)
            );
        }
    }

    #[test]
    fn setting_ids_map_to_expected_categories() {
        // Pins the page grouping the store's per-page dirty rollup depends on.
        assert_eq!(SettingId::MonitorMode.category(), Category::Display);
        assert_eq!(SettingId::WallpaperPath.category(), Category::Theme);
        assert_eq!(SettingId::KeyboardLayouts.category(), Category::Input);
        assert_eq!(SettingId::KeyboardOptions.category(), Category::Input);
        assert_eq!(SettingId::MouseSensitivity.category(), Category::Input);
        assert_eq!(SettingId::DimTimeout.category(), Category::PowerAndIdle);
        assert_eq!(SettingId::LockCommand.category(), Category::PowerAndIdle);
        assert_eq!(
            SettingId::NotifdTimeoutLow.category(),
            Category::Notifications
        );
        assert_eq!(
            SettingId::NotifdCopyCodes.category(),
            Category::Notifications
        );
    }

    #[test]
    fn backing_marks_only_the_laptop_toggle_runtime_only() {
        // The routing marker the store's bypass depends on (R5.2): the
        // laptop-display toggle is the sole runtime-only setting today; every other
        // setting is file-backed and staged (R5.1). Iterating the exhaustive list
        // keeps this honest as the enum grows.
        assert_eq!(
            SettingId::LaptopDisplayEnabled.backing(),
            Backing::RuntimeOnly
        );
        for &id in ALL_SETTING_IDS {
            let expected = if id == SettingId::LaptopDisplayEnabled {
                Backing::RuntimeOnly
            } else {
                Backing::FileBacked
            };
            assert_eq!(id.backing(), expected, "unexpected backing for {id:?}");
        }
    }

    #[test]
    fn validate_rejects_a_value_of_the_wrong_kind() {
        // The kind guard: staging a boolean for a text setting is a WrongKind error
        // naming both kinds, not a panic or a silent pass.
        let error = SettingId::WallpaperPath
            .validate(&Value::Bool(true))
            .expect_err("a boolean is not a valid wallpaper path");
        match error {
            ValidationError::WrongKind { expected, found } => {
                assert_eq!(expected, ValueKind::String);
                assert_eq!(found, ValueKind::Bool);
            }
            other => panic!("expected WrongKind, got {other:?}"),
        }
        // The Display message names both kinds so the UI can explain the mismatch.
        assert_eq!(
            error.to_string(),
            "expected a text value but got a boolean value"
        );
    }

    #[test]
    fn the_hex_color_validator_accepts_only_bare_hex() {
        // Valid: exactly six hex digits, either case.
        assert!(validate_hex_color("83c092").is_ok());
        assert!(validate_hex_color("ABCDEF").is_ok());

        // Invalid: wrong length, non-hex characters, a leading `#`, and empty. A
        // `#`-prefixed value is the most likely real mistake, since that is how hex
        // appears in the generated (read-only) files.
        for invalid in ["abc", "abcdef0", "gggggg", "#abcdef", ""] {
            let error =
                validate_hex_color(invalid).expect_err(&format!("`{invalid}` must be rejected"));
            assert!(
                matches!(error, ValidationError::NotBareHexColor { .. }),
                "expected NotBareHexColor for `{invalid}`, got {error:?}"
            );
        }

        // Dispatch through a SettingId reaches the same validator.
        assert!(
            SettingId::PaletteColor
                .validate(&Value::String("83c092".to_string()))
                .is_ok()
        );
        assert!(
            SettingId::PaletteColor
                .validate(&Value::String("nope".to_string()))
                .is_err()
        );
    }

    #[test]
    fn keyboard_layouts_require_at_least_one_entry() {
        // R8.3: a non-empty layout list validates, but an empty one
        // — the user removed every entry — is rejected so the framework's invalid-edit
        // path reverts the control rather than writing a bare `kb_layout=` (which
        // Hyprland silently replaces with its default, losing the config).
        assert!(validate_layout_list("us,se").is_ok());
        assert!(validate_layout_list("us").is_ok());

        // Empty, and values that trim/split to no real entry, are all rejected.
        for empty in ["", ",", " ", " , "] {
            assert!(
                matches!(
                    validate_layout_list(empty),
                    Err(ValidationError::EmptyKeyboardLayouts)
                ),
                "`{empty}` must be rejected as an empty layout list"
            );
        }

        // Dispatch through the setting reaches the same guard.
        assert!(
            SettingId::KeyboardLayouts
                .validate(&Value::String("us,se".to_string()))
                .is_ok()
        );
        let error = SettingId::KeyboardLayouts
            .validate(&Value::String(String::new()))
            .expect_err("an empty layout list must be rejected");
        assert!(matches!(error, ValidationError::EmptyKeyboardLayouts));

        // The keyboard-option list, by contrast, may legitimately be empty.
        assert!(
            SettingId::KeyboardOptions
                .validate(&Value::String(String::new()))
                .is_ok(),
            "no keyboard options set is valid"
        );
    }

    #[test]
    fn lock_command_rejects_a_newline_or_hash() {
        // R8.3 (task 6.8): the lock command is free text but is written into a hyprlang
        // value, so a `#` (hyprlang comment start) or a line break would break the config.
        // These must be rejected at stage time — the store validates on stage, so the
        // free-text Entry reverts the control on the offending keystroke rather than the
        // whole Apply aborting.
        assert!(validate_command("pidof hyprlock || hyprlock").is_ok());
        assert!(validate_command("hyprlock").is_ok());
        // Empty is allowed (an unset command); the setting is otherwise unconstrained.
        assert!(validate_command("").is_ok());

        for unsafe_value in ["hyprlock # note", "line1\nline2", "cmd\rmore"] {
            let error = validate_command(unsafe_value)
                .expect_err(&format!("`{unsafe_value}` must be rejected"));
            assert!(
                matches!(error, ValidationError::UnsafeCommand { .. }),
                "expected UnsafeCommand for `{unsafe_value}`, got {error:?}"
            );
        }

        // Dispatch through the setting reaches the same guard, so `SettingsStore::stage`
        // (which calls `validate` first) rejects it and the control reverts.
        assert!(
            SettingId::LockCommand
                .validate(&Value::String("hyprlock".to_string()))
                .is_ok()
        );
        let error = SettingId::LockCommand
            .validate(&Value::String("hyprlock # oops".to_string()))
            .expect_err("a `#` in the lock command must be rejected");
        assert!(matches!(error, ValidationError::UnsafeCommand { .. }));
    }

    #[test]
    fn the_monitor_mode_validator_accepts_valid_modes() {
        // Both WIDTHxHEIGHT[@Hz] modes and Hyprland's special mode tokens.
        for valid in [
            "2880x1800@120",
            "1920x1080@60",
            "2560x1440@59.951",
            // Bare WIDTHxHEIGHT (no @refresh) is valid: Hyprland accepts it and the
            // live monitors.conf uses it (analysis §4). `2560x1440` is the real
            // deployed eDP mode field.
            "2560x1440",
            "1920x1080",
            "preferred",
            "highres",
            "highrr",
        ] {
            assert!(
                validate_monitor_mode(valid).is_ok(),
                "`{valid}` should be a valid mode"
            );
        }
    }

    #[test]
    fn the_monitor_mode_validator_rejects_invalid_modes() {
        // Structural problems and out-of-range numbers are both rejected, each as an
        // InvalidMonitorMode carrying the offending value.
        for invalid in [
            "1920@60",        // missing xheight
            "abcxdef@60",     // non-numeric resolution
            "1920x1080@",     // empty refresh after the @
            "1920x1080@abc",  // non-numeric refresh
            "0x1080@60",      // zero width (below range)
            "1920x0",         // zero height (below range), bare form
            "99999x1080@60",  // width above the plausible range
            "1920x1080@0",    // refresh below range
            "1920x1080@2000", // refresh above range
            "1920x1080@inf",  // non-finite refresh
        ] {
            let error =
                validate_monitor_mode(invalid).expect_err(&format!("`{invalid}` must be rejected"));
            match error {
                ValidationError::InvalidMonitorMode { value, .. } => {
                    assert_eq!(value, invalid, "the error must echo the offending value");
                }
                other => panic!("expected InvalidMonitorMode for `{invalid}`, got {other:?}"),
            }
        }

        // Dispatch: MonitorMode is an Enum-kind setting forwarded to this validator.
        assert!(
            SettingId::MonitorMode
                .validate(&Value::Enum("2880x1800@120".to_string()))
                .is_ok()
        );
        assert!(
            SettingId::MonitorMode
                .validate(&Value::Enum("broken".to_string()))
                .is_err()
        );
    }

    #[test]
    fn the_position_validator_accepts_valid_positions() {
        // Every documented `auto…` token, and whole-number XxY coordinate pairs.
        for valid in [
            // Every placement token Hyprland documents; a hand-written monitors.conf
            // may carry any of them (see MONITOR_POSITION_TOKENS).
            "auto",
            "auto-right",
            "auto-left",
            "auto-up",
            "auto-down",
            "auto-center-right",
            "auto-center-left",
            "auto-center-up",
            "auto-center-down",
            // Explicit coordinates, including the negative axes a monitor placed left
            // of or above the origin needs.
            "0x0",
            "2880x0",
            "-1920x0",
            "-1920x-1080",
        ] {
            assert!(
                validate_monitor_position(valid).is_ok(),
                "`{valid}` should be a valid position"
            );
        }
    }

    #[test]
    fn monitor_position_validator_rejects_malformed_positions() {
        // R8.3: anything that is neither a known token nor an integer `XxY` pair is
        // rejected, each as an InvalidMonitorPosition echoing the offending value. The
        // comma and `#` cases are the load-bearing ones: either would break the
        // `monitor=` record's positional structure (see validate_monitor_position).
        for invalid in [
            "0,0",           // a comma would add a field and desync the record's awk parse
            "0x0 # comment", // a `#` would truncate the record as a hyprlang comment
            "auto ",         // stray whitespace is not the `auto` token
            "autoo",         // a typo, not a placement token
            "",              // empty
            "0x",            // missing Y
            "x0",            // missing X
            "1920",          // no axis separator
            "1920.5x0",      // fractional axis
            "+1920x0",       // an explicit plus sign is not accepted
        ] {
            let error = validate_monitor_position(invalid)
                .expect_err(&format!("`{invalid}` must be rejected"));
            match error {
                ValidationError::InvalidMonitorPosition { value, .. } => {
                    assert_eq!(value, invalid, "the error must echo the offending value");
                }
                other => panic!("expected InvalidMonitorPosition for `{invalid}`, got {other:?}"),
            }
        }

        // A well-formed but implausible coordinate must be reported as out of range,
        // naming the bounds — not as "not a whole number", which it plainly is. This is
        // why the axis parser is wider (i64) than the range it is checked against.
        for out_of_range in ["99999x0", "0x-99999", "99999999999x0"] {
            let error = validate_monitor_position(out_of_range)
                .expect_err(&format!("`{out_of_range}` must be rejected"));
            match error {
                ValidationError::InvalidMonitorPosition { value, detail } => {
                    assert_eq!(value, out_of_range);
                    assert!(
                        detail.contains("outside the plausible range"),
                        "`{out_of_range}` must report a range problem, got: {detail}"
                    );
                }
                other => {
                    panic!("expected InvalidMonitorPosition for `{out_of_range}`, got {other:?}")
                }
            }
        }

        // Dispatch: MonitorPosition is an Enum-kind setting forwarded to this
        // validator, so a staged position is re-checked by the Apply pipeline (R8.3).
        assert!(
            SettingId::MonitorPosition
                .validate(&Value::Enum("0x0".to_string()))
                .is_ok()
        );
        assert!(
            SettingId::MonitorPosition
                .validate(&Value::Enum("0,0".to_string()))
                .is_err()
        );
    }

    #[test]
    fn the_sensitivity_range_is_inclusive() {
        // Boundaries are inclusive; just outside on either side is rejected; NaN is
        // outside every range.
        assert!(validate_float_range(-1.0, &SENSITIVITY_RANGE).is_ok());
        assert!(validate_float_range(0.0, &SENSITIVITY_RANGE).is_ok());
        assert!(validate_float_range(1.0, &SENSITIVITY_RANGE).is_ok());
        assert!(validate_float_range(-1.1, &SENSITIVITY_RANGE).is_err());
        assert!(validate_float_range(1.1, &SENSITIVITY_RANGE).is_err());
        assert!(validate_float_range(f64::NAN, &SENSITIVITY_RANGE).is_err());

        // Dispatch through the setting, and confirm the error names the bounds.
        let error = SettingId::MouseSensitivity
            .validate(&Value::Float(2.0))
            .expect_err("2.0 is outside sensitivity range");
        match error {
            ValidationError::OutOfRange { value, min, max } => {
                assert_eq!(value, "2");
                assert_eq!(min, "-1");
                assert_eq!(max, "1");
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn the_scale_range_is_inclusive() {
        // Boundaries are inclusive; values just outside either one are rejected.
        assert!(validate_float_range(0.5, &SCALE_RANGE).is_ok());
        assert!(validate_float_range(1.333_333, &SCALE_RANGE).is_ok());
        assert!(validate_float_range(3.0, &SCALE_RANGE).is_ok());
        assert!(validate_float_range(0.4, &SCALE_RANGE).is_err());
        assert!(validate_float_range(3.1, &SCALE_RANGE).is_err());

        assert!(
            SettingId::MonitorScale
                .validate(&Value::Float(1.25))
                .is_ok()
        );
        assert!(
            SettingId::MonitorScale
                .validate(&Value::Float(0.0))
                .is_err()
        );
    }

    #[test]
    fn timeout_range_is_positive_with_a_sane_ceiling() {
        // Valid: the inclusive boundaries and a typical value.
        assert!(validate_int_range(1, &TIMEOUT_SECONDS_RANGE).is_ok());
        assert!(validate_int_range(300, &TIMEOUT_SECONDS_RANGE).is_ok());
        assert!(validate_int_range(86_400, &TIMEOUT_SECONDS_RANGE).is_ok());
        // Invalid: zero and negatives (must be positive) and just above the ceiling.
        assert!(validate_int_range(0, &TIMEOUT_SECONDS_RANGE).is_err());
        assert!(validate_int_range(-5, &TIMEOUT_SECONDS_RANGE).is_err());
        assert!(validate_int_range(86_401, &TIMEOUT_SECONDS_RANGE).is_err());

        // Every timeout setting dispatches to this validator.
        for id in [
            SettingId::DimTimeout,
            SettingId::LockTimeout,
            SettingId::DpmsTimeout,
        ] {
            assert!(id.validate(&Value::Integer(120)).is_ok());
            let error = id
                .validate(&Value::Integer(0))
                .expect_err("zero is not a positive timeout");
            assert!(matches!(error, ValidationError::OutOfRange { .. }));
        }
    }

    #[test]
    fn lock_grace_accepts_zero_up_to_ten_minutes() {
        // Unlike a timeout, zero is a real grace period (lock immediately); the ceiling
        // is the ten-minute safety bound (task 11.13, R8.3).
        for seconds in [0, 10, 600] {
            assert!(
                SettingId::LockGracePeriod
                    .validate(&Value::Integer(seconds))
                    .is_ok(),
                "{seconds} s is a valid grace period"
            );
        }
        for seconds in [-1, 601] {
            assert!(matches!(
                SettingId::LockGracePeriod.validate(&Value::Integer(seconds)),
                Err(ValidationError::OutOfRange { .. })
            ));
        }
        assert!(matches!(
            SettingId::LockGracePeriod.validate(&Value::String("10".into())),
            Err(ValidationError::WrongKind { .. })
        ));
        assert_eq!(SettingId::LockGracePeriod.kind(), ValueKind::Integer);
        assert_eq!(
            SettingId::LockGracePeriod.category(),
            Category::PowerAndIdle
        );
        assert_eq!(SettingId::LockGracePeriod.backing(), Backing::FileBacked);
    }

    #[test]
    fn image_path_validator_accepts_a_readable_image() {
        // Accept criterion: an existing, readable file with an image extension
        // validates. A case-insensitive extension (`.PNG`) is accepted too.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("wallpaper.png");
        fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("write a dummy image");
        assert!(
            validate_image_path(&path).is_ok(),
            "a readable .png must validate"
        );

        let upper = dir.path().join("cover.PNG");
        fs::write(&upper, b"\x89PNG\r\n\x1a\n").expect("write a dummy image");
        assert!(
            validate_image_path(&upper).is_ok(),
            "the extension check must be case-insensitive"
        );

        // Dispatch through the wallpaper/lock settings reaches the same validator.
        assert!(
            SettingId::WallpaperPath
                .validate(&Value::String(path.to_string_lossy().into_owned()))
                .is_ok()
        );
        assert!(
            SettingId::LockBackgroundPath
                .validate(&Value::String(path.to_string_lossy().into_owned()))
                .is_ok()
        );
    }

    #[test]
    fn the_image_path_validator_rejects_unusable_paths() {
        // Missing paths, non-files and non-images are each rejected with their reason.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");

        // Non-existent path.
        let missing = dir.path().join("gone.png");
        assert!(matches!(
            validate_image_path(&missing).expect_err("missing must be rejected"),
            ValidationError::ImagePath {
                problem: ImagePathProblem::DoesNotExist,
                ..
            }
        ));

        // A directory is not a regular file.
        assert!(matches!(
            validate_image_path(dir.path()).expect_err("a directory must be rejected"),
            ValidationError::ImagePath {
                problem: ImagePathProblem::NotAFile,
                ..
            }
        ));

        // A real file with a non-image extension.
        let text = dir.path().join("notes.txt");
        fs::write(&text, b"not an image").expect("write a text file");
        match validate_image_path(&text).expect_err("a .txt must be rejected") {
            ValidationError::ImagePath {
                problem: ImagePathProblem::UnsupportedExtension { extension },
                ..
            } => assert_eq!(extension.as_deref(), Some("txt")),
            other => panic!("expected UnsupportedExtension, got {other:?}"),
        }

        // A real file with no extension at all.
        let no_ext = dir.path().join("wallpaper");
        fs::write(&no_ext, b"data").expect("write an extensionless file");
        assert!(matches!(
            validate_image_path(&no_ext).expect_err("no extension must be rejected"),
            ValidationError::ImagePath {
                problem: ImagePathProblem::UnsupportedExtension { extension: None },
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn image_path_validator_reports_an_unreadable_image() {
        // A file that exists with a valid image extension but whose read permission
        // is cleared must be reported as Unreadable — stat succeeds, opening fails.
        // Guarded so it degrades to a no-op when run as root (where the mode is
        // ignored and the open still succeeds).
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("locked.png");
        fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("write a dummy image");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("revoke read permission");

        if File::open(&path).is_err() {
            assert!(matches!(
                validate_image_path(&path).expect_err("an unreadable image must be rejected"),
                ValidationError::ImagePath {
                    problem: ImagePathProblem::Unreadable(_),
                    ..
                }
            ));
        }

        // Restore permissions so the temp dir can be cleaned up.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    }

    #[cfg(unix)]
    #[test]
    fn image_path_validator_reports_a_dangling_symlink_as_missing() {
        // `validate_image_path` uses `fs::metadata`, which follows symlinks, so a
        // link whose target does not exist behaves like a missing path (not a
        // broken-symlink special case). Locks in that behavior.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let link = dir.path().join("wallpaper.png");
        symlink(dir.path().join("no-such-target.png"), &link).expect("create a dangling symlink");

        assert!(matches!(
            validate_image_path(&link).expect_err("a dangling symlink must be rejected"),
            ValidationError::ImagePath {
                problem: ImagePathProblem::DoesNotExist,
                ..
            }
        ));
    }

    #[test]
    fn a_tilde_image_value_is_validated_on_its_expanded_path() {
        // Task 11.10: the dotfiles store `~/Pictures/…`; validation must check the file
        // hyprpaper/hyprlock will load, i.e. the path below the home directory.
        let home = tempfile::tempdir().expect("temp dir should be creatable");
        fs::create_dir_all(home.path().join("Pictures")).expect("create Pictures");
        fs::write(home.path().join("Pictures/wall.jpg"), b"img").expect("write image");
        fs::write(home.path().join("Pictures/notes.txt"), b"hi").expect("write text");

        assert!(validate_image_value("~/Pictures/wall.jpg", Some(home.path())).is_ok());
        // An absolute value is unaffected by the home directory.
        let absolute = home.path().join("Pictures/wall.jpg");
        assert!(validate_image_value(&absolute.to_string_lossy(), Some(home.path())).is_ok());

        // A missing file and a non-image are still rejected, reported with the value as
        // written in the config.
        match validate_image_value("~/Pictures/gone.jpg", Some(home.path())) {
            Err(ValidationError::ImagePath {
                path,
                problem: ImagePathProblem::DoesNotExist,
            }) => assert_eq!(path, PathBuf::from("~/Pictures/gone.jpg")),
            other => panic!("expected DoesNotExist, got {other:?}"),
        }
        assert!(matches!(
            validate_image_value("~/Pictures/notes.txt", Some(home.path())),
            Err(ValidationError::ImagePath {
                problem: ImagePathProblem::UnsupportedExtension { .. },
                ..
            })
        ));
        // A bare `~` expands to the home directory, which is not a regular file.
        assert!(matches!(
            validate_image_value("~", Some(home.path())),
            Err(ValidationError::ImagePath {
                problem: ImagePathProblem::NotAFile,
                ..
            })
        ));
    }

    #[test]
    fn a_home_relative_image_value_without_a_home_fails_cleanly() {
        // No `$HOME`: nothing to expand against, so the value is rejected with its own
        // reason instead of being probed relative to the working directory.
        for value in ["~/Pictures/wall.jpg", "~"] {
            assert!(
                matches!(
                    validate_image_value(value, None),
                    Err(ValidationError::ImagePath {
                        problem: ImagePathProblem::HomeUnknown,
                        ..
                    })
                ),
                "{value} must be rejected without a home directory"
            );
        }
        // `~user/…` is not home-relative: it is checked literally and simply missing.
        assert!(matches!(
            validate_image_value("~nobody/wall.jpg", None),
            Err(ValidationError::ImagePath {
                problem: ImagePathProblem::DoesNotExist,
                ..
            })
        ));
    }

    #[test]
    fn theme_values_are_validated_against_their_writers() {
        // Task 9.27: the four Theme-page values reach step 1 as `SettingId`s, and each is
        // held to the union of its writers' rules (see `core::value_writers`).
        let name = |text: &str| Value::Enum(text.to_string());
        assert!(SettingId::CursorTheme.validate(&name("Nord#ic")).is_ok());
        assert!(SettingId::CursorTheme.validate(&name("Nord\"ic")).is_err());
        assert!(SettingId::IconTheme.validate(&name("Pap\"irus")).is_ok());
        assert!(SettingId::IconTheme.validate(&name("Pap\nirus")).is_err());
        assert!(SettingId::GtkTheme.validate(&name("a/b")).is_err());
        assert!(SettingId::CursorSize.validate(&name("24")).is_ok());
        assert!(matches!(
            SettingId::CursorSize.validate(&name("0")),
            Err(ValidationError::InvalidCursorSize { .. })
        ));
        // The kind guard still applies: a theme name is a drop-down choice.
        assert!(matches!(
            SettingId::GtkTheme.validate(&Value::String("Adwaita".to_string())),
            Err(ValidationError::WrongKind { .. })
        ));
    }

    #[test]
    fn an_unwritable_value_message_names_character_and_writers() {
        // R8.3 message quality: the user is told which character and which files, and a
        // control character in the quoted value is escaped so it cannot break the text.
        let error = SettingId::CursorTheme
            .validate(&Value::Enum("Nord\nic".to_string()))
            .expect_err("a line break is refused");
        assert_eq!(
            error.to_string(),
            "`Nord\\nic` cannot be used: it contains a line break (U+000A), which \
             settings.ini, uwsm/env, the hl.env lines of hyprland.lua, \
             ~/.icons/default/index.theme and the live hyprctl setcursor request cannot \
             hold — uwsm/env is read by the login shell, so the value must be one plain \
             word without spaces or shell special characters"
        );
        let error = SettingId::GtkTheme
            .validate(&Value::Enum("..".to_string()))
            .expect_err("not a directory name");
        assert_eq!(
            error.to_string(),
            "`..` cannot be used: the GTK 4 theme links in ~/.config/gtk-4.0 need a single \
             directory name (no `/`, and not `.` or `..`)"
        );
    }

    #[test]
    fn image_path_characters_are_checked_first() {
        // A refused character is reported even for a path that names no file, so the
        // user is not first told to pick an existing file and then refused for its name.
        let error =
            validate_wallpaper_value("/missing/wall#1.png", None).expect_err("a `#` is refused");
        assert!(matches!(
            error,
            ValidationError::Unwritable {
                problem: Unwritable::Character('#'),
                ..
            }
        ));
        // The check runs on the expanded path, but the message quotes the value as
        // supplied.
        let home = Path::new("/home/a,b");
        let error = validate_wallpaper_value("~/wall.png", Some(home))
            .expect_err("the expanded path holds a comma");
        assert!(
            error.to_string().starts_with("`~/wall.png` cannot be used"),
            "{error}"
        );
        // A separate lock image never reaches the hyprpaper request, so its comma passes
        // the character rule and the missing file is what is reported.
        assert!(matches!(
            validate_lock_background_value("/missing/lock,1.png", None),
            Err(ValidationError::ImagePath { .. })
        ));
    }

    #[test]
    fn validation_error_messages_are_human_readable() {
        // The UI shows these verbatim, so exercise every Display arm and confirm it
        // produces a non-empty, contextual message (also covers each error variant).
        let cases: Vec<ValidationError> = vec![
            ValidationError::WrongKind {
                expected: ValueKind::Integer,
                found: ValueKind::String,
            },
            ValidationError::NotBareHexColor {
                value: "#fff".to_string(),
            },
            ValidationError::EmptyKeyboardLayouts,
            ValidationError::UnsafeCommand {
                value: "hyprlock # x".to_string(),
            },
            ValidationError::InvalidMonitorMode {
                value: "bad".to_string(),
                detail: "nope".to_string(),
            },
            ValidationError::InvalidMonitorPosition {
                value: "1920,0".to_string(),
                detail: "nope".to_string(),
            },
            ValidationError::OutOfRange {
                value: "5".to_string(),
                min: "1".to_string(),
                max: "3".to_string(),
            },
            ValidationError::ImagePath {
                path: PathBuf::from("/x.png"),
                problem: ImagePathProblem::DoesNotExist,
            },
            ValidationError::ImagePath {
                path: PathBuf::from("/x"),
                problem: ImagePathProblem::NotAFile,
            },
            ValidationError::ImagePath {
                path: PathBuf::from("/x.txt"),
                problem: ImagePathProblem::UnsupportedExtension {
                    extension: Some("txt".to_string()),
                },
            },
            ValidationError::ImagePath {
                path: PathBuf::from("/x"),
                problem: ImagePathProblem::UnsupportedExtension { extension: None },
            },
            ValidationError::ImagePath {
                path: PathBuf::from("/x.png"),
                problem: ImagePathProblem::Unreadable(io::Error::from(
                    io::ErrorKind::PermissionDenied,
                )),
            },
            ValidationError::ImagePath {
                path: PathBuf::from("~/x.png"),
                problem: ImagePathProblem::HomeUnknown,
            },
            ValidationError::Unwritable {
                value: "a\"b".to_string(),
                problem: Unwritable::Character('"'),
                writers: vec![ValueWriter::HyprlandLuaEnv],
            },
            ValidationError::Unwritable {
                value: "a/b".to_string(),
                problem: Unwritable::NotASingleDirectoryName,
                writers: vec![ValueWriter::Gtk4ThemeLinks],
            },
            ValidationError::InvalidCursorSize {
                value: "big".to_string(),
            },
        ];
        for error in cases {
            assert!(
                !error.to_string().is_empty(),
                "every ValidationError must render a message, got empty for {error:?}"
            );
        }
    }
}
