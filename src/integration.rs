//! 系统集成：全局热键、托盘常驻、开机自启、任务栏搜索框开关、底部停靠定位、单实例保活。
//!
//! Windows 不允许第三方替换任务栏搜索框本身，这里采用与 Everything / uTools
//! 相同的方案：热键呼出停靠在任务栏上方的搜索窗 + 可选隐藏原生搜索框（记录原值可恢复）。
//!
//! 关键约束：winit 在窗口隐藏后不再派发重绘，egui 事件轮询会停摆。
//! 因此热键/托盘/跨实例唤出都直接用 Win32 显示窗口；窗口可见后 egui 帧自动恢复，
//! 再由 App 消费 `Bridge::pending_show` 做清屏与聚焦。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::thread;
use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
use winreg::RegKey;

/// 呼出模式（底栏停靠）下的窗口尺寸
pub const LAUNCHER_W: f32 = 720.0;
pub const LAUNCHER_H: f32 = 560.0;

/// App 与事件处理器（热键/托盘/唤出线程）之间共享的极小状态。
/// 处理器直接用 Win32 切换窗口可见性，仅在可见后由 App 帧消费 pending_show。
pub struct Bridge {
    /// 主窗口 HWND（App 首帧发现后写入；0 = 尚未就绪）
    pub hwnd: AtomicIsize,
    /// 是否为系统式面板模式（无边框 + 工具窗口），跟随设置
    pub panel: AtomicBool,
    /// 外部（热键/托盘/唤出）刚把窗口显示出来，App 需清空查询并聚焦输入框
    pub pending_show: AtomicBool,
    /// 剪切板热键/托盘刚请求打开「剪切板」面板（App 帧内消费并切换模式）
    pub clip_pending: AtomicBool,
    /// 暂停记录剪切板（托盘与面板共用；监听线程按此跳过入库）
    pub clip_paused: Arc<AtomicBool>,
}

impl Bridge {
    pub fn new(clip_paused: Arc<AtomicBool>) -> Self {
        Self {
            hwnd: AtomicIsize::new(0),
            panel: AtomicBool::new(true),
            pending_show: AtomicBool::new(false),
            clip_pending: AtomicBool::new(false),
            clip_paused,
        }
    }

    /// 呼出：先套用面板形态再显示，保证第一帧就是 Shell 形态。
    /// 返回呼出后的可见状态（窗口未就绪时返回 false）。
    pub fn show_panel(&self) -> bool {
        let hwnd = self.hwnd.load(Ordering::Relaxed);
        if hwnd == 0 {
            return false;
        }
        ensure_panel_style(hwnd, self.panel.load(Ordering::Relaxed));
        win_show_launcher(hwnd);
        self.pending_show.store(true, Ordering::Relaxed);
        true
    }

    /// 切换显示/隐藏（热键、托盘左键、托盘菜单）。
    pub fn toggle(&self) -> bool {
        let hwnd = self.hwnd.load(Ordering::Relaxed);
        if hwnd == 0 {
            return false;
        }
        if win_is_visible(hwnd) {
            win_hide(hwnd);
            false
        } else {
            self.show_panel()
        }
    }
}

// ---------- Windows 原生声明（免额外依赖） ----------

#[repr(C)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

