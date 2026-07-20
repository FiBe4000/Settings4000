//! GTK-free Input-page domain logic (task 6.6; architecture §3, §6; R2.3, R4.2,
//! R4.4, R5.6, R8.3, R6.2).
//!
//! # What this module is
//!
//! The Input page edits the keyboard layout list, keyboard options, mouse
//! sensitivity, and the two touchpad toggles — all staged to
//! `config/hypr/input.conf`, the `source=`d file the `input {}` block was extracted
//! into (analysis §6.3, **not** `hyprland.conf`). Unlike the Display and Theme pages
//! these settings map cleanly onto the fixed
//! [`SettingId`](crate::core::model::SettingId) enum, so they are staged in the shared
//! [`SettingsStore`](crate::core::store) like any other file-backed setting and
//! rendered by the declarative row framework (task 5.2). This module supplies the two
//! Input-specific pieces the store cannot:
//!
//! - the **store-`SettingId` → `input.conf` write glue**: [`render_input_conf`] takes
//!   the store's dirty Input settings and applies each to the file through the surgical
//!   hyprlang writer (§3.2), producing one lossless [`FileWrite`] whose diff is limited
//!   to the touched value spans (the first store-driven page to produce a real write;
//!   task 6.7 reuses the same shape for swaync);
//! - the **keyboard-layout candidate list** sourced from the XKB registry
//!   (`/usr/share/xkb/rules/evdev.xml`), parsed by [`parse_xkb_layouts`], which the
//!   Input page's reorderable add-control offers (R2.3).
//!
//! It lives in `core/` because both pieces are pure domain logic over bytes — no GTK,
//! no process side effects — so they are unit-tested headlessly (R6.2); the layering
//! guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import here.
//!
//! # Why the whole `kb_options` list is one setting (preserve unknowns)
//!
//! [`SettingId::KeyboardOptions`](crate::core::model::SettingId::KeyboardOptions) holds
//! Hyprland's raw comma-joined `kb_options` value, not one setting per option. The
//! Input page shows a curated *switch* for each option it understands (`caps:escape`,
//! `grp:win_space_toggle`), and toggling a switch adds or removes only that token from
//! the string. Because the whole string is the stored value, any option the app has no
//! switch for is carried through an edit **verbatim** — the app never drops a
//! `kb_options` entry it does not recognise (R4.2). The token add/remove logic is the
//! pure `value_from_token_toggle` in [`crate::ui::row`], which every curated switch
//! shares.
//!
//! # Conflict safety comes from the store (R5.6)
//!
//! `input.conf` is loaded through the store at startup, which fingerprints it as the
//! freshness baseline and hands the same tracker to the Apply pipeline
//! ([`apply::run`](crate::core::apply::run)); a commit re-baselines it. So this module
//! deliberately owns **no** [`FreshnessTracker`](crate::core::freshness::FreshnessTracker):
//! [`InputModel::input_conf_write`] just reads the current bytes and renders the edit,
//! and the pipeline's step-2 conflict check aborts the apply if the file changed
//! externally — nothing here can clobber an external edit.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::core::apply::FileWrite;
use crate::core::model::{SettingId, Value};
use crate::core::reload::BackingFile;
use crate::parsers::hyprlang::{EditError, HyprlangFile, KeyPath};

/// Why [`InputModel::input_conf_write`] could not produce a write despite there being
/// dirty Input settings to apply (task 6.6 review M1).
///
/// This is distinct from "nothing was dirty" (which is a plain `Ok(None)`): when the
/// user *has* pending Input edits but the write cannot be rendered, the Apply must
/// **abort** rather than skip the write and let the store commit the staged values
/// against an unchanged file — that would desync the store from disk. Both cases are
/// near-unreachable in practice (`input.conf` is app-owned and readable), but treating
/// them as failures keeps the store and the file in agreement (R8.3).
#[derive(Debug)]
pub enum InputWriteError {
    /// `input.conf` could not be read to render the edits.
    Read(io::Error),
    /// The hyprlang writer rejected an edit — a value it cannot represent, or a missing
    /// `input {}` section to write into.
    Render(EditError),
}

impl fmt::Display for InputWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputWriteError::Read(error) => write!(f, "input.conf could not be read: {error}"),
            InputWriteError::Render(error) => {
                write!(f, "the input.conf edit could not be applied: {error}")
            }
        }
    }
}

impl std::error::Error for InputWriteError {}

