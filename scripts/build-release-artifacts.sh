#!/usr/bin/env bash
# Build release artifacts for lamco-rdp-server.
#
# Produces (under dist/release/):
#   lamco-rdp-server_<ver>_amd64.deb          — Debian/Ubuntu/Parrot package
#   lamco-rdp-server-<ver>-linux-x86_64.tar.gz — portable binary tarball with install.sh
#   SHA256SUMS.txt                            — checksums for all artifacts
#
# <ver> comes from --version (default: Cargo.toml version + -hyperv1 suffix,
# overridable via LAMCO_RELEASE_VERSION or the first CLI arg).
#
# Run identically from a local WSL checkout or a GitHub Actions runner:
#   scripts/build-release-artifacts.sh [--version 1.4.4-hyperv1] [--features ...]
#
# Exit codes: 0 success; 1 usage; 2 missing dependency; 3 build failure;
# 4 packaging failure; 5 license gate failure.

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$PWD"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
FEATURES="${LAMCO_RELEASE_FEATURES:-default,vaapi,gui,vsock,websocket,kwin-virtual,x264}"
LAMCO_RELEASE_VERSION="${LAMCO_RELEASE_VERSION:-}"
ARCH="$(uname -m)"
DIST_DIR="${REPO_ROOT}/dist/release"

# Staging happens on a NATIVE-filesystem temp dir, never the repo path: on WSL
# /mnt/c (drvfs) every file reads as mode 777, which dpkg-deb rejects for the
# DEBIAN control directory ("bad permissions 777") and which would otherwise
# leak 777 modes into the deb/tarball entries. mktemp -d gives /tmp (ext4)
# under WSL and a native tmp under CI.
STAGE_ROOT="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT"' EXIT
BUILD_PROFILE_OVERRIDES=(
  "CARGO_PROFILE_RELEASE_LTO=thin"
  "CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4"
)

# The Cisco OpenH264 binary license (BINARY_LICENSE.txt). The file is NOT
# tracked in this repository (upstream's responsibility). It ships inside the
# openh264-sys2 crate that Cargo.lock already pins, so we copy it from the
# local cargo registry at build time. It is compiled into the binary via
# include_str! (src/third_party.rs) as required by condition 4 of that
# license, and must never be committed.
# CR-normalized sha256 of the canonical text (identical in openh264-sys2
# 0.9.6 and 0.9.7):
OPENH264_LICENSE_SHA256="bd9f363c5ea11ef723d0304cddacb5273c43c0e1194097c7a045d05273635418"
LICENSE_FILE="licenses/OpenH264-BINARY_LICENSE.txt"

usage() {
  cat <<EOF
Usage: $0 [--version <ver>] [--features <csv>] [--skip-build] [--skip-deb] [--skip-tarball] [--audit-secrets]
Options:
  --version <ver>    Package version (default: Cargo.toml version + '-hyperv1')
  --features <csv>    Cargo features to build (default: $FEATURES)
  --skip-build       Reuse an existing target/release build
  --skip-deb         Do not produce the .deb
  --skip-tarball     Do not produce the tarball
  --audit-secrets    Spot-check binaries + staged packages for secret-shaped
                     strings (credential assignments, GitHub/AWS/Slack/Stripe
                     token formats, embedded PEM bodies). Off by default; run
                     it whenever you want a one-off review before publishing.
  --help             Show this help
Environment:
  LAMCO_RELEASE_VERSION   Same as --version
  LAMCO_RELEASE_FEATURES  Same as --features
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
SKIP_BUILD=0
SKIP_DEB=0
SKIP_TARBALL=0
AUDIT_SECRETS=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || { echo "error: --version needs a value" >&2; exit 1; }
      LAMCO_RELEASE_VERSION="$2"; shift 2 ;;
    --features)
      [[ $# -ge 2 ]] || { echo "error: --features needs a value" >&2; exit 1; }
      FEATURES="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-deb) SKIP_DEB=1; shift ;;
    --skip-tarball) SKIP_TARBALL=1; shift ;;
    --audit-secrets) AUDIT_SECRETS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { printf '\033[1;36m[release]\033[0m %s\n' "$*"; }
