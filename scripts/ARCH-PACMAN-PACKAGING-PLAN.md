# Plan: Native Arch Linux Installation via CI-Built Pacman Artifact

Status: **Implemented** — on `feature/hyperv-enhanced-session-v3` (local commits only; deliberately **no PRs opened against upstream `lamco-admin/lamco-rdp-server`**, per maintainer decision, and no AUR publication).

> Post-implementation corrections: several details below were refined during implementation and validated empirically against real Arch-authored `.pkg.tar.zst` packages on a test system. Where this document still shows the originally proposed shape, the shipped code wins. The main divergences, all reflected in the current `build_pacman()`:
>
> - `.PKGINFO` uses a **single combined `pkgver = <ver>-<pkgrel>` field** (e.g. `pkgver = 1.4.5-hyperv3-1`), not separate `pkgver`/`pkgrel` lines — that is what real packages ship, and what pacman's parser reads. `pkgbase` and `xdata = pkgtype=pkg` are also emitted.
> - **No `backup=` entry**: the D-Bus policy ships under `/usr/share/dbus-1/system.d/` (not `/etc/dbus-1/system.d/` as first sketched), and `/etc/lamco-rdp-server/` is an empty runtime config dir owned by the admin, so there is nothing to `.pacsave`.
> - **`.MTREE` is gzip-compressed** (`#mtree` header, `/set type=file uid=0 gid=0 mode=644`, per-file `time=<epoch>.0 size=<n> sha256digest=<hex>`, `mode=755` for executables, `type=dir` entries), matching real packages, and it **is implemented** (the v1 fallback of skipping it was never used).
> - `.BUILDINFO` is omitted — optional for `pacman -U` and irrelevant to a binary artifact not built by makepkg.
> - The stale unit-name hints in the DEB postinst and tarball `install.sh` were fixed in the same change set as `.INSTALL`.

## 1. Goal and scope

Support native installation of this fork's build (the Hyper-V Enhanced Session variant of `lamco-rdp-server`) on Arch Linux by extending the fork's release pipeline to build and publish a pacman package that users install directly:

```bash
sudo pacman -U lamco-rdp-server-1.4.5-hyperv3-1-x86_64.pkg.tar.zst
```

Scope decision (recorded from maintainer input): **CI-built pacman artifact, no AUR publication.** The `.pkg.tar.zst` ships as an additional GitHub release asset alongside the existing DEB and tarball. This gets Arch users running immediately without AUR account/maintenance overhead, and without the fork's git-rev source dependencies (which would complicate a source-built AUR package; they are irrelevant to a binary artifact).

Out of scope:

- AUR publication of any flavor. A thin `-bin` wrapper consuming this same `.pkg.tar.zst` remains a possible future follow-up (candidate names `lamco-rdp-server-hyperv-bin` etc. were verified unclaimed at plan time); no account is needed until then.
- `aarch64` builds — the fork's CI is x86_64-only today (upstream's PKGBUILD declares both arches).
- Changes to upstream's AUR package or `packaging/aur/`; that PKGBUILD stays untouched as the proven reference recipe.

## 2. Current state

Release assets today (`v1.4.5-hyperv.2`, `v1.4.5-hyperv.3`): DEB, tar.gz, `SHA256SUMS.txt`. The README's "Native / Distribution Packages" table points Arch users at the upstream AUR package `lamco-rdp-server`, which does not contain the fork's `vsock`, `websocket`, `kwin-virtual`, or `x264` features.

Relevant machinery (all on `feature/hyperv-enhanced-session-v3`):