extern "system" {
    fn SystemParametersInfoW(action: u32, param: u32, pvparam: *mut core::ffi::c_void, winini: u32) -> i32;
    fn CreateMutexW(attr: *mut core::ffi::c_void, owner: i32, name: *const u16) -> *mut core::ffi::c_void;
    fn CreateEventW(attr: *mut core::ffi::c_void, manual: i32, initial: i32, name: *const u16) -> *mut core::ffi::c_void;
    fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut core::ffi::c_void;
    fn SetEvent(h: *mut core::ffi::c_void) -> i32;
    fn WaitForSingleObject(h: *mut core::ffi::c_void, ms: u32) -> u32;
    fn GetLastError() -> u32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
    fn SetFocus(hwnd: isize) -> isize;
    fn BringWindowToTop(hwnd: isize) -> i32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
    fn EnumWindows(cb: Option<extern "system" fn(isize, isize) -> i32>, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
    fn SetWindowLongW(hwnd: isize, index: i32, value: i32) -> i32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
}

const SPI_GETWORKAREA: u32 = 0x0048;
const ERROR_ALREADY_EXISTS: u32 = 183;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const INFINITE: u32 = 0xFFFF_FFFF;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SW_HIDE: i32 = 0;
const SW_RESTORE: i32 = 9;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_FRAMECHANGED: u32 = 0x0020;
const SWP_SHOWWINDOW: u32 = 0x0040;
const HWND_TOPMOST: isize = -1;
const HWND_NOTOPMOST: isize = -2;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const VK_MENU: u8 = 0x12;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------- 窗口显示 / 隐藏（可从任意线程调用） ----------

/// 主显示器工作区（扣除任务栏）内、底边居中的窗口左上角坐标。
/// 优先 SPI_GETWORKAREA；受限环境（服务会话等）下降级为全屏减去任务栏估高。
pub fn bottom_center_position(width: f32, height: f32) -> Option<(f32, f32)> {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rc as *mut RECT as *mut _, 0) };
    let (area_w, area_h) = if ok != 0 && rc.right > rc.left && rc.bottom > rc.top {
        ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32)
    } else {
        // SPI 失败（如服务会话）：SM_CXSCREEN/SM_CYSCREEN，预留任务栏估高 48px
        let (cx, cy) = unsafe { (GetSystemMetrics(0), GetSystemMetrics(1)) };
        if cx <= 0 || cy <= 0 {
            return None;
        }
        ((cx as f32).max(width), ((cy - 48) as f32).max(height))
    };
    let x = area_w / 2.0 - width / 2.0;
    let y = area_h - height;
    Some((x.max(0.0), y.max(0.0)))
}

pub fn win_is_visible(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(hwnd) != 0 }
}

/// 当前前台窗口 HWND（失焦判定用：比 winit 的 focused 事件在 Win32 直呼出路径下更可靠）。
pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow() }
}

/// 以底栏停靠姿态显示主窗口并抢焦点。
///
/// Windows 前台锁（foreground lock）会拦下后台进程的 `SetForegroundWindow`，导致热键唤出时
/// 窗口只闪烁任务栏、不浮到前台。这里用业界标准组合拳：恢复+置顶+显示 → AttachThreadInput
/// 借用前台线程输入状态 → 抢前台 → 取消置顶；若仍被前台锁拒绝，再用 Alt 键兜底换取前台权限。
/// 注意不能带 SWP_NOZORDER，否则 HWND_TOPMOST 会被忽略。
pub fn win_show_launcher(hwnd: isize) {
    let pos = bottom_center_position(LAUNCHER_W, LAUNCHER_H);
    let (x, y) = pos.map(|(x, y)| (x as i32, y as i32)).unwrap_or((0, 0));
    unsafe {
        // 1) 若最小化先恢复，再置顶 + 显示 + 定位到停靠位
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            LAUNCHER_W as i32,
            LAUNCHER_H as i32,
            SWP_SHOWWINDOW,
        );

        // 2) 把本线程输入队列并入前台线程，绕过前台锁的“最后输入”判定
        let fg = GetForegroundWindow();
        let fg_thread = if fg != 0 {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        } else {
            0
        };
        let this_thread = GetCurrentThreadId();
        let attached = fg_thread != 0
            && fg_thread != this_thread
            && AttachThreadInput(this_thread, fg_thread, 1) != 0;

        // 3) 抢前台并聚焦
        let fg_ok = SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        SetFocus(hwnd);

        // 4) 前台锁拒绝时用 Alt 键兜底：模拟一次真实键盘输入换取前台权限
        if fg_ok == 0 {
            keybd_event(VK_MENU, 0, 0, 0);
            keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
        }

        if attached {
            AttachThreadInput(this_thread, fg_thread, 0);
        }

        // 5) 取消置顶，避免面板常驻最上层
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    }
}

pub fn win_hide(hwnd: isize) {
    unsafe { ShowWindow(hwnd, SW_HIDE); }
}

/// 设置/清除 WS_EX_TOOLWINDOW：工具窗口不出现在任务栏和 Alt+Tab，
/// 让呼出的搜索面板表现为 Shell 的一部分而非一个应用。
pub fn win_set_tool_window(hwnd: isize, on: bool) {
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOOLWINDOW: i32 = 0x0080;
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let next = if on { style | WS_EX_TOOLWINDOW } else { style & !WS_EX_TOOLWINDOW };
        if next != style {
            SetWindowLongW(hwnd, GWL_EXSTYLE, next);
        }
    }
}

