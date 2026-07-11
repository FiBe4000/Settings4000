//! The Theme page's palette-scheme domain model (task 6.3; architecture §6, §7;
//! R3.2, R4.2, R4.4, R8.5, R6.2).
//!
//! # What this module is
//!
//! The Theme page's first section lets the user switch the central color palette.
//! The dotfiles keep one file per scheme under the repo's `colors/` directory
//! (`colors/nord`, `colors/everforest`, …), and switching is not a file edit but a
//! *regeneration*: the app runs `scripts/generate-colors <scheme>`, which rewrites
//! the read-only generated color partials from the chosen source (the palette
//! gotcha — the app never edits `colors/<scheme>` in v1, requirements §9). This
//! module is the GTK-free staging model behind that control:
//!
//! - it **enumerates** the switchable schemes from the discovered `colors/`
//!   directory (skipping dotfiles, subdirectories, and non-palette files so a
//!   `state/active-scheme`-style marker or a `README.md` never appears as a scheme);
//! - it **detects the active scheme** from the deployed generated `colors.conf`
//!   header (task 3.7, R3.2) and preselects it;
//! - it **stages** a pending switch and reports it as an
//!   [`apply::PaletteSwitch`](crate::core::apply::PaletteSwitch) contribution the
//!   Apply pipeline runs last (so `generate-colors` runs after every file write and
//!   a rollback never strands the generated files on the new scheme).
//!
//! # Why a bespoke model, not a `SettingId` in the store
//!
//! Every file-backed setting flows through [`SettingsStore`](crate::core::store) as
//! an `original`/`staged` [`Value`](crate::core::model::Value) keyed by a
//! [`SettingId`](crate::core::model::SettingId), and the store's Apply produces a
//! [`FileWrite`](crate::core::apply::FileWrite). The palette scheme fits neither
//! end of that: its "original" is read from a *generated* file's header (which the
//! app never writes), and its Apply produces **no** file write at all — it runs the
//! generator. Forcing it through the store would mean tracking the generated
//! `colors.conf` for freshness/conflict and then re-baselining a file the app did
//! not write, a poor fit. So — exactly like the Display page's per-monitor model
//! ([`crate::core::display`]) — the palette scheme is a small self-contained staging
//! model that the window folds into the same Apply/Reset chrome and the same
//! [`apply::run`](crate::core::apply::run) pipeline as a second staging source. Its
//! Apply contribution populates [`ApplyPlan::palette`](crate::core::apply::ApplyPlan),
//! not `writes`.
//!
//! # Read-only degrade (R3.2)
//!
//! With fewer than two schemes there is nothing to switch *to*, so the model reports
//! [`is_switchable`](PaletteModel::is_switchable) as `false` and the page shows the
//! active scheme read-only rather than a functional drop-down.
//!
//! It lives in `core/` so the enumeration, preselect, and staging logic are
//! headlessly testable (R6.2) — the layering guard in `tests/module_boundaries.rs`
//! forbids any `gtk`/`relm4` import here. Every path is injected, so tests drive it
//! against a temporary `colors/` directory with no live dotfiles deployment.

use std::path::{Path, PathBuf};

use crate::core::apply::PaletteSwitch;
use crate::parsers::generated;

/// One discovered, switchable palette scheme.
///
/// A scheme is one schema-valid palette file in the `colors/` directory. Its
/// [`preview`](Self::preview) colors are parsed once at load from the file's swatch
/// (task 3.7) so the UI can draw a small preview strip without re-reading the file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scheme {
    /// The scheme's name — its file name under `colors/` (e.g. `nord`), which is also
    /// the argument passed to `generate-colors`.
    name: String,
    /// The scheme's palette colors as RGB components in `0.0..=1.0`, in the palette's
    /// canonical key order, for a preview strip. Empty when no color could be parsed;
    /// a value that is not bare hex is skipped rather than failing the scheme.
    preview: Vec<(f64, f64, f64)>,
}

impl Scheme {
    /// The scheme's name (its `colors/` file name).
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The scheme's preview colors as RGB components in `0.0..=1.0`.
    pub(crate) fn preview(&self) -> &[(f64, f64, f64)] {
        &self.preview
    }
}

