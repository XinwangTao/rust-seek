//! 整机文件名索引：建一次（或从磁盘缓存载入），之后按名字匹配，百毫秒级返回。
//!
//! 实测这台机器整机遍历 250 万条目约 19s，**而且第二遍同样慢**——Windows 不会把
//! 全部目录元数据留在缓存里，「多搜几次就快了」不成立。所以对常驻的 MCP server 来说，
//! 唯一能让整盘搜索变快的手段就是索引：
//!
//! - 首次整盘查询：后台建索引（约 20s），本次查询退回普通遍历
//! - 之后同一进程内：0.1～0.2s 返回
//! - 进程重启：从磁盘缓存载入（约 0.3s），不用重扫
//!
//! 内存布局用一整块字符串 + 32 字节定长条目，而不是每条一个 `Box<str>`：
//! 160 万条目下 ~140MB（每条约 88 字节）而不是 ~260MB，扫描时也是顺序访问。
//!
//! 索引只收文件（目录不进索引），剪枝规则与普通搜索一致：系统大头目录、回收站、
//! node_modules 之类噪声目录都不进。所以「走索引」和「走遍历」给出相同的结果集。

use crate::fastwalk::{self, Step};
use crate::integration;
use crate::searcher::{is_noise_dir_name, path_is_system_root, SKIP_DIRS};
use regex::Regex;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

/// 条目上限，防止在超大存储上把内存吃穿
const MAX_ENTRIES: usize = 4_000_000;
/// 缓存年龄超过这个值就在后台重建（旧索引继续服务）
const TTL_SECS: u64 = 24 * 3600;
/// 超过这个年龄的缓存直接不用，重新遍历
const MAX_CACHE_AGE_SECS: u64 = 7 * 24 * 3600;

const MAGIC: &[u8; 8] = b"RSKIDX\x01\x00";
const HDR: usize = 32;
const ITEM_BYTES: usize = 32;

#[derive(Clone, Copy)]
struct Item {
    path_off: u32,
    path_len: u32,
    name_off: u32,
    name_len: u32,
    size: u64,
    mtime: u64,
}

struct Index {
    /// 所有路径按顺序拼接（`F:/a/b.rs` 形式），条目用偏移引用
    blob: String,
    items: Vec<Item>,
    built_unix: u64,
}

static INDEX: Mutex<Option<Box<Index>>> = Mutex::new(None);
static BUILDING: AtomicBool = AtomicBool::new(false);

pub struct Hit {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
}

/// 索引可用时按文件名匹配；不可用返回 None，调用方照常走目录遍历。
/// 返回 `(命中, 索引条目数, 索引年龄 ms, 是否提前结束)`。
pub fn lookup(
    re: &Regex,
    exts: &HashSet<String>,
    max: usize,
    budget: Option<Duration>,
) -> Option<(Vec<Hit>, u64, u128, bool)> {
    let guard = lock();
    let index = guard.as_ref()?;
    let start = std::time::Instant::now();
    let mut hits = Vec::new();
    let mut full = false;
    for item in &index.items {
        if hits.len() >= max {
            full = true;
            break;
        }
        if let Some(b) = budget {
            if start.elapsed() >= b {
                full = true;
                break;
            }
        }
        let name = &index.blob[item.name_off as usize..(item.name_off + item.name_len) as usize];
        if !exts.is_empty() && !ext_matches(name, exts) {
            continue;
        }
        if re.is_match(name) {
            let path = &index.blob[item.path_off as usize..(item.path_off + item.path_len) as usize];
            hits.push(Hit { path: path.to_string(), size: item.size, mtime: item.mtime });
        }
    }
    let age_ms = now_unix().saturating_sub(index.built_unix) as u128 * 1000;
    Some((hits, index.items.len() as u64, age_ms, full))
}

/// `idle` / `building` / `ready`
pub fn status() -> &'static str {
    if BUILDING.load(Ordering::Relaxed) {
        return "building";
    }
    if lock().is_some() {
        "ready"
    } else {
        "idle"
    }
}

