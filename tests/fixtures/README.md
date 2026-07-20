# Test fixtures

## `dotfiles/` — anonymized dotfiles repo (task 7.1, R6.1)

An anonymized copy of the real `~/.dotfiles` layout the app targets (see
`docs/dotfiles_analysis.md`, especially §6 for the post-prep state). Integration
suites never read it in place: `settings4000::testing::FixtureDotfiles::install()`
copies it into a fresh `tempfile` directory per test and creates the per-file
deployment symlinks under a fake `$XDG_CONFIG_HOME`, exactly as the real
`setup.sh` deployment does.

### What it contains

- `colors/` — the two palette schemes (`everforest`, `nord`; full 17-key schema)
  plus a `README.md`, which scheme enumeration must skip.
- `scripts/generate-colors` — an executable stub. Detection only checks its
  presence; tests drive the generator through `MockCommandRunner`, so the stub
  exits non-zero to make an accidental real invocation loud.
- `state/active-scheme`, `theme/fonts` — the post-prep repo-level sources.
- `config/` — every file the app parses or writes (`hypr/*`, both GTK
  `settings.ini`, `uwsm/env`, `swaync/config.json`) plus the generated
  color/font partials with their `# Generated from …` headers, and the
  non-target files deployment also symlinks (`kitty.conf`, swaync's
  `style.css`, …) so the tree is deployment-faithful.
- `zsh/colors.zsh` — the sixth generated color partial (deployed as
  `~/.zsh_colors`).

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
  is a task-7.2 concern.
- **No hardware serials or locations.** Monitor EDID `desc:` strings and the
  location comments in `monitors.conf` are neutral placeholders — still
  awk-parseable in the exact record shape `hypr-display-profile.sh` expects.
- **Long freeform files are trimmed stand-ins** (`hyprland.conf` keeps every
  load-bearing shape — `source=` lines, the two cursor `env =` lines, nested
  sections, repeatable keys, commented-out lines; swaync's `style.css` keeps
  only the palette import) rather than full copies.

### Deployed items the fixture omits

The real deployment (analysis §1) symlinks a few items the fixture does not
carry, because the app never reads or writes them and they are effectively
code rather than config: `eww/eww.scss`, `eww/eww.yuck`, `eww/scripts` (a
whole-**directory** symlink — a link shape the installer never creates; it
links files only), `rofi/config.rasi`, and the home-level `~/.zshrc`. Do not
assume they exist in an installed tree; if a future suite needs one, add it to
the fixture (anonymized) and extend the installer if the directory-link shape
matters.

When extending the fixture, never copy a real config verbatim without checking
its contents against these rules.
