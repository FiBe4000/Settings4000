//! The Apply-time write glue shared by every page whose settings are staged in the
//! [`SettingsStore`](crate::core::store) and land in exactly one backing file (task 9.17;
//! architecture §6; R5.3, R5.6, R8.3).
//!
//! # What this module is
//!
//! Three pages are file-backed in the plainest possible way: Input (`input.conf`),
//! Notifications (swaync's `config.json`) and Power & Idle (`hypridle.conf`). Every one of
//! their settings is an ordinary [`SettingId`] staged in the shared store, and an Apply
//! turns that page's dirty settings into exactly one [`FileWrite`]. The interesting part of
//! each is its **renderer** — which parser it drives and which addresses it edits — and
//! that stays with the page ([`crate::core::input::render_input_conf`] and its two
//! siblings). What is not interesting, and was copied out once per page before this module
//! existed, is the glue around it:
//!
//! 1. nothing dirty → no write at all (`Ok(None)`), so the pipeline also plans no reload;
//! 2. read the file's current bytes, logging and aborting if that fails;
//! 3. hand those bytes and the dirty settings to the page's renderer, logging and aborting
//!    if *that* fails;
//! 4. wrap the rendered bytes in a [`FileWrite`] carrying the file's path, its reload
//!    concern and its R8.3 validation provenance.
//!
//! [`StoreBackedFile`] holds the per-file data steps 2–4 need, its
//! [`render_write`](StoreBackedFile::render_write) is the one implementation of the steps
//! themselves, and [`StoreWriteError`] is the one failure type — generic over the
//! renderer's own error, which is the only genuinely page-specific piece of it (the
//! hyprlang writer's [`EditError`](crate::parsers::hyprlang::EditError) for two of the
//! three files, an unparseable-JSON failure for swaync's).
//!
//! # Why a failure here must abort the whole Apply, not skip the file
//!
//! A [`StoreWriteError`] deliberately does not mean "skip this file and apply the rest".
//! The staged values are still in the store, and the store is committed after a successful
//! Apply: skipping the write would promote values that never reached disk, leaving the app
//! showing settings the desktop does not actually have — silently, and for as long as the
//! app runs (R8.3). So [`crate::core::assemble`] turns any error from here into a refusal
//! to plan the Apply at all: nothing is written, nothing is committed, and the user's edits
//! survive for a retry.
//!
//! # Why the file is re-read on every Apply
//!
//! [`render_write`](StoreBackedFile::render_write) reads the file each time rather than
//! caching the bytes (or a parsed copy) from startup. That is what keeps it correct across
//! repeated applies without a bespoke
//! [`FreshnessTracker`](crate::core::freshness::FreshnessTracker) per page: these files are
//! store-loaded, so the store baselines them at load and the Apply pipeline's conflict
//! check aborts an Apply whose file changed externally (R5.6). A fresh read can therefore
//! never clobber an external edit — it either matches the baseline or the Apply never gets
//! this far.

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::core::apply::{FileWrite, WriteValidation};
use crate::core::model::{SettingId, Value};
use crate::core::reload::BackingFile;

/// The complete new bytes of one backing file plus the labels of the keys that changed —
/// what a page's renderer produces from the store's dirty settings.
///
/// One shared type for all three store-backed pages: the renderers differ, their results
/// do not. The `contents` are always the *whole* file (the pipeline writes them verbatim),
/// never a patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedEdit {
    /// The complete new file contents: the file as it was read, with only the edited value
    /// spans rewritten (surgical, span-preserving — architecture §3).
    pub contents: Vec<u8>,
    /// The keys this edit changed, in the page's own notation (e.g. `input.kb_layout`,
    /// `listener[1].timeout`, `positionY`). Used only for the apply-level log line (R7.3),
    /// never for the file contents.
    pub changed_keys: Vec<String>,
}

/// One store-backed backing file: where it lives, plus everything the shared glue needs in
/// order to write it and to describe a failure to write it.
///
/// A page's model holds one of these in place of a bare `PathBuf`, built once at load, so
/// its Apply-time contribution is a single delegation to
/// [`render_write`](Self::render_write). Everything but the path is fixed per file and
/// stated at the file's own definition site, which is what keeps the glue a single
/// implementation without hiding any of the per-file decisions inside it.
pub struct StoreBackedFile {
    /// The file's live XDG runtime path (R8.5) — read to render an edit and named as the
    /// write's target. Never a hardcoded `~/.dotfiles` path; the writer canonicalizes it,
    /// so a symlink into a dotfiles repo has its real target rewritten and the link
    /// preserved.
    pub path: PathBuf,
    /// How the file is named in the failure message the user sees, e.g. `input.conf` or
    /// `swaync config.json`.
    ///
    /// A short display name rather than [`Self::path`]: the dialog's job is to tell the
    /// user which configuration file to go and fix, and the full runtime path (with its
    /// symlink indirection into the dotfiles repo) is noise for that.
    pub name: &'static str,
    /// The reload concern this file drives, which decides what the pipeline reloads after
    /// writing it (task 4.4).
    pub backing: BackingFile,
    /// What every write of this file declares about where its values get their R8.3
    /// validation ([`WriteValidation`]).
    ///
    /// Task 9.10 made this a per-*write* claim, because one file can carry either kind
    /// depending on which value changed. A store-backed file is the case where it is
    /// nonetheless constant: every write of it is rendered from the same dirty store
    /// settings that
    /// [`base_apply_plan`](crate::core::assemble::base_apply_plan) puts into the plan's
    /// `validations`, so [`WriteValidation::InPlan`] holds by construction. It is a field
    /// rather than a constant buried in the glue for two reasons: the declaration stays
    /// where a reader can check it against the file it describes, and a future page that
    /// renders something the plan does *not* validate has to say so rather than silently
    /// inheriting a claim that would then be false.
    pub validation: WriteValidation,
    /// The `error`-level line logged when [`Self::path`] cannot be read (R7.3).
    ///
    /// Spelled out per file rather than composed from one format string, for the same
    /// reason as [`crate::core::assemble`]'s abort lines: each stays a literal that can be
    /// grepped from a journal entry straight back to the code that emitted it.
    pub read_failure_log: &'static str,
    /// The `error`-level line logged when the page's renderer rejects the edits (R7.3).
    /// A per-file literal, for the reason given on [`Self::read_failure_log`].
    pub render_failure_log: &'static str,
}

