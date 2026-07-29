#!/usr/bin/env python3
"""xai-dict settings GUI (PyQt6) — tabbed, roomy layout + model download."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request
from pathlib import Path

from PyQt6.QtCore import Qt, QThread, pyqtSignal
from PyQt6.QtGui import QFont
from PyQt6.QtWidgets import (
    QApplication,
    QCheckBox,
    QComboBox,
    QFileDialog,
    QFormLayout,
    QFrame,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMainWindow,
    QMessageBox,
    QProgressDialog,
    QPushButton,
    QScrollArea,
    QSizePolicy,
    QSpinBox,
    QDoubleSpinBox,
    QTabWidget,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)

# ---------------------------------------------------------------------------
# Catalog / paths
# ---------------------------------------------------------------------------

MODELS_ROOT = Path.home() / ".local" / "share" / "xai-dict" / "models"

MODEL_CATALOG = {
    "qwen3": {
        "title": "Qwen3-ASR 0.6B int8（定稿）",
        "url": (
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
            "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2"
        ),
        "dirname": "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25",
        "kind": "tar",
        "ready": lambda d: (d / "encoder.int8.onnx").is_file()
        or (d / "encoder.onnx").is_file(),
        "size_hint": "~1 GB",
    },
    "paraformer": {
        "title": "Streaming Paraformer 中英（字级预编辑）",
        "url": (
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
            "sherpa-onnx-streaming-paraformer-bilingual-zh-en.tar.bz2"
        ),
        "dirname": "sherpa-onnx-streaming-paraformer-bilingual-zh-en",
        "kind": "tar",
        "ready": lambda d: (d / "encoder.int8.onnx").is_file()
        or (d / "encoder.onnx").is_file(),
        "size_hint": "~1 GB",
    },
    "zipformer": {
        "title": "Streaming Zipformer 中文 int8（备用预编辑）",
        "url": (
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
            "sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30.tar.bz2"
        ),
        "dirname": "sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30",
        "kind": "tar",
        "ready": lambda d: (d / "joiner.int8.onnx").is_file()
        or (d / "joiner.onnx").is_file(),
        "size_hint": "~160 MB",
    },
    "whisper_small": {
        "title": "Whisper ggml-small（local 后端）",
        "url": "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        "filename": "ggml-small.bin",
        "kind": "file",
        "ready": lambda p: p.is_file() and p.stat().st_size > 100_000_000,
        "size_hint": "~466 MB",
    },
}

SPACING = 14
MARGINS = (20, 18, 20, 18)
GROUP_MARGINS = (16, 20, 16, 16)


def config_path() -> Path:
    if env := os.environ.get("XAI_DICT_CONFIG"):
        return Path(env)
    base = os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "xai-dict" / "config.toml"
    return Path.home() / ".config" / "xai-dict" / "config.toml"


def load_config(path: Path) -> dict:
    defaults = {
        "provider": "qwen3",
        "language": "zh",
        "sample_rate": 16000,
        "paste": True,
        "proxy": "",
        "proxy_enabled": True,
        "proxy_remember": "",
        "local_model": "",
        "local_threads": 6,
        "qwen3_model_dir": "",
        "qwen3_max_new_tokens": 128,
        "qwen3_hotwords": "",
        "hotkey": "rightalt",
        "stream": True,
        "stream_min_silence_ms": 600,
        "stream_max_segment_ms": 12000,
        "stream_min_speech_ms": 280,
        "dual_model": True,
        "dual_preedit": True,
        "stream_model_dir": "",
        "stream_threads": 3,
        "near_field": True,
        "vad_speech_rms": 0.0,
        "vad_snr": 0.0,
        "agc_max_gain": 0.0,
        "api_base": "https://api.x.ai",
        "stt_path": "/v1/stt",
        "interim_results": True,
        "endpointing_ms": 400,
        "keyterms": [],
    }
    if not path.is_file():
        return defaults
    try:
        with path.open("rb") as f:
            data = tomllib.load(f)
    except Exception as e:
        print(f"warn: parse {path}: {e}", file=sys.stderr)
        return defaults
    out = defaults.copy()
    out.update(data)
    if "proxy_enabled" not in data:
        out["proxy_enabled"] = bool(str(out.get("proxy", "")).strip())
    return out


def toml_escape(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def save_config(path: Path, cfg: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    proxy_enabled = bool(cfg.get("proxy_enabled", True))
    proxy_url = str(cfg.get("proxy", "")).strip()
    active_proxy = proxy_url if proxy_enabled else ""
    remembered = proxy_url or str(cfg.get("proxy_remember", "")).strip()

    keyterms = cfg.get("keyterms") or []
    if isinstance(keyterms, str):
        keyterms = [t.strip() for t in keyterms.split(",") if t.strip()]
    kt = ", ".join(toml_escape(t) for t in keyterms)

    lines = [
        "# xai-dict configuration — settings GUI",
        "# systemctl --user restart xai-dict",
        "",
        f"provider = {toml_escape(str(cfg.get('provider', 'qwen3')))}",
        f"language = {toml_escape(str(cfg.get('language', 'zh')))}",
        f"sample_rate = {int(cfg.get('sample_rate', 16000))}",
        f"paste = {'true' if cfg.get('paste', True) else 'false'}",
        f"proxy = {toml_escape(active_proxy)}",
        f"proxy_enabled = {'true' if proxy_enabled else 'false'}",
        f"proxy_remember = {toml_escape(remembered)}",
        f"api_base = {toml_escape(str(cfg.get('api_base', 'https://api.x.ai')))}",
        f"stt_path = {toml_escape(str(cfg.get('stt_path', '/v1/stt')))}",
        f"interim_results = {'true' if cfg.get('interim_results', True) else 'false'}",
        f"endpointing_ms = {int(cfg.get('endpointing_ms', 400))}",
        f"keyterms = [{kt}]",
        "",
        f"local_model = {toml_escape(str(cfg.get('local_model', '')))}",
        f"local_threads = {int(cfg.get('local_threads', 6))}",
        f"qwen3_model_dir = {toml_escape(str(cfg.get('qwen3_model_dir', '')))}",
        f"qwen3_max_new_tokens = {int(cfg.get('qwen3_max_new_tokens', 128))}",
        f"qwen3_hotwords = {toml_escape(str(cfg.get('qwen3_hotwords', '')))}",
        "",
        f"hotkey = {toml_escape(str(cfg.get('hotkey', 'rightalt')))}",
        "",
        f"stream = {'true' if cfg.get('stream', True) else 'false'}",
        f"stream_min_silence_ms = {int(cfg.get('stream_min_silence_ms', 600))}",
        f"stream_max_segment_ms = {int(cfg.get('stream_max_segment_ms', 12000))}",
        f"stream_min_speech_ms = {int(cfg.get('stream_min_speech_ms', 280))}",
        f"dual_model = {'true' if cfg.get('dual_model', True) else 'false'}",
        f"dual_preedit = {'true' if cfg.get('dual_preedit', True) else 'false'}",
        f"stream_model_dir = {toml_escape(str(cfg.get('stream_model_dir', '')))}",
        f"stream_threads = {int(cfg.get('stream_threads', 3))}",
        "",
        f"near_field = {'true' if cfg.get('near_field', True) else 'false'}",
        f"vad_speech_rms = {float(cfg.get('vad_speech_rms', 0.0))}",
        f"vad_snr = {float(cfg.get('vad_snr', 0.0))}",
        f"agc_max_gain = {float(cfg.get('agc_max_gain', 0.0))}",
        "",
    ]
    path.write_text("\n".join(lines), encoding="utf-8")


def restart_daemon() -> tuple[bool, str]:
    try:
        r = subprocess.run(
            ["systemctl", "--user", "restart", "xai-dict"],
            capture_output=True,
            text=True,
            timeout=15,
        )
        if r.returncode == 0:
            return True, "daemon 已重启"
        return False, (r.stderr or r.stdout or f"exit {r.returncode}").strip()
    except Exception as e:
        return False, str(e)


def daemon_status() -> str:
    try:
        r = subprocess.run(
            ["systemctl", "--user", "is-active", "xai-dict"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return (r.stdout or r.stderr or "unknown").strip()
    except Exception:
        return "unknown"


def model_installed(key: str) -> tuple[bool, Path]:
    meta = MODEL_CATALOG[key]
    if meta["kind"] == "file":
        p = MODELS_ROOT / meta["filename"]
        return bool(meta["ready"](p)), p
    p = MODELS_ROOT / meta["dirname"]
    return bool(meta["ready"](p)), p


def hint(text: str) -> QLabel:
    lab = QLabel(text)
    lab.setWordWrap(True)
    lab.setStyleSheet("color: #666; font-size: 12px; margin: 2px 0 8px 0;")
    return lab


def roomy_form(parent: QWidget | None = None) -> QFormLayout:
    f = QFormLayout(parent)
    f.setSpacing(SPACING)
    f.setContentsMargins(*GROUP_MARGINS)
    f.setLabelAlignment(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter)
    f.setFormAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignTop)
    f.setFieldGrowthPolicy(QFormLayout.FieldGrowthPolicy.ExpandingFieldsGrow)
    f.setHorizontalSpacing(18)
    f.setVerticalSpacing(12)
    return f


def roomy_group(title: str) -> tuple[QGroupBox, QFormLayout]:
    g = QGroupBox(title)
    f = roomy_form(g)
    return g, f


def scroll_wrap(widget: QWidget) -> QScrollArea:
    sc = QScrollArea()
    sc.setWidgetResizable(True)
    sc.setFrameShape(QFrame.Shape.NoFrame)
    sc.setWidget(widget)
    sc.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
    return sc


# ---------------------------------------------------------------------------
# Download
# ---------------------------------------------------------------------------


class DownloadWorker(QThread):
    progress = pyqtSignal(int, str)
    finished_ok = pyqtSignal(str, str)
    failed = pyqtSignal(str)

    def __init__(self, model_key: str, parent=None):
        super().__init__(parent)
        self.model_key = model_key
        self._cancel = False

    def cancel(self) -> None:
        self._cancel = True

    def run(self) -> None:
        try:
            meta = MODEL_CATALOG[self.model_key]
            MODELS_ROOT.mkdir(parents=True, exist_ok=True)
            url = meta["url"]
            self.progress.emit(0, f"连接 {url.split('/')[-1]} …")

            if meta["kind"] == "file":
                dest = MODELS_ROOT / meta["filename"]
                self._download_file(url, dest)
                if self._cancel:
                    return
                if not meta["ready"](dest):
                    raise RuntimeError("下载完成但文件校验失败")
                self.finished_ok.emit(self.model_key, str(dest))
                return

            with tempfile.TemporaryDirectory(prefix="xai-dict-dl-") as tmp:
                tmp_path = Path(tmp)
                archive = tmp_path / "model.tar.bz2"
                self._download_file(url, archive)
                if self._cancel:
                    return
                self.progress.emit(92, "解压中…")
                with tarfile.open(archive, "r:bz2") as tar:
                    tar.extractall(path=tmp_path)
                candidates = [p for p in tmp_path.iterdir() if p.is_dir()]
                if not candidates:
                    raise RuntimeError("压缩包内未找到模型目录")
                src = next(
                    (c for c in candidates if c.name == meta["dirname"]),
                    candidates[0],
                )
                dest = MODELS_ROOT / meta["dirname"]
                if dest.exists():
                    shutil.rmtree(dest)
                shutil.move(str(src), str(dest))
                if not meta["ready"](dest):
                    raise RuntimeError(f"解压后模型不完整: {dest}")
                self.progress.emit(100, "完成")
                self.finished_ok.emit(self.model_key, str(dest))
        except Exception as e:
            if not self._cancel:
                self.failed.emit(str(e))

    def _download_file(self, url: str, dest: Path) -> None:
        curl = shutil.which("curl")
        if curl:
            self._download_curl(curl, url, dest)
            return
        self._download_urllib(url, dest)

    def _download_curl(self, curl: str, url: str, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        part = dest.with_suffix(dest.suffix + ".part")
        cmd = [curl, "-fL", "--retry", "3", "-C", "-", "--progress-bar", "-o", str(part), url]
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        n = 0
        while proc.poll() is None:
            if self._cancel:
                proc.terminate()
                try:
                    proc.wait(timeout=3)
                except Exception:
                    proc.kill()
                return
            n += 1
            size = part.stat().st_size if part.exists() else 0
            self.progress.emit(min(88, 5 + n % 80), f"curl 下载中… {size / (1024 * 1024):.1f} MB")
            self.msleep(400)
        if proc.returncode != 0:
            err = (proc.stderr.read()[-400:] if proc.stderr else "") or str(proc.returncode)
            self.progress.emit(5, "curl 失败，改用内置下载…")
            try:
                self._download_urllib(url, dest)
            except Exception as e2:
                raise RuntimeError(f"curl: {err}; urllib: {e2}") from e2
            return
        part.replace(dest)
        self.progress.emit(90, "下载完成")

    def _download_urllib(self, url: str, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        part = dest.with_suffix(dest.suffix + ".part")
        req = urllib.request.Request(url, headers={"User-Agent": "xai-dict-settings/1.0"})
        with urllib.request.urlopen(req, timeout=60) as resp:
            total = resp.headers.get("Content-Length")
            total_n = int(total) if total and total.isdigit() else 0
            done = 0
            with part.open("wb") as out:
                while True:
                    if self._cancel:
                        return
                    chunk = resp.read(1024 * 256)
                    if not chunk:
                        break
                    out.write(chunk)
                    done += len(chunk)
                    if total_n > 0:
                        pct = min(90, int(done * 90 / total_n))
                        self.progress.emit(
                            pct,
                            f"下载中 {done / (1024 * 1024):.1f} / {total_n / (1024 * 1024):.0f} MB",
                        )
                    else:
                        self.progress.emit(
                            min(90, 10 + (done // (1024 * 1024)) % 80),
                            f"下载中 {done / (1024 * 1024):.1f} MB",
                        )
        part.replace(dest)
        self.progress.emit(90, "下载完成")


class PathRow(QWidget):
    def __init__(self, parent=None, directory: bool = False):
        super().__init__(parent)
        self.directory = directory
        lay = QHBoxLayout(self)
        lay.setContentsMargins(0, 0, 0, 0)
        lay.setSpacing(10)
        self.edit = QLineEdit()
        self.edit.setMinimumHeight(32)
        btn = QPushButton("浏览…")
        btn.setMinimumHeight(32)
        btn.setMinimumWidth(72)
        btn.clicked.connect(self._browse)
        lay.addWidget(self.edit, 1)
        lay.addWidget(btn)

    def _browse(self):
        if self.directory:
            p = QFileDialog.getExistingDirectory(self, "选择目录", self.edit.text())
        else:
            p, _ = QFileDialog.getOpenFileName(self, "选择文件", self.edit.text())
        if p:
            self.edit.setText(p)

    def text(self) -> str:
        return self.edit.text().strip()

    def setText(self, s: str) -> None:
        self.edit.setText(s)


# ---------------------------------------------------------------------------
# Main window
# ---------------------------------------------------------------------------


class SettingsWindow(QMainWindow):
    def __init__(self, path: Path):
        super().__init__()
        self.path = path
        self.cfg = load_config(path)
        self._worker: DownloadWorker | None = None
        self._progress: QProgressDialog | None = None

        self.setWindowTitle("xai-dict 设置")
        self.resize(820, 720)
        self.setMinimumSize(700, 560)

        root = QWidget()
        self.setCentralWidget(root)
        outer = QVBoxLayout(root)
        outer.setSpacing(SPACING)
        outer.setContentsMargins(18, 16, 18, 14)

        # Header
        head = QVBoxLayout()
        head.setSpacing(6)
        title = QLabel("xai-dict 语音听写设置")
        tf = QFont()
        tf.setPointSize(14)
        tf.setBold(True)
        title.setFont(tf)
        head.addWidget(title)
        head.addWidget(QLabel(f"配置文件　{path}"))
        self.status_label = QLabel(f"daemon　{daemon_status()}")
        head.addWidget(self.status_label)
        outer.addLayout(head)

        # Tabs
        tabs = QTabWidget()
        tabs.setDocumentMode(True)
        tabs.setTabPosition(QTabWidget.TabPosition.North)
        tabs.addTab(self._tab_basic(), "基本")
        tabs.addTab(self._tab_dictation(), "听写行为")
        tabs.addTab(self._tab_models(), "模型")
        tabs.addTab(self._tab_network(), "网络 / 云端")
        tabs.addTab(self._tab_advanced(), "高级")
        outer.addWidget(tabs, 1)

        # Footer buttons
        btns = QHBoxLayout()
        btns.setSpacing(10)
        b_journal = QPushButton("查看日志")
        b_journal.setMinimumHeight(36)
        b_journal.clicked.connect(self.open_journal)
        b_folder = QPushButton("模型目录")
        b_folder.setMinimumHeight(36)
        b_folder.clicked.connect(self.open_models_dir)
        b_edit = QPushButton("文本编辑…")
        b_edit.setMinimumHeight(36)
        b_edit.clicked.connect(self.open_in_editor)
        b_save = QPushButton("保存")
        b_save.setMinimumHeight(36)
        b_save.clicked.connect(lambda: self.save(restart=False))
        b_apply = QPushButton("保存并重启")
        b_apply.setMinimumHeight(36)
        b_apply.setDefault(True)
        b_apply.clicked.connect(lambda: self.save(restart=True))
        b_close = QPushButton("关闭")
        b_close.setMinimumHeight(36)
        b_close.clicked.connect(self.close)
        btns.addWidget(b_journal)
        btns.addWidget(b_folder)
        btns.addWidget(b_edit)
        btns.addStretch(1)
        btns.addWidget(b_save)
        btns.addWidget(b_apply)
        btns.addWidget(b_close)
        outer.addLayout(btns)

    # ----- tabs -------------------------------------------------------------

    def _tab_basic(self) -> QWidget:
        page = QWidget()
        lay = QVBoxLayout(page)
        lay.setSpacing(SPACING)
        lay.setContentsMargins(*MARGINS)

        g, f = roomy_group("识别与输入")
        self.provider = QComboBox()
        self.provider.addItem("qwen3 — 本地 Qwen3-ASR（推荐）", "qwen3")
        self.provider.addItem("local — Whisper.cpp", "local")
        self.provider.addItem("xai — 云端 xAI STT", "xai")
        self._set_combo_data(self.provider, str(self.cfg.get("provider", "qwen3")))
        self.provider.setMinimumHeight(34)
        f.addRow("识别后端", self.provider)
        f.addRow("", hint("本地听写选 qwen3；仅云端调试时用 xai。"))

        self.hotkey = QComboBox()
        self.hotkey.addItem("右 Alt（默认）", "rightalt")
        self.hotkey.addItem("左 Alt", "leftalt")
        self.hotkey.addItem("关闭内置热键（只用 xai-dict toggle）", "none")
        self._set_combo_data(self.hotkey, str(self.cfg.get("hotkey", "rightalt")))
        self.hotkey.setMinimumHeight(34)
        f.addRow("内置热键", self.hotkey)
        f.addRow("", hint("daemon 内监听的按键；也可在系统设置里绑 xai-dict toggle。"))

        self.language = QLineEdit(str(self.cfg.get("language", "zh")))
        self.language.setMinimumHeight(34)
        self.language.setPlaceholderText("zh / en / auto")
        f.addRow("语言", self.language)
        f.addRow("", hint("主要影响 Whisper；Qwen3 对中文默认友好。"))

        self.paste = QCheckBox("识别后自动写入当前输入框")
        self.paste.setChecked(bool(self.cfg.get("paste", True)))
        f.addRow(self.paste)

        self.threads = QSpinBox()
        self.threads.setRange(1, 32)
        self.threads.setMinimumHeight(34)
        self.threads.setValue(int(self.cfg.get("local_threads", 6)))
        f.addRow("定稿模型线程数", self.threads)
        f.addRow("", hint("Qwen3 / Whisper 推理线程，建议 4–8。"))
        lay.addWidget(g)

        g2, f2 = roomy_group("快捷操作")
        row = QHBoxLayout()
        row.setSpacing(12)
        b1 = QPushButton("仅重启 daemon")
        b1.setMinimumHeight(36)
        b1.clicked.connect(self.restart_only)
        b2 = QPushButton("刷新状态")
        b2.setMinimumHeight(36)
        b2.clicked.connect(self.refresh_status)
        row.addWidget(b1)
        row.addWidget(b2)
        row.addStretch(1)
        f2.addRow(row)
        lay.addWidget(g2)
        lay.addStretch(1)
        return scroll_wrap(page)

    def _tab_dictation(self) -> QWidget:
        page = QWidget()
        lay = QVBoxLayout(page)
        lay.setSpacing(SPACING)
        lay.setContentsMargins(*MARGINS)

        g, f = roomy_group("流式听写")
        self.stream = QCheckBox("启用流式（边说边切句上屏）")
        self.stream.setChecked(bool(self.cfg.get("stream", True)))
        f.addRow(self.stream)
        f.addRow("", hint("关闭后：整段录音结束再识别一次。"))

        self.dual = QCheckBox("双模型：流式预编辑 + Qwen3 定稿")
        self.dual.setChecked(bool(self.cfg.get("dual_model", True)))
        f.addRow(self.dual)

        self.preedit = QCheckBox("显示字级预编辑（fcitx Preedit）")
        self.preedit.setChecked(bool(self.cfg.get("dual_preedit", True)))
        f.addRow(self.preedit)
        f.addRow("", hint("关闭后只在定稿时出字，更稳、无实时闪字。"))
        lay.addWidget(g)

        g2, f2 = roomy_group("切句与时长")
        self.silence = QSpinBox()
        self.silence.setRange(200, 3000)
        self.silence.setSingleStep(50)
        self.silence.setSuffix(" ms")
        self.silence.setMinimumHeight(34)
        self.silence.setValue(int(self.cfg.get("stream_min_silence_ms", 600)))
        f2.addRow("停顿多久切一句", self.silence)
        f2.addRow("", hint("越大越不容易把一句话切碎；默认约 600ms。"))

        self.maxseg = QSpinBox()
        self.maxseg.setRange(2000, 60000)
        self.maxseg.setSingleStep(500)
        self.maxseg.setSuffix(" ms")
        self.maxseg.setMinimumHeight(34)
        self.maxseg.setValue(int(self.cfg.get("stream_max_segment_ms", 12000)))
        f2.addRow("一句最长（强制切）", self.maxseg)
        f2.addRow("", hint("连续说太久会硬切；过短会提高错误率。建议 10–15 秒。"))

        self.minspeech = QSpinBox()
        self.minspeech.setRange(100, 2000)
        self.minspeech.setSuffix(" ms")
        self.minspeech.setMinimumHeight(34)
        self.minspeech.setValue(int(self.cfg.get("stream_min_speech_ms", 280)))
        f2.addRow("最短有效语音", self.minspeech)
        f2.addRow("", hint("更短的片段直接丢弃，减少杂音触发。"))

        self.stream_threads = QSpinBox()
        self.stream_threads.setRange(1, 16)
        self.stream_threads.setMinimumHeight(34)
        self.stream_threads.setValue(int(self.cfg.get("stream_threads", 3)))
        f2.addRow("流式模型线程", self.stream_threads)
        lay.addWidget(g2)

        g3, f3 = roomy_group("抗旁人 / 近讲")
        self.near = QCheckBox("近讲优先（抑制旁人小声）")
        self.near.setChecked(bool(self.cfg.get("near_field", True)))
        f3.addRow(self.near)

        self.near_preset = QComboBox()
        self.near_preset.addItem("温和（推荐）", "mild")
        self.near_preset.addItem("严格（更抗旁人，可能漏识）", "strict")
        self.near_preset.addItem("关闭近讲优化", "off")
        self.near_preset.setMinimumHeight(34)
        self.near_preset.currentIndexChanged.connect(self._apply_near_preset)
        f3.addRow("一键预设", self.near_preset)
        f3.addRow("", hint("预设会改 near_field 与下方高级 VAD；仍可手动微调。"))
        lay.addWidget(g3)
        lay.addStretch(1)
        return scroll_wrap(page)

    def _tab_models(self) -> QWidget:
        page = QWidget()
        lay = QVBoxLayout(page)
        lay.setSpacing(SPACING)
        lay.setContentsMargins(*MARGINS)

        g, f = roomy_group("模型路径")
        self.qwen_dir = PathRow(directory=True)
        self.qwen_dir.setText(str(self.cfg.get("qwen3_model_dir", "")))
        f.addRow("Qwen3 定稿模型", self._path_dl_row(self.qwen_dir, "qwen3"))

        self.stream_dir = PathRow(directory=True)
        self.stream_dir.setText(str(self.cfg.get("stream_model_dir", "")))
        f.addRow(
            "流式预编辑模型",
            self._path_dl_row(self.stream_dir, "paraformer", extra="zipformer"),
        )

        self.whisper = PathRow(directory=False)
        self.whisper.setText(str(self.cfg.get("local_model", "")))
        f.addRow("Whisper 文件", self._path_dl_row(self.whisper, "whisper_small"))

        self.hotwords = QLineEdit(str(self.cfg.get("qwen3_hotwords", "")))
        self.hotwords.setMinimumHeight(34)
        self.hotwords.setPlaceholderText("专有名词1,专有名词2")
        f.addRow("Qwen3 热词", self.hotwords)
        f.addRow("", hint("逗号分隔，提升人名/术语识别。"))

        self.max_tok = QSpinBox()
        self.max_tok.setRange(32, 512)
        self.max_tok.setMinimumHeight(34)
        self.max_tok.setValue(int(self.cfg.get("qwen3_max_new_tokens", 128)))
        f.addRow("Qwen3 max tokens", self.max_tok)
        f.addRow("", hint("单次解码上限；流式短句 128 通常够用。"))
        lay.addWidget(g)

        g2, f2 = roomy_group("安装状态")
        self.model_status = QLabel(self._model_status_text())
        self.model_status.setWordWrap(True)
        self.model_status.setMinimumHeight(100)
        self.model_status.setStyleSheet(
            "background: #f4f4f5; border-radius: 8px; padding: 12px; color: #333;"
        )
        f2.addRow(self.model_status)
        f2.addRow("", hint(f"默认下载目录：{MODELS_ROOT}"))
        lay.addWidget(g2)
        lay.addStretch(1)
        return scroll_wrap(page)

    def _tab_network(self) -> QWidget:
        page = QWidget()
        lay = QVBoxLayout(page)
        lay.setSpacing(SPACING)
        lay.setContentsMargins(*MARGINS)

        g, f = roomy_group("HTTP 代理")
        self.proxy_on = QCheckBox("启用代理")
        proxy_val = str(self.cfg.get("proxy", "")).strip()
        if not proxy_val:
            proxy_val = str(self.cfg.get("proxy_remember", "")).strip()
        if "proxy_enabled" in self.cfg:
            enabled = bool(self.cfg.get("proxy_enabled"))
        else:
            enabled = bool(str(self.cfg.get("proxy", "")).strip())
        if not proxy_val:
            proxy_val = "http://127.0.0.1:7897"
        self.proxy_on.setChecked(enabled)
        self.proxy_on.toggled.connect(self._on_proxy_toggled)
        f.addRow(self.proxy_on)

        self.proxy = QLineEdit(proxy_val)
        self.proxy.setMinimumHeight(34)
        self.proxy.setPlaceholderText("http://127.0.0.1:7897")
        self.proxy.setEnabled(enabled)
        f.addRow("代理地址", self.proxy)
        f.addRow(
            "",
            hint("仅云端 provider=xai 需要。本地 Qwen3/Paraformer 可关代理。关闭后地址仍会记住。"),
        )
        lay.addWidget(g)

        g2, f2 = roomy_group("xAI 云端 STT")
        self.api_base = QLineEdit(str(self.cfg.get("api_base", "https://api.x.ai")))
        self.api_base.setMinimumHeight(34)
        f2.addRow("API Base", self.api_base)

        self.stt_path = QLineEdit(str(self.cfg.get("stt_path", "/v1/stt")))
        self.stt_path.setMinimumHeight(34)
        f2.addRow("STT 路径", self.stt_path)

        self.interim = QCheckBox("云端流式 interim 结果（WebSocket）")
        self.interim.setChecked(bool(self.cfg.get("interim_results", True)))
        f2.addRow(self.interim)

        self.endpointing = QSpinBox()
        self.endpointing.setRange(100, 5000)
        self.endpointing.setSuffix(" ms")
        self.endpointing.setMinimumHeight(34)
        self.endpointing.setValue(int(self.cfg.get("endpointing_ms", 400)))
        f2.addRow("云端 endpointing", self.endpointing)
        f2.addRow("", hint("云端静音端点检测，与本地 VAD 切句是两套逻辑。"))

        kts = self.cfg.get("keyterms") or []
        if isinstance(kts, list):
            kt_text = ", ".join(str(x) for x in kts)
        else:
            kt_text = str(kts)
        self.keyterms = QLineEdit(kt_text)
        self.keyterms.setMinimumHeight(34)
        self.keyterms.setPlaceholderText("term1, term2（云端 keyterms）")
        f2.addRow("云端关键词", self.keyterms)
        lay.addWidget(g2)
        lay.addStretch(1)
        return scroll_wrap(page)

    def _tab_advanced(self) -> QWidget:
        page = QWidget()
        lay = QVBoxLayout(page)
        lay.setSpacing(SPACING)
        lay.setContentsMargins(*MARGINS)

        g, f = roomy_group("采样与音频")
        self.sample_rate = QSpinBox()
        self.sample_rate.setRange(8000, 48000)
        self.sample_rate.setSingleStep(8000)
        self.sample_rate.setSuffix(" Hz")
        self.sample_rate.setMinimumHeight(34)
        self.sample_rate.setValue(int(self.cfg.get("sample_rate", 16000)))
        f.addRow("采样率", self.sample_rate)
        f.addRow("", hint("模型按 16 kHz 训练，一般不要改。"))
        lay.addWidget(g)

        g2, f2 = roomy_group("VAD / AGC（0 = 跟随 near_field 自动）")
        self.vad_rms = QDoubleSpinBox()
        self.vad_rms.setRange(0, 10000)
        self.vad_rms.setDecimals(1)
        self.vad_rms.setMinimumHeight(34)
        self.vad_rms.setValue(float(self.cfg.get("vad_speech_rms", 0.0)))
        f2.addRow("vad_speech_rms", self.vad_rms)
        f2.addRow("", hint("绝对能量门限。越大越难被旁人触发，过大会漏识。"))

        self.vad_snr = QDoubleSpinBox()
        self.vad_snr.setRange(0, 20)
        self.vad_snr.setDecimals(2)
        self.vad_snr.setMinimumHeight(34)
        self.vad_snr.setValue(float(self.cfg.get("vad_snr", 0.0)))
        f2.addRow("vad_snr", self.vad_snr)
        f2.addRow("", hint("相对环境底噪倍数。0=自动。"))

        self.agc = QDoubleSpinBox()
        self.agc.setRange(0, 32)
        self.agc.setDecimals(1)
        self.agc.setMinimumHeight(34)
        self.agc.setValue(float(self.cfg.get("agc_max_gain", 0.0)))
        f2.addRow("agc_max_gain", self.agc)
        f2.addRow("", hint("软件最大增益。过大易放大旁人；过热麦会自动压增益。"))
        lay.addWidget(g2)

        g3, f3 = roomy_group("说明")
        note = QTextEdit()
        note.setReadOnly(True)
        note.setMaximumHeight(140)
        note.setPlainText(
            "• 日常听写：provider=qwen3 + stream + dual_model。\n"
            "• 旁人干扰：先开「近讲优先」；仍乱再把 vad_snr 调到 2.8–3.5。\n"
            "• 错字变多：降低门限、加长「一句最长」、或关 dual_preedit。\n"
            "• 改模型/热键后务必「保存并重启」。"
        )
        f3.addRow(note)
        lay.addWidget(g3)
        lay.addStretch(1)
        return scroll_wrap(page)

    # ----- helpers ----------------------------------------------------------

    def _set_combo_data(self, combo: QComboBox, value: str) -> None:
        for i in range(combo.count()):
            if combo.itemData(i) == value or combo.itemText(i) == value:
                combo.setCurrentIndex(i)
                return
        # fallback: match prefix
        for i in range(combo.count()):
            data = combo.itemData(i)
            if data and str(data) == value:
                combo.setCurrentIndex(i)
                return

    def _combo_data(self, combo: QComboBox) -> str:
        data = combo.currentData()
        return str(data) if data is not None else combo.currentText()

    def _path_dl_row(
        self, row: PathRow, model_key: str, extra: str | None = None
    ) -> QWidget:
        w = QWidget()
        lay = QHBoxLayout(w)
        lay.setContentsMargins(0, 0, 0, 0)
        lay.setSpacing(10)
        lay.addWidget(row, 1)
        btn = QPushButton("下载并应用")
        btn.setMinimumHeight(32)
        btn.setToolTip(
            MODEL_CATALOG[model_key]["title"]
            + f" ({MODEL_CATALOG[model_key]['size_hint']})"
        )
        btn.clicked.connect(lambda: self.start_download(model_key, row))
        lay.addWidget(btn)
        if extra:
            b2 = QPushButton("Zipformer")
            b2.setMinimumHeight(32)
            b2.setToolTip(MODEL_CATALOG[extra]["title"])
            b2.clicked.connect(lambda: self.start_download(extra, row))
            lay.addWidget(b2)
        return w

    def _model_status_text(self) -> str:
        lines = []
        for key, meta in MODEL_CATALOG.items():
            ok, p = model_installed(key)
            mark = "✓" if ok else "✗"
            short = meta["title"].split("（")[0]
            lines.append(f"{mark}  {short}  →  {p if ok else '未安装'}")
        return "\n".join(lines)

    def _on_proxy_toggled(self, on: bool) -> None:
        self.proxy.setEnabled(on)

    def _apply_near_preset(self) -> None:
        preset = self.near_preset.currentData()
        if preset == "mild":
            self.near.setChecked(True)
            self.vad_rms.setValue(0)
            self.vad_snr.setValue(0)
            self.agc.setValue(0)
        elif preset == "strict":
            self.near.setChecked(True)
            self.vad_rms.setValue(900)
            self.vad_snr.setValue(3.2)
            self.agc.setValue(4)
        elif preset == "off":
            self.near.setChecked(False)
            self.vad_rms.setValue(0)
            self.vad_snr.setValue(0)
            self.agc.setValue(0)

    def refresh_status(self) -> None:
        self.status_label.setText(f"daemon　{daemon_status()}")
        self.model_status.setText(self._model_status_text())

    def restart_only(self) -> None:
        ok, detail = restart_daemon()
        self.refresh_status()
        if ok:
            QMessageBox.information(self, "完成", detail)
        else:
            QMessageBox.warning(self, "重启失败", detail)

    def open_journal(self) -> None:
        # Prefer konsole / kitty follow
        cmd = [
            "journalctl",
            "--user",
            "-u",
            "xai-dict",
            "-n",
            "80",
            "--no-pager",
        ]
        try:
            out = subprocess.check_output(cmd, text=True, timeout=5)
        except Exception as e:
            QMessageBox.warning(self, "日志", str(e))
            return
        dlg = QMessageBox(self)
        dlg.setWindowTitle("xai-dict 最近日志")
        dlg.setText("journalctl --user -u xai-dict -n 80")
        dlg.setDetailedText(out[-8000:] if out else "(空)")
        dlg.setStandardButtons(QMessageBox.StandardButton.Ok)
        dlg.exec()

    def open_models_dir(self) -> None:
        MODELS_ROOT.mkdir(parents=True, exist_ok=True)
        for c in (["xdg-open", str(MODELS_ROOT)], ["dolphin", str(MODELS_ROOT)]):
            try:
                subprocess.Popen(c)
                return
            except FileNotFoundError:
                continue

    # ----- download ---------------------------------------------------------

    def start_download(self, model_key: str, target_row: PathRow) -> None:
        if self._worker and self._worker.isRunning():
            QMessageBox.information(self, "下载中", "已有下载任务在进行，请稍候。")
            return
        meta = MODEL_CATALOG[model_key]
        ok, path = model_installed(model_key)
        if ok:
            r = QMessageBox.question(
                self,
                "已安装",
                f"{meta['title']}\n已存在：\n{path}\n\n仍要重新下载吗？",
                QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            )
            if r != QMessageBox.StandardButton.Yes:
                target_row.setText(str(path))
                self.model_status.setText(self._model_status_text())
                return

        self._dl_target_row = target_row
        self._progress = QProgressDialog(
            f"准备下载 {meta['title']} ({meta['size_hint']})…",
            "取消",
            0,
            100,
            self,
        )
        self._progress.setWindowTitle("下载模型")
        self._progress.setWindowModality(Qt.WindowModality.WindowModal)
        self._progress.setMinimumDuration(0)
        self._progress.setMinimumWidth(420)
        self._progress.setValue(0)
        self._progress.canceled.connect(self._cancel_download)

        self._worker = DownloadWorker(model_key, self)
        self._worker.progress.connect(self._on_dl_progress)
        self._worker.finished_ok.connect(self._on_dl_ok)
        self._worker.failed.connect(self._on_dl_fail)
        self._worker.start()

    def _cancel_download(self) -> None:
        if self._worker:
            self._worker.cancel()

    def _on_dl_progress(self, pct: int, msg: str) -> None:
        if self._progress:
            self._progress.setValue(pct)
            self._progress.setLabelText(msg)

    def _on_dl_ok(self, model_key: str, applied: str) -> None:
        if self._progress:
            self._progress.setValue(100)
            self._progress.close()
            self._progress = None
        row = getattr(self, "_dl_target_row", None)
        if row is not None:
            row.setText(applied)
        self.model_status.setText(self._model_status_text())
        QMessageBox.information(
            self,
            "下载完成",
            f"{MODEL_CATALOG[model_key]['title']}\n\n已安装到：\n{applied}\n\n"
            "路径已填入；请点「保存」或「保存并重启」。",
        )

    def _on_dl_fail(self, err: str) -> None:
        if self._progress:
            self._progress.close()
            self._progress = None
        QMessageBox.critical(
            self,
            "下载失败",
            f"{err}\n\n国内环境请确认代理，或手动下载后点「浏览」。",
        )

    # ----- collect / save ---------------------------------------------------

    def collect(self) -> dict:
        c = dict(self.cfg)
        c["provider"] = self._combo_data(self.provider)
        c["hotkey"] = self._combo_data(self.hotkey)
        c["language"] = self.language.text().strip() or "zh"
        c["paste"] = self.paste.isChecked()
        c["proxy_enabled"] = self.proxy_on.isChecked()
        c["proxy"] = self.proxy.text().strip()
        c["proxy_remember"] = self.proxy.text().strip()
        c["local_threads"] = self.threads.value()
        c["stream"] = self.stream.isChecked()
        c["dual_model"] = self.dual.isChecked()
        c["dual_preedit"] = self.preedit.isChecked()
        c["near_field"] = self.near.isChecked()
        c["stream_min_silence_ms"] = self.silence.value()
        c["stream_max_segment_ms"] = self.maxseg.value()
        c["stream_min_speech_ms"] = self.minspeech.value()
        c["stream_threads"] = self.stream_threads.value()
        c["qwen3_model_dir"] = self.qwen_dir.text()
        c["stream_model_dir"] = self.stream_dir.text()
        c["local_model"] = self.whisper.text()
        c["qwen3_hotwords"] = self.hotwords.text().strip()
        c["qwen3_max_new_tokens"] = self.max_tok.value()
        c["vad_speech_rms"] = self.vad_rms.value()
        c["vad_snr"] = self.vad_snr.value()
        c["agc_max_gain"] = self.agc.value()
        c["api_base"] = self.api_base.text().strip() or "https://api.x.ai"
        c["stt_path"] = self.stt_path.text().strip() or "/v1/stt"
        c["interim_results"] = self.interim.isChecked()
        c["endpointing_ms"] = self.endpointing.value()
        c["sample_rate"] = self.sample_rate.value()
        kt = self.keyterms.text().strip()
        c["keyterms"] = [t.strip() for t in kt.split(",") if t.strip()] if kt else []
        return c

    def save(self, restart: bool) -> None:
        cfg = self.collect()
        try:
            save_config(self.path, cfg)
            self.cfg = cfg
        except Exception as e:
            QMessageBox.critical(self, "保存失败", str(e))
            return
        active = cfg["proxy"] if cfg.get("proxy_enabled") else "（关闭）"
        msg = f"已写入\n{self.path}\n\n代理：{active}"
        if restart:
            ok, detail = restart_daemon()
            self.refresh_status()
            if ok:
                msg += f"\n\n{detail}"
            else:
                QMessageBox.warning(self, "已保存，但重启失败", f"{msg}\n\n{detail}")
                return
        QMessageBox.information(self, "完成", msg)

    def open_in_editor(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        if not self.path.is_file():
            save_config(self.path, self.collect())
        for cmd in (
            ["kate", str(self.path)],
            ["kwrite", str(self.path)],
            ["xdg-open", str(self.path)],
        ):
            try:
                subprocess.Popen(cmd)
                return
            except FileNotFoundError:
                continue
        QMessageBox.information(self, "打开", f"请手动编辑：\n{self.path}")


def main() -> int:
    path = config_path()
    if len(sys.argv) > 1 and sys.argv[1] in ("-h", "--help"):
        print(f"Usage: {sys.argv[0]} [config.toml]")
        return 0
    if len(sys.argv) > 1:
        path = Path(sys.argv[1])

    os.environ.setdefault("QT_QPA_PLATFORM", "wayland;xcb")

    app = QApplication(sys.argv)
    app.setApplicationName("xai-dict-settings")
    app.setStyle("Fusion")
    win = SettingsWindow(path)
    win.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
