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
        lowest_valid: data.LowestValidUsn,
    })
}

/// `start_usn` 这个起点还能不能用来做增量。
/// journal_id 变了（日志被重建）或起点早于 lowest_valid（中间被截断）都不行。
pub fn can_resume(info: &JournalInfo, expected_id: u64, start_usn: i64) -> bool {
    info.journal_id == expected_id && start_usn >= info.lowest_valid && start_usn <= info.next_usn
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_rules() {        let info = JournalInfo { journal_id: 7, next_usn: 100, lowest_valid: 10 };
        assert!(can_resume(&info, 7, 50));
        assert!(!can_resume(&info, 8, 50), "日志被重建时必须回退");
        assert!(!can_resume(&info, 7, 5), "起点早于 lowest_valid 必须回退");
        assert!(!can_resume(&info, 7, 101), "起点超过 next_usn 不可信，必须回退");
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
        let start = (info.next_usn - 2000).max(info.lowest_valid);
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
