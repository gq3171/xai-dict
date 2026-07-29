#!/usr/bin/env bash
# Build & install fcitx5-xaidict Module (works alongside 拼音).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
BUILD="$ROOT/build"
PREFIX="${HOME}/.local"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/fcitx5"

mkdir -p "$BUILD"
cd "$BUILD"
cmake "$ROOT" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DFCITX_INSTALL_ADDONDIR="$PREFIX/lib/fcitx5" \
  -DFCITX_INSTALL_PKGDATADIR="$PREFIX/share/fcitx5"
cmake --build . -j"$(nproc)"
cmake --install .

# Remove obsolete InputMethod entry from earlier 0.2.x installs
rm -f "$DATA/inputmethod/xaidict.conf" \
      "$PREFIX/share/fcitx5/inputmethod/xaidict.conf" \
      /usr/share/fcitx5/inputmethod/xaidict.conf 2>/dev/null || true
# Prefer user module over old system Module binary if we can
if [[ -w /usr/lib/fcitx5 ]]; then
  cp -f "$PREFIX/lib/fcitx5/libxaidict.so" /usr/lib/fcitx5/libxaidict.so
  cp -f "$PREFIX/share/fcitx5/addon/xaidict.conf" /usr/share/fcitx5/addon/xaidict.conf
elif command -v sudo >/dev/null; then
  sudo -n cp -f "$PREFIX/lib/fcitx5/libxaidict.so" /usr/lib/fcitx5/libxaidict.so 2>/dev/null || true
  sudo -n cp -f "$PREFIX/share/fcitx5/addon/xaidict.conf" /usr/share/fcitx5/addon/xaidict.conf 2>/dev/null || true
  sudo -n rm -f /usr/share/fcitx5/inputmethod/xaidict.conf 2>/dev/null || true
fi

# Drop xai-dict from IM group (no longer an IM)
if busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 \
    InputMethodGroupInfo s Default &>/dev/null; then
  busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 \
    SetInputMethodGroupInfo ssa\(ss\) Default us 2 \
    keyboard-us "" pinyin "" >/dev/null 2>&1 || true
  busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 Save \
    >/dev/null 2>&1 || true
fi

echo "Installed module: $PREFIX/lib/fcitx5/libxaidict.so"
echo "Restarting fcitx5…"
killall fcitx5 2>/dev/null || true
sleep 0.6
nohup fcitx5 -d >/tmp/fcitx5-xaidict.log 2>&1 &
sleep 1.2

if busctl --user introspect org.fcitx.Fcitx5 /xaidict 2>/dev/null | grep -q Toggle; then
  echo "OK: /xaidict DBus live (Commit/Preedit/Toggle)"
else
  echo "WARN: /xaidict missing — check /tmp/fcitx5-xaidict.log"
fi

echo
echo "=== 用法（与拼音并存）==="
echo "1. 输入法保持「拼音」即可，无需切换到语音听写"
echo "2. 任意输入框: Super+V 或 F9 → 开始/结束听写"
echo "3. 仍可用拼音打字；听写文字由 daemon 经 fcitx Commit 上屏"
echo "4. daemon: systemctl --user status xai-dict"
echo "5. 右 Alt 全局热键仍可用；若重复触发设 hotkey=none，只用 Super+V"
