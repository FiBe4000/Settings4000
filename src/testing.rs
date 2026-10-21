//! Test support: the fixture-dotfiles installer (task 7.1, R6.1), the shared
//! harness of the end-to-end Apply suites (task 7.2), the small text
//! assertions ([`replace_once`], [`differing_lines`]) that the parser and page
//! unit tests use to state the surgical-edit contract (task 9.13), and a scoped
//! log capture ([`capture_logs`]) for asserting what a hidden feature logs
//! (R4.2, R7.3).
//!
//! Integration suites need a realistic dotfiles environment: a repo tree like
//! `~/.dotfiles` (palette sources, `scripts/generate-colors`, `theme/fonts`,
//! the tracked configs, the generated partials) *deployed* the way the real
//! `setup.sh` deploys it — per-file symlinks from `$XDG_CONFIG_HOME` into the
//! repo (analysis §1). [`FixtureDotfiles::install`] materializes exactly that
//! from the anonymized snapshot in `tests/fixtures/dotfiles/` into a fresh
//! [`tempfile::TempDir`] per test, so every test gets an isolated, writable
//! tree it can stage edits against and apply to, with the symlink-following
//! write path (R5.4/R8.5) and repo-root discovery (R3.2) behaving as they do
//! on the real machine.
//!
//! Tests do not point the app at the tree via environment variables (mutating
//! the process environment is racy under the parallel test harness). Instead
//! they inject the fixture's paths through the seams the code already exposes:
//! [`crate::core::detect::DetectionInputs`] takes the palette anchor and config
//! paths directly, the parsers take file contents or paths, and the Apply
//! pipeline takes explicit write targets. The handle's accessors
//! ([`FixtureDotfiles::config_path`], [`FixtureDotfiles::repo_path`], …) exist
//! so suites never rebuild those paths by hand.
//!
//! # Panics
//!
//! Everything here panics (with a descriptive message) on I/O failure rather
//! than returning `Result`: this module is compiled only for tests (`cfg(test)`
//! or the `testing` feature — never a release build), and a fixture that cannot
//! be built must abort the test loudly. The crate-wide "no panics on fallible
//! runtime paths" rule applies to the shipped application, not to test-only
//! scaffolding.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::core::apply::{ApplyOutcome, ApplyPlan};
use crate::core::reload::ReloadError;
use crate::core::store::{FileValues, SettingsStore};

/// The placeholder home-directory prefix used inside fixture files.
///
/// The real dotfiles contain absolute paths under the user's home (the
/// wallpaper/lock-background paths in `hyprpaper.conf`/`hyprlock.conf`); the
/// checked-in fixture anonymizes them to this prefix, and the installer
/// rewrites it to the temp tree's home at install time so the installed paths
/// point at real, readable files (the R8.3 image validator checks existence).
///
/// Public so the smoke suite (`tests/fixture_tree.rs`) can assert that no
/// installed file still contains it — the guard that catches a future fixture
/// file whose placeholder lands somewhere no parser test happens to read.
pub const HOME_PLACEHOLDER: &str = "/home/user";

/// The wallpaper path, relative to the fixture home, that the installed
/// `hyprpaper.conf`/`hyprlock.conf` reference after placeholder substitution.
///
/// The installer creates a small stub file here so the configured wallpaper
/// exists and is readable (decodability is not a concern: the R8.3 validator
/// checks existence, readability, and extension only).
const WALLPAPER_RELATIVE: &str = "Pictures/wallpaper/wallpaper.jpg";

/// The deployment links that live under `$HOME` itself rather than under
/// `$XDG_CONFIG_HOME`, as `(link relative to home, target relative to the repo)`.
///
/// Everything under the repo's `config/` is linked per file by
/// [`deploy_config_symlinks`]; these are the only other links the real
/// `setup.sh` creates that the app reads or writes (analysis §1, §8.4):
/// the generated zsh color partial, and the XCursor fallback theme that is
/// part of the R3.4 cursor write set. The link's parent directories are
/// created plain, as `setup.sh` does (`mkdir -p ~/.icons/default`).
const HOME_LINKS: &[(&str, &str)] = &[
    (".zsh_colors", "zsh/colors.zsh"),
    (".icons/default/index.theme", "icons/default/index.theme"),
];

