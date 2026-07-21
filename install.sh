#!/usr/bin/env bash
# install.sh — per-user install (and uninstall) of Settings4000 (task 8.2).
#
# Builds the release binary and installs the three desktop-integration pieces
# entirely under the invoking user's home — no root, nothing outside ~/.local
# (R8.2):
#
#   ~/.local/bin/settings4000                                  the binary
#   $XDG_DATA_HOME/applications/<app-id>.desktop               launcher entry
#   $XDG_DATA_HOME/icons/hicolor/scalable/apps/<app-id>.svg    icon
#
# ($XDG_DATA_HOME defaults to ~/.local/share.) The desktop entry is what makes
# the app launchable from rofi's drun mode and gives its Wayland windows an
# icon; single-instance activation (R8.4) is handled by the app itself via its
# fixed GApplication ID, which the desktop file name must match — see
# data/org.settings4000.Settings4000.desktop.
#
# Usage:
#   ./install.sh              build and install
#   ./install.sh --uninstall  remove the three installed files
#
# Re-running is safe: every step overwrites its previous output.

set -euo pipefail

# Must equal the APP_ID constant in src/ui/app.rs. A unit test there asserts
# this exact assignment line (and the data/ file names), so a drift between
# the script and the constant fails `cargo test`.
app_id="org.settings4000.Settings4000"

# Resolve the repository root from this script's own location so the script
# works from any working directory.
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_path="$HOME/.local/bin/settings4000"
apps_dir="$data_home/applications"
desktop_path="$apps_dir/$app_id.desktop"
icon_path="$data_home/icons/hicolor/scalable/apps/$app_id.svg"

# Refresh the launcher's view of the applications directory. The database is a
# lookup cache — launchers fall back to scanning the directory when it is
# stale or the tool is absent — so a missing update-desktop-database is not an
# error. The directory check keeps `--uninstall` working on a machine that
# never installed: update-desktop-database exits non-zero on a missing
# directory, which would kill the script under `set -e`.
refresh_desktop_database() {
    if [[ -d "$apps_dir" ]] && command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$apps_dir"
    fi
}

# Refresh the GTK icon cache when one is actually in use. The cache is
# optional (icon lookup falls back to scanning the theme directories), and
# gtk-update-icon-cache refuses to run on a theme directory without an
# index.theme — the per-user hicolor tree usually has none, since the real
# index lives in /usr/share/icons/hicolor. Run on uninstall too, so a cached
# copy of the removed icon cannot linger.
refresh_icon_cache() {
    if command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && [[ -f "$data_home/icons/hicolor/index.theme" ]]; then
        gtk-update-icon-cache -q "$data_home/icons/hicolor"
    fi
}

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f -- "$bin_path" "$desktop_path" "$icon_path"
    refresh_desktop_database
    refresh_icon_cache
    echo "Removed $bin_path, $desktop_path, and $icon_path."
    exit 0
elif [[ "${1:-}" != "" ]]; then
    echo "Usage: $0 [--uninstall]" >&2
    exit 2
fi

# The desktop entry is installed with its Exec line rewritten to the absolute
# binary path (see the rewrite below): the Desktop Entry spec performs no
# ~/$HOME expansion in Exec, and ~/.local/bin is not guaranteed to be on the
# PATH rofi launches with. The spec gives whitespace and `%` special meaning
# inside Exec (argument splitting and field codes); embedding such a path
# verbatim would produce a broken or spec-invalid entry, and handling it would
# need the spec's own quoting/`%%`-escaping rules. Home paths like that are
# vanishingly rare, so abort with a clear message instead of carrying escaping
# code — checked up front, before anything is built or installed.
if [[ "$bin_path" == *[[:space:]%]* ]]; then
    echo "error: '$bin_path' contains whitespace or '%', which cannot be" >&2
    echo "written into a desktop entry's Exec line without spec escaping" >&2
    echo "this script does not implement; install the binary to a plain path." >&2
    exit 1
fi

# --locked builds against the committed Cargo.lock, so an install always uses
# the dependency versions the test suite ran against.
cargo build --release --locked --manifest-path "$repo_root/Cargo.toml"

install -Dm755 -- "$repo_root/target/release/settings4000" "$bin_path"
install -Dm644 -- "$repo_root/data/$app_id.svg" "$icon_path"

# The Exec rewrite copies the template line by line (rather than through sed, whose
# replacement text has its own escaping rules) into a temporary file beside
# the destination; that temp file is validated *before* being moved into
# place, so a malformed entry fails the install loudly and never becomes
# visible to the launcher. The dot-prefixed temp name keeps launchers that
# scan mid-install from picking it up, and mv within one directory is atomic.
mkdir -p -- "$apps_dir"
tmp_entry="$(mktemp --suffix=.desktop -- "$apps_dir/.$app_id.XXXXXX")"
while IFS= read -r line; do
    if [[ "$line" == Exec=* ]]; then
        printf 'Exec=%s\n' "$bin_path"
    else
        printf '%s\n' "$line"
    fi
done <"$repo_root/data/$app_id.desktop" >"$tmp_entry"
# mktemp creates the file 0600; installed entries are conventionally 0644.
chmod 644 -- "$tmp_entry"

if command -v desktop-file-validate >/dev/null 2>&1 \
    && ! desktop-file-validate "$tmp_entry"; then
    rm -f -- "$tmp_entry"
    echo "error: the generated desktop entry failed validation (see above);" >&2
    echo "nothing was installed to $desktop_path." >&2
    exit 1
fi

mv -- "$tmp_entry" "$desktop_path"

refresh_desktop_database
refresh_icon_cache

echo "Installed:"
echo "  $bin_path"
echo "  $desktop_path"
echo "  $icon_path"
