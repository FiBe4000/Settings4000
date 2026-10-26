# Releasing Settings4000

This document is the complete procedure for publishing a new version of
Settings4000: the versioning policy, how the two Arch Linux packages relate to
releases, and the step-by-step checklist. It is meant to be followed as written,
without needing any other source.

## Versioning policy

- **One version source.** The version is the `version` field of `Cargo.toml`
  (requirement R8.6 in `docs/requirements.md`). The `--version` output, the
  startup log and the About window read it at compile time, and the `-git`
  package derives its version from it. The only other copy is the release
  package's `pkgver=` (see below), which a unit test keeps equal to it.
- **Semantic versioning.** The version is `MAJOR.MINOR.PATCH` and is bumped
  whenever a user-visible change ships:
  - `PATCH` for fixes that change no intended behaviour;
  - `MINOR` for new settings, pages, packaging or other features, and for
    support of a changed dotfiles layout that keeps the old one working;
  - `MAJOR` when something a user relied on stops working as before, such as
    dropping support for a dotfiles layout or removing a setting.

  Changes nobody can observe from outside — refactoring, tests, CI, internal
  documentation — do not need a release.
- **Every bump is a release, and every release is a tag.** The commit that
  bumps the version is tagged with an annotated git tag `v<version>` (for
  example `v1.1.0`), and the commit and the tag are pushed to GitHub together.
  A version is never bumped without being tagged.
- **A published tag is never moved or deleted.** The release package builds
  the tag, so moving it would make the same version mean different code. A
  mistake in a release is fixed by a new release.
- **`main` between tags may carry unreleased work.** Such commits are not
  releases; the `-git` package exists to install them.

The first tagged release is `v1.1.0`. Version 1.0.0 was never tagged: its tree
has no license and no packaging, so no package could be built from it.

## The two Arch Linux packages

Both live under `packaging/arch/` and install the same files; they conflict
with each other, so a system has one or the other.

- **`settings4000-git`** (`packaging/arch/settings4000-git/PKGBUILD`) builds
  the latest commit of `main`. Its version is computed at build time from
  `Cargo.toml` plus the commit count and hash (for example
  `1.1.0.r112.g0abc123`), so it needs no change when releasing; its
  `pkgver=0` line is only the placeholder makepkg requires and stays `0`.
- **`settings4000`** (`packaging/arch/settings4000/PKGBUILD`) builds the
  release tag `v<pkgver>`, cloned from GitHub with a `#tag=` git source. Its
  `pkgver=` is therefore a literal that must be updated in every release. The
  source is the git tag rather than a source tarball because a tarball
  checksum cannot be committed with the release it describes: the tarball of a
  tag contains the PKGBUILD itself, so its hash would have to cover itself. A
  git source needs no checksum (`sha256sums=('SKIP')`). makepkg 7 can
  checksum a `#tag=` source too — `makepkg -g` prints a hash of `git archive`
  of the tag — but that hash covers the tagged tree, PKGBUILD included, so it
  has the same self-reference and is deliberately not used: keep `'SKIP'`.
  makepkg rejects an annotated tag whose embedded name differs from the
  requested one (a "forged" tag); a lightweight tag carries no name and passes
  that check silently, which is one reason the policy requires annotated tags.
  If releases are ever GPG-signed, adding `?signed` to the source and the
  signing key to `validpgpkeys` is the integrity upgrade.

Unit tests in `src/ui/app.rs` fail when the release package's `pkgver=` differs
from `Cargo.toml`, and when its build functions, `depends` or `optdepends`
drift from the `-git` package's.

## Release checklist

Replace `<version>` with the new version, for example `1.2.0`, throughout.

1. **Start from a clean, current `main`** that already contains everything the
   release should include, with the last CI run green:

   ```sh
   git switch main
   git pull --ff-only
   git status        # must report a clean working tree
   ```

2. **Choose the version** by the policy above.

3. **Bump `Cargo.toml` and `Cargo.lock`.** Set `version = "<version>"` in the
   `[package]` section of `Cargo.toml`, then let Cargo record it in the lock
   file without touching any dependency:

   ```sh
   cargo update --workspace
   git diff Cargo.lock    # only the `settings4000` entry's version may change
   ```

4. **Update the release package.** In `packaging/arch/settings4000/PKGBUILD`
   set `pkgver=<version>` and `pkgrel=1`, then regenerate its `.SRCINFO`:

   ```sh
   cd packaging/arch/settings4000
   makepkg --printsrcinfo > .SRCINFO
   cd -
   ```

   Leave `packaging/arch/settings4000-git/` unchanged.

5. **Run the three gates**, which include the test that pins `pkgver=` to
   `Cargo.toml`:

   ```sh
   cargo fmt --check
   cargo clippy -- -D warnings
   cargo test
   ```

6. **Commit** the four changed files (`Cargo.toml`, `Cargo.lock`, and the
   release package's `PKGBUILD` and `.SRCINFO`) with the subject
   `Release <version>`, optionally followed by a short bullet list of the
   user-visible changes:

   ```sh
   git commit -a -m "Release <version>"
   ```

7. **Tag that commit** with an annotated tag (a lightweight tag is not pushed
   by `--follow-tags` in the next step):

   ```sh
   git tag -a v<version> -m "Settings4000 <version>"
   ```

8. **Push the commit and the tag together:**

   ```sh
   git push --follow-tags origin main
   ```

   They must go out in the same push. The CI run for the `main` push builds the
   release package by cloning the tag `v<version>` from GitHub; if the commit
   arrives without its tag, that build fails. (For the same reason a pull
   request that bumps the version cannot pass CI before the tag exists, so
   releases are committed on `main` directly.)

9. **Watch both CI runs** — one for `main`, one for the tag — until they are
   green. In the tag's run, the `Arch package` jobs additionally check that the
   tag equals the version in `Cargo.toml` and in the release `PKGBUILD`:

   ```sh
   gh run list --limit 4
   gh run watch <run id>
   ```

10. **Optionally publish a GitHub release** from the tag:

    ```sh
    gh release create v<version> --verify-tag --title "Settings4000 <version>" \
      --notes "<the user-visible changes>"
    ```

## When something goes wrong

- **CI fails after the push, or a release turns out broken:** do not move or
  delete the tag. Fix the problem on `main` and publish the next `PATCH`
  version with this checklist.
- **The release PKGBUILD needs a packaging-only fix** (for example a new
  dependency) with no change to the application: increment `pkgrel=` in
  `packaging/arch/settings4000/PKGBUILD` and commit without a new tag. makepkg
  uses the PKGBUILD from the checkout it runs in and still builds the existing
  tag. Apply the same change to `packaging/arch/settings4000-git/PKGBUILD`,
  since the tests require the dependency arrays and build functions to stay
  identical, and regenerate **both** `.SRCINFO` files
  (`makepkg --printsrcinfo > .SRCINFO` in each directory) — CI fails on either
  one being stale. The `-git` package's `pkgrel=` may be incremented as well;
  that does not affect its computed version. The release `pkgrel` returns to
  `1` with the next version.
