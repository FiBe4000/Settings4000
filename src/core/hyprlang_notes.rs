//! User-facing notes for hyprlang config files whose layout makes a value the app
//! edits ambiguous (task 9.31; R5.3, R4.4, R7.3).
//!
//! # The problem
//!
//! A hand-edited `hypridle.conf` can declare its `general { }` block twice. hyprlang
//! merges the two blocks and the later assignment of a key wins, while the app
//! addresses a value by section path ([`KeyPath`](crate::parsers::hyprlang::KeyPath)),
//! which names **one** block — the first, since every page addresses its plain
//! categories with [`KeyPath::at`](crate::parsers::hyprlang::KeyPath::at). The
//! app therefore shows the first block's value and edits it, and if the later block
//! assigns the same key, the edit is written correctly but has no effect: R5.3's
//! "the change takes live effect" is silently not met. The same holds for two
//! hyprpaper `wallpaper { }` blocks naming the same monitor. The file is valid and
//! every other setting works, so this is neither an error nor a reason to hide the
//! control (R4.4 hides controls only for files that cannot be read), and guessing
//! which block the user meant would be worse than saying so.
//!
//! # What this module does
//!
//! The hyprlang parser reports such shapes as
//! [`Ambiguity`] values ([`HyprlangFile::ambiguities`]). This module is the one place
//! that turns that report into what the user and the journal see:
//!
//! - [`log_ambiguities`] writes each report to the journal at `warn`, once per load of
//!   the file — the caller decides when a load happens;
//! - [`ambiguity_notes`] phrases the reports that touch a category the page edits as
//!   informational notes ([`AmbiguityNote`]) to show next to the affected controls,
//!   stating which block the app edits and which one the daemon applies.
//!
//! Notes never block a control or an Apply: they are plain text a page renders. Both
//! the log line and the note carry the file name, category or key names and line
//! numbers only — never a value from the file (R7.3).
//!
//! # What is deliberately not turned into a note
//!
//! - A **doubled repeatable field** ([`Ambiguity::DoubledRepeatableField`], e.g.
//!   `env = XCURSOR_THEME,…` written twice) is still logged, but no page edits a
//!   repeatable hyprlang key since the cursor write moved to `hyprland.lua` (task
//!   11.5), so it has no control to sit next to.
//! - A repeat in a category no page edits (a second `general { }` in
//!   `hyprlock.conf`) is logged but not shown: no value the user can change is
//!   shadowed by it.
//! - Repeated **anonymous** blocks (hypridle's `listener { }`, hyprlock's
//!   `background { }`) are never reported by the parser at all — each block is its
//!   own object, which is why the Power & Idle page addresses listeners by position.

use crate::core::model::SettingId;
use crate::parsers::hyprlang::{Ambiguity, HyprlangFile, HyprlangSchema};

/// One informational note about an ambiguous category in a hyprlang file, for the page
/// that edits values in that category (task 9.31).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmbiguityNote {
    /// The top-level category the ambiguity lies in (e.g. `general`), so a page can
    /// place the note next to the controls that edit values in it.
    pub category: String,
    /// The full sentence(s) to show, naming the file, the category, the line numbers,
    /// which block the app edits and which one the daemon applies.
    pub text: String,
}

/// A note attached to the declarative rows that edit the given settings — the form
/// the Power & Idle page's framework rows take an [`AmbiguityNote`] in (task 9.31).
///
/// The page shows the note once, next to the first of `settings` it displays, so two
/// rows backed by the same ambiguous category (the lock command and its grace period
/// both live in `general.lock_cmd`) do not repeat it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingNote {
    /// The settings whose values lie in the ambiguous category, in page order.
    pub settings: Vec<SettingId>,
    /// The note text (see [`AmbiguityNote::text`]).
    pub text: String,
}

/// Parses `text` as hyprlang and returns its ambiguity report under `schema`, logging
/// each entry at `warn` under the label `file` — the one call a loader makes per load
/// of the file (task 9.31).
///
/// The parse itself already logs any malformed line; this adds only the ambiguity
/// report. `file` is a short static name (`hypridle.conf`), never a path or contents.
pub fn read_ambiguities(file: &'static str, text: &str, schema: &HyprlangSchema) -> Vec<Ambiguity> {
    let (parsed, _warnings) = HyprlangFile::parse(text);
    let ambiguities = parsed.ambiguities(schema);
    log_ambiguities(file, &ambiguities);
    ambiguities
}

