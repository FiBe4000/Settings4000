//! Source-level guards for the page-entry deferral rule of the runtime-backed pages
//! (tasks 9.4, 9.26, 11.7; R8.1).
//!
//! # The rule
//!
//! The Sound, Network and Notifications pages read live system state by spawning a
//! command (`pw-dump`/`wpctl`, `nmcli`, `busctl`). None of that may happen while the
//! window *builds* the page: the build runs inside the window's populate pass on the
//! GTK main thread, where a wedged daemon would hold a synchronous command for the
//! full 5 s timeout and delay every category's appearance against the R8.1 budget.
//! Each page therefore starts from a placeholder and does its first read from the
//! window's deferred page-entry hook, which calls the page's `refresh`.
//!
//! So for each page module two things must hold, and this suite checks both:
//!
//! 1. nothing reachable from its `build` function can spawn a command;
//! 2. its `refresh` does reach the read, or the deferral would leave the page empty.
//!
//! # Why a lexical check
//!
//! The behaviour cannot be reached headlessly: building any widget needs an
//! initialised GTK and therefore a display server, which the test suite does not
//! have. Instead this suite reads each module's source (blanked of comments, strings
//! and character literals by the shared [`lexical_guard::strip_comments`], so the
//! rule can be documented without tripping it), splits it into functions, and walks
//! the call graph outwards from an entry point looking for a marker that can spawn
//! something. Since task 11.7 this is the one copy of the walker; the Sound page's
//! earlier in-module copy moved here.
//!
//! # What the walk ignores, and why that is sound
//!
//! **Signal-handler closures.** A closure passed to a GTK `connect_*` method runs
//! when the signal fires — when the user clicks or toggles — not while `build` runs,
//! so the argument list of every `.connect_…(…)` call is dropped before the walk.
//! That is what lets a page connect its buttons in `build` (the Network and
//! Notifications pages do) while the guard stays exact for everything that runs
//! synchronously. The one way around it would be a `build` that emits a signal
//! itself after connecting its handler (e.g. a programmatic `set_active` on a
//! connected switch); the pages render before they connect, and a reviewer adding
//! such a call must keep that order.
//!
//! **Calls into the shared widget helpers** are followed: the pages build their rows
//! with `labelled_row`, `note` and the drop-down builders from `src/ui/widgets.rs`
//! (task 9.19), so that module's functions are appended to every page's function set.
//! A walk confined to the page file would stop at those calls and could never notice
//! a spawn added to a shared helper. Each page's `build_helper` is now one of the
//! shared helpers, so [`the_walk_follows_calls_from_build`] also proves the walk
//! crosses into that module.
//!
//! **Same-named functions** are walked together: functions are matched by name only,
//! so reaching `refresh` reaches every `fn refresh` in the module (e.g. a page's
//! `refresh` and its inner state's). That over-approximates, which for a guard that
//! looks for forbidden calls is the safe direction.
//!
//! What it cannot see: `Shell::populate` itself (GTK-bound), macro-generated code,
//! and calls through function pointers or traits. Calling a spawning API under an
//! alias would hide one marker, which is why each page lists both the runner type and
//! its own read entry points.

mod lexical_guard;

use lexical_guard::strip_comments;

/// Markers that can spawn a process from any page module: the real runner (and the
/// trait behind it, should a runner be passed in) and — should anyone bypass the
/// `CommandRunner` seam, which the architecture forbids — `std::process::Command`.
/// Matched as substrings of a function body.
const COMMON_MARKERS: &[&str] = &[
    "SystemCommandRunner",
    "CommandRunner",
    "process::Command",
    "Command::new",
];

/// One runtime-backed page module under guard.
struct Page {
    /// Its source file, for messages.
    file: &'static str,
    /// Its source text, embedded at compile time.
    source: &'static str,
    /// Its own read entry points, beyond [`COMMON_MARKERS`]: a body naming one of
    /// these reaches the page's live read.
    read_markers: &'static [&'static str],
    /// A helper `build` is known to call, proving the walk follows calls at all.
    build_helper: &'static str,
}

/// The shared widget helpers every page module calls (task 9.19), walked together
/// with each page's own functions (see the module docs).
const SHARED_WIDGETS: &str = include_str!("../src/ui/widgets.rs");

/// The Sound page: the `pw-dump` / `wpctl status` device load (task 9.4).
const SOUND: Page = Page {
    file: "src/ui/sound.rs",
    source: include_str!("../src/ui/sound.rs"),
    read_markers: &["sound::load("],
    build_helper: "note",
};

