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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// 条目上限默认值，防止在超大存储上把内存吃穿（800 万条 ≈ 600MB；每 100 万条约 75MB）。
/// 可用 `RUST_SEEK_MAX_ENTRIES` 覆盖（调小省内存，或调大覆盖更大机器）。
/// 撞上限时索引只覆盖部分目录，查询会回退真实遍历兜底，不会给出假阴性。
const MAX_ENTRIES_DEFAULT: usize = 8_000_000;
/// 缓存校验用的硬上限：即使当前配置调小，也不因此丢弃更大但合法的旧缓存
const MAX_ENTRIES_CEIL: usize = 20_000_000;
/// 缓存年龄超过这个值就整盘重建一次（旧索引继续服务，增量刷新解决不了的情形兜底）。
/// 平时靠增量刷新保新鲜，这个只负责定期清掉增量可能的边角隐患。
const TTL_SECS: u64 = 24 * 3600;
/// 超过这个年龄的缓存直接不用，重新遍历
const MAX_CACHE_AGE_SECS: u64 = 7 * 24 * 3600;

/// 缓存格式 v4：在 v3 基础上加一张「目录表」（目录路径 + 该目录自身的 LastWriteTime）。
/// 目录 mtime 不变 ⇒ 里面没有增删改名 ⇒ 整棵子树可以整段复用，增量刷新不用重扫全盘。
/// 旧版本缓存因 MAGIC 不匹配自动作废，重新建一次即可。
const MAGIC: &[u8; 8] = b"RSKIDX\x06\x00";
/// v5：水位并入缓存；v4：没有水位。都能读，但只有 v6 的条目/目录块是 8 字节对齐的，
/// 能直接 mmap 成切片零拷贝查询；v4/v5 载入后按老路复制一份。
const MAGIC_V5: &[u8; 8] = b"RSKIDX\x05\x00";
/// 旧格式（v4）仍可读：读进来没有 USN 水位，随后做一次全盘巡把水位种上，
/// 避免升级后立刻重扫 760 万条重建索引。
const MAGIC_V4: &[u8; 8] = b"RSKIDX\x04\x00";
const HDR: usize = 56;
/// v5 在 56 字节头后多一个 usn_count，末尾跟若干 24 字节水位记录
const HDR_V5: usize = HDR + 8;
const USN_BYTES: usize = 24;

/// 把偏移补到 8 字节对齐：条目/目录块要能直接当 `&[Item]` 用，起点必须对齐。
fn pad8(off: usize) -> usize {
    (8 - (off % 8)) % 8
}
const FLAG_COMPLETE: u64 = 1 << 0;
/// 刷新时有目录读不到：该子树按旧快照给出，结果里要如实标注
const FLAG_INCOMPLETE_DIRS: u64 = 1 << 1;

fn max_entries() -> usize {
    std::env::var("RUST_SEEK_MAX_ENTRIES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 100_000)
        .map(|n| n.min(MAX_ENTRIES_CEIL))
        .unwrap_or(MAX_ENTRIES_DEFAULT)
}
const ITEM_BYTES: usize = 32;
const DIR_BYTES: usize = 16;
/// 目录 mtime 距本次刷新不足这么多秒时，不敢用「mtime 没变 = 子树没变」判定：
/// 秒级 mtime 下「同一秒内新建又删除」不会留下痕迹，留 2s 余量、重新枚举一遍即可。
const DIR_SLACK_SECS: u64 = 2;

/// 索引超过这么久就做一次目录级增量刷新。
/// 监听在跑时刷新只枚举 mtime 变了的目录（几十毫秒）；但「需要全盘巡」时一次约 20s
/// （31.7 万目录 × ~1ms，过滤驱动下 stat 很慢），所以阈值不能太小：
/// 2026-11-06 由 2s 提到 30s —— 全盘巡的结果现在会落盘，别的进程 30s 内加载到它
/// 就直接跳过巡；超过 30s 仍会刷新。`RUST_SEEK_REFRESH_SECS` 可覆盖。
pub fn auto_refresh_secs() -> u64 {
    std::env::var("RUST_SEEK_REFRESH_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Item {
    path_off: u32,
    path_len: u32,
    name_off: u32,
    name_len: u32,
    size: u64,
    mtime: u64,
}

/// 目录表条目：目录路径（在 `dir_blob` 里的偏移）+ 上次枚举它时的 LastWriteTime。
#[repr(C)]
#[derive(Clone, Copy)]
struct DirRec {
    path_off: u32,
    path_len: u32,
    mtime: u64,
}

/// 索引数据：要么自己拥有（写者 / 刷新后），要么直接映射缓存文件（读者零拷贝）。
///
/// 读者只读，永远不需要复制 700MB；写者刷新时 `ensure_owned()` 一次转成 owned。
enum Payload {
    Owned {
        /// 所有路径按顺序拼接（`F:/a/b.rs` 形式），条目用偏移引用
        blob: String,
        items: Vec<Item>,
        /// 已进索引的目录（路径升序，不含盘符根）+ 它们的路径文本
        dir_blob: String,
        dirs: Vec<DirRec>,
    },
    Mapped(Mapped),
}

/// 映射视图：每段记「起点偏移 + 元素个数」。
struct Mapped {
    map: std::sync::Arc<memmap2::Mmap>,
    blob: (usize, usize),
    items: (usize, usize),
    dir_blob: (usize, usize),
    dirs: (usize, usize),
}

// 缓存里的条目/目录块是按固定字节布局排布的，mmap 后直接当切片读。
// 改了字段就必须同步改 ITEM_BYTES/DIR_BYTES 并升 MAGIC，否则会静默读错。
const _: () = assert!(std::mem::size_of::<Item>() == ITEM_BYTES);
const _: () = assert!(std::mem::size_of::<DirRec>() == DIR_BYTES);

struct Index {
    payload: Payload,
    /// 这份索引覆盖到的 USN 水位（每块盘一条）。**必须和缓存同生共死**：
    /// 分两个文件写会在并发下出现「旧内容 + 新水位」，那批变更被永久跳过。
    usn: Vec<crate::usn::Commit>,
    built_unix: u64,
    /// false = 构建时撞到条目上限提前收尾，这台机器有文件没进索引。
    /// 查询方必须据此回退真实遍历，否则会拿到假阴性。
    complete: bool,
    /// 上次增量刷新有目录读不到：该子树仍按旧快照给出，结果需标注
    incomplete_dirs: bool,
}

static INDEX: Mutex<Option<Box<Index>>> = Mutex::new(None);
static BUILDING: AtomicBool = AtomicBool::new(false);
/// 构建状态的互斥锁：acquire / Drop / 卡死复位必须互斥。
/// 否则「复位」与「新构建」交错时，新构建的 Drop 会因代次不匹配而不清标志，
/// 进程永久停在 building；反过来旧构建也可能清掉新构建的标志。
static BUILD_LOCK: Mutex<()> = Mutex::new(());
/// 构建代次：每拿到一次构建权 +1。卡死复位后旧线程收尾时据此判断
/// 「自己已经不是当前构建」，不会误清掉新构建的标志。
static BUILD_GEN: AtomicU64 = AtomicU64::new(0);
/// 本次构建开始时间（unix 秒），用于卡死检测
static BUILD_STARTED: AtomicU64 = AtomicU64::new(0);
/// 构建超过这个时间还没结束就视为卡死（正常整盘约 40s；本机有磁盘假死史）。
/// 卡死时若不复位，进程会永远停在 building：索引不再刷新、内存也不再回收。
const BUILD_STUCK_SECS: u64 = 900;

/// 构建权守卫：acquire 时置 BUILDING，Drop 时（含 panic 展开）释放。
/// 没有它，构建线程一次 panic 就会让该进程永久停在 building。
struct BuildSlot(u64);

impl BuildSlot {
    fn acquire() -> Option<BuildSlot> {
        let _g = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if BUILDING.load(Ordering::SeqCst) {
            return None;
        }
        BUILDING.store(true, Ordering::SeqCst);
        let gen = BUILD_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        BUILD_STARTED.store(now_unix(), Ordering::SeqCst);
        Some(BuildSlot(gen))
    }
}

impl Drop for BuildSlot {
    fn drop(&mut self) {
        let _g = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if BUILD_GEN.load(Ordering::SeqCst) == self.0 {
            BUILDING.store(false, Ordering::SeqCst);
            BUILD_STARTED.store(0, Ordering::SeqCst);
        }
    }
}

/// 构建超过 [`BUILD_STUCK_SECS`] 仍未结束：视为卡死并复位，允许重新构建。
/// `observed_started` 是调用方看到的时间戳：期间若已开始新构建则不复位。
fn reset_stuck_build(observed_started: u64) -> bool {
    let _g = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !BUILDING.load(Ordering::SeqCst) || BUILD_STARTED.load(Ordering::SeqCst) != observed_started {
        return false;
    }
    BUILDING.store(false, Ordering::SeqCst);
    BUILD_GEN.fetch_add(1, Ordering::SeqCst);
    BUILD_STARTED.store(0, Ordering::SeqCst);
    true
}
/// 兜底遍历后回填索引的节流时钟（unix 秒）
static LAST_REFRESH_REQ: AtomicU64 = AtomicU64::new(0);
/// 内存索引最后一次被查询的时间（unix 秒）
static LAST_USE: AtomicU64 = AtomicU64::new(0);
static REAPER: AtomicBool = AtomicBool::new(false);
/// 内存索引约 600MB/进程（本机 620 万条）。空闲超过这个时间就释放，
/// 下次查询从磁盘缓存重新载入（约 0.2s）。否则多个 MCP 进程 + GUI 会常驻 4GB+。
/// `RUST_SEEK_IDLE_RELEASE_SECS` 可覆盖（测试/内存紧张时调小）。
fn idle_release_secs() -> u64 {
    std::env::var("RUST_SEEK_IDLE_RELEASE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n >= 10)
        .unwrap_or(600)
}

pub struct Hit {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
}

impl Hit {
    pub fn modified(&self) -> Option<std::time::SystemTime> {
        if self.mtime == 0 {
            None
        } else {
            Some(UNIX_EPOCH + Duration::from_secs(self.mtime))
        }
    }
}

impl Index {
    fn new_owned(
        blob: String,
        items: Vec<Item>,
        dir_blob: String,
        dirs: Vec<DirRec>,
    ) -> Payload {
        Payload::Owned { blob, items, dir_blob, dirs }
    }

    fn blob(&self) -> &str {
        match &self.payload {
            Payload::Owned { blob, .. } => blob,
            // 载入时已整段校验过 UTF-8
            Payload::Mapped(m) => unsafe {
                std::str::from_utf8_unchecked(&m.map[m.blob.0..m.blob.1])
            },
        }
    }

    fn dir_blob(&self) -> &str {
        match &self.payload {
            Payload::Owned { dir_blob, .. } => dir_blob,
            Payload::Mapped(m) => unsafe {
                std::str::from_utf8_unchecked(&m.map[m.dir_blob.0..m.dir_blob.1])
            },
        }
    }

    fn items(&self) -> &[Item] {
        match &self.payload {
            Payload::Owned { items, .. } => items,
            // 保存时按 8 字节对齐排布，且长度已按头部校验过
            Payload::Mapped(m) => unsafe {
                std::slice::from_raw_parts(m.map.as_ptr().add(m.items.0) as *const Item, m.items.1)
            },
        }
    }

    fn dirs(&self) -> &[DirRec] {
        match &self.payload {
            Payload::Owned { dirs, .. } => dirs,
            Payload::Mapped(m) => unsafe {
                std::slice::from_raw_parts(m.map.as_ptr().add(m.dirs.0) as *const DirRec, m.dirs.1)
            },
        }
    }

    /// 要把映射视图改成自己的（刷新/重建前调用一次）。
    fn ensure_owned(&mut self) {
        let Payload::Mapped(m) = &self.payload else { return };
        let blob = unsafe { String::from_utf8_unchecked(m.map[m.blob.0..m.blob.1].to_vec()) };
        let dir_blob =
            unsafe { String::from_utf8_unchecked(m.map[m.dir_blob.0..m.dir_blob.1].to_vec()) };
        let items: Vec<Item> = self.items().to_vec();
        let dirs: Vec<DirRec> = self.dirs().to_vec();
        self.payload = Payload::Owned { blob, items, dir_blob, dirs };
    }

    fn blob_mut(&mut self) -> &mut String {
        self.ensure_owned();
        match &mut self.payload {
            Payload::Owned { blob, .. } => blob,
            Payload::Mapped(_) => unreachable!(),
        }
    }

    fn dir_blob_mut(&mut self) -> &mut String {
        self.ensure_owned();
        match &mut self.payload {
            Payload::Owned { dir_blob, .. } => dir_blob,
            Payload::Mapped(_) => unreachable!(),
        }
    }

    fn items_mut(&mut self) -> &mut Vec<Item> {
        self.ensure_owned();
        match &mut self.payload {
            Payload::Owned { items, .. } => items,
            Payload::Mapped(_) => unreachable!(),
        }
    }

    fn dirs_mut(&mut self) -> &mut Vec<DirRec> {
        self.ensure_owned();
        match &mut self.payload {
            Payload::Owned { dirs, .. } => dirs,
            Payload::Mapped(_) => unreachable!(),
        }
    }

    fn path(&self, item: &Item) -> &str {
        path_of(self.blob(), item)
    }

    fn name(&self, item: &Item) -> &str {
        let b = self.blob();
        &b[item.name_off as usize..(item.name_off + item.name_len) as usize]
    }
}

/// 条目的路径文本。独立成自由函数：增量刷新会先把旧条目表 take 出去，
/// 只剩 `blob` 可借，此时还能照旧读路径。
fn path_of<'a>(blob: &'a str, item: &Item) -> &'a str {
    &blob[item.path_off as usize..(item.path_off + item.path_len) as usize]
}

fn dir_path_of<'a>(dir_blob: &'a str, rec: &DirRec) -> &'a str {
    &dir_blob[rec.path_off as usize..(rec.path_off + rec.path_len) as usize]
}

fn unix_secs(t: Option<std::time::SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 索引可用时按文件名匹配；不可用返回 None，调用方照常走目录遍历。
///
/// `roots` 为空表示全盘；否则只扫这些前缀命中的条目（条目按路径排序，二分定位）。
/// 返回 `(命中, 索引条目数, 索引年龄 ms, 是否提前结束, 索引是否完整)`。
/// 最后一项为 false 时，命中为空/不足 **不能** 当作「文件不存在」，
/// 调用方应回退遍历兜底（索引在条目上限处截断，剩余目录没进索引）。
pub fn query(
    re: &Regex,
    roots: &[String],
    exts: &HashSet<String>,
    max: usize,
    budget: Option<Duration>,
    match_path: bool,
) -> Option<(Vec<Hit>, u64, u128, bool, bool)> {
    let guard = lock();
    let index = guard.as_ref()?;
    touch_index();
    // 索引里存的是正斜杠路径；粘贴反斜杠路径（D:\work\a.rs）时索引匹配不上，
    // 直接交回调用方走真实遍历（searcher 会做原生形式二次匹配），
    // 否则会以 source:"index"、truncated:false 回一个假的「没有」。
    if match_path && re.as_str().contains('\\') {
        return None;
    }
    let start = Instant::now();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    if roots.is_empty() {
        ranges.push((0, index.items().len()));
    } else {
        for root in roots {
            let prefix = norm_prefix(root);
            let (lo, hi) = prefix_bounds(index, &prefix);
            if hi > lo {
                ranges.push((lo, hi));
            } else {
                // 二分没找到：多半是用户输入的路径大小写和磁盘不一致，退化成线性比
                loose.push(prefix);
            }
        }
    }

    // 全盘 618 万条目顺序扫一遍约 0.5s，是每次搜索的固定成本。
    // 按 64k 条目切块并行扫：块数远大于线程数，天然负载均衡，
    // 也不用担心某个盘/某段路径特别慢把整体拖住。
    const CHUNK: usize = 64 * 1024;
    let mut tasks: Vec<(usize, usize)> = Vec::new();
    for (lo, hi) in &ranges {
        let mut a = *lo;
        while a < *hi {
            let b = (a + CHUNK).min(*hi);
            tasks.push((a, b));
            a = b;
        }
    }

    let max = max.max(1);
    let mut hits: Vec<Hit> = Vec::new();
    let mut truncated = false;
    // 纯字面量 + 名字匹配才走连续扫描；任意正则与路径查询一律保留逐条匹配
    let fast_literal = !match_path && safe_literal(re);

    if !tasks.is_empty() {
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let out: Mutex<Vec<Hit>> = Mutex::new(Vec::new());
        let workers = num_cpus::get().clamp(1, 16).min(tasks.len());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let cursor = &cursor;
                let stop = &stop;
                let out = &out;
                let tasks = &tasks;
                scope.spawn(move || {
                    let mut local: Vec<Hit> = Vec::new();
                    // 快路径缓冲按 worker 复用，跨任务只 clear 不重新分配
                    let mut name_buf = String::new();
                    let mut name_map: Vec<(usize, u32)> = Vec::new();
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Some(b) = budget {
                            if start.elapsed() >= b {
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some((lo, hi)) = tasks.get(i).copied() else { break };
                        let chunk = &index.items()[lo..hi];
                        let limit_hit = if fast_literal {
                            scan_literal_chunk(
                                index,
                                chunk,
                                re,
                                exts,
                                max,
                                &mut local,
                                &mut name_buf,
                                &mut name_map,
                            )
                        } else {
                            for item in chunk {
                                // 单线程也设上限：命中上万的模式（比如搜 "a"）不该把结果集撑爆
                                if local.len() >= max {
                                    break;
                                }
                                match_item(index, item, re, exts, &mut local, match_path);
                            }
                            local.len() >= max
                        };
                        if limit_hit {
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    if !local.is_empty() {
                        out.lock().unwrap().extend(local);
                    }
                });
            }
        });

        hits = out.into_inner().unwrap_or_default();
        // 截断时先按路径排序再取前 max 条：并行扫的顺序随线程调度变化，
        // 不排序的话同一查询两次可能给出不同结果集。
        if hits.len() > max {
            truncated = true;
            hits.sort_unstable_by(|a, b| a.path.cmp(&b.path));
            hits.truncate(max);
        }
        if stop.load(Ordering::Relaxed) {
            truncated = true;
        }
    }

    // 大小写和磁盘不一致时二分找不到区间，退化成线性比（罕见路径，保持串行）
    if !loose.is_empty() {
        for item in index.items() {
            if hits.len() >= max {
                truncated = true;
                break;
            }
            if let Some(b) = budget {
                if start.elapsed() >= b {
                    truncated = true;
                    break;
                }
            }
            let path = index.path(item);
            let hit_prefix = loose.iter().any(|p| {
                path.get(..p.len())
                    .map_or(false, |head| head.eq_ignore_ascii_case(p))
            });
            if hit_prefix {
                match_item(index, item, re, exts, &mut hits, match_path);
            }
        }
    }

    let age_ms = now_unix().saturating_sub(index.built_unix) as u128 * 1000;
    Some((hits, index.items().len() as u64, age_ms, truncated, index.complete))
}

/// 只按扩展名 + 根目录取候选文件，不做名字/内容匹配。
///
/// 给内容搜索用：整机遍历是 19s 的瓶颈，而按扩展名筛完通常只剩几千个文件，
/// 先拿候选再逐个读，就绕开了遍历。返回 `(候选, 索引条目数, 索引年龄 ms, 是否被截断, 索引是否完整)`。
pub fn candidates(
    roots: &[String],
    exts: &HashSet<String>,
    max: usize,
) -> Option<(Vec<Hit>, u64, u128, bool, bool)> {
    let guard = lock();
    let index = guard.as_ref()?;
    touch_index();
    let mut hits = Vec::new();
    let mut truncated = false;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    if roots.is_empty() {
        ranges.push((0, index.items().len()));
    } else {
        for root in roots {
            let prefix = norm_prefix(root);
            let (lo, hi) = prefix_bounds(index, &prefix);
            if hi > lo {
                ranges.push((lo, hi));
            } else {
                loose.push(prefix);
            }
        }
    }

    let push = |index: &Index, item: &Item, hits: &mut Vec<Hit>| -> bool {
        if !exts.is_empty() && !ext_matches(index.name(item), exts) {
            return true;
        }
        hits.push(Hit {
            path: index.path(item).to_string(),
            size: item.size,
            mtime: item.mtime,
        });
        hits.len() < max
    };

    'scan: for (lo, hi) in ranges {
        for item in &index.items()[lo..hi] {
            if !push(index, item, &mut hits) {
                truncated = true;
                break 'scan;
            }
        }
    }

    if !loose.is_empty() && !truncated {
        for item in index.items() {
            let path = index.path(item);
            let hit_prefix = loose.iter().any(|p| {
                path.get(..p.len())
                    .map_or(false, |head| head.eq_ignore_ascii_case(p))
            });
            if hit_prefix && !push(index, item, &mut hits) {
                truncated = true;
                break;
            }
        }
    }

    let age_ms = now_unix().saturating_sub(index.built_unix) as u128 * 1000;
    Some((hits, index.items().len() as u64, age_ms, truncated, index.complete))
}

