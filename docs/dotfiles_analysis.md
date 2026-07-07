# Dotfiles Analysis for the Settings GUI

Findings from exploring `~/.dotfiles` (2026-07-06). Reference for designing a native settings app that reads/writes these configs and live-reloads components.

> **Update (2026-07-06, prep tasks implemented):** all 12 tasks in `~/.dotfiles/settings_app_prep_tasks.md` — the restructurings this doc's §5 recommends — have since been carried out. Sections §1–§5 below describe the **pre-prep** state and are kept intact because `requirements.md` §9 and `architecture.md` reference them by number (`§5`, `§5.3`, …). **§6 records the resulting current state and flags every place an app-spec assumption changed (`[app-spec impact]`).** When a §1–§5 fact conflicts with §6, §6 wins.

## 1. Architecture & deployment

- `~/.dotfiles` is a plain git repo; configs deployed as **per-file symlinks** by `setup.sh` (no GNU stow). Editing repo files takes effect immediately in `~/.config`.
- `setup.sh` detects laptop vs desktop via `/sys/class/power_supply/BAT0/capacity`; many legacy links (bspwm/polybar/picom/dunst) are commented out.
- Active stack: **Hyprland** (+ hypridle/hyprlock/hyprpaper), **eww** bars, **kitty**, **rofi**, **swaync**, **zsh**, **uwsm** env.
- Live symlinks include: `~/.config/hypr/{colors,hypridle,hyprland,hyprlock,hyprpaper,monitors}.conf`, `~/.config/eww/{_colors.scss,eww.scss,eww.yuck,scripts}`, `~/.config/kitty/{kitty.conf,colors.conf}`, `~/.config/rofi/{config.rasi,colors.rasi}`, `~/.config/swaync/{config.json,colors.css,style.css}`, `~/.config/uwsm/env`, `~/.zshrc`, `~/.zsh_colors` → `zsh/colors.zsh`.
- Repo layout: `config/` (XDG app configs), `colors/` (palette sources), `scripts/` (generator + monitor/session helpers), `zsh/`, plus legacy dirs. README changelog is stale (describes old Nord/bspwm era).

## 2. Color & theme system

- **Single source of truth already exists**: `colors/<scheme>` (currently `nord` and `everforest`; everforest active). Format: one bare-hex `key=value` per line, e.g. `bg0=272e33`, `accent0=83c092`.
- Fixed 17-name schema, validated by the generator: `bg0 bg1 bg2 bg3 fg0 fg1 fg2 accent0-3 red orange yellow green blue purple`.
- `scripts/generate-colors [scheme]` writes six per-app files, each headed `# Generated from colors/<scheme> — do not edit manually`:
  - `config/hypr/colors.conf` — hyprlang vars: `$bg0 = rgb(272e33)` (bare hex, no `#`)
  - `config/eww/_colors.scss` — SCSS: `$bg0: #272e33;`
  - `config/swaync/colors.css` — GTK CSS: `@define-color bg0 #272e33;`
  - `config/rofi/colors.rasi` — rasi `* { }` block; includes a derived `bg0-overlay: #...66` (alpha suffix)
  - `config/kitty/colors.conf` — terminal keys, palette **remapped** to 16 ANSI slots (`color1 #e67e80` = red, etc.)
  - `zsh/colors.zsh` — single `THEME_ACCENT2='#a7c080'` used by the powerlevel10k prompt
- Downstream configs reference palette **names**, not raw hex: `col.active_border=$accent2` (hyprland), `@import 'colors';` + `$accent2` (eww.scss), `@import 'colors.css'` + `@bg0` / `alpha(@bg0,0.85)` (swaync style.css), `@import "colors.rasi"` (rofi config.rasi), `include colors.conf` (kitty.conf).
- Implication for the GUI: **edit only `colors/<scheme>` and rerun `generate-colors`**; treat all generated files as read-only outputs. Legacy `config/kitty/nord.conf` and `config/polybar/colors` are stale, outside the pipeline.
- `generate-colors` uses `set -euo pipefail` and validates all 17 keys before writing — it fails loudly on a malformed palette rather than emitting partial files.

## 3. Reload mechanisms

**Gap: `generate-colors` only writes files — nothing triggers reloads.** The GUI must orchestrate apply itself:

