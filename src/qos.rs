//! 后台任务优先级：索引构建/刷新这类「可以慢、但不能抢前台」的线程，
//! 在 macOS 上降到后台 QoS + 磁盘 I/O throttle；在 Windows 上降到
//! 「后台处理模式」（CPU 最低 + I/O 优先级 Very Low + 内存优先级最低），
//! 并把后台并发压到「核数一半、2～8 条」、按目标速率给扫盘限速。
//!
//! 用户查询线程不受影响：它们最多降到「礼貌」档（CPU 次低），I/O 保持正常。
//!
//! 背景（2026-11-19）：Windows 侧之前是空实现，索引巡盘用满 16~32 条线程、
//! 按正常 I/O 优先级打盘，在机械盘 / 装了过滤驱动的机器上会把磁盘队列打满、
//! 整机卡死。这里补上 Windows 的后台档。

use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Foreground = 0,
    Polite = 1,
    Background = 2,
}

thread_local! {
    static LEVEL: Cell<Level> = const { Cell::new(Level::Foreground) };
}

/// 把**当前线程**降到后台档（每个线程只生效一次）。
pub fn set_background() {
    let changed = LEVEL.with(|l| {
        if l.get() >= Level::Background {
            false
        } else {
            l.set(Level::Background);
            true
        }
    });
    if changed {
        imp::apply(Level::Background);
    }
}

/// 把**当前线程**降到「礼貌」档：CPU 让给前台，I/O 仍按正常优先级。
/// 用于用户主动发起、但会整盘遍历的搜索——要快，但别把整机 I/O 队列压满。
pub fn set_polite() {
    let changed = LEVEL.with(|l| {
        if l.get() >= Level::Polite {
            false
        } else {
            l.set(Level::Polite);
            true
        }
    });
    if changed {
        imp::apply(Level::Polite);
    }
}

/// 当前线程是否已经降到后台档。后台线程里派生的遍历要继承这个身份
/// （遍历线程是新线程，`opts.background` 不一定带得过来）。
pub fn is_background() -> bool {
    LEVEL.with(|l| l.get()) == Level::Background
}

/// 后台扫盘的线程数上限。Windows 上按「核数一半、2～8 条」封顶：
/// 索引巡盘是随机元数据读，队列越深越挤占前台（机械盘上还会退化成寻道抖动），
/// 8 条以上吞吐基本不再涨，只是把磁盘队列堆得更深。
/// 可用 `RUST_SEEK_BG_THREADS` 覆盖（1～64）。
pub fn background_threads(desired: usize) -> usize {
    imp::background_threads(desired)
}

/// 前台搜索的线程数上限：16。再多的并发只会把磁盘队列堆深，吞吐基本不涨，
/// 前台应用反而更卡；16 条已经能跑满常见 SSD。
pub fn foreground_threads() -> usize {
    num_cpus::get().clamp(1, 16)
}

/// 后台遍历每枚举 `dirs` 个目录调用一次：按目标速率休眠，给前台留出磁盘。
/// 速率用 `RUST_SEEK_BG_DIRS_PER_SEC` 覆盖，0 表示不限速。
pub fn pace(dirs: u64) {
    imp::pace(dirs);
}

// ---------- Windows ----------

