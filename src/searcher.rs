use anyhow::Result;
use encoding_rs::{GBK, UTF_16BE, UTF_16LE};
use memmap2::Mmap;
use regex::Regex;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub struct Match {
    pub line_num: usize,
    pub line: String,
    pub ranges: Vec<(usize, usize)>,
    pub context_before: Option<Arc<String>>,
    pub context_after: Option<Arc<String>>,
}

pub struct SearchResult {
    pub path: String,
    pub win_path: String,
    pub icon: &'static str,
    pub matches: Vec<Match>,
    pub file_size: u64,
    pub modified: Option<std::time::SystemTime>,
}

#[allow(dead_code)]
pub fn search_filename(path: &Path, pattern: &Regex) -> Option<SearchResult> {
    search_filename_meta(path, pattern, || {
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    })
}

/// 大小/修改时间用闭包延迟获取：**只有文件名命中时才去取**。
///
/// 遍历时给每个条目取元数据（walkdir 的 `metadata()` 走 `symlink_metadata`）在整机
/// 250 万条目上就是 250 万次系统调用，而命中通常只有几十个。走 [`crate::fastwalk`]
/// 时枚举本身已经带回大小和时间，闭包直接给出、一次系统调用都不用。
pub fn search_filename_meta<F>(path: &Path, pattern: &Regex, meta: F) -> Option<SearchResult>
where
    F: FnOnce() -> Option<(u64, Option<std::time::SystemTime>)>,
{
    search_filename_meta_path(path, pattern, false, meta)
}

