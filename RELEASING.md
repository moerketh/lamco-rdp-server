# Releasing lamco-rdp-server (moerketh fork)

Releases are published as **GitHub Releases** on `moerketh/lamco-rdp-server`
from tags matching `v<base>-hyperv.<n>`, where `<base>` is the `Cargo.toml`
version (currently `1.4.5`). A tag push triggers `.github/workflows/release.yml`,
which builds the artifacts with `scripts/build-release-artifacts.sh` and
attaches them to a **draft** release; publishing is a manual click after review.

Artifacts per release (x86_64):

| Artifact | Purpose |
|---|---|
| `lamco-rdp-server_<ver>_amd64.deb` | Debian / Ubuntu / Parrot package |
| `lamco-rdp-server-<ver>-linux-x86_64.tar.gz` | Portable tarball with `install.sh` (`/usr/local` prefix) |
| `lamco-rdp-server-<ver>-1-x86_64.pkg.tar.zst` | Arch / Manjaro pacman package (`<ver>-1` = version-pkgrel) |
| `SHA256SUMS.txt` | Checksums for the above |

---

## Checklist (per release)

1. **Version**: decide the tag. Cargo.toml stays at `1.4.5`; the fork lineage
   is `v1.4.5-hyperv.2`, `v1.4.5-hyperv.3`, ... (bump `N` per release — the
   `-hyperv<n>` suffix is used in the released artifacts, but *not* in
   `Cargo.toml` or the binary's `--version`). If Cargo.toml's version ever
   changes, the tag base must match it — the workflow's tag guard enforces
   this and fails otherwise.
2. **CHANGELOG**: add a `## [<base>-hyperv.<N>]` section to `CHANGELOG.md`
   describing the release. The workflow extracts this section for the
   release notes; the fallback chain is the base heading `## [<base>]`,
   then a list of the last 10 commit subjects ("No CHANGELOG.md section
   found"). A missing `.hyperv.N` section does **not** fail the run —
   only the notes are affected — but add the section so the notes are right.
3. **Lockfile discipline** (the v1.4.5-hyperv.6 lesson): if you change
   `[patch.crates-io]`, git dependencies, or any version pins, regenerate
   **and commit** `Cargo.lock` in the same change. CI builds run
   `--locked`; a stale lock silently drops patches (recorded as
   `[[patch.unused]]` by cargo) and `cargo fetch --locked` refuses the
   build with errors the release script now surfaces.
4. **Pre-flight**: run the workflow-logic harness against the tag you
   are about to create (it exercises the tag guard, CHANGELOG notes
   extraction, and the pacman build wiring — run from a real Linux
   checkout such as WSL; it needs the `zstd` and `awk` CLIs and the
   checkout's Cargo.toml/CHANGELOG.md):
   ```bash
   bash scripts/test-release-workflow-logic.sh v1.4.5-hyperv.<N>
   ```
   All checks must pass before tagging.
5. **Commit** any changes to `main` and push.
6. **Tag** (from `main`):
   ```bash
   git tag v1.4.5-hyperv.<N>
   git push origin v1.4.5-hyperv.<N>   # explicit single-tag push — NEVER `git push --tags`
   ```
7. **Watch the workflow** (Actions tab). It: builds (thin LTO, codegen-units=4,
   features `default,vaapi,gui,vsock,websocket,kwin-virtual,x264`), smoke-tests
   `--version` and the `--licenses` output, and creates the **draft** release
   with all artifacts.
8. **Review the draft release**: asset names, SHA256SUMS, notes. Fix notes in
   the GitHub UI if needed. Check that the pacman asset is present
   (`lamco-rdp-server-<ver>-1-x86_64.pkg.tar.zst`, `<ver>-1` = version-pkgrel)
   and that SHA256SUMS.txt lists the deb, tarball, and pacman package.
9. **Validate on the test VM before publishing** (see below).
10. **Publish** the release in the GitHub UI.

## Pre-publish VM validation (<vm-id>)

Download the draft assets, then install on the target VM and verify:

```powershell
# On the Windows host (Hyper-V VM <vm-id>, <vm-ip>):
$key = "<path-to-ssh-key>"
scp -i $key dist/release/lamco-rdp-server_1.4.5-hyperv6_amd64.deb <user>@<vm-ip>:/tmp/
ssh -i $key <user>@<vm-ip>
```

```bash
# On the VM (<user>@<vm>, sudo is passwordless):
sudo apt-get update
sudo apt-get install -y /tmp/lamco-rdp-server_1.4.5-hyperv6_amd64.deb   # resolves deps
lamco-rdp-server --version                                               # must print 1.4.5...
lamco-rdp-server --licenses | head -5                                     # Cisco binary license header
sha256sum -c SHA256SUMS.txt   # if SHA256SUMS.txt was uploaded alongside
```

Expected dependency resolution on Debian-based systems: `libfuse3-3`,
`pipewire`, `xdg-desktop-portal`, `libwayland-client0`, `libxkbcommon0`,
`libpam0g`, `libva2`, `libssl3`, `libdbus-1-3`. If apt reports missing
dependencies, update the static `Depends:` list in
`scripts/build-release-artifacts.sh` and re-tag.

Also verify the tarball path on the VM:
```bash
tar xzf lamco-rdp-server-1.4.5-hyperv6-linux-x86_64.tar.gz
cd lamco-rdp-server-1.4.5-hyperv6-linux-x86_64
sudo ./install.sh
lamco-rdp-server --version          # /usr/local/bin
```

And the pacman path on an Arch-based VM / live system:
```bash
sudo pacman -U lamco-rdp-server-1.4.5-hyperv6-1-x86_64.pkg.tar.zst
lamco-rdp-server --version           # must print 1.4.5...
lamco-rdp-server --licenses | head -5 # Cisco binary license header
```

`pacman -U` resolves the `depends` list encoded in the package — the
`glibc`, `gcc-libs`, `dbus`, `pipewire`, `libpipewire`,
`xdg-desktop-portal`, `wayland`, `libxkbcommon`, `pam`, `fuse3`,
`libva`, `openssl` — and offers the `optdepends` (`x264`, `openh264`,
`vulkan-icd-loader`, portal backends) as optional. If pacman reports
missing dependencies, update the `depend` list in
`scripts/build-release-artifacts.sh` and re-tag.

---

## Notes & gotchas

- **Never `git push --tags`** — always push the single tag explicitly.
  (`git push --tags` would push stale refs.)
- **The OpenH264 binary license file** (`licenses/OpenH264-BINARY_LICENSE.txt`)
  is intentionally NOT tracked in this fork. The release script copies the
  canonical Cisco text from the `openh264-sys2` crate pinned by `Cargo.lock`
  (sha256-verified) at build time; it is compiled into the `--licenses`
  output and shipped in `/usr/share/doc/lamco-rdp-server/`. Never commit it.
- **Draft releases**: assets on a draft release are not publicly visible
  until published. Validate first, publish second.
- **The tag guard** rejects tags whose base doesn't match Cargo.toml —
  e.g. `v1.5.0-hyperv.1` fails while Cargo.toml says `1.4.5`. Bump Cargo.toml
  first if the base version changes.
- **Moving an already-published tag** re-runs the release workflow and the
  previous run's release remains as a draft/record from the old commit —
  re-check that the draft you publish belongs to the *new* tag commit, note
  the move in the release notes, and never rewrite a tag that anyone
  (VMCreate) has already consumed.
- **Stale refs**: after Phase-0-style history cleanups, old tags/branches may
  linger locally. The safety bundle (`../lamco-rdp-server-backup.bundle`)
  predates the v2 rewrite.
- **Local dry-run** without tagging: `bash scripts/build-release-artifacts.sh`
  from a WSL checkout (see README "Building from Source" for system deps).
  Flags: `--skip-build` (reuse an existing `target/release/`), `--skip-deb`,
  `--skip-tarball`, `--skip-pacman`, `--audit-secrets` (opt-in secrets
  spot-check). The pacman package requires the `zstd` CLI (the zstd crate
  only provides a decompressor; `pacman -U` archives are themselves
  zstd-compressed tarballs, compressed at build time by the CLI `zstd` tool).
- **CRLF worktrees**: this repo has `core.autocrlf=true` and no
  `.gitattributes`, so shell scripts check out with CRLF on Windows. Bash
  (WSL) chokes on `\r`. Before running any script, strip CR first:
  `tr -d "\r" < scripts/test-release-workflow-logic.sh > /tmp/t.sh && bash /tmp/t.sh <tag>`