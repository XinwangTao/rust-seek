//! NTFS USN 变更日志：只用来回答「上次确认之后，这块盘上哪些东西变了」。
//!
//! 设计见 docs/usn_incremental_plan.md 与 docs/archify/usn_incremental_workflow.json。
//! 这里的**全部**函数在失败时返回 None，调用方必须回退到「全盘目录 stat 巡」——
//! 不允许用不完整的信息下结论。
//!
//! 读 USN 需要卷句柄，即管理员权限；普通权限下 `open_volume` 失败，一切自动回退。

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};

/// 一块盘的日志状态。`next_usn` 是当前最新一条之后的位置，`lowest_valid`
/// 是日志里最老还保留下来的位置：`start_usn < lowest_valid` 说明中间被截断，
/// 增量不可信，必须回退全盘巡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub next_usn: i64,
    pub lowest_valid: i64,
}

/// 感兴趣的变化：只看「名字/存在性」变了的事件。内容改写（DATA_OVERWRITE）不改变索引。
const REASON_MASK: u32 = 0x0000_0100   // FILE_CREATE
    | 0x0000_0200                       // FILE_DELETE
    | 0x0000_1000                       // RENAME_OLD_NAME
    | 0x0000_2000; // RENAME_NEW_NAME

/// 一次增量里最多解析这么多条记录。超过就认为「变化太多，全盘巡更划算」，
/// 直接放弃增量——对应设计里「不确定就回退」的原则。
const MAX_RECORDS: usize = 2_000_000;

/// 一次读日志的缓冲区。USN 记录很小，1MB 能装几千条。
const READ_BUF_BYTES: usize = 1 << 20;

/// 打开卷句柄（`\\.\D:`）。读 USN 需要管理员权限，普通权限会失败 → None。
fn open_volume(drive: char) -> Option<HANDLE> {
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
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(h)
    }
}

fn query_journal(h: HANDLE) -> Option<JournalInfo> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};
    use windows_sys::Win32::System::IO::DeviceIoControl;

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
    if ok == 0 {
        return None;
    }
    Some(JournalInfo {
        journal_id: data.UsnJournalID,
        next_usn: data.NextUsn,
        lowest_valid: data.LowestValidUsn,
    })
}

/// 查询某个卷的 USN 日志状态。非 NTFS / 没有日志 / 没权限 都返回 None。
pub fn journal_info(drive: char) -> Option<JournalInfo> {
    let h = open_volume(drive)?;
    let info = query_journal(h);
    unsafe { CloseHandle(h) };
    info
}

/// `start_usn` 这个起点还能不能用来做增量。
/// journal_id 变了（日志被重建）或起点早于 lowest_valid（中间被截断）都不行。
pub fn can_resume(info: &JournalInfo, expected_id: u64, start_usn: i64) -> bool {
    info.journal_id == expected_id && start_usn >= info.lowest_valid && start_usn <= info.next_usn
}

// ---------- 读记录 ----------

/// 一条 USN 记录里我们需要的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// 被改动条目自己的 FRN（重命名时要用来定位新位置，这里先保留备用）
    pub frn: u64,
    /// 父目录 FRN —— 索引只需要知道「哪个目录脏了」
    pub parent_frn: u64,
    pub reason: u32,
}

/// 解析 `FSCTL_READ_USN_JOURNAL` 的返回缓冲：前 8 字节是下一个 USN，其后是记录串。
/// 只认 V2/V3 记录；其它版本跳过（不认识的格式宁可少收，也不猜）。
fn parse_records(buf: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut off = 8usize;
    while off + 60 <= buf.len() {
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if len < 60 || off + len > buf.len() {
            break;
        }
        let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
        let frn = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
        let parent = u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap());
        let reason = u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap());
        // V2 用 64 位 FRN；V3 是 128 位，同一个字段解析方式不同，跳过。
        if major == 2 {
            out.push(Record { frn, parent_frn: parent, reason });
        }
        off += len;
    }
    out
}

/// 从 `start_usn` 读到日志末尾，返回（记录, 读完后的 next_usn）。
fn read_from(h: HANDLE, journal_id: u64, start_usn: i64) -> Option<(Vec<Record>, i64)> {
    use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let mut buf = vec![0u8; READ_BUF_BYTES];
    let mut records: Vec<Record> = Vec::new();
    let mut cursor = start_usn;
    loop {
        let data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: cursor,
            ReasonMask: REASON_MASK,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_READ_USN_JOURNAL,
                &data as *const _ as *mut core::ffi::c_void,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        if returned < 8 {
            return Some((records, cursor));
        }
        let next = i64::from_le_bytes(buf[0..8].try_into().ok()?);
        let batch = parse_records(&buf[..returned as usize]);
        // 没有记录且游标不再前进 = 已到末尾
        if batch.is_empty() && next <= cursor {
            return Some((records, cursor));
        }
        records.extend(batch);
        if records.len() > MAX_RECORDS {
            return None;
        }
        if next <= cursor {
            return Some((records, cursor));
        }
        cursor = next;
    }
}