fn match_item(
    index: &Index,
    item: &Item,
    re: &Regex,
    exts: &HashSet<String>,
    hits: &mut Vec<Hit>,
    match_path: bool,
) -> Option<()> {
    let name = index.name(item);
    if !exts.is_empty() && !ext_matches(name, exts) {
        return None;
    }
    let target = if match_path { index.path(item) } else { name };
    if !re.is_match(target) {
        return None;
    }
    hits.push(Hit { path: index.path(item).to_string(), size: item.size, mtime: item.mtime });
    Some(())
}

/// 这个正则是不是「安全纯字面量」：非空、只由字面量字符组成（允许 `\.` 这类
/// 转义元字符）、不含换行。
///
/// 只有这种模式才能把多条文件名用 `\n` 拼接后连续扫描而不改变语义：
/// - 无「活的」元字符 ⇒ 匹配的就是这段字节本身（`re` 原样使用，忽略大小写设置也保留）；
/// - 不含 `\n` ⇒ 命中不可能跨越条目分隔符；
/// - 非空 ⇒ 不会产生零宽命中。
///
/// **转义元字面量仍然算字面量**：GUI 文件名「纯文本」模式与 MCP 非正则模式都先
/// `regex::escape`，`main.rs` 会变成 `main\.rs`——它匹配的依旧是一段固定字节，
/// 拼起来扫不会跨条目，放行才能让「带点的文件名」也吃到快路径。
/// 其余模式（锚点、`\s`/`\d` 等字符类、字符类、内联 flag、反引用…）一律保留逐条匹配。
fn safe_literal(re: &Regex) -> bool {
    let s = re.as_str();
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            // 分隔符本身：命中会跨条目
            '\n' | '\r' => return false,
            '\\' => match chars.next() {
                // 只有「转义标点」是字面量；\s \d \w \b \n \x41 \u{..} 全是字母数字，一律回退
                Some(n) if !n.is_ascii_alphanumeric() => {}
                _ => return false,
            },
            '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}' => {
                return false
            }
            _ => {}
        }
    }
    true
}

/// chunk 级连续名字扫描（worker 复用 `buf` / `map`，不跨查询持有）。
///
/// 把通过扩展名筛选的文件名按条目顺序用 `\n` 拼接，用同一个正则连续扫；
/// 命中后直接跳到该条目末尾——每个文件最多一个 Hit，也不会为一个文件里的
/// 多次命中反复枚举。返回 true 表示本 worker 已达 `max`。
///
/// 调用前必须确认 `safe_literal(re)` 且 `match_path == false`。
fn scan_literal_chunk(
    index: &Index,
    items: &[Item],
    re: &Regex,
    exts: &HashSet<String>,
    max: usize,
    hits: &mut Vec<Hit>,
    buf: &mut String,
    map: &mut Vec<(usize, u32)>,
) -> bool {
    // 小批拼接：64K 条目整块拼一次要写 1.6MB，早命中（比如搜 "a"）也要先付完
    // 这笔钱；1024 条一批约 25KB，缓冲留在 L1/L2 里，命中上限就在批内立即返回。
    // 批越小早停越及时，但整盘扫描的批数（7.6M/1024≈7.6K 次）摊到 16 个 worker
    // 每人也只有几百次调用，正则调用开销可以忽略。
    const BATCH: usize = 1024;
    for (base, batch) in items.chunks(BATCH).enumerate() {
        buf.clear();
        map.clear();
        for (i, item) in batch.iter().enumerate() {
            let name = index.name(item);
            if !exts.is_empty() && !ext_matches(name, exts) {
                continue;
            }
            buf.push_str(name);
            buf.push('\n');
            // 记录「条目结束（含分隔符）」的累计偏移；条目 i 的名字在 [上一个 end, end-1)
            map.push((buf.len(), (base * BATCH + i) as u32));
        }
        if map.is_empty() {
            continue;
        }

        let mut pos = 0usize;
        while pos < buf.len() {
            let Some(m) = re.find_at(buf.as_str(), pos) else { break };
            // 命中起点落在哪个条目：第一个 end > m.start() 的条目。
            // 纯字面量不含换行，命中不可能跨分隔符，所以一定找得到。
            let idx = map.partition_point(|&(end, _)| end <= m.start());
            let Some(&(end, item_idx)) = map.get(idx) else { break };
            let item = &items[item_idx as usize];
            hits.push(Hit {
                path: index.path(item).to_string(),
                size: item.size,
                mtime: item.mtime,
            });
            if hits.len() >= max {
                return true;
            }
            // 同一个文件只算一次：跳到下一条目起点，跳过本名字剩余命中
            pos = end;
        }
    }
    false
}

/// `C:/Users` → `C:/Users/`，并把盘符统一成大写（索引里的写法）。
fn norm_prefix(root: &str) -> String {
    let mut p = root.replace('\\', "/");
    if !p.ends_with('/') {
        p.push('/');
    }
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let head = p[..1].to_ascii_uppercase();
        p.replace_range(..1, &head);
    }
    p
}

/// 排序后所有以 `prefix` 开头的条目是连续区间 `[lo, hi)`。
/// 上界用「前缀末字节 +1」定位（前缀必然以 `/` 结尾，`/`+1 = `0`），不用逐条判前缀。
fn prefix_bounds(index: &Index, prefix: &str) -> (usize, usize) {
    prefix_bounds_raw(&index.blob(), &index.items(), prefix)
}

/// 同上，但直接接旧条目表：增量刷新会把 items 先 take 出去。
fn prefix_bounds_raw(blob: &str, items: &[Item], prefix: &str) -> (usize, usize) {
    let lo = items.partition_point(|it| path_of(blob, it) < prefix);
    let mut upper = prefix.as_bytes().to_vec();
    if let Some(last) = upper.last_mut() {
        *last = last.wrapping_add(1);
    }
    let hi = match std::str::from_utf8(&upper) {
        Ok(u) => items.partition_point(|it| path_of(blob, it) < u),
        Err(_) => lo,
    };
    (lo, hi.max(lo))
}

fn dir_prefix_bounds_raw(dir_blob: &str, dirs: &[DirRec], prefix: &str) -> (usize, usize) {
    let lo = dirs.partition_point(|rec| dir_path_of(dir_blob, rec) < prefix);
    let mut upper = prefix.as_bytes().to_vec();
    if let Some(last) = upper.last_mut() {
        *last = last.wrapping_add(1);
    }
    let hi = match std::str::from_utf8(&upper) {
        Ok(u) => dirs.partition_point(|rec| dir_path_of(dir_blob, rec) < u),
        Err(_) => lo,
    };
    (lo, hi.max(lo))
}

/// 索引是「建好那一刻的快照」，之后新建/改名的文件不在里面。
/// 完整索引却零命中，且索引不是刚刚建好的秒级缓存时，不能直接下「文件不存在」的结论，
/// 调用方应回退真实遍历兜底（否则新建文件会静默搜不到）。
pub fn suspicious_miss(hits: usize, age_ms: u128, complete: bool) -> bool {
    const MIN_AGE_MS: u128 = 3_000;
    /// 索引超过这个年龄还没被刷新掉，说明刷新本身没成功（盘掉线/权限/抢锁）：
    /// 此时即便有命中也不能保证是全量，得回退真实遍历。
    const REFRESH_STALE_MS: u128 = 60_000;
    if !complete {
        return false;
    }
    // 只读读者：快照可信与否**不看文件年龄**，看写者是否还在维护。
    // 写者只在内容真变时才落盘，所以「文件一小时没变」既可能是没变化、
    // 也可能是没落盘——时间戳区分不了。写者活着（维护循环在跳心跳）就说明
    // 它已经把该发布的都发布了；写者一停，reader_mode() 立刻变 false，
    // 本进程会转成写者自己刷，不再走这里。
    if reader_mode() {
        return false;
    }
    // 监听在跑：索引就是当前文件系统的权威视图，零命中/有命中都直接信它。
    // 否则「查一个刚删掉的文件」会先等 20-48s 的全盘遍历才回「没有」。
    if watcher_trusted() {
        return age_ms >= REFRESH_STALE_MS;
    }
    age_ms >= MIN_AGE_MS && (hits == 0 || age_ms >= REFRESH_STALE_MS)
}

/// 结果仍可用，但索引可能不含「建立之后新建/改名」的文件。
/// 仅用于结果标注，不触发兜底遍历（命中非零时兜底会把每次搜索拖慢到全盘级）。
pub fn maybe_stale(age_ms: u128) -> bool {
    age_ms >= 300_000
}

/// 本次运行是否已经用 USN 增量把索引刷新到「当前」。
/// GUI 状态栏用它显示「增量刷新已生效 / 未生效（需要管理员）」。
pub fn usn_trusted() -> bool {
    USN_TRUSTED.load(Ordering::Relaxed) && !NEED_FULL_SCAN.load(Ordering::Relaxed)
}

/// `idle` / `building` / `ready`
pub fn status() -> &'static str {
    if is_building() {
        return "building";
    }
    if lock().is_some() {
        "ready"
    } else {
        "idle"
    }
}

/// 索引已在内存里。
pub fn is_ready() -> bool {
    lock().is_some()
}

/// 后台是否正在建索引。构建超时（疑似卡死）时复位标志并返回 false，
/// 让调用方还能重新发起构建。
pub fn is_building() -> bool {
    if !BUILDING.load(Ordering::Relaxed) {
        return false;
    }
    let started = BUILD_STARTED.load(Ordering::Relaxed);
    if started > 0 && now_unix().saturating_sub(started) > BUILD_STUCK_SECS {
        reset_stuck_build(started);
        return false;
    }
    true
}

/// 同步载入磁盘缓存（不触发构建）。返回载入后是否有可用索引。
/// 已有索引直接 true；正在后台构建时返回 false，调用方照常遍历。
pub fn try_load() -> bool {
    if is_ready() {
        return true;
    }
    let Some(_slot) = BuildSlot::acquire() else {
        return false;
    };
    let loaded = load_cache();
    let ok = loaded.is_some();
    let built = loaded.as_ref().map_or(0, |ix| ix.built_unix);
    {
        let mut guard = lock();
        if let Some(index) = loaded {
            *guard = Some(Box::new(index));
        }
    }
    if ok {
        adopt_cache(built);
    }
    ok
}

/// 载入磁盘缓存后：记下它对应哪份日志，并尝试回放日志补齐「进程没在跑的那段空窗」。
///
/// 回放成功 ⇒ 我们相信自己对文件系统的了解是完整的 ⇒ 不用再做全量 stat 巡
/// （316k 目录约 10-15s，这是新进程首次查询慢的唯一原因）。
fn adopt_cache(cache_unix: u64) {
    if cache_unix == 0 {
        return;
    }
    JOURNAL_CACHE_UNIX.store(cache_unix, Ordering::Relaxed);
    JOURNAL_READY.store(false, Ordering::Relaxed);
    if let Some(dirs) = journal_replay(cache_unix) {
        {
            let mut g = DIRTY_DIRS.lock().unwrap_or_else(|e| e.into_inner());
            g.extend(dirs);
        }
        NEED_FULL_SCAN.store(false, Ordering::Relaxed);
    }
}

/// 保证索引可用：已有就检查是否需要刷新（增量，不整盘重扫）；
/// 没有就尝试磁盘缓存；再没有就后台建。缓存载入是同步的（约 0.2s），因为调用方可能马上要查。
/// GUI 进程启动时调用一次。
pub fn set_gui_mode() {
    IS_GUI.store(true, Ordering::Relaxed);
}

/// 只读读者：GUI 在世（心跳新鲜）时，索引维护与落盘都由它负责，
/// 本进程只映射快照、按文件变化重映射——不监听、不刷新、不写盘。
pub fn reader_mode() -> bool {
    if IS_GUI.load(Ordering::Relaxed) {
        return false;
    }
    // 每次现判，不缓存结论：写者可能已经退出（GUI 关掉），
    // 读者必须能自己接管；写者回来后再退回去读。
    // gui_alive() 只是一次小文件读取，代价可忽略。
    let alive = integration::gui_alive();
    READER_MODE.store(alive, Ordering::Relaxed);
    alive
}

fn cache_stamp() -> Option<(u64, u64)> {
    let path = cache_path()?;
    let md = std::fs::metadata(&path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((md.len(), mtime))
}

/// 读者刷新：文件没变就什么都不做；变了就重新映射（约 30ms，零拷贝）。
fn ensure_mapped() {
    let stamp = cache_stamp();
    let same = {
        let g = MAPPED_STAMP.lock().unwrap_or_else(|e| e.into_inner());
        match (&*g, &stamp) {
            (Some(cur), Some(new)) => cur == new,
            _ => false,
        }
    };
    if same && is_ready() {
        return;
    }
    if let Some(ix) = load_cache() {
        let built = ix.built_unix;
        *lock() = Some(Box::new(ix));
        if let Ok(mut g) = MAPPED_STAMP.lock() {
            *g = stamp;
        }
        // 不 adopt_cache：读者不维护日志，也不做增量刷新
        let _ = built;
    }
}

pub fn ensure_ready() {
    cleanup_stale_tmp();
    if reader_mode() {
        ensure_mapped();
        return;
    }
    start_watchers();
    // USN 增量是毫秒级的：先把磁盘缓存载入内存，再把「缓存写成之后」的变更补上。
    // 不做这一步的话，新进程即便缓存只旧了几秒，NEED_FULL_SCAN 仍为 true，
    // 零命中就要走 35 秒的全盘兜底——那正是这条链路要消灭的慢路径。
    if NEED_FULL_SCAN.load(Ordering::Relaxed) && !is_ready() {
        let _ = try_load();
    }
    if NEED_FULL_SCAN.load(Ordering::Relaxed) {
        let drives = integration::local_drives();
        if crate::usn::can_attempt(&drives, &current_usn_state()) {
            let _ = refresh_now_mode(false);
        }
    }
    // 有管理员权限、但还没有可用的增量起点：后台建一次索引把它种下来
    // （带跨进程节流，多个 MCP 进程不会同时扫盘）。种好之后，
    // 以后每次启动都只补变化，不再全盘巡。
    if !SEED_REQUESTED.load(Ordering::Relaxed) {
        let drives = integration::local_drives();
        if crate::usn::needs_seed(&drives, &current_usn_state())
            && crate::usn::available(&drives)
        {
            SEED_REQUESTED.store(true, Ordering::Relaxed);
            rebuild_throttled();
        }
    }
    let age = current_age_secs();
    match age {
        Some(a) if a <= auto_refresh_secs() && !has_pending_changes() => {}
        Some(a) => {
            refresh_smart();
            // 很久没整盘重建过：顺手排一次后台全量重建（跨进程节流），
            // 用来清掉增量刷新可能漏掉的边角（mtime 撒谎 / 幽灵条目）。
            if a > TTL_SECS {
                rebuild_throttled();
            }
        }
        None => {
            if try_load() {
                refresh_smart();
            } else if let Some(slot) = BuildSlot::acquire() {
                spawn_build(slot);
            }
        }
    }
}

/// 同步刷新（只是应用监听到的变更时代价可忽略）；需要全量扫描时交给后台，
/// 本次先用当前快照作答——全量扫描约 10-20s，不能挂在搜索路径上。
///
/// 例外：索引已经旧到「零命中也要回退整盘遍历」的程度（> 60s）时，直接同步扫完更划算
/// （10-20s vs 遍历 20-50s），而且结果一定是当前真相，不会给假阴性。
fn refresh_smart() -> bool {
    if !NEED_FULL_SCAN.load(Ordering::Relaxed) {
        return refresh_now();
    }
    // USN 增量是毫秒级的：确认能走增量就同步做掉，别把便宜活甩到后台。
    // 只有 can_attempt 成立才同步——否则 refresh_now 会退化成 17-22s 的全盘巡。
    let drives = integration::local_drives();
    if crate::usn::can_attempt(&drives, &current_usn_state()) && refresh_now_mode(false) {
        return true;
    }
    let badly_stale = current_age_secs().map_or(true, |a| a > 60);
    if badly_stale {
        return refresh_now();
    }
    spawn_refresh_async();
    is_ready()
}

/// 确保索引比 `max_age_secs` 新：不够新就同步跑一次增量刷新。
/// 返回刷新后是否有可用索引（索引不存在时返回 false，调用方自己决定要不要遍历）。
pub fn ensure_fresh(max_age_secs: u64) -> bool {
    if reader_mode() {
        ensure_mapped();
        return is_ready();
    }
    start_watchers();
    match current_age_secs() {
        // 年龄够新、但监听还攒着没应用的变更（刚才建的文件就在里面）也得刷一下
        Some(a) if a <= max_age_secs && !has_pending_changes() => true,
        Some(_) => refresh_smart(),
        None => false,
    }
}

/// 上一次「先答后补」的后台刷新安排时间（秒）。
static LAST_QUICK_REQ: AtomicU64 = AtomicU64::new(0);

/// 当前索引自带的水位（盘符 → 上次覆盖到的 USN）。
fn usn_state(index: &Index) -> std::collections::HashMap<String, crate::usn::VolState> {
    index
        .usn
        .iter()
        .map(|c| {
            (
                crate::usn::key_of(c.drive),
                crate::usn::VolState { journal_id: c.journal_id, next_usn: c.next_usn },
            )
        })
        .collect()
}

/// 当前内存索引的水位（没有索引时为空）。
fn current_usn_state() -> std::collections::HashMap<String, crate::usn::VolState> {
    lock().as_ref().map(|ix| usn_state(ix)).unwrap_or_default()
}

/// 写者定时调用：内存索引比磁盘缓存新就落盘一次，让只读读者看到最新快照。
/// 没有变更时是 O(1)，不会每 30 秒白写 790MB。
///
/// 失败时**不动** `SAVED_GEN`：内容代数保持「已保存 < 当前」，
/// 下一轮维护会继续重试；否则读者会永远停在旧快照。
pub fn persist_if_dirty() {
    if SAVED_GEN.load(Ordering::Relaxed) == CONTENT_GEN.load(Ordering::Relaxed) {
        return;
    }
    if !publish_cache() && std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] persist_if_dirty: 缓存发布失败，保持脏，等待重试");
    }
}

