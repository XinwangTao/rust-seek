//! 非 Windows 的最小降级：保留搜索 GUI 所需接口，不伪装系统集成功能已生效。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;

pub const LAUNCHER_W: f32 = 720.0;
pub const LAUNCHER_H: f32 = 560.0;

pub struct Bridge {
    pub hwnd: AtomicIsize,
    pub panel: AtomicBool,
    pub pending_show: AtomicBool,
    pub clip_pending: AtomicBool,
    pub clip_paused: Arc<AtomicBool>,
}

impl Bridge {
    pub fn new(clip_paused: Arc<AtomicBool>) -> Self {
        Self {
            hwnd: AtomicIsize::new(0),
            panel: AtomicBool::new(false),
            pending_show: AtomicBool::new(false),
            clip_pending: AtomicBool::new(false),
            clip_paused,
        }
    }

    pub fn show_panel(&self) -> bool {
        self.pending_show.store(true, Ordering::Relaxed);
        false
    }

    pub fn toggle(&self) -> bool {
        false
    }
}

pub fn bottom_center_position() -> Option<(f32, f32)> {
    None
}

pub fn win_is_visible(_hwnd: isize) -> bool {
    false
}

pub fn foreground_window() -> isize {
    0
}

pub fn win_show_launcher(_hwnd: isize) {}
pub fn win_hide(_hwnd: isize) {}
pub fn win_set_tool_window(_hwnd: isize, _on: bool) {}
pub fn win_set_frameless(_hwnd: isize, _frameless: bool) {}
pub fn ensure_panel_style(_hwnd: isize, _panel_mode: bool) {}

pub fn find_main_hwnd() -> isize {
    0
}

/// Unix 的整机搜索根目录。
pub fn local_drives() -> Vec<String> {
    vec!["/".to_string()]
}

pub fn local_drives_detailed() -> Vec<(String, &'static str)> {
    vec![("/".to_string(), "fixed")]
}

pub fn notify_existing_or_claim() -> bool {
    false
}

pub fn release_single_instance() {}
pub fn spawn_show_watcher(_bridge: Arc<Bridge>) {}

pub fn hotkey_presets() -> Vec<(&'static str, global_hotkey::hotkey::HotKey)> {
    Vec::new()
}

pub struct HotkeySetup {
    pub label: &'static str,
    pub win_s_ok: bool,
    pub clip_label: Option<&'static str>,
}

pub fn setup_hotkey(_preset_index: usize, _bridge: Arc<Bridge>) -> Result<HotkeySetup, String> {
    Err("当前 Mac 版本暂未启用全局热键".into())
}

pub struct TraySetup {
    pub show_item: tray_icon::menu::MenuItem,
    pub autostart_item: tray_icon::menu::CheckMenuItem,
    pub hide_search_item: tray_icon::menu::CheckMenuItem,
    pub clip_item: tray_icon::menu::MenuItem,
    pub clip_pause_item: tray_icon::menu::CheckMenuItem,
}

pub fn setup_tray(
    _hotkey_label: &str,
    _clip_label: Option<&str>,
    _bridge: Arc<Bridge>,
) -> Result<TraySetup, String> {
    Err("当前 Mac 版本暂未启用系统托盘".into())
}

fn unsupported(feature: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("当前平台不支持{feature}"),
    ))
}

pub fn bing_disabled() -> bool {
    false
}

pub fn set_bing_disabled(_disabled: bool) -> io::Result<()> {
    unsupported("Bing 搜索设置")
}

pub fn highlights_enabled() -> bool {
    false
}

pub fn set_highlights_enabled(_enabled: bool) -> io::Result<()> {
    unsupported("Windows 搜索突出显示设置")
}

pub fn autostart_enabled() -> bool {
    false
}

pub fn set_autostart(_enabled: bool) -> io::Result<()> {
    unsupported("开机自启设置")
}

pub fn taskbar_search_hidden() -> bool {
    false
}

pub fn hide_taskbar_search() -> Option<bool> {
    None
}

pub fn restore_taskbar_search() {}
pub fn restart_explorer() {}
