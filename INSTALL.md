# Installation Guide

Quick reference for installing lamco-rdp-server on various Linux distributions.

---

## GitHub Releases (Recommended for this fork)

Prebuilt x86_64 packages are attached to each release on
[moerketh/lamco-rdp-server/releases](https://github.com/moerketh/lamco-rdp-server/releases).
Two formats are provided; pick one.

### Debian / Ubuntu / Parrot (`.deb`)

```bash
# Download the .deb from the release page, then:
sudo apt-get install -y ./lamco-rdp-server_1.4.4-hyperv1_amd64.deb

# Verify checksums (SHA256SUMS.txt from the same release)
sha256sum -c SHA256SUMS.txt
```

This installs both binaries (`/usr/bin/lamco-rdp-server`,
`/usr/bin/lamco-rdp-server-gui`), the systemd user unit, desktop entry,
icons, D-Bus service, and docs. apt resolves runtime dependencies
automatically.

### Any distro (portable tarball)

```bash
tar xzf lamco-rdp-server-1.4.4-hyperv1-linux-x86_64.tar.gz
cd lamco-rdp-server-1.4.4-hyperv1-linux-x86_64
sudo ./install.sh          # default prefix /usr/local
```

`install.sh` copies binaries to `/usr/local/bin`, the systemd unit to
`/usr/local/lib/systemd/user`, icons/desktop/metainfo under `/usr/local/share`,
and prints the next steps.

### Post-install (both formats)

```bash
# 1. Generate TLS certificates (one-time)
sudo lamco-rdp-server-setup-certs
#    (or: lamco-rdp-server --generate-certs)

# 2. Grant portal permissions (one-time, per user, inside your desktop session)
lamco-rdp-server --grant-permission

# 3. Enable the user service
systemctl --user enable --now lamco-rdp-server.service
```

**Encoders:** the release ships both OpenH264 and x264 AVC420 support.
OpenH264 loads Cisco's prebuilt binary (`libopenh264-7` on Debian/Ubuntu).
x264 (2-3x faster, AVC420 only) is used automatically when `libx264`
(`libx264-164`) is installed; otherwise the server falls back to OpenH264.
AVC444 always uses OpenH264. See `lamco-rdp-server --licenses` for the
Cisco OpenH264 binary license terms.

**Hyper-V Enhanced Session** and **WebSocket/RDCleanPath** transports are
opt-in via `[server.transports]` in the config — absent means off.

---

## Flatpak (Universal)

**Works on:** All Linux distributions with Flatpak support

```bash
# Install from Flathub (once published)
flatpak install flathub io.lamco.rdp-server

# Run
flatpak run io.lamco.rdp-server --help
```

**Setup:**
```bash
# Grant permissions (one-time)
flatpak run io.lamco.rdp-server --grant-permission

# Create config (optional)
mkdir -p ~/.var/app/io.lamco.rdp-server/config/lamco-rdp-server
cp config.toml.example ~/.var/app/io.lamco.rdp-server/config/lamco-rdp-server/config.toml
```

---

## Fedora / RHEL / AlmaLinux

**RPM package** (when available from repositories):

```bash
# Install
sudo dnf install lamco-rdp-server

# Generate certificates
sudo lamco-rdp-server-setup-certs

# Enable and start service
systemctl --user enable --now lamco-rdp-server.service

# Grant permissions (one-time)
lamco-rdp-server --grant-permission
```

---

## Ubuntu / Debian

**DEB package** (when available from repositories):

```bash
# Install
sudo apt install lamco-rdp-server

# Generate certificates
sudo lamco-rdp-server-setup-certs

# Enable and start service
systemctl --user enable --now lamco-rdp-server.service

# Grant permissions (one-time)
lamco-rdp-server --grant-permission
```

---

## From Source

### Prerequisites

```bash
# Fedora/RHEL
sudo dnf install rust cargo nasm openssl-devel pipewire-devel libva-devel

# Ubuntu/Debian
sudo apt install cargo nasm libssl-dev libpipewire-0.3-dev libva-dev

# Arch
sudo pacman -S rust nasm openssl pipewire libva
```

### Build and Install

```bash
# Clone repository
git clone https://github.com/moerketh/lamco-rdp-server
cd lamco-rdp-server

# Build (software encoding)
cargo build --release

# Or with hardware encoding
cargo build --release --features hardware-encoding

# Install binary
sudo cp target/release/lamco-rdp-server /usr/local/bin/

# Install systemd service
mkdir -p ~/.config/systemd/user
cp packaging/systemd/lamco-rdp-server.service ~/.config/systemd/user/

# Generate certificates
sudo mkdir -p /etc/lamco-rdp-server
sudo ./scripts/generate-certs.sh /etc/lamco-rdp-server $(hostname)

# Create config
sudo cp config.toml.example /etc/lamco-rdp-server/config.toml

# Enable service
systemctl --user enable --now lamco-rdp-server.service
```

---

## Post-Installation

### Verify Installation

```bash
# Check version
lamco-rdp-server --version

# Check capabilities
lamco-rdp-server --show-capabilities

# Run diagnostics
lamco-rdp-server --diagnose
```

### Grant Permissions

**Required once for unattended operation:**

```bash
lamco-rdp-server --grant-permission
```

This displays the Portal permission dialog. Click "Allow" to grant screen sharing permissions. A restore token will be stored for future automatic operation.

### Test Connection

From a Windows client:
1. Press `Win+R`, type `mstsc`, press Enter
2. Computer: `your-server-ip:3389`
3. Username: Your Linux username
4. Password: Your Linux password
5. Connect

---

## Troubleshooting

### "Certificate not found"

```bash
# Generate certificates
sudo ./scripts/generate-certs.sh /etc/lamco-rdp-server $(hostname)

# Or manually
sudo openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout /etc/lamco-rdp-server/key.pem \
  -out /etc/lamco-rdp-server/cert.pem \
  -days 365 -subj "/CN=$(hostname)"
```

### "Portal permission denied"

```bash
# Check Portal is running
systemctl --user status xdg-desktop-portal

# Install Portal backend for your desktop
sudo apt install xdg-desktop-portal-gnome  # GNOME
sudo apt install xdg-desktop-portal-kde    # KDE
sudo apt install xdg-desktop-portal-wlr    # wlroots (Sway/Hyprland)
```

### "PipeWire connection failed"

```bash
# Check PipeWire is running
systemctl --user status pipewire

# Start if not running
systemctl --user start pipewire wireplumber
```

### Service won't start

```bash
# Check logs
journalctl --user -u lamco-rdp-server -n 50

# Enable linger (keeps service running when not logged in)
sudo loginctl enable-linger $USER
```

---

## Next Steps

- See [README.md](README.md) for configuration options
- See [config.toml.example](config.toml.example) for all settings
- See [CHANGELOG.md](CHANGELOG.md) for version history

**Note:** Some installation methods (RPM/DEB repositories) are not yet available. Use Flatpak or build from source until packages are published to repositories.
