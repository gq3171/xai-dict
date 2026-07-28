# xai-dict

Linux 语音听写（LazyTyper 风格）：**全局快捷键 → 说话 → 文字进当前光标**。

默认引擎：**双模型**

- **Zipformer 流式**（常驻）：边说边出预编辑（接近字级）  
- **Qwen3-ASR**（常驻）：停顿后定稿上屏（更准）

```
┌─────────────────────────────────────────────┐
│  热键 → xai-dict daemon                     │
│    mic ─┬→ Zipformer  → fcitx Preedit       │
│         └→ VAD 切句 → Qwen3 → Commit 定稿   │
└─────────────────────────────────────────────┘
```

## 快速开始（输入法式）

```bash
# 依赖
sudo pacman -S sherpa-onnx wl-clipboard wtype xdotool libnotify pipewire

# 模型 ~1.9GB（若尚未下载）
mkdir -p ~/.local/share/xai-dict/models && cd ~/.local/share/xai-dict/models
curl -fL -O https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2
tar xjf sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2 && rm *.tar.bz2

# 安装并启动守护进程
cd ~/Projects/rust/xai-dict
cargo install --path .
xai-dict install
```

### 绑定快捷键（KDE）

**系统设置 → 键盘 → 快捷键 → 命令（或自定义）**

| 项 | 值 |
|----|-----|
| 命令 | `~/.cargo/bin/xai-dict toggle` |
| 建议键 | `Meta+Alt+V` 或 `Alt_R` |

### 使用

1. 把光标放到任意输入框  
2. **按一次**快捷键 → 提示「录音中」→ 开始说话  
3. **边说边出字**（默认开启流式）：停顿约半秒后，该句自动上屏  
4. **再按一次**结束；尾句也会补上  

关闭流式（改回整段结束后再上屏）：在 `~/.config/xai-dict/config.toml` 设 `stream = false`。

```bash
xai-dict status     # idle / recording / transcribing
xai-dict whoami
journalctl --user -u xai-dict -f
```

## 命令一览

| 命令 | 作用 |
|------|------|
| `xai-dict daemon` | 前台跑守护进程 |
| `xai-dict toggle` | 切换录音（给快捷键用） |
| `xai-dict start` / `stop` | 显式开始/结束 |
| `xai-dict status` / `quit` | 状态 / 退出 daemon |
| `xai-dict install` | 装 systemd 服务 + desktop |
| `xai-dict` / `dict` | 一次性终端听写（Enter 结束） |
| `xai-dict --provider local` | 改用 Whisper |
| `xai-dict --provider xai` | 云端 xAI STT |

## 配置

`~/.config/xai-dict/config.toml`

```toml
provider = "qwen3"   # qwen3 | local | xai
qwen3_model_dir = ".../sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25"
qwen3_max_new_tokens = 512
qwen3_hotwords = ""
paste = true
local_threads = 6
```

## 与 LazyTyper / fcitx5-vinput 的关系

| | Lazy / fcitx5-vinput | xai-dict（当前） |
|--|---------------------|------------------|
| 触发 | fcitx5 抓键（可按住） | 全局快捷键 **点按切换** |
| 进字 | fcitx5 `commitString` | clipboard + wtype/xdotool |
| ASR | 内置 worker / 云 | **自有** Qwen3 / Whisper / xAI |
| 所有权 | 闭源 worker 曾出问题 | 全开源、可改 |

后续可做真正的 fcitx5 addon（按住说话 + 预编辑），当前方案已覆盖日常「任何输入框语音输入」。

## License

Apache-2.0
