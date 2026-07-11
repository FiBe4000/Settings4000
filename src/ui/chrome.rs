//! The Apply/Reset chrome: the pure decision logic that turns store state and Apply
//! results into what the UI should show, plus the small plain-GTK widgets that show
//! it (task 5.3; architecture §7; R5.1, R5.5, R5.6, R4.3).
//!
//! # What this module is
//!
//! Task 5.3 adds the window "chrome" around the category pages: a suggested-action
//! **Apply** button and a **Reset** button (both enabled only while the store is
//! dirty, R5.1), per-page dirty markers, a non-fatal **toast** for reload failures
//! (R5.5), and a **conflict warning** dialog when files changed on disk (R5.6). The
//! window (`super::window`) owns the widgets and the wiring; this module holds the two
//! things worth isolating:
//!
//! - **The decisions** — pure, GTK-free functions that map the domain state to a UI
//!   intent: whether the actions are enabled ([`actions_enabled`]), which model
//!   [`Category`] a sidebar page maps to for its marker ([`marker_category`]), what to
//!   do with an [`ApplyOutcome`] ([`respond_to_apply`]), and whether a
//!   [`RefreshReport`] warrants a conflict warning ([`refresh_conflict_warning`]).
//!   These carry no GTK types so they are unit-tested headlessly (R6.2) — the tests at
//!   the bottom cover the dirty→enabled/marker mapping against a real store, the
//!   apply-outcome→toast/dialog/commit mapping, and the refresh-report→dialog content.
//! - **The widgets** — [`Toast`] (the non-libadwaita transient notification) and
//!   [`show_warning`] (the conflict/error dialog), built from plain GTK4 only.
//!
//! # No libadwaita, no custom CSS (R2.1)
//!
//! The toast is a [`Revealer`] over the content in a `GtkOverlay`, not an
//! `adw::Toast` — libadwaita is deliberately not used (architecture §7). The dialog is
//! a plain modal [`Window`], not the deprecated `MessageDialog`. Visual emphasis uses
//! only *built-in* theme style classes via `add_css_class` (the suggested-action Apply
//! button in the window, the `osd` class on the toast): selecting styling the active
//! theme already provides is not custom CSS, so it does not trip
//! `tests/no_custom_css.rs`.

use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, Button, Label, Orientation, Revealer,
    RevealerTransitionType, Window, glib,
};

use crate::core::apply::ApplyOutcome;
use crate::core::model::Category;
use crate::core::reload::ReloadError;
use crate::core::store::{RefreshReport, SettingsStore};
use crate::ui::category::SidebarCategory;

/// How long a non-fatal toast stays visible before it auto-hides (R5.5).
///
/// Long enough to read a short reload-failure summary, short enough not to linger over
/// the content. The toast is also dismissable at once via its button.
const TOAST_VISIBLE_SECONDS: u32 = 8;

/// Whether the Apply and Reset controls should be enabled, from the store's dirty
/// state (R5.1).
///
/// Both are driven by the same condition — there is something to apply or discard iff
/// a file-backed setting has a pending edit — so this is the single predicate the
/// window binds the two buttons' `sensitive` to.
pub(crate) fn actions_enabled(store: &SettingsStore) -> bool {
    store.is_dirty()
}

/// The file-backed [`Category`] a sidebar page maps to for its per-page dirty marker,
/// or `None` for a runtime-only page that carries no staged state (R5.1/R5.2).
///
/// The window sets a stack page's `needs-attention` property from
/// `store.is_category_dirty(category)` for the returned category, so the sidebar shows
/// a marker on a page with pending edits. [`SidebarCategory::Sound`] and
/// [`SidebarCategory::Network`] are runtime-only (R3.1) — their controls apply
/// immediately and are never staged — so they have no [`Category`] and never carry a
/// dirty marker.
pub(crate) fn marker_category(category: SidebarCategory) -> Option<Category> {
    match category {
        SidebarCategory::Display => Some(Category::Display),
        SidebarCategory::Theme => Some(Category::Theme),
        SidebarCategory::Input => Some(Category::Input),
        SidebarCategory::Notifications => Some(Category::Notifications),
        SidebarCategory::PowerAndIdle => Some(Category::PowerAndIdle),
        SidebarCategory::Sound | SidebarCategory::Network => None,
    }
}