/// FRN → 完整路径（正斜杠，无尾斜杠）。目录已删除 / 无权限时返回 None。
fn path_of_frn(volume: HANDLE, frn: u64) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdType, GetFinalPathNameByHandleW, OpenFileById, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    desc.Type = FileIdType;
    desc.Anonymous = FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 };

    let h = unsafe {
        OpenFileById(
            volume,
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut out = vec![0u16; 1024];
    let n = unsafe { GetFinalPathNameByHandleW(h, out.as_mut_ptr(), out.len() as u32, 0) };
    unsafe { CloseHandle(h) };
    if n == 0 || n as usize >= out.len() {
        return None;
    }
    let raw = String::from_utf16_lossy(&out[..n as usize]);
    // `\\?\D:\a\b` 或 `\\?\UNC\...`：去掉前缀，统一成正斜杠、无尾斜杠
    let path = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    Some(normalize(path))
}

/// 统一成索引里的键格式：`D:/a/b`（盘符大写、无尾斜杠）。
fn normalize(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    if let Some(rest) = p.strip_prefix("//?/") {
        p = rest.to_string();
    }
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let head = p[..1].to_ascii_uppercase();
        p.replace_range(..1, &head);
    }
    // 盘根统一成 `D:/`（与监听线程的键一致）：裸 `D:` 当枚举根会被当成
    // 「C 盘当前目录下的 D:」，既扫不到东西，又会把整盘条目误判成已删除。
    if p.len() == 2 && p.as_bytes()[1] == b':' {
        p.push('/');
    }
    while p.len() > 3 && p.ends_with('/') {
        p.pop();
    }
    p
}

// ---------- 快照状态 ----------

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct VolState {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// 本次处理到哪：由调用方在全盘巡 / 增量成功后提交。
#[derive(Debug, Clone, Copy)]
pub struct Commit {
    pub drive: char,
    pub journal_id: u64,
    pub next_usn: i64,
}

fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("rust-seek").join("usn-state.json"))
}

