#!/usr/bin/env bash
# Build & install fcitx5-xaidict into ~/.local for the current user.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
BUILD="$ROOT/build"
PREFIX="${HOME}/.local"

mkdir -p "$BUILD"
cd "$BUILD"
cmake "$ROOT" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DFCITX_INSTALL_ADDONDIR="$PREFIX/lib/fcitx5" \
  -DFCITX_INSTALL_PKGDATADIR="$PREFIX/share/fcitx5"
cmake --build . -j"$(nproc)"
cmake --install .

# Ensure fcitx5 finds user addons (standard XDG paths)
mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/fcitx5/addon"
# conf already installed to ~/.local/share/fcitx5/addon if PREFIX/share maps correctly
# Also symlink if needed
if [[ ! -f "${HOME}/.local/share/fcitx5/addon/xaidict.conf" ]]; then
  ln -sfn "$PREFIX/share/fcitx5/addon/xaidict.conf" \
    "${HOME}/.local/share/fcitx5/addon/xaidict.conf" 2>/dev/null || true
fi

echo "Installed libxaidict → $PREFIX/lib/fcitx5/"
echo "Reloading fcitx5…"
fcitx5-remote -r 2>/dev/null || (killall fcitx5 2>/dev/null; sleep 0.5; nohup fcitx5 -d >/dev/null 2>&1 &)
sleep 0.8
# Probe
if busctl --user introspect org.fcitx.Fcitx5 /xaidict 2>/dev/null | grep -q Commit; then
  echo "OK: /xaidict Commit is live"
else
  echo "WARN: /xaidict not visible yet — fully restart fcitx5 from tray/KDE Virtual Keyboard"
fi