/// 直接改 Win32 样式去除/恢复标题栏。
/// 不走 winit 的 set_decorations：实测其在部分环境下不生效（样式被保留）。
pub fn win_set_frameless(hwnd: isize, frameless: bool) {
    const GWL_STYLE: i32 = -16;
    const WS_CAPTION: i32 = 0x00C0_0000;
    const WS_THICKFRAME: i32 = 0x0004_0000;
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let next = if frameless {
            style & !(WS_CAPTION | WS_THICKFRAME)
        } else {
            style | WS_CAPTION | WS_THICKFRAME
        };
        if next != style {
            SetWindowLongW(hwnd, GWL_STYLE, next);
            // 通知系统非客户区已变化，立即重绘边框
            SetWindowPos(hwnd, 0, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
    }
}

/// 停靠模式的外观是否已就位（无边框 + 工具窗口），不符则自动修复。
/// winit/系统可能在某些时机重设样式，低频校正保证面板始终是“Shell 形态”。
pub fn ensure_panel_style(hwnd: isize, panel_mode: bool) {
    win_set_frameless(hwnd, panel_mode);
    win_set_tool_window(hwnd, panel_mode);
}

/// 按进程 + 标题找到主窗口 HWND（App 首帧调用一次）。
pub fn find_main_hwnd() -> isize {
    static RESULT: AtomicIsize = AtomicIsize::new(0);
    extern "system" fn cb(hwnd: isize, _l: isize) -> i32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == std::process::id() {
            let mut buf = [0u16; 64];
            let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), 64) };
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if title == "Rust Seek" {
                RESULT.store(hwnd, Ordering::Relaxed);
                return 0;
            }
        }
        1
    }
    RESULT.store(0, Ordering::Relaxed);
    unsafe { EnumWindows(Some(cb), 0) };
    RESULT.load(Ordering::Relaxed)
}

// ---------- 磁盘枚举 ----------

/// 可搜索的本地磁盘，如 ["C:", "D:", "F:"]。
/// 只取固定盘与可移动盘，跳过网络盘/光驱/内存盘——整机搜索时它们要么极慢要么本就该排除。
pub fn local_drives() -> Vec<String> {
    local_drives_detailed().into_iter().map(|(d, _)| d).collect()
}

/// 同 [`local_drives`]，附带类型（"fixed" / "removable"），供 MCP 的 list_drives 使用。
pub fn local_drives_detailed() -> Vec<(String, &'static str)> {
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let w = wide(&root);
        let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
        if kind == DRIVE_FIXED {
            out.push((format!("{letter}:"), "fixed"));
        } else if kind == DRIVE_REMOVABLE {
            out.push((format!("{letter}:"), "removable"));
        }
    }
    out
}

// ---------- 单实例保活 ----------

const SHOW_EVENT: &str = "Local\\RustSeek.ShowEvent";
const MUTEX: &str = "Local\\RustSeek.SingleInstance";

/// 已有实例在运行时通知其呼出窗口并返回 true；否则创建互斥体并返回 false（成为主实例）。
pub fn notify_existing_or_claim() -> bool {
    unsafe {
        let name = wide(MUTEX);
        let h = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if !h.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let ev = wide(SHOW_EVENT);
            let eh = OpenEventW(EVENT_MODIFY_STATE, 0, ev.as_ptr());
            if !eh.is_null() {
                SetEvent(eh);
            }
            return true;
        }
        false
    }
}

/// 主实例后台线程：其他实例发来“呼出”事件时呼出主窗口。
/// 窗口隐藏、或可见但失焦/被遮挡时都置顶抢焦点；仅当窗口已在前台时不动，
/// 避免误触发 pending_show 清空用户正在输入的查询。
pub fn spawn_show_watcher(bridge: Arc<Bridge>) {
    thread::spawn(move || unsafe {
        let ev = wide(SHOW_EVENT);
        let h = CreateEventW(std::ptr::null_mut(), 0, 0, ev.as_ptr());
        if h.is_null() {
            return;
        }
        loop {
            if WaitForSingleObject(h, INFINITE) != 0 {
                return;
            }
            let hwnd = bridge.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 && (!win_is_visible(hwnd) || foreground_window() != hwnd) {
                bridge.show_panel();
            }
        }
    });
}