/// The palette-scheme staging model for the Theme page (task 6.3).
///
/// Built by [`PaletteModel::load`] from the discovered palette source and folded into
/// the window's Apply/Reset chrome as a second staging source (see the module docs).
#[derive(Clone, Debug)]
pub(crate) struct PaletteModel {
    /// The switchable schemes discovered in `colors/`, sorted by name for a stable
    /// drop-down order.
    schemes: Vec<Scheme>,
    /// The active scheme detected from the deployed generated `colors.conf` header
    /// (R3.2), or `None` when it could not be determined.
    active: Option<String>,
    /// The pending scheme switch, or `None` when nothing is staged. Only ever set to a
    /// scheme that differs from [`active`](Self::active), so `staged.is_some()` is
    /// exactly the dirty condition.
    staged: Option<String>,
    /// The discovered `scripts/generate-colors` path (R8.5), passed verbatim into the
    /// Apply contribution so the pipeline runs it with no shell.
    generate_colors: PathBuf,
}

impl PaletteModel {
    /// Builds the model by enumerating `colors_dir` and detecting the active scheme
    /// from `active_scheme_source` (task 6.3; R3.2, R8.5).
    ///
    /// `colors_dir` and `generate_colors` come from the discovered
    /// [`PaletteSource`](crate::core::detect::PaletteSource); `active_scheme_source` is
    /// the deployed generated color file (`~/.config/hypr/colors.conf`) whose header
    /// names the active scheme (task 3.7). All three are injected so the model can be
    /// built against a temporary fixture in tests. Nothing here fails: an unreadable
    /// directory yields no schemes and an unrecognized header yields an unknown active
    /// scheme, both of which the UI renders as the read-only degrade.
    pub(crate) fn load(
        colors_dir: &Path,
        active_scheme_source: &Path,
        generate_colors: PathBuf,
    ) -> PaletteModel {
        let schemes = enumerate_schemes(colors_dir);
        let active = generated::read_active_scheme(active_scheme_source)
            .name()
            .map(str::to_string);
        tracing::info!(
            schemes = schemes.len(),
            active = ?active,
            "loaded palette schemes for the Theme page (task 6.3, R3.2)"
        );
        PaletteModel {
            schemes,
            active,
            staged: None,
            generate_colors,
        }
    }

    /// The discovered schemes, in drop-down order.
    pub(crate) fn schemes(&self) -> &[Scheme] {
        &self.schemes
    }

    /// The active scheme name detected from the generated header, or `None` when
    /// unknown (R3.2).
    pub(crate) fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Whether the palette control should be an interactive drop-down (R3.2).
    ///
    /// `true` only when there are at least two schemes — with zero or one there is
    /// nothing to switch to, so the UI shows the active scheme read-only instead.
    pub(crate) fn is_switchable(&self) -> bool {
        self.schemes.len() >= 2
    }

    /// The effective selected scheme — the staged switch if one is pending, otherwise
    /// the active scheme — used to preselect the drop-down (R3.2).
    pub(crate) fn selected(&self) -> Option<&str> {
        self.staged.as_deref().or(self.active.as_deref())
    }

    /// The index of the [`selected`](Self::selected) scheme within
    /// [`schemes`](Self::schemes), for preselecting the drop-down.
    ///
    /// `None` when the selected scheme is not among the enumerated schemes — e.g. the
    /// active scheme's file is malformed or absent while others exist — in which case
    /// the UI leaves the drop-down at its default and stages nothing.
    pub(crate) fn selected_index(&self) -> Option<usize> {
        let selected = self.selected()?;
        self.schemes
            .iter()
            .position(|scheme| scheme.name == selected)
    }

    /// Stages a switch to the scheme named `name` (R3.2).
    ///
    /// Re-selecting the active scheme clears any pending switch (so the page is not
    /// dirty), matching the store's rule that re-choosing the current value is not an
    /// edit. A name that is not among the enumerated schemes is ignored — the drop-down
    /// only offers real schemes, so this is a defensive guard against an out-of-band
    /// caller.
    pub(crate) fn stage(&mut self, name: &str) {
        if !self.schemes.iter().any(|scheme| scheme.name == name) {
            tracing::warn!(
                scheme = name,
                "ignoring a palette scheme that is not in the discovered set"
            );
            return;
        }
        if self.active.as_deref() == Some(name) {
            self.staged = None;
        } else {
            self.staged = Some(name.to_string());
        }
    }

