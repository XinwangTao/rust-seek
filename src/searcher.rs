use anyhow::Result;
use encoding_rs::GBK;
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
}

pub fn search_filename(path: &Path, pattern: &Regex) -> Option<SearchResult> {
    let name = path.file_name()?.to_string_lossy();
    let ranges: Vec<(usize, usize)> = pattern
        .find_iter(name.as_ref())
        .map(|m| (m.start(), m.end()))
        .collect();
    if ranges.is_empty() { return None; }
    let (display_path, win_path) = make_paths(path);
    let icon = file_icon(&display_path);
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Some(SearchResult {
        icon, win_path, path: display_path, file_size,
        matches: vec![Match {
            line_num: 0, line: name.into_owned(), ranges,
            context_before: None, context_after: None,
        }],
    })
}

const MAX_LINE_LEN: usize = 512;
const BINARY_CHECK_LEN: usize = 1024;

pub fn search_file(path: &Path, pattern: &Regex, max_filesize: u64) -> Result<Option<SearchResult>> {
    let metadata = std::fs::metadata(path)?;
    let file_len = metadata.len();
    if file_len == 0 || file_len > max_filesize { return Ok(None); }

    let matches = if file_len < 4096 {
        let bytes = std::fs::read(path)?;
        if is_binary(&bytes, bytes.len().min(BINARY_CHECK_LEN)) { return Ok(None); }
        let s = decode(&bytes);
        collect_matches(&s, pattern)
    } else {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if is_binary(&mmap, BINARY_CHECK_LEN) { return Ok(None); }
        let s = decode(&mmap);
        collect_matches(&s, pattern)
    };

    if matches.is_empty() { return Ok(None); }
    let (display_path, win_path) = make_paths(path);
    let icon = file_icon(&display_path);
    Ok(Some(SearchResult { path: display_path, win_path, icon, matches, file_size: file_len }))
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
    data[..data.len().min(check_len)].contains(&0)
}

/// Detect encoding from a small sample, then decode the full buffer once
fn decode(bytes: &[u8]) -> String {
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
        if line.len() > MAX_LINE_LEN * 4 {
            prev = None;
            continue;
        }
        if pattern.find(line).is_none() {
            // Reuse allocation via Rc — context_before of next match shares this
            prev = Some(Arc::new(truncate(line)));
            continue;
        }
        let ranges: Vec<(usize, usize)> = pattern.find_iter(line)
            .map(|m| (m.start(), m.end()))
            .collect();
        let display = truncate(line);
        let context_before = prev.clone();
        let context_after = lines_iter.peek()
            .map(|(_, next)| Arc::new(truncate(next)));
        matches.push(Match {
            line_num: i + 1,
            line: display,
            ranges,
            context_before,
            context_after,
        });
        prev = Some(Arc::new(line.to_string()));
    }
    matches
}

fn truncate(line: &str) -> String {
    if line.len() > MAX_LINE_LEN {
        let end = line.char_indices().nth(MAX_LINE_LEN).map(|(i, _)| i).unwrap_or(line.len());
        line[..end].to_string() + "…"
    } else {
        line.to_string()
    }
}

pub fn file_icon(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "exe" | "msi"                          => "⚙",
        "bat" | "cmd" | "ps1" | "sh"           => "⚙",
        "rs" | "py" | "js" | "ts" | "go"
        | "c" | "cpp" | "java" | "cs" | "rb"  => "📝",
        "toml" | "json" | "yaml" | "yml"
        | "xml" | "ini" | "cfg" | "env"        => "🔧",
        "md" | "txt" | "log"                   => "📄",
        "png" | "jpg" | "jpeg" | "gif"
        | "svg" | "ico" | "bmp" | "webp"       => "🖼",
        "mp4" | "mkv" | "avi" | "mov"          => "🎬",
        "mp3" | "wav" | "flac" | "ogg"         => "🎵",
        "zip" | "rar" | "7z" | "tar" | "gz"   => "📦",
        "pdf"                                  => "📕",
        "doc" | "docx"                         => "📘",
        "xls" | "xlsx"                         => "📗",
        "ppt" | "pptx"                         => "📙",
        "lnk"                                  => "🔗",
        "db" | "sqlite" | "sql"                => "🗄",
        _                                      => "📄",
    }
}
