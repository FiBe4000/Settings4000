# Test fixtures

## `dotfiles/` — anonymized dotfiles repo (task 7.1, R6.1)

An anonymized copy of the real `~/.dotfiles` layout the app targets (see
`docs/dotfiles_analysis.md`: §6 for the original post-prep state, §7–§8 for the
`wayland-dev` layout). Integration suites never read it in place:
`settings4000::testing::FixtureDotfiles::install()` copies it into a fresh
`tempfile` directory per test and creates the deployment symlinks — one per
file under a fake `$XDG_CONFIG_HOME`, plus the two home-level links listed
below — exactly as the real `setup.sh` deployment does.

### Source

The tree was first built in task 7.1 from the §6 layout and refreshed in task
11.1 from the dotfiles repo at commit **`8a5b20b`** (the head of branch
`wayland-dev`, 2026-10-20). Files were taken from that commit with
`git show 8a5b20b:<path>`, never from a working tree. Task 11.1 was
additions-only; the files whose `8a5b20b` content broke existing tests were
refreshed later by the tasks that retargeted those tests (11.5, 11.6, 11.10,
11.13, listed below). Since task 11.13 no fixture file is held in an older
shape for the sake of a test. The files that still differ from `8a5b20b` are:

- anonymized copies (see "Anonymization rules"): `config/hypr/hyprland.lua`,
  `config/hypr/monitors.conf`, `config/uwsm/env`;
- stand-ins and stubs (see "What it contains"): `colors/README.md`,
  `scripts/generate-colors`, `scripts/hypr-toggle-laptop-display`,
  `scripts/notifctl`, `config/eww/scripts/bar-text-tone`;
- two task-7.1 copies that were never refreshed because the app reads nothing
  that changed in them: `config/kitty/kitty.conf` (lacks the later
  `remember_window_size no` block; the app only sends kitty a signal) and
  `theme/fonts` (shorter comments; identical keys and values).

Every other file is the `8a5b20b` content byte for byte.

### What it contains

- `colors/` — the two palette schemes (`everforest`, `nord`, verbatim
  `8a5b20b`: **18 keys**, `shadow` last) plus a short stand-in `README.md`,
  which the scheme scan must skip.
- `scripts/generate-colors` — an executable stub that exits 2 on its first
  command, so an accidental real invocation fails loudly (tests drive the
  generator through `MockCommandRunner`; detection only checks presence).
  Below the `exit 2` it carries a verbatim, never-executed copy of the real
  script at `8a5b20b`, so the file holds the real `NAMES=(…)` schema line
  (18 keys, `shadow` last) and the `colors.lua` emitter as text for the app
  to read: since task 11.6 the palette schema is parsed from that line,
  matched only at the start of a line because the stub's own header comment
  mentions `NAMES` too.
- `scripts/hypr-toggle-laptop-display`, `scripts/notifctl` — executable
  no-op stubs. Each writes one line (script name plus shell-quoted arguments)
  to stderr, appends the same line to the file named by
  `SETTINGS4000_FIXTURE_STUB_LOG` when that variable is set, and exits 0. The
  real scripts toggle the laptop panel and drive the notifd daemon over D-Bus;
  the app runs the toggle through `CommandRunner` (task 11.8), so tests
  normally never execute either stub.
- `state/active-scheme`, `theme/fonts` — the repo-level palette/font sources.
- `icons/default/index.theme` — the XCursor fallback theme (`[Icon Theme]`,
  `Inherits=breeze_cursors`), deployed as `~/.icons/default/index.theme`
  (analysis §8.4; a write target of the cursor setting since task 11.5).
- `config/` — every file the app parses or writes plus the generated
  color/font partials with their `# Generated from …` headers, and the
  non-target files deployment also symlinks (`kitty.conf`, …) so the tree is
  deployment-faithful. New in task 11.1:
  - `hypr/hyprland.lua` — the real Lua Hyprland config (485 lines), anonymized
    only in its laptop-panel `desc:` key. It is the realistic fixture for the
    Lua-literal editor (task 11.3): nested tables, the two cursor `hl.env`
    lines the cursor write edits (task 11.5), and plenty of code the editor
    must treat as opaque.
  - `hypr/input.lua` — the app-owned, schema-flat input table (verbatim);
    the Input page's backing file since task 11.4, which also removed the
    hyprlang input file it replaced.
  - `hypr/colors.lua` — the generated Lua palette partial (verbatim, header
    prefix `-- `, includes `shadow`); a read-only generated input.
  - `notifd/config` — the notifd daemon's seven-key `key=value` file with its
    comments (verbatim).

  New in task 11.7:
  - `systemd/user/notifd.service` — the notifd user unit (verbatim
    `8a5b20b`; its only path is `%h/.dotfiles/scripts/notifd`, which systemd
    expands and the fixture never runs). Deployed like every `config/` file, as
    `~/.config/systemd/user/notifd.service`; detection counts its presence as
    "notifd installed, will activate" when the daemon's bus name has no owner.

  New in task 11.14:
  - `eww/scripts/bar-text-tone` — an executable no-op stub of the same shape
    as `scripts/notifctl` (records its argv to stderr and to
    `SETTINGS4000_FIXTURE_STUB_LOG`, exits 0). The real script (Python, at
    `8a5b20b`) re-picks the eww bar's text colours from the wallpaper and,
    with `--apply`, pushes them with `eww update`. Deployed as
    `~/.config/eww/scripts/bar-text-tone`; detection finds it there and the
    wallpaper reload runs `<that path> --apply` through `CommandRunner`, so
    tests never execute it unless they mean to.
