use crate::fastwalk::{self, Step};
use crate::integration;
use crate::searcher::{
    path_in_noise_dir, path_is_system_root, search_file, search_filename_meta, SearchResult,
};
use eframe::egui;
use egui::{Color32, RichText, ScrollArea, TextEdit, Ui};
use regex::RegexBuilder;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

enum SearchMsg {
    Result(SearchResult),
    /// 结束：耗时、遍历到的条目数、实际搜索范围（便于看清整机到底搜了哪些盘）
    Done { ms: u128, visited: u64, scope: String },
}

#[derive(PartialEq, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum SearchMode { Text, Filename }

const MAX_HISTORY: usize = 20;
const COLLAPSE_THRESHOLD: usize = 5;
/// Cap results to avoid rendering tens of thousands of items
const MAX_RESULTS: usize = 2000;

fn default_true() -> bool { true }

/// 结果排序方式
#[derive(PartialEq, Clone, Copy, serde::Serialize, serde::Deserialize, Default)]
enum SortKey {
    #[default]
    Path,
    Modified,
    Size,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Path => "路径",
            SortKey::Modified => "修改时间",
            SortKey::Size => "大小",
        }
    }
    const ALL: [SortKey; 3] = [SortKey::Path, SortKey::Modified, SortKey::Size];
}

/// 搜索范围
#[derive(PartialEq, Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
enum Scope {
    /// 整机：所有本地磁盘
    #[default]
    Machine,
    /// 单个磁盘，如 "F:"
    Drive(String),
    /// 自定义目录（路径输入框 / 拖入的文件夹）
    Folder,
}

impl Scope {
    fn label(&self) -> String {
        match self {
            Scope::Machine => "整机".to_string(),
            Scope::Drive(d) => format!("{d} 盘"),
            Scope::Folder => "指定目录".to_string(),
        }
    }
}

/// 文件类型筛选（按扩展名分组，作用于搜索阶段，避免整机搜索读取无关文件）
#[derive(PartialEq, Clone, Copy, serde::Serialize, serde::Deserialize, Default)]
enum FileKind {
    #[default]
    All,
    Doc,
    Image,
    Video,
    Audio,
    Code,
    Archive,
}

impl FileKind {
    fn label(self) -> &'static str {
        match self {
            FileKind::All => "全部类型",
            FileKind::Doc => "文档",
            FileKind::Image => "图片",
            FileKind::Video => "视频",
            FileKind::Audio => "音频",
            FileKind::Code => "代码",
            FileKind::Archive => "压缩包",
        }
    }

    const ALL: [FileKind; 7] = [
        FileKind::All,
        FileKind::Doc,
        FileKind::Image,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Code,
        FileKind::Archive,
    ];

    fn exts(self) -> &'static [&'static str] {
        match self {
            FileKind::All => &[],
            FileKind::Doc => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf",
                "csv", "odt", "ods", "odp", "epub", "mobi",
            ],
            FileKind::Image => &[
                "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff",
                "heic", "raw", "cr2", "nef", "psd",
            ],
            FileKind::Video => &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "rmvb", "mpg",
            ],
            FileKind::Audio => &[
                "mp3", "wav", "flac", "ogg", "aac", "m4a", "wma", "ape", "opus", "mid",
            ],
            FileKind::Code => &[
                "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "cpp", "cc", "h", "hpp",
                "java", "cs", "rb", "php", "sh", "bat", "ps1", "json", "toml", "yaml", "yml",
                "xml", "html", "css", "scss", "sql", "lua", "kt", "swift", "vue",
            ],
            FileKind::Archive => &[
                "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "tgz", "iso", "cab",
            ],
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Prefs {
    path_history: Vec<String>,
    pattern_history: Vec<String>,
    last_path: String,
    last_mode: Option<SearchMode>,
    #[serde(default)]
    hotkey_preset: usize,
    /// 呼出时停靠在屏幕底部（任务栏上方居中），Esc 隐藏
    #[serde(default = "default_true")]
    dock_bottom: bool,
    /// 隐藏任务栏搜索框之前 SearchboxTaskbarMode 的原值（None = 原先没有该值）
    #[serde(default)]
    searchbox_prev: Option<Option<u32>>,
    /// 结果排序方式
    #[serde(default)]
    sort_key: SortKey,
    /// 后缀筛选（逗号分隔，如 "rs,toml"）
    #[serde(default)]
    ext_filter: String,
    /// 搜索范围（整机 / 指定磁盘 / 指定目录），默认整机
    #[serde(default)]
    scope: Scope,
    /// 文件类型筛选
    #[serde(default)]
    file_kind: FileKind,
    /// 整机搜索时跳过系统大头目录（Windows / Program Files / ProgramData…）
    #[serde(default = "default_true")]
    prune_system: bool,
}

impl Prefs {
    fn load() -> Self {
        prefs_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    #[allow(dead_code)]
    fn save(&self) {
        if let Some(p) = prefs_path() {
            if let Ok(s) = serde_json::to_string(self) {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, s);
            }
        }
    }
}

fn prefs_path() -> Option<std::path::PathBuf> {
    Some(dirs::data_local_dir()?.join("rust-seek").join("prefs.json"))
}

