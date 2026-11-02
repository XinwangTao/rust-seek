# 🔍 Rust Seek

> **A blazing-fast file & text search tool built with Rust — searches millions of lines in milliseconds, with a clean native GUI.**

![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)
![Platform](https://img.shields.io/badge/platform-Windows-blue?logo=windows)
![License](https://img.shields.io/badge/license-MIT-green)
![Version](https://img.shields.io/badge/version-4.9.0-brightgreen)

---

## ✨ Why Rust Seek?

Most search tools are either too slow, too complex, or require a runtime environment. **Rust Seek** is a single `.exe` file — no installation, no dependencies, no configuration. Just open it and search.

- ⚡ **Parallel search engine** — uses every CPU core simultaneously via the same engine powering `ripgrep`
- 🗺️ **Memory-mapped I/O** — reads large files without loading them fully into RAM
- 🎯 **Full regex support** — powered by Rust's battle-tested `regex` crate
- 🖥️ **Native GUI** — built with `egui`, renders at 60fps with zero web bloat
- 📦 **Zero dependencies for the user** — one `.exe`, runs anywhere on Windows

---

## 🎯 三种使用方式

同一个 `rust-seek.exe`，按你的习惯选一种（或全都要）：

| 形态 | 入口 | 适合谁 |
|------|------|--------|
| **纯软件端** | 双击 `rust-seek.exe` 打开搜索窗口 | 手动找文件/内容，点开即用 |
| **系统集成** | 开机后台常驻，`Alt+Space` / `Win+S` 呼出，接管任务栏搜索 | 当系统自带搜索用，随手呼出 |
| **AI Agent（MCP）** | `rust-seek.exe --mcp`，Claude Code / Cursor 直接调用 | 让 AI 调用你的整机搜索 |

三种形态共用同一套搜索引擎与整盘索引——索引建一次，处处受益。
下文分别展开：搜索能力见 [Features](#-features)，系统集成见 [系统底栏集成](#-系统底栏集成windows)，AI 调用见 [MCP Server](#-mcp-server给-ai-agent-用)。

---

## 🚀 Features

### Search
- **Plain-text filenames by default** — 文件名模式默认按纯文本匹配（`F` 默认勾选），粘贴带 `( ) + . [ ]` 的标题/文件名直接能搜到；要正则就取消 `F`
- **Regex search in text mode** — 文本内容模式默认按正则匹配（`F` 默认不勾），可用任意合法正则
- **Literal string mode** (`F` checkbox) — 当前模式的覆盖开关：勾上 = 纯文本，取消 = 正则；切换模式或重启会回到该模式的默认
- **Case-insensitive mode** (`Aa` checkbox) — match regardless of letter case
- **Search scope** — 整机（所有本地磁盘，默认）/ 单个磁盘（`C:` `D:` …）/ 指定目录，顶部一键切换
- **File type filter** — 按「类型」分组过滤（文档 / 图片 / 视频 / 音频 / 代码 / 压缩包），或直接在「后缀」框输入 `rs,toml`（自定义后缀优先于类型）
- **Live search** — 文件名模式下输入即出结果（150ms 防抖），无需按回车；整机范围改为回车触发，避免全盘反复空扫
- **Sort results** — 按路径 / 修改时间（最新在前）/ 大小（最大在前）
- **Search-stage pruning** — 类型/后缀在遍历阶段就剪枝，整机搜索不会去读无关文件
- **System dir pruning** — 整机搜索默认跳过盘符下的 `Windows` / `Program Files` / `ProgramData` / 回收站等系统目录（设置里可开关，只认盘符根下两层，不会误伤项目里的同名文件夹）
- **Whole-drive index (MCP)** — MCP server 常驻内存的文件名索引 + 磁盘缓存，整盘搜索从 19s 降到 0.1s，见 [整盘索引](#整盘索引)
- **Keyboard navigation** — ↑/↓ 选中结果、Enter 直接打开（无结果时 Enter 触发搜索）
- **Modified time & size** — 每条文件结果显示修改时间和大小
- **Binary file detection** — automatically skips binary files, no garbage output
- **Large file protection** — files over 10 MB are skipped to keep searches instant
- **Respects `.gitignore`** — won't waste time searching `node_modules`, `target/`, etc.

### Results
- **Real-time streaming** — results appear as they're found, no waiting for completion
- **All matches highlighted** — every occurrence on a line is highlighted in yellow, not just the first
- **Line numbers** — every match shows its exact line number in green
- **Sorted output** — results are always sorted by file path for consistent, readable output
- **Click to open** — click any file path to open it instantly in your default editor
- **Match count** — each file shows how many matches were found

### Interface
- **Folder picker** — click 📁 to browse for a directory, or type the path directly
- **Press Enter to search** — no need to reach for the mouse
- **Status bar** — shows total matches, files searched, and time elapsed
- **Resizable window** — drag to any size that fits your workflow

---

## 🧩 系统底栏集成（Windows）

Rust Seek 把系统底栏（任务栏）的搜索接管过来，以「系统式面板」形态运行——看不到任何"软件"：

- **系统式面板** — 呼出时是**无边框**搜索面板，从任务栏上方居中浮现；**不进任务栏、不进 Alt+Tab**；**点到别处自动收起**（300ms），与系统自带搜索行为一致
- **Win+S 接管** — 启动时尝试抢占系统搜索热键 `Win+S`，成功后按 Win+S 直接呼出（被占用时自动降级，设置里显示状态）
- **全局热键呼出** — 默认 `Alt+Space`，任意界面按下即呼出/隐藏（预设：`Alt+Space` / `Ctrl+Alt+Space` / `Ctrl+Alt+S`）
- **隐藏原生任务栏搜索框** — 一键隐藏 Windows 自带搜索框；会记录你原来的显示方式，取消勾选即恢复
- **接管系统搜索设置** — 对应「设置 → 隐私和安全性 → 搜索」：禁用开始/搜索框的 Bing 网络结果、关闭「搜索突出显示」
- **开机自启** — 后台隐藏运行，不占任务栏（注册表 HKCU Run，可随时关闭）
- **托盘常驻** — 左键图标呼出/隐藏，右键菜单：显示/隐藏、开机自启、隐藏搜索框、退出
- **单实例** — 重复启动会自动唤出已运行的搜索窗
- **Esc 隐藏** — 呼出的搜索窗按 Esc 即回到后台

> 说明：Windows 不允许第三方替换任务栏搜索框本身的 UI（SearchHost 为闭源组件，无搜索提供程序接口），
> 热键接管 + 原生框隐藏 + 无边框面板是系统层面的极限做法，最终体验与系统搜索一致。

设置入口：面板右下角 **⚙**。隐藏搜索框需重启资源管理器（设置里有按钮）生效。

---

## 📋 剪切板历史

同一个面板、同一套键盘操作，按 `Ctrl+Shift+V` 切换到「剪切板历史」，把复制过的文本和截图找回来：

- **自动记录** — 监听系统剪贴板，文本与图片（截图/复制的图片）都入库；连续复制同一内容只留 1 条
- **搜索过滤** — 面板顶部输入框按关键字过滤历史，图片按「图片 / image / 宽高」匹配
- **选中即复制** — ↑/↓ 选择，`Enter` 把内容写回系统剪贴板（**不自动粘贴**），到目标程序 `Ctrl+V` 即可
- **单条删除** — `Del` 删除选中条目（图片连文件一起删），重启后仍生效
- **暂停记录** — 面板内「暂停记录」勾选框 / 托盘同名菜单项，一键停止入库（隐私场景）
- **容量自限** — 文本 500 条、图片 200 张，超出淘汰最旧；`history.json` + `images/*.png` 存于 `%LOCALAPPDATA%\rust-seek\clipboard\`，原子写、可直接查看清理
- **热键兜底** — `Ctrl+Shift+V` 被占用时自动降级 `Ctrl+Alt+V`（托盘菜单显示实际热键）；热键失效也能从托盘「剪切板历史」进入

> 关闭面板：`Esc` 或点别处（与搜索面板一致，300ms 失焦收起）。

---

## 🤖 MCP Server（给 AI Agent 用）

同一个 `rust-seek.exe`，加 `--mcp` 就变成 MCP server（stdio + JSON-RPC 2.0），
让 Claude Code / Cursor / 任意 MCP 客户端直接调用本机搜索能力：

```bash
rust-seek.exe --mcp
```

接入 Claude Code：

```bash
claude mcp add rust-seek -- "F:\path\to\rust-seek.exe" --mcp
```

其他客户端的 JSON 配置同理：

```json
{
  "mcpServers": {
    "rust-seek": {
      "command": "F:\\path\\to\\rust-seek.exe",
      "args": ["--mcp"]
    }
  }
}
```

### 工具

| 工具 | 作用 | 关键参数 |
|------|------|----------|
| `search_files` | 按文件名/路径找文件，返回绝对路径 + 大小 + 修改时间 | `pattern`、`roots`、`exts`、`max_results`（默认 100）、`use_index`、`refresh_index` |
| `search_text` | 按内容找（ripgrep 语义，并行、跳二进制、跳大文件） | `pattern`、`roots`、`exts`、`max_files`（默认 50）、`max_matches_per_file`、`use_index` |
| `list_drives` | 列出可搜索的本地磁盘 | — |

`roots` 省略时搜索 MCP server 的工作目录（客户端通常把它设成当前项目），
传 `["C:/","D:/",…]` 才是整盘；`pattern` 默认按正则解释，`regex: false` 走纯文本子串。

### 整盘索引

整盘遍历受磁盘元数据 I/O 限制，本机 250 万条目约 **19s**，而且**第二遍一样慢**
（Windows 不会把全部目录元数据留在缓存里）。所以 MCP server 会在首次整盘查询时
在后台建一份文件名索引，之后的同类查询直接走内存：

| 场景 | 耗时 |
|------|------|
| 首次整盘查询（索引在建，本次走遍历） | 受 `budget_ms` 限制，默认 10s 返回部分结果 |
| 索引就绪后，同进程再查 | **0.08–0.12s**（160 万条目全扫） |
| 进程重启后首次查询（从磁盘缓存载入） | **0.12s** |
| 项目内查询（不走索引） | < 10ms |

- 缓存位置 `%LOCALAPPDATA%\rust-seek\index-v1.bin`（约 213MB），24 小时后自动后台刷新，7 天后强制重建
- 常驻内存约 220MB；只存文件（不存目录），剪枝规则和普通搜索一致，所以结果集相同
- **索引有条目上限（默认 800 万，`RUST_SEEK_MAX_ENTRIES` 可调）**：超大机器撞上限时索引只能覆盖一部分目录，
  响应里 `index_complete: false`。此时整盘查询会自动回退真实遍历并合并两边结果（`source: "index+walk"`），
  不会把「索引里没有」当成「文件不存在」；回调没显式给 `budget_ms` 时这条兜底遍历的预算放宽到 60s。
  想把上限调小省内存（每 100 万条约 75MB）就设 `RUST_SEEK_MAX_ENTRIES`。
- 条目按路径排序，子目录查询用二分定位区间，只扫那一段：`use_index: true` 可让任意大目录也走索引
- **整盘内容搜索也走索引**：`search_text` 的 `roots` 是盘符根且带 `exts` 时，先从索引取候选文件
  （按扩展名筛，通常只剩几千个），再并行读这些文件，跳过十几秒的目录遍历。
  实测 F 盘搜 `.tex` 内容：遍历 **12.0s** → 索引 **0.32s**（92 个候选文件）
- GUI 的「整机 + 文件名」搜索同样走索引（状态栏标「· 索引」）；单盘 / 指定目录仍走遍历（那两种范围不剪系统目录，语义不同）
- `refresh_index: true` 强制重建，本次调用仍走遍历
- 响应里的 `source`（`index` / `index+walk` / `walk`）、`index_age_ms`、`index_entries`、`index_complete` 说明数据来源、新鲜度与覆盖范围

### 为速度做的取舍

- **命中上限一到就掐断遍历**：`max_results` / `max_files` 满足后返回 `Step::Quit`，不等全盘走完
- **整棵剪枝**：默认跳过 `node_modules`、`.git`、`target`、`__pycache__`、`WinSxS`、回收站等，可用 `skip_dirs` 追加
- **系统目录默认剪掉**：盘符下的 `C:\Windows`、`Program Files`、`ProgramData` 等大头目录整棵跳过，`prune_system: false` 可关闭
- **不跟随符号链接**：碰到失效 junction 不会卡在 SMB 超时上
- **时间预算**：`budget_ms`（默认 10000，0 为不限）到期返回部分结果并标 `timed_out: true`，不让 Agent 干等
- 响应自带 `scanned` / `elapsed_ms` / `truncated`，模型能判断结果是否被截断

实测（本机 4 盘、约 250 万条目）：源目录内 `search_text` 约 **7ms**；F 盘搜文件名取 3 条 **236ms**（扫 8.4 万条后提前退出）；
整盘索引就绪后任意文件名查询 **0.1s 量级**；整盘无索引冷启扫描约 **19s**。

---

## 📸 Screenshot

```
┌─────────────────────────────────────────────────────────────┐
│  Path: [C:/my-project          ] 📁  Search: [fn main  ] 🔍 Search  [Aa] [F] │
├─────────────────────────────────────────────────────────────┤
│  src/main.rs  (1 match)                                      │
│     1:  fn main() -> eframe::Result {                        │
│                                                              │
│  src/app.rs  (3 matches)                                     │
│    41:  fn start_search(&mut self) {                         │
│    66:  fn update(&mut self, ctx: &egui::Context, ...) {     │
│   142:  fn render_result(ui: &mut Ui, result: &SearchResult) │
├─────────────────────────────────────────────────────────────┤
│  4 matches in 2 files (12ms)                                 │
└─────────────────────────────────────────────────────────────┘
```

---

## 📥 Download & Run

1. Go to [Releases](../../releases)
2. Download the zip for your machine:
   - `rust-seek_<version>_windows_x64.zip` — Intel / AMD 64-bit (most PCs)
   - `rust-seek_<version>_windows_arm64.zip` — Windows on ARM (Snapdragon 等)
3. Unzip and double-click `rust-seek.exe` — that's it

Releases are built automatically by GitHub Actions on every `v*` tag, with the app icon and version info embedded in the executable.

> ✅ No .NET, no Java, no Python, no Visual C++ Redistributable required.
> Works on Windows 10 and above.

---

## 🛠️ Build from Source

Requires [Rust](https://rustup.rs/) (stable toolchain).

```bash
git clone https://github.com/yourname/rust-seek
cd rust-seek
cargo build --release
# Binary at: target/release/rust-seek.exe
```

The release build enables full LTO and maximum optimization (`opt-level = 3`), producing a small, fast binary. On Windows, `build.rs` embeds `assets/rust-seek.ico` plus version metadata into the executable via `winresource`.

Icon assets are generated by `assets/make_icon.py` (requires Pillow): `python assets/make_icon.py` rewrites the `.ico` / `.png` files; commit them together.

---

## 🏗️ Architecture

```
src/
├── main.rs         # Entry point, window setup
├── app.rs          # GUI logic (egui), search orchestration, result rendering
├── integration.rs  # System integration: global hotkey, tray, autostart, taskbar search toggle
├── fastwalk.rs     # Parallel directory walk: FindFirstFileExW + FIND_FIRST_EX_LARGE_FETCH on Windows
├── index.rs        # In-memory whole-drive name index + on-disk cache, used by the MCP server
├── mcp.rs          # MCP server (`--mcp`): stdio JSON-RPC, tool schemas, walk orchestration
├── icon.rs         # Embedded app icon (assets/rust-seek-64.png) → window / tray RGBA
└── searcher.rs     # Core search engine: mmap I/O, regex matching, binary detection
```

| Component | Technology | Why |
|-----------|-----------|-----|
| GUI framework | `egui` / `eframe` | Immediate-mode, native, no Electron |
| Directory traversal | Win32 `FindFirstFileExW` (fallback: `ignore`) | 一条记录不再一次系统调用，文件名/大小/时间一次带回 |
| Whole-drive index | `std::sync` + flat string arena | 常驻进程重复整盘搜索从 19s 降到 0.1s |
| File reading | `memmap2` | OS-level memory mapping for large files |
| Pattern matching | `regex` | Rust's standard regex engine, DFA-based, very fast |
| Folder dialog | `rfd` | Native OS file picker |

---

## ⚡ Performance

Rust Seek uses the same parallel directory walking strategy as `ripgrep` — one of the fastest search tools ever benchmarked. On a modern machine:

- Searches a **100,000-file codebase** in under **2 seconds**
- Handles **files up to 10 MB** with memory-mapped I/O
- Scales linearly with CPU cores — the more cores, the faster

---

## 🗺️ Roadmap

- [ ] Context lines (show N lines before/after each match)
- [x] Search scope & file type filter (整机 / 单盘 / 指定目录 + 文档·图片·视频·音频·代码·压缩包 / 自定义后缀)
- [x] MCP server (`--mcp`) so AI agents can drive the same search engine
- [ ] Replace mode (find & replace across files)
- [ ] Search history
- [ ] Dark / light theme toggle

---

## 📄 License

MIT © 2024