/// An installed fixture dotfiles tree (task 7.1, R6.1).
///
/// Owns the backing [`TempDir`]: the whole tree is deleted when the handle is
/// dropped, so a test simply keeps it alive for as long as it needs the files.
/// The layout mirrors the real machine (analysis §1/§6):
///
/// ```text
/// <temp>/home/                     — the fake $HOME
/// ├── .dotfiles/                   — the repo (copied from tests/fixtures/dotfiles)
/// │   ├── colors/{everforest,nord,README.md}
/// │   ├── scripts/{generate-colors,hypr-toggle-laptop-display,notifctl}
/// │   ├── state/active-scheme
/// │   ├── theme/fonts
/// │   ├── icons/default/index.theme
/// │   ├── zsh/colors.zsh
/// │   └── config/{hypr,gtk-3.0,gtk-4.0,uwsm,notifd,swaync,kitty,eww,rofi}/…
/// ├── .config/                     — the fake $XDG_CONFIG_HOME; every file under
/// │   │                              the repo's config/ is symlinked here per-file
/// │   └── hypr/colors.conf -> …/.dotfiles/config/hypr/colors.conf   (etc.)
/// ├── .icons/default/index.theme -> …/.dotfiles/icons/default/index.theme
/// ├── .zsh_colors -> .dotfiles/zsh/colors.zsh
/// └── Pictures/wallpaper/wallpaper.jpg   — stub the configs point at
/// ```
#[derive(Debug)]
pub struct FixtureDotfiles {
    /// Keeps the temp directory alive; dropping it removes the whole tree.
    _temp: TempDir,
    /// The fake `$HOME` (canonicalized).
    home: PathBuf,
    /// The dotfiles repo root: `<home>/.dotfiles`.
    repo_root: PathBuf,
    /// The fake `$XDG_CONFIG_HOME`: `<home>/.config`.
    config_root: PathBuf,
}

impl FixtureDotfiles {
    /// Copies the fixture snapshot into a fresh temp directory and deploys it.
    ///
    /// Installation performs, in order:
    ///
    /// 1. copy `tests/fixtures/dotfiles/` to `<home>/.dotfiles`, substituting the
    ///    [`HOME_PLACEHOLDER`] prefix in every file for the temp home and
    ///    preserving each file's permissions (the `scripts/` stubs stay executable);
    /// 2. symlink every file under the repo's `config/` from `<home>/.config`,
    ///    per file, exactly like the real `setup.sh` deployment (analysis §1) —
    ///    so e.g. `<home>/.config/hypr/colors.conf` is a symlink the repo-root
    ///    discovery (R8.5) can canonicalize and the atomic writer follows (R5.4);
    /// 3. create the home-level deployment links in [`HOME_LINKS`]
    ///    (`<home>/.zsh_colors`, `<home>/.icons/default/index.theme`), which
    ///    live outside `$XDG_CONFIG_HOME`;
    /// 4. create the stub wallpaper file the substituted configs point at.
    ///
    /// Each call builds a completely independent tree, so parallel tests never
    /// share state. Panics on any I/O failure (see the module docs).
    pub fn install() -> Self {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("dotfiles");
        assert!(
            fixture_root.is_dir(),
            "fixture snapshot missing at {} — was tests/fixtures/dotfiles moved?",
            fixture_root.display()
        );

        let temp = TempDir::new().unwrap_or_else(|e| panic!("failed to create temp dir: {e}"));
        // Canonicalize once so every path the handle exposes is already fully
        // resolved: tests compare these against `fs::canonicalize` outputs (the
        // repo-root discovery canonicalizes, R8.5), and a symlinked system temp
        // directory must not make those comparisons fail.
        let temp_root = fs::canonicalize(temp.path())
            .unwrap_or_else(|e| panic!("failed to canonicalize temp dir: {e}"));

        let home = temp_root.join("home");
        let repo_root = home.join(".dotfiles");
        let config_root = home.join(".config");

        let home_str = home
            .to_str()
            .unwrap_or_else(|| panic!("temp home path is not UTF-8: {}", home.display()));

        copy_tree_substituting(&fixture_root, &repo_root, home_str);
        deploy_config_symlinks(&repo_root.join("config"), &config_root);

        for (link_relative, target_relative) in HOME_LINKS {
            let link = home.join(link_relative);
            let target = repo_root.join(target_relative);
            create_parent_dirs(&link);
            symlink(&target, &link).unwrap_or_else(|e| {
                panic!(
                    "failed to symlink {} -> {}: {e}",
                    link.display(),
                    target.display()
                )
            });
        }

        // The wallpaper the substituted hyprpaper/hyprlock configs reference.
        let wallpaper = home.join(WALLPAPER_RELATIVE);
        create_parent_dirs(&wallpaper);
        // Not a decodable image — the R8.3 validator checks existence,
        // readability, and extension, not contents.
        fs::write(&wallpaper, b"fixture wallpaper stand-in")
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", wallpaper.display()));

        FixtureDotfiles {
            _temp: temp,
            home,
            repo_root,
            config_root,
        }
    }