pub struct App {
    pattern: String,
    search_path: String,
    ignore_case: bool,
    fixed_string: bool,
    mode: SearchMode,
    results: Vec<SearchResult>,
    total_matches: usize,   // incremental, avoids O(n) sum each frame
    result_capped: bool,    // true when MAX_RESULTS was hit
    collapsed: HashSet<String>,
    expanded: HashSet<String>,
    status: String,
    live_count: usize,
    live_matches: usize,
    path_error: Option<String>,
    regex_error: Option<String>,
    last_pat: String,       // cache key: pattern
    last_ic: bool,          // cache key: ignore_case
    last_fs: bool,          // cache key: fixed_string
    filter: String,         // post-search filter on results
    searching: bool,
    rx: Option<Receiver<SearchMsg>>,
    cancel: Option<Arc<AtomicBool>>,
    last_repaint: Instant,
    prefs: Prefs,
    needs_focus: bool,
    // —— 搜索增强 ——
    sort: SortKey,              // 当前排序
    ext_filter: String,         // 后缀筛选输入框内容
    selected_path: Option<String>, // 键盘导航选中的结果
    live_typed: String,         // 实时搜索已触发的关键词
    live_prev: String,          // 上一帧的关键词（检测新输入）
    live_edit: Instant,         // 最近一次输入时间（防抖）
    searched_pattern: String,   // 最近一次搜索的关键词
    // —— 搜索范围 / 类型筛选 ——
    scope: Scope,               // 整机 / 指定磁盘 / 指定目录
    file_kind: FileKind,        // 类型筛选（文档/图片/…）
    drives: Vec<String>,        // 本地磁盘列表（启动时枚举）
    research_at: Option<Instant>, // 类型/后缀改动后的延迟重搜计时
    // —— 系统集成状态 ——
    force_quit: bool,
    /// eframe 首帧后强制显示窗口（防白闪），`--hidden` 启动需在第二帧再隐藏
    hide_on_start: bool,
    first_frame_done: bool,
    /// 面板样式低频校正计时
    last_style_check: Option<Instant>,
    /// 失焦自动隐藏的计时（系统式面板行为）
    unfocused_since: Option<Instant>,
    /// 与热键/托盘/唤出线程共享的桥接状态
    bridge: Arc<integration::Bridge>,
    hotkey: Option<integration::HotkeySetup>,
    active_hotkey_preset: usize,
    hotkey_error: Option<String>,
    tray: Option<integration::TraySetup>,
    autostart: bool,
    show_settings: bool,
    explorer_confirm: bool,
}