impl StoreBackedFile {
    /// Renders the page's `dirty` store settings into this file's [`FileWrite`], or reports
    /// why no write could be prepared.
    ///
    /// `dirty` must be the store's dirty settings for the page that owns this file (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category));
    /// that is the precondition [`Self::validation`] rests on. `render` is the page's own
    /// renderer, which applies those settings to the bytes just read through the file's
    /// parser (e.g. [`crate::core::input::render_input_conf`]). Its error need only be
    /// [`Display`](fmt::Display), which is all the glue does with it: log the reason, and
    /// carry it for the caller to quote.
    ///
    /// Returns:
    /// - `Ok(None)` when there is nothing to write — either `dirty` is empty (the common
    ///   clean case) or the renderer changed no key at all. Either way the pipeline sees no
    ///   change to this file and so plans no reload for it.
    /// - `Ok(Some(write))` with the one surgical write.
    /// - `Err(_)` when there *are* dirty settings but the write cannot be produced: the
    ///   file is unreadable, or the renderer rejected an edit. The caller must abort the
    ///   whole Apply — see the module docs.
    pub fn render_write<E: fmt::Display>(
        &self,
        dirty: &[(SettingId, Value)],
        render: impl FnOnce(&[u8], &[(SettingId, Value)]) -> Result<RenderedEdit, E>,
    ) -> Result<Option<FileWrite>, StoreWriteError<E>> {
        if dirty.is_empty() {
            return Ok(None);
        }
        let bytes = std::fs::read(&self.path).map_err(|error| {
            tracing::error!(path = %self.path.display(), %error, "{}", self.read_failure_log);
            StoreWriteError::Read {
                file: self.name,
                error,
            }
        })?;
        let edit = render(&bytes, dirty).map_err(|error| {
            tracing::error!(path = %self.path.display(), %error, "{}", self.render_failure_log);
            StoreWriteError::Render {
                file: self.name,
                error,
            }
        })?;
        // A renderer that applied none of the dirty settings has nothing to write. For
        // today's three pages that is unreachable — the store rejects a value of the wrong
        // kind when it is staged, and `dirty_in_category` never yields a setting from
        // another page, so every dirty setting maps to a key the renderer changes — but the
        // check is what stops that assumption from turning into a byte-identical no-op
        // write (and the pointless reload behind it) should a future renderer be able to
        // skip everything it was given.
        if edit.changed_keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileWrite {
            path: self.path.clone(),
            contents: edit.contents,
            changed_keys: edit.changed_keys,
            backing: self.backing,
            validation: self.validation,
        }))
    }
}

/// Why one store-backed file's write could not be prepared, despite its page having dirty
/// settings to apply (task 9.17).
///
/// Generic over `E`, the page renderer's own failure: the hyprlang writer's
/// [`EditError`](crate::parsers::hyprlang::EditError) for `input.conf` and
/// `hypridle.conf`, an unparseable-JSON failure for swaync's `config.json`. Both variants
/// carry the file's display name because the [`Display`](fmt::Display) text is quoted
/// verbatim in the dialog the user sees (`ui::chrome`'s `assembly_warning`), which has to
/// say which file to go and fix.
///
/// This is distinct from "nothing was dirty", which is a plain `Ok(None)`. Why an error
/// here has to abort the Apply rather than skip the file is in the module docs.
#[derive(Debug)]
pub enum StoreWriteError<E> {
    /// The file could not be read, so there were no current bytes to render the edits
    /// into.
    Read {
        /// The file's display name, as it appears in the message.
        file: &'static str,
        /// The underlying read failure.
        error: io::Error,
    },
    /// The page's renderer rejected the edits — a value it cannot represent, a section or
    /// record it cannot write into, or a file that no longer parses.
    Render {
        /// The file's display name, as it appears in the message.
        file: &'static str,
        /// The renderer's own failure.
        error: E,
    },
}

