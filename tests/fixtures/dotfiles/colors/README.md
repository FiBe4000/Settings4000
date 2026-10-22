# Palette schemes

One file per scheme, plain `key=value` lines with bare hex values (no `#`).
Every scheme must define exactly these 18 keys (the `NAMES=(…)` list in
`scripts/generate-colors`):

    bg0 bg1 bg2 bg3 fg0 fg1 fg2 accent0 accent1 accent2 accent3
    red orange yellow green blue purple shadow

`scripts/generate-colors <scheme>` validates key presence and templates the
palette into the generated per-app partials. Never hand-edit generated files.

This README deliberately lives inside `colors/` so the scheme scan must
skip non-palette files (it is part of the test fixture's coverage).