    /// The fake `$HOME` of the installed tree (canonicalized).
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The dotfiles repo root (`<home>/.dotfiles`), i.e. what repo-root
    /// discovery (R8.5) resolves from a deployed config symlink.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// The fake `$XDG_CONFIG_HOME` (`<home>/.config`) holding the deployment
    /// symlinks — the live paths the app addresses files by (R8.5).
    pub fn config_root(&self) -> &Path {
        &self.config_root
    }

    /// A path under the deployed config root, e.g.
    /// `config_path("hypr/input.lua")` — the live XDG path of a backing file.
    pub fn config_path(&self, relative: &str) -> PathBuf {
        self.config_root.join(relative)
    }

    /// A path under the repo root, e.g. `repo_path("colors/everforest")` — for
    /// the repo-only sources that have no XDG location (R8.5).
    pub fn repo_path(&self, relative: &str) -> PathBuf {
        self.repo_root.join(relative)
    }

    /// The stub wallpaper file the installed `hyprpaper.conf`/`hyprlock.conf`
    /// point at (exists and is readable, so R8.3 path validation passes).
    pub fn wallpaper_path(&self) -> PathBuf {
        self.home.join(WALLPAPER_RELATIVE)
    }
}

/// Recursively copies the fixture tree at `from` to `to`, replacing every
/// occurrence of [`HOME_PLACEHOLDER`] in file contents with `home` and
/// preserving each file's permissions.
///
/// All fixture files are UTF-8 text, so the substitution runs on every file;
/// a file the substitution does not apply to is copied byte-identical.
fn copy_tree_substituting(from: &Path, to: &Path, home: &str) {
    fs::create_dir_all(to).unwrap_or_else(|e| panic!("failed to create {}: {e}", to.display()));

    let entries =
        fs::read_dir(from).unwrap_or_else(|e| panic!("failed to read {}: {e}", from.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", from.display()));
        let source = entry.path();
        let target = to.join(entry.file_name());

        if source.is_dir() {
            copy_tree_substituting(&source, &target, home);
            continue;
        }

        let contents = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("failed to read fixture file {}: {e}", source.display()));
        let substituted = contents.replace(HOME_PLACEHOLDER, home);
        fs::write(&target, substituted)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", target.display()));

        // Substitution went through read/write, which does not carry the mode
        // over — restore it so the `scripts/` stubs stay executable.
        let permissions = fs::metadata(&source)
            .unwrap_or_else(|e| panic!("failed to stat {}: {e}", source.display()))
            .permissions();
        fs::set_permissions(&target, permissions)
            .unwrap_or_else(|e| panic!("failed to set permissions on {}: {e}", target.display()));
    }
}

