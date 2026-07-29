#!/usr/bin/env bash
# Add xai-dict to the current fcitx5 Default group (idempotent).
set -euo pipefail
CTRL=org.fcitx.Fcitx5
PATH_C=/controller
IFACE=org.fcitx.Fcitx.Controller1

# Ensure addon is loaded
if ! busctl --user introspect "$CTRL" /xaidict &>/dev/null; then
  echo "xaidict not loaded — run ./install-user.sh first"
  exit 1
fi

# Parse AvailableInputMethods to ensure unique name exists
if ! busctl --user call "$CTRL" "$PATH_C" "$IFACE" AvailableInputMethods 2>/dev/null | grep -q '"xai-dict"'; then
  echo "xai-dict not in AvailableInputMethods — reinstall addon / restart fcitx5"
  exit 1
fi

# Get current group layout + items via FullInputMethodGroupInfo or GroupInfo
# InputMethodGroupInfo returns: s "us" a(ss) ...
raw=$(busctl --user call "$CTRL" "$PATH_C" "$IFACE" InputMethodGroupInfo s Default)
# Example: sa(ss) "us" 2 "keyboard-us" "" "pinyin" ""
# Use python to parse and rebuild
python3 - "$raw" <<'PY2'
import subprocess, sys, re
raw = sys.argv[1]
# extract quoted strings after layout
parts = re.findall(r'"([^"]*)"', raw)
# parts[0] is layout, then pairs of (name, layout)
if not parts:
    print("cannot parse group info:", raw, file=sys.stderr)
    sys.exit(1)
layout = parts[0]
pairs = list(zip(parts[1::2], parts[2::2]))
names = [n for n, _ in pairs]
if "xai-dict" not in names:
    pairs.append(("xai-dict", ""))
# build busctl args
n = len(pairs)
args = ["busctl", "--user", "call", "org.fcitx.Fcitx5", "/controller",
        "org.fcitx.Fcitx.Controller1", "SetInputMethodGroupInfo",
        "ssa(ss)", "Default", layout, str(n)]
for name, lay in pairs:
    args += [name, lay]
print("running:", " ".join(args))
subprocess.check_call(args)
subprocess.check_call(["busctl", "--user", "call", "org.fcitx.Fcitx5", "/controller",
                       "org.fcitx.Fcitx.Controller1", "Save"])
print("OK: Default group now:", ", ".join(n for n,_ in pairs))
PY2

echo
echo "切换到语音听写:"
echo "  busctl --user call org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1 SetCurrentIM s xai-dict"
echo "  或 Ctrl+Space 循环切换，直到托盘出现 🎤"