/// The well-known filesystem locations of the XKB rules registry the keyboard-layout
/// candidate list is parsed from, tried in order (task 6.6 / requirements §3).
///
/// Requirements §3 names `/usr/share/xkb/rules/evdev.xml`, but `xkeyboard-config`
/// installs to the FHS-standard `/usr/share/X11/xkb/...` on virtually every distro (and
/// on the target machine), so both are checked — [`default_xkb_registry`] returns the
/// first that exists. The registry path is still passed explicitly to
/// [`InputModel::load`], so a test injects a fixture path instead of reading the system
/// file, and a machine with neither location degrades to an empty candidate list (R4.4).
pub const XKB_REGISTRY_CANDIDATES: &[&str] = &[
    "/usr/share/xkb/rules/evdev.xml",
    "/usr/share/X11/xkb/rules/evdev.xml",
];

/// The XKB rules registry path to read on the real system: the first
/// [`XKB_REGISTRY_CANDIDATES`] entry that exists, or the first entry as a fallback so a
/// "not found" log names a canonical path (task 6.6).
///
/// Existence is probed rather than assumed because the file's location varies by distro
/// (see [`XKB_REGISTRY_CANDIDATES`]); when none exists, reading the fallback simply
/// yields an empty candidate list (R4.4).
pub fn default_xkb_registry() -> PathBuf {
    XKB_REGISTRY_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(XKB_REGISTRY_CANDIDATES[0]))
}

/// One keyboard layout offered by the XKB registry.
///
/// The `code` is what Hyprland stores in `kb_layout` (e.g. `us`, `se`); the
/// `description` is the human label the registry gives it (e.g. `English (US)`). They
/// are kept separate so the UI can show a friendly label while storing the code — the
/// same split the row framework's [`DropDownOption`](crate::ui::row::DropDownOption)
/// uses, which the Input page maps these onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutOption {
    /// The XKB layout code stored in `kb_layout` (e.g. `us`).
    pub code: String,
    /// The registry's human-readable description (e.g. `English (US)`).
    pub description: String,
}

/// The complete new `input.conf` bytes plus the changed-key labels, produced by
/// applying the store's dirty Input edits to the current file (task 6.6).
///
/// Returned by [`render_input_conf`] and wrapped in a [`FileWrite`] by
/// [`InputModel::input_conf_write`]. The `changed_keys` are the rendered section paths
/// (e.g. `input.kb_layout`) used only for the apply-level log line (R7.3), never the
/// file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputConfEdit {
    /// The complete new file contents (surgical, span-preserving — §3).
    pub contents: Vec<u8>,
    /// The section paths this edit changed, for logging.
    pub changed_keys: Vec<String>,
}

/// The `input.conf` section path each Input [`SettingId`] edits, or `None` for a
/// setting the Input page does not back (a guard for a caller that passes a foreign
/// id).
///
/// This is the store-`SettingId` → hyprlang address map: keyboard layout/options live
/// directly under `input {}`, sensitivity likewise, and the two touchpad toggles one
/// section deeper under `input.touchpad`. The paths match the read side in
/// [`crate::ui::startup`] so a value round-trips through the same address it was parsed
/// from.
pub fn input_section_path(id: SettingId) -> Option<KeyPath> {
    match id {
        SettingId::KeyboardLayouts => Some(KeyPath::at(&["input"], "kb_layout")),
        SettingId::KeyboardOptions => Some(KeyPath::at(&["input"], "kb_options")),
        SettingId::MouseSensitivity => Some(KeyPath::at(&["input"], "sensitivity")),
        SettingId::TouchpadNaturalScroll => {
            Some(KeyPath::at(&["input", "touchpad"], "natural_scroll"))
        }
        SettingId::TouchpadTapToClick => Some(KeyPath::at(&["input", "touchpad"], "tap-to-click")),
        _ => None,
    }
}

/// The on-disk `input.conf` string form of an Input setting's [`Value`], or `None`
/// when the value's kind does not match the setting (a guard, not an expected case —
/// the store validates kinds on stage).
///
/// A [`Value::Float`] (sensitivity) is rendered with the shortest round-tripping
/// decimal Rust produces (`0.3`, `1`, `-1`), which Hyprland accepts; a [`Value::Bool`]
/// (touchpad toggle) becomes the literal `true`/`false`; a [`Value::String`] (the
/// comma-joined layout or option list) is written verbatim.
fn value_to_hypr_string(id: SettingId, value: &Value) -> Option<String> {
    match (id, value) {
        (SettingId::KeyboardLayouts | SettingId::KeyboardOptions, Value::String(text)) => {
            Some(text.clone())
        }
        (SettingId::MouseSensitivity, Value::Float(number)) => Some(number.to_string()),
        (SettingId::TouchpadNaturalScroll | SettingId::TouchpadTapToClick, Value::Bool(flag)) => {
            Some(if *flag { "true" } else { "false" }.to_string())
        }
        _ => None,
    }
}