/// Symlinks every file under the repo's `config/` tree from the fake
/// `$XDG_CONFIG_HOME`, one link per file, mirroring the real per-file
/// deployment (analysis §1). Directories are created plain (never linked), so
/// the app can later create sibling files without touching the repo.
fn deploy_config_symlinks(repo_config: &Path, config_root: &Path) {
    fs::create_dir_all(config_root)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", config_root.display()));

    let entries = fs::read_dir(repo_config)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", repo_config.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", repo_config.display()));
        let source = entry.path();
        let target = config_root.join(entry.file_name());

        if source.is_dir() {
            deploy_config_symlinks(&source, &target);
        } else {
            symlink(&source, &target).unwrap_or_else(|e| {
                panic!(
                    "failed to symlink {} -> {}: {e}",
                    target.display(),
                    source.display()
                )
            });
        }
    }
}

/// Creates all missing parent directories of `path`.
fn create_parent_dirs(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", parent.display()));
    }
}

// ===========================================================================
// End-to-end Apply-suite harness (task 7.2, R5.3–R5.6, R6.1)
// ===========================================================================

/// The app's real startup loaders, re-exposed for the integration suites (task 7.2).
///
/// The end-to-end Apply suites must load the [`SettingsStore`] **exactly as the app
/// does** — the same `SettingId` ↔ file-key mapping, the same re-reader registered
/// for conflict reloads — so instead of re-implementing the mapping (which would
/// drift), these thin wrappers hand the suites the crate-private loader functions
/// from [`crate::ui::startup`]. Each has the plain-`fn` shape a
/// [`FileReader`](crate::core::store::FileReader) needs, so a suite can pass the
/// same function as both the initial load and the store's re-reader, mirroring the
/// startup wiring.
pub mod loaders {
    use std::io;
    use std::path::Path;

    use crate::core::store::FileValues;

    /// The app's `~/.config/hypr/input.lua` loader (Input page, tasks 6.6, 11.4).
    pub fn input_lua(path: &Path) -> io::Result<FileValues> {
        crate::ui::startup::load_input_lua(path)
    }

    /// The app's `~/.config/swaync/config.json` loader (Notifications page, task 6.7).
    pub fn swaync_config(path: &Path) -> io::Result<FileValues> {
        crate::ui::startup::load_swaync_config(path)
    }

    /// The app's `~/.config/hypr/hypridle.conf` loader (Power & Idle page, task 6.8).
    pub fn hypridle_conf(path: &Path) -> io::Result<FileValues> {
        crate::ui::startup::load_hypridle_conf(path)
    }
}

/// Loads one backing file into `store` through the given app loader, registering the
/// same loader as the file's conflict re-reader — the exact wiring the startup path
/// performs per [`LoadedFile`](crate::ui::startup) (task 5.4/7.2).
///
/// Panics on a load failure: the suites always load a fixture file that exists, so a
/// failure is a broken test setup to surface loudly (see the module docs on panics).
pub fn load_into_store(
    store: &mut SettingsStore,
    path: &Path,
    loader: fn(&Path) -> io::Result<FileValues>,
) {
    let initial =
        loader(path).unwrap_or_else(|e| panic!("failed to load fixture {}: {e}", path.display()));
    store.load_file(path, initial, Box::new(loader));
}

/// Builds the base [`ApplyPlan`] from the store's dirty edits, through the app's real
/// plan builder ([`crate::core::assemble::base_apply_plan`], task 5.3/7.2).
///
/// The per-page suites then fold in that page's write by hand — the store-driven
/// `FileWrite` from its model, or a Display/Theme contribution — which keeps each suite
/// focused on one page while still starting from the app's own validations half. The
/// *whole* assembly, over every source at once, is
/// [`crate::core::assemble::assemble_apply_plan`] and has its own suite
/// (`tests/apply_assembly.rs`).
///
/// This wrapper exists only so the suites need one import path for the harness; the
/// underlying function is public API of [`crate::core`] like the rest of the domain layer.
pub fn base_apply_plan(store: &SettingsStore) -> ApplyPlan {
    crate::core::assemble::base_apply_plan(store)
}