- `zsh/colors.zsh` — a generated color partial (deployed as `~/.zsh_colors`).

The installer creates two links outside `$XDG_CONFIG_HOME`: `~/.zsh_colors`
and `~/.icons/default/index.theme` (`HOME_LINKS` in `src/testing.rs`); the
parent directories are plain, as `setup.sh` creates them.

It also builds a stand-in for `/usr/share/themes` outside the fake home,
`<temp>/usr/share/themes` (`FixtureDotfiles::system_themes_root`; task 11.16),
which is generated in code rather than copied from this snapshot because it is
not part of the dotfiles: `Everforest-Green-Dark` (the theme the fixture
`settings.ini` names) and `Fixture-Gtk4`, each with `gtk-3.0/` and a
`gtk-4.0/{gtk.css,gtk-dark.css,assets/}`, and `Fixture-Gtk3-Only` with only
`gtk-3.0/`. Like `setup.sh`, the installer then links
`~/.config/gtk-4.0/{gtk.css,gtk-dark.css,assets}` to the entries of
`Everforest-Green-Dark/gtk-4.0/` (absolute link targets).

### Refreshed to `8a5b20b` in task 11.1

- `config/hypr/monitors.conf` — the current file: frozen-grammar header,
  `bitdepth,10` on the eDP record, the laptop-B rule at scale `1`, and the
  four external rules active with pinned `@60`/`@120` modes (anonymized, see
  below).
- `config/hypr/hyprlock.conf` — verbatim `8a5b20b` (grace-period comment,
  `dots_rounding`, lock-indicator colours, greeting and keyboard-layout
  labels, `background.path = ~/Pictures/wallpaper/18.jpg`); its `path` line
  was completed in task 11.10, see below.
- `config/hypr/hyprpaper.conf` — verbatim `8a5b20b`; see task 11.10 below.
- `scripts/generate-colors` — see above.

### Refreshed to `8a5b20b` in task 11.5

- `config/gtk-3.0/settings.ini`, `config/gtk-4.0/settings.ini` — verbatim:
  `breeze_cursors`/`24`, no cursor comment lines, no `gtk-xft-dpi`, and no
  `gtk-modules` in the GTK 3 file.
- `config/uwsm/env` — `breeze_cursors`/`24` and the cursor comment that names
  the two `hl.env` lines in `hyprland.lua`; the Electron comment block keeps
  the shortened wording introduced in task 7.1 (it omits the real comment's
  application name and display scale factor, see "Anonymization rules").

Since this refresh every cursor copy in the fixture agrees on
`breeze_cursors`/`24`: both `settings.ini`, `uwsm/env`, the two `hl.env`
lines of `hyprland.lua`, and `icons/default/index.theme`.

### Refreshed to `8a5b20b` in task 11.6

- The eight palette files are verbatim `8a5b20b`: `colors/everforest`,
  `colors/nord` (they gained `shadow`) and the six generated color partials
  `config/hypr/colors.conf`, `config/hypr/colors.lua`,
  `config/eww/_colors.scss`, `config/rofi/colors.rasi`,
  `config/kitty/colors.conf`, `zsh/colors.zsh` (generated there from
  `everforest`, the fixture's active scheme; three of them gained their
  `shadow` line, the rest were already current). `colors/README.md` stays a
  stand-in; its key list now names all 18 keys.

### Refreshed to `8a5b20b` in task 11.10

- `config/hypr/hyprlock.conf`, `config/hypr/hyprpaper.conf` — now verbatim:
  both store the wallpaper home-relative, `path = ~/Pictures/wallpaper/18.jpg`.
  The app expands a leading `~/` against the home directory for validation and
  the file chooser and writes a chosen image back in each file's own form, so
  the real value resolves inside each test's temp home: the installer creates a
  stub image at `<home>/Pictures/wallpaper/18.jpg`
  (`FixtureDotfiles::wallpaper_path`). The install-time `/home/user`
  placeholder substitution these two files used to need is gone; the installer
  now copies the tree byte for byte.

