//! NTFS USN 变更日志：只用来回答「上次确认之后，这块盘上哪些东西变了」。
//!
//! 设计见 docs/usn_incremental_plan.md。这里的**全部**函数在失败时返回 None，
//! 调用方必须回退到「全盘目录 stat 巡」—— 不允许用不完整的信息下结论。
//!
//! 本文件目前只实现第一块：开卷 + 查询日志状态。读记录与 FRN→路径在后续增量里加。

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// 一块盘的日志状态。`next_usn` 是当前最新一条之后的位置，`lowest_valid`
/// 是日志里最老还保留下来的位置：`start_usn < lowest_valid` 说明中间被截断，
/// 增量不可信，必须回退全盘巡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub next_usn: i64,
    /// 日志里最早还保留的位置。`start_usn < first_usn` 会被内核直接拒绝
    /// （ERROR_INVALID_PARAMETER）—— D 盘 32MB 日志回卷快，必须用它校验。
    pub first_usn: i64,
    pub lowest_valid: i64,
}

/// 打开卷句柄（`\\.\D:`）。读 USN 需要管理员权限，普通权限会失败 → None。
fn open_volume(drive: char) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let path = format!("\\\\.\\{}:", drive.to_ascii_uppercase());
    let wide: Vec<u16> = OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        None
    } else {
        Some(h)
    }
}

/// 查询某个卷的 USN 日志状态。非 NTFS / 没有日志 / 没权限 都返回 None。
pub fn journal_info(drive: char) -> Option<JournalInfo> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let h = open_volume(drive)?;
    let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut data as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    Some(JournalInfo {
        journal_id: data.UsnJournalID,
        next_usn: data.NextUsn,
        first_usn: data.FirstUsn,
        lowest_valid: data.LowestValidUsn,
    })
}

/// `start_usn` 这个起点还能不能用来做增量。
/// journal_id 变了（日志被重建）或起点早于 lowest_valid（中间被截断）都不行。
pub fn can_resume(info: &JournalInfo, expected_id: u64, start_usn: i64) -> bool {
    info.journal_id == expected_id && start_usn >= info.first_usn && start_usn <= info.next_usn
}

/// 一条原始变更记录。只保留定位「哪个目录脏了」需要的最小信息。
#[derive(Debug, Clone)]
pub struct RawRecord {
    /// 父目录的 file reference number
    pub parent_frn: u64,
    /// 文件名（不含目录）
    pub name: String,
    pub reason: u32,
}

/// 单次扫描最多读这么多条：超过就当作「变动太多」，调用方回退全盘巡。
const MAX_RECORDS: usize = 200_000;