- **Hyprland**: `hyprctl reload` (rereads `hyprland.conf` + all `source=`d files). Granular live changes possible via `hyprctl keyword ...` (see `scripts/hypr-monitor-hotplug`).
- **eww**: `eww reload` (recompiles SCSS). Bars launched per-monitor by `scripts/launch-eww-bars`.
- **kitty**: no `allow_remote_control` configured, so reload requires `kill -SIGUSR1 $(pidof kitty)`. Enabling remote control + a listen socket would allow flicker-free `kitten @ set-colors`.
- **swaync**: `swaync-client -rs` (reloads CSS + config; `"cssPriority": "user"` is set).
- **rofi**: launched on demand — reads configs fresh each invocation, no reload needed.
- **zsh**: new shells or `source ~/.zsh_colors`; no daemon.
- **hyprpaper**: reads config at start; drive live via `hyprctl hyprpaper ...`. **hypridle**: restart to pick up changes. **hyprlock**: reads config at launch.
- No existing "apply theme" wrapper script — natural for the GUI (or a new repo script) to own: regenerate → reload each running daemon.

## 4. User-settings surface (file map)

- **Display/monitors**: `config/hypr/monitors.conf`, line-oriented, sourced from hyprland.conf:
  `monitor=desc:AU Optronics 0x2036,2560x1440,auto,1.066667` + catchall `monitor=,preferred,auto,1`. Later rules override earlier; desc- and name-based rules both used.
  ⚠ Per-machine eDP-1 profile logic is **duplicated** in `scripts/hypr-display-profile.sh` (`EDP_MODE/EDP_SCALE/...` keyed on EDID desc); comments say "keep the two in sync". Hotplug: `scripts/hypr-monitor-hotplug`; manual toggle uses state file `/tmp/hypr-laptop-display-forced`.
- **Keyboard/input**: `input { }` block in `config/hypr/hyprland.conf` — `kb_layout=us,se`, `kb_options=grp:win_space_toggle,caps:escape`, `sensitivity=0.3`, nested `touchpad { natural_scroll=true; tap-to-click=true }`.
- **Cursor**: ⚠ conflicting duplicates — `XCURSOR_THEME,breeze_cursors` / size 24 as env in hyprland.conf vs `XCURSOR_THEME=Nordic-cursors` / size 16 in `config/uwsm/env`.
- **Sound**: no config files in repo (no pipewire/wireplumber). Runtime-only: `pactl set-sink-volume @DEFAULT_SINK@` in hyprland binds, `pamixer`/`pactl` + `config/eww/scripts/{getvol,getmute}` in eww. GUI should drive `wpctl`/`pactl` live, not edit files.
- **Wallpaper**: `config/hypr/hyprpaper.conf` (`path = ~/Pictures/wallpaper/18.jpg`, `fit_mode = cover`). ⚠ Lock-screen background is a separate hardcoded path in `config/hypr/hyprlock.conf` (`17.png`).
- **Fonts**: scattered across 4+ files — kitty.conf (`font_family FiraCodeNFM-Med`, `font_size 10`), eww.scss (`font-family DejaVuSansM Nerd Font Mono`, hardcoded per-widget sizes), rofi config.rasi (`font: "DejaVuSans Mono 20"`), hyprlock (`font_family = Noto Sans`), plus `config/fontconfig/fonts.conf`.
- **GTK/Qt/env**: `config/uwsm/env` (`QT_QPA_PLATFORMTHEME=gtk3`, `GTK_THEME` commented out, Ozone hints). No `gtk-3.0/settings.ini` in repo.
- **Notifications**: `config/swaync/config.json` (JSON — positions, timeouts, widgets) + `style.css` + generated `colors.css`.
- **Idle**: `config/hypr/hypridle.conf` — `general { lock_cmd, before_sleep_cmd, ... }` + three `listener { timeout; on-timeout }` blocks (150/300/330s → dim, lock, dpms off).
- **Lock screen**: `config/hypr/hyprlock.conf` — colors via palette vars (`$bg3`, `$orange`, `$red`), fingerprint auth.
- **Autostart**: `exec-once=` lines at the bottom of hyprland.conf (hyprpaper, swaync, nm-applet, gammastep, hypridle, hotplug watcher, eww bars).