/// 实时输入用：**绝不在搜索路径上刷新**。
///
/// 刷新要把 760 万条条目重拼一遍（100–300ms），挂在每次按键上就是
/// 「打字一顿一顿」的直接来源——而系统后台总在写文件，脏目录几乎从不清空。
/// 这里只用当前快照作答，另起后台线程把变更补上；显式搜索（回车）仍走同步刷新。
pub fn ensure_fresh_quick(max_age_secs: u64) -> bool {
    if reader_mode() {
        ensure_mapped();
        return is_ready();
    }
    start_watchers();
    if !is_ready() {
        return false;
    }
    let stale = current_age_secs().map_or(true, |a| a > max_age_secs);
    if stale || has_pending_changes() {
        schedule_refresh();
    }
    true
}

/// 安排一次后台增量刷新：只在进程内节流（1 秒一次），不阻塞调用方。
/// 用 `refresh_now_mode(false)`——绝不在后台顺手起全盘巡，那会长时间持锁。
pub fn schedule_refresh() {
    let now = now_unix();
    let last = LAST_QUICK_REQ.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 1 {
        return;
    }
    if LAST_QUICK_REQ
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        let _ = refresh_now_mode(false);
    });
}

/// 监听是否攒着还没应用的变更事件。
fn has_pending_changes() -> bool {
    DIRTY_DIRS
        .try_lock()
        .map(|g| !g.is_empty())
        .unwrap_or(false)
}

/// 内存索引的年龄（秒）；没有索引时 None。
pub fn current_age_secs() -> Option<u64> {
    let guard = lock();
    guard
        .as_ref()
        .map(|index| now_unix().saturating_sub(index.built_unix))
}

/// 索引快照的建立时间（unix 秒）；没有索引时 0。
pub fn built_unix() -> u64 {
    let guard = lock();
    guard.as_ref().map_or(0, |index| index.built_unix)
}

/// 上次增量刷新/构建时有目录读不到：该子树的文件可能滞后于磁盘。
pub fn incomplete_dirs() -> bool {
    let guard = lock();
    guard.as_ref().map_or(false, |index| index.incomplete_dirs)
}

/// 强制重建；重建期间旧索引仍可查询。已在建则忽略。
pub fn rebuild() {
    if let Some(slot) = BuildSlot::acquire() {
        spawn_build(slot);
    }
}

/// 索引需要刷新时调用：后台重建，但带节流，避免多个 MCP 进程 + GUI
/// 在同一时间各扫一遍整盘。
///
/// - 已有构建在跑：直接返回（不写节流标记，让下次还有机会）；
/// - 10 分钟内别的进程已请求过重建：返回（那个进程会写缓存，本次先继续用旧索引）。
fn rebuild_throttled() {
    const COOLDOWN_SECS: u64 = 600;
    if is_building() || cross_process_recent_refresh(COOLDOWN_SECS) {
        return;
    }
    let now = now_unix();
    let last = LAST_REFRESH_REQ.load(Ordering::Relaxed);
    if now.saturating_sub(last) < COOLDOWN_SECS {
        return;
    }
    if LAST_REFRESH_REQ
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        stamp_refresh_now();
        rebuild();
    }
}

/// 兜底遍历把整盘完整走完后调用：后台重建索引，下一次搜索就能直接命中，
/// 不用再为同一个「新文件」反复整盘扫描。
pub fn refresh_after_full_walk() {
    rebuild_throttled();
}

/// 跨进程共享的「上次兜底回填时间」，与 index-v1.bin 同目录。
fn refresh_stamp_path() -> Option<PathBuf> {
    cache_path().map(|p| p.with_file_name("refresh-stamp"))
}

fn cross_process_recent_refresh(cooldown_secs: u64) -> bool {
    let Some(path) = refresh_stamp_path() else { return false };
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let Ok(last) = text.trim().parse::<u64>() else { return false };
    now_unix().saturating_sub(last) < cooldown_secs
}

fn stamp_refresh_now() {
    let Some(path) = refresh_stamp_path() else { return };
    // 多进程并发读：先写临时文件再 rename，避免读到半截数字
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, now_unix().to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn spawn_build(slot: BuildSlot) {
    // slot 移进线程：构建结束或被 panic 展开时由 Drop 释放 BUILDING
    std::thread::spawn(move || {
        let _slot = slot;
        // 建索引期间照样在遍历全盘：先记下日志位置，建完提交，下次启动就能走增量
        let sweep = crate::usn::snapshot_before_sweep(&integration::local_drives());
        let mut built = build();
        if let Some(index) = built.as_mut() {
            index.usn = sweep;
        }
        if let Some(index) = built {
            let built_unix = index.built_unix;
            {
                let mut guard = lock();
                *guard = Some(Box::new(index));
                // 新内容已经进内存：代数必须前进，否则发布会被当成「没变化」跳过
                CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
            }
            // 锁外发布；失败也不洗白代数，周期维护会重试
            let _ = publish_cache();
            adopt_cache(built_unix);
        }
    });
}

/// 冷启动同步建索引（一趟遍历，阻塞当前线程），返回是否成功。
/// 用于首次整盘内容搜索：先把索引建好再走 [`candidates`]，避免后台建索引与兜底遍历并发抢 I/O。
pub fn build_sync() -> bool {
    let Some(_slot) = BuildSlot::acquire() else {
        return false;
    };
    let sweep = crate::usn::snapshot_before_sweep(&integration::local_drives());
    let outcome = build();
    let ok = outcome.is_some();
    if let Some(mut index) = outcome {
        index.usn = sweep;
        let built = index.built_unix;
        {
            let mut guard = lock();
            *guard = Some(Box::new(index));
            CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
        }
        let _ = publish_cache();
        adopt_cache(built);
    }
    ok
}

/// 冷启动一趟遍历：同时建索引 + 按文件名答查询。
/// 返回 `(命中, 索引条目数, 索引是否完整)`。
/// 只在整盘查询且索引缺失、无缓存、无后台在建时调用；失败返回 None，调用方照常退回遍历。
/// 不套 `budget`：这一趟本来就要走完全盘来建索引，提前停止收集命中只会白丢结果。
pub fn build_and_match(
    re: &Regex,
    exts: &HashSet<String>,
    max: usize,
    match_path: bool,
) -> Option<(Vec<Hit>, u64, bool)> {
    let _slot = BuildSlot::acquire()?;
    let sweep = crate::usn::snapshot_before_sweep(&integration::local_drives());
    let outcome = build_with_match(Some(re), Some(exts), max, match_path);
    match outcome {
        Some((mut index, hits)) => {
            let entries = index.items().len() as u64;
            let complete = index.complete;
            index.usn = sweep;
            let built = index.built_unix;
            {
                let mut guard = lock();
                *guard = Some(Box::new(index));
                CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
            }
            let _ = publish_cache();
            adopt_cache(built);
            Some((hits, entries, complete))
        }
        None => None,
    }
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

/// 记录索引被使用，并启动空闲回收线程（每个进程只启动一次）。
fn touch_index() {
    LAST_USE.store(now_unix(), Ordering::Relaxed);
    if REAPER.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(60));
        let last = LAST_USE.load(Ordering::Relaxed);
        if last == 0 || now_unix().saturating_sub(last) < idle_release_secs() {
            continue;
        }
        // 正在重建时别释放：构建完成后会写回新索引
        if is_building() {
            continue;
        }
        let mut guard = lock();
        if guard.is_some() {
            *guard = None;
            LAST_USE.store(now_unix(), Ordering::Relaxed);
        }
    });
}

// ---------- 构建 ----------

type RawItem = (Box<str>, u64, u64);

// ---------- 增量刷新 ----------

static REFRESHING: AtomicBool = AtomicBool::new(false);

/// 刷新权：同一进程只让一个线程做增量刷新，其余线程先用当前快照作答。
struct RefreshSlot;

impl RefreshSlot {
    fn acquire() -> Option<RefreshSlot> {
        REFRESHING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| RefreshSlot)
    }
}

impl Drop for RefreshSlot {
    fn drop(&mut self) {
        REFRESHING.store(false, Ordering::SeqCst);
    }
}

/// 同步做一次「目录级增量刷新」：只重新枚举 mtime 变了的子树。
/// 返回是否真的刷成了（索引不存在或有别的线程在刷时返回 false，调用方继续用旧快照）。
pub fn refresh_now() -> bool {
    refresh_now_mode(true)
}

fn refresh_now_mode(allow_full_sweep: bool) -> bool {
    let Some(_slot) = RefreshSlot::acquire() else { return false };
    let (ok, changed) = {
        let mut guard = lock();
        let Some(index) = guard.as_mut() else { return false };
        refresh_with_mode(index, allow_full_sweep)
    };
    // 790MB 序列化 + 写盘都放到索引锁外：否则写者自己的查询会被整份缓存写盘堵住。
    // 失败不推进 SAVED_GEN，下一轮维护会重试。
    if ok && changed && SAVED_GEN.load(Ordering::SeqCst) != CONTENT_GEN.load(Ordering::SeqCst) {
        let _ = publish_cache();
    }
    ok
}

/// 后台刷新：不阻塞查询。
///
/// 全量 stat 巡约 10s，如果持着索引锁跑，后面所有查询都会堵在那儿。
/// 所以扫描阶段只用「目录表快照」（20MB 量级），扫完再抢锁合并。
pub fn spawn_refresh_async() -> bool {
    let Some(slot) = RefreshSlot::acquire() else { return false };
    std::thread::spawn(move || {
        let _slot = slot;
        let snapshot = {
            let guard = lock();
            guard
                .as_ref()
                .map(|ix| (ix.dir_blob().to_string(), ix.dirs().to_vec()))
        };
        let Some((dir_blob, dirs)) = snapshot else { return };
        // 扫盘开始前记下 USN 位置：扫描成功后提交，下次启动就能走增量而不是再扫一遍
        let sweep = crate::usn::snapshot_before_sweep(&integration::local_drives());
        // 扫盘阶段：不持锁
        let (mut dirty, stat_failed) = scan_dirty_dirs(&dir_blob, &dirs, now_unix());
        drop(dir_blob);
        if !dirty.is_empty() {
            let keys: Vec<String> = dirty.iter().map(|(k, _)| k.clone()).collect();
            journal_append(JOURNAL_CACHE_UNIX.load(Ordering::Relaxed), &keys);
        }
        dirty.extend(take_dirty().into_iter().map(|k| (k, 0u64)));
        dirty.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut guard = lock();
        let Some(index) = guard.as_mut() else { return };
        if dirty.is_empty() {
            index.built_unix = now_unix();
            index.incomplete_dirs = stat_failed;
            NEED_FULL_SCAN.store(false, Ordering::Relaxed);
            // 这条路径不写缓存：水位不能单独推进（它会和旧缓存对不上），
            // 保持旧水位，下次从旧位置重放这批变更即可（幂等）。
            let _ = &sweep;
            return;
        }
        let hint: Vec<String> = dirty.into_iter().map(|(k, _)| k).collect();
        let mut changed = false;
        if refresh_in_place(index, Some(hint)) {
            NEED_FULL_SCAN.store(false, Ordering::Relaxed);
            CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
            changed = true;
            let _ = &sweep;
        }
        // 扫盘期间（约 10s）又攒下的变更，接着补上，最多 3 轮
        for _ in 0..3 {
            let more = take_dirty();
            if more.is_empty() {
                break;
            }
            if !refresh_in_place(index, Some(more)) {
                break;
            }
            // 每一轮都算内容变化：漏加代数会让读者停在更早的快照上
            CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
            changed = true;
        }
        drop(guard);
        // 序列化与写盘不持索引锁；失败保持脏，下一轮维护重试
        if changed {
            let _ = publish_cache();
        }
    });
    true
}

/// 监听在跑、也没丢过事件：这时索引可以当作「当前文件系统」的权威视图，
/// 零命中可以直接回「没有」，不必再花 20-48s 走一遍全盘遍历。
///
/// USN 增量成功提交后同样成立：那次刷新是把「上次快照到当前」的变更逐条应用掉的，
/// 结论与监听路径等价（之后的新变化由监听继续接住）。
fn watcher_trusted() -> bool {
    (WATCH_ACTIVE.load(Ordering::Relaxed) || USN_TRUSTED.load(Ordering::Relaxed))
        && !NEED_FULL_SCAN.load(Ordering::Relaxed)
}

/// 刷新策略：
/// - 监听在跑且没溢出 ⇒ 只重枚举「有变更的目录」（通常几个，耗时可忽略）
/// - 需要全盘巡时先试 USN 增量（读变更日志，只补关机期间变过的目录，毫秒级）
/// - USN 不可信 ⇒ 全量 stat 巡（约 17s，交给后台）
/// `allow_full_sweep=false`：只走「便宜的」路径（脏目录 / USN 增量），
/// 需要全盘巡时直接返回 false，交给调用方决定要不要甩到后台。
///
/// 返回 `(ok, changed)`：`changed` 表示内存内容/水位真的改过、需要发布到缓存。
fn refresh_with_mode(index: &mut Index, allow_full_sweep: bool) -> (bool, bool) {
    if integration::local_drives().len() as u64 != WATCHED_DRIVES.load(Ordering::Relaxed) {
        NEED_FULL_SCAN.store(true, Ordering::Relaxed);
    }
    // NEED_FULL_SCAN=false 表示「我们相信自己对变更的了解是完整的」——
    // 要么监听在跑，要么刚回放过变更日志。这时只应用脏目录，代价可忽略。
    if !NEED_FULL_SCAN.load(Ordering::Relaxed) {
        let dirty = take_dirty();
        if dirty.is_empty() {
            // 没有任何变更事件：索引就是最新的，只更新新鲜度戳（O(1)）
            index.built_unix = now_unix();
            return (true, false);
        }
        let had_changes = !dirty.is_empty();
        let ok = refresh_in_place(index, Some(dirty.clone()));
        if ok && had_changes {
            CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
        }
        if !ok {
            // 刷新失败不能当成成功：把脏目录放回队列，并强制下一轮全量扫描。
            // 否则这批变更永久缺失，而 built_unix 还会被推进到当前时间，
            // 「看起来更新鲜」的假象会让零命中直接返回「没有」。
            for d in dirty {
                note_dirty(&d);
            }
            NEED_FULL_SCAN.store(true, Ordering::Relaxed);
        }
        return (ok, ok && had_changes);
    }

    // ---- 全盘巡之前先试 USN 增量 ----
    // 读 NTFS 变更日志就知道「上次快照之后哪些目录变过」，只重枚举那几个目录即可，
    // 毫秒级；不需要 17-22 秒的全盘 stat 巡。任何一步不可信（没管理员权限 / 非 NTFS /
    // 日志重建 / 起点被截断 / 有条目解析不出路径）都返回 None，直接落回下面的全盘巡。
    let drives = integration::local_drives();
    let state = usn_state(index);
    let __t = Instant::now();
    let __usn = crate::usn::dirty_dirs_since_snapshot(&drives, &state);
    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!(
            "[debug] usn dirty={:?} in {:?}",
            __usn.as_ref().map(|(d, _)| d.len()),
            __t.elapsed()
        );
    }
    if let Some((dirty, commits)) = __usn {
        let __t2 = Instant::now();
        let __ok = refresh_in_place(index, Some(dirty.clone()));
        if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
            eprintln!("[debug] refresh_in_place {:?} ok={}", __t2.elapsed(), __ok);
        }
        if __ok {
            NEED_FULL_SCAN.store(false, Ordering::Relaxed);
            index.built_unix = now_unix();
            // 水位只在缓存真的写成功之后才推进，而且和缓存放在同一个文件里：
            // 内容与水位一起落盘，并发下也不会出现「旧内容 + 新水位」。
            // 发布交给调用方在锁外做；这里只把「内容变了」记进代数。
            let changed = !dirty.is_empty();
            if changed {
                index.usn = commits;
                CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
            }
            USN_TRUSTED.store(true, Ordering::Relaxed);
            return (true, changed);
        }
    }
    if !allow_full_sweep {
        return (false, false);
    }

    // 全盘巡开始前记下日志位置：巡成功提交后，下次启动就能走增量
    let sweep = crate::usn::snapshot_before_sweep(&drives);
    let ok = refresh_in_place(index, None);
    if ok {
        NEED_FULL_SCAN.store(false, Ordering::Relaxed);
        // 新鲜度记「巡完的时刻」而不是「开始巡的时刻」：本机巡一次约 17s，
        // 记开始时间会让刚落盘的文件在别人眼里已经旧了 17 秒，起不到作用。
        index.built_unix = now_unix();
        // 全盘巡的结果必须落盘，否则每个新进程（GUI + 每个 MCP 子进程）启动后
        // 都要各自再巡一遍 31.7 万目录（本机约 20s）。写盘后后来者加载到的
        // 缓存年龄接近 0，ensure_ready 会直接跳过巡。
        // 水位跟着缓存一起写：这就是「全盘巡完成」的坐标。
        index.usn = sweep;
        CONTENT_GEN.fetch_add(1, Ordering::SeqCst);
        return (true, true);
    }
    (false, false)
}