/// 从 `start_usn` 之后读到日志末尾。
///
/// 返回 `(records, next_usn)`；`next_usn` 是下次续读的起点。
/// 以下任一情况返回 None（调用方必须回退全盘巡）：
/// - 没有权限 / 不是 NTFS / 没有日志
/// - `expected_id` 与当前日志 ID 不一致（日志被重建）
/// - `start_usn` 早于 `lowest_valid`（中间被截断）
/// - 遇到非 v2 记录（格式看不懂就不猜）
/// - 记录数超过 `MAX_RECORDS`
pub fn scan(drive: char, expected_id: u64, start_usn: i64) -> Option<(Vec<RawRecord>, i64)> {
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1,
        USN_JOURNAL_DATA_V0,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let h = open_volume(drive)?;
    let result = (|| {
        // 1) 取当前日志状态并校验能不能续
        let mut jd: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                &mut jd as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        let info = JournalInfo {
            journal_id: jd.UsnJournalID,
            next_usn: jd.NextUsn,
            first_usn: jd.FirstUsn,
            lowest_valid: jd.LowestValidUsn,
        };
        if !can_resume(&info, expected_id, start_usn) {
            return None;
        }

        // 2) 循环读记录
        let mut out: Vec<RawRecord> = Vec::new();
        let mut cursor = start_usn;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            // V1 显式声明「只要 v2 结构」：解析器按 v2 布局读，版本协商必须钉死。
            let input = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: cursor,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: expected_id,
                MinMajorVersion: 2,
                MaxMajorVersion: 2,
            };
            let mut ret: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    h,
                    FSCTL_READ_USN_JOURNAL,
                    &input as *const _ as *mut core::ffi::c_void,
                    std::mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || ret < 8 {
                let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                eprintln!("[usn] READ_USN_JOURNAL ok={ok} ret={ret} err={e} cursor={cursor}");
                return None;
            }
            let next = i64::from_le_bytes(buf[0..8].try_into().ok()?);
            let body = &buf[8..ret as usize];

            let mut off = 0usize;
            while off + 8 <= body.len() {
                let len = u32::from_le_bytes(body[off..off + 4].try_into().ok()?) as usize;
                if len < 60 || off + len > body.len() {
                    break;
                }
                let major = u16::from_le_bytes(body[off + 4..off + 6].try_into().ok()?);
                if major != 2 {
                    // v3/v4（128 位 FRN 等）不认识，整次作废，别猜
                    return None;
                }
                let parent = u64::from_le_bytes(body[off + 16..off + 24].try_into().ok()?);
                let reason = u32::from_le_bytes(body[off + 40..off + 44].try_into().ok()?);
                let name_len = u16::from_le_bytes(body[off + 56..off + 58].try_into().ok()?) as usize;
                let name_off = u16::from_le_bytes(body[off + 58..off + 60].try_into().ok()?) as usize;
                if name_off + name_len > len || name_len % 2 != 0 {
                    return None;
                }
                let units: Vec<u16> = body[off + name_off..off + name_off + name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                out.push(RawRecord {
                    parent_frn: parent,
                    name: String::from_utf16_lossy(&units),
                    reason,
                });
                if out.len() > MAX_RECORDS {
                    return None;
                }
                off += len;
            }

            if next == cursor {
                break; // 没有新记录
            }
            cursor = next;
        }
        Some((out, cursor))
    })();
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    result
}

/// 由 file reference number 反查完整路径（正斜杠形式，与索引口径一致）。
/// 任何一步失败都返回 None —— 调用方必须回退全盘巡，不许猜。
pub fn path_of_frn(drive: char, frn: u64) -> Option<String> {
    let vol = open_volume(drive)?;
    let out = path_of_frn_with(vol, frn);
    unsafe { windows_sys::Win32::Foundation::CloseHandle(vol) };
    out
}

/// 同 [`path_of_frn`]，但复用调用方已打开的卷句柄。
/// 批量反查时一条记录开一次卷代价太大（本机 5 万条），所以拆出这一层。
fn path_of_frn_with(
    vol: windows_sys::Win32::Foundation::HANDLE,
    frn: u64,
) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdType, GetFinalPathNameByHandleW, OpenFileById, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, VOLUME_NAME_DOS,
    };

    let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    desc.Type = FileIdType;
    desc.Anonymous = FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 };

    let fh = unsafe {
        OpenFileById(
            vol,
            &desc,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            0,
        )
    };
    if fh.is_null() || fh == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut buf = vec![0u16; 1024];
    let n = unsafe {
        GetFinalPathNameByHandleW(
            fh,
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    unsafe {
        CloseHandle(fh);
    }
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    let raw = String::from_utf16_lossy(&buf[..n as usize]);
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    Some(trimmed.replace('\\', "/"))
}

// ---------- 状态落盘（usn-state.json）----------
//
// 只回答一个问题：这份索引对应每块盘日志里的哪个位置。
//
// 文件是**跨进程的暖启动提示**，不是真源：进程内以内存游标为准，落盘失败
// 最多让下次多点重复工作，不会产生错误结果。`cache_unix` 记着它属于哪一份
// 磁盘缓存——对不上就整份作废（拿旧游标去增量刷一个别的索引 = 静默丢文件）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveCheckpoint {
    /// 日志 ID。存十六进制字符串而不是 u64：JSON 里超过 2^53 的整数会被解析器截断。
    pub journal_id: String,
    /// 下次续读的起点。
    pub next_usn: i64,
}

impl DriveCheckpoint {
    fn new(journal_id: u64, next_usn: i64) -> Self {
        Self {
            journal_id: format!("{journal_id:#018x}"),
            next_usn,
        }
    }

