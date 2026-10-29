//! 最小 MCP（Model Context Protocol）服务端。
//!
//! 用 `rust-seek.exe --mcp` 启动，走 stdio + 换行分隔的 JSON-RPC 2.0，供 AI Agent
//! 直接调用搜索能力。和 GUI 共用同一个 `searcher`，所以是同一套匹配语义。
//!
//! 为 Agent 场景做的取舍：
//! - 达到 `max_results` 立即 `WalkState::Quit` 掐断整棵遍历，不等全盘走完
//! - 默认用 `WalkState::Skip` 整棵剪掉 node_modules/.git/target/WinSxS 之类目录
//! - 结果按路径排序后返回，便于模型稳定引用；响应自带 scanned/elapsed_ms 便于判断是否截断

use crate::integration;
use crate::searcher::{search_file, search_filename_meta};
use ignore::{WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PROTOCOL_VERSION: &str = "2025-06-18";

/// 默认整机搜索时整棵跳过的目录（小写比较）。Agent 场景下这些目录只会拖慢搜索。
const DEFAULT_SKIP: &[&str] = &[
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

/// 遍历回调的返回：继续 / 跳过整个目录 / 提前结束整次遍历
enum Step {
    Continue,
    Skip,
    Stop,
}

pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let msg = json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                if !write_msg(&mut out, msg)? {
                    break;
                }
                continue;
            }
        };

        // 通知（没有 id）不需要回应
        let Some(id) = req.get("id").cloned() else { continue };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();

        let result = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rust-seek", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => call_tool(req.get("params").unwrap_or(&Value::Null)),
            other => Err((-32601i64, format!("unknown method: {other}"))),
        };

        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": code, "message": message }
            }),
        };
        if !write_msg(&mut out, msg)? {
            break;
        }
    }
    Ok(())
}

/// 写一行 JSON-RPC 消息。返回 false 表示对端已关闭管道，调用方应正常退出。
fn write_msg(out: &mut impl Write, msg: Value) -> io::Result<bool> {
    match writeln!(out, "{msg}").and_then(|_| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
    }
}

// ---------- 工具定义 ----------

