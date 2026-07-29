#!/usr/bin/env bash
# Build & install fcitx5-xaidict (real Input Method) into ~/.local
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

mkdir -p "$DATA/addon" "$DATA/inputmethod"
# PREFIX/share often IS XDG_DATA_HOME; only copy when different.
if [[ "$(realpath -m "$PREFIX/share/fcitx5/addon")" != "$(realpath -m "$DATA/addon")" ]]; then
  cp -f "$PREFIX/share/fcitx5/addon/xaidict.conf" "$DATA/addon/xaidict.conf"
  cp -f "$PREFIX/share/fcitx5/inputmethod/xaidict.conf" "$DATA/inputmethod/xaidict.conf"
fi

echo "Installed:"
echo "  $PREFIX/lib/fcitx5/libxaidict.so"
echo "  $PREFIX/share/fcitx5/addon/xaidict.conf"
echo "  $PREFIX/share/fcitx5/inputmethod/xaidict.conf"
echo
echo "Restarting fcitx5 (required to reload addon)…"
# Soft reload often keeps the old .so mapped — hard restart is reliable.
killall fcitx5 2>/dev/null || true
sleep 0.6
nohup fcitx5 -d >/tmp/fcitx5-xaidict.log 2>&1 &
sleep 1.2

echo
if busctl --user introspect org.fcitx.Fcitx5 /xaidict 2>/dev/null | grep -q Toggle; then
  echo "OK: DBus /xaidict (Commit/Preedit/Toggle) is live"
else
  echo "WARN: /xaidict not visible — restart fcitx5 from KDE tray"
  echo "      log: /tmp/fcitx5-xaidict.log"
fi

echo
echo "=== 使用 ==="
echo "1. 系统设置 → 键盘 → 虚拟键盘 / 输入法 → 添加 →「语音听写」"
echo "2. systemctl --user status xai-dict   # daemon 需运行"
echo "3. 切换到该输入法后：Super+V 或 F9 开始/结束"
echo "4. 避免与 daemon 右 Alt 双触发：hotkey=none 或只用 Super+V"
