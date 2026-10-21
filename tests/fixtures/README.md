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
`git show 8a5b20b:<path>`, never from a working tree. The refresh was
additions-only apart from the files named below as refreshed, so some fixture
files are deliberately still in their §6 shape; see "Known divergences from
`8a5b20b`".

### What it contains

- `colors/` — the two palette schemes (`everforest`, `nord`, verbatim
  `8a5b20b`: **18 keys**, `shadow` last) plus a short stand-in `README.md`,
  which scheme enumeration must skip.
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
  non-target files deployment also symlinks (`kitty.conf`, swaync's
  `style.css`, …) so the tree is deployment-faithful. New in task 11.1:
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
- `zsh/colors.zsh` — a generated color partial (deployed as `~/.zsh_colors`).

The installer creates two links outside `$XDG_CONFIG_HOME`: `~/.zsh_colors`
and `~/.icons/default/index.theme` (`HOME_LINKS` in `src/testing.rs`); the
parent directories are plain, as `setup.sh` creates them.

### Refreshed to `8a5b20b` in task 11.1

- `config/hypr/monitors.conf` — the current file: frozen-grammar header,
  `bitdepth,10` on the eDP record, the laptop-B rule at scale `1`, and the
  four external rules active with pinned `@60`/`@120` modes (anonymized, see
  below).
- `config/hypr/hyprlock.conf` — the current file (grace-period comment,
  `dots_rounding`, lock-indicator colours, greeting and keyboard-layout
  labels), **except** `background.path`, which keeps the absolute
  placeholder (see "Known divergences").
- `config/hypr/hyprpaper.conf` — unchanged: it already matched `8a5b20b` in
  everything but the same `path` value.
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

### Known divergences from `8a5b20b`

Each of these files is kept in its §6 shape because the `8a5b20b` version
breaks an existing test whose expectation is the old content; the task that
retargets the code updates the file together with those tests. The divergence
was verified by refreshing each file in turn and running the suite.

| Fixture file | `8a5b20b` has | Existing tests it breaks | Refresh with |
|---|---|---|---|
| `config/hypr/hypridle.conf` | `--grace 10` in `lock_cmd`, dual-form dpms commands, `idle-dim` dim/restore, explanatory comments | `apply_power::a_lock_command_edit_restarts_via_the_kill_and_respawn_fallback` (expects the old `lock_cmd` text) | 11.13 |
| `config/hypr/hyprlock.conf`, `config/hypr/hyprpaper.conf` | `path = ~/Pictures/wallpaper/18.jpg` | `fixture_tree::hyprlang_configs_parse_losslessly_at_their_deployed_paths`, `apply_theme::a_wallpaper_change_dual_writes_hyprpaper_and_hyprlock_and_reloads`, `apply_theme::a_tilde_shaped_wallpaper_original_is_replaced_surgically` (all expect the substituted absolute path of an existing file) | 11.10 |

### Scheduled for removal

The live tree no longer has these (analysis §8.3, §8.6). They stay in the
fixture until the tasks that retire the code and tests targeting them:

| Fixture file | Removed by |
|---|---|
| `config/swaync/` (`config.json`, `style.css`, `colors.css`) | 11.7 (swaync replaced by notifd) |

`swaync/colors.css` is no longer a generator output (analysis §8.3): since
task 11.6 no test expects it to exist or to be deployed, and it is not in the
app's generated-file set; the copy only waits for the directory's removal.

The hyprlang input file went with task 11.4 and the hyprlang main config with
task 11.5 (its cursor copy now lives in `hyprland.lua`), together with every
test that targeted them.

### Anonymization rules

The content mirrors the real files' formats byte-for-byte where they carry no
personal data (palette hex values, section structure, comments that document
sync invariants). Everything identifying is replaced:

- **No usernames/home paths.** Absolute paths under the user's home use the
  placeholder prefix `/home/user`, which the installer rewrites to the
  temp-dir home at install time (so e.g. the wallpaper path points at a real,
  readable file inside the tree and passes the R8.3 image-path validator).
  This is a deliberate divergence from the real machine, where the
  wallpaper/lock-background values are tilde-shaped (`~/Pictures/wallpaper/…`,
  analysis §4/§6.2): substitution needs a rewritable absolute placeholder,
  since a literal `~` has no install-time home to rewrite to and would not
  resolve to a readable file for validation. A tilde-shaped-original test case
  is a task-7.2 concern; once the app expands `~` itself (task 11.10), the
  fixture can carry the real `~/…` form (see "Known divergences").
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
- **Long freeform files are trimmed stand-ins** (swaync's `style.css` keeps
  only the palette import) rather than full copies. The exception is
  `hyprland.lua`, copied whole because the Lua-literal editor must be tested
  against the real mix of addressable lines and opaque code. It has no
  literal home path to anonymize: it builds paths from `os.getenv("HOME")`.

### Deployed items the fixture omits

The real deployment (analysis §1) symlinks a few items the fixture does not
carry, because the app never reads or writes them and they are effectively
code rather than config: `eww/eww.scss`, `eww/eww.yuck`, `eww/scripts` (a
whole-**directory** symlink — a link shape the installer never creates; it
links files only), `rofi/config.rasi`, and the home-level `~/.zshrc`. Since
task 11.1 the same holds for the `wayland-dev` additions the app does not read
yet: `scripts/notifd` itself, `config/systemd/user/notifd.service` and the
D-Bus activation file
`local/share/dbus-1/services/org.freedesktop.Notifications.service` (task 11.7
counts a present `~/.config/systemd/user/notifd.service` as "notifd
installed", so it will need the unit), the remaining display/idle scripts
(`hypr-display-profile.sh`, `hypr-monitor-hotplug`, `launch-eww-bars`,
`idle-dim`, `apply-theme`) and `config/environment.d/`. Do not assume they
exist in an installed tree; if a future suite needs one, add it to the fixture
(anonymized) and extend the installer if the directory-link shape matters.

When extending the fixture, never copy a real config verbatim without checking
its contents against these rules.