impl App {
    pub fn new(start_hidden: bool) -> Self {
        let prefs = Prefs::load();
        let prefs_sort_key = prefs.sort_key;
        let prefs_ext = prefs.ext_filter.clone();
        let cwd = if prefs.last_path.is_empty() {
            std::env::current_dir().unwrap_or_default()
                .to_string_lossy().replace('\\', "/")
        } else {
            prefs.last_path.clone()
        };
        let drives = integration::local_drives();
        // 上次选的磁盘已不存在（U 盘拔出等）时回落到整机
        let scope = match &prefs.scope {
            Scope::Drive(d) if !drives.iter().any(|x| x == d) => Scope::Machine,
            s => s.clone(),
        };
        let file_kind = prefs.file_kind;

        // 与热键/托盘/唤出线程共享的桥
        let bridge = Arc::new(integration::Bridge::new());
        bridge.panel.store(prefs.dock_bottom, Ordering::Relaxed);
        // 全局热键（呼出/隐藏）
        let (hotkey, hotkey_error) = match integration::setup_hotkey(prefs.hotkey_preset, bridge.clone()) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e)),
        };
        // 托盘常驻
        let hotkey_label = hotkey.as_ref().map(|h| h.label).unwrap_or("Rust Seek");
        let tray = integration::setup_tray(hotkey_label, bridge.clone()).ok();
        // 其他实例唤出事件
        integration::spawn_show_watcher(bridge.clone());

        Self {
            pattern: String::new(),
            search_path: cwd,
            ignore_case: true,
            fixed_string: false,
            mode: prefs.last_mode.unwrap_or(SearchMode::Filename),
            results: Vec::new(),
            total_matches: 0,
            result_capped: false,
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            status: String::from("就绪"),
            live_count: 0,
            live_matches: 0,
            path_error: None,
            regex_error: None,
            last_pat: String::new(),
            last_ic: false,
            last_fs: false,
            filter: String::new(),
            searching: false,
            rx: None,
            cancel: None,
            last_repaint: Instant::now(),
            prefs,
            needs_focus: true,
            sort: prefs_sort_key,
            ext_filter: prefs_ext,
            selected_path: None,
            live_typed: String::new(),
            live_prev: String::new(),
            live_edit: Instant::now(),
            searched_pattern: String::new(),
            scope,
            file_kind,
            drives,
            research_at: None,
            force_quit: false,
            hide_on_start: start_hidden,
            first_frame_done: false,
            last_style_check: None,
            unfocused_since: None,
            bridge,
            hotkey,
            active_hotkey_preset: 0,
            hotkey_error,
            tray,
            autostart: integration::autostart_enabled(),
            show_settings: false,
            explorer_confirm: false,
        }
        .with_active_preset()
    }

    /// 记录实际生效的热键预设（注册成功与否都同步，避免每次重绘反复尝试）
    fn with_active_preset(mut self) -> Self {
        self.active_hotkey_preset = self.prefs.hotkey_preset;
        self
    }

    fn hotkey_label(&self) -> String {
        integration::hotkey_presets()
            .get(self.prefs.hotkey_preset)
            .map(|(l, _)| l.to_string())
            .unwrap_or_else(|| "未知".to_string())
    }

    fn save_prefs(&self) {
        if let Some(p) = prefs_path() {
            if let Ok(s) = serde_json::to_string(&self.prefs) {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, s);
            }
        }
    }

    /// 按当前排序方式重排结果
    fn sort_results(&mut self) {
        match self.sort {
            SortKey::Path => self.results.sort_unstable_by(|a, b| a.path.cmp(&b.path)),
            SortKey::Modified => self.results.sort_unstable_by(|a, b| {
                b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path))
            }),
            SortKey::Size => self.results.sort_unstable_by(|a, b| {
                b.file_size.cmp(&a.file_size).then_with(|| a.path.cmp(&b.path))
            }),
        }
    }

    /// 解析后缀筛选输入（"rs, toml" → ["rs", "toml"]）
    fn ext_list(&self) -> Vec<String> {
        self.ext_filter
            .split([',', ';', ' '])
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 实际生效的扩展名筛选：自定义后缀优先，否则用「类型」分组
    fn ext_filter_active(&self) -> Vec<String> {
        let custom = self.ext_list();
        if !custom.is_empty() {
            return custom;
        }
        self.file_kind.exts().iter().map(|s| s.to_string()).collect()
    }

    /// 本次搜索的根路径列表（整机 = 所有本地磁盘根）
    fn search_roots(&self) -> Vec<String> {
        match &self.scope {
            Scope::Machine if self.drives.is_empty() => vec![self.search_path.clone()],
            Scope::Machine => self.drives.iter().map(|d| format!("{d}/")).collect(),
            Scope::Drive(d) => vec![format!("{d}/")],
            Scope::Folder => vec![self.search_path.clone()],
        }
    }

    /// 单条结果是否通过「路径过滤 + 后缀筛选」
    fn result_visible(&self, r: &SearchResult, exts: &[String], filter_lc: &str) -> bool {
        if !filter_lc.is_empty() && !r.path.to_lowercase().contains(filter_lc) {
            return false;
        }
        if !exts.is_empty() {
            let name = r.path.rsplit('/').next().unwrap_or("");
            let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            match ext {
                Some(e) if exts.iter().any(|x| *x == e) => {}
                _ => return false,
            }
        }
        true
    }

    fn sync_tray_checks(&mut self) {
        if let Some(tray) = self.tray.as_ref() {
            tray.autostart_item.set_checked(self.autostart);
            tray.hide_search_item.set_checked(integration::taskbar_search_hidden());
        }
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    /// 消费外部事件（热键/托盘/其他实例）设置的桥接标志。
    /// 显示/隐藏本身由处理器用 Win32 直接完成，这里只做 egui 侧的善后。
    fn poll_bridge_events(&mut self) {
        // 窗口刚被外部呼出：清空上一次查询，聚焦输入框
        if self.bridge.pending_show.swap(false, Ordering::Relaxed) {
            self.cancel_search();
            self.pattern.clear();
            self.results.clear();
            self.total_matches = 0;
            self.result_capped = false;
            self.filter.clear();
            self.status = "就绪".to_string();
            self.needs_focus = true;
        }
    }

    /// record_history=false 用于实时搜索的中间态，避免刷爆历史记录
    fn start_search(&mut self, record_history: bool) {
        if self.pattern.is_empty() { return; }
        // 只保留真实存在的根：拔掉的 U 盘、失效的映射盘直接跳过，而不是整体报错
        let roots: Vec<String> = self
            .search_roots()
            .into_iter()
            .filter(|p| std::path::Path::new(p.as_str()).exists())
            .collect();
        if roots.is_empty() {
            self.path_error = Some(format!("没有可搜索的路径：{}", self.scope.label()));
            return;
        }
        let scope_label = match &self.scope {
            Scope::Machine => format!(
                "整机（{}）",
                roots.iter()
                    .map(|r| r.trim_end_matches('/'))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Scope::Drive(d) => format!("{d} 盘"),
            Scope::Folder => format!("目录 {}", self.search_path),
        };
        self.path_error = None;
        self.cancel_search();
        self.searched_pattern = self.pattern.clone();
        self.selected_path = None;
        self.research_at = None;

        if record_history {
            if self.scope == Scope::Folder {
                push_history(&mut self.prefs.path_history, self.search_path.clone());
            }
            push_history(&mut self.prefs.pattern_history, self.pattern.clone());
        }
        self.prefs.last_path = self.search_path.clone();
        self.prefs.last_mode = Some(self.mode);
        self.prefs.scope = self.scope.clone();
        self.prefs.file_kind = self.file_kind;
        // Save prefs async to avoid blocking UI
        let prefs_clone = serde_json::to_string(&self.prefs).ok();
        thread::spawn(move || {
            if let (Some(p), Some(s)) = (prefs_path(), prefs_clone) {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, s);
            }
        });

        // Fixed string: always escape. Otherwise treat as regex.
        let pat = if self.fixed_string {
            regex::escape(&self.pattern)
        } else {
            self.pattern.clone()
        };
        let re = match RegexBuilder::new(&pat)
            .case_insensitive(self.ignore_case)
            .unicode(true)
            .build()
        {
            Ok(r) => r,
            Err(e) => { self.status = format!("无效的正则: {e}"); return; }
        };

        self.results.clear();
        self.total_matches = 0;
        self.result_capped = false;
        self.collapsed.clear();
        self.expanded.clear();
        self.live_count = 0;
        self.live_matches = 0;
        self.searching = true;
        self.status = "搜索中…".to_string();

        let (tx, rx): (Sender<SearchMsg>, Receiver<SearchMsg>) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.rx = Some(rx);
        self.cancel = Some(cancelled.clone());

        let threads = num_cpus::get();
        let mode = self.mode;
        let exts = self.ext_filter_active();
        // 整机搜索时跳过系统目录里的条目；用户手动指定目录时不干预
        let prune_system = self.prefs.prune_system && matches!(self.scope, Scope::Machine);
        let walk_roots: Vec<std::path::PathBuf> =
            roots.iter().map(std::path::PathBuf::from).collect();

        thread::spawn(move || {
            let start = Instant::now();
            let opts = fastwalk::Options { include_hidden: false, threads };
            let visited = fastwalk::walk(&walk_roots, &opts, &cancelled, |path, info| {
                // 回收站 / 系统还原 / WinSxS，以及盘符下的 Windows、Program Files 等大头目录：
                // 命中的是目录就整棵剪掉，否则这些目录会被逐条遍历，白白拖慢整机搜索。
                // depth == 0 是用户给的根，不剪。
                if prune_system
                    && info.depth > 0
                    && (path_in_noise_dir(path) || path_is_system_root(path))
                {
                    return if info.is_dir { Step::Skip } else { Step::Continue };
                }
                // 类型/后缀预筛：在读取文件之前剪枝，整机搜索才不会白读全盘
                if !exts.is_empty() {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !exts.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                        return Step::Continue;
                    }
                }
                let result = match mode {
                    // 大小/时间直接用目录枚举带回的，命中时零系统调用
                    SearchMode::Filename => {
                        search_filename_meta(path, &re, || Some((info.size, info.modified)))
                    }
                    SearchMode::Text => {
                        if info.is_dir {
                            return Step::Continue;
                        }
                        search_file(path, &re, 10 * 1024 * 1024).ok().flatten()
                    }
                };
                if let Some(r) = result {
                    if tx.send(SearchMsg::Result(r)).is_err() {
                        return Step::Quit;
                    }
                }
                Step::Continue
            });
            let _ = tx.send(SearchMsg::Done {
                ms: start.elapsed().as_millis(),
                visited,
                scope: scope_label,
            });
        });
    }

    fn cancel_search(&mut self) {
        if let Some(c) = self.cancel.take() { c.store(true, Ordering::Relaxed); }
        self.rx = None;
        self.searching = false;
    }

}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 面板样式：首帧应用 + 每 2 秒校正（winit/系统可能重设样式）
        let hwnd_now = self.bridge.hwnd.load(Ordering::Relaxed);
        if hwnd_now == 0 {
            let hwnd = integration::find_main_hwnd();
            if hwnd != 0 {
                self.bridge.hwnd.store(hwnd, Ordering::Relaxed);
            }
        }
        let need_check = self
            .last_style_check
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(2));
        if need_check {
            self.last_style_check = Some(Instant::now());
            let hwnd = self.bridge.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                integration::ensure_panel_style(hwnd, self.prefs.dock_bottom);
            }
        }

        // eframe 首帧渲染后会强制 set_visible(true)，`--hidden` 启动时在第二帧隐藏回来
        if self.hide_on_start {
            if self.first_frame_done {
                self.hide_on_start = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.first_frame_done = true;
                ctx.request_repaint();
            }
        }

        // 消费外部呼出事件（Win32 显示已完成，这里做 egui 侧善后）
        self.poll_bridge_events();

        // 关闭窗口 = 隐藏到托盘（托盘菜单“退出”才真正退出）
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.force_quit {
                return;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
            return;
        }

        // Drain results
        if self.rx.is_some() {
            let mut done = false;
            let mut got = false;
            loop {
                match self.rx.as_ref().unwrap().try_recv() {
                    Ok(SearchMsg::Result(r)) => {
                        if self.results.len() < MAX_RESULTS {
                            self.live_matches += r.matches.len();
                            self.results.push(r);
                            self.live_count += 1;
                        } else if !self.result_capped {
                            // 达到上限就停止遍历：整机搜索没必要把全盘走完
                            self.result_capped = true;
                            if let Some(c) = self.cancel.as_ref() {
                                c.store(true, Ordering::Relaxed);
                            }
                        }
                        got = true;
                    }
                    Ok(SearchMsg::Done { ms, visited, scope }) => {
                        self.sort_results();
                        self.total_matches = self.live_matches; // already incremental
                        self.status = if self.results.is_empty() {
                            format!("未找到结果（{}ms，扫描 {} 项）· 范围 {}", ms, visited, scope)
                        } else if self.mode == SearchMode::Filename {
                            if self.result_capped {
                                format!("找到 {}+ 个文件（已截断）({}ms) · 范围 {}", MAX_RESULTS, ms, scope)
                            } else {
                                format!("找到 {} 个文件 ({}ms) · 范围 {}", self.results.len(), ms, scope)
                            }
                        } else {
                            format!(
                                "{} 处匹配，共 {} 个文件 ({}ms) · 范围 {}",
                                self.total_matches,
                                self.results.len(),
                                ms,
                                scope
                            )
                        };
                        self.searching = false;
                        self.cancel = None;
                        done = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            if done { self.rx = None; }
            if got {
                self.status = if self.mode == SearchMode::Filename {
                    format!("搜索中… 已找到 {} 个文件{}", self.live_count,
                        if self.result_capped { "（已达上限）" } else { "" })
                } else {
                    format!("搜索中… {} 处匹配 / {} 个文件", self.live_matches, self.live_count)
                };
                let now = Instant::now();
                if now.duration_since(self.last_repaint) >= Duration::from_millis(100) {
                    self.last_repaint = now;
                    ctx.request_repaint();
                }
            }
        }

        // Accept folder drag-and-drop onto the window
        ctx.input(|i| {
            if let Some(dropped) = i.raw.dropped_files.first() {
                if let Some(ref p) = dropped.path {
                    let s = p.to_string_lossy().replace('\\', "/");
                    self.search_path = s;
                    self.scope = Scope::Folder;
                    self.path_error = None;
                }
            }
        });

        // Esc: 搜索中取消；否则（停靠模式）隐藏窗口
        let esc_pressed = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if self.searching && esc_pressed {
            self.cancel_search();
            self.status = "已取消".to_string();
        } else if esc_pressed && self.prefs.dock_bottom && !self.show_settings {
            self.hide_window(ctx);
        }

        // —— 失焦自动隐藏：像系统搜索面板一样，点到别处即收起 ——
        if self.prefs.dock_bottom && !self.show_settings {
            let hwnd = self.bridge.hwnd.load(Ordering::Relaxed);
            let visible = hwnd != 0 && integration::win_is_visible(hwnd);
            let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
            if visible && !focused {
                let since = *self.unfocused_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(300) {
                    self.unfocused_since = None;
                    self.cancel_search();
                    self.hide_window(ctx);
                }
            } else {
                self.unfocused_since = None;
            }
        }

        // —— 实时搜索（文件名模式，150ms 防抖，输入即出结果）——
        // 整机范围不实时搜索：全盘按 150ms 节奏反复重扫只会空跑，改为回车触发
        let live_enabled = self.mode == SearchMode::Filename
            && !matches!(self.scope, Scope::Machine);
        if self.pattern != self.live_prev {
            self.live_prev = self.pattern.clone();
            self.live_edit = Instant::now();
            if live_enabled {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        } else if live_enabled
            && !self.pattern.is_empty()
            && self.pattern != self.live_typed
            && !self.searching
            && self.live_edit.elapsed() >= Duration::from_millis(150)
        {
            self.live_typed = self.pattern.clone();
            self.start_search(false);
        }

        // —— 类型/后缀改动后延迟重搜（筛选在搜索阶段生效，只改显示不够）——
        if let Some(t) = self.research_at {
            if t.elapsed() < Duration::from_millis(300) {
                ctx.request_repaint_after(Duration::from_millis(300));
            } else if self.searching {
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.research_at = None;
                if !self.pattern.is_empty() && self.searched_pattern == self.pattern {
                    self.start_search(false);
                }
            }
        }

        // —— 结果可见性：路径过滤 + 后缀筛选 ——
        let filter_lc = self.filter.to_lowercase();
        let exts = self.ext_filter_active();
        let vis_flags: Vec<bool> = self.results.iter()
            .map(|r| self.result_visible(r, &exts, &filter_lc))
            .collect();
        let visible: Vec<usize> = (0..self.results.len()).filter(|&i| vis_flags[i]).collect();

        // —— 键盘导航：↑/↓ 选中，Enter 打开（无结果时 Enter 触发搜索）——
        let (down, up, enter) = ctx.input(|i| (
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::Enter),
        ));
        if !visible.is_empty() && (down || up) {
            let cur = visible.iter().position(|&i| {
                Some(self.results[i].path.as_str()) == self.selected_path.as_deref()
            });
            let next = match (cur, down) {
                (Some(p), true) => (p + 1).min(visible.len() - 1),
                (Some(p), false) => p.saturating_sub(1),
                (None, true) => 0,
                (None, false) => visible.len() - 1,
            };
            self.selected_path = Some(self.results[visible[next]].path.clone());
        }
        if enter && !self.searching {
            if self.results.is_empty() || visible.is_empty() {
                if !self.pattern.is_empty() {
                    self.start_search(true);
                }
            } else {
                let target = self
                    .selected_path
                    .clone()
                    .or_else(|| self.results.get(visible[0]).map(|r| r.path.clone()));
                if let Some(win) = target
                    .as_deref()
                    .and_then(|p| self.results.iter().find(|r| r.path == p))
                    .map(|r| r.win_path.clone())
                {
                    let _ = open::that(&win);
                    if self.prefs.dock_bottom {
                        self.hide_window(ctx);
                    }
                }
            }
        }

        // Toolbar
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("搜索:");
                let pat_id = ui.make_persistent_id("pat_popup");
                let hint = match (self.mode, &self.scope) {
                    (SearchMode::Filename, Scope::Machine) => "文件名…（整机：按回车）",
                    (SearchMode::Filename, _) => "文件名…",
                    (SearchMode::Text, _) => "关键词…",
                };
                let pat_resp = ui.add(
                    TextEdit::singleline(&mut self.pattern)
                        .hint_text(hint)
                        .desired_width(230.0),
                );

                // Live regex validation — only re-check when pattern/flags change
                if self.pattern != self.last_pat
                    || self.ignore_case != self.last_ic
                    || self.fixed_string != self.last_fs
                {
                    self.last_pat = self.pattern.clone();
                    self.last_ic = self.ignore_case;
                    self.last_fs = self.fixed_string;
                    let pat_for_check = if self.fixed_string {
                        regex::escape(&self.pattern)
                    } else {
                        self.pattern.clone()
                    };
                    self.regex_error = if self.pattern.is_empty() {
                        None
                    } else {
                        RegexBuilder::new(&pat_for_check)
                            .case_insensitive(self.ignore_case)
                            .build()
                            .err()
                            .map(|e| format!("{e}"))
                    };
                }

                // Auto-focus on first frame
                if self.needs_focus {
                    pat_resp.request_focus();
                    self.needs_focus = false;
                }

                // 只在用户主动点进输入框时弹历史；获得焦点（含呼出面板时的自动聚焦）不弹，
                // 否则下拉会盖住下面的「范围 / 类型 / 后缀」那一行
                if pat_resp.clicked() && !self.prefs.pattern_history.is_empty() {
                    ui.memory_mut(|m| m.open_popup(pat_id));
                }
                egui::popup_below_widget(ui, pat_id, &pat_resp, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                    ui.set_min_width(240.0);
                    for h in self.prefs.pattern_history.clone() {
                        if ui.selectable_label(false, &h).clicked() {
                            self.pattern = h;
                            ui.memory_mut(|m| m.close_popup());
                        }
                    }
                });

                if self.searching {
                    if ui.button("⏹ 取消").on_hover_text("也可按 Esc").clicked() {
                        self.cancel_search();
                        self.status = "已取消".to_string();
                    }
                } else if ui.button("🔍 搜索").clicked() {
                    self.start_search(true);
                }

                ui.separator();
                ui.checkbox(&mut self.ignore_case, "Aa").on_hover_text("忽略大小写");
                ui.checkbox(&mut self.fixed_string, "F").on_hover_text("纯文本（不使用正则）");

                ui.separator();
                // Mode toggle — switching clears results
                let prev_mode = self.mode;
                ui.selectable_value(&mut self.mode, SearchMode::Filename, "🗂 文件名")
                    .on_hover_text("按文件/文件夹名搜索");
                ui.selectable_value(&mut self.mode, SearchMode::Text, "📄 文本")
                    .on_hover_text("搜索文件内容");
                if self.mode != prev_mode {
                    self.results.clear();
                    self.cancel_search();
                    self.status = "就绪".to_string();
                }
            });

            // 范围（整机 / 指定磁盘 / 指定目录）+ 类型 + 后缀
            ui.horizontal_wrapped(|ui| {
                ui.label("范围:");
                let mut new_scope: Option<Scope> = None;
                if ui
                    .selectable_label(matches!(self.scope, Scope::Machine), "整机")
                    .on_hover_text("搜索所有本地磁盘")
                    .clicked()
                {
                    new_scope = Some(Scope::Machine);
                }
                for d in self.drives.clone() {
                    let selected = matches!(&self.scope, Scope::Drive(x) if x.as_str() == d.as_str());
                    if ui
                        .selectable_label(selected, d.as_str())
                        .on_hover_text(format!("只搜索 {d} 盘"))
                        .clicked()
                    {
                        new_scope = Some(Scope::Drive(d));
                    }
                }
                if ui
                    .selectable_label(matches!(self.scope, Scope::Folder), "目录")
                    .on_hover_text("只搜索指定文件夹")
                    .clicked()
                {
                    new_scope = Some(Scope::Folder);
                }
                if let Some(s) = new_scope {
                    if s != self.scope {
                        self.scope = s;
                        self.prefs.scope = self.scope.clone();
                        self.save_prefs();
                        self.results.clear();
                        self.cancel_search();
                        self.research_at = None;
                        self.status = format!("范围：{}", self.scope.label());
                    }
                }

                if self.scope == Scope::Folder {
                    let path_id = ui.make_persistent_id("path_popup");
                    let path_resp = ui.add(
                        TextEdit::singleline(&mut self.search_path)
                            .desired_width(200.0)
                            .text_color(if self.path_error.is_some() {
                                Color32::from_rgb(255, 100, 100)
                            } else {
                                ui.visuals().text_color()
                            }),
                    );
                    if path_resp.gained_focus() && !self.prefs.path_history.is_empty() {
                        ui.memory_mut(|m| m.open_popup(path_id));
                    }
                    egui::popup_below_widget(ui, path_id, &path_resp, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                        ui.set_min_width(240.0);
                        for h in self.prefs.path_history.clone() {
                            if ui.selectable_label(false, &h).clicked() {
                                self.search_path = h;
                                ui.memory_mut(|m| m.close_popup());
                            }
                        }
                    });
                    if ui.button("📁").on_hover_text("选择文件夹").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.search_path = p.to_string_lossy().replace('\\', "/");
                            self.path_error = None;
                        }
                    }
                }

                ui.separator();
                ui.label("类型:");
                let mut kind = self.file_kind;
                egui::ComboBox::from_id_salt("file_kind")
                    .selected_text(kind.label())
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        for k in FileKind::ALL {
                            ui.selectable_value(&mut kind, k, k.label());
                        }
                    });
                if kind != self.file_kind {
                    self.file_kind = kind;
                    self.prefs.file_kind = kind;
                    self.save_prefs();
                    self.selected_path = None;
                    self.research_at = Some(Instant::now());
                }

                ui.label("后缀:");
                let e = ui
                    .add(
                        TextEdit::singleline(&mut self.ext_filter)
                            .hint_text("如 rs,toml")
                            .desired_width(110.0),
                    )
                    .on_hover_text("逗号分隔多个后缀；填写后优先于「类型」");
                if e.changed() {
                    self.selected_path = None;
                    self.prefs.ext_filter = self.ext_filter.clone();
                    self.save_prefs();
                    self.research_at = Some(Instant::now());
                }
                if !self.ext_filter.is_empty() && ui.small_button("✕").clicked() {
                    self.ext_filter.clear();
                    self.prefs.ext_filter.clear();
                    self.save_prefs();
                    self.research_at = Some(Instant::now());
                }
            });

            if let Some(ref err) = self.path_error {
                ui.label(RichText::new(err).color(Color32::from_rgb(255, 100, 100)).small());
            }
            if let Some(ref err) = self.regex_error {
                ui.label(RichText::new(format!("正则错误: {err}")).color(Color32::from_rgb(255, 140, 0)).small());
            }
            ui.add_space(4.0);
        });

        // Status bar
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.searching { ui.spinner(); }
                ui.label(RichText::new(&self.status).color(Color32::GRAY).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("▾").on_hover_text("收起（Esc / 热键 / 点其他地方）").clicked() {
                        self.hide_window(ctx);
                    }
                    if ui.small_button("⚙").on_hover_text("系统集成设置（热键 / 开机自启 / 隐藏任务栏搜索框）").clicked() {
                        self.show_settings = !self.show_settings;
                        // 托盘菜单可能改过注册表，打开设置时以注册表为准
                        self.autostart = integration::autostart_enabled();
                    }
                    if let Some(ref hk) = self.hotkey {
                        ui.label(RichText::new(format!("热键: {}", hk.label)).color(Color32::DARK_GRAY).small());
                    }
                });
            });
        });

        // Results
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.results.is_empty() && !self.searching {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("输入关键词后按回车或点击搜索").color(Color32::GRAY));
                });
                return;
            }

            // Toolbar above results
            if !self.results.is_empty() {
                ui.horizontal(|ui| {
                    if self.mode == SearchMode::Text {
                        if ui.small_button("▼ 全部展开").clicked() { self.collapsed.clear(); }
                        if ui.small_button("▶ 全部折叠").clicked() {
                            for r in &self.results { self.collapsed.insert(r.path.clone()); }
                        }
                    }
                    if ui.small_button("📋 复制全部路径").clicked() {
                        let all = self.results.iter().map(|r| r.path.as_str()).collect::<Vec<_>>().join("\n");
                        ctx.copy_text(all);
                    }
                    if self.result_capped {
                        ui.label(RichText::new(format!("⚠ 结果已截断至 {} 条", MAX_RESULTS)).color(Color32::YELLOW).small());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("显示 {}/{}", visible.len(), self.results.len())).color(Color32::GRAY).small());
                    });
                });
                // 过滤 / 排序（类型与后缀在顶部工具栏）
                ui.horizontal(|ui| {
                    ui.label(RichText::new("过滤:").small());
                    let f = ui.add(TextEdit::singleline(&mut self.filter)
                        .hint_text("路径包含…")
                        .desired_width(120.0));
                    if f.changed() { self.selected_path = None; }
                    if !self.filter.is_empty() && ui.small_button("✕").clicked() {
                        self.filter.clear();
                    }
                    ui.separator();
                    ui.label(RichText::new("排序:").small());
                    for k in SortKey::ALL {
                        if ui.selectable_label(self.sort == k, k.label())
                            .on_hover_text(if k == SortKey::Modified { "最新修改在前" } else if k == SortKey::Size { "最大在前" } else { "按路径" })
                            .clicked()
                        {
                            self.sort = k;
                            self.prefs.sort_key = k;
                            self.sort_results();
                            self.save_prefs();
                        }
                    }
                });
                ui.separator();
            }

            let mut toggle_collapse: Option<String> = None;
            let mut toggle_expand: Option<String> = None;

            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for (idx, result) in self.results.iter().enumerate() {
                    if !vis_flags[idx] {
                        continue;
                    }
                    let is_selected = self.selected_path.as_deref() == Some(result.path.as_str());
                    let is_collapsed = self.collapsed.contains(&result.path);
                    let is_expanded = self.expanded.contains(&result.path);
                    let shown = if self.mode == SearchMode::Text && !is_collapsed {
                        if is_expanded { result.matches.len() } else { result.matches.len().min(COLLAPSE_THRESHOLD) }
                    } else { 0 };
                    let has_more = self.mode == SearchMode::Text
                        && !is_collapsed
                        && result.matches.len() > COLLAPSE_THRESHOLD
                        && !is_expanded;

                    match render_result(ui, result, self.mode, is_collapsed, shown, has_more, ctx, is_selected) {
                        RowAction::ToggleCollapse(p) => toggle_collapse = Some(p),
                        RowAction::ToggleExpand(p)   => toggle_expand = Some(p),
                        RowAction::None => {}
                    }
                }
            });

            if let Some(p) = toggle_collapse {
                if self.collapsed.contains(&p) { self.collapsed.remove(&p); } else { self.collapsed.insert(p); }
            }
            if let Some(p) = toggle_expand {
                self.expanded.insert(p);
            }
        });

        // 系统集成设置窗口
        self.show_settings_window(ctx);

        // 窗口隐藏后 winit 停止派发重绘，帧循环自然停摆；
        // 热键/托盘通过 Win32 直接显示窗口，可见后帧循环自动恢复。
    }
}