/// Unwraps an [`ApplyOutcome::Applied`], returning its reload failures and written
/// paths; panics (with the outcome) on any other variant.
///
/// The suites' happy-path and reload-failure assertions all start from an `Applied`
/// outcome, so this keeps the match noise out of every test.
///
/// It additionally asserts the apply reported **no** unrelated conflicts — no tracked
/// file outside the plan's writes was changed on disk behind the suite's back (task
/// 9.11). Every suite installs its own fixture tree and edits it deliberately, so a
/// stray conflict means the fixture moved under the test and its other assertions can no
/// longer be trusted. A suite that deliberately provokes one matches
/// [`ApplyOutcome::Applied`] itself instead of calling this.
pub fn expect_applied(outcome: ApplyOutcome) -> (Vec<ReloadError>, Vec<PathBuf>) {
    match outcome {
        ApplyOutcome::Applied {
            reload_failures,
            written,
            unrelated_conflicts,
        } => {
            assert!(
                unrelated_conflicts.is_empty(),
                "the apply reported conflicts in tracked files it did not write, so the \
                 fixture changed underneath the test: {unrelated_conflicts:?}"
            );
            (reload_failures, written)
        }
        other => panic!("expected ApplyOutcome::Applied, got {other:?}"),
    }
}

/// Snapshots every file in the installed repo tree: repo-relative path → exact bytes.
///
/// Taken before an Apply and diffed after it (see [`assert_repo_untouched_except`]),
/// this is what lets a suite assert the surgical-write contract tree-wide: *only* the
/// planned files changed, and every other file — including the generated color/font
/// partials a rolled-back palette apply must never touch — is byte-identical (R5.3).
pub fn repo_snapshot(fx: &FixtureDotfiles) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let root = fx.repo_root();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or_else(|_| panic!("{} is outside the repo root", path.display()))
                    .to_str()
                    .unwrap_or_else(|| panic!("non-UTF-8 fixture path {}", path.display()))
                    .to_string();
                let bytes = fs::read(&path)
                    .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
                snapshot.insert(relative, bytes);
            }
        }
    }
    snapshot
}

/// Asserts that, relative to the `before` snapshot, exactly the files named in
/// `expected_changed` (repo-relative paths) changed and every other repo file is
/// byte-identical — the tree-wide surgical-write assertion (task 7.2, R5.3/R5.4).
///
/// Files named in `expected_changed` are additionally asserted to have *actually*
/// changed, so a test that expected a write cannot pass vacuously on a no-op apply.
/// The file **set** must be unchanged too: an Apply never creates or deletes repo
/// files.
pub fn assert_repo_untouched_except(
    fx: &FixtureDotfiles,
    before: &BTreeMap<String, Vec<u8>>,
    expected_changed: &[&str],
) {
    let after = repo_snapshot(fx);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "an Apply must never create or delete repo files"
    );
    for (path, old_bytes) in before {
        let new_bytes = &after[path];
        if expected_changed.contains(&path.as_str()) {
            assert_ne!(
                old_bytes, new_bytes,
                "{path} was expected to change but is byte-identical"
            );
        } else {
            assert_eq!(
                old_bytes, new_bytes,
                "{path} changed but was not a planned write target"
            );
        }
    }
}