/// The Network page: the `nmcli` status read (task 9.4).
const NETWORK: Page = Page {
    file: "src/ui/network.rs",
    source: include_str!("../src/ui/network.rs"),
    read_markers: &["network::load("],
    build_helper: "note",
};

/// The Notifications page: the notifd `busctl` read through the model's `refresh`
/// (tasks 9.26, 11.7). `.refresh(` is the model call; the page's own `fn refresh`
/// lines are not calls and do not contain it.
const NOTIFICATIONS: Page = Page {
    file: "src/ui/notifications.rs",
    source: include_str!("../src/ui/notifications.rs"),
    read_markers: &[".refresh("],
    build_helper: "labelled_row",
};

impl Page {
    /// Every marker that counts as reaching a spawn or the page's read.
    fn markers(&self) -> impl Iterator<Item = &'static str> {
        COMMON_MARKERS.iter().chain(self.read_markers).copied()
    }

    /// The production code: the page module followed by [`SHARED_WIDGETS`], each with
    /// comments, strings and character literals blanked and everything from its test
    /// attribute on cut off, so the guard never sees a test module's mock runners.
    fn production_code(&self) -> String {
        let mut code = production_code_of(self.source);
        code.push('\n');
        code.push_str(&production_code_of(SHARED_WIDGETS));
        code
    }
}

/// `source` with comments, strings and character literals blanked and everything from
/// its `#[cfg(test)]` attribute on cut off.
fn production_code_of(source: &str) -> String {
    let code = strip_comments(source);
    match code.find("#[cfg(test)]") {
        Some(end) => code[..end].to_string(),
        None => code,
    }
}

/// Every function defined in `code` as `(name, body)` pairs, with the argument list
/// of every `.connect_…(…)` call blanked out of the body (see the module docs).
///
/// Bounded by indentation rather than brace counting: `rustfmt` — a CI gate — puts an
/// item's closing brace at exactly the indentation of its signature line and indents
/// everything nested deeper, so the first following line consisting of that
/// indentation plus `}` ends the body. Only definitions are collected: an `fn`
/// preceded on its line by anything but a visibility keyword (a call, an `impl Fn`
/// bound, a type) is skipped.
fn functions(code: &str) -> Vec<(&str, String)> {
    let mut functions = Vec::new();
    let mut search = 0;

    while let Some(relative) = code[search..].find("fn ") {
        let keyword = search + relative;
        search = keyword + "fn ".len();

        let line_start = code[..keyword].rfind('\n').map_or(0, |index| index + 1);
        let prefix = &code[line_start..keyword];
        if !matches!(prefix.trim(), "" | "pub" | "pub(crate)" | "pub(super)") {
            continue;
        }
        let indent: String = prefix
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        let name_start = search;
        let Some(name_len) = code[name_start..].find(|c: char| !c.is_alphanumeric() && c != '_')
        else {
            continue;
        };
        let name_end = name_start + name_len;
        let Some(body_start) = code[name_end..].find('{').map(|index| name_end + index + 1) else {
            continue;
        };
        let terminator = format!("\n{indent}}}");
        let Some(body_end) = code[body_start..]
            .find(&terminator)
            .map(|index| body_start + index)
        else {
            continue;
        };

        functions.push((
            &code[name_start..name_end],
            without_signal_handlers(&code[body_start..body_end]),
        ));
    }

    functions
}