err()  { printf '\033[1;31m[release:ERROR]\033[0m %s\n' "$*" >&2; }
die()  { err "$2"; exit "$1"; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die 2 "missing dependency: $1 (install it and re-run)"
}

# ---------------------------------------------------------------------------
# 1. OpenH264 binary license gate (src/third_party.rs include_str! target)
# ---------------------------------------------------------------------------
ensure_openh264_license() {
  local need_fetch=0
  if [[ ! -f "$LICENSE_FILE" ]]; then
    need_fetch=1
  elif grep -q "PLACEHOLDER" "$LICENSE_FILE" 2>/dev/null; then
    log "existing $LICENSE_FILE is a placeholder — replacing with canonical Cisco text"
    need_fetch=1
  else
    # Already present and not the placeholder: trust it (dev machines may
    # have the real file; sha check below only runs on copy).
    log "$LICENSE_FILE present"
  fi

  if [[ "$need_fetch" -eq 1 ]]; then
    mkdir -p licenses
    local src=""
    # Preferred source: the openh264-sys2 crate pinned by Cargo.lock (its
    # tests/reference dir ships the canonical Cisco license text).
    local cargo_src
    cargo_src="$(find "$HOME/.cargo/registry/src" -path '*openh264-sys2*/tests/reference/BINARY_LICENSE.txt' 2>/dev/null | sort -V | tail -1 || true)"

    if [[ -z "$cargo_src" ]] && command -v cargo >/dev/null 2>&1; then
      # Fresh checkout: fetch dependencies so the pinned crate is available
      # locally before the compile step needs this file.
      log "populating cargo registry (cargo fetch --locked)"
      cargo fetch --locked >/dev/null 2>&1 || log "cargo fetch reported issues; continuing"
      cargo_src="$(find "$HOME/.cargo/registry/src" -path '*openh264-sys2*/tests/reference/BINARY_LICENSE.txt' 2>/dev/null | sort -V | tail -1 || true)"
    fi

    if [[ -n "$cargo_src" ]]; then
      cp "$cargo_src" "$LICENSE_FILE"
    else
      # Fallback: extract from the cached .crate tarball directly.
      local crate_file
      crate_file="$(ls "$HOME"/.cargo/registry/cache/*/openh264-sys2-*.crate 2>/dev/null | sort -V | tail -1 || true)"
      if [[ -n "$crate_file" ]]; then
        log "extracting license from $(basename "$crate_file")"
        tar -xzOf "$crate_file" --wildcards '*/tests/reference/BINARY_LICENSE.txt' > "$LICENSE_FILE" \
          || die 5 "could not extract BINARY_LICENSE.txt from $crate_file"
      elif [[ -n "${OPENH264_LICENSE_URL:-}" ]]; then
        # Last resort: explicit URL (the sha256 pin below still applies).
        require_cmd curl
        curl -fsSL "$OPENH264_LICENSE_URL" -o "$LICENSE_FILE" || die 5 "failed to fetch OpenH264 binary license from $OPENH264_LICENSE_URL"
      else
        die 5 "no source for $LICENSE_FILE: openh264-sys2 crate unavailable and OPENH264_LICENSE_URL unset"
      fi
    fi

    local actual
    actual="$(tr -d '\r' < "$LICENSE_FILE" | sha256sum | cut -d' ' -f1)"
    [[ "$actual" == "$OPENH264_LICENSE_SHA256" ]] || {
      rm -f "$LICENSE_FILE"
      die 5 "OpenH264 license sha256 mismatch (got $actual, want $OPENH264_LICENSE_SHA256) — refusing to distribute a non-compliant build"
    }
    log "installed canonical Cisco OpenH264 binary license (sha256 verified)"
  fi
}