/// Replaces exactly one occurrence of `from` in `text` with `to`, panicking unless
/// `from` occurs exactly once.
///
/// The suites build their *expected* post-apply bytes by patching the original
/// fixture text — the surgical-edit contract says everything but the target span is
/// byte-identical, so the expectation *is* the original with one span replaced. The
/// exactly-once check keeps an ambiguous pattern (which could silently build a wrong
/// expectation) from passing.
pub fn replace_once(text: &str, from: &str, to: &str) -> String {
    let occurrences = text.matches(from).count();
    assert_eq!(
        occurrences, 1,
        "pattern {from:?} must occur exactly once in the fixture text, found {occurrences}"
    );
    text.replacen(from, to, 1)
}

/// Splits both texts into lines and returns the indices at which they differ,
/// asserting first that the edit neither added nor removed a line.
///
/// This is the standard way the parser and page-model unit tests state the
/// surgical-edit contract (architecture §3): an edit rewrites the targeted value
/// span and leaves every other byte alone, so the expected result of changing one
/// setting is *exactly one* differing line index. Asserting the line count first
/// makes the common failure — a writer that regenerated the file, reordered it, or
/// dropped a comment — fail with a clear message instead of an opaque index list.
///
/// Line endings are consumed by [`str::lines`], so a text whose final newline was
/// added or removed compares equal here; the round-trip tests
/// (`parse → emit == input`) are what pin the terminators.
///
/// # Panics
///
/// Panics if the two texts have different line counts (see above). Test-only code,
/// so panicking is the intended way to report the failure.
#[track_caller]
pub fn differing_lines(before: &str, after: &str) -> Vec<usize> {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    assert_eq!(
        before_lines.len(),
        after_lines.len(),
        "a surgical edit must not add or remove lines"
    );
    before_lines
        .iter()
        .zip(&after_lines)
        .enumerate()
        .filter_map(|(index, (b, a))| (b != a).then_some(index))
        .collect()
}

// ===========================================================================
// Log capture
// ===========================================================================

/// One log event captured by [`capture_logs`].
///
/// The message and the fields are kept apart so a test asserting on the message can
/// never be satisfied by a field value that happens to contain the same words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedEvent {
    /// The event's level (`info`, `warn`, …).
    pub level: tracing::Level,
    /// The event's message, the text after its fields in the `tracing` macro.
    pub message: String,
    /// The event's other fields, rendered as `name=value ` pairs in order.
    pub fields: String,
}

/// Runs `action` under a scoped `tracing` subscriber and returns the events it
/// logged, for tests that pin what a hidden or degraded feature logs (R4.2, R7.3).
///
/// The subscriber is installed only for the current thread (no global state), but
/// `tracing` keeps a single process-wide cache of each callsite's *interest*: a
/// parallel test that hits the same `info!` callsite first, under the no-op default
/// subscriber, can cache it as "never enabled", which would suppress the event here.
/// So `action` runs **twice**: once to register every callsite it reaches, then —
/// after the interest cache is rebuilt against this subscriber and the first run's
/// events are discarded — once more for the events that are returned. `action` must
/// therefore be safe to repeat.
///
/// # Panics
///
/// Panics if the capture's mutex is poisoned, which only happens when `action`
/// itself panicked — already a test failure.
pub fn capture_logs(mut action: impl FnMut()) -> Vec<CapturedEvent> {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Capture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    struct Visitor<'a> {
        message: &'a mut String,
        fields: &'a mut String,
    }

    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            if field.name() == "message" {
                let _ = write!(self.message, "{value:?}");
            } else {
                let _ = write!(self.fields, "{}={value:?} ", field.name());
            }
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut message = String::new();
            let mut fields = String::new();
            event.record(&mut Visitor {
                message: &mut message,
                fields: &mut fields,
            });
            self.events
                .lock()
                .expect("log capture mutex should not be poisoned")
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    message,
                    fields,
                });
        }
    }

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    tracing::subscriber::with_default(subscriber, || {
        action();
        tracing::callsite::rebuild_interest_cache();
        capture
            .events
            .lock()
            .expect("log capture mutex should not be poisoned")
            .clear();
        action();
    });
    let events = capture
        .events
        .lock()
        .expect("log capture mutex should not be poisoned");
    events.clone()
}
