//! 后台任务优先级：索引构建/刷新这类「可以慢、但不能抢前台」的线程，
//! 在 macOS 上降到后台 QoS + 磁盘 I/O throttle。用户查询线程不受影响。
//!
//! macOS 的 QoS 会同时影响 CPU 调度与 I/O 优先级；`setiopolicy_np(IOPOL_THROTTLE)`
//! 进一步把该线程的磁盘 I/O 压到最低档，避免索引巡盘把系统拖卡。

/// 把**当前线程**降到后台 QoS（每个线程只生效一次）。非 macOS 为空操作。
pub fn set_background() {
    #[cfg(target_os = "macos")]
    {
        use std::cell::Cell;
        thread_local! {
            static DONE: Cell<bool> = const { Cell::new(false) };
        }
        DONE.with(|done| {
            if done.replace(true) {
                return;
            }
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
                setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
            }
        });
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

#[cfg(target_os = "macos")]
const IOPOL_TYPE_DISK: i32 = 0;
#[cfg(target_os = "macos")]
const IOPOL_SCOPE_THREAD: i32 = 1;
#[cfg(target_os = "macos")]
const IOPOL_THROTTLE: i32 = 3;