/// 同 [`search_filename_meta`]，`match_path=true` 时拿完整路径去匹配。
///
/// 用户经常直接粘贴一个完整路径（`D:/a/b/c.html`）而不是文件名；只比
/// `file_name` 的话这种输入永远 0 命中，看着就是「搜不到」。
pub fn search_filename_meta_path<F>(
    path: &Path,
    pattern: &Regex,
    match_path: bool,
    meta: F,
) -> Option<SearchResult>
where
    F: FnOnce() -> Option<(u64, Option<std::time::SystemTime>)>,
{
    let (display_path, win_path) = make_paths(path);
    let name = path.file_name()?.to_string_lossy();
    let target = if match_path {
        display_path.as_str()
    } else {
        name.as_ref()
    };
    let mut ranges = Vec::new();
    for m in pattern.find_iter(target) {
        ranges.push((m.start(), m.end()));
    }
    // 粘贴的是反斜杠路径：正斜杠形式匹配不上时再试原生形式（两者长度一致，偏移可复用）
    if ranges.is_empty() && match_path {
        for m in pattern.find_iter(win_path.as_str()) {
            ranges.push((m.start(), m.end()));
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let icon = file_icon(&display_path);
    let (file_size, modified) = meta().unwrap_or((0, None));
    let line = if match_path {
        display_path.clone()
    } else {
        name.into_owned()
    };
    Some(SearchResult {
        icon,
        win_path,
        path: display_path,
        file_size,
        modified,
        matches: vec![Match {
            line_num: 0,
            line,
            ranges,
            context_before: None,
            context_after: None,
        }],
    })
}

/// 搜索模式清洗：粘贴来的文件名常带 BOM / 零宽字符 / 首尾空白，
/// 按字面匹配就永远 0 命中（看着就是「搜不到」）。
/// 返回 `(清洗后的模式, 去掉的字符数)`。
pub fn clean_pattern(s: &str) -> (String, usize) {
    const INVISIBLE: &[char] = &[
        '\u{feff}', '\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{00a0}',
    ];
    let mut out = String::with_capacity(s.len());
    let mut removed = 0usize;
    for ch in s.chars() {
        if INVISIBLE.contains(&ch) {
            removed += 1;
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim().to_string();
    removed += out.chars().count() - trimmed.chars().count();
    (trimmed, removed)
}

/// 模式里带路径分隔符 ⇒ 用户粘的是路径片段，应按完整路径匹配。
/// 粘的是路径片段时按完整路径匹配。
///
/// 关键：`\` 在正则里是转义符（`\.` `\d` `\w`），不能一见到反斜杠就当路径 ——
/// 否则 `^Cargo\.toml$` 这类锚定文件名的正则会变成对绝对路径匹配，静默 0 命中。
/// 正则模式下只认明确形态（`X:\`、UNC `\\server`）；非正则模式才把任意反斜杠当分隔符。
pub fn looks_like_path(s: &str, is_regex: bool) -> bool {
    if s.contains('/') {
        return true;
    }
    let b = s.as_bytes();
    let drive_abs = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    if drive_abs || s.starts_with("\\\\") {
        return true;
    }
    !is_regex && s.contains('\\')
}

const MAX_LINE_LEN: usize = 512;
const BINARY_CHECK_LEN: usize = 1024;

pub fn search_file(
    path: &Path,
    pattern: &Regex,
    max_filesize: u64,
) -> Result<Option<SearchResult>> {
    let metadata = std::fs::metadata(path)?;
    let file_len = metadata.len();
    if file_len == 0 || file_len > max_filesize {
        return Ok(None);
    }

    let matches = if file_len < 4096 {
        let bytes = std::fs::read(path)?;
        if is_binary(&bytes, bytes.len().min(BINARY_CHECK_LEN)) {
            return Ok(None);
        }
        let s = decode(&bytes);
        collect_matches(&s, pattern)
    } else {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if is_binary(&mmap, BINARY_CHECK_LEN) {
            return Ok(None);
        }
        let s = decode(&mmap);
        collect_matches(&s, pattern)
    };

    if matches.is_empty() {
        return Ok(None);
    }
    let (display_path, win_path) = make_paths(path);
    let icon = file_icon(&display_path);
    let modified = metadata.modified().ok();
    Ok(Some(SearchResult {
        path: display_path,
        win_path,
        icon,
        matches,
        file_size: file_len,
        modified,
    }))
}

/// Compute both display path (forward slash) and win_path (backslash) in one pass
fn make_paths(path: &Path) -> (String, String) {
    let raw = path.to_string_lossy();
    if raw.contains('\\') {
        (raw.replace('\\', "/"), raw.into_owned())
    } else {
        (raw.clone().into_owned(), raw.into_owned())
    }
}

fn is_binary(data: &[u8], check_len: usize) -> bool {
    // 带 BOM 的 UTF-16 文本（PowerShell .ps1、.reg、部分 .ini）高字节恒为 0x00，
    // 按「含 NUL 即二进制」判会整类漏掉，先认 BOM。
    if data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]) {
        return false;
    }
    data[..data.len().min(check_len)].contains(&0)
}

/// Detect encoding from a small sample, then decode the full buffer once
fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (cow, _, _) = UTF_16LE.decode(bytes);
        return cow.into_owned();
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, _) = UTF_16BE.decode(bytes);
        return cow.into_owned();
    }
    if std::str::from_utf8(bytes).is_ok() {
        // SAFETY: just validated above
        return unsafe { std::str::from_utf8_unchecked(bytes) }.to_owned();
    }
    let (cow, _, _) = GBK.decode(bytes);
    cow.into_owned()
}

fn collect_matches(content: &str, pattern: &Regex) -> Vec<Match> {
    let mut matches = Vec::new();
    let mut prev: Option<Arc<String>> = None;
    let mut lines_iter = content.lines().enumerate().peekable();

    while let Some((i, line)) = lines_iter.next() {
        if pattern.find(line).is_none() {
            // Reuse allocation via Rc — context_before of next match shares this
            prev = Some(Arc::new(truncate(line)));
            continue;
        }
        // 长行（压缩后的 js/css、单行 JSON、大日志）照样要匹配：以前整行丢弃，
        // 等于对这些文件静默返回 0 命中。命中区间设上限，避免单行刷出百万条。
        let ranges: Vec<(usize, usize)> = pattern
            .find_iter(line)
            .take(100)
            .map(|m| (m.start(), m.end()))
            .collect();
        let display = truncate(line);
        let context_before = prev.clone();
        let context_after = lines_iter.peek().map(|(_, next)| Arc::new(truncate(next)));
        matches.push(Match {
            line_num: i + 1,
            line: display,
            ranges,
            context_before,
            context_after,
        });
        prev = Some(Arc::new(truncate(line)));
    }
    matches
}