    fn parsed_id(&self) -> Option<u64> {
        let hex = self.journal_id.trim_start_matches("0x");
        u64::from_str_radix(hex, 16).ok()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsnState {
    /// 这份状态对应的磁盘缓存（= `Index::built_unix`）。0 表示没有可用缓存。
    #[serde(default)]
    pub cache_unix: u64,
    #[serde(default)]
    pub drives: BTreeMap<String, DriveCheckpoint>,
}

/// 本进程当前持有的游标，与内存里的索引严格同步推进。
/// `None` = 还不知道（首次全盘巡之前 / 缓存对不上），此时只能走全盘巡。
static CURSORS: Mutex<Option<BTreeMap<String, DriveCheckpoint>>> = Mutex::new(None);

fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("rust-seek").join("usn-state.json"))
}

fn load_from_disk() -> UsnState {
    state_file()
        .map(|p| load_at(&p))
        .unwrap_or_default()
}

fn load_at(path: &Path) -> UsnState {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// 原子写（临时文件 + rename）：多进程同时写不会读到半截 JSON。
fn save_to_disk(state: &UsnState) -> bool {
    let Some(path) = state_file() else {
        return false;
    };
    save_at(&path, state)
}

fn save_at(path: &Path, state: &UsnState) -> bool {
    let path = path.to_path_buf();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(buf) = serde_json::to_vec(state) else {
        return false;
    };
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

fn lock_cursors() -> std::sync::MutexGuard<'static, Option<BTreeMap<String, DriveCheckpoint>>> {
    CURSORS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 载入磁盘缓存后调用：只有状态文件记的正是这份缓存，才敢拿它的游标做增量。
pub fn adopt_state(cache_unix: u64) {
    let st = load_from_disk();
    let usable = state_usable(cache_unix, &st);
    *lock_cursors() = if usable { Some(st.drives) } else { None };
}

/// 状态文件能不能拿来续读：必须属于同一份缓存，且至少有一块盘有游标。
fn state_usable(cache_unix: u64, st: &UsnState) -> bool {
    cache_unix != 0 && st.cache_unix == cache_unix && !st.drives.is_empty()
}

fn drive_letter(drive: &str) -> Option<char> {
    let mut it = drive.chars();
    let c = it.next()?;
    if !c.is_ascii_alphabetic() {
        return None;
    }
    match it.next() {
        Some(':') | None => Some(c),
        _ => None,
    }
}

/// 反查出来的路径统一成索引里的目录键口径：正斜杠、不带结尾斜杠（盘根是 `D:`）。
fn normalize_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 一次成功的增量查询：脏目录 + 处理到哪了。
pub struct Incremental {
    pub dirty_dirs: Vec<String>,
    /// 与调用方传入的 `drives` 一一对应。
    pub checkpoints: Vec<DriveCheckpoint>,
}

/// 从本进程的游标读到日志末尾，返回「哪些目录脏了」。
///
/// 任何一块盘不满足条件（没有游标 / 日志重建 / 起点越界 / 读失败 / 单条 FRN
/// 解析失败）都整体返回 None —— 调用方必须回退全盘巡，不许拿半份结果充数。
pub fn incremental_since(drives: &[String]) -> Option<Incremental> {
    if drives.is_empty() {
        return None;
    }
    let cursors = lock_cursors().clone()?;

    let mut dirty: Vec<String> = Vec::new();
    let mut checkpoints: Vec<DriveCheckpoint> = Vec::with_capacity(drives.len());

    for drive in drives {
        let letter = drive_letter(drive)?;
        let cp = cursors.get(drive)?;
        let expected = cp.parsed_id()?;
        let info = journal_info(letter)?;
        if !can_resume(&info, expected, cp.next_usn) {
            return None;
        }
        let (records, next) = scan(letter, expected, cp.next_usn)?;

        // 同一条记录里的父目录大量重复（一次批量操作几十个文件同目录），
        // 先去重再开卷反查。
        let mut seen = std::collections::HashSet::new();
        let mut frns: Vec<u64> = Vec::new();
        for r in &records {
            if seen.insert(r.parent_frn) {
                frns.push(r.parent_frn);
            }
        }
        drop(records);

        let vol = open_volume(letter)?;
        let mut paths: Vec<String> = Vec::with_capacity(frns.len());
        let mut failed = false;
        for frn in frns {
            match path_of_frn_with(vol, frn).and_then(|p| normalize_dir(&p)) {
                Some(p) => paths.push(p),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        unsafe { windows_sys::Win32::Foundation::CloseHandle(vol) };
        if failed {
            return None;
        }
        dirty.append(&mut paths);
        checkpoints.push(DriveCheckpoint::new(expected, next));
    }

    dirty.sort_unstable();
    dirty.dedup();
    Some(Incremental {
        dirty_dirs: dirty,
        checkpoints,
    })
}

/// 增量刷新成功之后：推进本进程游标；`persist` 为真时才写状态文件。
///
/// 只有「缓存确实落盘了」才能 persist —— 否则状态文件会指向一份不存在的缓存，
/// 下一个进程对不上就直接全盘巡（安全，但白跑一趟）。
pub fn commit_snapshot(
    cache_unix: u64,
    drives: &[String],
    checkpoints: &[DriveCheckpoint],
    persist: bool,
) {
    if drives.len() != checkpoints.len() {
        return;
    }
    let map: BTreeMap<String, DriveCheckpoint> = drives
        .iter()
        .cloned()
        .zip(checkpoints.iter().cloned())
        .collect();
    *lock_cursors() = Some(map.clone());
    if persist {
        let _ = save_to_disk(&UsnState {
            cache_unix,
            drives: map,
        });
    }
}

/// 全盘巡成功之后：重新问一遍日志边界，把当前位置作为新游标。
///
/// 读不到日志的盘（没权限 / 非 NTFS）直接不记：下次那块盘只会再全巡一遍，
/// 不会拿一个猜出来的位置去续读。
pub fn commit_full_sweep(drives: &[String], cache_unix: u64, persist: bool) {
    let mut map = BTreeMap::new();
    for d in drives {
        let Some(letter) = drive_letter(d) else {
            continue;
        };
        let Some(info) = journal_info(letter) else {
            continue;
        };
        map.insert(
            d.clone(),
            DriveCheckpoint::new(info.journal_id, info.next_usn),
        );
    }
    let usable = !map.is_empty();
    *lock_cursors() = if usable { Some(map.clone()) } else { None };
    if persist && usable {
        let _ = save_to_disk(&UsnState {
            cache_unix,
            drives: map,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn resume_rules() {
        let info = JournalInfo { journal_id: 7, next_usn: 100, first_usn: 10, lowest_valid: 10 };
        assert!(can_resume(&info, 7, 50));
        assert!(!can_resume(&info, 8, 50), "日志被重建时必须回退");
        assert!(!can_resume(&info, 7, 5), "起点早于 lowest_valid 必须回退");
        assert!(!can_resume(&info, 7, 101), "起点超过 next_usn 不可信，必须回退");
    }

    #[test]
    fn journal_id_survives_json() {
        // 真实日志 ID 是 64 位，直接当 JSON 数字会被解析器按 f64 截断；
        // 存十六进制字符串才能原样回来。
        let raw = 0x01dd_6db4_68d4_884du64;
        let cp = DriveCheckpoint::new(raw, 24240);
        let text = serde_json::to_string(&cp).unwrap();
        let back: DriveCheckpoint = serde_json::from_str(&text).unwrap();
        assert_eq!(back.parsed_id(), Some(raw));
        assert_eq!(back.next_usn, 24240);
    }

    #[test]
    fn state_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("rust-seek-usn-test-{}", std::process::id()));
        let path = dir.join("usn-state.json");
        let raw = 0x01dd_6db4_68d4_884du64;
        let mut st = UsnState { cache_unix: 1_762_000_000, drives: BTreeMap::new() };
        st.drives.insert("D:".into(), DriveCheckpoint::new(raw, 24240));
        assert!(save_at(&path, &st));

        let back = load_at(&path);
        assert_eq!(back.cache_unix, st.cache_unix);
        assert_eq!(back.drives["D:"].parsed_id(), Some(raw));
        assert_eq!(back.drives["D:"].next_usn, 24240);

        // 半截/损坏的文件必须当成「没有状态」，而不是猜
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_at(&path).drives.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_must_match_cache() {
        let mut st = UsnState { cache_unix: 100, drives: BTreeMap::new() };
        assert!(!state_usable(100, &st), "没有游标也不算可用");
        st.drives.insert("D:".into(), DriveCheckpoint::new(7, 50));
        assert!(state_usable(100, &st));
        assert!(!state_usable(99, &st), "缓存换了就不能拿旧游标续读");
        assert!(!state_usable(0, &st), "没有缓存时不能用状态");
    }

    #[test]
    fn dir_key_normalisation() {
        assert_eq!(normalize_dir("D:/auto/"), Some("D:/auto".to_string()));
        assert_eq!(normalize_dir("D:/auto"), Some("D:/auto".to_string()));
        assert_eq!(normalize_dir("D:/"), Some("D:".to_string()));
        assert_eq!(normalize_dir(""), None);
        assert_eq!(drive_letter("D:"), Some('D'));
        assert_eq!(drive_letter("D"), Some('D'));
        assert_eq!(drive_letter("DD:"), None);
    }

    /// 实机端到端：记下游标 → 新建一个文件 → 增量必须精确报出那个目录。
    /// 需要管理员权限。运行：
    ///   cargo test live_incremental_probe -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_incremental_probe() {
        let drive = "D:".to_string();
        let Some(info) = journal_info('D') else {
            println!("D: 读不到日志（无管理员权限？）");
            return;
        };
        let before = DriveCheckpoint::new(info.journal_id, info.next_usn);
        commit_snapshot(1, std::slice::from_ref(&drive), std::slice::from_ref(&before), false);

        let dir = format!("D:/_rs_usn_probe_{}", std::process::id());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/a.txt"), b"probe").unwrap();

        let out = incremental_since(std::slice::from_ref(&drive));

        match out {
            Some(inc) => {
                let hit = inc.dirty_dirs.iter().any(|d| d == &dir);
                println!(
                    "dirty={} 命中新建目录={} next={}",
                    inc.dirty_dirs.len(),
                    hit,
                    inc.checkpoints.first().map_or(-1, |c| c.next_usn)
                );
                for d in inc.dirty_dirs.iter().take(3) {
                    println!("  {d} exists={}", std::path::Path::new(d).is_dir());
                }
                assert!(hit, "增量没报出刚新建的目录 —— 会静默丢文件");
                assert!(
                    inc.checkpoints[0].next_usn >= before.next_usn,
                    "游标不能倒退"
                );
            }
            None => panic!("增量返回 None（按设计应回退全盘巡，但这里是探针）"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 实机探针（默认忽略）：需要管理员权限才能读卷。
    /// 运行：cargo test --release -- --ignored --nocapture
    /// 实机探针（默认忽略）：需要管理员权限才能读卷。
    /// 运行：cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_scan_probe() {
        let info = match journal_info('D') {
            Some(i) => i,
            None => {
                println!("D: 读不到日志（无管理员权限？）");
                return;
            }
        };
        let start = info.first_usn;
        match scan('D', info.journal_id, start) {
            Some((recs, next)) => {
                println!("records={} next={}", recs.len(), next);
                for r in recs.iter().rev().take(3) {
                    println!("  {:?} reason={:#x} parent={:#x}", r.name, r.reason, r.parent_frn);
                }
            }
            None => println!("scan 返回 None（按设计应回退全盘巡）"),
        }
    }

    #[test]
    #[ignore]
    fn live_journal_probe() {
        for d in ['C', 'D'] {
            match journal_info(d) {
                Some(i) => println!(
                    "{d}: journal_id={:#x} next={} lowest={}",
                    i.journal_id, i.next_usn, i.lowest_valid
                ),
                None => println!("{d}: 读不到（无管理员权限 / 非 NTFS / 无日志）"),
            }
        }
    }
}