fn tool_defs() -> Value {
    json!([
        {
            "name": "search_files",
            "description": "Search the local machine for files by name/path. Returns absolute paths with size and mtime. \
Traversal stops as soon as max_results matches are collected, so whole-machine searches return fast. \
node_modules/.git/target/WinSxS and similar directories are pruned by default.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex over the file name (or a plain substring when regex=false)." },
                    "regex": { "type": "boolean", "default": true, "description": "Treat pattern as a regular expression. Set false for literal substring matching." },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Directories to search, e.g. [\"F:/\", \"C:/Users/me/proj\"]. Defaults to every local drive." },
                    "exts": { "type": "array", "items": { "type": "string" }, "description": "Only keep these extensions (without dot), e.g. [\"rs\",\"toml\"]." },
                    "max_results": { "type": "integer", "default": 100, "description": "Hard cap on returned files (1-2000). Traversal quits once reached." },
                    "include_hidden": { "type": "boolean", "default": false },
                    "skip_dirs": { "type": "array", "items": { "type": "string" }, "description": "Extra directory names to prune, on top of the defaults." }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "search_text",
            "description": "Search file contents with a regex (ripgrep semantics: parallel walk, binary files and huge files skipped). \
Returns one entry per file with line numbers and matching lines.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex matched against file contents." },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Directories to search. Defaults to every local drive." },
                    "exts": { "type": "array", "items": { "type": "string" }, "description": "Only read files with these extensions (without dot). Strongly recommended for speed." },
                    "max_files": { "type": "integer", "default": 50, "description": "Hard cap on returned files (1-500). Traversal quits once reached." },
                    "max_matches_per_file": { "type": "integer", "default": 5, "description": "Cap on matches reported per file (1-50)." },
                    "max_file_size_mb": { "type": "integer", "default": 10, "description": "Skip files larger than this (1-200 MB)." },
                    "include_hidden": { "type": "boolean", "default": false },
                    "skip_dirs": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "list_drives",
            "description": "List local drives available for searching (letters and whether fixed or removable).",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

// ---------- 工具调用 ----------

fn call_tool(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let body = match name {
        "search_files" => run_search_files(&args),
        "search_text" => run_search_text(&args),
        "list_drives" => Ok(list_drives()),
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };

    Ok(match body {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(msg) => json!({
            "content": [{ "type": "text", "text": format!("error: {msg}") }],
            "isError": true
        }),
    })
}

fn list_drives() -> String {
    let drives: Vec<Value> = integration::local_drives_detailed()
        .into_iter()
        .map(|(d, kind)| json!({ "root": format!("{d}/"), "kind": kind }))
        .collect();
    json!({ "drives": drives }).to_string()
}

fn run_search_files(args: &Value) -> Result<String, String> {
    let pattern = arg_str(args, "pattern").ok_or("missing required argument: pattern")?;
    let re = build_regex(&pattern, arg_bool(args, "regex", true), arg_bool(args, "case_sensitive", false))?;
    let roots = resolve_roots(args)?;
    let max_results = arg_usize(args, "max_results", 100, 1, 2000);
    let include_hidden = arg_bool(args, "include_hidden", false);
    let exts = lower_list(args, "exts");
    let skip = skip_set(args);

    let start = Instant::now();
    let hits: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let hits_c = hits.clone();
    let re_c = re.clone();
    let exts_c = exts.clone();
    let skip_c = skip.clone();
    let scanned = walk_roots(&roots, include_hidden, stop.clone(), move |entry| {
        let path = entry.path();
        if is_skipped(path, &skip_c) {
            return if is_dir(entry) { Step::Skip } else { Step::Continue };
        }
        if !exts_c.is_empty() && !ext_matches(path, &exts_c) {
            return Step::Continue;
        }
        // 元数据用遍历器已缓存的（symlink_metadata），不要 fs::metadata：
        // 后者会跟随 junction，指向失效网络路径时会阻塞整棵遍历
        let Some(hit) = search_filename_meta(path, &re_c, entry.metadata().ok()) else {
            return Step::Continue;
        };
        let mut guard = hits_c.lock().unwrap();
        guard.push(json!({
            "path": hit.path,
            "size": hit.file_size,
            "modified": hit.modified.map(fmt_time),
        }));
        let full = guard.len() >= max_results;
        drop(guard);
        if full { Step::Stop } else { Step::Continue }
    });

    let mut files: Vec<Value> = hits.lock().map(|g| g.clone()).unwrap_or_default();
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let count = files.len();

    Ok(json!({
        "pattern": pattern,
        "roots": roots,
        "scanned": scanned,
        "elapsed_ms": start.elapsed().as_millis(),
        "truncated": count >= max_results,
        "count": count,
        "files": files,
    })
    .to_string())
}

fn run_search_text(args: &Value) -> Result<String, String> {
    let pattern = arg_str(args, "pattern").ok_or("missing required argument: pattern")?;
    let re = build_regex(&pattern, true, arg_bool(args, "case_sensitive", false))?;
    let roots = resolve_roots(args)?;
    let max_files = arg_usize(args, "max_files", 50, 1, 500);
    let per_file = arg_usize(args, "max_matches_per_file", 5, 1, 50);
    let max_bytes = arg_usize(args, "max_file_size_mb", 10, 1, 200) as u64 * 1024 * 1024;
    let include_hidden = arg_bool(args, "include_hidden", false);
    let exts = lower_list(args, "exts");
    let skip = skip_set(args);

    let start = Instant::now();
    let hits: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let hits_c = hits.clone();
    let re_c = re.clone();
    let exts_c = exts.clone();
    let skip_c = skip.clone();
    let scanned = walk_roots(&roots, include_hidden, stop.clone(), move |entry| {
        let path = entry.path();
        if is_skipped(path, &skip_c) {
            return if is_dir(entry) { Step::Skip } else { Step::Continue };
        }
        if !entry.file_type().map_or(false, |t| t.is_file()) {
            return Step::Continue;
        }
        if !exts_c.is_empty() && !ext_matches(path, &exts_c) {
            return Step::Continue;
        }
        let Ok(Some(hit)) = search_file(path, &re_c, max_bytes) else {
            return Step::Continue;
        };
        let matches: Vec<Value> = hit
            .matches
            .iter()
            .take(per_file)
            .map(|m| json!({ "line": m.line_num, "text": m.line }))
            .collect();
        let mut guard = hits_c.lock().unwrap();
        guard.push(json!({
            "path": hit.path,
            "match_count": hit.matches.len(),
            "matches": matches,
        }));
        let full = guard.len() >= max_files;
        drop(guard);
        if full { Step::Stop } else { Step::Continue }
    });

    let mut files: Vec<Value> = hits.lock().map(|g| g.clone()).unwrap_or_default();
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let count = files.len();

    Ok(json!({
        "pattern": pattern,
        "roots": roots,
        "scanned": scanned,
        "elapsed_ms": start.elapsed().as_millis(),
        "truncated": count >= max_files,
        "count": count,
        "files": files,
    })
    .to_string())
}

// ---------- 遍历 ----------

/// 并行遍历若干根目录；回调返回 `Stop` 或外部 `stop` 置位时立即结束整次遍历。
fn walk_roots<F>(roots: &[String], include_hidden: bool, stop: Arc<AtomicBool>, visit: F) -> u64
where
    F: Fn(&ignore::DirEntry) -> Step + Send + Sync + 'static,
{
    let scanned = Arc::new(AtomicU64::new(0));
    let visit = Arc::new(visit);

    let mut builder = WalkBuilder::new(&roots[0]);
    for r in &roots[1..] {
        builder.add(r);
    }
    builder
        .hidden(!include_hidden)
        .git_ignore(false)
        .ignore(false)
        .follow_links(false)
        .threads(num_cpus::get());

    let counter = scanned.clone();
    let stop_flag = stop.clone();
    builder.build_parallel().run(move || {
        let visit = visit.clone();
        let counter = counter.clone();
        let stop = stop_flag.clone();
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            match visit(&entry) {
                Step::Continue => WalkState::Continue,
                Step::Skip => WalkState::Skip,
                Step::Stop => {
                    stop.store(true, Ordering::Relaxed);
                    WalkState::Quit
                }
            }
        })
    });

    scanned.load(Ordering::Relaxed)
}