impl App {
    fn show_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let hotkey_label = self.hotkey_label();
        // Window::open 持有 bool 的可变借用，需用局部变量中转，避免与内部闭包冲突
        let mut settings_open = self.show_settings;
        egui::Window::new("⚙ 系统集成")
            .open(&mut settings_open)
            .show(ctx, |ui| {
                ui.label(RichText::new(
                    "把系统底栏（任务栏）搜索替换为 Rust Seek：热键随时呼出，窗口停靠在任务栏上方。")
                    .weak());
                ui.add_space(6.0);

                // 全局热键
                ui.horizontal(|ui| {
                    ui.label("全局热键:");
                    egui::ComboBox::from_id_salt("hotkey_preset")
                        .selected_text(&hotkey_label)
                        .show_ui(ui, |ui| {
                            for (i, (label, _)) in integration::hotkey_presets().iter().enumerate() {
                                ui.selectable_value(&mut self.prefs.hotkey_preset, i, *label);
                            }
                        });
                });
                if let Some(ref err) = self.hotkey_error {
                    ui.label(RichText::new(err).color(Color32::from_rgb(255, 140, 0)).small());
                }
                if self.prefs.hotkey_preset != self.active_hotkey_preset {
                    match integration::setup_hotkey(self.prefs.hotkey_preset, self.bridge.clone()) {
                        Ok(h) => {
                            if let Some(tray) = self.tray.as_ref() {
                                tray.show_item.set_text(format!("显示 / 隐藏 ({})", h.label));
                            }
                            self.hotkey = Some(h);
                            self.hotkey_error = None;
                        }
                        Err(e) => self.hotkey_error = Some(e),
                    }
                    self.active_hotkey_preset = self.prefs.hotkey_preset;
                    self.save_prefs();
                }
                // Win+S 接管状态
                if let Some(hk) = self.hotkey.as_ref() {
                    if hk.win_s_ok {
                        ui.label(RichText::new("✅ Win+S 已接管：按 Win+S（系统搜索热键）直接呼出 Rust Seek")
                            .color(Color32::from_rgb(100, 200, 100)).small());
                    } else {
                        ui.label(RichText::new(
                            "Win+S 暂被系统搜索占用；开机自启后 Rust Seek 抢先注册可能接管成功。当前仍可用上方热键呼出。")
                            .weak().small());
                    }
                }

                ui.separator();

                // 接管 Windows 自带搜索（对应 设置 → 隐私和安全性 → 搜索）
                ui.label(RichText::new("Windows 自带搜索（设置 → 隐私和安全性 → 搜索）").strong().small());
                let mut bing = integration::bing_disabled();
                if ui.checkbox(&mut bing, "禁用开始/搜索框的 Bing 网络结果").changed() {
                    match integration::set_bing_disabled(bing) {
                        Ok(()) => self.status = if bing { "已禁用 Bing 网络结果".into() } else { "已恢复 Bing 网络结果".into() },
                        Err(e) => self.status = format!("设置失败: {e}"),
                    }
                }
                let mut hl = integration::highlights_enabled();
                if ui.checkbox(&mut hl, "关闭「搜索突出显示」").changed() {
                    match integration::set_highlights_enabled(hl) {
                        Ok(()) => self.status = if hl { "已开启搜索突出显示".into() } else { "已关闭搜索突出显示".into() },
                        Err(e) => self.status = format!("设置失败: {e}"),
                    }
                }
                ui.label(RichText::new("改动即刻写入系统；原生搜索框行为可能在重启资源管理器后完全刷新。")
                    .weak().small());

                ui.separator();

                // 开机自启
                if ui.checkbox(&mut self.autostart, "开机自启（后台隐藏运行，不占任务栏）").changed() {
                    match integration::set_autostart(self.autostart) {
                        Ok(()) => self.status = if self.autostart { "已开启开机自启".into() } else { "已关闭开机自启".into() },
                        Err(e) => {
                            self.autostart = !self.autostart;
                            self.status = format!("设置开机自启失败: {e}");
                        }
                    }
                    self.sync_tray_checks();
                }

                ui.separator();

                // 隐藏任务栏搜索框
                let mut hidden_ui = integration::taskbar_search_hidden();
                if ui.checkbox(&mut hidden_ui, "隐藏 Windows 任务栏搜索框（用 Rust Seek 替代）").changed() {
                    if hidden_ui {
                        self.prefs.searchbox_prev = integration::hide_taskbar_search();
                        self.status = "已隐藏任务栏搜索框；重启资源管理器后生效".into();
                    } else {
                        let prev = self.prefs.searchbox_prev.take().flatten();
                        integration::restore_taskbar_search(prev);
                        self.status = "已恢复任务栏搜索框；重启资源管理器后生效".into();
                    }
                    self.save_prefs();
                    self.sync_tray_checks();
                }
                ui.label(RichText::new(
                    "取消勾选或通过托盘菜单可随时恢复原状（会记录并还原你之前的显示方式）。")
                    .weak().small());
                ui.horizontal(|ui| {
                    if ui.small_button("重启资源管理器（立即生效）").clicked() {
                        if self.explorer_confirm {
                            integration::restart_explorer();
                            self.explorer_confirm = false;
                            self.status = "正在重启资源管理器…".into();
                        } else {
                            self.explorer_confirm = true;
                        }
                    }
                    if self.explorer_confirm {
                        ui.label(RichText::new("再次点击确认：会短暂黑屏并关闭资源管理器窗口")
                            .color(Color32::YELLOW).small());
                        if ui.small_button("取消").clicked() {
                            self.explorer_confirm = false;
                        }
                    }
                });

                ui.separator();

                if ui.checkbox(&mut self.prefs.prune_system,
                    "整机搜索时跳过系统目录（Windows / Program Files / ProgramData / 回收站…）").changed()
                {
                    self.save_prefs();
                    self.status = if self.prefs.prune_system {
                        "已开启：整机搜索将跳过系统目录".into()
                    } else {
                        "已关闭：整机搜索会遍历整个系统盘（明显更慢）".into()
                    };
                }
                ui.label(RichText::new(
                    "只跳过盘符根下的系统目录，项目里的同名文件夹不受影响；仅对「整机」范围生效。")
                    .weak().small());

                ui.separator();

                if ui.checkbox(&mut self.prefs.dock_bottom,
                    "系统式面板（停靠屏幕底部、无边框、不进任务栏，失焦自动收起）").changed()
                {
                    self.save_prefs();
                    let dock = self.prefs.dock_bottom;
                    self.bridge.panel.store(dock, Ordering::Relaxed);
                    let hwnd = self.bridge.hwnd.load(Ordering::Relaxed);
                    if hwnd != 0 {
                        integration::ensure_panel_style(hwnd, dock);
                    }
                }
                ui.add_space(4.0);
                ui.label(RichText::new(
                    "关闭窗口 = 隐藏到托盘；完全退出请用托盘图标右键 → 退出。")
                    .weak().small());
            });
        self.show_settings = settings_open;
    }
}