    /// Whether a scheme switch is pending (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards a pending scheme switch, returning the page to clean (R5.1).
    pub(crate) fn reset(&mut self) {
        self.staged = None;
    }

    /// Commits a pending switch after a successful Apply: the staged scheme becomes the
    /// active one, so the page is clean and the next Apply is a no-op for the palette.
    ///
    /// Called by the window only after [`apply::run`](crate::core::apply::run) reports
    /// the switch applied. There is no on-disk baseline to re-record: the app does not
    /// write the generated `colors.conf` (the generator does), so it is not tracked for
    /// conflicts.
    pub(crate) fn commit(&mut self) {
        if let Some(scheme) = self.staged.take() {
            self.active = Some(scheme);
        }
    }

    /// The palette switch to contribute to the Apply plan, or `None` when nothing is
    /// staged (task 4.5; R3.2, R8.5).
    ///
    /// The window folds this into [`ApplyPlan::palette`](crate::core::apply::ApplyPlan),
    /// so the pipeline runs the discovered `generate-colors <scheme>` as its last write
    /// step and then the palette reload chain. It carries no file write: a v1 palette
    /// switch edits no file directly.
    pub(crate) fn apply_contribution(&self) -> Option<PaletteSwitch> {
        self.staged.as_ref().map(|scheme| PaletteSwitch {
            scheme: scheme.clone(),
            generate_colors: self.generate_colors.clone(),
        })
    }
}

/// Enumerates the switchable schemes in `colors_dir`, skipping non-scheme entries
/// (R3.2, R8.5).
///
/// An entry is a scheme only when it is (1) not a dotfile, (2) a regular file
/// (following symlinks, so subdirectories are excluded), and (3) a schema-valid
/// palette. The schema-validity check is what filters out a `README.md`, an
/// `active-scheme`-style marker, or any other non-palette file: only a file with all
/// the palette's schema keys present parses as valid — the same bar
/// `generate-colors` uses, so every scheme offered here is one it would accept. The
/// result is sorted by name for a deterministic drop-down order regardless of the
/// directory's iteration order.
fn enumerate_schemes(colors_dir: &Path) -> Vec<Scheme> {
    let mut schemes = Vec::new();
    let entries = match std::fs::read_dir(colors_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::info!(
                dir = %colors_dir.display(),
                %error,
                "palette colors/ directory could not be read; no schemes enumerated (R4.4)"
            );
            return schemes;
        }
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // Skip dotfiles. The `state/active-scheme` marker lives outside `colors/`
        // (analysis §6.4), but any hidden file here is likewise not a scheme.
        if name.starts_with('.') {
            continue;
        }
        // Skip anything that is not a regular file. `metadata` follows symlinks, so a
        // scheme deployed as a symlink still counts while a subdirectory is excluded.
        match std::fs::metadata(entry.path()) {
            Ok(metadata) if metadata.is_file() => {}
            _ => continue,
        }
        // A scheme must parse as a schema-valid palette; this is what excludes
        // README.md, a state marker, or any other non-palette file (task 3.7 swatch).
        let Some(swatch) = generated::read_scheme_swatch(&entry.path()) else {
            continue;
        };
        if !swatch.validation().is_valid() {
            tracing::debug!(
                scheme = name,
                "skipping a colors/ entry that is not a complete palette"
            );
            continue;
        }
        // Accepted edge: the bar here is schema validity, not the file name, so a
        // stray but schema-valid file with an incidental name/extension (e.g.
        // `nord.bak`) is trusted and offered as a scheme — the same key-presence bar
        // `generate-colors` uses, which already excludes READMEs, markers, and
        // subdirectories as required.
        let preview = swatch
            .colors()
            .iter()
            .filter_map(|color| parse_hex_rgb(color.value()))
            .collect();
        schemes.push(Scheme {
            name: name.to_string(),
            preview,
        });
    }

    schemes.sort_by(|a, b| a.name.cmp(&b.name));
    schemes
}

