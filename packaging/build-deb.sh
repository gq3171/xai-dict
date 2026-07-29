#!/usr/bin/env bash
# Build an amd64 .deb for xai-dict (app + workers; models NOT included).
#
# Layout intended for "dpkg -i && reboot/login just works":
#   /usr/bin/xai-dict
#   /usr/lib/xai-dict/{qwen3,zipformer}_worker + bundled .so
#   /usr/lib/systemd/user/xai-dict.service   (WantedBy=default.target)
#   /etc/xdg/autostart/xai-dict.desktop      (calls ensure if unit not up)
#   postinst enables service for SUDO_USER when session bus is available
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')}"
ARCH="${ARCH:-amd64}"
PKG_NAME="xai-dict"
STAGE="${ROOT}/target/deb-stage"
OUT_DIR="${ROOT}/target/debian"
DEB="${OUT_DIR}/${PKG_NAME}_${VERSION}_${ARCH}.deb"

echo "==> version=${VERSION} arch=${ARCH}"

# --- build -----------------------------------------------------------------
export CARGO_TERM_COLOR=always
cargo build --release

BIN="${ROOT}/target/release/xai-dict"
QWEN_W="${ROOT}/target/release/qwen3_worker"
ZIP_W="${ROOT}/target/release/zipformer_worker"

if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN" >&2
  exit 1
fi
if [[ ! -x "$QWEN_W" || ! -x "$ZIP_W" ]]; then
  echo "workers missing — need gcc + libsherpa-onnx-c-api + onnxruntime" >&2
  exit 1
fi

# --- stage tree ------------------------------------------------------------
rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN"
mkdir -p "$STAGE/usr/bin"
mkdir -p "$STAGE/usr/lib/xai-dict"
mkdir -p "$STAGE/usr/share/xai-dict"
mkdir -p "$STAGE/usr/share/applications"
mkdir -p "$STAGE/usr/share/doc/${PKG_NAME}"
mkdir -p "$STAGE/usr/lib/systemd/user"
mkdir -p "$STAGE/etc/xdg/autostart"

install -m755 "$BIN" "$STAGE/usr/bin/xai-dict"
install -m755 "$QWEN_W" "$STAGE/usr/lib/xai-dict/qwen3_worker"
install -m755 "$ZIP_W" "$STAGE/usr/lib/xai-dict/zipformer_worker"
install -m755 "$ROOT/scripts/settings_gui.py" "$STAGE/usr/share/xai-dict/settings_gui.py"
if [[ -f "$ROOT/scripts/osd_bar.py" ]]; then
  install -m755 "$ROOT/scripts/osd_bar.py" "$STAGE/usr/share/xai-dict/osd_bar.py"
fi
# fcitx helper script path (source tree copy for install --fcitx)
if [[ -d "$ROOT/fcitx5-xaidict" ]]; then
  mkdir -p "$STAGE/usr/share/xai-dict/fcitx5-xaidict"
  cp -a "$ROOT/fcitx5-xaidict/." "$STAGE/usr/share/xai-dict/fcitx5-xaidict/"
  # drop local build artifacts from the package
  rm -rf "$STAGE/usr/share/xai-dict/fcitx5-xaidict/build" 2>/dev/null || true
fi

# Bundle non-glibc runtime libs needed by workers (portable Release)
bundle_libs() {
  local bin="$1"
  local dest="$2"
  local libs
  libs=$(ldd "$bin" | awk '/=> \// {print $3}' | sort -u)
  for lib in $libs; do
    base=$(basename "$lib")
    case "$base" in
      libc.so*|libm.so*|libdl.so*|librt.so*|libpthread.so*|ld-linux*|libgcc_s.so*|libstdc++.so*)
        continue
        ;;
    esac
    if [[ -f "$lib" ]]; then
      cp -aL "$lib" "$dest/" 2>/dev/null || cp -a "$lib" "$dest/"
      chmod 755 "$dest/$base" 2>/dev/null || true
      echo "  bundled $base"
    fi
  done
}

echo "==> bundling worker shared libraries into /usr/lib/xai-dict"
bundle_libs "$QWEN_W" "$STAGE/usr/lib/xai-dict"
bundle_libs "$ZIP_W" "$STAGE/usr/lib/xai-dict"

# desktop entries
cat > "$STAGE/usr/share/applications/xai-dict.desktop" <<EOF
[Desktop Entry]
Name=xai-dict
Comment=Voice dictation (local Qwen3 / streaming ASR)
Exec=/usr/bin/xai-dict whoami
Icon=audio-input-microphone
Terminal=true
Type=Application
Categories=Utility;AudioVideo;
EOF