/// `body` with the parenthesised argument list of every `.connect_…(…)` call removed.
/// Strings and comments are already blank, so counting parentheses is reliable.
fn without_signal_handlers(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find(".connect_") {
        let Some(open) = rest[at..].find('(').map(|index| at + index) else {
            break;
        };
        let mut depth = 0usize;
        let mut close = None;
        for (index, c) in rest[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        out.push_str(&rest[..=open]);
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

/// Whether `body` mentions `name` as a whole word (a call or reference), so `build`
/// does not match `build_dropdown`.
fn mentions(body: &str, name: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    body.match_indices(name).any(|(index, _)| {
        let before = body[..index].chars().next_back();
        let after = body[index + name.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// The names `entry` can run synchronously, transitively, including itself.
fn reachable_from<'a>(entry: &str, functions: &[(&'a str, String)], file: &str) -> Vec<&'a str> {
    let Some(start) = functions.iter().find(|(name, _)| *name == entry) else {
        panic!(
            "the guard could not find `fn {entry}` in {file} — a rename or a broken \
             extraction would make these guards vacuous"
        );
    };
    let mut reached: Vec<&'a str> = Vec::new();
    let mut pending = vec![start.0];
    while let Some(name) = pending.pop() {
        if reached.contains(&name) {
            continue;
        }
        reached.push(name);
        for (_, body) in functions.iter().filter(|(candidate, _)| *candidate == name) {
            for (candidate, _) in functions {
                if !reached.contains(candidate) && mentions(body, candidate) {
                    pending.push(candidate);
                }
            }
        }
    }
    reached
}

/// The first `(function, marker)` showing that `entry` reaches a spawn or the page's
/// read, or `None` when nothing reachable from it does.
fn marker_reachable_from(page: &Page, entry: &str) -> Option<(String, &'static str)> {
    let code = page.production_code();
    let functions = functions(&code);
    let reached = reachable_from(entry, &functions, page.file);
    functions
        .iter()
        .filter(|(name, _)| reached.contains(name))
        .find_map(|(name, body)| {
            page.markers()
                .find(|marker| body.contains(marker))
                .map(|marker| ((*name).to_string(), marker))
        })
}

/// Asserts the two halves of the rule for `page`.
fn assert_deferred(page: &Page) {
    if let Some((function, marker)) = marker_reachable_from(page, "build") {
        panic!(
            "{}: `build` can spawn a command — `{function}` reaches `{marker}`. The \
             first read is deferred to page entry so nothing runs on the populate path \
             (tasks 9.4/9.26, R8.1)",
            page.file
        );
    }
    assert!(
        marker_reachable_from(page, "refresh").is_some(),
        "{}: `refresh` must reach the page's live read — it performs both the deferred \
         first read and every later page entry",
        page.file
    );
}

#[test]
fn the_sound_page_defers_its_device_load() {
    assert_deferred(&SOUND);
}

#[test]
fn the_network_page_defers_its_status_read() {
    assert_deferred(&NETWORK);
}

#[test]
fn the_notifications_page_defers_its_daemon_read() {
    assert_deferred(&NOTIFICATIONS);
}

#[test]
fn the_notifications_refresh_reaches_the_model_refresh() {
    // The specific path the window relies on: `NotificationsPage::refresh` →
    // `Inner::refresh` → `NotificationsModel::refresh` with the real runner.
    let code = NOTIFICATIONS.production_code();
    let functions = functions(&code);
    let reached = reachable_from("refresh", &functions, NOTIFICATIONS.file);
    assert!(
        functions
            .iter()
            .filter(|(name, _)| reached.contains(name))
            .any(|(_, body)| body.contains(".refresh(&SystemCommandRunner::new())")),
        "the page's refresh must call the model's refresh with the real runner"
    );
}

#[test]
fn the_walk_follows_calls_from_build() {
    // Without this, a walk that only ever returned its entry point would make the
    // `build` halves above pass whatever a helper does.
    for page in [&SOUND, &NETWORK, &NOTIFICATIONS] {
        let code = page.production_code();
        let reached = reachable_from("build", &functions(&code), page.file);
        assert!(
            reached.contains(&page.build_helper),
            "{}: expected the walk from `build` to reach `{}`; reached {reached:?}",
            page.file,
            page.build_helper
        );
    }
}

#[test]
fn a_handler_closure_is_not_walked_but_a_direct_call_is() {
    // The one rule the walk relaxes, pinned on a synthetic module: a spawn inside a
    // `connect_clicked` closure does not count, the same spawn called directly does.
    let code = strip_comments(
        "fn build() {\n    button.connect_clicked(move |_| {\n        spawn();\n    });\n}\n\
         fn eager() {\n    spawn();\n}\n\
         fn spawn() {\n    SystemCommandRunner::new();\n}\n",
    );
    let page = Page {
        file: "<synthetic>",
        source: "",
        read_markers: &[],
        build_helper: "",
    };
    let functions = functions(&code);
    for (entry, expect_reach) in [("build", false), ("eager", true)] {
        let reached = reachable_from(entry, &functions, page.file);
        let spawns = functions
            .iter()
            .filter(|(name, _)| reached.contains(name))
            .any(|(_, body)| page.markers().any(|marker| body.contains(marker)));
        assert_eq!(spawns, expect_reach, "{entry}");
    }
}

#[test]
fn the_markers_are_not_stale() {
    // A rename must break this loudly rather than silently disable a rule.
    for page in [&SOUND, &NETWORK, &NOTIFICATIONS] {
        let code = page.production_code();
        for marker in
            std::iter::once("SystemCommandRunner").chain(page.read_markers.iter().copied())
        {
            assert!(
                code.contains(marker),
                "`{marker}` no longer appears in {}; update the guard's markers",
                page.file
            );
        }
    }
}