// ---------- 目录变更监听（ReadDirectoryChangesW） ----------
//
// 为什么必须事件驱动：本机 31.7 万个目录，单次 stat 约 30µs（NVMe 上也这个量级，
// 主要是文件系统过滤器成本），全量 stat 一轮 ≈ 10s。放在查询路径上不可接受，而
// 「新建文件要等 10s 才能搜到」在用户看来仍然是「搜不到」。
//
// 所以每个本地盘开一个监听线程（普通用户权限即可，不需要管理员）：系统把增删改名
// 事件直接推过来，我们只记「哪个目录变了」。查询前的刷新只重枚举这几个目录。
// 监听没起来（或缓冲区溢出丢事件）时退回全量 stat 巡，保证最终一致。

static DIRTY_DIRS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 变更日志对应的「哪一份磁盘缓存」（= 那份缓存的 built_unix）。
/// 日志记的是「这份缓存写就之后，哪些目录变过」，新进程载入同一份缓存时直接回放，
/// 免掉 31.7 万目录的全量 stat 巡（约 10-15s）。
static JOURNAL_CACHE_UNIX: AtomicU64 = AtomicU64::new(0);
/// 本进程已确认日志头部与当前缓存匹配（每个进程只校验一次）
static JOURNAL_READY: AtomicBool = AtomicBool::new(false);
/// 至少有一个盘的监听在跑
static WATCH_ACTIVE: AtomicBool = AtomicBool::new(false);
static WATCH_STARTED: AtomicBool = AtomicBool::new(false);
/// 启动后 / 监听溢出 / 盘数量变化：必须做一次全量扫描才敢说索引是全的
static NEED_FULL_SCAN: AtomicBool = AtomicBool::new(true);
/// 本进程已经成功用 USN 增量把「上次快照 → 现在」的变更全部应用过。
/// 与「监听在跑」等价：可以拿它当零命中的判断依据。
static USN_TRUSTED: AtomicBool = AtomicBool::new(false);
/// 本进程已经尝试过「种增量起点」，别每次查询都去读状态文件
static SEED_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 本进程是 GUI（天然写者）。
static IS_GUI: AtomicBool = AtomicBool::new(false);
/// 当前角色：true = 只读读者（GUI 在世，索引由它维护）。
static READER_MODE: AtomicBool = AtomicBool::new(false);
/// 当前映射对应的缓存文件戳（长度, mtime），用来判断「文件换了，要重映射」。
static MAPPED_STAMP: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// 内存索引内容改动代数 / 上次落盘时的代数。
/// 只读读者靠缓存文件看世界，写者必须把「内容变了」及时发布出去，
/// 否则读者会一直停在旧快照（实测遇到过 13 分钟没更新的情况）。
static CONTENT_GEN: AtomicU64 = AtomicU64::new(0);
static SAVED_GEN: AtomicU64 = AtomicU64::new(0);
/// 缓存发布串行锁：并发发布会让旧快照最后落盘、覆盖新快照。
static CACHE_PUBLISH: Mutex<()> = Mutex::new(());
/// 进程内缓存写序号：临时文件名不能只用 pid——同进程两次发布并发时
/// 会抢同一个临时文件。
static CACHE_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
static WATCHED_DRIVES: AtomicU64 = AtomicU64::new(0);

/// 启动目录监听（幂等）。GUI 与 MCP 都会调，重复调用无代价。
pub fn start_watchers() {
    if WATCH_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let drives = integration::local_drives();
    WATCHED_DRIVES.store(drives.len() as u64, Ordering::Relaxed);
    for d in drives {
        let root = format!("{d}/");
        std::thread::spawn(move || watch_volume(&root));
    }
}

/// 日志多久没被写过就不再信任：说明这段时间没有进程在看盘（空窗未知），
/// 必须退回全量 stat 巡。
const JOURNAL_SLACK_SECS: u64 = 120;
/// 日志体积上限：超了就丢掉重来（下次退回全量扫描），别无限长
const JOURNAL_MAX_BYTES: u64 = 8 * 1024 * 1024;
const JOURNAL_HEADER: &str = "#rust-seek-journal v1";

fn journal_path() -> Option<PathBuf> {
    cache_path().map(|p| p.with_file_name("journal-v1.txt"))
}

/// 把「这些目录变过」追加进变更日志。
///
/// 只追加、不重写（每个进程各自 append，整批一次 write），所以代价与变更量成正比、
/// 与索引规模无关。头部记着对应缓存的 built_unix：缓存重建后头部对不上，
/// 日志自动作废（`save_cache` 也会直接删掉它）。
fn journal_append(cache_unix: u64, dirs: &[String]) {
    if cache_unix == 0 || dirs.is_empty() {
        return;
    }
    let Some(path) = journal_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !JOURNAL_READY.load(Ordering::Relaxed) {
        // 首次写：确认头部匹配现有缓存，不匹配就重建
        let header_ok = std::fs::read(&path)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| t.lines().next().map(str::to_string))
            .map_or(false, |h| h == format!("{JOURNAL_HEADER} {cache_unix}"));
        if !header_ok {
            let mut head = format!("{JOURNAL_HEADER} {cache_unix}\n");
            for d in dirs {
                head.push_str(d);
                head.push('\n');
            }
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, head).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            JOURNAL_READY.store(true, Ordering::Relaxed);
            return;
        }
        JOURNAL_READY.store(true, Ordering::Relaxed);
    }
    if std::fs::metadata(&path).map_or(false, |m| m.len() > JOURNAL_MAX_BYTES) {
        // 太长就别再追加了：等下次缓存重建时它会被删掉
        NEED_FULL_SCAN.store(true, Ordering::Relaxed);
        return;
    }
    let mut batch = String::with_capacity(dirs.len() * 48);
    for d in dirs {
        batch.push_str(d);
        batch.push('\n');
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&path) {
        let _ = f.write_all(batch.as_bytes());
    }
}

/// 回放变更日志：返回「自这份缓存写就以来变过的目录」。
///
/// 只信任两种情况：头部匹配 + 日志最近还在被写（说明这段空窗有进程在看盘）。
/// 否则返回 None，调用方退回全量 stat 巡——宁可多花 10s，也不给假阴性。
fn journal_replay(cache_unix: u64) -> Option<Vec<String>> {
    let path = journal_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > JOURNAL_MAX_BYTES {
        return None;
    }
    let age = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .map(|t| now_unix().saturating_sub(t))
        .unwrap_or(u64::MAX);
    if age > JOURNAL_SLACK_SECS {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != format!("{JOURNAL_HEADER} {cache_unix}") {
        return None;
    }
    let mut dirs: Vec<String> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // 盘符根（"D:"）在日志里意味着「盘根下直接有增删」：回放它等于重扫整盘，
        // 还不如直接退回全量扫描（扫描本来就更省事，也不会漏）。
        if line.len() <= 2 && line.ends_with(':') {
            return None;
        }
        dirs.push(line.to_string());
    }
    dirs.sort_unstable();
    dirs.dedup();
    Some(dirs)
}

fn note_dirty(dir: &str) {
    let mut g = DIRTY_DIRS.lock().unwrap_or_else(|e| e.into_inner());
    // 上限保护：疯狂抖动时别把内存吃穿，超了就让下一次退化成全量扫描
    if g.len() >= 20_000 {
        NEED_FULL_SCAN.store(true, Ordering::Relaxed);
        g.clear();
        return;
    }
    g.push(dir.to_string());
}

fn take_dirty() -> Vec<String> {
    let mut g = DIRTY_DIRS.lock().unwrap_or_else(|e| e.into_inner());
    let mut v = std::mem::take(&mut *g);
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(windows)]
fn watch_volume(root: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let native = root.replace('/', "\\");
    let wide: Vec<u16> = std::ffi::OsStr::new(&native)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // 监听起不来：只能靠全量扫描，标记必须扫
        NEED_FULL_SCAN.store(true, Ordering::Relaxed);
        return;
    }
    WATCH_ACTIVE.store(true, Ordering::Relaxed);
    // 注意：这里**不**置 NEED_FULL_SCAN。
    // 「进程启动前的空窗」由变更日志回放负责；置 true 会让日志白回放、每次启动都全扫。
    // 初始值本来就是 true，只有日志回放成功才清掉。

    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let mut returned: u32 = 0;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                1, // 递归监听整盘
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if ok == 0 || returned == 0 {
            // 句柄失效 / 缓冲区溢出丢事件：标脏，退回全量扫描保最终一致
            NEED_FULL_SCAN.store(true, Ordering::Relaxed);
            if ok == 0 {
                // ERROR_NOTIFY_ENUM_DIR(1022) 只是缓冲区溢出丢了事件，监听本身还活着，
                // 继续 arm 即可；真正的句柄失效才退出，而且必须清掉 WATCH_ACTIVE——
                // 否则「监听已死」仍被当成可信，之后零命中会被当权威答案。
                let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                if err != 1022 {
                    WATCH_ACTIVE.store(false, Ordering::Relaxed);
                    break;
                }
            }
            continue;
        }
        collect_changes(root, &buf[..returned as usize]);
    }
    unsafe { CloseHandle(handle) };
}

#[cfg(windows)]
fn collect_changes(root: &str, buf: &[u8]) {
    use windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_INFORMATION;

    // FILE_ACTION_ADDED=1 / REMOVED=2 / MODIFIED=3 / RENAMED_OLD=4 / RENAMED_NEW=5
    const ACTIONS: [u32; 4] = [1, 2, 4, 5];
    // 一批事件的脏目录攒起来，末尾一次性追加进变更日志（一次 write，不是每事件一次）
    let mut batch: Vec<String> = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 12 > buf.len() {
            break;
        }
        let rec = unsafe { &*(buf.as_ptr().add(off) as *const FILE_NOTIFY_INFORMATION) };
        let chars = rec.FileNameLength as usize / 2;
        if chars > 0 && off + 12 + chars * 2 <= buf.len() {
            let units = unsafe { std::slice::from_raw_parts(rec.FileName.as_ptr(), chars) };
            let rel = String::from_utf16_lossy(units);
            // MODIFIED 只是内容变了，名字集合不受影响，不用刷索引
            if ACTIONS.contains(&rec.Action) {
                let full = format!("{}{}", root, rel.replace('\\', "/"));
                if let Some(i) = full.rfind('/') {
                    let raw = &full[..i];
                    // 盘根：`D:/foo.txt` 取出来是裸 `D:`。既不是绝对路径（枚举会拼成
                    // <cwd>/D: 得到 0 条），却又能按 `D:/` 前缀把整盘旧条目划进删除集，
                    // 结果就是「往盘根拖一个文件 → 整个 D 盘从索引里消失」。统一成 `D:/`。
                    let dir = if raw.len() == 2 && raw.as_bytes()[1] == b':' {
                        format!("{raw}/")
                    } else {
                        raw.to_string()
                    };
                    note_dirty(&dir);
                    batch.push(dir);
                }
            }
        }
        if rec.NextEntryOffset == 0 {
            break;
        }
        off += rec.NextEntryOffset as usize;
    }
    if !batch.is_empty() {
        journal_append(JOURNAL_CACHE_UNIX.load(Ordering::Relaxed), &batch);
    }
}

#[cfg(not(windows))]
fn watch_volume(_root: &str) {}