/// The UI response the window performs for a terminal [`ApplyOutcome`] — the pure
/// decision that keeps the window's click handler a thin dispatcher (R5.3–R5.6).
///
/// Exactly one is returned per apply. The distinction the window acts on is *commit vs.
/// warn*: an [`ApplyOutcome::Applied`] means the writes stand, so the window must
/// commit the apply to the store (promote staged→original and re-baseline the written
/// files' freshness — see [`respond_to_apply`]); every other outcome aborted before or
/// during the write and must be surfaced without committing.
#[derive(Debug)]
pub(crate) enum ApplyResponse {
    /// The writes stood (R5.5). The window commits the apply to the store, then — if
    /// `toast` is `Some` — shows it as a non-fatal notification. A `toast` is present
    /// only when some component could not be reloaded; the file writes still stand.
    Commit {
        /// A non-fatal reload-failure summary to toast, or `None` for a fully clean
        /// apply.
        toast: Option<String>,
    },
    /// The apply aborted or failed (invalid values, an external conflict, or a write
    /// failure). The window shows this as a modal warning and does **not** commit, so
    /// the staged edits are kept for correction/retry.
    Warn {
        /// A short heading for the dialog title/summary.
        heading: String,
        /// The body text explaining what went wrong and what happened to the files.
        detail: String,
    },
}

/// Decides how the window should respond to `outcome` (R5.3–R5.6).
///
/// The load-bearing mapping the window relies on:
///
/// - [`ApplyOutcome::Applied`] → [`ApplyResponse::Commit`]. The writes reached disk, so
///   the window must call
///   [`SettingsStore::commit_apply`](crate::core::store::SettingsStore::commit_apply)
///   with the plan's written paths + bytes; the pipeline is a pure orchestrator and
///   never touches the store, so without this commit the store stays dirty and the
///   next apply spuriously conflicts on the app's own write (task 4.5). A non-empty
///   `reload_failures` becomes a toast (R5.5) — the reloads are non-fatal and the
///   writes still stand.
/// - [`ApplyOutcome::ValidationFailed`] / [`ApplyOutcome::Conflicted`] /
///   [`ApplyOutcome::WriteFailed`] → [`ApplyResponse::Warn`]. Nothing durable changed
///   that the store must learn about (a write failure already rolled back), so no
///   commit; the user sees the reason and the staged edits are retained.
pub(crate) fn respond_to_apply(outcome: &ApplyOutcome) -> ApplyResponse {
    match outcome {
        ApplyOutcome::Applied {
            reload_failures, ..
        } => {
            let toast =
                (!reload_failures.is_empty()).then(|| reload_failure_summary(reload_failures));
            ApplyResponse::Commit { toast }
        }
        ApplyOutcome::ValidationFailed(invalid) => {
            let mut detail =
                String::from("Some values could not be applied, so nothing was written:");
            for setting in invalid {
                // TODO(§6): prefix each line with a user-facing setting label — those
                // labels live with the §6 category pages. Until then show only the
                // validation message (which describes the value problem), never the
                // raw `SettingId` Debug string, which is an internal Rust name.
                detail.push_str(&format!("\n• {}", setting.error));
            }
            ApplyResponse::Warn {
                heading: "Some settings are invalid".to_string(),
                detail,
            }
        }
        ApplyOutcome::Conflicted(conflicts) => {
            let mut detail = String::from(
                "One or more configuration files changed on disk since Settings4000 read \
                 them, so nothing was written. Refresh to reload them, then try again:",
            );
            for conflict in conflicts {
                detail.push_str(&format!("\n• {}", conflict.path().display()));
            }
            ApplyResponse::Warn {
                heading: "Files changed on disk".to_string(),
                detail,
            }
        }
        ApplyOutcome::WriteFailed(failure) => {
            let mut detail = format!("The changes could not be saved: {}", failure.cause);
            if failure.rollback_failures.is_empty() {
                detail.push_str("\n\nAll files were restored to their previous contents.");
            } else {
                detail.push_str(
                    "\n\nWARNING: some files could not be restored and may be left changed:",
                );
                for (path, error) in &failure.rollback_failures {
                    detail.push_str(&format!("\n• {}: {error}", path.display()));
                }
            }
            ApplyResponse::Warn {
                heading: "Could not apply changes".to_string(),
                detail,
            }
        }
    }
}