## 5. Parseability assessment & recommended dotfile changes

**Easy to machine read/write**
- `colors/*` — trivial kv, bare hex; ideal GUI edit target (color picker ↔ 17 keys).
- `config/swaync/config.json` — real JSON; best-in-class for a settings form.
- `config/hypr/monitors.conf` — line-oriented `monitor=` records; caveat: comment/ordering semantics and the shell-script duplicate.
- All generated color files — regular, but treat as **read-only**.

**Moderate**
- hyprlang files (`hyprland.conf`, `hypridle.conf`, `hyprlock.conf`) — nested `key { }` + flat `key=value` + `source=` includes; parseable, but a naive writer clobbers comments, ordering, and commented-out example lines.
- `kitty.conf`, `config.rasi`, `uwsm/env` (shell exports), `colors.zsh` — freeform but safe for targeted known-key edits.

**Hard — avoid writing directly**
- `eww.scss` — real SCSS (`@import`, nesting, `rgba($var,0.5)` derivations, hardcoded per-widget sizes); only the imported `_colors.scss` is generated.
- `eww.yuck` — Lisp-like s-expressions with embedded shell; effectively code.

**Pain points**
- Mixed color formats across the ecosystem: bare hex, `rgb(272e33)` without `#`, `#rrggbb`, rasi `#rrggbbaa`, CSS `alpha(@x,0.85)`, SCSS `rgba($x,0.5)` — already normalized by the generator, which is exactly why the GUI should go through it.
- Duplicated/split values a writer could desync: cursor (hyprland.conf vs uwsm/env), wallpaper vs lock background, monitors.conf vs hypr-display-profile.sh, fonts across 4+ files.
- No reload orchestration (§3).

**Recommended structural changes to `~/.dotfiles`**
1. Keep `colors/<scheme>` as the sole palette source; GUI edits it + runs `generate-colors` (this is ~90% built already).
2. Add an "apply" wrapper: regenerate, then `hyprctl reload && eww reload && swaync-client -rs && kill -SIGUSR1 $(pidof kitty)`.
3. Split hyprland.conf user-editables into dedicated `source=`d files (`input.conf`, `appearance.conf`, `autostart.conf`, `env.conf`), mirroring the existing `colors.conf`/`monitors.conf` pattern — GUI then writes small, comment-free files.
4. Consolidate duplicates: one cursor source, one wallpaper key reused by hyprlock, derive `hypr-display-profile.sh` values from `monitors.conf` (or vice versa).
5. Extend the kv-source + generator pattern to fonts (a sibling `theme` kv file templated into kitty/eww/rofi) so one "UI font" setting propagates.
6. Enable kitty `allow_remote_control` + listen socket for instant `kitten @ set-colors` application.

## 6. Post-prep state (settings-app prep tasks A–E implemented, 2026-07-06)

All 12 tasks in `~/.dotfiles/settings_app_prep_tasks.md` are done (git `9717f00..2014d10`). Below is the resulting layout, grouped by prep-task section, cross-referencing the §1–§5 facts each one changes. Items tagged **[app-spec impact]** change an assumption baked into `requirements.md`/`architecture.md`/`tasks.md` and should be reconciled there.

### 6.1 Apply & reload plumbing (A1, A2)

- **New `scripts/apply-theme [scheme]`** (default `nord`): runs `generate-colors <scheme>`, then best-effort live-reloads each *running* component — `hyprctl reload`, `eww reload`, `swaync-client -rs`, `pkill -USR1 -x kitty`. Each reload is guarded by `command -v` + `pgrep`; missing/stopped components are skipped silently. Exits non-zero **only** if generation fails (`set -euo pipefail`). This documents the canonical reload set for a palette change and closes the §3 "no apply wrapper" gap. **The app does not call this script** — it runs `generate-colors` and issues the same reloads itself (architecture §6); keep this list and the app's reload table in sync. Note kitty reload here is `pkill -USR1 -x kitty`, equivalent to the SIGUSR1-to-kitty-PIDs path.
- **kitty remote control enabled**: `kitty.conf` now has `allow_remote_control socket-only` + `listen_on unix:@kitty-{kitty_pid}` (per-instance abstract socket). Enables future flicker-free `kitten @ set-colors`; v1 (and apply-theme) still use SIGUSR1 (closes §5.6, §3 kitty note).

