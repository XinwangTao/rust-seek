# 🔍 Rust Seek

> **A blazing-fast file & text search tool built with Rust — searches millions of lines in milliseconds, with a clean native GUI.**

![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)
![Platform](https://img.shields.io/badge/platform-Windows-blue?logo=windows)
![License](https://img.shields.io/badge/license-MIT-green)
![Version](https://img.shields.io/badge/version-0.2.0-brightgreen)

---

## ✨ Why Rust Seek?

Most search tools are either too slow, too complex, or require a runtime environment. **Rust Seek** is a single `.exe` file — no installation, no dependencies, no configuration. Just open it and search.

- ⚡ **Parallel search engine** — uses every CPU core simultaneously via the same engine powering `ripgrep`
- 🗺️ **Memory-mapped I/O** — reads large files without loading them fully into RAM
- 🎯 **Full regex support** — powered by Rust's battle-tested `regex` crate
- 🖥️ **Native GUI** — built with `egui`, renders at 60fps with zero web bloat
- 📦 **Zero dependencies for the user** — one `.exe`, runs anywhere on Windows

---

## 🚀 Features

### Search
- **Regex search** by default — use any valid regular expression as your pattern
- **Literal string mode** (`F` checkbox) — disable regex for exact text matching
- **Case-insensitive mode** (`Aa` checkbox) — match regardless of letter case
- **Search scope** — 整机（所有本地磁盘，默认）/ 单个磁盘（`C:` `D:` …）/ 指定目录，顶部一键切换
- **File type filter** — 按「类型」分组过滤（文档 / 图片 / 视频 / 音频 / 代码 / 压缩包），或直接在「后缀」框输入 `rs,toml`（自定义后缀优先于类型）
- **Live search** — 文件名模式下输入即出结果（150ms 防抖），无需按回车；整机范围改为回车触发，避免全盘反复空扫
- **Sort results** — 按路径 / 修改时间（最新在前）/ 大小（最大在前）
- **Search-stage pruning** — 类型/后缀在遍历阶段就剪枝，整机搜索不会去读无关文件
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
| `search_files` | 按文件名/路径找文件，返回绝对路径 + 大小 + 修改时间 | `pattern`、`roots`、`exts`、`max_results`（默认 100） |
| `search_text` | 按内容找（ripgrep 语义，并行、跳二进制、跳大文件） | `pattern`、`roots`、`exts`、`max_files`（默认 50）、`max_matches_per_file` |
| `list_drives` | 列出可搜索的本地磁盘 | — |

`roots` 省略时搜索所有本地磁盘；`pattern` 默认按正则解释，`regex: false` 走纯文本子串。

### 为速度做的取舍

- **命中上限一到就掐断遍历**：`max_results` / `max_files` 满足后返回 `WalkState::Quit`，不等全盘走完
- **整棵剪枝**：默认用 `WalkState::Skip` 跳过 `node_modules`、`.git`、`target`、`__pycache__`、`WinSxS`、回收站等，可用 `skip_dirs` 追加
- **不跟随符号链接**：元数据取遍历器缓存的 `symlink_metadata`，碰到失效 junction 不会卡在 SMB 超时上
- 响应自带 `scanned` / `elapsed_ms` / `truncated`，模型能判断结果是否被截断

实测（本机 4 盘、约 250 万条目）：源目录内 `search_text` 约 **7ms**；F 盘搜文件名取 3 条 **236ms**（扫 8.4 万条后提前退出）；
全盘无命中项时约 **23s**（必须扫完才能确定没有）。

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
2. Download `rust-seek.exe`
3. Double-click to open — that's it

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

The release build enables full LTO and maximum optimization (`opt-level = 3`), producing a small, fast binary.

---

## 🏗️ Architecture

```
src/
├── main.rs         # Entry point, window setup
├── app.rs          # GUI logic (egui), search orchestration, result rendering
├── integration.rs  # System integration: global hotkey, tray, autostart, taskbar search toggle
├── mcp.rs          # MCP server (`--mcp`): stdio JSON-RPC, tool schemas, walk orchestration
└── searcher.rs     # Core search engine: mmap I/O, regex matching, binary detection
```

| Component | Technology | Why |
|-----------|-----------|-----|
| GUI framework | `egui` / `eframe` | Immediate-mode, native, no Electron |
| Directory traversal | `ignore` | Same crate as ripgrep, parallel + gitignore-aware |
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