/// 保证索引可用：已有就检查是否需要后台刷新；没有就尝试磁盘缓存；再没有就后台建。
/// 磁盘缓存载入是同步的（约 0.3s），因为调用方可能马上要查。
pub fn ensure_ready() {
    enum Ready {
        Fresh,
        Stale,
        Missing,
    }
    let state = {
        let guard = lock();
        match guard.as_ref() {
            Some(index) => {
                if now_unix().saturating_sub(index.built_unix) > TTL_SECS {
                    Ready::Stale
                } else {
                    Ready::Fresh
                }
            }
            None => Ready::Missing,
        }
    };

    match state {
        Ready::Fresh => {}
        Ready::Stale => rebuild(),
        Ready::Missing => {
            // BUILDING 兼作载入锁，避免两个请求同时读盘
            if BUILDING.swap(true, Ordering::SeqCst) {
                return;
            }
            let loaded = load_cache();
            let loaded_ok = loaded.is_some();
            {
                let mut guard = lock();
                if let Some(index) = loaded {
                    *guard = Some(Box::new(index));
                }
            }
            BUILDING.store(false, Ordering::SeqCst);

            if !loaded_ok {
                if !BUILDING.swap(true, Ordering::SeqCst) {
                    spawn_build();
                }
            } else if lock()
                .as_ref()
                .map_or(false, |i| now_unix().saturating_sub(i.built_unix) > TTL_SECS)
            {
                rebuild();
            }
        }
    }
}

/// 强制重建；重建期间旧索引仍可查询。已在建则忽略。
pub fn rebuild() {
    if BUILDING.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn_build();
}

fn spawn_build() {
    // 调用前 BUILDING 已置 true
    std::thread::spawn(|| {
        let built = build();
        if let Some(index) = &built {
            save_cache(index);
        }
        let mut guard = lock();
        if let Some(index) = built {
            *guard = Some(Box::new(index));
        }
        BUILDING.store(false, Ordering::SeqCst);
    });
}

fn lock() -> MutexGuard<'static, Option<Box<Index>>> {
    INDEX.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------- 构建 ----------

type RawItem = (Box<str>, u64, u64);

fn build() -> Option<Index> {
    let roots: Vec<PathBuf> = integration::local_drives()
        .into_iter()
        .map(|d| PathBuf::from(format!("{d}/")))
        .collect();
    if roots.is_empty() {
        return None;
    }

    let cancel = AtomicBool::new(false);
    let overflow = AtomicBool::new(false);
    let count = std::sync::atomic::AtomicUsize::new(0);

    // 遍历回调是 `Fn + Sync` 且被所有线程共享：用 channel 单向下发，
    // 每条只做一次原子操作，避免 160 万次共享锁竞争。
    let (tx, rx) = std::sync::mpsc::channel::<RawItem>();
    let collector = std::thread::spawn(move || {
        let mut raw: Vec<RawItem> = Vec::with_capacity(1 << 19);
        while let Ok(item) = rx.recv() {
            raw.push(item);
        }
        raw
    });

    {
        let tx = tx.clone();
        fastwalk::walk(
            &roots,
            &fastwalk::Options { include_hidden: false, threads: num_cpus::get() },
            &cancel,
            |path, info| {
                if info.depth > 0 && is_pruned(path) {
                    return if info.is_dir { Step::Skip } else { Step::Continue };
                }
                if info.is_dir {
                    return Step::Continue;
                }
                if count.fetch_add(1, Ordering::Relaxed) >= MAX_ENTRIES {
                    overflow.store(true, Ordering::Relaxed);
                    return Step::Quit;
                }
                let item = (
                    path.to_string_lossy().replace('\\', "/").into_boxed_str(),
                    info.size,
                    info.modified
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                );
                if tx.send(item).is_err() {
                    return Step::Quit;
                }
                Step::Continue
            },
        );
    }
    drop(tx);
    let raw = collector.join().ok()?;
    if overflow.load(Ordering::Relaxed) {
        eprintln!("rust-seek: index hit the {MAX_ENTRIES} entry cap, results may be incomplete");
    }

    let total: usize = raw.iter().map(|(p, _, _)| p.len()).sum();
    let mut blob = String::with_capacity(total);
    let mut items = Vec::with_capacity(raw.len());
    for (path, size, mtime) in raw {
        if blob.len() + path.len() > u32::MAX as usize {
            break;
        }
        let path_off = blob.len() as u32;
        blob.push_str(&path);
        let path_len = path.len() as u32;
        let rel = path.rfind('/').map_or(0, |i| i + 1);
        items.push(Item {
            path_off,
            path_len,
            name_off: path_off + rel as u32,
            name_len: path_len - rel as u32,
            size,
            mtime,
        });
    }

    Some(Index { blob, items, built_unix: now_unix() })
}

