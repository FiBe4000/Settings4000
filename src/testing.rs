//! Test support: the fixture-dotfiles installer (task 7.1, R6.1).
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

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

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
/// │   ├── scripts/generate-colors
/// │   ├── state/active-scheme
/// │   ├── theme/fonts
/// │   ├── zsh/colors.zsh
/// │   └── config/{hypr,gtk-3.0,gtk-4.0,uwsm,swaync,kitty,eww,rofi}/…
/// ├── .config/                     — the fake $XDG_CONFIG_HOME; every file under
/// │   │                              the repo's config/ is symlinked here per-file
/// │   └── hypr/colors.conf -> …/.dotfiles/config/hypr/colors.conf   (etc.)
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
    ///    preserving each file's permissions (`generate-colors` stays executable);
    /// 2. symlink every file under the repo's `config/` from `<home>/.config`,
    ///    per file, exactly like the real `setup.sh` deployment (analysis §1) —
    ///    so e.g. `<home>/.config/hypr/colors.conf` is a symlink the repo-root
    ///    discovery (R8.5) can canonicalize and the atomic writer follows (R5.4);
    /// 3. symlink `<home>/.zsh_colors` to the repo's `zsh/colors.zsh` (the one
    ///    deployed link that lives at the home level rather than under XDG);
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

        // The single home-level deployment link: `~/.zsh_colors` → the generated
        // zsh color partial (analysis §1).
        let zsh_link = home.join(".zsh_colors");
        let zsh_target = repo_root.join("zsh").join("colors.zsh");
        symlink(&zsh_target, &zsh_link).unwrap_or_else(|e| {
            panic!(
                "failed to symlink {} -> {}: {e}",
                zsh_link.display(),
                zsh_target.display()
            )
        });

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
    /// `config_path("hypr/input.conf")` — the live XDG path of a backing file.
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
        // over — restore it so `scripts/generate-colors` stays executable.
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