cat > "$STAGE/usr/share/applications/xai-dict-settings.desktop" <<EOF
[Desktop Entry]
Name=xai-dict Settings
Name[zh_CN]=xai-dict 设置
Comment=Configure xai-dict voice dictation
Exec=env XAI_DICT_SETTINGS_GUI=/usr/share/xai-dict/settings_gui.py /usr/bin/xai-dict config gui
Icon=preferences-desktop-multimedia
Terminal=false
Type=Application
Categories=Settings;Utility;AudioVideo;
StartupNotify=true
EOF

cat > "$STAGE/usr/share/applications/xai-dict-toggle.desktop" <<EOF
[Desktop Entry]
Name=xai-dict Toggle
Comment=Start/stop voice dictation
Exec=/usr/bin/xai-dict toggle
Icon=audio-input-microphone
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=false
EOF

# Login autostart: ensure user unit is up (covers desktops that skip default.target bits)
cat > "$STAGE/etc/xdg/autostart/xai-dict.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=xai-dict
Name[zh_CN]=xai-dict 语音听写
Comment=Ensure voice dictation daemon is running
Exec=/usr/bin/xai-dict ensure --quiet
Icon=audio-input-microphone
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
X-KDE-autostart-phase=2
EOF

# systemd user unit — ALWAYS /usr/bin (never ~/.cargo)
cat > "$STAGE/usr/lib/systemd/user/xai-dict.service" <<'EOF'
[Unit]
Description=xai-dict voice dictation daemon
Documentation=https://github.com/gq3171/xai-dict
After=pipewire.service pipewire-pulse.service graphical-session.target
Wants=pipewire.service

[Service]
Type=simple
ExecStart=/usr/bin/xai-dict daemon
Restart=on-failure
RestartSec=2
# Bundled sherpa/onnx next to workers
Environment=RUST_LOG=info
Environment=PATH=/usr/local/bin:/usr/bin:/bin
Environment=LD_LIBRARY_PATH=/usr/lib/xai-dict
Environment=XAI_DICT_LIBDIR=/usr/lib/xai-dict
Environment=XAI_DICT_SETTINGS_GUI=/usr/share/xai-dict/settings_gui.py
KillMode=process

[Install]
WantedBy=default.target
EOF

cat > "$STAGE/usr/share/doc/${PKG_NAME}/README.Debian" <<'EOF'
xai-dict
========

Models are NOT included (download after install).

Typical install:

  sudo dpkg -i xai-dict_*.deb
  # postinst tries to enable the user service for SUDO_USER

  xai-dict config          # GUI: download models + hotkeys
  sudo usermod -aG input $USER   # for Right-Alt global hotkey; re-login

  # After reboot / login, daemon should start via systemd --user
  systemctl --user status xai-dict

If the service is not active:

  systemctl --user daemon-reload
  systemctl --user enable --now xai-dict
  # or:
  xai-dict ensure

Docs: https://github.com/gq3171/xai-dict
EOF

# control
INSTALLED_SIZE=$(du -sk "$STAGE" | awk '{print $1}')
cat > "$STAGE/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: sound
Priority: optional
Architecture: ${ARCH}
Maintainer: xai-dict contributors <noreply@users.noreply.github.com>
Installed-Size: ${INSTALLED_SIZE}
Depends: libc6, libgcc-s1 | libgcc1, libstdc++6, libnotify-bin, python3, pipewire-bin | pulseaudio-utils | alsa-utils
Recommends: python3-pyqt6 | python3-pyqt5, wl-clipboard, fcitx5, ydotool
Suggests: cmake, fcitx5-modules-dev | fcitx5-module-dev
Homepage: https://github.com/gq3171/xai-dict
Description: System voice dictation (local Qwen3-ASR + streaming preedit)
 LazyTyper-style global hotkey dictation for Linux. Ships the daemon binary
 and sherpa-onnx worker helpers. ASR model weights are downloaded separately
 via "xai-dict config". User systemd unit auto-starts after login.
EOF

# Enable for the installing desktop user when possible (sudo dpkg -i …)
cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi

enable_for_user() {
  u="$1"
  [ -n "$u" ] || return 0
  [ "$u" = "root" ] && return 0
  uid=$(id -u "$u" 2>/dev/null) || return 0
  runtime="/run/user/$uid"
  # Only when the user has an active session (login)
  if [ ! -d "$runtime" ]; then
    echo "xai-dict: user $u has no active session yet — service will start on next login (autostart + systemd)."
    return 0
  fi
  bus="unix:path=${runtime}/bus"
  export XDG_RUNTIME_DIR="$runtime"
  export DBUS_SESSION_BUS_ADDRESS="$bus"
  if command -v runuser >/dev/null 2>&1; then
    runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user daemon-reload 2>/dev/null || true
    runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user enable xai-dict.service 2>/dev/null || true
    runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user restart xai-dict.service 2>/dev/null \
      || runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
           systemctl --user start xai-dict.service 2>/dev/null || true
  else
    sudo -u "$u" XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user daemon-reload 2>/dev/null || true
    sudo -u "$u" XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user enable --now xai-dict.service 2>/dev/null || true
  fi
  # Prefer input group for global hotkey (needs re-login)
  if getent group input >/dev/null 2>&1; then
    if ! id -nG "$u" 2>/dev/null | tr ' ' '\n' | grep -qx input; then
      if command -v usermod >/dev/null 2>&1; then
        usermod -aG input "$u" 2>/dev/null && \
          echo "xai-dict: added $u to group 'input' (re-login for Right-Alt hotkey)" || true
      fi
    fi
  fi
  # Drop stale cargo user unit if present
  home=$(getent passwd "$u" | cut -d: -f6)
  stale="$home/.config/systemd/user/xai-dict.service"
  if [ -f "$stale" ] && grep -q '\.cargo/bin/xai-dict' "$stale" 2>/dev/null; then
    mv -f "$stale" "$stale.cargo-bak" 2>/dev/null || true
    echo "xai-dict: moved stale cargo unit → $stale.cargo-bak"
    runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user daemon-reload 2>/dev/null || true
    runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
      systemctl --user enable --now xai-dict.service 2>/dev/null || true
  fi
}

case "$1" in
  configure)
    # Prefer the user who invoked sudo
    if [ -n "${SUDO_USER:-}" ]; then
      enable_for_user "$SUDO_USER"
    elif [ -n "${PKEXEC_UID:-}" ]; then
      enable_for_user "$(getent passwd "$PKEXEC_UID" | cut -d: -f1)"
    fi
    # Also try loginctl list-users (graphical sessions)
    if command -v loginctl >/dev/null 2>&1; then
      loginctl list-users --no-legend 2>/dev/null | awk '{print $2}' | while read -r u; do
        [ -n "$u" ] && [ "$u" != "root" ] && enable_for_user "$u"
      done
    fi
    echo ""
    echo "xai-dict ${VERSION:-} installed."
    echo "  1) xai-dict config          # download models (required once)"
    echo "  2) systemctl --user status xai-dict"
    echo "  3) Re-login if you were just added to group 'input'"
    echo "Docs: /usr/share/doc/xai-dict/README.Debian"
    ;;
esac

exit 0
EOF
# inject version into postinst message without breaking heredoc — use package version at build
sed -i "s/\${VERSION:-}/${VERSION}/g" "$STAGE/DEBIAN/postinst"
chmod 755 "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
# Best-effort stop for active users (upgrade/remove)
if [ "$1" = "remove" ] || [ "$1" = "upgrade" ]; then
  if command -v loginctl >/dev/null 2>&1; then
    loginctl list-users --no-legend 2>/dev/null | awk '{print $1" "$2}' | while read -r uid u; do
      runtime="/run/user/$uid"
      [ -d "$runtime" ] || continue
      bus="unix:path=${runtime}/bus"
      if command -v runuser >/dev/null 2>&1; then
        runuser -u "$u" -- env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="$bus" \
          systemctl --user stop xai-dict.service 2>/dev/null || true
      fi
    done
  fi
fi
exit 0
EOF
chmod 755 "$STAGE/DEBIAN/prerm"

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "purge" ]; then
  # leave user config/models; only note
  echo "xai-dict purged. User models may remain in ~/.local/share/xai-dict"
fi
exit 0
EOF
chmod 755 "$STAGE/DEBIAN/postrm"

# strip binaries to shrink (optional)
if command -v strip >/dev/null; then
  strip --strip-unneeded "$STAGE/usr/bin/xai-dict" 2>/dev/null || true
  strip --strip-unneeded "$STAGE/usr/lib/xai-dict/qwen3_worker" 2>/dev/null || true
  strip --strip-unneeded "$STAGE/usr/lib/xai-dict/zipformer_worker" 2>/dev/null || true
fi

mkdir -p "$OUT_DIR"
dpkg-deb --root-owner-group --build "$STAGE" "$DEB"
echo "==> built $DEB"
ls -lh "$DEB"
dpkg-deb -I "$DEB" | head -30
echo "Install: sudo dpkg -i $DEB"
echo "Models:  xai-dict config  (download buttons)"
echo "Service: systemctl --user status xai-dict"
