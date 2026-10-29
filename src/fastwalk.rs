//! 并行目录遍历：Windows 上比 `ignore`/walkdir 快数倍。
//!
//! walkdir 每列一个目录要一次 `FindFirstFile`，之后每一条记录再一次 `FindNextFile`，
//! 一次系统调用只带回一条；这里用 `FindFirstFileExW` 配 `FIND_FIRST_EX_LARGE_FETCH`
//! （64KB 缓冲），一次调用批量带回几十条记录，文件名、大小、时间、属性全在返回结构里，
//! 不需要任何额外 stat。
//!
//! 语义对齐原来的 `ignore::WalkBuilder`：不跟随符号链接 / junction，可跳过隐藏项，
//! 回调返回 [`Step::Skip`] 整棵剪掉目录，[`Step::Quit`] 立即结束整次遍历。
//! 非 Windows 平台退回 `ignore`，行为一致、只是慢。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

/// 一条目录项的信息，全部来自目录枚举本身，没有额外系统调用。
pub struct Info {
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 相对遍历根的层数，根为 0
    pub depth: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Skip,
    Quit,
}

pub struct Options {
    /// 是否把隐藏项（Windows 隐藏属性 / 点开头）也报出来
    pub include_hidden: bool,
    pub threads: usize,
}

/// 遍历若干根目录，返回扫描到的条目数。
/// `cancel` 置位后各线程尽快退出；`visit` 返回 `Quit` 会立即结束整次遍历。
pub fn walk<F>(roots: &[PathBuf], opts: &Options, cancel: &AtomicBool, visit: F) -> u64
where
    F: Fn(&Path, &Info) -> Step + Sync,
{
    imp::walk(roots, opts, cancel, visit)
}