impl<E: fmt::Display> fmt::Display for StoreWriteError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreWriteError::Read { file, error } => write!(f, "{file} could not be read: {error}"),
            StoreWriteError::Render { file, error } => {
                write!(f, "the {file} edit could not be applied: {error}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for StoreWriteError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreWriteError::Read { error, .. } => Some(error),
            StoreWriteError::Render { error, .. } => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::model::SettingId;

    /// A renderer failure with a [`Display`] the shared glue must quote verbatim.
    #[derive(Debug)]
    struct FakeRenderError;

    impl fmt::Display for FakeRenderError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "the renderer said no")
        }
    }

    /// A store-backed file description standing in for a real page's, so these tests cover
    /// the glue rather than any one page's renderer.
    fn target(path: PathBuf) -> StoreBackedFile {
        StoreBackedFile {
            path,
            name: "test.conf",
            backing: BackingFile::InputConf,
            validation: WriteValidation::InPlan,
            read_failure_log: "could not read test.conf",
            render_failure_log: "could not render test.conf",
        }
    }

    /// One dirty setting, enough to get past the empty-dirty early return.
    fn one_dirty() -> Vec<(SettingId, Value)> {
        vec![(SettingId::MouseSensitivity, Value::Float(0.5))]
    }

    #[test]
    fn nothing_dirty_renders_no_write_without_touching_the_file() {
        // The common clean case: with nothing dirty the glue must not even read the file,
        // so a page whose backing file has since vanished still assembles a clean Apply.
        let file = target(PathBuf::from("/nonexistent/test.conf"));
        let write = file
            .render_write(&[], |_, _| -> Result<RenderedEdit, FakeRenderError> {
                panic!("the renderer must not run with nothing dirty")
            })
            .expect("no error");
        assert!(write.is_none());
    }

    #[test]
    fn a_rendered_edit_becomes_a_file_write_carrying_the_file_declarations() {
        // The happy path: the renderer's bytes and changed keys go into the FileWrite
        // unchanged, and the write carries the path, reload concern and R8.3 validation
        // provenance the file declared (task 9.10).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.conf");
        fs::write(&path, b"before\n").expect("write the fixture");
        let file = target(path.clone());

        let write = file
            .render_write(&one_dirty(), |bytes, dirty| {
                assert_eq!(bytes, b"before\n", "the renderer sees the current bytes");
                assert_eq!(dirty.len(), 1, "and the dirty settings it must apply");
                Ok::<_, FakeRenderError>(RenderedEdit {
                    contents: b"after\n".to_vec(),
                    changed_keys: vec!["some.key".to_string()],
                })
            })
            .expect("no error")
            .expect("a dirty setting produces a write");

        assert_eq!(write.path, path);
        assert_eq!(write.contents, b"after\n");
        assert_eq!(write.changed_keys, vec!["some.key".to_string()]);
        assert_eq!(write.backing, BackingFile::InputConf);
        assert_eq!(write.validation, WriteValidation::InPlan);
    }

    #[test]
    fn a_renderer_that_changed_no_key_renders_no_write() {
        // The guard against a byte-identical no-op write: a renderer that applied none of
        // the edits leaves the file (and therefore the reloads) alone.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.conf");
        fs::write(&path, b"before\n").expect("write the fixture");
        let write = target(path)
            .render_write(&one_dirty(), |_, _| {
                Ok::<_, FakeRenderError>(RenderedEdit {
                    contents: b"before\n".to_vec(),
                    changed_keys: Vec::new(),
                })
            })
            .expect("no error");
        assert!(write.is_none());
    }

    #[test]
    fn an_unreadable_file_fails_naming_the_file_for_the_dialog() {
        // The abort-not-skip contract (R8.3): dirty settings plus an unreadable file is an
        // error, and its message — quoted verbatim in the dialog — names the file.
        let dir = tempfile::tempdir().expect("temp dir");
        let file = target(dir.path().join("gone.conf"));
        let error = file
            .render_write(
                &one_dirty(),
                |_, _| -> Result<RenderedEdit, FakeRenderError> {
                    panic!("the renderer must not run when the file cannot be read")
                },
            )
            .expect_err("an unreadable file must fail");
        assert!(matches!(error, StoreWriteError::Read { .. }));
        assert!(
            error
                .to_string()
                .starts_with("test.conf could not be read: "),
            "the message names the file: {error}"
        );
    }

    #[test]
    fn a_rejected_edit_fails_quoting_the_renderers_own_reason() {
        // The other abort path: the file was readable but the renderer refused. The
        // renderer's reason must survive into the message the dialog quotes.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.conf");
        fs::write(&path, b"before\n").expect("write the fixture");
        let error = target(path)
            .render_write(
                &one_dirty(),
                |_, _| -> Result<RenderedEdit, FakeRenderError> { Err(FakeRenderError) },
            )
            .expect_err("a rejected edit must fail");
        assert!(matches!(error, StoreWriteError::Render { .. }));
        assert_eq!(
            error.to_string(),
            "the test.conf edit could not be applied: the renderer said no"
        );
    }
}
