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
```

**安装后会怎样（为「别人装完能用」设计）：**

| 项 | 行为 |
|----|------|
| 二进制 | `/usr/bin/xai-dict`，worker + 库在 `/usr/lib/xai-dict` |
| systemd 用户单元 | `/usr/lib/systemd/user/xai-dict.service` → `ExecStart=/usr/bin/xai-dict daemon` |
| postinst | 对 `SUDO_USER` **enable + start**；尽量把用户加入 **input** 组 |
| 登录自启 | systemd `WantedBy=default.target` + `/etc/xdg/autostart` 调用 `xai-dict ensure` |
| 模型 | **不在 deb 内**，需一次：`xai-dict config` 下载 |

```bash
xai-dict config                 # 必做一次：下模型 + 设热键
systemctl --user status xai-dict
# 若刚被加入 input 组：重新登录后全局热键才生效
# 可选 fcitx 插件（与拼音并存 Super+V）:
xai-dict install --fcitx
```

若服务没起来：`xai-dict ensure` 或  
`systemctl --user enable --now xai-dict`。

本地打包（开发机需已装 `sherpa-onnx` C API + `onnxruntime`）：

```bash
./packaging/build-deb.sh
# → target/debian/xai-dict_<version>_amd64.deb
```

### 从源码（Arch 等）

```bash
# 依赖
sudo pacman -S sherpa-onnx wl-clipboard wtype xdotool libnotify pipewire

# 模型：xai-dict config 里一键下载，或手动放到 ~/.local/share/xai-dict/models

cargo install --path .
xai-dict install
xai-dict install --fcitx   # 可选
```

## 使用

1. 把光标放到任意输入框  
2. **点按模式（默认）**：按一次热键 →「录音中」→ 说话 → 再按结束  
3. **PTT 模式**：配置 `hotkey_mode = "ptt"` → **按住**热键说话，**松手**定稿  
4. **边说边出字**（默认流式 + 双模型）；停顿后该句 Qwen3 定稿上屏  

热键：

| 来源 | 默认 |
|------|------|
| daemon 内置（需 `input` 组） | 右 Alt（`hotkey`） |
| fcitx5 Module | **Super+V** / **F9**（与拼音并存） |
| 系统快捷键 | 绑 `xai-dict toggle` |

fcitx 插件**不会**抢占右 Alt / AltGr（避免破坏符号输入、也避免与 daemon 双触发）。  
关闭 daemon 热键：`hotkey = "none"`，只保留 Super+V / F9。

```bash
xai-dict status     # idle / recording / transcribing
xai-dict whoami
xai-dict mic-test   # 3 秒电平
xai-dict mic-list   # 输入设备列表
journalctl --user -u xai-dict -f   # 搜 metric: 看每句延迟
```

## 命令一览

| 命令 | 作用 |
|------|------|
| `xai-dict daemon` | 前台跑守护进程 |
| `xai-dict toggle` / `start` / `stop` | 切换 / 开始 / 结束录音 |
| `xai-dict status` / `quit` | 状态 / 退出 daemon |
| `xai-dict install [--fcitx]` | 装 systemd + desktop；可选 fcitx Module |
| `xai-dict config` / `config gui` | **图形设置**（PTT、设备、热词、模型） |
| `xai-dict config show` / `set` / `edit` | 查看 / 改键 / 编辑器 |
| `xai-dict mic-test` / `mic-list` | 麦克风电平 / 设备列表 |
| `xai-dict` / `dict` | 一次性终端听写 |
| `./packaging/build-deb.sh` | 打 amd64 `.deb`（不含模型） |

## 配置

图形界面（推荐）：

```bash
xai-dict config          # 或: xai-dict config gui
# 应用菜单 →「xai-dict 设置」
```

也可编辑 `~/.config/xai-dict/config.toml`：

```toml
provider = "qwen3"          # qwen3 | local | xai
hotkey = "rightalt"         # rightalt | leftalt | none
hotkey_mode = "toggle"      # toggle | ptt
input_device = ""            # 空=默认；见 mic-list
stream = true
dual_model = true
dual_preedit = true
near_field = true
qwen3_hotwords = "专有名词,项目名"
qwen3_model_dir = ".../sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25"
stream_model_dir = ".../sherpa-onnx-streaming-paraformer-bilingual-zh-en"
paste = true
local_threads = 6
```

改配置后：`systemctl --user restart xai-dict`。

### 调参提示

| 现象 | 建议 |
|------|------|
| 旁人串音 | `near_field = true`；或调高 `vad_snr` |
| 自己声音轻 / USB 麦 | `xai-dict mic-test`；适当增大系统输入；或 `near_field = false` |
| 预编辑乱跳 | `dual_preedit = false` |
| 人名术语错 | `qwen3_hotwords`（设置页可粘贴追加） |
| 想按住说 | `hotkey_mode = "ptt"` |

日志中的 `metric:` 行：`qwen_ms`（定稿耗时）、`first_preedit_ms`、`dropped`（丢句）。

### 离线 CER 冒烟

```bash
python3 scripts/cer_smoke.py --ref "你好世界" --hyp "你好是界"
# 或 TSV: 参考\\t假设  每行一条
python3 scripts/cer_smoke.py --file samples.tsv
```

## 与 LazyTyper / fcitx5-vinput 的关系

| | Lazy / fcitx5-vinput | xai-dict（当前） |
|--|---------------------|------------------|
| 触发 | fcitx5 抓键（可按住） | 全局热键 **toggle / PTT** + fcitx Super+V |
| 上屏 | fcitx5 `commitString` | **优先** fcitx `Commit` / `Preedit`，否则 clipboard + wtype/ydotool |
| ASR | 内置 worker / 云 | **自有** Qwen3 / Paraformer / Whisper / xAI |
| 所有权 | 闭源 worker 曾出问题 | 全开源、可改 |

### fcitx5 插件（与拼音并存）

```bash
xai-dict install --fcitx
# 或: cd fcitx5-xaidict && ./install-user.sh
```

这是 **常驻 Module**，**不用**切换到「语音听写」输入法，**拼音照常打字**。

| 操作 | 说明 |
|------|------|
| 输入法 | 保持 **拼音**（或键盘）即可 |
| 听写热键 | **Super+V** 或 **F9** |
| 上屏 | daemon → fcitx `Commit` / `Preedit` |
| 也可用 | daemon 全局热键（默认右 Alt，需 `input` 组） |

## License

Apache-2.0