### Refreshed to `8a5b20b` in task 11.13

- `config/hypr/hypridle.conf` — verbatim: `lock_cmd = pidof hyprlock ||
  hyprlock --grace 10` with the comment block above it explaining the grace
  rule, the dual-form dpms commands (`hyprctl dispatch dpms …` with the Lua
  `hl.dsp.dpms` fallback), and the dim listener calling
  `~/.dotfiles/scripts/idle-dim dim` / `restore`. Its home paths are
  home-relative (`~/.dotfiles/…`) and name the dotfiles repo, not a user, so
  they needed no anonymization; the fixture omits `scripts/idle-dim` (see
  "Deployed items the fixture omits"), which no test runs. The Power & Idle
  page's lock grace control (task 11.13) reads and edits the `--grace 10`
  token of that line.

The "Known divergences from `8a5b20b`" table that tracked the files still in
their §6 shape is gone with this refresh: it became empty.

### Removed

The live tree no longer has these (analysis §8.3, §8.6), and they left the
fixture together with the code and tests that targeted them: the hyprlang
input file with task 11.4, the hyprlang main config with task 11.5 (its
cursor copy now lives in `hyprland.lua`), and the previous notification
daemon's config directory (its JSON config, stylesheet and generated color
stylesheet) with task 11.7, which replaced that daemon's page with the
runtime-only notifd controls.

### Anonymization rules

The content mirrors the real files' formats byte-for-byte where they carry no
personal data (palette hex values, section structure, comments that document
sync invariants). Everything identifying is replaced:

- **No usernames/home paths.** No fixture file names an absolute path under
  `/home/` (`fixture_tree::no_fixture_file_names_an_absolute_home_path`
  enforces it). Home paths are spelled the way the real dotfiles spell them —
  home-relative (`~/Pictures/wallpaper/18.jpg` in `hyprpaper.conf` and
  `hyprlock.conf`) or built from `$HOME` at run time (`hyprland.lua`) — so they
  resolve inside each test's temp home without rewriting. (Until task 11.10 the
  two wallpaper values were an absolute `/home/user/…` placeholder that the
  installer substituted, because the app could not yet expand `~`.)
- **No hardware serials or locations.** Monitor EDID `desc:` strings and the
  location/model mentions in `monitors.conf` comments are neutral
  placeholders — still awk-parseable in the exact record shape
  `hypr-display-profile.sh` expects, and free of `,`/`#` as the frozen
  grammar requires. The mapping is consistent across files: the laptop-B
  panel is `desc:Example Panel Vendor 0x2036` in both `monitors.conf` and
  `hyprland.lua`'s `LAPTOP_PANEL_DESCS`; the office pair is
  `Example Monitor Corp QHD-24 0x0001`/`0x0002`; the home pair is
  `Example Home Display 27G 0x0003`/`27T 0x0004`. The two laptops are
  "laptop A" (the generic `eDP-1` rule) and "laptop B" (the `desc:` rule).
- **No personal setup details in comments.** A comment that names an
  application the user runs or a machine-specific value is reworded without
  them: the Electron note in `uwsm/env` omits the real comment's example
  application and the laptop's scale factor, and is otherwise unchanged.
- **Long freeform files are trimmed stand-ins** rather than full copies. The
  exception is
  `hyprland.lua`, copied whole because the Lua-literal editor must be tested
  against the real mix of addressable lines and opaque code. It has no
  literal home path to anonymize: it builds paths from `os.getenv("HOME")`.

### Deployed items the fixture omits

The real deployment (analysis §1) symlinks a few items the fixture does not
carry, because the app never reads or writes them and they are effectively
code rather than config: `eww/eww.scss`, `eww/eww.yuck`, the rest of
`eww/scripts` (deployed as a whole-**directory** symlink — a link shape the
installer never creates; it links files only, so the one script the fixture
carries, `bar-text-tone`, is deployed as a file link inside a plain
`~/.config/eww/scripts/`), `rofi/config.rasi`, and the home-level `~/.zshrc`. Since
task 11.1 the same holds for the `wayland-dev` additions the app does not read
yet: `scripts/notifd` itself and the D-Bus activation file
`local/share/dbus-1/services/org.freedesktop.Notifications.service` (the app
talks to the daemon only over D-Bus and never reads either; the unit file, which
detection does read, joined the fixture with task 11.7), the remaining
display/idle scripts
(`hypr-display-profile.sh`, `hypr-monitor-hotplug`, `launch-eww-bars`,
`idle-dim`, `apply-theme`) and `config/environment.d/`. Do not assume they
exist in an installed tree; if a future suite needs one, add it to the fixture
(anonymized) and extend the installer if the directory-link shape matters.

When extending the fixture, never copy a real config verbatim without checking
its contents against these rules.