### 6.2 Consolidated duplicates (B1, B2, B3)

- **Cursor unified to `Nordic-cursors` / size `16`** (resolves the §2/§4 conflict, was `breeze_cursors`/24 vs `Nordic-cursors`/16). Now identical in: `config/uwsm/env` (**canonical**), `config/hypr/hyprland.conf` env lines, `scripts/launchhyprland.sh` (size only), and the new `config/gtk-{3,4}.0/settings.ini`. Each location comments the canonical source. ⚠ Still multi-located by design — the app must keep writing every copy identically (per R3.4 it writes gsettings + both `settings.ini` + `hyprctl setcursor` + the hyprland.conf env line + `uwsm/env`; `launchhyprland.sh` is a launch wrapper the app need not write but should be aware of). hyprland.conf now carries a comment forbidding **non-cursor** `env =` lines there (see 6.3).
- **Wallpaper & lock background unified** to `~/Pictures/wallpaper/18.jpg` (was 18.jpg vs 17.png, §4). `hyprpaper.conf` `wallpaper.path` and `hyprlock.conf` `background.path` now match, each commented "keep in sync." Still two keys in two files → the app exposes one "wallpaper" setting with an optional lock-screen override, writing both via the hyprlang editor (matches task 6.5).
- **eDP display profile single-sourced** (resolves §4 ⚠ duplication). `scripts/hypr-display-profile.sh` **no longer hardcodes** mode/scale; it now `awk`-parses `config/hypr/monitors.conf` for the matching `monitor=` rule (key `eDP-1` for the work laptop, `desc:AU Optronics 0x2036` for the personal one), splitting it into `name,MODE,pos,SCALE[,EXTRA…]` and deriving `EDP_MODE`/`EDP_SCALE`/`EDP_EXTRA` plus `EDP_DPI = round(96 × scale)`. Fail-safe: unreadable/absent rule → `preferred`, scale 1, DPI 96. `monitors.conf`'s comment now names it the single source. **[app-spec impact]** architecture §3 and task 6.1's "diff the two sources + non-blocking *keep in sync manually* warning" is now largely obsolete — the script reads `monitors.conf`, so an app edit to the `monitor=` record automatically feeds hotplug/toggle. The app should instead ensure its edits stay **parseable by that awk**: the target rule must keep `key,` as the leading token, mode as field 2, scale as field 4, and any extras (e.g. `bitdepth,10`) after — matching the record-editing strategy already in §5/architecture §3.

### 6.3 hyprland.conf split (C1, C2)

- **New `config/hypr/input.conf`**, `source=`d from `hyprland.conf`. The whole `input { }` block (kb_layout/variant/model/options/rules, sensitivity, follow_mouse, nested `touchpad { }`) moved there verbatim; hyprland.conf no longer contains an `input {}` block. Header marks it **app-owned**. **[app-spec impact]** the hyprlang writer target list (architecture §3, task 3.2) and the Input page (task 6.6) must now target `config/hypr/input.conf`, **not** `hyprland.conf`. Section paths are unchanged (`input.touchpad.natural_scroll`, `input.kb_layout`, …) — only the file differs, and `setup.sh` symlinks it.
- **Session env consolidated into `config/uwsm/env`**: the non-cursor `env =` lines (Qt platform theme, Ozone/GTK/backend hints; `GDK_BACKEND` added) now live only in `uwsm/env`; `hyprland.conf` retains **only** the two cursor env lines, with a comment forbidding other `env =` entries. `GTK_THEME` remains **commented-out** in `uwsm/env` (still the source the R3.3 override-warning check reads). ⚠ **`scripts/launchhyprland.sh` still exports `GTK_THEME=Nordic-bluish-accent` uncommented** — if the session is started via that wrapper, `GTK_THEME` will be present in the app's own environment, which is the *other* place architecture §5 says to check for the override. (Pre-existing; unchanged by the prep except the cursor-size fix.)

### 6.4 Palette/theme pipeline hygiene (D1, D2, D3)