/// Parses a bare six-digit hex color (e.g. `83c092`) into RGB components in
/// `0.0..=1.0`, or `None` if it is not well-formed.
///
/// Palette values are bare hex (no `#`), so this expects exactly six hex digits. A
/// scheme can be schema-valid (all keys present) yet carry a malformed value —
/// `generate-colors` checks key presence, not value format (analysis §6.4) — so a bad
/// value is skipped from the preview rather than treated as an error.
fn parse_hex_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    if hex.len() != 6 {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16)
            .ok()
            .map(|value| f64::from(value) / 255.0)
    };
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use nix::sys::signal::Signal;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities, Daemon};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::reload::ReloadParams;
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::{MockProcessSignaller, SignalCall};

    /// A complete, schema-valid `colors/<scheme>` source with all 17 keys.
    const VALID_SCHEME: &str = "\
bg0=272e33
bg1=2e383c
bg2=374145
bg3=414b50
fg0=d3c6aa
fg1=9da9a0
fg2=859289
accent0=83c092
accent1=a7c080
accent2=7fbbb3
accent3=d699b6
red=e67e80
orange=e69875
yellow=dbbc7f
green=a7c080
blue=7fbbb3
purple=d699b6
";

    /// Writes a `colors/` directory with the given scheme files, plus the non-scheme
    /// entries the enumeration must skip, and returns the directory path.
    ///
    /// Every named scheme is written as a complete valid palette. In addition it
    /// writes a `README.md` (a non-palette file), a `.hidden` dotfile (whose *content*
    /// is a valid palette, to prove the dotfile is skipped by name, not content), an
    /// `active-scheme`-style single-line marker, and a subdirectory — none of which
    /// may appear as a scheme.
    fn write_colors_dir(dir: &Path, schemes: &[&str]) {
        fs::create_dir_all(dir).expect("create colors dir");
        for scheme in schemes {
            fs::write(dir.join(scheme), VALID_SCHEME).expect("write a scheme file");
        }
        // Non-palette file: markdown, not a scheme.
        fs::write(dir.join("README.md"), b"# Palette schemes\n").expect("write README");
        // A dotfile whose content is a valid palette — must still be skipped by name.
        fs::write(dir.join(".hidden"), VALID_SCHEME).expect("write a hidden file");
        // A `state/active-scheme`-style marker: a single scheme name on one line. It
        // lives outside colors/ in the real repo, but if one appeared here it must not
        // be surfaced as a scheme.
        fs::write(dir.join("active-scheme"), b"nord\n").expect("write a marker");
        // A subdirectory must be skipped.
        fs::create_dir_all(dir.join("subdir")).expect("create a subdir");
        fs::write(dir.join("subdir").join("nested"), VALID_SCHEME).expect("write nested file");
    }

    /// A generated `colors.conf` naming `scheme` in its header (task 3.7).
    fn write_active_source(path: &Path, scheme: &str) {
        fs::write(
            path,
            format!(
                "# Generated from colors/{scheme} — do not edit manually\n$bg0 = rgb(272e33)\n"
            ),
        )
        .expect("write generated colors.conf");
    }

    #[test]
    fn enumeration_skips_dotfiles_subdirs_and_non_palette_files() {
        // Accept criterion: the scheme list is exactly the two palette files, with the
        // README, the dotfile, the state-marker, and the subdirectory all skipped —
        // so a marker never appears as a scheme.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);

        let schemes = enumerate_schemes(&colors);
        let names: Vec<&str> = schemes.iter().map(Scheme::name).collect();
        assert_eq!(
            names,
            vec!["everforest", "nord"],
            "only the two palette files are schemes, sorted by name"
        );
    }

    #[test]
    fn a_valid_scheme_carries_a_parsed_preview() {
        // The swatch parse (task 3.7) feeds a preview strip: a complete palette yields
        // all 17 colors as RGB, and the first entry (`bg0=272e33`) parses correctly.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["nord"]);

        let schemes = enumerate_schemes(&colors);
        let nord = schemes.iter().find(|s| s.name() == "nord").expect("nord");
        assert_eq!(
            nord.preview().len(),
            17,
            "a complete palette previews all 17"
        );
        let (r, g, b) = nord.preview()[0];
        assert!((r - f64::from(0x27) / 255.0).abs() < 1e-9);
        assert!((g - f64::from(0x2e) / 255.0).abs() < 1e-9);
        assert!((b - f64::from(0x33) / 255.0).abs() < 1e-9);
    }

    #[test]
    fn active_scheme_is_detected_and_preselected() {
        // Accept criterion: the active scheme is detected from the generated header
        // (task 3.7) and preselected in the drop-down.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert_eq!(model.active(), Some("nord"));
        assert_eq!(
            model.selected(),
            Some("nord"),
            "the active scheme is preselected"
        );
        // schemes are [everforest, nord]; nord is at index 1.
        assert_eq!(model.selected_index(), Some(1));
        assert!(!model.is_dirty(), "no switch is staged at load");
    }

    #[test]
    fn an_undetectable_active_scheme_preselects_nothing() {
        // Degraded path (task 6.3 review S2): the generated header does not name a
        // recognizable scheme, so detection degrades to `Unknown`. With no active
        // scheme there is nothing to preselect even though the schemes exist — the
        // UI must NOT fall back to GTK's index-0 default and present the first scheme
        // as if it were active.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        // A file with no `# Generated from colors/<scheme>` header degrades to
        // `ActiveScheme::Unknown`, so the model has no active scheme.
        let active_source = dir.path().join("colors.conf");
        fs::write(&active_source, "$bg0 = rgb(272e33)\n").expect("write a headerless file");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert!(model.is_switchable(), "two schemes are still switchable");
        assert_eq!(model.active(), None, "an unrecognized header is unknown");
        assert_eq!(
            model.selected(),
            None,
            "nothing is selected when the active scheme is unknown"
        );
        assert_eq!(
            model.selected_index(),
            None,
            "no drop-down index is preselected when the active scheme is unknown"
        );
    }

    #[test]
    fn an_active_scheme_absent_from_colors_preselects_nothing() {
        // Degraded path (task 6.3 review S2): the generated header names a scheme
        // that is not among the enumerated `colors/` files (e.g. its source file was
        // deleted or renamed). The active name is still reported, but it maps to no
        // drop-down index, so the UI preselects nothing rather than the first scheme.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "midnight");

        let model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));
        assert_eq!(
            model.active(),
            Some("midnight"),
            "the header's scheme name is reported even when its file is absent"
        );
        assert_eq!(
            model.selected(),
            Some("midnight"),
            "the selected scheme is the active one"
        );
        assert_eq!(
            model.selected_index(),
            None,
            "an active scheme absent from colors/ maps to no drop-down index"
        );
    }

    #[test]
    fn fewer_than_two_schemes_degrades_to_read_only() {
        // Accept criterion (R3.2): with zero or one scheme there is nothing to switch
        // to, so the control is not switchable (the UI shows a read-only display).
        let dir = tempfile::tempdir().expect("temp dir");
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        // One scheme -> read-only.
        let one = dir.path().join("one");
        write_colors_dir(&one, &["nord"]);
        let model = PaletteModel::load(&one, &active_source, PathBuf::from("/gen"));
        assert_eq!(model.schemes().len(), 1);
        assert!(!model.is_switchable(), "one scheme is not switchable");

        // Zero valid schemes (only non-palette entries) -> read-only.
        let none = dir.path().join("none");
        write_colors_dir(&none, &[]);
        let model = PaletteModel::load(&none, &active_source, PathBuf::from("/gen"));
        assert!(
            model.schemes().is_empty(),
            "no valid palette files -> no schemes"
        );
        assert!(!model.is_switchable());

        // Two schemes -> switchable.
        let two = dir.path().join("two");
        write_colors_dir(&two, &["everforest", "nord"]);
        let model = PaletteModel::load(&two, &active_source, PathBuf::from("/gen"));
        assert!(model.is_switchable(), "two schemes are switchable");
    }

    #[test]
    fn staging_a_different_scheme_is_dirty_and_reselecting_active_is_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let mut model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));

        model.stage("everforest");
        assert!(model.is_dirty(), "switching to a different scheme is dirty");
        assert_eq!(model.selected(), Some("everforest"));

        // Re-selecting the active scheme clears the pending switch (not dirty).
        model.stage("nord");
        assert!(
            !model.is_dirty(),
            "re-selecting the active scheme is not dirty"
        );
        assert_eq!(model.selected(), Some("nord"));

        // An unknown scheme is ignored.
        model.stage("does-not-exist");
        assert!(!model.is_dirty());
    }

    #[test]
    fn reset_discards_and_commit_promotes_the_staged_scheme() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");

        let mut model = PaletteModel::load(&colors, &active_source, PathBuf::from("/gen"));

        model.stage("everforest");
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending switch");
        assert_eq!(
            model.selected(),
            Some("nord"),
            "reset reverts to the active scheme"
        );

        model.stage("everforest");
        model.commit();
        assert!(!model.is_dirty(), "commit clears the dirty state");
        assert_eq!(
            model.active(),
            Some("everforest"),
            "commit promotes the staged scheme to active"
        );
    }

    #[test]
    fn apply_contribution_is_none_when_clean_and_carries_the_generator_path_when_dirty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");
        let generate_colors = PathBuf::from("/repo/scripts/generate-colors");

        let mut model = PaletteModel::load(&colors, &active_source, generate_colors.clone());
        assert!(
            model.apply_contribution().is_none(),
            "nothing staged -> no contribution"
        );

        model.stage("everforest");
        let switch = model
            .apply_contribution()
            .expect("a staged switch contributes a PaletteSwitch");
        assert_eq!(switch.scheme, "everforest");
        assert_eq!(switch.generate_colors, generate_colors);
    }

    #[test]
    fn applying_a_scheme_switch_runs_generate_colors_then_the_reload_chain() {
        // Accept criterion: feeding the model's contribution through the Apply pipeline
        // produces the exact command sequence — `generate-colors <scheme>` (the last
        // write step) then the palette reload chain — with NO colors/<scheme> file
        // write. This is the end-to-end proof the Theme page's palette switch drives
        // the pipeline correctly.
        let dir = tempfile::tempdir().expect("temp dir");
        let colors = dir.path().join("colors");
        write_colors_dir(&colors, &["everforest", "nord"]);
        let active_source = dir.path().join("colors.conf");
        write_active_source(&active_source, "nord");
        let generate_colors = PathBuf::from("/repo/scripts/generate-colors");

        let mut model = PaletteModel::load(&colors, &active_source, generate_colors);
        model.stage("everforest");

        let plan = ApplyPlan {
            validations: Vec::new(),
            // The switch contributes NO file write: v1 never edits colors/<scheme>.
            writes: Vec::new(),
            palette: model.apply_contribution(),
            reload_params: ReloadParams::default(),
        };
        // A palette switch writes no tracked file, so an empty tracker is correct.
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![4242])]);
        let caps = Capabilities::for_tests(
            &[Binary::Hyprctl],
            &[Daemon::Eww, Daemon::Swaync, Daemon::Kitty],
            true,
        );

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        match outcome {
            ApplyOutcome::Applied { written, .. } => {
                assert!(
                    written.is_empty(),
                    "a palette switch writes no backing file"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // generate-colors runs FIRST (the last write step), then the apply-theme chain.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/repo/scripts/generate-colors").arg("everforest"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
            ]
        );
        // The palette reload chain finishes by delivering SIGUSR1 to the running
        // kitty (seeded above), which re-reads its colors — asserting it here proves
        // the full reload chain from the model level, not just the subprocess steps.
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![4242],
            }]
        );
    }

    #[test]
    fn parse_hex_rgb_reads_bare_hex_and_rejects_malformed_values() {
        assert_eq!(parse_hex_rgb("000000"), Some((0.0, 0.0, 0.0)));
        assert_eq!(parse_hex_rgb("ffffff"), Some((1.0, 1.0, 1.0)));
        // Wrong length and non-hex characters are rejected (skipped from the preview).
        assert_eq!(parse_hex_rgb("fff"), None);
        assert_eq!(parse_hex_rgb("gggggg"), None);
        assert_eq!(parse_hex_rgb("#ffffff"), None);
    }
}
