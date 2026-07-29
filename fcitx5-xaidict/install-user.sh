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

# Plasma 「虚拟键盘」里经常搜不到 fcitx 自建 IM — 直接写入 Default 组
if [[ -x "$ROOT/enable-im.sh" ]]; then
  bash "$ROOT/enable-im.sh" || true
fi

echo
echo "=== 使用（不要去 Plasma「虚拟键盘」里找）==="
echo "1. 打开配置:  fcitx5-configtool"
echo "   或托盘键盘图标 → 配置"
echo "2. 当前列表应含「语音听写 / Voice Dictation / xai-dict」"
echo "3. 切换: Ctrl+Space 循环，直到托盘显示 🎤"
echo "   或: busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 SetCurrentIM s xai-dict"
echo "4. Super+V 或 F9 开始/结束（需 xai-dict daemon 运行）"
echo "5. 避免与右 Alt 双触发: hotkey = \"none\""