/// 目录级增量刷新（原地、不拷整份 blob）。
///
/// 核心假设：目录自身的 LastWriteTime 变了，才可能有增删改名；没变就能把整棵子树
/// 的旧条目原样搬过来，不用枚举、不用给每个文件 stat。全盘 620 万文件 / 30 万目录，
/// 脏目录通常几十个，所以整次刷新通常在几十毫秒量级。
///
/// 返回 false 表示这次刷新不可信（旧内容原样恢复，调用方继续用旧快照）。
fn refresh_in_place(index: &mut Index, hint: Option<Vec<String>>) -> bool {
    let t_start = Instant::now();
    let started = now_unix();
    let roots: Vec<PathBuf> = integration::local_drives()
        .into_iter()
        .map(|d| PathBuf::from(format!("{d}/")))
        .collect();
    if roots.is_empty() {
        return false;
    }

    let old_items = std::mem::take(index.items_mut());
    let old_dirs = std::mem::take(index.dirs_mut());
    let was_complete = index.complete;

    // ---- 第 1 步：找出「哪些目录变了」 ----
    //
    // 两种来源：
    // - hint=Some：监听线程（ReadDirectoryChangesW）直接推过来的脏目录 —— 常态，代价可忽略
    // - hint=None：全量 stat 巡 —— 只在首次启动/监听丢事件时做，本机 31.7 万目录约 10-15s
    //
    // 为什么不能只靠「父目录 mtime 没变 ⇒ 子树没变」：改动落在深层目录时，只有它自己
    // 的 mtime 会变，祖先一个都不动。按祖先剪枝会把整棵 `D:/auto` 判成没变，于是深层
    // 新建/删除的文件永远刷不进来（这正是「刚建的文件搜不到」的成因之一）。
    // stat 一个目录 = 1 次系统调用，枚举一个目录 = 1 + N 次，所以全量巡也用 stat 而不是遍历。
    // hint=Some 时是「监听推过来的脏目录」，不需要全量 stat 巡。
    // 落在剪枝路径里的（node_modules/target/回收站…）直接丢掉：索引本来就不收这些，
    // 把它们当根会把目录表污染成「遍历和索引结果集不一致」。
    let (mut dirty, stat_failed) = match hint {
        Some(dirs) => (
            dirs.into_iter()
                .filter(|d| !is_pruned(Path::new(d)))
                .map(|d| (d, 0u64))
                .collect(),
            false,
        ),
        None => scan_dirty_dirs(&index.dir_blob(), &old_dirs, started),
    };
    let scan_ms = t_start.elapsed().as_millis();
    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] refresh_in_place dirty={} scan_ms={}", dirty.len(), scan_ms);
    }
    dirty.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // 把「这些目录变过」记进变更日志：下一个进程载入同一份缓存时直接回放，
    // 就不用再花 10-15s 做全量 stat 巡。监听路径也会写，重复无妨（回放时会去重）。
    if !dirty.is_empty() {
        let keys: Vec<String> = dirty.iter().map(|(k, _)| k.clone()).collect();
        journal_append(index.built_unix, &keys);
    }

    // 一个脏目录都没有：文件系统没动过，刷新只是一次 stat 巡。
    if dirty.is_empty() {
        *index.items_mut() = old_items;
        *index.dirs_mut() = old_dirs;
        index.built_unix = started;
        index.incomplete_dirs = stat_failed;
        return true;
    }

    // 枚举只从「最上层」的脏目录开始：脏目录的脏后代会被这趟遍历顺着走下去，
    // 重复当根会把同一棵子树枚举两遍、产出重复条目。
    let dirty_all: HashSet<&str> = dirty.iter().map(|(k, _)| k.as_str()).collect();
    let mut worklist: Vec<String> = Vec::with_capacity(dirty.len());
    for (key, _) in &dirty {
        let mut ancestor_dirty = false;
        let mut probe = key.as_str();
        while let Some(pos) = probe.rfind('/') {
            probe = &probe[..pos];
            // 先判是不是脏祖先，再看要不要停：盘符根（"D:"）本身也可能是脏的
            // （文件直接建在盘根下时就是这个键）；先 break 会把 D: 和 D:/x 都当根，
            // 同一棵树枚举两遍 ⇒ 索引里出现重复条目。
            if dirty_all.contains(probe) {
                ancestor_dirty = true;
                break;
            }
            if probe.len() <= 2 {
                break;
            }
        }
        if !ancestor_dirty {
            worklist.push(key.clone());
        }
    }

    // ---- 第 2 步：只枚举脏目录及其新增子树 ----
    let fresh_files: Mutex<Vec<RawItem>> = Mutex::new(Vec::new());
    let fresh_dirs: Mutex<Vec<(Box<str>, u64)>> = Mutex::new(Vec::new());
    let enumerated: Mutex<Vec<Box<str>>> = Mutex::new(Vec::new());
    let cap = max_entries();
    let count = std::sync::atomic::AtomicUsize::new(0);
    let overflow = AtomicBool::new(false);
    let cancel = AtomicBool::new(false);

    // 脏目录自己也要更新记录（它的 mtime 变了），而且它是被枚举的根。
    for key in &worklist {
        // 根源目录的 mtime 只能现场取一次（遍历不会回调根自己）；根的数量很少
        let mtime = dir_mtime_secs(key).unwrap_or(0);
        fresh_dirs.lock().unwrap().push((key.as_str().into(), mtime));
        enumerated.lock().unwrap().push(key.as_str().into());
    }

    {
        let dir_blob_snapshot = index.dir_blob();
        let walk_roots: Vec<PathBuf> = worklist.iter().map(PathBuf::from).collect();
        fastwalk::walk(
            &walk_roots,
            &fastwalk::Options { include_hidden: false, threads: num_cpus::get() },
            &cancel,
            |path, info| {
                if is_pruned(path) {
                    return if info.is_dir { Step::Skip } else { Step::Continue };
                }
                if info.is_dir {
                    let key = path.to_string_lossy().replace('\\', "/");
                    let mtime = unix_secs(info.modified);
                    // 索引里没有这个目录（新建的）也必须枚举，否则它的子树文件全丢
                    let known = old_dirs
                        .binary_search_by(|rec| {
                            dir_path_of(dir_blob_snapshot, rec).cmp(key.as_str())
                        })
                        .is_ok();
                    let is_dirty = dirty_all.contains(key.as_str()) || !known;
                    fresh_dirs.lock().unwrap().push((key.as_str().into(), mtime));
                    if is_dirty {
                        // 脏：枚举它，并且要往下走
                        enumerated.lock().unwrap().push(key.into_boxed_str());
                        return Step::Continue;
                    }
                    // 不脏：子树没变，整棵复用（记录已登记，挂在被枚举的父目录下）
                    return Step::Skip;
                }
                if count.fetch_add(1, Ordering::Relaxed) >= cap {
                    overflow.store(true, Ordering::Relaxed);
                    return Step::Quit;
                }
                fresh_files.lock().unwrap().push((
                    path.to_string_lossy().replace('\\', "/").into_boxed_str(),
                    info.size,
                    unix_secs(info.modified),
                ));
                Step::Continue
            },
        );
    }

    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] worklist={:?} walk={:?}ms", worklist, t_start.elapsed().as_millis());
    }
    let mut fresh = fresh_files.into_inner().unwrap_or_default();
    fresh.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // 兜底去重：万一某棵子树被枚举了两次（重命名的两个事件、重叠的工作根），
    // 也不能把同一路径插进索引两遍——条目表必须严格升序。
    fresh.dedup_by(|a, b| a.0 == b.0);
    let mut fresh_d = fresh_dirs.into_inner().unwrap_or_default();
    fresh_d.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    fresh_d.dedup_by(|a, b| a.0 == b.0);
    let mut enumer = enumerated.into_inner().unwrap_or_default();
    enumer.sort_unstable();
    enumer.dedup();

    // ---- 第 3 步：算要丢掉的旧条目 ----
    //
    // 规则：**被枚举过的目录，其整棵子树里的旧条目一律作废**（用这次枚举的结果替换），
    // 唯一例外是「不脏、被跳过」的子目录——它的子树整棵复用。
    //
    // 不能只丢「直接子项」：字典序下直接子项和后代是交错的
    // （`docs/sub/b.html` 排在 `docs/z.html` 前面），按连续区间取「直接子项」会漏掉
    // 一部分，漏掉的又被新枚举插一遍 ⇒ 索引里出现重复路径，二分区间查询跟着错位。
    let mut enumer_sub: Vec<(usize, usize)> = Vec::new();
    let mut enumer_dir_sub: Vec<(usize, usize)> = Vec::new();
    for dir in &enumer {
        let prefix = format!("{dir}/");
        let (lo, hi) = prefix_bounds_raw(&index.blob(), &old_items, &prefix);
        if hi > lo {
            enumer_sub.push((lo, hi));
        }
        let (dlo, dhi) = dir_prefix_bounds_raw(&index.dir_blob(), &old_dirs, &prefix);
        if dhi > dlo {
            enumer_dir_sub.push((dlo, dhi));
        }
    }
    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] ranges {:?}ms fresh={} enumer={}", t_start.elapsed().as_millis(), fresh.len(), enumer.len());
    }
    let enumer_set: HashSet<&str> = enumer.iter().map(|k| k.as_ref()).collect();
    let mut keep_sub: Vec<(usize, usize)> = Vec::new();
    let mut keep_dir_sub: Vec<(usize, usize)> = Vec::new();
    for (key, _) in &fresh_d {
        if enumer_set.contains(key.as_ref()) {
            continue; // 被枚举的目录不算「没变」，不豁免
        }
        let prefix = format!("{key}/");
        let (lo, hi) = prefix_bounds_raw(&index.blob(), &old_items, &prefix);
        if hi > lo {
            keep_sub.push((lo, hi));
        }
        let (dlo, dhi) = dir_prefix_bounds_raw(&index.dir_blob(), &old_dirs, &prefix);
        if dhi > dlo {
            keep_dir_sub.push((dlo, dhi));
        }
    }

    let t_ranges = t_start.elapsed().as_millis();
    let mut item_drop = subtract(&merge_ranges(enumer_sub), &merge_ranges(keep_sub));
    let mut dir_drop = subtract(&merge_ranges(enumer_dir_sub), &merge_ranges(keep_dir_sub));

    // 脏目录自己的旧记录（挂在没被枚举的父目录下，不在上面任何子树里）换成新 mtime
    for key in &worklist {
        if let Ok(i) =
            old_dirs.binary_search_by(|rec| dir_path_of(&index.dir_blob(), rec).cmp(key.as_str()))
        {
            dir_drop.push((i, i + 1));
        }
    }

    item_drop.sort_unstable();
    dir_drop.sort_unstable();

    // ---- 第 4 步：拼新表 ----
    let kept_len = old_items.len() - covered(&item_drop);
    let kept_dirs_len = old_dirs.len() - covered(&dir_drop);

    // 丢太多 ⇒ 枚举出了系统性问题（盘没就绪 / 权限 / 拔盘），不信这次结果。
    if old_items.len() >= 10_000 && kept_len * 5 < old_items.len() * 4 {
        eprintln!(
            "rust-seek: incremental refresh would drop {} of {} entries; keeping previous index",
            old_items.len() - kept_len,
            old_items.len()
        );
        *index.items_mut() = old_items;
        *index.dirs_mut() = old_dirs;
        return false;
    }

    let t_drop = t_start.elapsed().as_millis();
    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] drop_plan {:?}ms kept={} of {}", t_drop, kept_len, old_items.len());
    }
    if fresh.is_empty() && kept_len == old_items.len() {
        *index.items_mut() = old_items;
    } else {
        let mut kept: Vec<Item> = Vec::with_capacity(kept_len);
        copy_gaps(&old_items, &item_drop, &mut kept);
        drop(old_items);

        // 新路径追加到 blob 尾部：旧偏移全部保留，不重拷 580MB 文本。
        let mut appended: Vec<Item> = Vec::with_capacity(fresh.len());
        for (path, size, mtime) in &fresh {
            if index.blob().len() + path.len() > u32::MAX as usize {
                break;
            }
            let path_off = index.blob().len() as u32;
            index.blob_mut().push_str(path);
            let path_len = path.len() as u32;
            let rel = path.rfind('/').map_or(0, |i| i + 1) as u32;
            appended.push(Item {
                path_off,
                path_len,
                name_off: path_off + rel,
                name_len: path_len - rel,
                size: *size,
                mtime: *mtime,
            });
        }

        // 归并：新条目相对全盘很少，用二分定位插入点再一次拼好，
        // 避免走 620 万条路径的全量字符串归并（那要 0.5s+）。
        let dbg = std::env::var_os("RUST_SEEK_DEBUG_REFRESH").is_some();
        let blob = index.blob();
        if dbg {
            let mut bad = 0usize;
            for w in kept.windows(2) {
                if path_of(blob, &w[0]) >= path_of(blob, &w[1]) {
                    bad += 1;
                    if bad <= 3 {
                        eprintln!(
                            "  kept-bad: {:?} -> {:?}",
                            path_of(blob, &w[0]),
                            path_of(blob, &w[1])
                        );
                    }
                }
            }
            let mut abad = 0usize;
            for w in appended.windows(2) {
                if path_of(blob, &w[0]) >= path_of(blob, &w[1]) {
                    abad += 1;
                }
            }
            eprintln!(
                "  pre-merge: kept={} kept_bad={} appended={} appended_bad={}",
                kept.len(),
                bad,
                appended.len(),
                abad
            );
        }
        let mut merges: Vec<(usize, usize)> = Vec::with_capacity(appended.len());
        for (i, item) in appended.iter().enumerate() {
            let target = path_of(blob, item);
            let mut pos = kept.partition_point(|it| path_of(blob, it) < target);
            // 同一路径已存在（例如上一轮刷新把条目撞重了）：用新条目顶掉，不再插入，
            // 否则条目表里会出现重复路径，二分区间查询会连带出错。
            if pos < kept.len() && path_of(blob, &kept[pos]) == target {
                kept.remove(pos);
                pos = kept.partition_point(|it| path_of(blob, it) < target);
            }
            merges.push((pos, i));
        }
        let mut merged: Vec<Item> = Vec::with_capacity(kept.len() + appended.len());
        let mut prev = 0usize;
        for (pos, idx) in merges {
            let pos = pos.max(prev);
            merged.extend_from_slice(&kept[prev..pos]);
            merged.push(appended[idx]);
            prev = pos;
        }
        merged.extend_from_slice(&kept[prev..]);
        *index.items_mut() = merged;
    }

    if fresh_d.is_empty() && kept_dirs_len == old_dirs.len() {
        *index.dirs_mut() = old_dirs;
    } else {
        let mut kept_dirs: Vec<DirRec> = Vec::with_capacity(kept_dirs_len);
        copy_gaps(&old_dirs, &dir_drop, &mut kept_dirs);
        drop(old_dirs);

        let mut appended_d: Vec<DirRec> = Vec::with_capacity(fresh_d.len());
        for (path, mtime) in &fresh_d {
            if index.dir_blob().len() + path.len() > u32::MAX as usize {
                break;
            }
            let path_off = index.dir_blob().len() as u32;
            index.dir_blob_mut().push_str(path);
            appended_d.push(DirRec { path_off, path_len: path.len() as u32, mtime: *mtime });
        }

        let dir_blob = index.dir_blob();
        let mut merges: Vec<(usize, usize)> = Vec::with_capacity(appended_d.len());
        for (i, rec) in appended_d.iter().enumerate() {
            let target = dir_path_of(dir_blob, rec);
            let mut pos = kept_dirs.partition_point(|r| dir_path_of(dir_blob, r) < target);
            if pos < kept_dirs.len() && dir_path_of(dir_blob, &kept_dirs[pos]) == target {
                kept_dirs.remove(pos);
                pos = kept_dirs.partition_point(|r| dir_path_of(dir_blob, r) < target);
            }
            merges.push((pos, i));
        }
        let mut merged: Vec<DirRec> = Vec::with_capacity(kept_dirs.len() + appended_d.len());
        let mut prev = 0usize;
        for (pos, idx) in merges {
            let pos = pos.max(prev);
            merged.extend_from_slice(&kept_dirs[prev..pos]);
            merged.push(appended_d[idx]);
            prev = pos;
        }
        merged.extend_from_slice(&kept_dirs[prev..]);
        *index.dirs_mut() = merged;
    }

    let t_merge = t_start.elapsed().as_millis();
    if std::env::var_os("RUST_SEEK_DEBUG_REFRESH").is_some() {
        // 结构自检：条目表必须严格升序（等价于无重复）——二分区间查询全靠它
        let mut bad = 0usize;
        for w in index.items().windows(2) {
            if path_of(&index.blob(), &w[0]) >= path_of(&index.blob(), &w[1]) {
                bad += 1;
                if bad <= 3 {
                    eprintln!(
                        "rust-seek BAD ORDER: {:?} -> {:?}",
                        path_of(&index.blob(), &w[0]),
                        path_of(&index.blob(), &w[1])
                    );
                }
            }
        }
        let mut dbad = 0usize;
        for w in index.dirs().windows(2) {
            if dir_path_of(&index.dir_blob(), &w[0]) >= dir_path_of(&index.dir_blob(), &w[1]) {
                dbad += 1;
            }
        }
        eprintln!(
            "rust-seek selfcheck: item_order_violations={} dir_order_violations={}", bad, dbad
        );
        eprintln!(
            "rust-seek refresh: dirty={} roots={} enumerated={} fresh_files={} kept={}/{} dirs_kept={}/{} scan={}ms ranges={}ms drop={}ms merge={}ms total={}ms stat_failed={}",
            dirty.len(),
            worklist.len(),
            enumer.len(),
            fresh.len(),
            index.items().len(),
            kept_len,
            index.dirs().len(),
            kept_dirs_len,
            scan_ms,
            t_ranges,
            t_drop,
            t_merge,
            t_start.elapsed().as_millis(),
            stat_failed
        );
    }
    index.built_unix = started;
    index.complete = was_complete && !overflow.load(Ordering::Relaxed);
    index.incomplete_dirs = stat_failed;
    // 刷新只往 blob 尾部追加，被删掉的路径会留下空洞；累计浪费超过 25% 才整理一次
    repack(index);
    true
}

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;

/// 取目录自身的 LastWriteTime（unix 秒）。
///
/// 不用 `std::fs::metadata`：Windows 上它要 CreateFile 拿到句柄再查信息，
/// 单次约 90µs；`GetFileAttributesExW` 只查元数据，快一个数量级。
/// 30 万个目录下这就是 29s 和 0.3s 的区别。
#[cfg(windows)]
fn dir_mtime_secs(path: &str) -> Result<u64, u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };

    let native = path.replace('/', "\\");
    let wide: Vec<u16> = std::ffi::OsStr::new(&native)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide.as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut WIN32_FILE_ATTRIBUTE_DATA as *mut core::ffi::c_void,
        )
    };
    if ok == 0 {
        return Err(unsafe { windows_sys::Win32::Foundation::GetLastError() });
    }
    let ticks = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | data.ftLastWriteTime.dwLowDateTime as u64;
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let secs = ticks / 10_000_000;
    Ok(secs.saturating_sub(EPOCH_DIFF_SECS))
}

#[cfg(not(windows))]
fn dir_mtime_secs(path: &str) -> Result<u64, u32> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
        .map_err(|_| 0)
}

/// 并行 stat 所有已知目录，返回 `(脏目录 [(路径, 新 mtime)], 是否有目录 stat 失败)`。
///
/// 「脏」= mtime 变了，或 mtime 离这次刷新不足 `DIR_SLACK_SECS`（秒级 mtime 下
/// 「同一秒内新建又删除」不留痕迹，宁可多枚举一个目录）。
/// stat 失败的目录**不算脏**：读不到就当没变，保留旧数据，只把状态标成不完整——
/// 反过来把「读不到」当「空了」，会直接把整棵子树的文件从索引里抹掉。
fn scan_dirty_dirs(
    dir_blob: &str,
    old_dirs: &[DirRec],
    started: u64,
) -> (Vec<(String, u64)>, bool) {
    if old_dirs.is_empty() {
        return (Vec::new(), false);
    }
    let threads = num_cpus::get().clamp(1, 16);
    let chunk = ((old_dirs.len() + threads - 1) / threads).max(1);
    let failed = AtomicBool::new(false);
    let out: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for slice in old_dirs.chunks(chunk) {
            let out = &out;
            let failed = &failed;
            scope.spawn(move || {
                let mut local: Vec<(String, u64)> = Vec::new();
                for rec in slice {
                    let key = dir_path_of(dir_blob, rec);
                    match dir_mtime_secs(key) {
                        Ok(mtime) => {
                            if mtime != rec.mtime
                                || mtime.saturating_add(DIR_SLACK_SECS) >= started
                            {
                                local.push((key.to_string(), mtime));
                            }
                        }
                        // 目录已经不在了：它的父目录 mtime 必然也变过、会被枚举到，
                        // 那里会把这条记录清掉，这里不需要也不该标成「读不到」。
                        Err(code) if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND => {}
                        Err(_) => failed.store(true, Ordering::Relaxed),
                    }
                }
                if !local.is_empty() {
                    out.lock().unwrap().extend(local);
                }
            });
        }
    });

    (out.into_inner().unwrap_or_default(), failed.load(Ordering::Relaxed))
}