#[cfg(windows)]
mod imp {
    use super::Level;
    use std::cell::Cell;
    use std::sync::OnceLock;
    use std::time::Duration;
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadInformation, SetThreadPriority, ThreadMemoryPriority,
        MEMORY_PRIORITY_VERY_LOW, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_BELOW_NORMAL,
    };

    pub fn apply(level: Level) {
        unsafe {
            let h = GetCurrentThread();
            match level {
                // THREAD_MODE_BACKGROUND_BEGIN：CPU 调度优先级降到最低，
                // 同时把线程的 I/O 优先级降到 Very Low——磁盘调度器会先服务前台 I/O，
                // 索引巡盘再重也不会让整机卡住。
                Level::Background => {
                    SetThreadPriority(h, THREAD_MODE_BACKGROUND_BEGIN);
                    // 内存优先级也压到最低：后台扫出来的页缓存别把前台工作集挤出去
                    let prio: u32 = MEMORY_PRIORITY_VERY_LOW;
                    SetThreadInformation(
                        h,
                        ThreadMemoryPriority,
                        &prio as *const u32 as *const core::ffi::c_void,
                        std::mem::size_of::<u32>() as u32,
                    );
                }
                Level::Polite => {
                    SetThreadPriority(h, THREAD_PRIORITY_BELOW_NORMAL);
                }
                Level::Foreground => {}
            }
        }
    }

    pub fn background_threads(desired: usize) -> usize {
        static CAP: OnceLock<usize> = OnceLock::new();
        let cap = *CAP.get_or_init(|| {
            std::env::var("RUST_SEEK_BG_THREADS")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| (1..=64).contains(n))
                .unwrap_or_else(|| (num_cpus::get() / 2).clamp(2, 8))
        });
        desired.clamp(1, cap)
    }

    fn nanos_per_dir() -> u64 {
        static NS: OnceLock<u64> = OnceLock::new();
        *NS.get_or_init(|| {
            let per_sec = std::env::var("RUST_SEEK_BG_DIRS_PER_SEC")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(30_000);
            1_000_000_000u64.checked_div(per_sec).unwrap_or(0)
        })
    }

    pub fn pace(dirs: u64) {
        let ns = nanos_per_dir();
        if ns == 0 || dirs == 0 {
            return;
        }
        // 攒够 1ms 再睡：Windows 的定时器粒度是 ~15ms，逐目录睡反而更慢
        const SLICE_NS: u64 = 1_000_000;
        thread_local! {
            static DEBT_NS: Cell<u64> = const { Cell::new(0) };
        }
        let sleep_ms = DEBT_NS.with(|d| {
            let debt = d.get().saturating_add(dirs.saturating_mul(ns));
            if debt >= SLICE_NS {
                d.set(debt % SLICE_NS);
                debt / SLICE_NS
            } else {
                d.set(debt);
                0
            }
        });
        if sleep_ms > 0 {
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }
    }
}

// ---------- macOS ----------

#[cfg(target_os = "macos")]
mod imp {
    use super::Level;

    pub fn apply(level: Level) {
        if level != Level::Background {
            // macOS 的「礼貌」档没有单独的系统调用：CPU 调度本来就会先跑前台线程
            return;
        }
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
            // 再叠一层磁盘 I/O throttle：把本线程的 I/O 压到最低档
            setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
        }
    }

    pub fn background_threads(desired: usize) -> usize {
        desired
    }

    pub fn pace(_dirs: u64) {}

    extern "C" {
        fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
    }

    const IOPOL_TYPE_DISK: i32 = 0;
    const IOPOL_SCOPE_THREAD: i32 = 1;
    const IOPOL_THROTTLE: i32 = 3;
}

// ---------- 其它平台 ----------

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::Level;

    pub fn apply(_level: Level) {}

    pub fn background_threads(desired: usize) -> usize {
        desired
    }

    pub fn pace(_dirs: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_threads_never_returns_zero() {
        assert!(background_threads(0) >= 1);
        assert!(background_threads(1) >= 1);
    }

    #[test]
    fn background_threads_caps_on_windows() {
        let n = background_threads(1024);
        #[cfg(windows)]
        assert!((1..=64).contains(&n));
        assert!(n >= 1);
    }

    /// 实机校验：后台档必须真的把线程的 CPU 优先级压到最低。
    /// （I/O 优先级由 `THREAD_MODE_BACKGROUND_BEGIN` 一并降到 Very Low，未单独断言。）
    #[cfg(windows)]
    #[test]
    fn background_really_lowers_thread_priority() {
        let prio = std::thread::spawn(|| {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_LOWEST,
            };
            let before = unsafe { GetThreadPriority(GetCurrentThread()) };
            set_background();
            let after = unsafe { GetThreadPriority(GetCurrentThread()) };
            (before, after, THREAD_PRIORITY_LOWEST)
        })
        .join()
        .unwrap();
        let (before, after, lowest) = prio;
        assert!(before > lowest, "普通线程本应是普通优先级，实测 {before}");
        assert!(
            after <= lowest,
            "后台档没有降优先级：before={before} after={after} lowest={lowest}"
        );
    }

    #[test]
    fn level_transitions_are_monotonic() {
        std::thread::spawn(|| {
            assert!(!is_background());
            set_polite();
            assert!(!is_background());
            set_background();
            assert!(is_background());
        })
        .join()
        .unwrap();
    }
}