# ---------------------------------------------------------------------------
# 2. Dependency verification
# ---------------------------------------------------------------------------
PKGS_DEB=(clang cmake libdbus-1-dev libfuse3-dev libpam0g-dev libpipewire-0.3-dev libspa-0.2-dev libssl-dev libva-dev libwayland-dev libxkbcommon-dev nasm libx264-dev)
verify_deps() {
  log "verifying build dependencies"
  require_cmd cargo
  require_cmd cc || true   # gcc/clang
  for c in curl sha256sum dpkg-deb; do require_cmd "$c"; done

  if command -v dpkg >/dev/null 2>&1 && command -v pkg-config >/dev/null 2>&1; then
    local missing=()
    while IFS= read -r p; do
      [[ -n "$p" ]] || continue
      dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
    done < <(printf '%s\n' "${PKGS_DEB[@]}")
    if [[ ${#missing[@]} -gt 0 ]]; then
      err "missing system packages: ${missing[*]}"
      err "install with: sudo apt-get install ${missing[*]}"
      die 2 "build dependencies unmet"
    fi
  fi
  log "dependency check OK"
}

# ---------------------------------------------------------------------------
# 2b. Secrets audit — OPT-IN spot check (--audit-secrets)
# ---------------------------------------------------------------------------
# One-off review tool, not a build gate. Scans the release binaries and
# staged package trees for secret-shaped content. Run it whenever you
# want to verify the artifacts before publishing a release.
#


# What counts as a hit:
#   - Credential-bearing assignments (PASSWORD=..., TOKEN=..., SECRET=...,
#     APIKEY=...) with a non-empty literal value
#   - GitHub tokens (ghp_, gho_, ghu_, ghs_, ghr_, github_pat_)
#   - AWS access keys (AKIA...), Slack tokens (xox[abprs]-), Stripe (sk_live_)
#   - Authorization headers with long literal values
#   - PEM (PRIVATE KEY / CERTIFICATE) ONLY when a header is followed by
#     base64 body material — a real embedded key/cert. Bare header strings
#     in a binary are cert-validation error messages and pass.
# Deliberately NOT flagged (reviewed and accepted):
#   - example-config.toml placeholders ("your-password") — docs, not secrets
#   - Certificate/key PATHS (key_path = "/etc/...") — path, not value
#   - Config KEY NAMES (security.key_path) — names, not values
#
# The scan is a backstop, not a substitute for not putting secrets in the
# tree. Known plaintext values used by tests (none today) would go here.
SECRET_SCAN_ALLOW_PATTERNS=(
  "your-password"
  "Check file starts with"
  "must be a valid"
  "Recommended: chmod 600"
)

# Scan one file for secret-looking content. Prints findings; returns nonzero
# if any hit is not allow-listed.
scan_file_for_secrets() {  # $1 = file, $2 = label
  local file="$1" label="$2" hits=0
  local tmp
  tmp="$(mktemp)"
  {
    strings -- "$file" 2>/dev/null | grep -nE \
      -e '(PASSWORD|PASSWD|TOKEN|SECRET|APIKEY|API_KEY|ACCESS_KEY|CLIENT_SECRET)["'"'"']?\s*[=:]\s*["'"'"']?[A-Za-z0-9+/_=@.~!?-]{8,}' \
      -e 'gh[pousr]_[A-Za-z0-9]{20,}' \
      -e 'github_pat_[A-Za-z0-9]{20,}' \
      -e 'AKIA[0-9A-Z]{16}' \
      -e 'xox[abprs]-[A-Za-z0-9-]{10,}' \
      -e 'sk_live_[A-Za-z0-9]{20,}' \
      -e 'Authorization: (Bearer|Basic) [A-Za-z0-9+/=._-]{16,}'
  } >> "$tmp" 2>/dev/null || true

  # PEM body detection: header followed within a few lines by 40+ base64
  # chars means a real embedded key/cert; lone header strings (validation
  # error text) have no body and stay clean.
  if strings -- "$file" 2>/dev/null \
     | grep -A4 -- '-----BEGIN [A-Z ]*PRIVATE KEY\|-----BEGIN CERTIFICATE' \
     | grep -qE '^[A-Za-z0-9+/]{40,}={0,2}$'; then
    echo "PEM body material found after BEGIN header" >> "$tmp"
  fi

  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    local allowed=0
    local ap
    for ap in "${SECRET_SCAN_ALLOW_PATTERNS[@]}"; do
      if [[ "$line" == *"$ap"* ]]; then allowed=1; break; fi
    done
    if [[ $allowed -eq 1 ]]; then
      log "  (allowed) $label: $line"
    else
      err "SECRETS SCAN HIT in $label: $line"
      hits=1
    fi
  done < "$tmp"
  rm -f "$tmp"
  [[ $hits -eq 0 ]]
}

audit_secrets() {
  log "secrets audit: scanning release binaries"
  local failed=0
  local bin
  for bin in target/release/lamco-rdp-server target/release/lamco-rdp-server-gui; do
    if [[ -f "$bin" ]]; then
      scan_file_for_secrets "$bin" "$(basename "$bin")" || failed=1
    fi
  done

  log "secrets audit: scanning staged package trees"
  local tree
  for tree in "$STAGE_ROOT/deb-stage" "$STAGE_ROOT/tarball-stage"; do
    if [[ -d "$tree" ]]; then
      while IFS= read -r -d '' f; do
        scan_file_for_secrets "$f" "${tree##*/}/${f#"$tree"/}" || failed=1
      done < <(find "$tree" -type f -print0)
    fi
  done

  if [[ $failed -ne 0 ]]; then
    die 5 "secrets audit FAILED — see findings above; remove the secret or (if reviewed) add an allow-pattern to SECRET_SCAN_ALLOW_PATTERNS"
  fi
  log "secrets audit: clean"
}

# ---------------------------------------------------------------------------
# 3. Version resolution
# ---------------------------------------------------------------------------
resolve_version() {
  if [[ -z "$LAMCO_RELEASE_VERSION" ]]; then
    local cargo_ver
    cargo_ver="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
    [[ -n "$cargo_ver" ]] || die 1 "could not read version from Cargo.toml"
    LAMCO_RELEASE_VERSION="${cargo_ver}-hyperv1"
    log "version defaulting to Cargo.toml + suffix: ${LAMCO_RELEASE_VERSION}"
  fi
}

# ---------------------------------------------------------------------------
# 4. Build
# ---------------------------------------------------------------------------
do_build() {
  log "building release binaries (features: $FEATURES)"
  log "profile overrides: ${BUILD_PROFILE_OVERRIDES[*]}"
  env "${BUILD_PROFILE_OVERRIDES[@]}" \
    cargo build --release --locked --features "$FEATURES" \
    || die 3 "cargo build failed"
  [[ -f target/release/lamco-rdp-server ]] || die 3 "server binary missing after build"
  [[ -f target/release/lamco-rdp-server-gui ]] || die 3 "GUI binary missing after build (gui feature required)"
  log "build OK"
}

# ---------------------------------------------------------------------------
# 5. Shared install-set staging (mirrors packaging/debian/rules)
# ---------------------------------------------------------------------------
stage_install_set() {  # $1 = destination root
  local dest="$1"
  install -Dm755 target/release/lamco-rdp-server     "$dest/usr/bin/lamco-rdp-server"
  install -Dm755 target/release/lamco-rdp-server-gui "$dest/usr/bin/lamco-rdp-server-gui"
  install -Dm644 packaging/systemd/lamco-rdp-server.service "$dest/usr/lib/systemd/user/lamco-rdp-server.service"
  install -Dm644 packaging/dbus/io.lamco.RdpServer.service "$dest/usr/share/dbus-1/services/io.lamco.RdpServer.service"
  install -Dm644 packaging/dbus/io.lamco.RdpServer.System.conf "$dest/usr/share/dbus-1/system.d/io.lamco.RdpServer.System.conf"
  install -Dm644 packaging/polkit/io.lamco.RdpServer.policy "$dest/usr/share/polkit-1/actions/io.lamco.RdpServer.policy"
  install -dm755 "$dest/etc/lamco-rdp-server"
  install -Dm644 example-config.toml "$dest/usr/share/doc/lamco-rdp-server/examples/example-config.toml"
  install -Dm644 INSTALL.md "$dest/usr/share/doc/lamco-rdp-server/INSTALL.md"
  gzip -9n "$dest/usr/share/doc/lamco-rdp-server/INSTALL.md"
  install -Dm644 licenses/OpenH264-BINARY_LICENSE.txt "$dest/usr/share/doc/lamco-rdp-server/OpenH264-BINARY_LICENSE.txt"
  install -Dm644 LICENSE "$dest/usr/share/doc/lamco-rdp-server/LICENSE"
  install -Dm644 data/io.lamco.rdp-server.desktop "$dest/usr/share/applications/io.lamco.rdp-server.desktop"
  install -Dm644 data/io.lamco.rdp-server.metainfo.xml "$dest/usr/share/metainfo/io.lamco.rdp-server.metainfo.xml"
  install -Dm644 data/icons/io.lamco.rdp-server.svg "$dest/usr/share/icons/hicolor/scalable/apps/io.lamco.rdp-server.svg"
  local size
  for size in 32 48 64 128 256; do
    [[ -f "data/icons/io.lamco.rdp-server-$size.png" ]] || continue
    install -Dm644 "data/icons/io.lamco.rdp-server-$size.png" "$dest/usr/share/icons/hicolor/${size}x${size}/apps/io.lamco.rdp-server.png"
  done
}

# ---------------------------------------------------------------------------
# 6. .deb assembly (dpkg-deb staging; packaging/debian/ stays untouched)
# ---------------------------------------------------------------------------
build_deb() {
  local stage="${STAGE_ROOT}/deb-stage"
  local out="${DIST_DIR}/lamco-rdp-server_${LAMCO_RELEASE_VERSION}_amd64.deb"
  rm -rf "$stage"
  mkdir -p "$stage/DEBIAN" "$stage/usr"
  stage_install_set "$stage"

  # Static runtime dependency list. An Ubuntu-built shlibs scan would emit
  # t64-suffixed names that do not exist on Debian/Parrot; the VM install
  # test (RELEASING.md) validates this list empirically.
  cat > "$stage/DEBIAN/control" <<EOF
Package: lamco-rdp-server
Version: ${LAMCO_RELEASE_VERSION}
Section: non-free/net
Priority: optional
Architecture: amd64
Maintainer: Greg Lamberson <greg@lamco.io>
Depends: libfuse3-3 | libfuse3-4, pipewire, xdg-desktop-portal, libwayland-client0, libxkbcommon0, libpam0g, libva2, libssl3 | libssl3t64, libdbus-1-3
Recommends: intel-media-va-driver | mesa-va-drivers, libva2
Suggests: libx264-164 | libx264
Conflicts: lamco-rdp-server-gui
Replaces: lamco-rdp-server-gui
Description: Native Wayland RDP server for Linux desktop sharing
 lamco-rdp-server is a high-performance RDP server for Wayland-based
 Linux desktops. It supports multiple screen capture and input backends:
 xdg-desktop-portal for GNOME and KDE, native wlroots protocols for
 Sway and Hyprland, and KWin zkde-screencast virtual outputs on KDE
 Plasma 6+, with automatic capability detection.
 .
 Features include H.264 video encoding (AVC420/AVC444), hardware-
 accelerated encoding via VA-API, multi-monitor support, clipboard
 synchronization, Hyper-V Enhanced Session transport (opt-in), and a
 graphical configuration GUI.
 .
 Built on IronRDP. Works with any standard RDP client.
EOF

  # Maintainer scripts: refresh icon/systemd/dbus caches on install.
  cat > "$stage/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ -x /usr/bin/gtk-update-icon-cache ]; then
  /usr/bin/gtk-update-icon-cache -q /usr/share/icons/hicolor || true
fi
systemctl --user daemon-reload >/dev/null 2>&1 || true
echo "lamco-rdp-server installed."
echo "Next steps:"
echo "  1. Generate certificates:  sudo lamco-rdp-server-setup-certs"
echo "     (or: lamco-rdp-server --generate-certs)"
echo "  2. Grant permissions:      lamco-rdp-server --grant-permission"
echo "  3. Enable the service:     systemctl --user enable --now lamco-rdp-server.service"
exit 0
EOF
  cat > "$stage/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
systemctl --user daemon-reload >/dev/null 2>&1 || true
exit 0
EOF
  chmod 755 "$stage/DEBIAN/postinst" "$stage/DEBIAN/postrm"

  mkdir -p "$DIST_DIR"
  dpkg-deb --build --root-owner-group "$stage" "$out" || die 4 "dpkg-deb build failed"
  rm -rf "$stage"
  log "built $(basename "$out")"
}

# ---------------------------------------------------------------------------
# 7. Portable tarball assembly (install.sh, /usr/local prefix)
# ---------------------------------------------------------------------------
build_tarball() {
  local stage="${STAGE_ROOT}/tarball-stage"
  local tar_root="lamco-rdp-server-${LAMCO_RELEASE_VERSION}-linux-${ARCH}"
  local out="${DIST_DIR}/lamco-rdp-server-${LAMCO_RELEASE_VERSION}-linux-${ARCH}.tar.gz"
  rm -rf "$stage"
  mkdir -p "$stage/$tar_root"
  stage_install_set "$stage/$tar_root/root"
  # Tarball layout: bin/ etc/ share/ at top level + install.sh; move the
  # staged usr tree into place.
  rm -rf "$stage/$tar_root/root/usr/lib"
  mv "$stage/$tar_root/root/usr/bin" "$stage/$tar_root/bin"
  mv "$stage/$tar_root/root/usr/lib" "$stage/$tar_root/lib" 2>/dev/null || true
  mkdir -p "$stage/$tar_root/lib/systemd/user"
  install -Dm644 packaging/systemd/lamco-rdp-server.service "$stage/$tar_root/lib/systemd/user/lamco-rdp-server.service"
  mv "$stage/$tar_root/root/etc" "$stage/$tar_root/etc"
  mkdir -p "$stage/$tar_root/share"
  mv "$stage/$tar_root/root/usr/share/"* "$stage/$tar_root/share/" 2>/dev/null || true
  rm -rf "$stage/$tar_root/root"
  install -Dm644 example-config.toml "$stage/$tar_root/share/doc/examples/example-config.toml"
  install -Dm644 licenses/OpenH264-BINARY_LICENSE.txt "$stage/$tar_root/share/doc/OpenH264-BINARY_LICENSE.txt"
  install -Dm644 LICENSE "$stage/$tar_root/share/doc/LICENSE"

  cat > "$stage/$tar_root/install.sh" <<'EOF'
#!/usr/bin/env bash
# Install lamco-rdp-server from the portable tarball.
# Default prefix: /usr/local (systemd unit under /usr/local/lib/systemd/user)
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "This script must run as root: sudo ./install.sh [--prefix /usr/local]" >&2
  exit 1
fi

PREFIX="/usr/local"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

SRC="$(cd "$(dirname "$0")" && pwd)"
[[ -d "$SRC/systemd" ]] && SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-$PREFIX/lib/systemd/user}"
SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-$PREFIX/lib/systemd/user}"

echo "Installing lamco-rdp-server to $PREFIX ..."

install -Dm755 "$SRC/bin/lamco-rdp-server"      "$PREFIX/bin/lamco-rdp-server"
install -Dm755 "$SRC/bin/lamco-rdp-server-gui"  "$PREFIX/bin/lamco-rdp-server-gui"
install -dm755 "$PREFIX/etc/lamco-rdp-server"

# Icons + desktop + metainfo + docs
for size in 32 48 64 128 256; do
  f="$SRC/share/icons/hicolor/${size}x${size}/apps/io.lamco.rdp-server.png"
  [[ -f "$f" ]] && install -Dm644 "$f" "$PREFIX/share/icons/hicolor/${size}x${size}/apps/io.lamco-rdp-server.png"
done
f="$SRC/share/icons/hicolor/scalable/apps/io.lamco.rdp-server.svg"
[[ -f "$f" ]] && install -Dm644 "$f" "$PREFIX/share/icons/hicolor/scalable/apps/io.lamco.rdp-server.svg"
[[ -f "$SRC/share/applications/io.lamco.rdp-server.desktop" ]] && \
  install -Dm644 "$SRC/share/applications/io.lamco.rdp-server.desktop" "$PREFIX/share/applications/io.lamco.rdp-server.desktop"
[[ -f "$SRC/share/metainfo/io.lamco.rdp-server.metainfo.xml" ]] && \
  install -Dm644 "$SRC/share/metainfo/io.lamco.rdp-server.metainfo.xml" "$PREFIX/share/metainfo/io.lamco.rdp-server.metainfo.xml"
[[ -f "$SRC/share/doc/examples/example-config.toml" ]] && \
  install -Dm644 "$SRC/share/doc/examples/example-config.toml" "$PREFIX/share/doc/lamco-rdp-server/examples/example-config.toml"
[[ -f "$SRC/share/doc/OpenH264-BINARY_LICENSE.txt" ]] && \
  install -Dm644 "$SRC/share/doc/OpenH264-BINARY_LICENSE.txt" "$PREFIX/share/doc/lamco-rdp-server/OpenH264-BINARY_LICENSE.txt"
[[ -f "$SRC/share/doc/LICENSE" ]] && \
  install -Dm644 "$SRC/share/doc/LICENSE" "$PREFIX/share/doc/lamco-rdp-server/LICENSE"

# systemd user unit — respects SYSTEMD_UNIT_DIR for /usr systems
unit_src="$SRC/lib/systemd/user/lamco-rdp-server.service"
[[ -f "$unit_src" ]] && install -Dm644 "$unit_src" "$SYSTEMD_UNIT_DIR/lamco-rdp-server.service"

echo
echo "Install complete. Next steps:"
echo "  1. Generate certificates:"
echo "       sudo lamco-rdp-server-setup-certs"
echo "     or create your own under /etc/lamco-rdp-server/"
echo "  2. Grant portal permissions (one-time, per user):"
echo "       $PREFIX/bin/lamco-rdp-server --grant-permission"
echo "  3. Enable the user service:"
echo "       systemctl --user enable --now lamco-rdp-server.service"
echo
echo "NOTE: x264 fast AVC420 encoding needs libx264 installed (libx264-164 /"
echo "libx264). Without it, OpenH264 is used (install libopenh264-7)."
EOF
  chmod 755 "$stage/$tar_root/install.sh"

  mkdir -p "$DIST_DIR"
  tar -C "$stage" -czf "$out" "$tar_root" || die 4 "tarball creation failed"
  rm -rf "$stage"
  log "built $(basename "$out")"
}

# ---------------------------------------------------------------------------
# 8. Checksums
# ---------------------------------------------------------------------------
write_checksums() {
  ( cd "$DIST_DIR" && sha256sum ./*.deb ./*.tar.gz > SHA256SUMS.txt 2>/dev/null ) || true
  log "wrote SHA256SUMS.txt"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  ensure_openh264_license
  verify_deps
  resolve_version
  log "package version: ${LAMCO_RELEASE_VERSION}"
  [[ "$SKIP_BUILD" -eq 0 ]] && do_build
  [[ "$SKIP_DEB" -eq 0 ]] && build_deb
  [[ "$SKIP_TARBALL" -eq 0 ]] && build_tarball
  # Opt-in spot check (--audit-secrets); off by default.
  [[ "$AUDIT_SECRETS" -eq 1 ]] && audit_secrets
  write_checksums
  log "artifacts in: ${DIST_DIR}"
  if [[ -d "$DIST_DIR" ]]; then
    ls -lh "$DIST_DIR"
  else
    log "no artifacts produced (all steps skipped)"
  fi
}

main "$@"