/// 与搜索默认剪枝完全一致：盘符根下的系统大头目录 + 任意层级的噪声目录。
fn is_pruned(path: &Path) -> bool {
    if path_is_system_root(path) {
        return true;
    }
    path.components().any(|c| match c {
        std::path::Component::Normal(s) => s.to_str().map_or(false, |n| {
            is_noise_dir_name(n) || SKIP_DIRS.iter().any(|d| n.eq_ignore_ascii_case(d))
        }),
        _ => false,
    })
}

fn ext_matches(name: &str, exts: &HashSet<String>) -> bool {
    match name.rsplit_once('.') {
        Some((_, e)) if !e.is_empty() && !e.contains('/') => exts.contains(&e.to_ascii_lowercase()),
        _ => false,
    }
}

// ---------- 磁盘缓存 ----------

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("rust-seek").join("index-v1.bin"))
}

fn load_cache() -> Option<Index> {
    let path = cache_path()?;
    let len = std::fs::metadata(&path).ok()?.len();
    if len < HDR as u64 + MAGIC.len() as u64 || len > 4_000_000_000 {
        return None;
    }
    let mut buf = Vec::with_capacity(len as usize);
    std::fs::File::open(&path).ok()?.read_to_end(&mut buf).ok()?;

    if buf.len() < HDR || &buf[..8] != MAGIC {
        return None;
    }
    let built_unix = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    if now_unix().saturating_sub(built_unix) > MAX_CACHE_AGE_SECS {
        return None;
    }
    let blob_len = u64::from_le_bytes(buf[16..24].try_into().ok()?) as usize;
    let count = u64::from_le_bytes(buf[24..32].try_into().ok()?) as usize;
    if count > MAX_ENTRIES || HDR.checked_add(blob_len)?.checked_add(count * ITEM_BYTES)? != buf.len() {
        return None;
    }

    let blob_end = HDR + blob_len;
    let blob = String::from_utf8(buf[HDR..blob_end].to_vec()).ok()?;
    let raw = &buf[blob_end..];
    let mut items = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(ITEM_BYTES) {
        let item = Item {
            path_off: u32::from_le_bytes(chunk[0..4].try_into().ok()?),
            path_len: u32::from_le_bytes(chunk[4..8].try_into().ok()?),
            name_off: u32::from_le_bytes(chunk[8..12].try_into().ok()?),
            name_len: u32::from_le_bytes(chunk[12..16].try_into().ok()?),
            size: u64::from_le_bytes(chunk[16..24].try_into().ok()?),
            mtime: u64::from_le_bytes(chunk[24..32].try_into().ok()?),
        };
        // 偏移必须落在 blob 内且在 UTF-8 边界上，否则整份缓存作废
        let ok = |off: u32, len: u32| {
            let (o, l) = (off as usize, len as usize);
            o.checked_add(l).map_or(false, |end| end <= blob.len())
                && blob.is_char_boundary(o)
                && blob.is_char_boundary(o + l as usize)
        };
        if !ok(item.path_off, item.path_len) || !ok(item.name_off, item.name_len) {
            return None;
        }
        items.push(item);
    }

    Some(Index { blob, items, built_unix })
}

fn save_cache(index: &Index) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut buf = Vec::with_capacity(HDR + index.blob.len() + index.items.len() * ITEM_BYTES);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&index.built_unix.to_le_bytes());
    buf.extend_from_slice(&(index.blob.len() as u64).to_le_bytes());
    buf.extend_from_slice(&(index.items.len() as u64).to_le_bytes());
    buf.extend_from_slice(index.blob.as_bytes());
    for it in &index.items {
        buf.extend_from_slice(&it.path_off.to_le_bytes());
        buf.extend_from_slice(&it.path_len.to_le_bytes());
        buf.extend_from_slice(&it.name_off.to_le_bytes());
        buf.extend_from_slice(&it.name_len.to_le_bytes());
        buf.extend_from_slice(&it.size.to_le_bytes());
        buf.extend_from_slice(&it.mtime.to_le_bytes());
    }

    // 临时文件名带 pid：多个 MCP 进程同时重建时各写各的，最后 rename 覆盖
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(_) => return,
    };
    if file.write_all(&buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(file);
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