// ---------- 全局热键 ----------

/// 热键预设（显示名, 热键）。
pub fn hotkey_presets() -> Vec<(&'static str, global_hotkey::hotkey::HotKey)> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    vec![
        ("Alt+Space", HotKey::new(Some(Modifiers::ALT), Code::Space)),
        ("Ctrl+Alt+Space", HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)),
        ("Ctrl+Alt+S", HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyS)),
    ]
}

pub struct HotkeySetup {
    /// 持有管理器保证热键注册不失效（Drop 即注销）
    _manager: global_hotkey::GlobalHotKeyManager,
    pub label: &'static str,
    pub win_s_ok: bool,
    /// 剪切板历史热键实际生效的组合（注册失败时为 None）
    pub clip_label: Option<&'static str>,
}

/// 注册全局热键：用户预设 + 尝试抢占 Win+S（Windows 自带搜索的热键，成功即完成热键层面接管）。
/// 按下时直接用 Win32 切换主窗口（隐藏窗口下 egui 轮询不可用）。
/// 另外注册剪切板历史热键：默认 Ctrl+Shift+V，被占用时降级 Ctrl+Alt+V。
pub fn setup_hotkey(preset_index: usize, bridge: Arc<Bridge>) -> Result<HotkeySetup, String> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    let presets = hotkey_presets();
    let (label, hk) = presets
        .get(preset_index)
        .ok_or("无效的热键预设")?
        .clone();
    let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
    manager
        .register(hk)
        .map_err(|e| format!("热键 {label} 注册失败（可能被其他程序占用）：{e}"))?;

    // 尝试接管 Win+S（系统搜索热键）。若已被占用（搜索服务先注册）则静默降级。
    let win_s = HotKey::new(Some(Modifiers::META), Code::KeyS);
    let win_s_ok = manager.register(win_s).is_ok();

    // 剪切板历史：Ctrl+Shift+V（被占用时降级 Ctrl+Alt+V）
    let clip_presets: [(&'static str, HotKey); 2] = [
        ("Ctrl+Shift+V", HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyV)),
        ("Ctrl+Alt+V", HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyV)),
    ];
    let mut clip_label: Option<&'static str> = None;
    let mut clip_id: Option<u32> = None;
    for (name, hotkey) in clip_presets {
        if manager.register(hotkey).is_ok() {
            clip_label = Some(name);
            clip_id = Some(hotkey.id());
            break;
        }
    }

    let preset_id = hk.id();
    let win_s_id = win_s.id();
    GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
        if ev.state != HotKeyState::Pressed {
            return;
        }
        if ev.id == preset_id || (win_s_ok && ev.id == win_s_id) {
            bridge.toggle();
        } else if clip_id == Some(ev.id) {
            // 剪切板面板总是「呼出」而不是切换：避免按一次热键就把面板关掉。
            // 注意顺序：先 show_panel（置 pending_show）再置 clip_pending——
            // 反过来的话 App 可能先消费 clip_pending，再被稍后到达的 pending_show 切回搜索视图。
            bridge.show_panel();
            bridge.clip_pending.store(true, Ordering::Relaxed);
        }
    }));
    Ok(HotkeySetup { _manager: manager, label, win_s_ok, clip_label })
}

// ---------- 托盘 ----------

/// muda 菜单项为 Rc 型（非 Send），但其事件处理器固定在主线程（WndProc）执行，
/// 用该包装把“仅在主线程使用”的约定编码进类型，避免跨线程误用。
/// 注意：必须通过 [`MainThread::with`] 访问内层值——闭包体直接写 `.0` 会触发
/// edition 2021 的精确字段捕获，令包装失效。
struct MainThread<T>(T);
unsafe impl<T> Send for MainThread<T> {}
unsafe impl<T> Sync for MainThread<T> {}

impl<T> MainThread<T> {
    fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0)
    }
}

pub struct TraySetup {
    _tray: tray_icon::TrayIcon,
    pub show_item: tray_icon::menu::MenuItem,
    pub autostart_item: tray_icon::menu::CheckMenuItem,
    pub hide_search_item: tray_icon::menu::CheckMenuItem,
    #[allow(dead_code)]
    pub clip_item: tray_icon::menu::MenuItem,
    pub clip_pause_item: tray_icon::menu::CheckMenuItem,
}