/// Applies the store's dirty Input `edits` to the current `input.conf` `bytes`,
/// returning the complete new bytes (task 6.6; R5.3 item 1).
///
/// This is the pure store-`SettingId` → `input.conf` glue. It parses the current file
/// losslessly, then for each `(id, value)` rewrites **only** that setting's value span
/// through the surgical hyprlang writer ([`HyprlangFile::set_value`]) — so comments,
/// ordering, unrelated keys, and every untouched byte stay identical and the emitted
/// diff is limited to the changed lines. A key missing on disk (e.g. `kb_options` in a
/// config that never set it) is appended at the `input {}` section's end by the writer.
///
/// Returns an [`EditError`] if the writer rejects an edit — a value containing a
/// newline/`#` (R8.3), or an `input {}` section that does not exist — leaving the caller
/// to skip the write rather than emit a partial file. A setting with no Input section
/// path is ignored (it does not belong to this file).
pub fn render_input_conf(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<InputConfEdit, EditError> {
    let text = String::from_utf8_lossy(bytes);
    let (mut file, _warnings) = HyprlangFile::parse(&text);

    let mut changed_keys = Vec::new();
    for (id, value) in edits {
        let (Some(path), Some(rendered)) =
            (input_section_path(*id), value_to_hypr_string(*id, value))
        else {
            // Not an Input-backed setting, or a kind mismatch the store would have
            // rejected on stage: skip rather than write a bad value.
            continue;
        };
        file.set_value(&path, &rendered)?;
        changed_keys.push(path.to_string());
    }

    Ok(InputConfEdit {
        contents: file.emit().into_bytes(),
        changed_keys,
    })
}

/// Parses the XKB rules registry XML into the keyboard-layout candidate list (task
/// 6.6; requirements §3).
///
/// The registry (`evdev.xml`) lists layouts as
/// `<layoutList><layout><configItem><name>us</name><description>English (US)</description>…`.
/// This extracts each layout's **base** name and description — the `<configItem>` before
/// its `<variantList>`, so a layout *variant* (e.g. `dvorak`) is never mistaken for a
/// base layout — de-duplicates by code, and sorts by description for a stable,
/// browsable add-list. It never fails: a registry whose structure it does not recognise
/// (or an empty one) yields an empty list, which the Input page shows as an empty
/// add-control (R4.4). A small hand-rolled scan is used rather than adding an XML
/// dependency, matching the project's dependency-light parsers (§3); it targets exactly
/// this file's regular shape.
pub fn parse_xkb_layouts(xml: &str) -> Vec<LayoutOption> {
    let Some(list) = slice_between(xml, "<layoutList>", "</layoutList>") else {
        return Vec::new();
    };

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut layouts = Vec::new();
    // Each `<layout>…</layout>` block holds one base layout. Splitting on the opening
    // tag and taking everything up to the matching close isolates each block; layouts
    // do not nest, so this is unambiguous.
    for chunk in list.split("<layout>").skip(1) {
        let block = chunk.split("</layout>").next().unwrap_or(chunk);
        // The layout's own configItem precedes its `<variantList>`; restricting to the
        // text before the variant list keeps variant names/descriptions out.
        let base = block.split("<variantList").next().unwrap_or(block);
        let (Some(name), Some(description)) = (
            slice_between(base, "<name>", "</name>"),
            slice_between(base, "<description>", "</description>"),
        ) else {
            continue;
        };
        let code = unescape_xml(name.trim());
        if code.is_empty() {
            continue;
        }
        if seen.insert(code.clone()) {
            layouts.push(LayoutOption {
                code,
                description: unescape_xml(description.trim()),
            });
        }
    }

    layouts.sort_by(|a, b| a.description.cmp(&b.description));
    layouts
}

/// Reads and parses the XKB registry at `registry`, degrading to an empty list when it
/// is missing or unreadable (R4.4).
///
/// A missing registry, or one that parses to no layouts, is logged at `info` (the
/// hidden/absent-feature signal, R4.2) — never as an error, since a machine without the
/// XKB rules file simply offers no layouts to add, which the Input page handles.
fn read_xkb_layouts(registry: &Path) -> Vec<LayoutOption> {
    match std::fs::read_to_string(registry) {
        Ok(xml) => {
            let layouts = parse_xkb_layouts(&xml);
            if layouts.is_empty() {
                tracing::info!(
                    path = %registry.display(),
                    "XKB registry parsed no layouts; the keyboard-layout add-list is empty (R4.4)"
                );
            }
            layouts
        }
        Err(error) => {
            tracing::info!(
                path = %registry.display(),
                %error,
                "XKB registry unreadable; the keyboard-layout add-list is empty (R4.4)"
            );
            Vec::new()
        }
    }
}

/// Returns the slice of `haystack` between the first `open` and the following `close`,
/// or `None` if either is missing. A tiny helper for the targeted XKB scan.
fn slice_between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let rest = &haystack[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

/// Decodes the five predefined XML entities that appear in registry descriptions.
///
/// `&amp;` is decoded last so a decoded `&` is never re-interpreted as the start of
/// another entity. This is sufficient for `evdev.xml`, which uses only these entities.
fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The Input page's runtime-loaded helpers: where `input.conf` lives and the XKB
/// layout candidates (task 6.6).
///
/// Built once on the startup worker (architecture §8) when Hyprland is present, and
/// held by the window. It is intentionally **not** a staging model — every Input
/// setting is staged in the shared store, so dirty tracking, validation, reset, and
/// commit all flow through the store. This just supplies the layout add-list and turns
/// the store's dirty Input settings into the `input.conf` [`FileWrite`] on Apply.
pub struct InputModel {
    /// The live XDG path of `input.conf` (R8.5), read fresh when rendering a write.
    input_conf: PathBuf,
    /// The XKB layout candidates the reorderable add-control offers (R2.3).
    layouts: Vec<LayoutOption>,
}

impl InputModel {
    /// Builds the model: records the `input.conf` path and reads the XKB layout
    /// candidates from `xkb_registry` (the production entry point, called from the
    /// startup worker — architecture §8).
    ///
    /// Reading the (large) registry here, off the main thread, keeps it inside the R8.1
    /// cold-start budget; an unreadable registry degrades to no candidates (R4.4).
    pub fn load(input_conf: PathBuf, xkb_registry: &Path) -> InputModel {
        InputModel {
            input_conf,
            layouts: read_xkb_layouts(xkb_registry),
        }
    }

    /// The XKB layout candidates for the reorderable add-control (R2.3).
    pub fn layout_options(&self) -> &[LayoutOption] {
        &self.layouts
    }

    /// Renders the store's dirty Input settings into an `input.conf` [`FileWrite`]
    /// (task 6.6).
    ///
    /// `dirty` is the store's dirty Input settings (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category)).
    /// It reads the current file bytes and applies the edits through [`render_input_conf`],
    /// returning a single surgical [`FileWrite`] for the shared Apply pipeline. Reading
    /// fresh each time — rather than caching a parsed copy — is what keeps it correct
    /// across repeated applies and external edits without a bespoke freshness tracker:
    /// the pipeline's conflict check (against the store's baseline) aborts the apply if
    /// the file changed since load, so a fresh read can never clobber an external edit.
    ///
    /// Returns:
    /// - `Ok(None)` when there is nothing to write — `dirty` is empty (the common clean
    ///   case);
    /// - `Ok(Some(write))` with the rendered write;
    /// - `Err(InputWriteError)` when there *are* dirty Input settings but the write
    ///   cannot be produced (the file is unreadable, or the writer rejects an edit). The
    ///   caller must **abort the Apply** in this case rather than skip the write, since
    ///   the store would otherwise commit the staged values against an unchanged file and
    ///   desync (task 6.6 review M1). Both failure modes are near-unreachable in practice.
    pub fn input_conf_write(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, InputWriteError> {
        if dirty.is_empty() {
            return Ok(None);
        }
        let bytes = std::fs::read(&self.input_conf).map_err(|error| {
            tracing::error!(
                path = %self.input_conf.display(),
                %error,
                "could not read input.conf to render the Input edits; aborting the apply (R8.3)"
            );
            InputWriteError::Read(error)
        })?;
        let edit = render_input_conf(&bytes, dirty).map_err(|error| {
            tracing::error!(
                path = %self.input_conf.display(),
                %error,
                "failed to render an input.conf edit; aborting the apply (R8.3)"
            );
            InputWriteError::Render(error)
        })?;
        // Defensive: every dirty Input setting maps to a section path and a rendered
        // value, so `changed_keys` is non-empty here — this branch is effectively dead,
        // but is guarded rather than emitting a byte-identical no-op write.
        if edit.changed_keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileWrite {
            path: self.input_conf.clone(),
            contents: edit.contents,
            changed_keys: edit.changed_keys,
            backing: BackingFile::InputConf,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::model::Category;
    use crate::core::reload::ReloadParams;
    use crate::core::store::{FileReader, FileValues, SettingsStore};
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;

    /// The app-owned `input.conf` shape (analysis §6.3): the `input {}` block with a
    /// layout list, an option list carrying an entry the app has no switch for, flat
    /// keys with inline formatting, and a nested `touchpad {}`.
    const INPUT_CONF: &str = "\
# Input configuration. App-owned.
input {
    kb_layout=us,se
    kb_options=grp:win_space_toggle,caps:escape,compose:ralt
    sensitivity=0.3
    follow_mouse=1

    touchpad {
        natural_scroll=true
        tap-to-click=true
    }
}
";

    /// The indices at which two texts' lines differ — to assert an edit touched exactly
    /// the expected lines (mirrors the hyprlang parser's own edit tests).
    fn differing_lines(before: &str, after: &str) -> Vec<usize> {
        let before: Vec<&str> = before.lines().collect();
        let after: Vec<&str> = after.lines().collect();
        assert_eq!(
            before.len(),
            after.len(),
            "a surgical edit must not add or remove lines"
        );
        before
            .iter()
            .zip(&after)
            .enumerate()
            .filter_map(|(i, (b, a))| (b != a).then_some(i))
            .collect()
    }

    /// Renders `edits` into `INPUT_CONF` and returns the emitted text.
    fn render(edits: &[(SettingId, Value)]) -> String {
        let edit = render_input_conf(INPUT_CONF.as_bytes(), edits).expect("edits render");
        String::from_utf8(edit.contents).expect("valid utf-8")
    }

    #[test]
    fn each_control_maps_to_the_right_section_path() {
        // The store-SettingId -> input.conf address map: the five Input settings resolve
        // to their `input.*` / `input.touchpad.*` paths, and a foreign setting does not.
        assert_eq!(
            input_section_path(SettingId::KeyboardLayouts).map(|p| p.to_string()),
            Some("input.kb_layout".to_string())
        );
        assert_eq!(
            input_section_path(SettingId::KeyboardOptions).map(|p| p.to_string()),
            Some("input.kb_options".to_string())
        );
        assert_eq!(
            input_section_path(SettingId::MouseSensitivity).map(|p| p.to_string()),
            Some("input.sensitivity".to_string())
        );
        assert_eq!(
            input_section_path(SettingId::TouchpadNaturalScroll).map(|p| p.to_string()),
            Some("input.touchpad.natural_scroll".to_string())
        );
        assert_eq!(
            input_section_path(SettingId::TouchpadTapToClick).map(|p| p.to_string()),
            Some("input.touchpad.tap-to-click".to_string())
        );
        // A setting that does not live in input.conf has no path here.
        assert_eq!(input_section_path(SettingId::NotificationTimeout), None);
    }

    #[test]
    fn a_layout_reorder_round_trips_to_kb_layout() {
        // Accept criterion: the ordered layout list serialises to `kb_layout=us,se`, and
        // reordering it edits only that one line (R2.3).
        let edited = render(&[(
            SettingId::KeyboardLayouts,
            Value::String("se,us".to_string()),
        )]);
        let changed = differing_lines(INPUT_CONF, &edited);
        let target = INPUT_CONF
            .lines()
            .position(|l| l == "    kb_layout=us,se")
            .expect("fixture has kb_layout");
        assert_eq!(changed, vec![target], "only the kb_layout line changes");
        assert_eq!(edited.lines().nth(target), Some("    kb_layout=se,us"));
    }

    #[test]
    fn an_unknown_kb_option_survives_an_edit_verbatim() {
        // Accept criterion: toggling a curated option off (here removing `caps:escape`,
        // as the UI's token toggle would) leaves the option the app has no switch for
        // (`compose:ralt`) untouched — the app never drops an unrecognised option.
        let edited = render(&[(
            SettingId::KeyboardOptions,
            // The value the curated-switch toggle produces when `caps:escape` is
            // switched off: the other tokens, in order, unknown one included.
            Value::String("grp:win_space_toggle,compose:ralt".to_string()),
        )]);
        let target = INPUT_CONF
            .lines()
            .position(|l| l == "    kb_options=grp:win_space_toggle,caps:escape,compose:ralt")
            .expect("fixture has kb_options");
        assert_eq!(
            differing_lines(INPUT_CONF, &edited),
            vec![target],
            "only the kb_options line changes"
        );
        assert_eq!(
            edited.lines().nth(target),
            Some("    kb_options=grp:win_space_toggle,compose:ralt"),
            "the unknown `compose:ralt` option is preserved verbatim"
        );
    }

    #[test]
    fn sensitivity_and_touchpad_edits_target_their_own_lines() {
        // The float sensitivity and the nested touchpad booleans each rewrite only their
        // own value span, at the right section depth.
        let sensitivity = render(&[(SettingId::MouseSensitivity, Value::Float(0.5))]);
        let s_line = INPUT_CONF
            .lines()
            .position(|l| l == "    sensitivity=0.3")
            .unwrap();
        assert_eq!(differing_lines(INPUT_CONF, &sensitivity), vec![s_line]);
        assert_eq!(sensitivity.lines().nth(s_line), Some("    sensitivity=0.5"));

        let natural = render(&[(SettingId::TouchpadNaturalScroll, Value::Bool(false))]);
        let n_line = INPUT_CONF
            .lines()
            .position(|l| l == "        natural_scroll=true")
            .unwrap();
        assert_eq!(differing_lines(INPUT_CONF, &natural), vec![n_line]);
        assert_eq!(
            natural.lines().nth(n_line),
            Some("        natural_scroll=false")
        );

        // The second nested touchpad boolean, so both `input.touchpad.*` paths are
        // exercised (not just natural_scroll).
        let tap = render(&[(SettingId::TouchpadTapToClick, Value::Bool(false))]);
        let t_line = INPUT_CONF
            .lines()
            .position(|l| l == "        tap-to-click=true")
            .unwrap();
        assert_eq!(differing_lines(INPUT_CONF, &tap), vec![t_line]);
        assert_eq!(tap.lines().nth(t_line), Some("        tap-to-click=false"));
    }

    #[test]
    fn kb_options_cleared_to_empty_writes_a_bare_key() {
        // Removing the last remaining option (KeyboardOptions -> "") rewrites the value
        // span to nothing, emitting a bare `kb_options=` on the same line — only that
        // line changes.
        let edited = render(&[(SettingId::KeyboardOptions, Value::String(String::new()))]);
        let target = INPUT_CONF
            .lines()
            .position(|l| l == "    kb_options=grp:win_space_toggle,caps:escape,compose:ralt")
            .expect("fixture has kb_options");
        assert_eq!(differing_lines(INPUT_CONF, &edited), vec![target]);
        assert_eq!(edited.lines().nth(target), Some("    kb_options="));
    }

    #[test]
    fn kb_options_is_appended_when_absent_leaving_other_lines_intact() {
        // When `input.conf` has an `input {}` section but no `kb_options` key (the
        // seeded-empty case — task 6.6), toggling the first curated option on appends a
        // new `kb_options` line inside the section; every original line is preserved.
        const NO_OPTIONS: &str = "\
input {
    kb_layout=us,se
    sensitivity=0.3
}
";
        let edit = render_input_conf(
            NO_OPTIONS.as_bytes(),
            &[(
                SettingId::KeyboardOptions,
                Value::String("caps:escape".to_string()),
            )],
        )
        .expect("append renders");
        let emitted = String::from_utf8(edit.contents).expect("valid utf-8");

        // The new key was appended inside the `input {}` block, before its closing brace.
        assert!(
            emitted.contains("kb_options") && emitted.contains("caps:escape"),
            "the appended kb_options line is present: {emitted:?}"
        );
        // Every original line survives (the append only adds a line, changes none).
        for line in NO_OPTIONS.lines() {
            assert!(
                emitted.lines().any(|l| l == line),
                "original line `{line}` must be preserved"
            );
        }
        assert_eq!(
            emitted.lines().count(),
            NO_OPTIONS.lines().count() + 1,
            "exactly one line (the kb_options assignment) is added"
        );
        assert_eq!(edit.changed_keys, vec!["input.kb_options".to_string()]);
    }

    #[test]
    fn no_edits_round_trips_the_file_byte_for_byte() {
        // With no dirty settings the render is the identity (nothing touched).
        let edit = render_input_conf(INPUT_CONF.as_bytes(), &[]).expect("renders");
        assert_eq!(edit.contents, INPUT_CONF.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    /// A trimmed but structurally faithful `evdev.xml`: a `<layoutList>` with three base
    /// layouts (one carrying a variant that must be ignored, one using an XML entity)
    /// plus the surrounding `<modelList>`/`<optionList>` the scan must not confuse for
    /// layouts.
    const EVDEV_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xkbConfigRegistry version="1.1">
  <modelList>
    <model><configItem><name>pc105</name><description>Generic 105-key</description></configItem></model>
  </modelList>
  <layoutList>
    <layout>
      <configItem>
        <name>us</name>
        <shortDescription>en</shortDescription>
        <description>English (US)</description>
      </configItem>
      <variantList>
        <variant><configItem><name>dvorak</name><description>English (Dvorak)</description></configItem></variant>
      </variantList>
    </layout>
    <layout>
      <configItem>
        <name>se</name>
        <description>Swedish</description>
      </configItem>
    </layout>
    <layout>
      <configItem>
        <name>epo</name>
        <description>Esperanto (H &amp; X)</description>
      </configItem>
    </layout>
  </layoutList>
  <optionList>
    <option><configItem><name>caps:escape</name><description>Caps as Escape</description></configItem></option>
  </optionList>
</xkbConfigRegistry>
"#;

    #[test]
    fn parses_base_layouts_from_the_xkb_registry() {
        // The candidate list is the base layouts only — a variant (`dvorak`), the model,
        // and the option are excluded — with XML entities decoded and the list sorted by
        // description.
        let layouts = parse_xkb_layouts(EVDEV_XML);
        assert_eq!(
            layouts,
            vec![
                LayoutOption {
                    code: "us".to_string(),
                    description: "English (US)".to_string(),
                },
                LayoutOption {
                    code: "epo".to_string(),
                    description: "Esperanto (H & X)".to_string(),
                },
                LayoutOption {
                    code: "se".to_string(),
                    description: "Swedish".to_string(),
                },
            ],
            "base layouts only, entity-decoded, sorted by description"
        );
    }

    #[test]
    fn xkb_parse_degrades_on_absent_or_garbled_input() {
        // No registry structure -> no candidates, never a panic (R4.4).
        assert!(parse_xkb_layouts("").is_empty());
        assert!(parse_xkb_layouts("<html><body>not xkb</body></html>").is_empty());
        assert!(
            parse_xkb_layouts("<layoutList></layoutList>").is_empty(),
            "an empty layout list yields no candidates"
        );
    }

    #[test]
    fn read_xkb_layouts_degrades_when_the_registry_is_missing() {
        // A missing registry file degrades to an empty candidate list rather than an
        // error (R4.4).
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("no-such-evdev.xml");
        assert!(read_xkb_layouts(&missing).is_empty());
    }

    #[test]
    fn input_model_layout_options_come_from_the_registry() {
        // The model exposes the parsed candidates for the add-control (R2.3).
        let dir = tempfile::tempdir().expect("temp dir");
        let registry = dir.path().join("evdev.xml");
        fs::write(&registry, EVDEV_XML).expect("write registry fixture");
        let model = InputModel::load(dir.path().join("input.conf"), &registry);
        let codes: Vec<&str> = model
            .layout_options()
            .iter()
            .map(|l| l.code.as_str())
            .collect();
        assert_eq!(codes, vec!["us", "epo", "se"]);
    }

    #[test]
    fn input_conf_write_is_none_when_clean() {
        // No dirty Input settings -> Ok(None), no write (the common clean case).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        fs::write(&path, INPUT_CONF).expect("write input.conf");
        let model = InputModel::load(path, Path::new("/nonexistent-xkb"));
        assert!(matches!(model.input_conf_write(&[]), Ok(None)));
    }

    #[test]
    fn input_conf_write_errors_when_dirty_but_the_section_is_missing() {
        // M1: dirty Input settings but the writer cannot apply them (no `input {}`
        // section) must surface as an error, so the Apply aborts rather than skipping the
        // write and letting the store commit an unwritten value against an unchanged file.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        fs::write(&path, "# no input block here\n").expect("write input.conf");
        let model = InputModel::load(path, Path::new("/nonexistent-xkb"));
        let dirty = vec![(
            SettingId::KeyboardLayouts,
            Value::String("us,se".to_string()),
        )];
        assert!(matches!(
            model.input_conf_write(&dirty),
            Err(InputWriteError::Render(_))
        ));
    }

    #[test]
    fn input_conf_write_errors_when_dirty_but_the_file_is_unreadable() {
        // M1: a dirty Input edit against a missing file is an error (the Apply aborts),
        // never a silent skip that would let commit_apply promote an unwritten value.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = InputModel::load(dir.path().join("gone.conf"), Path::new("/nonexistent-xkb"));
        let dirty = vec![(SettingId::MouseSensitivity, Value::Float(0.5))];
        assert!(matches!(
            model.input_conf_write(&dirty),
            Err(InputWriteError::Read(_))
        ));
    }

    #[test]
    fn a_dirty_input_edit_applies_through_the_pipeline_with_only_hyprctl_reload() {
        // The end-to-end store-SettingId -> FileWrite glue (task 6.6): a dirty Input
        // setting renders a surgical input.conf FileWrite whose diff is limited to the
        // touched line, and applying it through the shared pipeline writes that file and
        // triggers exactly `hyprctl reload` — nothing else (R5.3).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        fs::write(&path, INPUT_CONF).expect("write input.conf");
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb"));

        let dirty = vec![(SettingId::MouseSensitivity, Value::Float(0.5))];
        let write = model
            .input_conf_write(&dirty)
            .expect("no error rendering the write")
            .expect("a dirty Input setting produces a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::InputConf);
        assert_eq!(write.changed_keys, vec!["input.sensitivity".to_string()]);

        // The freshness baseline matches the on-disk bytes, so no conflict.
        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline input.conf");

        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(written, vec![path.clone()]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // Only the sensitivity line changed on disk.
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(
            differing_lines(INPUT_CONF, &on_disk),
            vec![
                INPUT_CONF
                    .lines()
                    .position(|l| l == "    sensitivity=0.3")
                    .unwrap()
            ]
        );
        // The Input change reloads only via `hyprctl reload`.
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("reload")]
        );
        assert!(signaller.calls().is_empty());
    }

    #[test]
    fn a_second_apply_after_commit_is_not_a_self_conflict_through_the_real_glue() {
        // S2: proves the window's fold-before-`store_writes`-capture plus
        // `commit_apply`'s re-baseline of input.conf work through the REAL renderer (not
        // apply.rs's hand-built write). Load input.conf into a real store, stage an Input
        // edit, build the write via `input_conf_write`, run `apply::run` over the store's
        // freshness, commit, then stage a SECOND edit and re-apply — the outcome must be
        // `Applied`, not `Conflicted` (the app's own first write must not read as an
        // external change on the next apply, R5.6).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        fs::write(&path, INPUT_CONF).expect("write input.conf");
        let model = InputModel::load(path.clone(), Path::new("/nonexistent-xkb"));

        // Load the real originals + freshness baseline, as the startup load does. The
        // conflict-reload path is not exercised here, so a trivial re-reader suffices.
        let reader: FileReader = Box::new(|p: &Path| {
            Ok(FileValues {
                bytes: fs::read(p)?,
                values: Vec::new(),
            })
        });
        let mut store = SettingsStore::new();
        let bytes = fs::read(&path).expect("read input.conf");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![
                    (SettingId::MouseSensitivity, Value::Float(0.3)),
                    (
                        SettingId::KeyboardLayouts,
                        Value::String("us,se".to_string()),
                    ),
                ],
            },
            reader,
        );

        // No hyprctl, so the InputConf reload is gated out — this test is about the
        // conflict check and re-baseline, not the reload set.
        let caps = Capabilities::for_tests(&[], &[], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        // First apply: change the sensitivity through the real glue.
        store
            .stage(SettingId::MouseSensitivity, Value::Float(0.5))
            .expect("stage the first edit");
        let dirty = store.dirty_in_category(Category::Input);
        let write = model
            .input_conf_write(&dirty)
            .expect("no error")
            .expect("a write");
        let written_bytes = write.contents.clone();
        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let written = match apply::run(&plan, store.freshness(), &caps, &runner, &signaller) {
            ApplyOutcome::Applied { written, .. } => written,
            other => panic!("expected Applied on the first apply, got {other:?}"),
        };
        assert_eq!(written, vec![path.clone()]);
        // Commit as the window does: re-baseline input.conf from the exact bytes written.
        store.commit_apply(&[(path.clone(), written_bytes)]);

        // Second apply: reorder the layouts. The on-disk file is now the app's own first
        // write; the commit re-baselined it, so this must NOT self-conflict.
        store
            .stage(
                SettingId::KeyboardLayouts,
                Value::String("se,us".to_string()),
            )
            .expect("stage the second edit");
        let dirty2 = store.dirty_in_category(Category::Input);
        let write2 = model
            .input_conf_write(&dirty2)
            .expect("no error")
            .expect("a write");
        let plan2 = ApplyPlan {
            validations: dirty2,
            writes: vec![write2],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        assert!(
            matches!(
                apply::run(&plan2, store.freshness(), &caps, &runner, &signaller),
                ApplyOutcome::Applied { .. }
            ),
            "the second apply must not self-conflict after the first commit re-baselined \
             input.conf through the real renderer"
        );
        // The second edit reached disk on top of the first (both values present).
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert!(
            on_disk.contains("kb_layout=se,us") && on_disk.contains("sensitivity=0.5"),
            "both applied edits are on disk: {on_disk:?}"
        );
    }
}
