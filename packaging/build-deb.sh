#!/usr/bin/env bash
# Build an amd64 .deb for xai-dict (app + workers; models NOT included).
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

install -m755 "$BIN" "$STAGE/usr/bin/xai-dict"
install -m755 "$QWEN_W" "$STAGE/usr/lib/xai-dict/qwen3_worker"
install -m755 "$ZIP_W" "$STAGE/usr/lib/xai-dict/zipformer_worker"
install -m755 "$ROOT/scripts/settings_gui.py" "$STAGE/usr/share/xai-dict/settings_gui.py"
if [[ -f "$ROOT/scripts/osd_bar.py" ]]; then
  install -m755 "$ROOT/scripts/osd_bar.py" "$STAGE/usr/share/xai-dict/osd_bar.py"
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

# systemd user unit (enable with: systemctl --user enable --now xai-dict)
cat > "$STAGE/usr/lib/systemd/user/xai-dict.service" <<'EOF'
[Unit]
Description=xai-dict voice dictation daemon
After=pipewire.service graphical-session.target
Wants=pipewire.service

[Service]
Type=simple
ExecStart=/usr/bin/xai-dict daemon
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
# workers ship next to bundled libs
Environment=PATH=/usr/bin:/bin

[Install]
WantedBy=default.target
EOF

cat > "$STAGE/usr/share/doc/${PKG_NAME}/README.Debian" <<'EOF'
xai-dict
========

Models are NOT included in this package (too large). After install:

  xai-dict config          # GUI: download Qwen3 + Paraformer models
  systemctl --user enable --now xai-dict

Or:
  xai-dict install

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
Recommends: python3-pyqt6 | python3-pyqt5, wl-clipboard, fcitx5
Suggests: ydotool
Homepage: https://github.com/gq3171/xai-dict
Description: System voice dictation (local Qwen3-ASR + streaming preedit)
 LazyTyper-style global hotkey dictation for Linux. Ships the daemon binary
 and sherpa-onnx worker helpers. ASR model weights are downloaded separately
 via "xai-dict config" (or manual place under ~/.local/share/xai-dict/models).
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
# refresh desktop database if available
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi
echo "xai-dict installed. Next:"
echo "  xai-dict config          # download models + settings"
echo "  systemctl --user daemon-reload"
echo "  systemctl --user enable --now xai-dict"
EOF
chmod 755 "$STAGE/DEBIAN/postinst"

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
# quick integrity
dpkg-deb -I "$DEB" | head -25
echo "Install: sudo dpkg -i $DEB"
echo "Models:  xai-dict config  (download buttons)"
