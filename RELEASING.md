# Releasing lamco-rdp-server (moerketh fork)

Releases are published as **GitHub Releases** on `moerketh/lamco-rdp-server`
from tags matching `v<base>-hyperv.<n>`, where `<base>` is the `Cargo.toml`
version (currently `1.4.4`). A tag push triggers `.github/workflows/release.yml`,
which builds the artifacts with `scripts/build-release-artifacts.sh` and
attaches them to a **draft** release; publishing is a manual click after review.

Artifacts per release (x86_64):

| Artifact | Purpose |
|---|---|
| `lamco-rdp-server_<ver>_amd64.deb` | Debian / Ubuntu / Parrot package |
| `lamco-rdp-server-<ver>-linux-x86_64.tar.gz` | Portable tarball with `install.sh` (`/usr/local` prefix) |
| `SHA256SUMS.txt` | Checksums for the above |

---

## Checklist (per release)

1. **Version**: decide the tag. Cargo.toml stays at `1.4.4`; the fork lineage
   is `v1.4.4-hyperv.2`, `v1.4.4-hyperv.3`, ... (bump `N` per release).
   If Cargo.toml's version ever changes, the tag base must match it — the
   workflow's tag guard enforces this and fails otherwise.
2. **CHANGELOG**: add a `## [<base>-hyperv.<N>]` section to `CHANGELOG.md`
   describing the release. The workflow extracts this section for the
   release notes (fallback: last 10 commit subjects).
3. **Commit** any changes to `feature/hyperv-enhanced-session-v2` and push.
4. **Tag** (from the v2 branch):
   ```bash
   git tag v1.4.4-hyperv.<N>
   git push origin v1.4.4-hyperv.<N>   # explicit single-tag push — NEVER `git push --tags`
   ```
5. **Watch the workflow** (Actions tab). It: builds (thin LTO, codegen-units=4,
   features `default,vaapi,gui,vsock,websocket,kwin-virtual,x264`), smoke-tests
   `--version` and the `--licenses` output, and creates the **draft** release
   with all artifacts.
6. **Review the draft release**: asset names, SHA256SUMS, notes. Fix notes in
   the GitHub UI if needed.
7. **Validate on the test VM before publishing** (see below).
8. **Publish** the release in the GitHub UI.

## Pre-publish VM validation (TEST_20260903001934)

Download the draft assets, then install on the target VM and verify:

```powershell
# On the Windows host (Hyper-V VM TEST_20260903001934, 172.23.88.99):
$key = "$env:LOCALAPPDATA\VMCreate\ssh\vmcreate_ed25519"
scp -i $key dist/release/lamco-rdp-server_1.4.4-hyperv1_amd64.deb vmcreate@172.23.88.99:/tmp/
ssh -i $key vmcreate@172.23.88.99
```

```bash
# On the VM (vmcreate@parrot, sudo is passwordless):
sudo apt-get update
sudo apt-get install -y /tmp/lamco-rdp-server_1.4.4-hyperv1_amd64.deb   # resolves deps
lamco-rdp-server --version                                               # must print 1.4.4...
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
tar xzf lamco-rdp-server-1.4.4-hyperv1-linux-x86_64.tar.gz
cd lamco-rdp-server-1.4.4-hyperv1-linux-x86_64
sudo ./install.sh
lamco-rdp-server --version          # /usr/local/bin
```

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
  e.g. `v1.5.0-hyperv.1` fails while Cargo.toml says `1.4.4`. Bump Cargo.toml
  first if the base version changes.
- **Stale refs**: after Phase-0-style history cleanups, old tags/branches may
  linger locally. The safety bundle (`../lamco-rdp-server-backup.bundle`)
  predates the v2 rewrite.
- **Local dry-run** without tagging: `bash scripts/build-release-artifacts.sh`
  from a WSL checkout (see INSTALL.md "From source" for system deps).
- **Deleting the old pre-v2 branch**: once this lineage is verified across a
  release or two, `git push origin --delete feature/hyperv-enhanced-session`
  (the pre-rewrite backup) is safe.