/// 把（可能重叠的）区间排序并合并。
fn merge_ranges(mut r: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    r.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(r.len());
    for (lo, hi) in r {
        if lo >= hi {
            continue;
        }
        match out.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// 区间集合差：`a` 里去掉被 `b` 覆盖的部分（两侧都已排序合并）。
fn subtract(a: &[(usize, usize)], b: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut j = 0usize;
    for &(lo, hi) in a {
        let mut cur = lo;
        while j < b.len() && b[j].1 <= cur {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && b[k].0 < hi {
            if b[k].0 > cur {
                out.push((cur, b[k].0));
            }
            cur = cur.max(b[k].1);
            if cur >= hi {
                break;
            }
            k += 1;
        }
        if cur < hi {
            out.push((cur, hi));
        }
    }
    out
}

/// 丢弃区间（已排序、可能重叠）覆盖的元素个数。
fn covered(ranges: &[(usize, usize)]) -> usize {
    let mut n = 0usize;
    let mut last = 0usize;
    for (lo, hi) in ranges {
        let lo = (*lo).max(last);
        if hi > &lo {
            n += hi - lo;
            last = *hi;
        }
    }
    n
}

/// 把 `items` 中没被丢弃区间覆盖的元素按原顺序拷进 `out`。
fn copy_gaps<T: Copy>(items: &[T], ranges: &[(usize, usize)], out: &mut Vec<T>) {
    let mut prev = 0usize;
    for (lo, hi) in ranges {
        let lo = (*lo).max(prev).min(items.len());
        let hi = (*hi).max(lo).min(items.len());
        if lo > prev {
            out.extend_from_slice(&items[prev..lo]);
        }
        prev = hi;
    }
    if prev < items.len() {
        out.extend_from_slice(&items[prev..]);
    }
}

/// 把 blob 里已失效（条目被删）的路径文本清掉，重新紧凑排布。
/// 原地压缩路径区：把还活着的路径往前搬，丢掉被删除路径留下的空洞。
///
/// **必须原地做**：老写法是「新开一个 String 再拷 470MB」，压缩那一刻进程里
/// 同时躺着 579MB 旧串 + 468MB 新串，峰值白白多 470MB。
///
/// **必须按旧偏移升序搬**（不是按条目顺序）：条目按路径排序，但路径区是只追加的，
/// 增量刷新把新路径写在尾部、条目插在中间，所以 `items[i].path_off` 并不单调。
/// 若按条目顺序搬，写出的字节会盖掉后面某条还没读的源路径，把索引写坏
/// （2026-11-19 实机踩过：8M 条里 320 万条路径被拼接错乱）。
/// 按偏移升序处理时目标位置 cur 恒 ≤ 源位置，后面还没读的源一定在 cur 之后。
///
/// 阈值 1/8：垃圾超过 12.5% 就整理。多一次 8M 条索引的排序（约 0.2s），
/// 换来路径区不随运行时间无限膨胀。
fn repack(index: &mut Index) {
    index.ensure_owned();
    let Payload::Owned { blob, items, dir_blob, dirs } = &mut index.payload else {
        return;
    };
    let used: usize = items.iter().map(|it| it.path_len as usize).sum();
    if used > 0 && blob.len() > used + used / 8 {
        let mut order: Vec<u32> = (0..items.len() as u32).collect();
        order.sort_unstable_by_key(|&i| items[i as usize].path_off);
        // SAFETY: 只整段搬运已存在的路径子串并按 UTF-8 边界截断，内容仍是合法 UTF-8
        let bytes = unsafe { blob.as_bytes_mut() };
        let mut cur = 0usize;
        for i in order {
            let it = &mut items[i as usize];
            let off = it.path_off as usize;
            let len = it.path_len as usize;
            if off != cur {
                bytes.copy_within(off..off + len, cur);
            }
            let rel = (it.name_off - it.path_off) as usize;
            it.path_off = cur as u32;
            it.name_off = (cur + rel) as u32;
            cur += len;
        }
        unsafe { blob.as_mut_vec().truncate(cur) };
        blob.shrink_to_fit();
    }
    let dused: usize = dirs.iter().map(|r| r.path_len as usize).sum();
    if dused > 0 && dir_blob.len() > dused + dused / 8 {
        let mut order: Vec<u32> = (0..dirs.len() as u32).collect();
        order.sort_unstable_by_key(|&i| dirs[i as usize].path_off);
        // SAFETY: 同上，只搬完整的目录路径子串
        let bytes = unsafe { dir_blob.as_bytes_mut() };
        let mut cur = 0usize;
        for i in order {
            let rec = &mut dirs[i as usize];
            let off = rec.path_off as usize;
            let len = rec.path_len as usize;
            if off != cur {
                bytes.copy_within(off..off + len, cur);
            }
            rec.path_off = cur as u32;
            cur += len;
        }
        unsafe { dir_blob.as_mut_vec().truncate(cur) };
        dir_blob.shrink_to_fit();
    }
}

fn build() -> Option<Index> {
    build_with_match(None, None, 0, false).map(|(index, _)| index)
}

/// 一趟遍历收集所有文件；`match_re` 为 Some 时同时按文件名答查询（冷启动合并用）。
/// 匹配语义与 [`query`] 的 `match_item` 完全一致，保证「冷启动答的」和「之后索引查的」结果集相同。
fn build_with_match(
    match_re: Option<&Regex>,
    match_exts: Option<&HashSet<String>>,
    max: usize,
    match_path: bool,
) -> Option<(Index, Vec<Hit>)> {
    // 索引代表「开始遍历那一刻」的文件系统，而不是结束时刻：
    // 遍历过程中新建的文件可能落在已扫过的目录里而没进索引。
    // 用开始时间做新鲜度基准，零命中兜底才不会被刚建完的索引误判为「已最新」。
    let build_started = now_unix();
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
    // 只读一次环境变量：闭包里每条文件查 env 太贵
    let cap = max_entries();

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

    let hits = Mutex::new(Vec::new());
    // 目录表：路径 + 目录自身 mtime，增量刷新靠它判断哪些子树根本没动
    let raw_dirs: Mutex<Vec<(Box<str>, u64)>> = Mutex::new(Vec::new());
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
                    let key = path.to_string_lossy().replace('\\', "/");
                    raw_dirs
                        .lock()
                        .unwrap()
                        .push((key.into_boxed_str(), unix_secs(info.modified)));
                    return Step::Continue;
                }
                if count.fetch_add(1, Ordering::Relaxed) >= cap {
                    overflow.store(true, Ordering::Relaxed);
                    return Step::Quit;
                }
                let path_str = path.to_string_lossy().replace('\\', "/");
                let mtime = info.modified
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                // 冷启动答查询：匹配语义与 query 的 match_item 一致（含「按路径匹配」）
                if let Some(re) = match_re {
                    let name = path_str.rsplit('/').next().unwrap_or(&path_str);
                    let ok = match match_exts {
                        Some(exts) => exts.is_empty() || ext_matches(name, exts),
                        None => true,
                    };
                    let target = if match_path { path_str.as_str() } else { name };
                    if ok && re.is_match(target) {
                        let mut guard = hits.lock().unwrap();
                        if guard.len() < max {
                            guard.push(Hit { path: path_str.clone(), size: info.size, mtime });
                        }
                    }
                }

                let item = (path_str.into_boxed_str(), info.size, mtime);
                if tx.send(item).is_err() {
                    return Step::Quit;
                }
                Step::Continue
            },
        );
    }
    drop(tx);
    let mut raw = collector.join().ok()?;
    let complete = !overflow.load(Ordering::Relaxed);
    if !complete {
        eprintln!(
            "rust-seek: index hit the {} entry cap, results may be incomplete (fallback traversal will cover the rest; raise RUST_SEEK_MAX_ENTRIES to index more)",
            cap
        );
    }
    // 按路径排序：子目录查询靠二分定位区间，不用扫全表
    raw.sort_unstable_by(|a, b| a.0.cmp(&b.0));

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

    let mut dirs_sorted = raw_dirs.into_inner().unwrap_or_default();
    dirs_sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut dir_blob = String::with_capacity(dirs_sorted.iter().map(|(p, _)| p.len()).sum());
    let mut dirs = Vec::with_capacity(dirs_sorted.len());
    for (path, mtime) in dirs_sorted {
        if dir_blob.len() + path.len() > u32::MAX as usize {
            break;
        }
        let path_off = dir_blob.len() as u32;
        let path_len = path.len() as u32;
        dir_blob.push_str(&path);
        dirs.push(DirRec { path_off, path_len, mtime });
    }

    let index = Index {
        payload: Index::new_owned(blob, items, dir_blob, dirs),
        usn: Vec::new(),
        built_unix: build_started,
        complete,
        incomplete_dirs: false,
    };
    Some((index, hits.into_inner().unwrap_or_default()))
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

/// 清掉异常退出留下的缓存临时文件（写一半被杀 / 改名没跑完）。
///
/// 这类文件与正式缓存等大：本机实测 801MB + 852MB 两个僵尸躺了三天，
/// 白白占 1.6GB 盘。每个进程只扫一次，且只删「一小时前」的
/// （别的进程此刻可能正在写自己的临时文件）。
fn cleanup_stale_tmp() {
    static CLEANED: AtomicBool = AtomicBool::new(false);
    if CLEANED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(dir) = cache_path().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let ours = name.starts_with("index-v1.") || name.starts_with("bench-copy");
        if !ours || !name.ends_with(".tmp") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d.as_secs() > 3600);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 并行校验 UTF-8。切点取 ASCII 字节（<0x80），这样每一段都从合法字符边界开始。
///
/// 载入 790MB 缓存时这一步约 220ms（7.5M 条目 / 180MB 路径池），单线程跑是首查延迟的
/// 主要固定成本之一；切段校验后降到几十毫秒。
fn utf8_valid_parallel(bytes: &[u8]) -> bool {
    const MIN_PARALLEL: usize = 4 << 20;
    let workers = num_cpus::get().clamp(1, 16);
    if workers == 1 || bytes.len() < MIN_PARALLEL {
        return std::str::from_utf8(bytes).is_ok();
    }
    let mut cuts: Vec<usize> = Vec::with_capacity(workers + 1);
    cuts.push(0);
    for k in 1..workers {
        let mut i = bytes.len() * k / workers;
        while i < bytes.len() && bytes[i] >= 0x80 {
            i += 1;
        }
        cuts.push(i);
    }
    cuts.push(bytes.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = cuts
            .windows(2)
            .filter(|w| w[0] < w[1])
            .map(|w| {
                let seg = &bytes[w[0]..w[1]];
                s.spawn(move || std::str::from_utf8(seg).is_ok())
            })
            .collect();
        handles.into_iter().all(|h| h.join().unwrap_or(false))
    })
}

/// 按条切块并行反序列化；任何一块校验失败整体作废（返回 None）。
fn parallel_parse<T, F>(
    count: usize,
    item_bytes: usize,
    raw: &[u8],
    parse: F,
) -> Option<Vec<T>>
where
    F: Fn(&[u8]) -> Option<Vec<T>> + Sync,
    T: Send,
{
    const MIN_PARALLEL: usize = 200_000;
    if count == 0 {
        return Some(Vec::new());
    }
    let workers = num_cpus::get().clamp(1, 16).min(count);
    if workers == 1 || count < MIN_PARALLEL {
        return parse(raw);
    }
    let per = count.div_ceil(workers);
    let parse = &parse;
    let mut parts: Vec<Option<Vec<T>>> = Vec::with_capacity(workers);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|i| {
                let lo = i * per;
                let hi = ((i + 1) * per).min(count);
                let seg = &raw[lo * item_bytes..hi * item_bytes];
                s.spawn(move || parse(seg))
            })
            .collect();
        for h in handles {
            parts.push(h.join().ok().flatten());
        }
    });
    let mut out: Vec<T> = Vec::with_capacity(count);
    for p in parts {
        out.extend(p?);
    }
    Some(out)
}

fn parse_items(raw: &[u8], blob: &str) -> Option<Vec<Item>> {
    let mut items = Vec::with_capacity(raw.len() / ITEM_BYTES);
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
                && blob.is_char_boundary(o + l)
        };
        if !ok(item.path_off, item.path_len) || !ok(item.name_off, item.name_len) {
            return None;
        }
        items.push(item);
    }
    Some(items)
}

fn parse_dirs(raw: &[u8], blob: &str) -> Option<Vec<DirRec>> {
    let mut dirs = Vec::with_capacity(raw.len() / DIR_BYTES);
    for chunk in raw.chunks_exact(DIR_BYTES) {
        let rec = DirRec {
            path_off: u32::from_le_bytes(chunk[0..4].try_into().ok()?),
            path_len: u32::from_le_bytes(chunk[4..8].try_into().ok()?),
            mtime: u64::from_le_bytes(chunk[8..16].try_into().ok()?),
        };
        let (o, l) = (rec.path_off as usize, rec.path_len as usize);
        if !o
            .checked_add(l)
            .map_or(false, |end| end <= blob.len())
            || !blob.is_char_boundary(o)
            || !blob.is_char_boundary(o + l)
        {
            return None;
        }
        dirs.push(rec);
    }
    Some(dirs)
}

fn load_cache() -> Option<Index> {
    load_cache_from(&cache_path()?)
}

fn load_cache_from(path: &Path) -> Option<Index> {
    let len = std::fs::metadata(path).ok()?.len();
    if len < HDR as u64 + MAGIC.len() as u64 || len > 4_000_000_000 {
        return None;
    }
    // 用 mmap 读：600MB 缓存先 read_to_end 到 Vec 再切片复制，等于白占 600MB 堆；
    // mmap 只映射虚拟地址，真正复制的是后面必须 own 的路径池和数据段。
    let file = std::fs::File::open(path).ok()?;
    let map = unsafe { memmap2::Mmap::map(&file).ok()? };
    let buf: &[u8] = &map;

    if buf.len() < HDR {
        return None;
    }
    let is_v6 = &buf[..8] == MAGIC;
    let is_v5 = &buf[..8] == MAGIC_V5;
    let is_v4 = &buf[..8] == MAGIC_V4;
    if !is_v6 && !is_v5 && !is_v4 {
        return None;
    }
    // v5/v6 头部多 8 字节 usn_count；blob/条目/目录的相对偏移随之整体后移
    let hdr = if is_v4 { HDR } else { HDR_V5 };
    let built_unix = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    if now_unix().saturating_sub(built_unix) > MAX_CACHE_AGE_SECS {
        return None;
    }
    let blob_len = u64::from_le_bytes(buf[16..24].try_into().ok()?) as usize;
    let count = u64::from_le_bytes(buf[24..32].try_into().ok()?) as usize;
    let flags = u64::from_le_bytes(buf[32..40].try_into().ok()?);
    let dir_blob_len = u64::from_le_bytes(buf[40..48].try_into().ok()?) as usize;
    let dir_count = u64::from_le_bytes(buf[48..56].try_into().ok()?) as usize;
    let usn_count = if is_v4 {
        0
    } else {
        u64::from_le_bytes(buf[HDR..HDR_V5].try_into().ok()?) as usize
    };
    // v6 在 blob 后、目录文本后各补 0~7 字节，让条目/目录块 8 字节对齐
    let pad1 = if is_v6 { pad8(hdr + blob_len) } else { 0 };
    let items_start = hdr + blob_len + pad1;
    let items_end = items_start + count * ITEM_BYTES;
    let dblob_start = items_end;
    let dblob_end = dblob_start + dir_blob_len;
    let pad2 = if is_v6 { pad8(dblob_end) } else { 0 };
    let dirs_start = dblob_end + pad2;
    let dirs_end = dirs_start + dir_count * DIR_BYTES;
    if count > MAX_ENTRIES_CEIL
        || dir_count > MAX_ENTRIES_CEIL
        || usn_count > 64
        || dirs_end.checked_add(usn_count * USN_BYTES)? != buf.len()
    {
        return None;
    }

    let t_load = Instant::now();
    // 两段文本整段校验 UTF-8（并行、不复制），之后才能安全地当 &str 用
    if !utf8_valid_parallel(&buf[hdr..hdr + blob_len])
        || !utf8_valid_parallel(&buf[dblob_start..dblob_end])
    {
        if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
            eprintln!("[debug] cache: 路径池不是合法 UTF-8");
        }
        return None;
    }

    // 尾部水位：u8 盘符 + 7 字节填充 + journal_id(8) + next_usn(8)
    let mut usn: Vec<crate::usn::Commit> = Vec::with_capacity(usn_count);
    for i in 0..usn_count {
        let off = dirs_end + i * USN_BYTES;
        if off + USN_BYTES > buf.len() {
            return None;
        }
        let drive = buf[off] as char;
        let journal_id = u64::from_le_bytes(buf[off + 8..off + 16].try_into().ok()?);
        let next_usn = i64::from_le_bytes(buf[off + 16..off + 24].try_into().ok()?);
        usn.push(crate::usn::Commit { drive, journal_id, next_usn });
    }

    let payload = if is_v6 {
        // 零拷贝：条目/目录块直接当切片读，路径池直接当 &str 用。
        // Arc 让 Index 能在需要时安全地共享同一份映射。
        Payload::Mapped(Mapped {
            map: std::sync::Arc::new(map),
            blob: (hdr, hdr + blob_len),
            items: (items_start, count),
            dir_blob: (dblob_start, dblob_end),
            dirs: (dirs_start, dir_count),
        })
    } else {
        // 旧格式没有对齐，只能老老实实复制一份
        let blob = unsafe {
            String::from_utf8_unchecked(buf[hdr..hdr + blob_len].to_vec())
        };
        let items = parallel_parse(count, ITEM_BYTES, &buf[items_start..items_end], |raw| {
            parse_items(raw, &blob)
        })?;
        let dir_blob = unsafe {
            String::from_utf8_unchecked(buf[dblob_start..dblob_end].to_vec())
        };
        let dirs = parallel_parse(dir_count, DIR_BYTES, &buf[dirs_start..dirs_end], |raw| {
            parse_dirs(raw, &dir_blob)
        })?;
        Index::new_owned(blob, items, dir_blob, dirs)
    };
    if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
        eprintln!("[debug] cache load v6={is_v6} in {:?}", t_load.elapsed());
    }

    Some(Index {
        payload,
        usn,
        built_unix,
        complete: flags & FLAG_COMPLETE != 0,
        incomplete_dirs: flags & FLAG_INCOMPLETE_DIRS != 0,
    })
}

/// 只做序列化，不碰磁盘。需要持有索引锁，但耗时只到内存拷贝（~300ms）。
/// 缓存文件三段对齐后的总字节数，以及两处填充长度。
fn cache_layout(index: &Index) -> (usize, usize, usize) {
    let pad1 = pad8(HDR_V5 + index.blob().len());
    let pad2 = pad8(
        HDR_V5
            + index.blob().len()
            + pad1
            + index.items().len() * ITEM_BYTES
            + index.dir_blob().len(),
    );
    let total = HDR_V5
        + index.blob().len()
        + pad1
        + index.items().len() * ITEM_BYTES
        + index.dir_blob().len()
        + pad2
        + index.dirs().len() * DIR_BYTES
        + index.usn.len() * USN_BYTES;
    (pad1, pad2, total)
}

/// 发布时每次从索引里切出来搬运的字节数。写 730MB 期间索引锁只被切成
/// 1MB 一段的毫秒级小片，查询不会被一整秒的写盘卡住。
const PUBLISH_CHUNK: usize = 1 << 20;

/// 缓存里四段大字节区。条目/目录块是 `#[repr(C)]` 定长结构，
/// 内存排布与小端序列化结果逐字节相同（mmap 载入路径本来就依赖这一点），
/// 所以可以直接按原始字节搬运，不必逐字段拼。
#[derive(Clone, Copy)]
enum Seg {
    Blob,
    Items,
    DirBlob,
    Dirs,
}

impl Seg {
    fn raw<'a>(self, ix: &'a Index) -> &'a [u8] {
        unsafe {
            match self {
                Seg::Blob => ix.blob().as_bytes(),
                Seg::Items => std::slice::from_raw_parts(
                    ix.items().as_ptr() as *const u8,
                    ix.items().len() * ITEM_BYTES,
                ),
                Seg::DirBlob => ix.dir_blob().as_bytes(),
                Seg::Dirs => std::slice::from_raw_parts(
                    ix.dirs().as_ptr() as *const u8,
                    ix.dirs().len() * DIR_BYTES,
                ),
            }
        }
    }
}

/// 缓存头部 64 字节。
fn cache_header(index: &Index) -> [u8; HDR_V5] {
    let mut h = [0u8; HDR_V5];
    h[0..8].copy_from_slice(MAGIC);
    h[8..16].copy_from_slice(&index.built_unix.to_le_bytes());
    h[16..24].copy_from_slice(&(index.blob().len() as u64).to_le_bytes());
    h[24..32].copy_from_slice(&(index.items().len() as u64).to_le_bytes());
    let mut flags = if index.complete { FLAG_COMPLETE } else { 0 };
    if index.incomplete_dirs {
        flags |= FLAG_INCOMPLETE_DIRS;
    }
    h[32..40].copy_from_slice(&flags.to_le_bytes());
    h[40..48].copy_from_slice(&(index.dir_blob().len() as u64).to_le_bytes());
    h[48..56].copy_from_slice(&(index.dirs().len() as u64).to_le_bytes());
    h[56..64].copy_from_slice(&(index.usn.len() as u64).to_le_bytes());
    h
}

/// 顺序写入器：把索引按缓存格式填进一段已经备好的字节里（仅测试用：
/// 生产发布是分块直写，不再整块拼字节）。
#[cfg(test)]
struct ByteSink<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

#[cfg(test)]
impl<'a> ByteSink<'a> {
    fn put(&mut self, bytes: &[u8]) {
        let end = self.pos + bytes.len();
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
    }
    fn zeros(&mut self, n: usize) {
        let end = self.pos + n;
        self.buf[self.pos..end].fill(0);
        self.pos = end;
    }
}

#[cfg(test)]
fn write_cache_into(index: &Index, buf: &mut [u8], pad1: usize, pad2: usize) {
    let mut w = ByteSink { buf, pos: 0 };
    w.put(&cache_header(index));
    w.put(Seg::Blob.raw(index));
    // 对齐填充：让条目块从 8 的倍数偏移开始（mmap 后可直接当 &[Item]）
    w.zeros(pad1);
    w.put(Seg::Items.raw(index));
    w.put(Seg::DirBlob.raw(index));
    w.zeros(pad2);
    w.put(Seg::Dirs.raw(index));
    // 尾部：本份索引覆盖到的 USN 水位（24 字节/盘）
    for c in &index.usn {
        let mut rec = [0u8; USN_BYTES];
        rec[0] = c.drive as u8;
        rec[8..16].copy_from_slice(&c.journal_id.to_le_bytes());
        rec[16..24].copy_from_slice(&c.next_usn.to_le_bytes());
        w.put(&rec);
    }
}