| File | Role in this plan |
|---|---|
| `scripts/build-release-artifacts.sh` | The artifact factory to extend: `stage_install_set()` produces a single staging tree consumed by `build_deb` and `build_tarball`; `SKIP_BUILD`/`SKIP_DEB`/`SKIP_TARBALL` gate each artifact; `write_checksums()` globs `./*.deb ./*.tar.gz` into `SHA256SUMS.txt` |
| `.github/workflows/release.yml` | Tag guard derives `PKG_VERSION` (`v1.4.5-hyperv.3` → `1.4.5-hyperv3`), runs the build script, uploads `dist/release/*.deb`, `dist/release/*.tar.gz`, `dist/release/SHA256SUMS.txt` via `softprops/action-gh-release@v2` |
| `packaging/aur/PKGBUILD` | Proven dependency names, `backup=` entry, `options=(!lto)`, and `.install` migration pattern — reference for the fork's `.PKGINFO` and `.INSTALL` |
| `packaging/aur/lamco-rdp-server.install` | Migration precedent: carries systemd user unit enable-state from `lamco-rdp-server.service` to `app-io.lamco.rdp-server.service` |
| `scripts/test-release-workflow-logic.sh` | Dry-run harness; currently no pacman coverage |
| `RELEASING.md` | Tag guard + draft release checklist that gains a pacman step |

Build environment facts:

- CI runs `ubuntu-latest` (glibc 2.39). Arch rolling always provides a newer glibc, so Ubuntu-built binaries satisfy Arch's loader; the constraint only bites in the reverse direction.
- The fork builds with features `default,vaapi,gui,vsock,websocket,kwin-virtual,x264`; the binaries dlopen `libx264.so` (libloading shim) and openh264 at runtime — neither is installed by the package, so both are pacman `optdepends`.

## 3. Design: `build_pacman()` step

### 3.1 Flow and flags

Insert between `build_tarball` and `write_checksums`:

```
do_build -> build_deb -> build_tarball -> build_pacman -> write_checksums
```

`build_pacman` reuses `stage_install_set()` into `$(mktemp -d)` exactly like `build_deb`/`build_tarball`, so DEB, tarball, and pacman can never drift in install layout. Add `--skip-pacman` / `SKIP_PACMAN=1` mirroring the existing flag pattern.