enum RowAction { None, ToggleCollapse(String), ToggleExpand(String) }

#[allow(clippy::too_many_arguments)]
fn render_result(
    ui: &mut Ui,
    result: &SearchResult,
    mode: SearchMode,
    is_collapsed: bool,
    shown_matches: usize,
    has_more: bool,
    ctx: &egui::Context,
    is_selected: bool,
) -> RowAction {
    let mut action = RowAction::None;

    // 键盘选中行：先铺底色，控件随后绘制其上
    if is_selected {
        let rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
        );
        ui.painter().rect_filled(rect.expand2(egui::vec2(4.0, 2.0)), 3.0, Color32::from_rgb(45, 85, 140));
    }

    // File header row
    ui.horizontal(|ui| {
        if mode == SearchMode::Text {
            let arrow = if is_collapsed { "▶" } else { "▼" };
            if ui.small_button(arrow).clicked() {
                action = RowAction::ToggleCollapse(result.path.clone());
            }
        }

        ui.label(result.icon);

        let link = if mode == SearchMode::Filename {
            let file_name = result.path.rsplit('/').next().unwrap_or(&result.path);
            let parent = result.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            let l = ui.link(RichText::new(file_name).color(Color32::from_rgb(100, 180, 255)).strong());
            if !parent.is_empty() {
                ui.label(RichText::new(parent).color(Color32::DARK_GRAY).small());
            }
            if result.file_size > 0 {
                ui.label(RichText::new(fmt_size(result.file_size)).color(Color32::DARK_GRAY).small());
            }
            if let Some(m) = result.modified {
                ui.label(RichText::new(fmt_time(m)).color(Color32::DARK_GRAY).small());
            }
            l
        } else {
            let l = ui.link(RichText::new(&result.path).color(Color32::from_rgb(100, 180, 255)).strong());
            if !is_collapsed {
                ui.label(RichText::new(format!("({} 处匹配)", result.matches.len())).color(Color32::GRAY).small());
            }
            l
        };

        if link.clicked() { let _ = open::that(&result.win_path); }

        // Filename mode: inline highlight
        if mode == SearchMode::Filename {
            if let Some(m) = result.matches.first() {
                ui.add_space(4.0);
                render_highlighted(ui, &m.line, &m.ranges, false);
            }
        }

        link.context_menu(|ui| {
            if ui.button("📂  在文件夹中显示").clicked() {
                reveal_in_explorer(&result.win_path); ui.close_menu();
            }
            if ui.button("▶  打开").clicked() {
                let _ = open::that(&result.win_path); ui.close_menu();
            }
            ui.separator();
            if ui.button("📋  复制路径").clicked() {
                ctx.copy_text(result.path.clone()); ui.close_menu();
            }
            if ui.button("📋  复制文件夹路径").clicked() {
                let p = result.path.rsplit_once('/').map(|(p,_)| p).unwrap_or("").to_string();
                ctx.copy_text(p); ui.close_menu();
            }
        });
    });

    // Text mode: match lines with context
    if mode == SearchMode::Text && !is_collapsed {
        let mut last_shown_line: Option<usize> = None;
        for m in result.matches.iter().take(shown_matches) {
            // Context before — skip if already shown as previous match's after
            let before_line_num = m.line_num.saturating_sub(1);
            if let Some(ref before) = m.context_before {
                if last_shown_line != Some(before_line_num) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:>4}  ", before_line_num)).color(Color32::DARK_GRAY).monospace());
                        ui.label(RichText::new(before.as_str()).color(Color32::DARK_GRAY).monospace());
                    });
                }
            }
            // Match line
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(RichText::new(format!("{:>4}: ", m.line_num)).color(Color32::from_rgb(100, 200, 100)).monospace());
                render_highlighted(ui, &m.line, &m.ranges, true);
            });
            last_shown_line = Some(m.line_num);
            // Context after
            if let Some(ref after) = m.context_after {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:>4}  ", m.line_num + 1)).color(Color32::DARK_GRAY).monospace());
                    ui.label(RichText::new(after.as_str()).color(Color32::DARK_GRAY).monospace());
                });
                last_shown_line = Some(m.line_num + 1);
            }
            ui.add_space(2.0);
        }

        if has_more {
            let remaining = result.matches.len() - shown_matches;
            if ui.small_button(
                RichText::new(format!("  ↓ 显示另外 {} 处匹配", remaining)).color(Color32::GRAY).small()
            ).clicked() {
                action = RowAction::ToggleExpand(result.path.clone());
            }
        }
        ui.add(egui::Separator::default().spacing(6.0));
    }

    action
}