/// 测试与旧调用方用：序列化成一整块 `Vec<u8>`。
/// 生产发布**不走它**——840MB 的堆分配会让写者峰值内存翻倍。
#[cfg(test)]
fn build_cache_bytes(index: &Index) -> Vec<u8> {
    let (pad1, pad2, total) = cache_layout(index);
    let mut buf = vec![0u8; total];
    write_cache_into(index, &mut buf, pad1, pad2);
    buf
}

/// 发布当前内存索引到缓存文件。调用方**不得持有** INDEX 锁。
///
/// 三条不变量：
/// 1. 串行发布：并发发布会让旧快照最后落盘、把新快照盖掉；
/// 2. 锁内只序列化并记录「这份字节对应的代数」，写盘在索引锁外做；
/// 3. 成功后只能把 `SAVED_GEN` 推进到这份字节的代数，**绝不能**读「此刻最新」代数——
///    写盘期间若又发生刷新，读最新代数会把旧字节标成新内容已保存，之后永不补写。
fn publish_cache() -> bool {
    let Some(path) = cache_path() else { return false };
    publish_to(&path)
}

/// [`publish_cache`] 的本体：把当前内存索引发布到 `path`。
/// 只在目标是正式缓存时才清变更日志——测试写到临时目录不能动生产日志。
///
/// 写盘方式：**分块搬运**，不是「先拼一份 730MB」也不是「整块映射后写」。
/// 实测后两种都会让写者峰值多 600–800MB（拼 Vec 是私有内存，映射是驻留的脏页），
/// 而每 30 秒来一次的话，这一下就是「系统突然很卡」的直接来源。
/// 现在锁内只拷 1MB 到复用缓冲、锁外写盘，峰值不再随缓存大小翻倍。
fn publish_to(path: &Path) -> bool {
    let _serial = CACHE_PUBLISH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let fail = |step: &str, e: Option<std::io::Error>| {
        if std::env::var_os("RUST_SEEK_DEBUG").is_some() {
            eprintln!("[debug] publish_to {step} 失败: {e:?}");
        }
    };
    // 临时文件名带 pid + 进程内序号：同进程并发发布也不会抢同一个文件
    let seq = CACHE_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), seq));

    // 布局、头部、代数必须来自同一份快照：分开读会出现「旧长度 + 新代数」。
    let (pad1, pad2, total, header, lengths, snapshot_gen) = {
        let guard = lock();
        let Some(ix) = guard.as_ref() else { return false };
        let (pad1, pad2, total) = cache_layout(ix);
        let lengths = [
            ix.blob().len(),
            ix.items().len() * ITEM_BYTES,
            ix.dir_blob().len(),
            ix.dirs().len() * DIR_BYTES,
        ];
        (
            pad1,
            pad2,
            total,
            cache_header(ix),
            lengths,
            CONTENT_GEN.load(Ordering::SeqCst),
        )
    };

    let file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(e) => {
            fail("create", Some(e));
            return false;
        }
    };
    let mut out = std::io::BufWriter::with_capacity(PUBLISH_CHUNK, file);
    let mut chunk = vec![0u8; PUBLISH_CHUNK];
    let mut written = 0u64;

    // 任一步失败或发现内容已变：删掉半个临时文件，保持脏，下一轮重试。
    macro_rules! give_up {
        ($step:expr, $e:expr) => {{
            fail($step, $e);
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            return false;
        }};
    }

    if let Err(e) = out.write_all(&header) {
        give_up!("header", Some(e));
    }
    written += header.len() as u64;

    // 四段大区：锁内拷 1MB → 锁外写。中间任何内容变更都让这份字节前后矛盾，
    // 必须整体作废（CONTENT_GEN 只在持锁时变，所以每块检查一次就够）。
    let segs = [Seg::Blob, Seg::Items, Seg::DirBlob, Seg::Dirs];
    let pads = [pad1, 0, pad2, 0];
    for (i, &len) in lengths.iter().enumerate() {
        let seg = segs[i];
        let mut off = 0usize;
        while off < len {
            let n = (len - off).min(chunk.len());
            let stale = {
                let guard = lock();
                let Some(ix) = guard.as_ref() else { give_up!("index-gone", None) };
                if CONTENT_GEN.load(Ordering::SeqCst) != snapshot_gen {
                    true
                } else {
                    chunk[..n].copy_from_slice(&seg.raw(ix)[off..off + n]);
                    false
                }
            };
            if stale {
                give_up!("stale", None);
            }
            if let Err(e) = out.write_all(&chunk[..n]) {
                give_up!("body", Some(e));
            }
            off += n;
        }
        let pad = pads[i];
        if pad > 0 {
            if let Err(e) = out.write_all(&[0u8; 8][..pad]) {
                give_up!("pad", Some(e));
            }
        }
        written += len as u64 + pad as u64;
    }

    // 尾部水位（每盘 24 字节，很小，直接持锁写完）
    {
        let guard = lock();
        let Some(ix) = guard.as_ref() else { give_up!("index-gone", None) };
        if CONTENT_GEN.load(Ordering::SeqCst) != snapshot_gen {
            give_up!("stale", None);
        }
        for c in &ix.usn {
            let mut rec = [0u8; USN_BYTES];
            rec[0] = c.drive as u8;
            rec[8..16].copy_from_slice(&c.journal_id.to_le_bytes());
            rec[16..24].copy_from_slice(&c.next_usn.to_le_bytes());
            if let Err(e) = out.write_all(&rec) {
                give_up!("usn", Some(e));
            }
        }
        written += (ix.usn.len() * USN_BYTES) as u64;
    }

    if written != total as u64 {
        give_up!("size", None);
    }
    if let Err(e) = out.flush() {
        give_up!("flush", Some(e));
    }
    drop(out);

    if let Err(e) = std::fs::rename(&tmp, path) {
        fail("rename", Some(e));
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // 单调推进：即使有更旧的写迟到，也不能把「已保存代数」拉回去
    SAVED_GEN.fetch_max(snapshot_gen, Ordering::SeqCst);
    // 新缓存代表「此刻的文件系统」，之前那份日志描述的空窗已经归零。
    // 只对正式缓存生效：测试写到临时目录时不能删生产日志。
    if cache_path().as_deref() == Some(path) {
        if let Some(j) = journal_path() {
            let _ = std::fs::remove_file(j);
        }
    }
    true
}

/// 把一小段字节原子写盘（唯一临时文件 + rename）。测试与兼容用；
/// 生产发布走 [`publish_cache`] 的映射写法，不需要索引锁。
#[cfg(test)]
fn write_cache_bytes(buf: &[u8], path: &std::path::Path, snapshot_gen: u64) -> bool {
    // 临时文件名带 pid + 进程内序号：同进程并发发布也不会抢同一个文件
    let seq = CACHE_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), seq));
    let mut file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if file.write_all(buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    drop(file);
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // 单调推进：即使有更旧的写迟到，也不能把「已保存代数」拉回去
    SAVED_GEN.fetch_max(snapshot_gen, Ordering::SeqCst);
    true
}

#[cfg(test)]
mod tests {
    //! 索引层目前没有对外的 golden 对照，这里先钉住查询地基的两条不变量：
    //!   1) 条目按完整路径字节序升序（prefix_bounds 的二分依赖它）
    //!   2) `prefix_bounds` 给出的区间 == 线性筛出的子树（不漏不多）
    //! 以后改存储格式（目录/名字分离、mmap）时，这些用例必须原样通过。
    use super::*;