/// The heading and body for a conflict warning built from a [`RefreshReport`], or
/// `None` when the refresh was quiet (R5.6).
#[derive(Debug)]
pub(crate) struct ConflictWarning {
    /// A short heading for the dialog.
    pub(crate) heading: String,
    /// The body listing which files reloaded and which could not.
    pub(crate) detail: String,
}

/// Turns a [`RefreshReport`] into a conflict warning, or `None` if no tracked file
/// changed externally (R5.6).
///
/// Called after [`SettingsStore::refresh`](crate::core::store::SettingsStore::refresh)
/// on a manual refresh (and, later, window focus): a non-empty report means another
/// program edited a backing file since the app read it. The two lists are surfaced
/// distinctly because they mean different things — a *reloaded* file's originals were
/// refreshed (and any pending edit kept), whereas a *failed* file could not be re-read
/// (deleted or made unreadable) and keeps its last-known values.
pub(crate) fn refresh_conflict_warning(report: &RefreshReport) -> Option<ConflictWarning> {
    if report.is_empty() {
        return None;
    }

    let mut detail = String::from(
        "Configuration files were changed by another program since Settings4000 read them.",
    );
    if !report.reloaded().is_empty() {
        detail.push_str("\n\nReloaded from disk (any unsaved edits to these were kept):");
        for path in report.reloaded() {
            detail.push_str(&format!("\n• {}", path.display()));
        }
    }
    if !report.failed().is_empty() {
        detail
            .push_str("\n\nCould not be reloaded (they may have been deleted or made unreadable):");
        for path in report.failed() {
            detail.push_str(&format!("\n• {}", path.display()));
        }
    }

    Some(ConflictWarning {
        heading: "Files changed on disk".to_string(),
        detail,
    })
}

/// Formats a non-fatal reload-failure summary for a toast (R5.5).
///
/// Each failed reload is listed with its [`Display`](std::fmt::Display) message. The
/// framing makes clear the changes were saved (the writes stood) even though a
/// component could not be reloaded — the user just may need to restart that component
/// for the change to take visible effect.
fn reload_failure_summary(failures: &[ReloadError]) -> String {
    let mut summary =
        String::from("Your changes were saved, but some components could not be reloaded:");
    for failure in failures {
        summary.push_str(&format!("\n• {failure}"));
    }
    summary.push_str("\nThey will pick up the change the next time they start.");
    summary
}

/// A transient, non-fatal notification shown over the window content (R5.5).
///
/// This is the plain-GTK4 stand-in for `adw::Toast`, which the app cannot use because
/// libadwaita is not a dependency (architecture §7). It is a [`Revealer`] holding a
/// message [`Label`] and a Dismiss [`Button`]; the window adds
/// [`Self::revealer`] as an overlay child of a `GtkOverlay`, aligned to the bottom.
/// [`Self::show`] slides it in, and it auto-hides after [`TOAST_VISIBLE_SECONDS`] or
/// when the user clicks Dismiss. It is [`Clone`] because its fields are refcounted GTK
/// objects, so the window can hand a clone to the Apply handler while keeping one to
/// mount in the overlay — both refer to the same underlying widgets.
#[derive(Clone)]
pub(crate) struct Toast {
    /// The revealer the window mounts as an overlay child; also what
    /// [`Self::show`]/Dismiss reveal and hide.
    revealer: Revealer,
    /// The label whose text is set to the message on each [`Self::show`].
    label: Label,
}