fn render_highlighted(ui: &mut egui::Ui, line: &str, ranges: &[(usize, usize)], monospace: bool) {
    use egui::text::{LayoutJob, TextFormat};
    use egui::FontId;

    let mut job = LayoutJob::default();
    let font = if monospace {
        FontId::monospace(14.0)
    } else {
        FontId::proportional(12.0)
    };
    let normal_color = if monospace { Color32::LIGHT_GRAY } else { Color32::GRAY };

    let mut cursor = 0;
    for &(start, end) in ranges {
        let start = start.min(line.len());
        let end = end.min(line.len());
        if start > cursor {
            job.append(&line[cursor..start], 0.0, TextFormat {
                font_id: font.clone(),
                color: normal_color,
                ..Default::default()
            });
        }
        if start < end {
            job.append(&line[start..end], 0.0, TextFormat {
                font_id: font.clone(),
                color: Color32::BLACK,
                background: Color32::from_rgb(255, 200, 0),
                ..Default::default()
            });
        }
        cursor = end;
    }
    if cursor < line.len() {
        job.append(&line[cursor..], 0.0, TextFormat {
            font_id: font.clone(),
            color: normal_color,
            ..Default::default()
        });
    }
    ui.label(job);
}

fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 { format!("{} B", bytes) }
    else if bytes < 1024 * 1024 { format!("{:.1} KB", bytes as f64 / 1024.0) }
    else { format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0) }
}

fn fmt_time(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn reveal_in_explorer(path: &str) {
    // /select, and the path must be a single argument — splitting them breaks explorer
    let arg = format!("/select,{}", path);
    let _ = std::process::Command::new("explorer").arg(arg).spawn();
}

fn push_history(history: &mut Vec<String>, value: String) {
    history.retain(|h| h != &value);
    history.insert(0, value);
    history.truncate(MAX_HISTORY);
}
