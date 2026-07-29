# xai-dict

Linux 语音听写（LazyTyper 风格）：**全局快捷键 → 说话 → 文字进当前光标**。

默认引擎：**双模型**

- **Paraformer 流式**（常驻，可回退 Zipformer）：边说边出预编辑（接近字级）  
- **Qwen3-ASR**（常驻）：停顿后定稿上屏（更准）

```
┌─────────────────────────────────────────────┐
│  热键 → xai-dict daemon                     │
│    mic ─┬→ Paraformer → fcitx Preedit       │
│         └→ VAD 切句 → Qwen3 → Commit 定稿   │
└─────────────────────────────────────────────┘
```

## 安装

### Debian / Ubuntu（`.deb`，推荐）

从 [GitHub Releases](https://github.com/gq3171/xai-dict/releases) 下载：

```bash
sudo dpkg -i xai-dict_*_amd64.deb
# 若缺依赖：sudo apt -f install
xai-dict config          # 图形界面下载模型
systemctl --user daemon-reload
systemctl --user enable --now xai-dict
```

本地打包（开发机需已装 `sherpa-onnx` C API + `onnxruntime`）：

```bash
./packaging/build-deb.sh
# → target/debian/xai-dict_<version>_amd64.deb
```

**模型不进 deb**（约 2GB+），用设置界面「下载并应用」。

### 从源码（Arch 等）

```bash
# 依赖
sudo pacman -S sherpa-onnx wl-clipboard wtype xdotool libnotify pipewire

# 模型：xai-dict config 里一键下载，或手动放到 ~/.local/share/xai-dict/models

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
| `xai-dict config` / `config gui` | **图形设置界面** |
| `xai-dict config show` / `set` / `edit` | 查看 / 改键 / 编辑器打开 |
| `xai-dict` / `dict` | 一次性终端听写（Enter 结束） |
| `xai-dict --provider local` | 改用 Whisper |
| `xai-dict --provider xai` | 云端 xAI STT |
| `./packaging/build-deb.sh` | 打 amd64 `.deb`（不含模型） |

打 tag 发版会走 GitHub Actions：`git tag v0.1.0 && git push github v0.1.0`

## 配置

图形界面（推荐）：

```bash
xai-dict config          # 或: xai-dict config gui
# 应用菜单 →「xai-dict 设置」
```

也可直接编辑 `~/.config/xai-dict/config.toml`

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