/// Logs each ambiguity at `warn` (task 9.31; R7.3).
///
/// Called once per load of a file, so a reload or refresh logs the current state again
/// but rendering a page does not. The entries carry names and line numbers only.
///
/// A schema that lists [`HyprlangSchema::repeatable_keys`] would have a doubled
/// repeatable field ([`Ambiguity::DoubledRepeatableField`]) logged twice: here, and by
/// the parser's own `warn` each time the repeatable-key accessors read or write that
/// address. No production schema lists a repeatable key today, so in practice only this
/// log occurs.
pub fn log_ambiguities(file: &'static str, ambiguities: &[Ambiguity]) {
    for ambiguity in ambiguities {
        tracing::warn!(
            file,
            %ambiguity,
            "ambiguous hyprlang file: a category or repeatable line is declared more than \
             once, and the daemon applies the later assignment while the app addresses the \
             first (task 9.31)"
        );
    }
}

/// Phrases the reports in `ambiguities` that touch a category in `edited_categories` as
/// notes for the page (task 9.31).
///
/// `file` is the file's name as the user knows it (`hypridle.conf`) and `daemon` the
/// program that reads it (`hypridle`). `edited_categories` are the top-level categories
/// the page reads and writes values in; a report outside them is left to the journal
/// (see the module docs). Every page addresses those categories at their first block,
/// which is what the note states.
pub fn ambiguity_notes(
    file: &str,
    daemon: &str,
    ambiguities: &[Ambiguity],
    edited_categories: &[&str],
) -> Vec<AmbiguityNote> {
    ambiguities
        .iter()
        .filter_map(|ambiguity| match ambiguity {
            Ambiguity::RepeatedCategory { .. } => {
                let category = ambiguity.top_level_category()?;
                edited_categories
                    .contains(&category)
                    .then(|| AmbiguityNote {
                        category: category.to_string(),
                        text: format!(
                            "In {file}, {ambiguity}. {daemon} merges these blocks, so a \
                             value set again in a later block overrides the first. The app \
                             shows and edits only the first block: if a later block also \
                             sets a value shown here, a change made here has no effect."
                        ),
                    })
            }
            // No page edits a repeatable hyprlang key (see the module docs); the
            // report reached the journal through `log_ambiguities`.
            Ambiguity::DoubledRepeatableField { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::hyprlang::SpecialCategory;
    use crate::testing::capture_logs;

    /// hypridle's schema shape: `listener` is an anonymous special category.
    const SCHEMA: HyprlangSchema = HyprlangSchema {
        special_categories: &[SpecialCategory::Anonymous("listener")],
        repeatable_keys: &["env"],
    };

    /// A file with every kind of report: `general` twice (edited), `misc` twice (not
    /// edited), a doubled `env` field, and two listeners (never reported).
    const MIXED: &str = "\
general {
    lock_cmd = hyprlock
}
misc {
}
listener {
}
listener {
}
general {
    lock_cmd = swaylock
}
misc {
}
env = A,1
env = A,2
";

    #[test]
    fn notes_cover_only_the_edited_categories() {
        let (file, _) = HyprlangFile::parse(MIXED);
        let notes = ambiguity_notes(
            "hypridle.conf",
            "hypridle",
            &file.ambiguities(&SCHEMA),
            &["general", "listener"],
        );
        assert_eq!(
            notes,
            vec![AmbiguityNote {
                category: "general".to_string(),
                text: "In hypridle.conf, `general` is declared twice (lines 1 and 10). \
                       hypridle merges these blocks, so a value set again in a later \
                       block overrides the first. The app shows and edits only the first \
                       block: if a later block also sets a value shown here, a change made \
                       here has no effect."
                    .to_string(),
            }]
        );
    }

    #[test]
    fn every_report_is_logged_once_without_values() {
        // The three reports (general, misc, env) are logged at warn; none carries a
        // value from the file (R7.3) — `hyprlock`/`swaylock` never appear.
        let events = capture_logs(|| {
            let _ = read_ambiguities("hypridle.conf", MIXED, &SCHEMA);
        });
        let warnings: Vec<_> = events
            .iter()
            .filter(|event| event.level == tracing::Level::WARN)
            .collect();
        assert_eq!(warnings.len(), 3, "{events:?}");
        assert!(warnings.iter().all(|event| {
            event.fields.contains("hypridle.conf")
                && !event.fields.contains("hyprlock")
                && !event.fields.contains("swaylock")
        }));
    }

    #[test]
    fn a_clean_file_yields_no_report_and_no_log() {
        let clean = "general {\n    lock_cmd = hyprlock\n}\nlistener {\n}\nlistener {\n}\n";
        let mut report = Vec::new();
        let events = capture_logs(|| report = read_ambiguities("hypridle.conf", clean, &SCHEMA));
        assert!(report.is_empty());
        assert!(events.is_empty(), "{events:?}");
    }
}
