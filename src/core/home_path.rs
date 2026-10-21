//! Home-relative (`~/…`) paths in config values (task 11.10; R8.3, analysis §7.4).
//!
//! # Why this exists
//!
//! The dotfiles store the wallpaper (`hyprpaper.conf` `wallpaper.path`) and the
//! lock-screen background (`hyprlock.conf` `background.path`) as
//! `~/Pictures/wallpaper/18.jpg` — home-relative, so the repo is portable between
//! machines and user names (analysis §7.4). The app has to cope with that form in
//! three places, and this module is the single definition of each rule:
//!
//! - **validation** (R8.3): "the image exists, is readable, is an image" can only be
//!   checked on the expanded path ([`expand_home`]);
//! - **the file chooser**, which opens on the currently configured image and needs a
//!   real filesystem path, not a config literal ([`expand_home`] again);
//! - **writing** a newly chosen image back: a path under the home directory is
//!   written in the same form the file already uses, so a `~/…` file stays `~/…` and
//!   an absolute one stays absolute ([`path_in_form_of`]).
//!
//! # Which form is expanded, and why only that one
//!
//! Only a value that is exactly `~` or starts with `~/` is treated as home-relative;
//! `~user/…` (another user's home) and a `~` anywhere but the first character are
//! left alone. That is the one form every consumer of these values agrees on. Their
//! sources (hyprlock 0.9.6, hyprpaper 0.8.4, and `hyprctl` at Hyprland tag v0.56.2) each test
//! only whether the *first character* is `~` and then substitute `$HOME` without
//! looking at what follows:
//!
//! - hyprlock (`absolutePath` in `src/helpers/MiscFunctions.cpp`) joins `$HOME` with
//!   the value minus its first **two** characters. It is therefore correct only for
//!   `~/…`; `~user/x` silently becomes `$HOME/ser/x`, and a bare `~` makes the
//!   substring call throw.
//! - hyprpaper's config loader (`getPath` in `src/config/ConfigManager.cpp`) and
//!   `hyprctl hyprpaper wallpaper` (`getFullPath` in `hyprctl/src/hyprpaper/
//!   Hyprpaper.cpp`) build `$HOME + "/" + rest`, so `~user/x` becomes `$HOME/user/x`.
//!
//! Treating `~user/…` as home-relative would thus validate a path hyprlock and
//! hyprpaper resolve differently; leaving it literal makes validation fail instead,
//! which is the safe outcome R8.3 asks for.
//!
//! The home directory is always passed in explicitly (from [`home_dir`] in
//! production, from the fixture tree in tests) rather than read here, so the rules
//! are pure functions and tests never have to mutate the process environment.
//!
//! GTK-free like the rest of `core/` (R6.2).

use std::path::{Path, PathBuf};

/// The user's home directory from `$HOME`, or `None` when it is unset or empty.
///
/// An empty value counts as unset because joining onto an empty path would silently
/// produce a path relative to the working directory. Used for the `~` expansion here
/// and by the startup loader for the other home-based paths (`~/.icons`, `~/.themes`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Whether `value` is in the home-relative form this module expands: exactly `~`,
/// or starting with `~/`.
///
/// `~user/…` and a `~` that is not the first character are *not* home-relative (see
/// the module docs for why).
pub fn is_home_relative(value: &str) -> bool {
    value == "~" || value.starts_with("~/")
}

/// Expands a home-relative config value against `home`, returning the filesystem
/// path it denotes.
///
/// `~` becomes `home` and `~/rest` becomes `home/rest`; any other value — an absolute
/// path, `~user/…`, a mid-string `~` — is returned unchanged. When `home` is `None`
/// the value is also returned unchanged, so a home-relative value stays a relative
/// path that names nothing meaningful; callers that validate must check
/// [`is_home_relative`] first and report the unknown home (as
/// [`validate_image_value`](crate::core::model::validate_image_value) does) rather
/// than probe that relative path.
pub fn expand_home(value: &str, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if value == "~" => home.to_path_buf(),
        // Extra slashes after `~/` are dropped before joining: `Path::join` with an
        // absolute argument *replaces* the base, so `~//etc/x` would otherwise expand
        // to `/etc/x` instead of the `$HOME/etc/x` that hyprpaper resolves it to.
        Some(home) => match value.strip_prefix("~/") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(value),
        },
        None => PathBuf::from(value),
    }
}

