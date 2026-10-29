//! 最小 MCP（Model Context Protocol）服务端。
//!
//! 用 `rust-seek.exe --mcp` 启动，走 stdio + 换行分隔的 JSON-RPC 2.0，供 AI Agent
//! 直接调用搜索能力。和 GUI 共用同一个 `searcher`，所以是同一套匹配语义。
//!
//! 为 Agent 场景做的取舍：
//! - 达到 `max_results` 立即 `WalkState::Quit` 掐断整棵遍历，不等全盘走完
//! - 默认用 `WalkState::Skip` 整棵剪掉 node_modules/.git/target/WinSxS 之类目录
//! - 盘符下的 Windows / Program Files / ProgramData 等系统大头目录默认也整棵剪掉
//!   （`prune_system: false` 可关闭）
//! - 结果按路径排序后返回，便于模型稳定引用；响应自带 scanned/elapsed_ms 便于判断是否截断

use crate::fastwalk::{self, Step};
use crate::index;
use crate::integration;
use crate::searcher::{path_is_system_root, search_file, search_filename_meta, SKIP_DIRS};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// 默认整机搜索时整棵跳过的目录（小写比较）。Agent 场景下这些目录只会拖慢搜索。
const DEFAULT_SKIP: &[&str] = SKIP_DIRS;

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
Traversal stops as soon as max_results matches are collected or budget_ms expires, so it never blocks for long. \
roots defaults to the server's working directory (normally your project); pass roots like [\"C:/\",\"D:/\"] to search elsewhere or the whole machine. \
node_modules/.git/target/WinSxS and drive-root system directories are pruned by default. \
Whole-drive searches are served from an in-memory name index: the first such query starts building it in the background and falls back to a normal walk; once built, repeat queries come back in ~0.1-0.5s with source:\"index\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex over the file name (or a plain substring when regex=false)." },
                    "regex": { "type": "boolean", "default": true, "description": "Treat pattern as a regular expression. Set false for literal substring matching." },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Directories to search, e.g. [\"F:/\", \"C:/Users/me/proj\"]. Defaults to the server's working directory (usually the current project); pass [\"C:/\",\"D:/\",...] for a whole-machine search." },
                    "exts": { "type": "array", "items": { "type": "string" }, "description": "Only keep these extensions (without dot), e.g. [\"rs\",\"toml\"]." },
                    "max_results": { "type": "integer", "default": 100, "description": "Hard cap on returned files (1-2000). Traversal quits once reached." },
                    "include_hidden": { "type": "boolean", "default": false },
                    "prune_system": { "type": "boolean", "default": true, "description": "Skip whole-machine noise: C:\\Windows, Program Files, ProgramData, $Recycle.Bin and similar directories directly under a drive root. Set false to traverse them (much slower)." },
                    "budget_ms": { "type": "integer", "default": 10000, "description": "Wall-clock budget for the traversal in ms (0 = unlimited). On expiry the partial result comes back with timed_out:true." },
                    "skip_dirs": { "type": "array", "items": { "type": "string" }, "description": "Extra directory names to prune, on top of the defaults." },
                    "refresh_index": { "type": "boolean", "default": false, "description": "Rebuild the whole-drive name index (takes ~20s in the background) instead of querying it. Results for this call come from a normal walk." },
                    "use_index": { "type": "boolean", "default": false, "description": "Also serve large subdirectory roots from the name index (whole-drive roots already do). Much faster but results may lag the index by up to 24h, so newly created files can be missing." }
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
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Directories to search. Defaults to the server's working directory (usually the current project); pass [\"C:/\",\"D:/\",...] for a whole-machine search." },
                    "exts": { "type": "array", "items": { "type": "string" }, "description": "Only read files with these extensions (without dot). Strongly recommended for speed." },
                    "max_files": { "type": "integer", "default": 50, "description": "Hard cap on returned files (1-500). Traversal quits once reached." },
                    "max_matches_per_file": { "type": "integer", "default": 5, "description": "Cap on matches reported per file (1-50)." },
                    "max_file_size_mb": { "type": "integer", "default": 10, "description": "Skip files larger than this (1-200 MB)." },
                    "include_hidden": { "type": "boolean", "default": false },
                    "prune_system": { "type": "boolean", "default": true, "description": "Skip whole-machine noise: C:\\Windows, Program Files, ProgramData, $Recycle.Bin and similar directories directly under a drive root. Set false to traverse them (much slower)." },
                    "budget_ms": { "type": "integer", "default": 10000, "description": "Wall-clock budget for the traversal in ms (0 = unlimited). On expiry the partial result comes back with timed_out:true." },
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
    let prune_system = arg_bool(args, "prune_system", true);
    let budget_ms = arg_usize(args, "budget_ms", 10_000, 0, 600_000) as u128;
    let exts = lower_list(args, "exts");
    let skip = skip_set(args);
    let extra_skip = args
        .get("skip_dirs")
        .and_then(Value::as_array)
        .map_or(false, |a| !a.is_empty());
    let refresh = arg_bool(args, "refresh_index", false);
    let force_index = arg_bool(args, "use_index", false);

    let start = Instant::now();

    // 整盘查询优先走索引：只有剪枝参数与建索引时完全一致才用，保证结果集不变。
    // 索引没建过时会先尝试磁盘缓存（约 0.2s），没有缓存才退回遍历并在后台建。
    // 子目录查询默认不走索引（可能要刚建的文件），传 use_index: true 才强制用。
    if prune_system
        && !include_hidden
        && !extra_skip
        && (roots_are_drives(&roots) || force_index)
    {
        index::ensure_ready();
        if refresh {
            index::rebuild();
        } else {
            let budget = if budget_ms > 0 {
                Some(Duration::from_millis(budget_ms as u64))
            } else {
                None
            };
            if let Some((hits, entries, age, full)) =
                index::query(&re, &roots, &exts, max_results, budget)
            {
                let mut files: Vec<Value> = hits
                    .into_iter()
                    .map(|h| {
                        json!({
                            "path": h.path,
                            "size": h.size,
                            "modified": h.modified().map(fmt_time),
                        })
                    })
                    .collect();
                files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                let count = files.len();
                return Ok(json!({
                    "pattern": pattern,
                    "roots": roots,
                    "source": "index",
                    "index_entries": entries,
                    "index_age_ms": age,
                    "index_state": index::status(),
                    "elapsed_ms": start.elapsed().as_millis(),
                    "truncated": full,
                    "timed_out": false,
                    "count": count,
                    "files": files,
                })
                .to_string());
            }
        }
    }
    // 整盘查询顺手把索引准备好，后续同类查询就只剩内存扫描
    if roots_are_drives(&roots) || force_index {
        index::ensure_ready();
    }

    let hits: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));

    let hits_c = hits.clone();
    let re_c = re.clone();
    let exts_c = exts.clone();
    let skip_c = skip.clone();
    let timeout_c = timed_out.clone();
    let scanned = walk_roots(&roots, include_hidden, stop.clone(), move |path, info| {
        // 时间预算：宁可返回部分结果并标记 timed_out，也不让 Agent 干等十几秒
        if budget_ms > 0 && start.elapsed().as_millis() >= budget_ms {
            timeout_c.store(true, Ordering::Relaxed);
            return Step::Quit;
        }
        // 盘符下的 Windows / Program Files 等大头目录整棵剪掉（roots 自身不剪）
        if prune_system && info.depth > 0 && path_is_system_root(path) {
            return if info.is_dir { Step::Skip } else { Step::Continue };
        }
        if is_skipped(path, &skip_c) {
            return if info.is_dir { Step::Skip } else { Step::Continue };
        }
        if !exts_c.is_empty() && !ext_matches(path, &exts_c) {
            return Step::Continue;
        }
        // 大小/时间直接用目录枚举带回的，命中时零系统调用
        let Some(hit) = search_filename_meta(path, &re_c, || Some((info.size, info.modified))) else {
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
        if full { Step::Quit } else { Step::Continue }
    });

    let mut files: Vec<Value> = hits.lock().map(|g| g.clone()).unwrap_or_default();
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let count = files.len();
    let timed_out = timed_out.load(Ordering::Relaxed);

    Ok(json!({
        "pattern": pattern,
        "roots": roots,
        "source": "walk",
        "index_state": index::status(),
        "scanned": scanned,
        "elapsed_ms": start.elapsed().as_millis(),
        "truncated": timed_out || count >= max_results,
        "timed_out": timed_out,
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
    let prune_system = arg_bool(args, "prune_system", true);
    let budget_ms = arg_usize(args, "budget_ms", 10_000, 0, 600_000) as u128;
    let exts = lower_list(args, "exts");
    let skip = skip_set(args);

    let start = Instant::now();
    let hits: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));

    let hits_c = hits.clone();
    let re_c = re.clone();
    let exts_c = exts.clone();
    let skip_c = skip.clone();
    let timeout_c = timed_out.clone();
    let scanned = walk_roots(&roots, include_hidden, stop.clone(), move |path, info| {
        if budget_ms > 0 && start.elapsed().as_millis() >= budget_ms {
            timeout_c.store(true, Ordering::Relaxed);
            return Step::Quit;
        }
        if prune_system && info.depth > 0 && path_is_system_root(path) {
            return if info.is_dir { Step::Skip } else { Step::Continue };
        }
        if is_skipped(path, &skip_c) {
            return if info.is_dir { Step::Skip } else { Step::Continue };
        }
        if info.is_dir {
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
        if full { Step::Quit } else { Step::Continue }
    });

    let mut files: Vec<Value> = hits.lock().map(|g| g.clone()).unwrap_or_default();
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let count = files.len();
    let timed_out = timed_out.load(Ordering::Relaxed);

    Ok(json!({
        "pattern": pattern,
        "roots": roots,
        "scanned": scanned,
        "elapsed_ms": start.elapsed().as_millis(),
        "truncated": timed_out || count >= max_files,
        "timed_out": timed_out,
        "count": count,
        "files": files,
    })
    .to_string())
}

// ---------- 遍历 ----------

/// 并行遍历若干根目录；回调返回 `Step::Quit` 或外部 `stop` 置位时立即结束整次遍历。
fn walk_roots<F>(roots: &[String], include_hidden: bool, stop: Arc<AtomicBool>, visit: F) -> u64
where
    F: Fn(&Path, &fastwalk::Info) -> Step + Sync,
{
    let paths: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let opts = fastwalk::Options { include_hidden, threads: num_cpus::get() };
    fastwalk::walk(&paths, &opts, &stop, visit)
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

/// 未指定 roots 时用工作目录（MCP 客户端通常把 cwd 设成项目根），
/// 让 Agent 的「找 XX 文件」默认落在项目里、几十毫秒出结果；
/// cwd 是盘符根、系统目录或者干脆拿不到时才退回所有本地磁盘。
fn default_roots() -> Vec<String> {
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.to_string_lossy().replace('\\', "/");
        let depth = Path::new(&p).components().count();
        if depth >= 3 && !path_is_system_root(Path::new(&p)) {
            return vec![p];
        }
    }
    integration::local_drives().into_iter().map(|d| format!("{d}/")).collect()
}

/// 未指定 roots 时用工作目录 / 所有本地磁盘；不存在的路径直接剔除，避免整体报错
fn resolve_roots(args: &Value) -> Result<Vec<String>, String> {
    let raw: Vec<String> = match args.get("roots").and_then(Value::as_array) {
        Some(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().replace('\\', "/"))
            .filter(|s| !s.is_empty())
            .collect(),
        None => default_roots(),
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

/// roots 全是盘符根（`C:/`、`D:/`）时才可能有对应的整盘索引。
fn roots_are_drives(roots: &[String]) -> bool {
    !roots.is_empty()
        && roots.iter().all(|r| {
            let b = r.as_bytes();
            b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
        })
}