A pacman package is just a tar.zst archive with metadata entries at the root. `tar --zstd` needs the `zstd` binary on the runner (present on GitHub's ubuntu images; the plan still pins it in apt).

### 3.2 Package naming and version

Filename (pacman parses `_pkgver-pkgrel-arch.pkg.tar.zst`):

```
lamco-rdp-server-1.4.5-hyperv3-1-x86_64.pkg.tar.zst
```

- `pkgname = lamco-rdp-server`: parity with upstream. `pkgver` and `pkgrel` are emitted as separate `.PKGINFO` fields, so the hyphenated `1.4.5-hyperv3` is reused verbatim (matching the tag-derived `PKG_VERSION` and the DEB/tarball version exactly). pacman splits an assembled `pkgver-pkgrel` string at the *last* hyphen (`strrchr`), so `1.4.5-hyperv3-1` parses to `pkgver = 1.4.5-hyperv3`, `pkgrel = 1`. The PKGBUILD "no hyphen in pkgver" rule is a makepkg lint convention only and does not apply to a hand-rolled `.PKGINFO` (nor to `pacman -U`, which reads those fields directly, not the filename).
- `pkgrel = 1` (bump for re-publishes of the same upstream version bump).
- `arch = x86_64`.

### 3.3 Package metadata (`.PKGINFO`)

Generated by heredoc; `size`, `builddate`, and `packager` written per-build:

```text
pkgname = lamco-rdp-server
pkgver = 1.4.5-hyperv3
pkgrel = 1
pkgdesc = Remote desktop server (Hyper-V Enhanced Session fork: AF_VSOCK, WebSocket, x264)
url = https://github.com/moerketh/lamco-rdp-server
packager = Automated Release Pipeline <noreply@github.com>
license = BUSL-1.1
arch = x86_64
builddate = <unix seconds>
size = <installed size, bytes>
depend = glibc
depend = gcc-libs
depend = dbus
depend = pipewire
depend = libpipewire
depend = xdg-desktop-portal
depend = wayland
depend = libxkbcommon
depend = pam
depend = fuse3
depend = libva
depend = openssl
optdepend = x264: system encoder (dlopen'd via x264 feature)
optdepend = openh264: Cisco H.264 encoder (dlopen'd; see packaged license text)
optdepend = vulkan-icd-loader: Vulkan renderer for the GUI
optdepend = xdg-desktop-portal-gnome: GNOME portal backend
optdepend = xdg-desktop-portal-kde: KDE portal backend
optdepend = xdg-desktop-portal-wlr: wlroots portal backend
optdepend = xdg-desktop-portal-hyprland: Hyprland portal backend
backup = etc/dbus-1/system.d/io.lamco.RdpServer.System.conf
```

### 3.4 Dependency mapping (fork DEB static list → Arch)

The fork's DEB postinst/build script carries the empirically validated runtime list (Debian names, with alternatives); `packaging/debian/control` itself uses `${shlibs:Depends}` and holds no static list. Mapping, cross-checked against upstream's PKGBUILD `depends`:

| Fork's Debian runtime list | Arch `depend` |
|---|---|
| `libdbus-1-3` | `dbus` |
| `libfuse3-3 \| libfuse3-4` | `fuse3` |
| `libpam0g` | `pam` |
| `libpipewire-0.3` (implicit from build deps; pipewire libs) | `pipewire` + `libpipewire` |
| `libssl3 \| libssl3t64` | `openssl` |
| `libva2` | `libva` |
| `libwayland-client0` | `wayland` |
| `libxkbcommon0` | `libxkbcommon` |
| `pipewire` | *(covered above)* |
| `xdg-desktop-portal` | `xdg-desktop-portal` |
| *(baseline, always linked)* | `glibc`, `gcc-libs` |

Fork additions (dlopen'd at runtime, not linked → `optdepends`):

- `x264` — the fork's encoder feature, loaded via libloading shim
- `openh264` — upstream's precedent (optdepend + staged `OpenH264-BINARY_LICENSE.txt` in `/usr/share/doc/lamco-rdp-server/`; Cisco binary is never bundled)

The list must be revisited whenever the release feature set changes (e.g. dropping `vaapi` drops `libva`).

### 3.5 `.INSTALL` script

Derived from upstream's `packaging/aur/lamco-rdp-server.install` migration, with the fork's unit name used consistently everywhere (the shipped unit is `app-io.lamco.rdp-server.service`):

```bash
post_install() {
    cat <<'EOF'
:: lamco-rdp-server (Hyper-V Enhanced Session fork) installed.
   Enable the systemd user service:
     systemctl --user enable --now app-io.lamco.rdp-server.service
   To migrate enable-state from the historical lamco-rdp-server.service unit,
   this package's post_upgrade carries it over automatically on upgrade.
EOF
}

post_upgrade() {
    # Old unit was renamed to app-io.lamco.rdp-server.service so
    # xdg-desktop-portal can derive a real app id from the unit name. Carry
    # over enable-state so the service keeps autostarting at login; only
    # reachable for users with an active systemd --user manager.
    local old_unit="lamco-rdp-server.service"
    local new_unit="app-io.lamco.rdp-server.service"
    local socket uid user

    for socket in /run/user/*/systemd/private; do
        [ -S "$socket" ] || continue
        uid="${socket#/run/user/}"
        uid="${uid%%/*}"
        user="$(getent passwd "$uid" | cut -d: -f1)"
        [ -n "$user" ] || continue
        if systemctl --user --machine="${user}@" is-enabled "$old_unit" >/dev/null 2>&1; then
            systemctl --user --machine="${user}@" disable "$old_unit" >/dev/null 2>&1 || true
            systemctl --user --machine="${user}@" enable "$new_unit" >/dev/null 2>&1 || true
        fi
    done
}
```

Related tightly-coupled fix: the fork's DEB postinst and tarball `install.sh` still print the old unit name — update those hints to the `app-io` name at the same time, or Arch/DEB users receive (continue to receive) different instructions.

### 3.6 Staging notes (`stage_install_set()` reuse)

`stage_install_set` lays out (identical for DEB, tarball, pacman):

- `/usr/bin/lamco-rdp-server`, `/usr/bin/lamco-rdp-server-gui` (0755)
- `/usr/lib/systemd/user/app-io.lamco.rdp-server.service`
- `/usr/share/dbus-1/services/io.lamco.RdpServer.service`, `/usr/share/dbus-1/system.d/io.lamco.RdpServer.System.conf` (the `backup=` entry), `/usr/share/polkit-1/actions/io.lamco.RdpServer.policy`
- `/etc/lamco-rdp-server/` (0755, empty; user config lives here)
- `/usr/share/doc/lamco-rdp-server/{LICENSE, OpenH264-BINARY_LICENSE.txt, examples/example-config.toml}`
- `/usr/share/applications/lamco-rdp-server.desktop`, metainfo, hicolor icons (svg + png sizes)

For the pacman archive, metadata entries sit at archive root: `.PKGINFO`, `.MTREE`, `.INSTALL` must be at depth 0 relative to the package root, not nested in `/usr/...`. Two ways this goes wrong and the guards:

- **Absolute-path entries**: use `tar -C "$STAGE" -caf <file> .` plus separate metadata additions rather than absolute paths (absolute tar paths would strip to `usr/...` incorrectly and place `.PKGINFO` inside `/usr`, breaking pacman parsing) — or use `--transform`. The build step must assert the archive root contains `.PKGINFO` before finishing.
- **drvfs staging pitfall**: staging on Windows-mounted filesystems breaks byte-exactness (the existing reason `build_deb` mandates `mktemp -d` in `/tmp`). `build_pacman` inherits `mktemp -d` staging because tarring from drvfs is equally unreliable.

### 3.7 `.MTREE`

Not required for `pacman -U`. Required by `repo-add` (so a future AUR `-bin` or pacman repo could consume the artifact) and best practice. Generate a minimal ustar-style `.MTREE` (per-file `sha256`, `size`, `mtime`) in the same loop that computes the digest entries — no bsdtar dependency; hand-rolled is fine and matches repo-add's expectations. If implementation time-boxes it, skip `.MTREE` in v1 and document the omission.

### 3.8 Checksums and workflow upload

- `write_checksums()`: extend the glob to `sha256sum ./*.deb ./*.tar.gz ./*.pkg.tar.zst` (the `|| true` swallow stays because SHA256SUMS should not make the build fail if one artifact was skipped).
- `release.yml` upload `files:` gains `dist/release/*.pkg.tar.zst`.
- apt pin (`--no-install-recommends` group already exists): add `zstd`.
- All changes land behind `--skip-pacman`-aware code so a broken pacman step never blocks a DEB+tarball release (the maintainer can `--skip-pacman` an emergency release just like `--skip-deb`).

## 4. Documentation updates

- **README.md** "Native / Distribution Packages": add a fork row for Arch with the `pacman -U` command; add a caveat to the existing AUR row clarifying it is upstream's source-built package without the fork's features.
- Add an "Arch Linux (fork binary)" short section after install steps:

```markdown
### Arch Linux (fork binary)

Download the `.pkg.tar.zst` from the [latest release](https://github.com/moerketh/lamco-rdp-server/releases) and run:

    sudo pacman -U lamco-rdp-server-1.4.5-hyperv3-1-x86_64.pkg.tar.zst
    systemctl --user enable --now app-io.lamco.rdp-server.service

Arch names may conflict with the upstream AUR `lamco-rdp-server` package: if you have it installed, remove it first (`sudo pacman -R lamco-rdp-server`), since pacman refuses to replace a same-name package implicitly. The upstream package lacks this fork's Vsock/WebSocket/x264 features.
```

- **RELEASING.md**: add a checklist step (assert the `.pkg.tar.zst` asset exists with the expected filename, and is listed in `SHA256SUMS.txt`).
- **packaging/README.md**: describe the fork's pacman artifact and where its inputs live.

## 4.1 Testing strategy

- **Harness (`scripts/test-release-workflow-logic.sh`)**: add assertions that `--skip-pacman` is honored, the checksum glob includes `.pkg.tar.zst`, the upload globs include it, and the version string parses to `pkgver=1.4.5-hyperv3`, `pkgrel=1` (i.e. `pkgver` equals `PKG_VERSION`, guarding against tag-format regressions).
- **Manual validation on an Arch VM/toolbox (per RELEASING.md's draft-release rules)**:
  1. Draft release contains all four assets + `SHA256SUMS.txt` covering the pacman artifact.
  2. `sudo pacman -U …` succeeds; `pacman -Qi lamco-rdp-server` shows `1.4.5-hyperv3-1`, the mapped depends list, optdepends, and the `backup` entry.
  3. `systemctl --user enable --now app-io.lamco.rdp-server.service` starts; GUI launches.
  4. dlopen paths: with `x264` absent, server logs the x264-backend warning; with it installed, encoding works. Same for `openh264` on the vaapi path.
  5. AF_VSOCK smoke test in a Hyper-V VM (Windows client), per the fork's release validation rules.
  6. Upgrade path: install prior fork `.pkg.tar.zst`, then `pacman -U` the new artifact; `.INSTALL` migration carries old-unit enable-state.
  7. `sudo pacman -R lamco-rdp-server` removes everything; the `backup` file survives as `.pacsave` on removal, and install-time conflicts surface as `.pacnew`. `pacman -Ql` shows no absolute-path leakage from the tar stage (guards the §3.6 tar-root trap).
  8. `pacman -Qi` renders `pkgdesc` correctly (guards the `.PKGINFO` shape).

## 5. Risks

| Risk | Mitigation |
|---|---|
| Hyphen in pkgver might trip naive version parsing | pacman splits pkgver from pkgrel at the *last* hyphen, and `pacman -U` reads the explicit `.PKGINFO` fields; harness asserts `pkgver=1.4.5-hyperv3`, `pkgrel=1` |
| Same pkgname as upstream AUR package → install conflict | README + release notes state the conflict and uninstall-first steps; no AUR publication, so no overwrite risk exists |
| Ubuntu-built binaries on Arch | Non-issue; Arch's newer glibc runs them. Documented so nobody "fixes" it later |
| Missing `.MTREE` blocks `repo-add` later | Generate ustar-style `.MTREE` in v1 or document omission (v1 fallback) |
| drvfs staging breaks tar byte-exactness | Same `mktemp -d` staging as DEB; harness asserts staging stays off Windows mounts |
| `.PKGINFO` typo class (field shape, missing `=` spacing) | Harness regex assertions + `pacman -Qi` manual check |
| Stale unit name in instructions (DEB postinst / install.sh) | Same fix in the same PR as the `.INSTALL` (one migration source of truth) |
| OpenH264 binary license | License text staged under `/usr/share/doc/lamco-rdp-server/`; openh264 is an optdepend; the pinned openh264 artifact is never committed to the repo |
| Optdepend list drift as features change | Deps re-derived from the build script's static list which lives beside the feature list; release checklist re-check step |

## 6. Implementation outline

Implemented on `feature/hyperv-enhanced-session-v3` as **local commits only** — no PRs, issues, or comments against upstream `lamco-admin/lamco-rdp-server` (maintainer handles all public/upstream interaction). The originally sketched three stacked PRs became one local change set, in the same logical pieces:

1. **Pacman builder**: `build_pacman()` + `--skip-pacman` in `scripts/build-release-artifacts.sh`, checksum glob, harness coverage. Local builds exercise the artifact end-to-end.
2. **CI + docs**: `release.yml` upload glob + `zstd` pin, README table row + Arch section, RELEASING.md step, packaging/README.md.
3. **Related fix-ups**: stale unit-name hints in DEB postinst and tarball `install.sh` (consistency with `.INSTALL`); deferred AUR `-bin` wrapper if community demand appears.

## 7. Future: AUR `-bin` (deferred, optional)

A `lamco-rdp-server-hyperv-bin` PKGBUILD that consumes this exact `.pkg.tar.zst` as its `source` — identical install result, AUR-searchable. Names `lamco-rdp-server-bin`/`-hyperv-bin`/`-git` were all verified unclaimed at plan time. This plan's artifact is designed to be consumable by that wrapper without modification (hence .MTREE). No action until someone volunteers to maintain it.