/// Expands `value` like [`expand_home`] and returns it as a string, falling back to
/// `value` itself when the expanded path is not valid UTF-8.
///
/// For the places that need the expanded form as text — a command argument or a
/// [`Value::String`](crate::core::model::Value) to validate. A non-UTF-8 home
/// directory is the only way to reach the fallback (config values are UTF-8 text).
pub fn expand_home_to_string(value: &str, home: Option<&Path>) -> String {
    expand_home(value, home)
        .into_os_string()
        .into_string()
        .unwrap_or_else(|_| value.to_string())
}

/// Rewrites an absolute `path` that lies strictly below `home` as `~/<rest>`, or
/// returns `None` when it does not lie below `home` (or is `home` itself, or the
/// remainder is not UTF-8).
///
/// The comparison is by path components (so `/home/al` is not "below" `/home/a`, and
/// a trailing slash on `home` does not matter) and purely lexical: a path that
/// reaches the home directory only through a symlink is not rewritten. `home` itself
/// is excluded because the only consumers of the `~` form need `~/…` (hyprlock
/// cannot handle a bare `~`, see the module docs) and a directory is never a valid
/// image anyway.
pub fn contract_home(path: &Path, home: &Path) -> Option<String> {
    let rest = path.strip_prefix(home).ok()?;
    if rest.as_os_str().is_empty() {
        return None;
    }
    Some(format!("~/{}", rest.to_str()?))
}