// ---------- 参数解析 ----------

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn arg_usize(args: &Value, key: &str, default: usize, lo: usize, hi: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| (v as usize).clamp(lo, hi))
        .unwrap_or(default)
}

fn lower_list(args: &Value, key: &str) -> HashSet<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn skip_set(args: &Value) -> HashSet<String> {
    let mut set: HashSet<String> = DEFAULT_SKIP.iter().map(|s| s.to_string()).collect();
    set.extend(lower_list(args, "skip_dirs"));
    set
}

/// 未指定 roots 时用所有本地磁盘；不存在的路径直接剔除，避免整体报错
fn resolve_roots(args: &Value) -> Result<Vec<String>, String> {
    let raw: Vec<String> = match args.get("roots").and_then(Value::as_array) {
        Some(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().replace('\\', "/"))
            .filter(|s| !s.is_empty())
            .collect(),
        None => integration::local_drives().into_iter().map(|d| format!("{d}/")).collect(),
    };
    if raw.is_empty() {
        return Err("no roots to search".to_string());
    }
    let mut out = Vec::new();
    for r in raw {
        if Path::new(&r).exists() {
            out.push(r);
        }
    }
    if out.is_empty() {
        return Err("none of the given roots exist".to_string());
    }
    Ok(out)
}

// ---------- 匹配辅助 ----------

fn build_regex(pattern: &str, is_regex: bool, case_sensitive: bool) -> Result<Regex, String> {
    let src = if is_regex { pattern.to_string() } else { regex::escape(pattern) };
    RegexBuilder::new(&src)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

fn is_dir(entry: &ignore::DirEntry) -> bool {
    entry.file_type().map_or(false, |t| t.is_dir())
}

fn is_skipped(path: &Path, skip: &HashSet<String>) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(s) => s
            .to_str()
            .map_or(false, |n| skip.contains(&n.to_ascii_lowercase())),
        _ => false,
    })
}

fn ext_matches(path: &Path, exts: &HashSet<String>) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map_or(false, |e| exts.contains(&e.to_ascii_lowercase()))
}

fn fmt_time(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}