impl Toast {
    /// Builds a hidden toast ready to mount as an overlay child.
    pub(crate) fn new() -> Self {
        let label = Label::new(None);
        label.set_wrap(true);
        label.set_xalign(0.0);
        label.set_hexpand(true);

        let dismiss = Button::with_label("Dismiss");
        dismiss.set_valign(Align::Center);

        let bar = GtkBox::new(Orientation::Horizontal, 12);
        bar.set_margin_top(8);
        bar.set_margin_bottom(8);
        bar.set_margin_start(12);
        bar.set_margin_end(12);
        bar.append(&label);
        bar.append(&dismiss);
        // `osd` is a built-in theme style class for on-screen-display overlays, so the
        // toast reads as a floating notification without any custom CSS (R2.1). If the
        // active theme does not define it, the toast still works, just unstyled.
        bar.add_css_class("osd");

        let revealer = Revealer::new();
        revealer.set_child(Some(&bar));
        revealer.set_transition_type(RevealerTransitionType::SlideUp);
        revealer.set_reveal_child(false);
        // Float it at the bottom-centre of the content it overlays, not filling it.
        revealer.set_halign(Align::Center);
        revealer.set_valign(Align::End);
        revealer.set_margin_bottom(18);

        {
            let revealer = revealer.clone();
            dismiss.connect_clicked(move |_| revealer.set_reveal_child(false));
        }

        Toast { revealer, label }
    }

    /// The revealer to add as an overlay child (`GtkOverlay::add_overlay`).
    pub(crate) fn revealer(&self) -> &Revealer {
        &self.revealer
    }

    /// Shows `message` and schedules the toast to auto-hide (R5.5).
    ///
    /// The auto-hide is a one-shot main-loop timeout; the Dismiss button hides it
    /// sooner. (A later `show` before the timeout fires simply replaces the text and
    /// adds another timeout — acceptable for the rare reload-failure case.)
    pub(crate) fn show(&self, message: &str) {
        self.label.set_text(message);
        self.revealer.set_reveal_child(true);

        let revealer = self.revealer.clone();
        glib::timeout_add_seconds_local(TOAST_VISIBLE_SECONDS, move || {
            revealer.set_reveal_child(false);
            glib::ControlFlow::Break
        });
    }
}

