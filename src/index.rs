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
const MAGIC: &[u8; 8] = b"RSKIDX\x04\x00";
const HDR: usize = 56;
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

/// 索引超过这么久就同步做一次目录级增量刷新。
/// 刷新只枚举 mtime 变了的目录（空闲时几十毫秒），所以阈值放得很小：
/// 宁可每次整盘查询多花几十毫秒，也不要给用户一个「看不见刚建文件」的快照。
/// `RUST_SEEK_REFRESH_SECS` 可覆盖（调试时可调大）。
pub fn auto_refresh_secs() -> u64 {
    std::env::var("RUST_SEEK_REFRESH_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(2)
}

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
#[derive(Clone, Copy)]
struct DirRec {
    path_off: u32,
    path_len: u32,
    mtime: u64,
}

struct Index {
    /// 所有路径按顺序拼接（`F:/a/b.rs` 形式），条目用偏移引用
    blob: String,
    items: Vec<Item>,
    /// 已进索引的目录（路径升序，不含盘符根）+ 它们的路径文本
    dir_blob: String,
    dirs: Vec<DirRec>,
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
    fn path(&self, item: &Item) -> &str {
        path_of(&self.blob, item)
    }

    fn name(&self, item: &Item) -> &str {
        &self.blob[item.name_off as usize..(item.name_off + item.name_len) as usize]
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
        ranges.push((0, index.items.len()));
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
                        for item in &index.items[lo..hi] {
                            // 单线程也设上限：命中上万的模式（比如搜 "a"）不该把结果集撑爆
                            if local.len() >= max {
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                            match_item(index, item, re, exts, &mut local, match_path);
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
        for item in &index.items {
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
    Some((hits, index.items.len() as u64, age_ms, truncated, index.complete))
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
        ranges.push((0, index.items.len()));
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
        for item in &index.items[lo..hi] {
            if !push(index, item, &mut hits) {
                truncated = true;
                break 'scan;
            }
        }
    }

    if !loose.is_empty() && !truncated {
        for item in &index.items {
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
    Some((hits, index.items.len() as u64, age_ms, truncated, index.complete))
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
    prefix_bounds_raw(&index.blob, &index.items, prefix)
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
pub fn ensure_ready() {
    start_watchers();
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
    start_watchers();
    match current_age_secs() {
        // 年龄够新、但监听还攒着没应用的变更（刚才建的文件就在里面）也得刷一下
        Some(a) if a <= max_age_secs && !has_pending_changes() => true,
        Some(_) => refresh_smart(),
        None => false,
    }
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
        let built = build();
        if let Some(index) = &built {
            save_cache(index);
            adopt_cache(index.built_unix);
        }
        let mut guard = lock();
        if let Some(index) = built {
            *guard = Some(Box::new(index));
        }
    });
}

/// 冷启动同步建索引（一趟遍历，阻塞当前线程），返回是否成功。
/// 用于首次整盘内容搜索：先把索引建好再走 [`candidates`]，避免后台建索引与兜底遍历并发抢 I/O。
pub fn build_sync() -> bool {
    let Some(_slot) = BuildSlot::acquire() else {
        return false;
    };
    let outcome = build();
    let ok = outcome.is_some();
    if let Some(index) = outcome {
        save_cache(&index);
        let built = index.built_unix;
        *lock() = Some(Box::new(index));
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
    let outcome = build_with_match(Some(re), Some(exts), max, match_path);
    match outcome {
        Some((index, hits)) => {
            let entries = index.items.len() as u64;
            let complete = index.complete;
            save_cache(&index);
            let built = index.built_unix;
            *lock() = Some(Box::new(index));
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
    let Some(_slot) = RefreshSlot::acquire() else { return false };
    let mut guard = lock();
    let Some(index) = guard.as_mut() else { return false };
    refresh_with(index)
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
                .map(|ix| (ix.dir_blob.clone(), ix.dirs.clone()))
        };
        let Some((dir_blob, dirs)) = snapshot else { return };
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
            return;
        }
        let hint: Vec<String> = dirty.into_iter().map(|(k, _)| k).collect();
        if refresh_in_place(index, Some(hint)) {
            NEED_FULL_SCAN.store(false, Ordering::Relaxed);
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
        }
    });
    true
}

/// 监听在跑、也没丢过事件：这时索引可以当作「当前文件系统」的权威视图，
/// 零命中可以直接回「没有」，不必再花 20-48s 走一遍全盘遍历。
fn watcher_trusted() -> bool {
    WATCH_ACTIVE.load(Ordering::Relaxed) && !NEED_FULL_SCAN.load(Ordering::Relaxed)
}

/// 刷新策略：
/// - 监听在跑且没溢出 ⇒ 只重枚举「有变更的目录」（通常几个，耗时可忽略）
/// - 首次 / 监听溢出 / 盘数量变了 ⇒ 全量 stat 巡（约 10s，交给后台）
fn refresh_with(index: &mut Index) -> bool {
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
            return true;
        }
        let ok = refresh_in_place(index, Some(dirty.clone()));
        if !ok {
            // 刷新失败不能当成成功：把脏目录放回队列，并强制下一轮全量扫描。
            // 否则这批变更永久缺失，而 built_unix 还会被推进到当前时间，
            // 「看起来更新鲜」的假象会让零命中直接返回「没有」。
            for d in dirty {
                note_dirty(&d);
            }
            NEED_FULL_SCAN.store(true, Ordering::Relaxed);
        }
        return ok;
    }
    let ok = refresh_in_place(index, None);
    if ok {
        NEED_FULL_SCAN.store(false, Ordering::Relaxed);
    }
    ok
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

    let old_items = std::mem::take(&mut index.items);
    let old_dirs = std::mem::take(&mut index.dirs);
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
        None => scan_dirty_dirs(&index.dir_blob, &old_dirs, started),
    };
    let scan_ms = t_start.elapsed().as_millis();
    dirty.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // 把「这些目录变过」记进变更日志：下一个进程载入同一份缓存时直接回放，
    // 就不用再花 10-15s 做全量 stat 巡。监听路径也会写，重复无妨（回放时会去重）。
    if !dirty.is_empty() {
        let keys: Vec<String> = dirty.iter().map(|(k, _)| k.clone()).collect();
        journal_append(index.built_unix, &keys);
    }

    // 一个脏目录都没有：文件系统没动过，刷新只是一次 stat 巡。
    if dirty.is_empty() {
        index.items = old_items;
        index.dirs = old_dirs;
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
        let dir_blob_snapshot = index.dir_blob.as_str();
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
        let (lo, hi) = prefix_bounds_raw(&index.blob, &old_items, &prefix);
        if hi > lo {
            enumer_sub.push((lo, hi));
        }
        let (dlo, dhi) = dir_prefix_bounds_raw(&index.dir_blob, &old_dirs, &prefix);
        if dhi > dlo {
            enumer_dir_sub.push((dlo, dhi));
        }
    }
    let enumer_set: HashSet<&str> = enumer.iter().map(|k| k.as_ref()).collect();
    let mut keep_sub: Vec<(usize, usize)> = Vec::new();
    let mut keep_dir_sub: Vec<(usize, usize)> = Vec::new();
    for (key, _) in &fresh_d {
        if enumer_set.contains(key.as_ref()) {
            continue; // 被枚举的目录不算「没变」，不豁免
        }
        let prefix = format!("{key}/");
        let (lo, hi) = prefix_bounds_raw(&index.blob, &old_items, &prefix);
        if hi > lo {
            keep_sub.push((lo, hi));
        }
        let (dlo, dhi) = dir_prefix_bounds_raw(&index.dir_blob, &old_dirs, &prefix);
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
            old_dirs.binary_search_by(|rec| dir_path_of(&index.dir_blob, rec).cmp(key.as_str()))
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
        index.items = old_items;
        index.dirs = old_dirs;
        return false;
    }

    let t_drop = t_start.elapsed().as_millis();
    if fresh.is_empty() && kept_len == old_items.len() {
        index.items = old_items;
    } else {
        let mut kept: Vec<Item> = Vec::with_capacity(kept_len);
        copy_gaps(&old_items, &item_drop, &mut kept);
        drop(old_items);

        // 新路径追加到 blob 尾部：旧偏移全部保留，不重拷 580MB 文本。
        let mut appended: Vec<Item> = Vec::with_capacity(fresh.len());
        for (path, size, mtime) in &fresh {
            if index.blob.len() + path.len() > u32::MAX as usize {
                break;
            }
            let path_off = index.blob.len() as u32;
            index.blob.push_str(path);
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
        let blob = index.blob.as_str();
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
        index.items = merged;
    }

    if fresh_d.is_empty() && kept_dirs_len == old_dirs.len() {
        index.dirs = old_dirs;
    } else {
        let mut kept_dirs: Vec<DirRec> = Vec::with_capacity(kept_dirs_len);
        copy_gaps(&old_dirs, &dir_drop, &mut kept_dirs);
        drop(old_dirs);

        let mut appended_d: Vec<DirRec> = Vec::with_capacity(fresh_d.len());
        for (path, mtime) in &fresh_d {
            if index.dir_blob.len() + path.len() > u32::MAX as usize {
                break;
            }
            let path_off = index.dir_blob.len() as u32;
            index.dir_blob.push_str(path);
            appended_d.push(DirRec { path_off, path_len: path.len() as u32, mtime: *mtime });
        }

        let dir_blob = index.dir_blob.as_str();
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
        index.dirs = merged;
    }

    let t_merge = t_start.elapsed().as_millis();
    if std::env::var_os("RUST_SEEK_DEBUG_REFRESH").is_some() {
        // 结构自检：条目表必须严格升序（等价于无重复）——二分区间查询全靠它
        let mut bad = 0usize;
        for w in index.items.windows(2) {
            if path_of(&index.blob, &w[0]) >= path_of(&index.blob, &w[1]) {
                bad += 1;
                if bad <= 3 {
                    eprintln!(
                        "rust-seek BAD ORDER: {:?} -> {:?}",
                        path_of(&index.blob, &w[0]),
                        path_of(&index.blob, &w[1])
                    );
                }
            }
        }
        let mut dbad = 0usize;
        for w in index.dirs.windows(2) {
            if dir_path_of(&index.dir_blob, &w[0]) >= dir_path_of(&index.dir_blob, &w[1]) {
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
            index.items.len(),
            kept_len,
            index.dirs.len(),
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
fn repack(index: &mut Index) {
    let used: usize = index.items.iter().map(|it| it.path_len as usize).sum();
    if used > 0 && index.blob.len() > used + used / 4 {
        let mut blob = String::with_capacity(used);
        for it in index.items.iter_mut() {
            let path: &str =
                &index.blob[it.path_off as usize..(it.path_off + it.path_len) as usize];
            let path_off = blob.len() as u32;
            let rel = path.rfind('/').map_or(0, |i| i + 1) as u32;
            blob.push_str(path);
            it.path_off = path_off;
            it.name_off = path_off + rel;
            it.name_len = it.path_len - rel;
        }
        index.blob = blob;
    }
    let dused: usize = index.dirs.iter().map(|r| r.path_len as usize).sum();
    if dused > 0 && index.dir_blob.len() > dused + dused / 4 {
        let mut blob = String::with_capacity(dused);
        for rec in index.dirs.iter_mut() {
            let path: &str =
                &index.dir_blob[rec.path_off as usize..(rec.path_off + rec.path_len) as usize];
            rec.path_off = blob.len() as u32;
            blob.push_str(path);
        }
        index.dir_blob = blob;
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
        blob,
        items,
        dir_blob,
        dirs,
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

fn load_cache() -> Option<Index> {
    let path = cache_path()?;
    let len = std::fs::metadata(&path).ok()?.len();
    if len < HDR as u64 + MAGIC.len() as u64 || len > 4_000_000_000 {
        return None;
    }
    // 用 mmap 读：600MB 缓存先 read_to_end 到 Vec 再切片复制，等于白占 600MB 堆；
    // mmap 只映射虚拟地址，真正复制的是后面必须 own 的路径池和数据段。
    let file = std::fs::File::open(&path).ok()?;
    let map = unsafe { memmap2::Mmap::map(&file).ok()? };
    let buf: &[u8] = &map;

    if buf.len() < HDR || &buf[..8] != MAGIC {
        return None;
    }
    let built_unix = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    if now_unix().saturating_sub(built_unix) > MAX_CACHE_AGE_SECS {
        return None;
    }
    let blob_len = u64::from_le_bytes(buf[16..24].try_into().ok()?) as usize;
    let count = u64::from_le_bytes(buf[24..32].try_into().ok()?) as usize;
    let flags = u64::from_le_bytes(buf[32..40].try_into().ok()?);
    let dir_blob_len = u64::from_le_bytes(buf[40..48].try_into().ok()?) as usize;
    let dir_count = u64::from_le_bytes(buf[48..56].try_into().ok()?) as usize;
    if count > MAX_ENTRIES_CEIL
        || dir_count > MAX_ENTRIES_CEIL
        || HDR
            .checked_add(blob_len)?
            .checked_add(count * ITEM_BYTES)?
            .checked_add(dir_blob_len)?
            .checked_add(dir_count * DIR_BYTES)?
            != buf.len()
    {
        return None;
    }

    let blob_end = HDR + blob_len;
    let blob = String::from_utf8(buf[HDR..blob_end].to_vec()).ok()?;
    let items_end = blob_end + count * ITEM_BYTES;
    let raw = &buf[blob_end..items_end];
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

    let dir_blob_end = items_end + dir_blob_len;
    let dir_blob = String::from_utf8(buf[items_end..dir_blob_end].to_vec()).ok()?;
    let mut dirs = Vec::with_capacity(dir_count);
    for chunk in buf[dir_blob_end..].chunks_exact(DIR_BYTES) {
        let rec = DirRec {
            path_off: u32::from_le_bytes(chunk[0..4].try_into().ok()?),
            path_len: u32::from_le_bytes(chunk[4..8].try_into().ok()?),
            mtime: u64::from_le_bytes(chunk[8..16].try_into().ok()?),
        };
        let (o, l) = (rec.path_off as usize, rec.path_len as usize);
        if !o
            .checked_add(l)
            .map_or(false, |end| end <= dir_blob.len())
            || !dir_blob.is_char_boundary(o)
            || !dir_blob.is_char_boundary(o + l)
        {
            return None;
        }
        dirs.push(rec);
    }

    Some(Index {
        blob,
        items,
        dir_blob,
        dirs,
        built_unix,
        complete: flags & FLAG_COMPLETE != 0,
        incomplete_dirs: flags & FLAG_INCOMPLETE_DIRS != 0,
    })
}

fn save_cache(index: &Index) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut buf = Vec::with_capacity(
        HDR + index.blob.len() + index.items.len() * ITEM_BYTES + index.dir_blob.len() + index.dirs.len() * DIR_BYTES,
    );
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&index.built_unix.to_le_bytes());
    buf.extend_from_slice(&(index.blob.len() as u64).to_le_bytes());
    buf.extend_from_slice(&(index.items.len() as u64).to_le_bytes());
    let mut flags = if index.complete { FLAG_COMPLETE } else { 0 };
    if index.incomplete_dirs {
        flags |= FLAG_INCOMPLETE_DIRS;
    }
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&(index.dir_blob.len() as u64).to_le_bytes());
    buf.extend_from_slice(&(index.dirs.len() as u64).to_le_bytes());
    buf.extend_from_slice(index.blob.as_bytes());
    for it in &index.items {
        buf.extend_from_slice(&it.path_off.to_le_bytes());
        buf.extend_from_slice(&it.path_len.to_le_bytes());
        buf.extend_from_slice(&it.name_off.to_le_bytes());
        buf.extend_from_slice(&it.name_len.to_le_bytes());
        buf.extend_from_slice(&it.size.to_le_bytes());
        buf.extend_from_slice(&it.mtime.to_le_bytes());
    }
    buf.extend_from_slice(index.dir_blob.as_bytes());
    for rec in &index.dirs {
        buf.extend_from_slice(&rec.path_off.to_le_bytes());
        buf.extend_from_slice(&rec.path_len.to_le_bytes());
        buf.extend_from_slice(&rec.mtime.to_le_bytes());
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
        return;
    }
    // 新缓存代表「此刻的文件系统」，之前那份日志描述的空窗已经归零
    if let Some(j) = journal_path() {
        let _ = std::fs::remove_file(j);
    }
}
