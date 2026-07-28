#!/usr/bin/env python3
"""xai-dict Lazy-style OSD bar — bottom-center floating status.

Must NEVER take keyboard focus or create an fcitx/text-input context,
otherwise voice commit targets this window instead of the user's field.

State file (one line), written by the daemon:
  hidden
  recording
  transcribing
  done\\tpreview text
  error\\tmessage
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

from PyQt6.QtCore import Qt, QTimer, QRectF
from PyQt6.QtGui import QColor, QFont, QPainter, QPainterPath, QPen, QBrush
from PyQt6.QtWidgets import QApplication, QWidget


def state_path() -> Path:
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/xai-dict-{os.getuid()}")
    return Path(runtime) / "xai-dict-osd.state"


def pid_path() -> Path:
    return state_path().with_suffix(".pid")


class OsdBar(QWidget):
    def __init__(self) -> None:
        super().__init__()
        self.setWindowFlags(
            Qt.WindowType.FramelessWindowHint
            | Qt.WindowType.WindowStaysOnTopHint
            | Qt.WindowType.Tool
            | Qt.WindowType.WindowDoesNotAcceptFocus
            | Qt.WindowType.WindowTransparentForInput
            | Qt.WindowType.BypassWindowManagerHint
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
        self.setAttribute(Qt.WidgetAttribute.WA_X11DoNotAcceptFocus)
        # Critical: do not create Wayland text-input / fcitx InputContext.
        self.setAttribute(Qt.WidgetAttribute.WA_InputMethodEnabled, False)
        self.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.setFixedSize(420, 56)

        self.mode = "hidden"  # hidden | recording | transcribing | done | error
        self.message = ""
        self.pulse = 0.0
        self.t0 = time.monotonic()
        self.hide_at: float | None = None

        self.timer = QTimer(self)
        self.timer.timeout.connect(self.tick)
        self.timer.start(50)

        self.reposition()
        self.hide()

    def showEvent(self, event) -> None:  # noqa: N802
        # Re-assert no-focus every show (compositors can be sticky).
        self.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating, True)
        self.setAttribute(Qt.WidgetAttribute.WA_InputMethodEnabled, False)
        self.clearFocus()
        super().showEvent(event)

    def reposition(self) -> None:
        screen = QApplication.primaryScreen()
        if screen is None:
            return
        geo = screen.availableGeometry()
        x = geo.x() + (geo.width() - self.width()) // 2
        y = geo.y() + geo.height() - self.height() - 48
        self.move(x, y)

    def read_state(self) -> None:
        path = state_path()
        try:
            raw = path.read_text(encoding="utf-8").strip()
        except OSError:
            return
        if not raw:
            return
        if raw == "hidden":
            if self.mode != "hidden":
                self.mode = "hidden"
                self.hide()
            return
        if raw == "recording":
            if self.mode != "recording":
                self.mode = "recording"
                self.message = ""
                self.t0 = time.monotonic()
                self.hide_at = None
                self.reposition()
                self.show()
            return
        if raw == "transcribing":
            if self.mode != "transcribing":
                self.mode = "transcribing"
                self.message = ""
                self.hide_at = None
                self.reposition()
                self.show()
            return
        if raw.startswith("done\t") or raw.startswith("done "):
            text = raw.split("\t", 1)[-1] if "\t" in raw else raw[5:]
            self.mode = "done"
            self.message = text.strip()
            self.hide_at = time.monotonic() + 2.4
            self.reposition()
            self.show()
            try:
                path.write_text("hidden\n", encoding="utf-8")
            except OSError:
                pass
            return
        if raw.startswith("error\t") or raw.startswith("error "):
            text = raw.split("\t", 1)[-1] if "\t" in raw else raw[6:]
            self.mode = "error"
            self.message = text.strip()
            self.hide_at = time.monotonic() + 3.5
            self.reposition()
            self.show()
            try:
                path.write_text("hidden\n", encoding="utf-8")
            except OSError:
                pass

    def tick(self) -> None:
        self.read_state()
        self.pulse = (self.pulse + 0.08) % (2 * 3.14159)
        if self.hide_at is not None and time.monotonic() >= self.hide_at:
            self.hide_at = None
            self.mode = "hidden"
            self.hide()
            return
        if self.isVisible():
            self.update()

    def paintEvent(self, _event) -> None:  # noqa: N802
        if self.mode == "hidden":
            return
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)

        if self.mode == "recording":
            bg = QColor(28, 28, 32, 230)
            accent = QColor(239, 68, 68)
        elif self.mode == "transcribing":
            bg = QColor(28, 28, 32, 230)
            accent = QColor(59, 130, 246)
        elif self.mode == "done":
            bg = QColor(20, 40, 28, 235)
            accent = QColor(34, 197, 94)
        else:
            bg = QColor(40, 20, 20, 235)
            accent = QColor(248, 113, 113)

        rect = QRectF(2, 2, self.width() - 4, self.height() - 4)
        path = QPainterPath()
        path.addRoundedRect(rect, 16, 16)
        p.fillPath(path, QBrush(bg))
        p.setPen(QPen(QColor(255, 255, 255, 28), 1))
        p.drawPath(path)

        bar = QRectF(10, 14, 4, self.height() - 28)
        p.setPen(Qt.PenStyle.NoPen)
        p.setBrush(accent)
        p.drawRoundedRect(bar, 2, 2)

        cx, cy = 34, self.height() / 2
        if self.mode == "recording":
            import math

            r = 7 + 2.5 * (0.5 + 0.5 * math.sin(self.pulse * 4))
            p.setBrush(accent)
            p.drawEllipse(QRectF(cx - r, cy - r, r * 2, r * 2))
        else:
            p.setBrush(accent)
            p.drawEllipse(QRectF(cx - 6, cy - 6, 12, 12))

        p.setPen(QColor(245, 245, 247))
        font = QFont("Sans", 12)
        font.setWeight(QFont.Weight.DemiBold)
        p.setFont(font)

        if self.mode == "recording":
            elapsed = int(time.monotonic() - self.t0)
            mm, ss = divmod(elapsed, 60)
            label = f"录音中  {mm:02d}:{ss:02d}    再按右 Alt 结束"
        elif self.mode == "transcribing":
            label = "识别中…"
        elif self.mode == "done":
            preview = self.message
            if len(preview) > 28:
                preview = preview[:28] + "…"
            label = f"✓  {preview}" if preview else "✓  完成"
        else:
            preview = self.message
            if len(preview) > 30:
                preview = preview[:30] + "…"
            label = f"✗  {preview}" if preview else "✗  出错"

        p.drawText(
            QRectF(52, 0, self.width() - 64, self.height()),
            int(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft),
            label,
        )
        p.end()


def main() -> int:
    pp = pid_path()
    if pp.exists():
        try:
            old = int(pp.read_text().strip())
            os.kill(old, 0)
            return 0
        except (ValueError, OSError, ProcessLookupError):
            pass

    sp = state_path()
    sp.parent.mkdir(parents=True, exist_ok=True)
    if not sp.exists():
        sp.write_text("hidden\n", encoding="utf-8")

    # Qt platform: never use fcitx IM module for this process.
    os.environ["QT_IM_MODULE"] = "none"
    os.environ["GTK_IM_MODULE"] = "none"
    os.environ.pop("SDL_IM_MODULE", None)

    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)

    try:
        pp.write_text(f"{os.getpid()}\n", encoding="utf-8")
    except OSError:
        pass

    bar = OsdBar()
    keeper = QTimer()
    keeper.timeout.connect(lambda: None)
    keeper.start(1000)

    rc = app.exec()
    try:
        pp.unlink(missing_ok=True)
    except OSError:
        pass
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
