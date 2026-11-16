//! 平台系统集成。Windows 保留完整原生实现；其他平台使用明确的降级实现。

#[cfg(not(windows))]
mod fallback;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use fallback::*;
#[cfg(windows)]
pub use windows::*;

/// 给本地磁盘标识补搜索根分隔符；Unix 根目录 `/` 不应变成 `//`。
pub fn drive_root(drive: &str) -> String {
    if drive.ends_with('/') || drive.ends_with('\\') {
        drive.to_string()
    } else {
        format!("{drive}/")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn drive_root_preserves_existing_separator() {
        assert_eq!(super::drive_root("/"), "/");
        assert_eq!(super::drive_root("/Volumes/Data/"), "/Volumes/Data/");
        assert_eq!(super::drive_root("C:"), "C:/");
    }
}