fn truncate(line: &str) -> String {
    if line.len() > MAX_LINE_LEN {
        let end = line
            .char_indices()
            .nth(MAX_LINE_LEN)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        line[..end].to_string() + "…"
    } else {
        line.to_string()
    }
}

// ---------- 目录剪枝 ----------

/// 整机搜索 / 建索引时整棵跳过的目录名（任意层级，不分大小写）。
/// 这些目录里的文件 Agent 基本不会要，却常常占掉整盘一半条目。
pub const SKIP_DIRS: &[&str] = &[
    "$recycle.bin",
    "system volume information",
    "winsxs",
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".cache",
    ".gradle",
    ".next",
    ".nuxt",
];

/// 整机搜索时整棵跳过的系统目录名（任意层级）：回收站、系统还原、WinSxS。
pub fn is_noise_dir_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "$recycle.bin" | "system volume information" | "winsxs" | "recovery"
    )
}

/// `~/Library` 下高频抖动、几乎不承载用户文件的目录（macOS）。
///
/// 这些目录（缓存/容器/日志/窗口状态）在系统运行时每秒都有写入；把它们收进索引，
/// FSEvents 会持续产生脏目录、索引刷新变成永动机。它们也不是文件搜索的目标，
/// 与 node_modules / WinSxS 同属「噪声」，默认整棵剪掉。
#[cfg(target_os = "macos")]
pub fn path_is_user_library_noise(path: &Path) -> bool {
    let Some(home) = home_dir_cached() else {
        return false;
    };
    let Ok(rest) = path.strip_prefix(home.join("Library")) else {
        return false;
    };
    matches!(
        rest.components()
            .next()
            .and_then(|c| c.as_os_str().to_str()),
        Some("Caches" | "Containers" | "Group Containers" | "Logs" | "Saved Application State")
    )
}

/// `dirs::home_dir()` 可能走 getpwuid；FSEvents 回调里每条事件都调一次太贵。
#[cfg(target_os = "macos")]
fn home_dir_cached() -> Option<&'static std::path::Path> {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    HOME.get_or_init(dirs::home_dir).as_deref()
}

#[cfg(not(target_os = "macos"))]
pub fn path_is_user_library_noise(_path: &Path) -> bool {
    false
}

/// 路径中任意一段是整机噪声目录：回收站/系统还原/WinSxS，以及 node_modules、
/// .git、target 等常见构建/依赖目录；macOS 另加 `~/Library` 的抖动目录。
///
/// 口径必须与索引建库（`index::is_pruned`）一致：否则同一台机器的文件名搜索，
/// 走索引和走遍历会返回不同结果集。
pub fn path_in_noise_dir(path: &Path) -> bool {
    path_is_user_library_noise(path)
        || path.components().any(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map_or(false, |n| {
                is_noise_dir_name(n) || SKIP_DIRS.iter().any(|d| n.eq_ignore_ascii_case(d))
            }),
            _ => false,
        })
}

/// 卷根下第一层的大头目录（Windows 的 `C:\Windows`、`C:\Program Files`；
/// macOS 的 `/System`、`/Library`、`/private` 等）。
///
/// 只认「卷根 + 目录名」这两段，绝不按任意层级匹配：项目里常有 `src\windows`、
/// `web\Program Files` 这类同名目录，整层剪掉会直接搜不到代码。
///
/// macOS 上 `local_drives()` 返回 `/`，整盘索引会把 `/System`（约占目录 70%、
/// 文件 61%）、`/Library`、`/usr`、`/private` 全部收进来：本机实测 141.8 万文件 /
/// 41.1 万目录 / 290MB 缓存。这些目录与 Windows 的系统目录同义，必须同样剪掉，
/// 否则每次全量巡/兜底遍历都在扫系统文件，弱机器上直接卡死。
pub fn path_is_system_root(path: &Path) -> bool {
    let mut comps = path.components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Prefix(_)), Some(std::path::Component::Normal(s))) => {
            s.to_str().map_or(false, is_system_root_name)
        }
        (Some(std::path::Component::RootDir), Some(std::path::Component::Normal(s))) => {
            s.to_str().map_or(false, is_unix_system_root_name)
        }
        _ => false,
    }
}