pub fn setup_tray(hotkey_label: &str, clip_label: Option<&str>, bridge: Arc<Bridge>) -> Result<TraySetup, String> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    let menu = Menu::new();
    let show_item = MenuItem::new(format!("显示 / 隐藏 ({hotkey_label})"), true, None);
    let _ = menu.append(&show_item);
    let clip_item = MenuItem::new(
        format!("剪切板历史 ({})", clip_label.unwrap_or("热键未注册")),
        true,
        None,
    );
    let _ = menu.append(&clip_item);
    let clip_pause_item = CheckMenuItem::new(
        "暂停记录剪切板",
        true,
        bridge.clip_paused.load(Ordering::Relaxed),
        None,
    );
    let _ = menu.append(&clip_pause_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let autostart_item = CheckMenuItem::new("开机自启", true, autostart_enabled(), None);
    let _ = menu.append(&autostart_item);
    let hide_search_item = CheckMenuItem::new(
        "隐藏任务栏搜索框（用 Rust Seek 替代）",
        true,
        taskbar_search_hidden(),
        None,
    );
    let _ = menu.append(&hide_search_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let quit_item = MenuItem::new("退出", true, None);
    let _ = menu.append(&quit_item);

    let icon = tray_icon::Icon::from_rgba(tray_rgba(), 32, 32).map_err(|e| e.to_string())?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Rust Seek — 文件/文本搜索")
        .with_icon(icon)
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| e.to_string())?;

    let b = bridge.clone();
    let m_show_id = show_item.id().clone();
    let m_clip_id = clip_item.id().clone();
    let m_quit_id = quit_item.id().clone();
    let m_autostart = MainThread(autostart_item.clone());
    let m_hide_search = MainThread(hide_search_item.clone());
    let m_clip_pause = MainThread(clip_pause_item.clone());
    let b_pause = bridge.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |ev: tray_icon::menu::MenuEvent| {
        if ev.id == m_show_id {
            b.toggle();
        } else if ev.id == m_clip_id {
            // 同热键：先显示窗口（置 pending_show）再请求剪切板视图，避免被切回搜索
            b.show_panel();
            b.clip_pending.store(true, Ordering::Relaxed);
        } else if m_clip_pause.with(|i| ev.id == i.id()) {
            let next = !b_pause.clip_paused.load(Ordering::Relaxed);
            b_pause.clip_paused.store(next, Ordering::Relaxed);
            m_clip_pause.with(|i| i.set_checked(next));
        } else if m_autostart.with(|i| ev.id == i.id()) {
            let next = !autostart_enabled();
            if set_autostart(next).is_ok() {
                m_autostart.with(|i| i.set_checked(next));
            }
        } else if m_hide_search.with(|i| ev.id == i.id()) {
            if taskbar_search_hidden() {
                restore_taskbar_search(None);
                m_hide_search.with(|i| i.set_checked(false));
            } else {
                hide_taskbar_search();
                m_hide_search.with(|i| i.set_checked(true));
            }
        } else if ev.id == m_quit_id {
            std::process::exit(0);
        }
    }));
    let b2 = bridge;
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = ev
        {
            b2.toggle();
        }
    }));

    Ok(TraySetup {
        _tray: tray,
        show_item,
        autostart_item,
        hide_search_item,
        clip_item,
        clip_pause_item,
    })
}

/// 程序化绘制 32×32 放大镜托盘图标（避免引入 image 依赖）。
fn tray_rgba() -> Vec<u8> {
    const S: usize = 32;
    const SS: u32 = 3; // 3×3 超采样抗锯齿
    let mut px = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let mut cov = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    // 镜片圆环：圆心 (13,13)，半径 8，环宽 ~1.6
                    let d = ((fx - 13.0).powi(2) + (fy - 13.0).powi(2)).sqrt();
                    let in_ring = (d - 8.0).abs() <= 1.6;
                    // 手柄：线段 (19,19)-(25.5,25.5)，半径 1.7
                    let (ax, ay, bx, by) = (19.0f32, 19.0f32, 25.5f32, 25.5f32);
                    let (dx, dy) = (bx - ax, by - ay);
                    let t = (((fx - ax) * dx + (fy - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                    let (qx, qy) = (ax + t * dx, ay + t * dy);
                    let in_handle = ((fx - qx).powi(2) + (fy - qy).powi(2)).sqrt() <= 1.7;
                    if in_ring || in_handle {
                        cov += 1;
                    }
                }
            }
            let a = (cov * 255 / (SS * SS)) as u8;
            if a > 0 {
                let i = (y * S + x) * 4;
                px[i] = 86;
                px[i + 1] = 164;
                px[i + 2] = 255;
                px[i + 3] = a;
            }
        }
    }
    px
}

