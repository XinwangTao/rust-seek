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