/// The text to write for a newly chosen `path` into a config value whose current
/// value is `current`, preserving that value's form (task 11.10).
///
/// - When `current` is home-relative ([`is_home_relative`]) and the expanded `path`
///   lies below `home`, the result is `~/<rest>` — a dotfiles repo that stores
///   `~/Pictures/…` keeps doing so, so it stays portable between machines.
/// - Otherwise the result is the expanded, absolute `path`: a file that already uses
///   absolute paths keeps them, and an image outside the home directory can only be
///   named absolutely.
///
/// `path` is usually the absolute path a file chooser returned, but may itself be
/// home-relative (for example the wallpaper's `~/…` value being copied into the
/// lock-screen file); it is expanded first so either input gives the same result.
/// With no known `home` nothing can be expanded or contracted, and `path` is returned
/// unchanged.
///
/// Contraction is lexical ([`contract_home`]): when `$HOME` is reached through a
/// symlink and the chooser returns the canonical path, that path does not start with
/// `home` and is written absolute. That only loses the `~/…` spelling — the value
/// still names the right image.
pub fn path_in_form_of(path: &str, current: Option<&str>, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    let expanded = expand_home(path, Some(home));
    if current.is_some_and(is_home_relative) {
        if let Some(contracted) = contract_home(&expanded, home) {
            return contracted;
        }
    }
    expand_home_to_string(path, Some(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/alice";

    fn home() -> Option<&'static Path> {
        Some(Path::new(HOME))
    }

    #[test]
    fn a_leading_tilde_slash_expands_against_home() {
        assert_eq!(
            expand_home("~/Pictures/wallpaper/18.jpg", home()),
            PathBuf::from("/home/alice/Pictures/wallpaper/18.jpg")
        );
    }

    #[test]
    fn extra_slashes_after_the_tilde_stay_below_home() {
        // `Path::join("/etc/x")` would discard the home directory; the expansion must
        // keep the path below home, as hyprpaper's `$HOME + "/" + rest` does.
        assert_eq!(
            expand_home("~//etc/x.png", home()),
            PathBuf::from("/home/alice/etc/x.png")
        );
        assert_eq!(
            expand_home("~///etc/x.png", home()),
            PathBuf::from("/home/alice/etc/x.png")
        );
    }

    #[test]
    fn a_bare_tilde_expands_to_home_itself() {
        assert_eq!(expand_home("~", home()), PathBuf::from(HOME));
        assert!(is_home_relative("~"));
    }

    #[test]
    fn another_users_home_is_left_untouched() {
        // `~bob/x` is not this user's home; hyprlock and hyprpaper would each resolve
        // it differently (module docs), so it stays a literal and fails validation.
        assert_eq!(
            expand_home("~bob/x.png", home()),
            PathBuf::from("~bob/x.png")
        );
        assert!(!is_home_relative("~bob/x.png"));
    }

    #[test]
    fn a_tilde_that_is_not_the_first_character_is_left_untouched() {
        assert_eq!(
            expand_home("/srv/~/x.png", home()),
            PathBuf::from("/srv/~/x.png")
        );
        assert_eq!(expand_home("a~/x.png", home()), PathBuf::from("a~/x.png"));
        assert!(!is_home_relative("/srv/~/x.png"));
    }

    #[test]
    fn an_absolute_path_is_left_untouched() {
        assert_eq!(
            expand_home("/usr/share/backgrounds/a.png", home()),
            PathBuf::from("/usr/share/backgrounds/a.png")
        );
    }

    #[test]
    fn without_a_home_nothing_is_expanded() {
        assert_eq!(expand_home("~/x.png", None), PathBuf::from("~/x.png"));
        assert_eq!(expand_home("~", None), PathBuf::from("~"));
        assert_eq!(expand_home_to_string("~/x.png", None), "~/x.png");
    }

    #[test]
    fn contracting_requires_a_path_strictly_below_home() {
        assert_eq!(
            contract_home(Path::new("/home/alice/Pictures/a.png"), Path::new(HOME)),
            Some("~/Pictures/a.png".to_string())
        );
        // A trailing slash on the home directory does not matter.
        assert_eq!(
            contract_home(Path::new("/home/alice/a.png"), Path::new("/home/alice/")),
            Some("~/a.png".to_string())
        );
        // Component-wise, not a string prefix: /home/alicia is not below /home/alice.
        assert_eq!(
            contract_home(Path::new("/home/alicia/a.png"), Path::new(HOME)),
            None
        );
        assert_eq!(contract_home(Path::new(HOME), Path::new(HOME)), None);
        assert_eq!(
            contract_home(Path::new("/srv/a.png"), Path::new(HOME)),
            None
        );
    }

    #[test]
    fn a_path_under_home_follows_a_tilde_shaped_current_value() {
        assert_eq!(
            path_in_form_of(
                "/home/alice/Pictures/next.png",
                Some("~/Pictures/wallpaper/18.jpg"),
                home()
            ),
            "~/Pictures/next.png"
        );
    }

    #[test]
    fn a_path_under_home_follows_an_absolute_current_value() {
        assert_eq!(
            path_in_form_of(
                "/home/alice/Pictures/next.png",
                Some("/home/alice/Pictures/wallpaper/18.jpg"),
                home()
            ),
            "/home/alice/Pictures/next.png"
        );
        // A home-relative input is expanded when the file uses absolute paths.
        assert_eq!(
            path_in_form_of(
                "~/Pictures/next.png",
                Some("/home/alice/Pictures/wallpaper/18.jpg"),
                home()
            ),
            "/home/alice/Pictures/next.png"
        );
    }

    #[test]
    fn a_path_outside_home_is_always_absolute() {
        assert_eq!(
            path_in_form_of(
                "/usr/share/backgrounds/a.png",
                Some("~/Pictures/wallpaper/18.jpg"),
                home()
            ),
            "/usr/share/backgrounds/a.png"
        );
    }

    #[test]
    fn no_current_value_or_an_other_user_tilde_means_absolute() {
        assert_eq!(
            path_in_form_of("/home/alice/a.png", None, home()),
            "/home/alice/a.png"
        );
        assert_eq!(
            path_in_form_of("/home/alice/a.png", Some("~bob/a.png"), home()),
            "/home/alice/a.png"
        );
    }

    #[test]
    fn without_a_home_the_chosen_path_is_kept_verbatim() {
        assert_eq!(
            path_in_form_of("/home/alice/a.png", Some("~/b.png"), None),
            "/home/alice/a.png"
        );
        assert_eq!(path_in_form_of("~/a.png", Some("~/b.png"), None), "~/a.png");
    }
}