fn load_state() -> HashMap<String, VolState> {
    let Some(path) = state_path() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_state(map: &HashMap<String, VolState>) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_vec(map) else { return };
    // 原子写：先写临时文件再 rename，避免别的进程读到半截 JSON
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn key_of(drive: char) -> String {
    format!("{}:", drive.to_ascii_uppercase())
}

/// 本进程能不能读卷日志（= 有没有管理员权限 / 是否 NTFS）。
/// 只用于状态提示，不用来判断增量是否可信。
pub fn available(drives: &[String]) -> bool {
    drives
        .iter()
        .filter_map(|d| d.chars().next())
        .any(|ch| journal_info(ch).is_some())
}

/// 有没有卷缺「增量起点」。缺就得建一次索引把它种下来，否则永远只能全盘巡。
pub fn needs_seed(drives: &[String]) -> bool {
    let state = load_state();
    drives
        .iter()
        .filter_map(|d| d.chars().next())
        .any(|ch| !state.contains_key(&key_of(ch)))
}

/// 快查：这次**大概**能不能走增量（有状态、日志能打开、起点还能续）。
/// 只用于决定「值不值得同步试一次」，不保证一定成功——真正的判定在
/// [`dirty_dirs_since_snapshot`] 里逐条做，失败仍会回退全盘巡。
pub fn can_attempt(drives: &[String]) -> bool {
    let state = load_state();
    if state.is_empty() || drives.is_empty() {
        return false;
    }
    drives.iter().all(|d| {
        let Some(ch) = d.chars().next() else { return false };
        let Some(st) = state.get(&key_of(ch)) else { return false };
        journal_info(ch).map_or(false, |i| can_resume(&i, st.journal_id, st.next_usn))
    })
}

/// 用 USN 算「上次快照之后哪些目录变过」。
///
/// 返回 `Some((脏目录, 待提交快照))` 表示增量**完整可信**；
/// `None` 表示不可用（没权限 / 非 NTFS / 日志重建 / 起点被截断 / 有目录解析不出路径），
/// 调用方必须回退全盘巡。
pub fn dirty_dirs_since_snapshot(drives: &[String]) -> Option<(Vec<String>, Vec<Commit>)> {
    let state = load_state();
    if state.is_empty() {
        return None;
    }
    if drives.is_empty() {
        return None;
    }

    // 先把每块盘的增量都算出来：任何一块盘不可信，整体就不可信。
    let mut dirty: HashSet<String> = HashSet::new();
    let mut commits: Vec<Commit> = Vec::new();
    for drive_str in drives {
        let Some(ch) = drive_str.chars().next() else { return None };
        let st = state.get(&key_of(ch))?;
        let h = open_volume(ch)?;
        let outcome = (|| {
            let info = query_journal(h)?;
            if !can_resume(&info, st.journal_id, st.next_usn) {
                return None;
            }
            let (records, next_usn) = read_from(h, info.journal_id, st.next_usn)?;
            // 父目录去重后逐个解析成路径
            let parents: HashSet<u64> = records.iter().map(|r| r.parent_frn).collect();
            let mut dirs: Vec<String> = Vec::with_capacity(parents.len());
            for frn in parents {
                dirs.push(path_of_frn(h, frn)?);
            }
            Some((dirs, Commit { drive: ch, journal_id: info.journal_id, next_usn }))
        })();
        unsafe { CloseHandle(h) };
        let (dirs, commit) = outcome?;
        dirty.extend(dirs);
        commits.push(commit);
    }
    let mut dirty: Vec<String> = dirty.into_iter().collect();
    dirty.sort_unstable();
    Some((dirty, commits))
}

/// 全盘巡**开始前**调用：记下当前日志位置。巡完成功后把这份快照提交，
/// 下次启动就能走增量。拿不到日志（无权限/非 NTFS）时返回空，调用方照常提交空列表。
pub fn snapshot_before_sweep(drives: &[String]) -> Vec<Commit> {
    let mut out = Vec::new();
    for drive_str in drives {
        let Some(ch) = drive_str.chars().next() else { continue };
        let Some(info) = journal_info(ch) else { continue };
        out.push(Commit { drive: ch, journal_id: info.journal_id, next_usn: info.next_usn });
    }
    out
}

/// 提交快照：只覆盖本次真的处理过的卷，其它卷保持原值。
pub fn commit(commits: &[Commit]) {
    if commits.is_empty() {
        return;
    }
    let mut map = load_state();
    for c in commits {
        map.insert(key_of(c.drive), VolState { journal_id: c.journal_id, next_usn: c.next_usn });
    }
    save_state(&map);
}

/// 增量不可信时把某块盘的快照作废，保证下一次必然全盘巡。
pub fn invalidate(drives: &[String]) {
    let mut map = load_state();
    let mut changed = false;
    for d in drives {
        if let Some(ch) = d.chars().next() {
            map.remove(&key_of(ch));
            changed = true;
        }
    }
    if changed {
        save_state(&map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_rules() {
        let info = JournalInfo { journal_id: 7, next_usn: 100, lowest_valid: 10 };
        assert!(can_resume(&info, 7, 50));
        assert!(!can_resume(&info, 8, 50), "日志被重建时必须回退");
        assert!(!can_resume(&info, 7, 5), "起点早于 lowest_valid 必须回退");
        assert!(!can_resume(&info, 7, 101), "起点超过 next_usn 不可信，必须回退");
    }

    #[test]
    fn parse_skips_v3_and_stops_on_bad_length() {
        // 一条 V2 记录：长度 64，父目录 FRN = 0x22
        let mut buf = vec![0u8; 8]; // next_usn
        let mut rec = vec![0u8; 64];
        rec[0..4].copy_from_slice(&64u32.to_le_bytes());
        rec[4..6].copy_from_slice(&2u16.to_le_bytes());
        rec[8..16].copy_from_slice(&0x11u64.to_le_bytes());
        rec[16..24].copy_from_slice(&0x22u64.to_le_bytes());
        rec[40..44].copy_from_slice(&REASON_MASK.to_le_bytes());
        buf.extend_from_slice(&rec);
        let got = parse_records(&buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].parent_frn, 0x22);

        // 长度字段为 0：必须停下，不能死循环
        let mut bad = vec![0u8; 8];
        bad.extend_from_slice(&[0u8; 64]);
        assert!(parse_records(&bad).is_empty());
    }

    #[test]
    fn normalize_drive_and_separators() {
        assert_eq!(normalize(r"\\?\D:\a\b\"), "D:/a/b");
        assert_eq!(normalize(r"\\?\c:\"), "C:/");
        assert_eq!(normalize("D:"), "D:/");
        assert_eq!(normalize("D:/x"), "D:/x");
    }

    #[test]
    fn empty_state_means_no_incremental() {
        // 状态文件不存在时必须是 None（回退全盘巡），不能返回「零脏目录」
        let drives = vec!["Z:".to_string()];
        assert!(dirty_dirs_since_snapshot(&drives).is_none());
    }

    /// 实机探针（默认忽略）：提交快照 → 再算增量，验证整条链路能跑通。
    /// 运行：cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_usn_roundtrip() {
        let drives = vec!["C:".to_string(), "D:".to_string()];
        let snap = snapshot_before_sweep(&drives);
        println!("snapshot commits: {}", snap.len());
        commit(&snap);
        println!(
            "state file: {:?}",
            state_path().map(|p| (p.clone(), p.exists()))
        );
        let dirty = dirty_dirs_since_snapshot(&drives);
        match &dirty {
            Some((dirs, c)) => println!("dirty dirs: {} commits: {}", dirs.len(), c.len()),
            None => println!("dirty: None（不可增量）"),
        }
    }

    /// 实机探针（默认忽略）：需要管理员权限才能读卷。
    /// 运行：cargo test --release -- --ignored --nocapture
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