/// macOS 卷根下的系统目录（大小写敏感，按实际名字精确匹配）。
#[cfg(target_os = "macos")]
fn is_unix_system_root_name(name: &str) -> bool {
    matches!(
        name,
        "System" | "Library" | "private" | "usr" | "bin" | "sbin" | "dev" | "cores"
    )
}

/// 其他 Unix 卷根下的系统目录（保守列表：只含明确不该出现在搜索结果里的）。
#[cfg(all(unix, not(target_os = "macos")))]
fn is_unix_system_root_name(name: &str) -> bool {
    matches!(
        name,
        "proc" | "sys" | "dev" | "usr" | "bin" | "sbin" | "lib" | "lib64" | "boot"
    )
}

/// Windows 上没有 Unix 风格的卷根系统目录；保留函数让匹配分支保持穷举。
#[cfg(windows)]
fn is_unix_system_root_name(_name: &str) -> bool {
    false
}

fn is_system_root_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "windows"
            | "windows.old"
            | "program files"
            | "program files (x86)"
            | "programdata"
            | "perflogs"
            | "msocache"
            | "config.msi"
            | "recovery"
            | "$windows.~bt"
            | "$windows.~ws"
            | "$extend"
            | "$winreagent"
    )
}

pub fn file_icon(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "exe" | "msi" => "⚙",
        "bat" | "cmd" | "ps1" | "sh" => "⚙",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "java" | "cs" | "rb" => "📝",
        "toml" | "json" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "env" => "🔧",
        "md" | "txt" | "log" => "📄",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "bmp" | "webp" => "🖼",
        "mp4" | "mkv" | "avi" | "mov" => "🎬",
        "mp3" | "wav" | "flac" | "ogg" => "🎵",
        "zip" | "rar" | "7z" | "tar" | "gz" => "📦",
        "pdf" => "📕",
        "doc" | "docx" => "📘",
        "xls" | "xlsx" => "📗",
        "ppt" | "pptx" => "📙",
        "lnk" => "🔗",
        "db" | "sqlite" | "sql" => "🗄",
        _ => "📄",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_volume_root_system_dirs_are_pruned() {
        for p in [
            "/System",
            "/System/Volumes/Data",
            "/Library/Application Support",
            "/private/var/log",
            "/usr/local/bin",
            "/bin",
            "/sbin",
            "/dev",
            "/cores",
        ] {
            assert!(path_is_system_root(Path::new(p)), "{p} 应被剪掉");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_user_and_app_dirs_are_not_pruned() {
        for p in [
            "/",
            "/Users/Zhuanz2/Documents/System",
            "/Users/Zhuanz2/Library/Application Support",
            "/Applications/Xcode.app",
            "/opt/homebrew/bin",
            "System/Library", // 相对路径：不是卷根下第一层
        ] {
            assert!(!path_is_system_root(Path::new(p)), "{p} 不应被剪掉");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn noise_dirs_still_apply_at_any_depth() {
        assert!(path_in_noise_dir(Path::new(
            "/Users/a/project/node_modules/pkg/index.js"
        )));
        assert!(!path_in_noise_dir(Path::new(
            "/Users/a/project/src/main.rs"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_user_library_noise_is_pruned_but_user_data_is_not() {
        let home = dirs::home_dir().expect("home");
        for p in [
            "Caches",
            "Containers",
            "Group Containers",
            "Logs",
            "Saved Application State",
        ] {
            assert!(
                path_in_noise_dir(&home.join("Library").join(p).join("x")),
                "~/Library/{p} 应被剪掉"
            );
        }
        assert!(!path_in_noise_dir(
            &home.join("Library/Application Support/app/data.db")
        ));
        assert!(!path_in_noise_dir(&home.join("Downloads/Library/Caches/x")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_root_system_dirs_are_pruned() {
        assert!(path_is_system_root(Path::new("C:\\Windows\\System32")));
        assert!(path_is_system_root(Path::new("D:\\Program Files\\App")));
        assert!(!path_is_system_root(Path::new("C:\\Users\\a\\Windows")));
        assert!(!path_is_system_root(Path::new("C:\\")));
    }
}