/// Shows a modal warning dialog transient for `parent` (R5.5/R5.6).
///
/// Built as a plain modal [`Window`] rather than the deprecated `MessageDialog` (which
/// would fail the `-D warnings` clippy gate) or an `adw::AlertDialog` (libadwaita is
/// not used). The `heading` is the window title (a warning dialog's summary belongs in
/// the title bar, and repeating it in the body would show it twice); the body is the
/// wrapped `detail` plus a Close button. The user dismisses it and the staged edits are
/// untouched, so they can correct and retry.
pub(crate) fn show_warning(parent: &ApplicationWindow, heading: &str, detail: &str) {
    let dialog = Window::builder()
        .title(heading)
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let detail_label = Label::new(Some(detail));
    detail_label.set_halign(Align::Start);
    detail_label.set_wrap(true);
    detail_label.set_xalign(0.0);

    let close = Button::with_label("Close");
    close.set_halign(Align::End);
    {
        // A weak reference avoids a reference cycle (the button, a descendant of the
        // dialog, would otherwise strongly hold the dialog that owns it) so the dialog
        // is freed once closed.
        let weak = dialog.downgrade();
        close.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
        });
    }

    content.append(&detail_label);
    content.append(&close);
    dialog.set_child(Some(&content));
    dialog.present();
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::core::apply::{
        self, ApplyPlan, FileWrite, InvalidSetting, WriteFailure, WriteFailureCause,
    };
    use crate::core::detect::Capabilities;
    use crate::core::freshness::FreshnessTracker;
    use crate::core::model::{SettingId, Value};
    use crate::core::reload::{BackingFile, ReloadParams};
    use crate::core::store::{FileReader, FileValues, SettingsStore};
    use crate::system::command::MockCommandRunner;
    use crate::system::signal::MockProcessSignaller;

    /// A store loaded with `originals` under a synthetic key, with a reader that simply
    /// re-serves them. These tests never refresh, so the key is never resolved on disk.
    fn store_with(originals: &[(SettingId, Value)]) -> SettingsStore {
        let values = originals.to_vec();
        let reader_values = values.clone();
        let reader: FileReader = Box::new(move |_path: &Path| {
            Ok(FileValues {
                bytes: Vec::new(),
                values: reader_values.clone(),
            })
        });
        let mut store = SettingsStore::new();
        store.load_file(
            PathBuf::from("<chrome test seed>"),
            FileValues {
                bytes: Vec::new(),
                values,
            },
            reader,
        );
        store
    }

    #[test]
    fn every_file_backed_sidebar_category_maps_to_its_model_category() {
        assert_eq!(
            marker_category(SidebarCategory::Display),
            Some(Category::Display)
        );
        assert_eq!(
            marker_category(SidebarCategory::Theme),
            Some(Category::Theme)
        );
        assert_eq!(
            marker_category(SidebarCategory::Input),
            Some(Category::Input)
        );
        assert_eq!(
            marker_category(SidebarCategory::Notifications),
            Some(Category::Notifications)
        );
        assert_eq!(
            marker_category(SidebarCategory::PowerAndIdle),
            Some(Category::PowerAndIdle)
        );
        // Runtime-only pages carry no staged state, so no marker (R5.2).
        assert_eq!(marker_category(SidebarCategory::Sound), None);
        assert_eq!(marker_category(SidebarCategory::Network), None);
    }

    #[test]
    fn actions_track_dirty_and_reset_clears_them() {
        // Accept criterion: dirty markers track store state; Reset clears. The chrome's
        // per-page marker is `store.is_category_dirty`, and its Apply/Reset enablement
        // is `actions_enabled`; both follow a real stage → reset cycle.
        let mut store = store_with(&[
            (SettingId::MouseSensitivity, Value::Float(0.0)),
            (SettingId::NotificationTimeout, Value::Integer(10)),
        ]);
        assert!(
            !actions_enabled(&store),
            "a clean store disables Apply/Reset"
        );
        assert!(!store.is_category_dirty(Category::Input));

        store
            .stage(SettingId::MouseSensitivity, Value::Float(0.5))
            .expect("a valid edit stages");
        assert!(actions_enabled(&store), "a dirty store enables Apply/Reset");
        assert!(
            store.is_category_dirty(Category::Input),
            "the edited page's marker lights up"
        );
        assert!(
            !store.is_category_dirty(Category::Notifications),
            "an unedited page stays unmarked"
        );

        store.reset();
        assert!(!actions_enabled(&store), "Reset clears the dirty state");
        assert!(
            !store.is_category_dirty(Category::Input),
            "Reset clears the per-page marker"
        );
    }

    #[test]
    fn a_clean_apply_commits_without_a_toast() {
        let outcome = ApplyOutcome::Applied {
            reload_failures: Vec::new(),
            written: Vec::new(),
        };
        match respond_to_apply(&outcome) {
            ApplyResponse::Commit { toast } => {
                assert!(toast.is_none(), "a clean apply commits and shows no toast")
            }
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    #[test]
    fn an_apply_with_a_reload_failure_commits_and_toasts() {
        // R5.5: the writes stand (Commit), and the non-fatal reload failure surfaces as
        // a toast naming the failed component.
        let outcome = ApplyOutcome::Applied {
            reload_failures: vec![ReloadError::NonZeroExit {
                program: "hyprctl".to_string(),
                code: Some(1),
            }],
            written: Vec::new(),
        };
        match respond_to_apply(&outcome) {
            ApplyResponse::Commit { toast } => {
                let text = toast.expect("a reload failure must produce a toast");
                assert!(
                    text.contains("hyprctl"),
                    "the toast names the failed reload: {text}"
                );
            }
            other => panic!("expected Commit with a toast, got {other:?}"),
        }
    }

    #[test]
    fn a_validation_failure_warns_and_does_not_commit() {
        let error = SettingId::WallpaperPath
            .validate(&Value::String("/definitely/missing.png".to_string()))
            .expect_err("a missing wallpaper path is invalid");
        let outcome = ApplyOutcome::ValidationFailed(vec![InvalidSetting {
            id: SettingId::WallpaperPath,
            error,
        }]);
        match respond_to_apply(&outcome) {
            ApplyResponse::Warn { detail, .. } => {
                assert!(
                    detail.contains("no file exists"),
                    "the warning shows the validation message: {detail}"
                );
                assert!(
                    !detail.contains("WallpaperPath"),
                    "the warning must not expose the raw SettingId Debug name: {detail}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn a_write_failure_warns_with_the_cause() {
        let outcome = ApplyOutcome::WriteFailed(WriteFailure {
            cause: WriteFailureCause::GenerateColorsExit { code: Some(1) },
            rolled_back: Vec::new(),
            rollback_failures: Vec::new(),
        });
        match respond_to_apply(&outcome) {
            ApplyResponse::Warn { heading, detail } => {
                assert!(!heading.is_empty());
                assert!(
                    detail.contains("generate-colors"),
                    "the warning explains the cause: {detail}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn a_conflicted_apply_warns_and_does_not_commit() {
        // The only way to obtain a `Conflicted` outcome is a genuine conflict, so run
        // the pipeline against a file edited externally after its baseline was recorded
        // (R5.6), then assert the chrome maps it to a warning naming the file.
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("input.conf");
        fs::write(&target, b"kb_layout = us\n").expect("write the fixture");
        let mut tracker = FreshnessTracker::new();
        tracker.record(&target).expect("baseline the file");
        fs::write(&target, b"kb_layout = se\n").expect("external edit");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: target.clone(),
                contents: b"kb_layout = us,se\n".to_vec(),
                changed_keys: vec!["kb_layout".to_string()],
                backing: BackingFile::InputConf,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[], &[], false);
        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);

        assert!(
            matches!(outcome, ApplyOutcome::Conflicted(_)),
            "the external edit must conflict"
        );
        match respond_to_apply(&outcome) {
            ApplyResponse::Warn { detail, .. } => assert!(
                detail.contains("input.conf"),
                "the warning names the conflicting file: {detail}"
            ),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn a_quiet_refresh_produces_no_warning() {
        assert!(refresh_conflict_warning(&RefreshReport::default()).is_none());
    }

    #[test]
    fn an_external_change_report_becomes_a_conflict_warning() {
        // R5.6: build a real RefreshReport from a store whose backing file was edited
        // externally, and assert the warning names the reloaded file.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("timeout.conf");
        fs::write(&path, "300").expect("write the fixture");
        let bytes = fs::read(&path).expect("read the fixture");
        let reader: FileReader = Box::new(|p: &Path| {
            let bytes = fs::read(p)?;
            let seconds: i64 = String::from_utf8_lossy(&bytes)
                .trim()
                .parse()
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not an integer")
                })?;
            Ok(FileValues {
                bytes,
                values: vec![(SettingId::NotificationTimeout, Value::Integer(seconds))],
            })
        });
        let mut store = SettingsStore::new();
        store.load_file(
            path.clone(),
            FileValues {
                bytes,
                values: vec![(SettingId::NotificationTimeout, Value::Integer(300))],
            },
            reader,
        );

        fs::write(&path, "600").expect("external edit");
        let report = store.refresh();

        let warning = refresh_conflict_warning(&report).expect("an external change must warn");
        assert!(
            warning.detail.contains("timeout.conf"),
            "the warning names the changed file: {}",
            warning.detail
        );
    }
}