    /// 实机探针（默认忽略）：水位必须能从缓存文件里读出来，且能直接续增量。
    /// 这是「索引内容与水位的原子性」的验证点——不再有独立的 usn-state.json。
    /// 运行：cargo test --release live_usn_from_cache -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_usn_from_cache() {
        assert!(try_load(), "载入缓存失败");
        let st = current_usn_state();
        println!("cache watermark: {st:?}");
        assert!(!st.is_empty(), "缓存里没有 USN 水位");
        let drives = integration::local_drives();
        let r = crate::usn::dirty_dirs_since_snapshot(&drives, &st);
        println!("dirty dirs: {:?}", r.as_ref().map(|(d, _)| d.len()));
        assert!(r.is_some(), "从缓存水位出发的增量不可用");
        // 独立的 usn-state.json 不应再出现
        let legacy = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap()
            .join("rust-seek")
            .join("usn-state.json");
        assert!(!legacy.exists(), "仍然写出了旧的 usn-state.json");
    }

    /// 实机探针（默认忽略）：实时输入路径必须立刻返回，刷新只能甩后台。
    /// 运行：cargo test --release live_realtime_path -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_realtime_path() {
        let _ = try_load();
        let t = std::time::Instant::now();
        let ok = ensure_fresh_quick(auto_refresh_secs());
        let took = t.elapsed();
        println!("ensure_fresh_quick: {:?} ok={}", took, ok);
        assert!(ok);
        assert!(
            took < std::time::Duration::from_millis(50),
            "实时路径阻塞了 {took:?}，打字会卡"
        );
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    /// 实机探针（默认忽略）：拆 ensure_ready 与查询各自的耗时。
    /// 运行：cargo test --release live_ready_timing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_ready_timing() {
        let t = std::time::Instant::now();
        ensure_ready();
        println!("ensure_ready: {:?}", t.elapsed());
        let re = Regex::new("zzzz_no_such_q1").unwrap();
        let exts = HashSet::new();
        for i in 0..3 {
            let t = std::time::Instant::now();
            let r = query(&re, &["C:/".into(), "D:/".into()], &exts, 20, None, false);
            println!(
                "query {i}: {:?} hits={:?}",
                t.elapsed(),
                r.as_ref().map(|x| x.0.len())
            );
        }
    }

    /// 实机探针（默认忽略）：量一下载入 790MB 缓存到底花在哪。
    /// 运行：cargo test --release live_cache_load_timing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_cache_load_timing() {
        for i in 0..3 {
            let t = std::time::Instant::now();
            let ix = load_cache();
            println!(
                "load {i}: {:?} entries={:?}",
                t.elapsed(),
                ix.as_ref().map(|x| x.items().len())
            );
        }
    }

    /// 实机探针（默认忽略）：全盘建一次索引，验证「种下 USN 增量起点」。
    /// 运行：cargo test --release live_seed_usn -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_seed_usn_after_build() {
        let drives = integration::local_drives();
        let ok = build_sync();
        assert!(ok, "build_sync 失败");
        assert!(
            !crate::usn::needs_seed(&drives, &current_usn_state()),
            "建完索引后仍缺增量起点"
        );
    }

    /// 实机探针（默认忽略）：建一个文件，确认 USN 能把它所在目录报成脏目录。
    /// 运行：cargo test --release live_usn_sees_new_file -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_usn_sees_new_file() {
        let drives = integration::local_drives();
        if crate::usn::needs_seed(&drives, &current_usn_state()) {
            eprintln!("跳过：还没有增量起点，先跑 live_seed_usn_after_build");
            return;
        }
        let dir = std::path::Path::new(r"D:\");
        let probe = dir.join(format!("usn_probe_{}.txt", std::process::id()));
        std::fs::write(&probe, b"hello").expect("写探针文件失败");
        let state = current_usn_state();
        let (dirty, _) = crate::usn::dirty_dirs_since_snapshot(&drives, &state).expect("增量不可用");
        let target = format!("D:/{}", probe.file_name().unwrap().to_string_lossy());
        let parent = "D:/".to_string();
        let _ = target;
        assert!(
            dirty.iter().any(|d| d == &parent || d == "D:"),
            "USN 没把盘根报成脏目录，实际: {dirty:?}"
        );
        let _ = std::fs::remove_file(&probe);
    }

    /// 用一组完整路径拼出一个与 build 同构的索引（条目升序、目录表升序）。
    fn synth(paths: &[&str]) -> Index {
        let mut sorted: Vec<&str> = paths.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        let mut blob = String::new();
        let mut items = Vec::new();
        let mut dir_set: Vec<String> = Vec::new();
        for p in &sorted {
            let off = blob.len() as u32;
            blob.push_str(p);
            let name_start = p.rfind('/').map(|i| i + 1).unwrap_or(0);
            items.push(Item {
                path_off: off,
                path_len: p.len() as u32,
                name_off: off + name_start as u32,
                name_len: (p.len() - name_start) as u32,
                size: 0,
                mtime: 0,
            });
            if let Some(i) = p.rfind('/') {
                dir_set.push(p[..i].to_string());
            }
        }
        dir_set.sort_unstable();
        dir_set.dedup();

        let mut dir_blob = String::new();
        let mut dirs = Vec::new();
        for d in &dir_set {
            let off = dir_blob.len() as u32;
            dir_blob.push_str(d);
            dirs.push(DirRec { path_off: off, path_len: d.len() as u32, mtime: 0 });
        }

        Index {
            payload: Index::new_owned(blob, items, dir_blob, dirs),
            usn: Vec::new(),
            built_unix: 0,
            complete: true,
            incomplete_dirs: false,
        }
    }

    fn subtree(index: &Index, root: &str) -> Vec<String> {
        let prefix = norm_prefix(root);
        let (lo, hi) = prefix_bounds(index, &prefix);
        index.items()[lo..hi]
            .iter()
            .map(|it| index.path(it).to_string())
            .collect()
    }

    fn naive(index: &Index, root: &str) -> Vec<String> {
        let prefix = norm_prefix(root);
        index
            .items()
            .iter()
            .map(|it| index.path(it).to_string())
            .filter(|p| p.starts_with(&prefix))
            .collect()
    }

    const SAMPLE: &[&str] = &[
        "C:/Users/a/.env",
        "C:/Users/a/readme.md",
        "C:/Users/a/proj/src/main.rs",
        "C:/Users/a/proj/src/lib.rs",
        "C:/Users/ab/other.txt",
        "C:/Windows/notepad.exe",
        "D:/",
        "D:/work/a.txt",
        "D:/work/sub/b.txt",
        "D:/work/sub/deep/c.txt",
        "D:/work2/d.txt",
        "D:/work3.txt",
        "D:/归档/述职材料.md",
        "D:/归档/2026 报告/说明.txt",
    ];

    #[test]
    fn items_are_sorted_by_full_path() {
        let idx = synth(SAMPLE);
        for w in idx.items().windows(2) {
            assert!(
                idx.path(&w[0]) < idx.path(&w[1]),
                "条目未按路径升序：{:?} >= {:?}",
                idx.path(&w[0]),
                idx.path(&w[1])
            );
        }
    }

    #[test]
    fn prefix_bounds_match_linear_scan() {
        let idx = synth(SAMPLE);
        for root in [
            "D:/",
            "D:/work",
            "D:/work/",
            "D:/work/sub",
            "D:/work2",
            "C:/Users",
            "C:/Users/a",
            "C:/Users/ab",
            "C:/Windows",
            "D:/归档",
        ] {
            assert_eq!(subtree(&idx, root), naive(&idx, root), "根 {root} 的子树区间不对");
        }
    }

    #[test]
    fn prefix_bounds_does_not_leak_neighbours() {
        let idx = synth(SAMPLE);
        // D:/work 不能把 D:/work2、D:/work3.txt 带进来
        let got = subtree(&idx, "D:/work");
        assert!(!got.iter().any(|p| p.starts_with("D:/work2")));
        assert!(!got.iter().any(|p| p.starts_with("D:/work3")));
        // 盘根只取本盘
        assert!(subtree(&idx, "D:/").iter().all(|p| p.starts_with("D:/")));
    }

    #[test]
    fn drive_letter_case_is_normalised() {
        let idx = synth(SAMPLE);
        assert_eq!(subtree(&idx, "d:/work"), subtree(&idx, "D:/work"));
    }

    #[test]
    fn match_item_name_vs_path() {
        let idx = synth(SAMPLE);
        let re = regex::Regex::new(r"^lib\.rs$").unwrap();
        let exts = std::collections::HashSet::new();

        // 文件名模式：只有 lib.rs 命中
        let mut hits = Vec::new();
        for it in idx.items() {
            match_item(&idx, it, &re, &exts, &mut hits, false);
        }
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("/lib.rs"));

        // 整路径模式：锚定在文件名开头的正则对完整路径不命中（现有语义，供改造对照）
        let mut path_hits = Vec::new();
        for it in idx.items() {
            match_item(&idx, it, &re, &exts, &mut path_hits, true);
        }
        assert!(path_hits.is_empty());
    }

    // ---------- 纯字面量连续扫描 ----------

    fn exts_of(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn fast_hits(idx: &Index, re: &Regex, exts: &[&str], max: usize) -> Vec<String> {
        let exts = exts_of(exts);
        let mut hits = Vec::new();
        let mut buf = String::new();
        let mut map = Vec::new();
        scan_literal_chunk(
            idx,
            idx.items(),
            re,
            &exts,
            max,
            &mut hits,
            &mut buf,
            &mut map,
        );
        hits.into_iter().map(|h| h.path).collect()
    }

    fn legacy_hits(idx: &Index, re: &Regex, exts: &[&str], max: usize) -> Vec<String> {
        let exts = exts_of(exts);
        let mut hits = Vec::new();
        for item in idx.items() {
            if hits.len() >= max {
                break;
            }
            match_item(idx, item, re, &exts, &mut hits, false);
        }
        hits.into_iter().map(|h| h.path).collect()
    }

    #[test]
    fn safe_literal_only_accepts_plain_text() {
        for ok in ["rust-seek", "述职", "a b", "Cargo", "报告-2026"] {
            assert!(safe_literal(&Regex::new(ok).unwrap()), "{ok:?} 应走快路径");
        }
        // GUI 纯文本模式 / MCP 非正则模式都会先 regex::escape；
        // 转义后的元字符匹配的仍是固定字节，必须放行，否则「main.rs」这类
        // 最常见的文件名查询永远吃不到快路径。
        for raw in ["main.rs", "报告 (1).pdf", "a+b[c]", r"C:\path\x", "100%", "a|b"] {
            let pat = regex::escape(raw);
            assert!(
                safe_literal(&Regex::new(&pat).unwrap()),
                "{pat:?}（escape({raw:?})）应走快路径"
            );
        }
        for bad in [
            "", "foo.bar", "^lib", "lib$", "a|b", "a*", "a+", "(?i)foo", r"a\sb", "[ab]",
            "{2}", "foo\nbar", r"foo\r", r"\d+", r"\x41", r"\p{L}", r"\A", r"\z",
        ] {
            // 编译不过的模式根本走不到查询阶段；能编译的必须被判为非字面量
            if let Ok(re) = Regex::new(bad) {
                assert!(!safe_literal(&re), "{bad:?} 必须回退逐条匹配");
            }
        }
    }

    #[test]
    fn literal_scan_matches_legacy_per_item() {
        let idx = synth(SAMPLE);
        let patterns = [
            "rs", "lib.rs", "readme", "述职", "a", "2026", "no-such-file", r"\.exe", r"readme\.md",
        ];
        for pat in patterns {
            let re = Regex::new(pat).unwrap();
            for max in [1usize, 2, 5, 100] {
                for exts in [&[][..], &["rs"][..], &["md", "txt"][..]] {
                    assert_eq!(
                        fast_hits(&idx, &re, exts, max),
                        legacy_hits(&idx, &re, exts, max),
                        "模式 {pat:?} max={max} exts={exts:?} 的新旧结果不一致"
                    );
                }
            }
        }
    }

    #[test]
    fn literal_scan_batch_boundaries_match_legacy() {
        // 快路径按 1024 条一批拼接；这里专门跨批边界比对，
        // 防止 `base * BATCH + i` 把命中映射到错误的条目上。
        let names: Vec<String> = (0..5000)
            .map(|i| format!("D:/bulk/dir{}/file-{:04}-x{}.txt", i % 37, i, i % 11))
            .collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let idx = synth(&refs);

        for pat in [
            "file-0000", "file-1023", "file-1024", "file-2048", "file-4999", "dir36/", "file-",
        ] {
            let re = Regex::new(pat).unwrap();
            for max in [1usize, 3, 50, 10_000] {
                for exts in [&[][..], &["txt"][..], &["rs"][..]] {
                    assert_eq!(
                        fast_hits(&idx, &re, exts, max),
                        legacy_hits(&idx, &re, exts, max),
                        "模式 {pat:?} max={max} exts={exts:?} 跨批结果不一致"
                    );
                }
            }
        }
    }

    #[test]
    fn literal_scan_keeps_case_insensitive_semantics() {
        let idx = synth(SAMPLE);
        let re = regex::RegexBuilder::new("LIB")
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(safe_literal(&re), "忽略大小写不改变模式本身是纯字面量");
        assert_eq!(fast_hits(&idx, &re, &[], 10), legacy_hits(&idx, &re, &[], 10));
        assert!(fast_hits(&idx, &re, &[], 10)
            .iter()
            .any(|p| p.ends_with("/lib.rs")));
    }

    #[test]
    fn literal_scan_dedups_repeated_matches_per_file() {
        let idx = synth(&["D:/x/aaaa.txt", "D:/x/b.txt"]);
        let re = Regex::new("a").unwrap();
        assert_eq!(fast_hits(&idx, &re, &[], 10), vec!["D:/x/aaaa.txt".to_string()]);
        // 上限按「文件数」而不是命中次数：两个 a 也只占一个名额
        assert_eq!(fast_hits(&idx, &re, &[], 1), vec!["D:/x/aaaa.txt".to_string()]);
    }

    // ---------- 缓存发布代数 ----------

    static PUBLISH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_cache_path(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "rust-seek-publish-{}-{}-{}",
            tag,
            std::process::id(),
            CACHE_WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index-v1.bin");
        (dir, path)
    }

    #[test]
    fn publish_uses_snapshot_gen_not_latest() {
        let _g = PUBLISH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, path) = test_cache_path("gen");
        CONTENT_GEN.store(2, Ordering::SeqCst);
        SAVED_GEN.store(0, Ordering::SeqCst);

        // 写盘期间内容又变了（CONTENT_GEN=2），但这批字节只代表 gen 1
        assert!(write_cache_bytes(b"old", &path, 1));
        assert_eq!(
            SAVED_GEN.load(Ordering::SeqCst),
            1,
            "旧字节不能把新代数标成已保存，否则新内容永不补写"
        );

        assert!(write_cache_bytes(b"new", &path, 2));
        assert_eq!(SAVED_GEN.load(Ordering::SeqCst), 2);

        // 迟到的旧写也不能把「已保存代数」拉回去
        assert!(write_cache_bytes(b"late-old", &path, 1));
        assert_eq!(SAVED_GEN.load(Ordering::SeqCst), 2);

        CONTENT_GEN.store(0, Ordering::SeqCst);
        SAVED_GEN.store(0, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_failure_keeps_dirty_for_retry() {
        let _g = PUBLISH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, _) = test_cache_path("fail");
        let missing = dir.join("missing-dir").join("index-v1.bin");
        CONTENT_GEN.store(5, Ordering::SeqCst);
        SAVED_GEN.store(4, Ordering::SeqCst);

        assert!(!write_cache_bytes(b"x", &missing, 5));
        assert_eq!(
            SAVED_GEN.load(Ordering::SeqCst),
            4,
            "写盘失败不得对齐代数：必须保持脏，让维护循环重试"
        );
        assert_ne!(
            CONTENT_GEN.load(Ordering::SeqCst),
            SAVED_GEN.load(Ordering::SeqCst)
        );

        CONTENT_GEN.store(0, Ordering::SeqCst);
        SAVED_GEN.store(0, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生产发布路径本身：映射写出来的缓存必须能被载入，代数只推进到快照那一代。
    #[test]
    fn publish_mapped_cache_is_loadable() {
        let _g = PUBLISH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, path) = test_cache_path("mapped");
        *lock() = Some(Box::new(tiny_index()));
        CONTENT_GEN.store(9, Ordering::SeqCst);
        SAVED_GEN.store(0, Ordering::SeqCst);

        assert!(publish_to(&path), "映射发布失败");

        assert_eq!(SAVED_GEN.load(Ordering::SeqCst), 9);
        let back = load_cache_from(&path).expect("发布出来的缓存必须能载入");
        assert_eq!(back.items().len(), 3);
        assert_eq!(back.items()[2].mtime, 1_700_000_000);
        assert_eq!(dir_path_of(&back.dir_blob(), &back.dirs()[0]), "C:/a");

        *lock() = None;
        CONTENT_GEN.store(0, Ordering::SeqCst);
        SAVED_GEN.store(0, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 缓存格式：序列化/压缩 ----------

    /// 小索引构造器：路径池、条目、目录表三样内容自洽且严格升序。
    fn tiny_index() -> Index {
        let mut blob = String::new();
        let mut items = Vec::new();
        for path in ["C:/a/one.txt", "C:/a/two.rs", "D:/b/中文名.md"] {
            let path_off = blob.len() as u32;
            blob.push_str(path);
            let rel = path.rfind('/').map_or(0, |i| i + 1) as u32;
            items.push(Item {
                path_off,
                path_len: path.len() as u32,
                name_off: path_off + rel,
                name_len: path.len() as u32 - rel,
                size: path.len() as u64,
                mtime: 1_700_000_000,
            });
        }
        let mut dir_blob = String::new();
        let mut dirs = Vec::new();
        for d in ["C:/a", "D:/b"] {
            let path_off = dir_blob.len() as u32;
            dir_blob.push_str(d);
            dirs.push(DirRec { path_off, path_len: d.len() as u32, mtime: 1_700_000_001 });
        }
        Index {
            payload: Index::new_owned(blob, items, dir_blob, dirs),
            usn: vec![crate::usn::Commit { drive: 'C', journal_id: 7, next_usn: 12345 }],
            built_unix: now_unix(),
            complete: true,
            incomplete_dirs: false,
        }
    }

    /// 发布改成「写进映射区」后，字节布局必须和原来逐字节等价：
    /// 写出来的缓存能原样载入回来，路径/大小/mtime/目录/USN 水位一个不差。
    #[test]
    fn cache_bytes_roundtrip_preserves_entries() {
        let (dir, path) = test_cache_path("roundtrip");
        let ix = tiny_index();
        let bytes = build_cache_bytes(&ix);
        std::fs::write(&path, &bytes).unwrap();
        let back = load_cache_from(&path).expect("写出来的缓存必须能载入");

        assert_eq!(back.items().len(), ix.items().len());
        for (a, b) in ix.items().iter().zip(back.items()) {
            assert_eq!(ix.path(a), back.path(b), "路径必须原样往返");
            assert_eq!(ix.name(a), back.name(b), "名字偏移必须原样往返");
            assert_eq!(a.size, b.size);
            assert_eq!(a.mtime, b.mtime);
        }
        let pa: Vec<&str> =
            ix.dirs().iter().map(|r| dir_path_of(&ix.dir_blob(), r)).collect();
        let pb: Vec<&str> =
            back.dirs().iter().map(|r| dir_path_of(&back.dir_blob(), r)).collect();
        assert_eq!(pa, pb);
        assert_eq!(back.usn.len(), 1);
        assert_eq!(back.usn[0].next_usn, 12345);
        assert!(back.complete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路径池只追加，删掉的路径会留在尾部变成垃圾。原地压缩必须做到：
    /// 路径区长度回落到「活字节」、所有条目路径与名字偏移仍然正确。
    ///
    /// 用例特意让**条目偏移不单调**（增量刷新就是这样：新路径写在尾部、
    /// 条目插在中间）。按条目顺序搬的实现会覆盖还没读的源，实机把 8M 条里
    /// 320 万条路径拼坏——这条断言就是那次事故的护栏。
    #[test]
    fn repack_compacts_garbage_in_place() {
        let mut ix = tiny_index();
        // 追加一条「后来新增」的路径到尾部，条目插在路径序中间（偏移比前一条小）
        let appended = "C:/a/three.txt";
        let app_off = ix.blob().len() as u32;
        ix.blob_mut().push_str(appended);
        let rel = appended.rfind('/').map_or(0, |i| i + 1) as u32;
        ix.items_mut().insert(
            1,
            Item {
                path_off: app_off,
                path_len: appended.len() as u32,
                name_off: app_off + rel,
                name_len: appended.len() as u32 - rel,
                size: 7,
                mtime: 1_700_000_002,
            },
        );
        // 再造一堆没人指向的垃圾，逼过 1/8 阈值
        ix.blob_mut().push_str(&"X:/deleted/should-not-be-here.bin".repeat(200));
        ix.dir_blob_mut().push_str(&"Y:/gone".repeat(100));
        let live: usize = ix.items().iter().map(|it| it.path_len as usize).sum();
        let before: Vec<String> = ix.items().iter().map(|it| ix.path(it).to_string()).collect();
        assert!(ix.blob().len() > live + live / 8, "先制造超过阈值的垃圾");

        repack(&mut ix);

        assert_eq!(ix.blob().len(), live, "压缩后路径区只剩活字节");
        let after: Vec<String> = ix.items().iter().map(|it| ix.path(it).to_string()).collect();
        assert_eq!(before, after, "压缩不能改变任何条目的路径");
        assert_eq!(ix.name(&ix.items()[1]), "three.txt", "名字偏移必须跟着搬");
        assert_eq!(ix.name(&ix.items()[2]), "two.rs", "尾部追加的条目不能串行");
        assert_eq!(ix.dirs().len(), 2);
        assert_eq!(dir_path_of(&ix.dir_blob(), &ix.dirs()[1]), "D:/b");
    }

    // ---------- 端到端性能验收 ----------

    /// 生产查询循环的对照复刻：64K 条目切块、多线程抢游标、每 worker 私有
    /// `Vec<Hit>` 再合并，与 `query` 完全同构。
    ///
    /// - `use_fast=false` → v5.4.8 的逐条匹配（改造前基线）
    /// - `use_fast=true`  → 新的连续名字扫描
    /// - `collect=false`  → 命中后不物化 `Hit`（只对零命中模式等价）
    ///
    /// 返回 (最优耗时, 命中数)。
    fn parallel_replica(
        re: &Regex,
        max: usize,
        rounds: usize,
        use_fast: bool,
        collect: bool,
    ) -> (Duration, usize) {
        let exts: HashSet<String> = HashSet::new();
        let mut best = Duration::MAX;
        let mut last = 0usize;
        for _ in 0..rounds {
            let t = Instant::now();
            let guard = lock();
            let index = guard.as_ref().expect("索引未就绪");
            const CHUNK: usize = 64 * 1024;
            let items = index.items();
            let tasks: Vec<(usize, usize)> = (0..items.len())
                .step_by(CHUNK)
                .map(|a| (a, (a + CHUNK).min(items.len())))
                .collect();
            let cursor = std::sync::atomic::AtomicUsize::new(0);
            let stop = AtomicBool::new(false);
            let out: Mutex<Vec<Hit>> = Mutex::new(Vec::new());
            let workers = num_cpus::get().clamp(1, 16).min(tasks.len());
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let cursor = &cursor;
                    let stop = &stop;
                    let out = &out;
                    let tasks = &tasks;
                    let exts = &exts;
                    scope.spawn(move || {
                        let mut local: Vec<Hit> = Vec::new();
                        let mut name_buf = String::new();
                        let mut name_map: Vec<(usize, u32)> = Vec::new();
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            let i = cursor.fetch_add(1, Ordering::Relaxed);
                            let Some((lo, hi)) = tasks.get(i).copied() else { break };
                            let chunk = &index.items()[lo..hi];
                            let limit_hit = if use_fast {
                                scan_literal_chunk(
                                    index,
                                    chunk,
                                    re,
                                    exts,
                                    max,
                                    &mut local,
                                    &mut name_buf,
                                    &mut name_map,
                                )
                            } else {
                                for item in chunk {
                                    if local.len() >= max {
                                        break;
                                    }
                                    match_item(index, item, re, exts, &mut local, false);
                                }
                                local.len() >= max
                            };
                            if limit_hit && collect {
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                            if limit_hit {
                                // 只测内核：清掉命中继续，跳过物化成本
                                local.clear();
                            }
                        }
                        if !local.is_empty() {
                            out.lock().unwrap().extend(local);
                        }
                    });
                }
            });
            last = out.into_inner().unwrap().len();
            drop(guard);
            best = best.min(t.elapsed());
        }
        (best, last)
    }

    /// 实机性能验收（默认忽略，需真实整盘缓存）：
    /// cargo test --release live_query_perf_acceptance -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_query_perf_acceptance() {
        ensure_ready();
        let (items, name_bytes) = {
            let g = lock();
            let ix = g.as_ref().expect("索引未就绪");
            let bytes: usize = ix.items().iter().map(|it| ix.name(it).len()).sum();
            (ix.items().len(), bytes)
        };
        println!("索引条目 {items}，名字区 {:.1} MB", name_bytes as f64 / 1e6);
        println!(
            "{:<22} {:>6} {:>10} {:>10} {:>9}",
            "模式", "命中", "新(ms)", "旧(ms)", "加速"
        );

        let exts: HashSet<String> = HashSet::new();
        for pat in [
            "rust-seek",
            "zzz-nonexistent-zzz",
            "zzz-不存在的词-zzz",
            "述职",
            "a",
            r"\.exe",
        ] {
            let re = Regex::new(pat).unwrap();
            // 预热页缓存 + 线程池
            let _ = query(&re, &[], &exts, 10, None, false);

            let t = Instant::now();
            let (hits, ..) = query(&re, &[], &exts, 10, None, false).unwrap();
            let fast = t.elapsed();

            let (legacy, _) = parallel_replica(&re, 10, 3, false, true);
            println!(
                "{:<22} {:>6} {:>10.2} {:>10.2} {:>8.1}x",
                pat,
                hits.len(),
                fast.as_secs_f64() * 1000.0,
                legacy.as_secs_f64() * 1000.0,
                legacy.as_secs_f64() / fast.as_secs_f64()
            );

            // 零命中模式再拆出「拼接 + 扫描内核」，差值即 Hit 物化与线程开销
            if hits.is_empty() {
                let (kernel, _) = parallel_replica(&re, 10, 3, true, false);
                println!(
                    "   └ 仅拼接+扫描内核 {:.2}ms，物化/线程开销 {:.2}ms",
                    kernel.as_secs_f64() * 1000.0,
                    (fast.as_secs_f64() - kernel.as_secs_f64()) * 1000.0
                );
            }
        }

        // 决定性拆分：拼接（只写不扫）vs 逐条匹配（只扫不拼）
        let re = Regex::new("zzz-不存在的词-zzz").unwrap();
        let mut best_join = Duration::MAX;
        let mut sink = 0usize;
        for _ in 0..3 {
            let t = Instant::now();
            let guard = lock();
            let index = guard.as_ref().unwrap();
            const CHUNK: usize = 64 * 1024;
            let items = index.items();
            let mut buf = String::new();
            let mut lo = 0;
            while lo < items.len() {
                let hi = (lo + CHUNK).min(items.len());
                buf.clear();
                for item in &items[lo..hi] {
                    buf.push_str(index.name(item));
                    buf.push('\n');
                }
                sink = sink.wrapping_add(buf.len());
                lo = hi;
            }
            drop(guard);
            best_join = best_join.min(t.elapsed());
        }
        let mut best_scan = Duration::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let guard = lock();
            let index = guard.as_ref().unwrap();
            let mut n = 0usize;
            for item in index.items() {
                if re.is_match(index.name(item)) {
                    n += 1;
                }
            }
            sink = sink.wrapping_add(n);
            drop(guard);
            best_scan = best_scan.min(t.elapsed());
        }
        println!(
            "拆分：仅拼接 {:.2}ms / 仅逐条匹配 {:.2}ms（sink={}）",
            best_join.as_secs_f64() * 1000.0,
            best_scan.as_secs_f64() * 1000.0,
            sink
        );
    }

    /// 实机测量：索引里有多少「已经没人指向的路径字节」（只增不减的泄漏量）。
    /// cargo test --release live_index_garbage -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_index_garbage() {
        ensure_ready();
        let g = lock();
        let ix = g.as_ref().expect("索引未就绪");
        let live: usize = ix.items().iter().map(|it| it.path_len as usize).sum();
        let blob = ix.blob().len();
        let live_dirs: usize = ix.dirs().iter().map(|d| d.path_len as usize).sum();
        let dblob = ix.dir_blob().len();
        println!(
            "条目 {} | 路径区 {:.1}MB，活字节 {:.1}MB，垃圾 {:.1}MB（{:.1}%）",
            ix.items().len(),
            blob as f64 / 1e6,
            live as f64 / 1e6,
            (blob - live) as f64 / 1e6,
            (blob - live) as f64 * 100.0 / blob.max(1) as f64
        );
        println!(
            "目录 {} | 路径区 {:.2}MB，活字节 {:.2}MB，垃圾 {:.2}MB（{:.1}%）",
            ix.dirs().len(),
            dblob as f64 / 1e6,
            live_dirs as f64 / 1e6,
            (dblob - live_dirs) as f64 / 1e6,
            (dblob - live_dirs) as f64 * 100.0 / dblob.max(1) as f64
        );
        println!("缓存文件 {:?}", cache_path().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    }
}