// ---------- 注册表：开机自启 / 任务栏搜索框 ----------

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "RustSeek";
const SEARCH_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Search";
const SEARCH_VALUE: &str = "SearchboxTaskbarMode";

// —— 设置 → 隐私和安全性 → 搜索 对应的注册表 ——

/// 禁用开始/搜索框的 Bing 网络结果（Explorer 策略键）。
const POLICY_SUBKEY: &str = r"Software\Policies\Microsoft\Windows\Explorer";
const BING_VALUE: &str = "DisableSearchBoxSuggestions";
/// 搜索突出显示（SearchSettings）。
const SEARCH_SETTINGS_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\SearchSettings";
const HIGHLIGHTS_VALUE: &str = "IsDynamicSearchBoxEnabled";

fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(subkey)
        .and_then(|k| k.get_value::<u32, _>(value))
        .ok()
}

fn write_dword(subkey: &str, value: &str, data: u32) -> std::io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(subkey)?;
    key.set_value(value, &data)
}

fn delete_dword(subkey: &str, value: &str) {
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(subkey, KEY_SET_VALUE) {
        let _ = key.delete_value(value);
    }
}

/// Bing 网络结果当前是否被禁用。
pub fn bing_disabled() -> bool {
    read_dword(POLICY_SUBKEY, BING_VALUE) == Some(1)
}

pub fn set_bing_disabled(on: bool) -> std::io::Result<()> {
    if on {
        write_dword(POLICY_SUBKEY, BING_VALUE, 1)
    } else {
        delete_dword(POLICY_SUBKEY, BING_VALUE);
        Ok(())
    }
}

/// 搜索突出显示当前是否开启（值缺省 = 开启）。
pub fn highlights_enabled() -> bool {
    read_dword(SEARCH_SETTINGS_SUBKEY, HIGHLIGHTS_VALUE) != Some(0)
}

pub fn set_highlights_enabled(on: bool) -> std::io::Result<()> {
    write_dword(SEARCH_SETTINGS_SUBKEY, HIGHLIGHTS_VALUE, if on { 1 } else { 0 })
}

pub fn autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_SUBKEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE | KEY_QUERY_VALUE)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\" --hidden", exe.display()))?;
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
    Ok(())
}

/// 当前任务栏搜索框是否被隐藏（SearchboxTaskbarMode == 0）。
pub fn taskbar_search_hidden() -> bool {
    taskbar_search_mode() == Some(0)
}

fn taskbar_search_mode() -> Option<u32> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(SEARCH_SUBKEY)
        .and_then(|k| k.get_value::<u32, _>(SEARCH_VALUE))
        .ok()
}

/// 隐藏任务栏搜索框，返回覆盖前的原值（None = 原先没有该值），供恢复使用。
pub fn hide_taskbar_search() -> Option<Option<u32>> {
    let prev = taskbar_search_mode();
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(SEARCH_SUBKEY, KEY_SET_VALUE)
    {
        let _ = key.set_value(SEARCH_VALUE, &0u32);
    }
    Some(prev)
}

/// 恢复 `hide_taskbar_search` 记录的原值。
pub fn restore_taskbar_search(prev: Option<u32>) {
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(SEARCH_SUBKEY, KEY_SET_VALUE)
    {
        match prev {
            Some(v) => {
                let _ = key.set_value(SEARCH_VALUE, &v);
            }
            None => {
                let _ = key.delete_value(SEARCH_VALUE);
            }
        }
    }
}

/// 重启资源管理器使任务栏设置立即生效（会短暂黑屏并关闭资源管理器窗口）。
pub fn restart_explorer() {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("cmd")
        .args(["/C", "taskkill /F /IM explorer.exe & start explorer.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