- **Schema documented in-repo**: `colors/README.md` states the fixed 17-key schema, bare-hex (no `#`) format, and that generated files are never hand-edited; confirms `generate-colors` validates **key presence only** (no value-format check). Good authority for the app's palette handling and validation (R8.3).
- **Active-scheme marker**: `generate-colors` now also writes `state/active-scheme` — a repo-level, single-line file (currently `everforest`), deliberately **outside `colors/`** (so the scheme-enumeration scan never surfaces it) and **not symlinked**. Optional detection shortcut only: v1 still detects the active scheme from the `# Generated from colors/<scheme>` header (R3.2, task 3.7) unless that task is updated to prefer this file.
- **Stale artifacts removed**: `config/kitty/nord.conf` and `config/polybar/colors` are deleted. Theme/color discovery scans are now clean of out-of-pipeline files. (Remaining references are only in inactive, commented-out polybar legacy config — not in the active stack.)

### 6.5 Font pipeline & GTK settings.ini (E1, E2)

- **New font kv source `theme/fonts`** (sibling of `colors/`, documented in `theme/README.md`), read by `generate-colors` and templated into three new generated partials, each consumed like the color partials:
  - `config/kitty/fonts.conf` — `include`d from `kitty.conf` (whose inline `font_family`/`font_size` were removed);
  - `config/eww/_fonts.scss` — `@import 'fonts'` in `eww.scss`, which now uses `$mono-font`/`$mono-font-size`;
  - `config/rofi/fonts.rasi` — `@import`ed by `config.rasi` (whose inline `font:` was removed).
  Schema: optional `mono_font` (one family overriding all three monospace targets at once — the single "font" knob a future setting drives), plus required per-target fallbacks `kitty_font`/`eww_font`/`rofi_font` and sizes `kitty_font_size`/`eww_font_size`/`rofi_font_size` (independent because kitty=points, eww=pixels, rofi=points). `mono_font` is currently blank (no-op against historical fonts).
  - ⚠ **[app-spec impact]** `generate-colors` now has a **hard dependency on `theme/fonts`** and aborts if the file is missing or any of the six required font keys is absent. Because palette Apply runs `generate-colors` (R3.2), a broken `theme/fonts` now also fails a **palette** switch — the app should treat that as part of `generate-colors`'s (expanded) failure surface. The set of files `generate-colors` writes grew from **6 color partials → 6 color + 3 font partials + 1 state marker**. Fonts stay out of app v1 scope (requirements §9); `hyprlock`'s `Noto Sans` is intentionally **not** templated yet (deferred, per `theme/README.md`).
- **GTK `settings.ini` bootstrapped**: `config/gtk-3.0/settings.ini` and `config/gtk-4.0/settings.ini` are now tracked in-repo and symlinked by `setup.sh` (adds `mkdir -p ~/.config/gtk-{3,4}.0` + the two links). Both carry a `[Settings]` section with current values (`gtk-theme-name=Everforest-Green-Dark`, `gtk-icon-theme-name=Everforest-Dark`, cursor `Nordic-cursors`/`16`, `gtk-font-name=Noto Sans  10`, dpi, etc.). **[app-spec impact]** the INI writer (task 3.5) now edits **existing tracked files** on the target machine — the "create file + `[Settings]` section if absent" path becomes a fallback, not the norm. ⚠ Their in-file comment warns that in an XSETTINGS/dconf session GTK reads the cursor from `gsettings` (`org.gnome.desktop.interface cursor-theme/size`), which may differ — reinforcing R3.4's requirement that the app also set gsettings, not just the ini.

### 6.6 Net effect on the app's file map

- **New hyprlang parser target**: `config/hypr/input.conf` (input settings moved off `hyprland.conf`).
- **New INI targets now pre-existing**: `config/gtk-{3,4}.0/settings.ini`.
- **Read-only inputs unchanged in role, expanded in count**: the six generated color files, plus three generated font partials (`kitty/fonts.conf`, `eww/_fonts.scss`, `rofi/fonts.rasi`) and `state/active-scheme` — all carry a `Generated from …` header and must never be written directly.
- **New non-app sources to be aware of** (not written by v1): `theme/fonts` (font kv source), `scripts/apply-theme` (canonical CLI reload set to mirror).
- **Simplified**: cursor value is now consistent (still write-both-identically); wallpaper/lock share one path; the eDP monitor record is the sole source the profile script reads.