// ---------- Windows ----------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Condvar, Mutex};
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
        FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
    };

    const INVALID_HANDLE: isize = -1;

    struct State {
        queue: VecDeque<(PathBuf, usize)>,
        /// 队列中 + 正在枚举的目录数，归零即整次遍历结束
        pending: usize,
    }

    pub fn walk<F>(roots: &[PathBuf], opts: &Options, cancel: &AtomicBool, visit: F) -> u64
    where
        F: Fn(&Path, &Info) -> Step + Sync,
    {
        let jobs: VecDeque<(PathBuf, usize)> = roots
            .iter()
            .map(|r| (absolutize(r), 0))
            .collect();
        let state = Mutex::new(State { pending: jobs.len(), queue: jobs });
        let wake = Condvar::new();
        let stop = AtomicBool::new(false);
        let count = AtomicU64::new(0);
        let threads = opts.threads.max(1);

        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    // 取下一个目录；队列空且无人还在枚举，说明整次遍历结束
                    let (dir, depth) = {
                        let mut st = state.lock().unwrap();
                        loop {
                            if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                                stop.store(true, Ordering::Relaxed);
                                wake.notify_all();
                                return;
                            }
                            if let Some(job) = st.queue.pop_front() {
                                break job;
                            }
                            if st.pending == 0 {
                                wake.notify_all();
                                return;
                            }
                            st = wake.wait(st).unwrap();
                        }
                    };

                    let mut path = PathBuf::with_capacity(320);
                    let mut pattern: Vec<u16> = Vec::with_capacity(340);
                    let mut children: Vec<(PathBuf, usize)> = Vec::new();
                    enumerate(
                        &dir,
                        depth,
                        opts,
                        &mut path,
                        &mut pattern,
                        &mut children,
                        &count,
                        &stop,
                        &cancel,
                        &visit,
                    );

                    let mut st = state.lock().unwrap();
                    st.pending += children.len();
                    st.queue.extend(children.drain(..));
                    st.pending -= 1;
                    wake.notify_one();
                });
            }
        });

        count.load(Ordering::Relaxed)
    }

    /// 列一个目录，逐条回调；子目录塞进 `children` 交给队列。
    #[allow(clippy::too_many_arguments)]
    fn enumerate<F>(
        dir: &Path,
        depth: usize,
        opts: &Options,
        path: &mut PathBuf,
        pattern: &mut Vec<u16>,
        children: &mut Vec<(PathBuf, usize)>,
        count: &AtomicU64,
        stop: &AtomicBool,
        cancel: &AtomicBool,
        visit: &F,
    ) where
        F: Fn(&Path, &Info) -> Step,
    {
        build_pattern(dir, pattern);

        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                &mut data as *mut WIN32_FIND_DATAW as *mut _,
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if handle as isize == INVALID_HANDLE {
            return; // 无权限 / 目录不存在 / 空目录
        }

        // 路径前缀只拼一次，后面逐条 push 名字再 pop 回来
        path.clear();
        path.push(dir);

        loop {
            let name = first_nul(&data.cFileName);
            let dot = b'.' as u16;
            let is_dot = name.len() == 1 && name[0] == dot
                || name.len() == 2 && name[0] == dot && name[1] == dot;
            if !is_dot {
                let attrs = data.dwFileAttributes;
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                let hidden = attrs & FILE_ATTRIBUTE_HIDDEN != 0 || name.first() == Some(&(b'.' as u16));
                if opts.include_hidden || !hidden {
                    let os_name = OsString::from_wide(name);
                    path.push(&os_name);
                    let info = Info {
                        is_dir,
                        size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
                        modified: filetime(data.ftLastWriteTime.dwHighDateTime, data.ftLastWriteTime.dwLowDateTime),
                        depth: depth + 1,
                    };
                    count.fetch_add(1, Ordering::Relaxed);
                    let step = visit(path, &info);
                    let recurse = is_dir
                        && attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0
                        && step == Step::Continue;
                    if recurse {
                        children.push((path.clone(), depth + 1));
                    }
                    path.pop();
                    if step == Step::Quit {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }

            if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                break;
            }
            if unsafe { FindNextFileW(handle, &mut data) } == 0 {
                break;
            }
        }

        unsafe { FindClose(handle) };
    }

    /// 相对路径按当前工作目录补全，并消掉 `.` / `..`。
    /// `\\?\` 前缀下 Win32 不做路径归一化，`\\?\C:\a\..\b` 会直接失败，所以必须自己处理。
    fn absolutize(p: &Path) -> PathBuf {
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(c) => c.join(p),
                Err(_) => return p.to_path_buf(),
            }
        };
        let text = joined.to_string_lossy().replace('\\', "/");
        let unc = text.starts_with("//");
        let mut stack: Vec<&str> = Vec::new();
        for part in text.split('/') {
            match part {
                "" | "." => {}
                // 栈底可能是盘符（"C:"）或 UNC 的 server/share，别 pop 掉
                ".." => {
                    if stack.len() > if unc { 2 } else { 1 } {
                        stack.pop();
                    }
                }
                other => stack.push(other),
            }
        }
        let mut out = String::new();
        if unc {
            out.push_str("//");
        }
        out.push_str(&stack.join("/"));
        // 盘符根必须留尾部分隔符：`PathBuf::push` 对 "C:" 这种裸盘符不补分隔符，
        // 拼出来会变成 "C:Users"（盘符相对路径），`\\?\` 下直接失效
        if out.len() == 2 && out.ends_with(':') {
            out.push('/');
        }
        PathBuf::from(out)
    }

    /// `C:/a/b` → `\\?\C:\a\b\*`，UNC 走 `\\?\UNC\...`，避开 MAX_PATH。
    fn build_pattern(dir: &Path, out: &mut Vec<u16>) {
        let bs = b'\\' as u16;
        out.clear();
        out.extend_from_slice(&[bs, bs, b'?' as u16, bs]);

        let raw: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .map(|c| if c == b'/' as u16 { bs } else { c })
            .collect();
        if raw.starts_with(&[bs, bs]) {
            out.extend_from_slice(&[b'U' as u16, b'N' as u16, b'C' as u16, bs]);
            out.extend_from_slice(&raw[2..]);
        } else {
            out.extend_from_slice(&raw);
        }
        if out.last() != Some(&bs) {
            out.push(bs);
        }
        out.push(b'*' as u16);
        out.push(0);
    }

    fn first_nul(units: &[u16]) -> &[u16] {
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        &units[..end]
    }

    /// FILETIME（1601 起的 100ns）→ SystemTime
    fn filetime(high: u32, low: u32) -> Option<SystemTime> {
        const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
        let ticks = ((high as u64) << 32) | low as u64;
        if ticks == 0 {
            return None;
        }
        let secs = ticks / 10_000_000;
        if secs < EPOCH_DIFF_SECS {
            return None;
        }
        let nanos = ((ticks % 10_000_000) * 100) as u32;
        Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs - EPOCH_DIFF_SECS, nanos))
    }
}

// ---------- 其他平台：退回 ignore ----------

#[cfg(not(windows))]
mod imp {
    use super::*;
    use ignore::{WalkBuilder, WalkState};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    pub fn walk<F>(roots: &[PathBuf], opts: &Options, cancel: &AtomicBool, visit: F) -> u64
    where
        F: Fn(&Path, &Info) -> Step + Sync,
    {
        let mut builder = WalkBuilder::new(&roots[0]);
        for r in &roots[1..] {
            builder.add(r);
        }
        builder
            .hidden(!opts.include_hidden)
            .git_ignore(false)
            .ignore(false)
            .follow_links(false)
            .threads(opts.threads);

        let count = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let visit = Arc::new(visit);
        builder.build_parallel().run(|| {
            let count = count.clone();
            let stop = stop.clone();
            let visit = visit.clone();
            Box::new(move |entry| {
                if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                let meta = entry.metadata().ok();
                let info = Info {
                    is_dir: entry.file_type().map_or(false, |t| t.is_dir()),
                    size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    modified: meta.and_then(|m| m.modified().ok()),
                    depth: entry.depth(),
                };
                count.fetch_add(1, Ordering::Relaxed);
                match visit(entry.path(), &info) {
                    Step::Continue => WalkState::Continue,
                    Step::Skip => WalkState::Skip,
                    Step::Quit => {
                        stop.store(true, Ordering::Relaxed);
                        WalkState::Quit
                    }
                }
            })
        });
        count.load(Ordering::Relaxed)
    }
}
