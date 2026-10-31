//! 剪切板历史：Win32 监听（AddClipboardFormatListener）+ 本地存储 + 回填。
//!
//! 设计要点（对应 Archify 图 `docs/archify/clipboard_history_workflow.html`）：
//! - 监听：隐藏消息窗收 `WM_CLIPBOARDUPDATE`，只看文本 / DIB 位图两种格式。
//! - 存储：`%LOCALAPPDATA%\rust-seek\clipboard\history.json` + `images\<id>.png`，
//!   文本 500 条 / 图片 200 张封顶，超出淘汰最旧；同内容去重并提到最前。
//! - 回填：只写回系统剪贴板（CF_UNICODETEXT / CF_DIB），不模拟粘贴。
//! - 暂停：`paused` 原子开关由托盘和面板共享，暂停时新复制不入库。
//!
//! 所有磁盘写入都在监听线程里完成，UI 只做快照读取，避免卡住帧循环。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// 文本条数上限
pub const CAP_TEXT: usize = 500;
/// 图片张数上限
pub const CAP_IMAGE: usize = 200;
/// 超过该字节数的复制内容不记录（避免把大文件/巨型表格塞进历史）
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// 超大位图不记录
const MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// 历史文件版本
const HISTORY_VERSION: u32 = 1;

// ---------- 对外数据类型 ----------

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    Image,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipEntry {
    pub id: u64,
    pub kind: ClipKind,
    /// 内容指纹（文本按字节、图片按 RGBA 字节），用于去重
    pub hash: u64,
    #[serde(default)]
    pub text: Option<String>,
    /// 相对 `images/` 的文件名
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    pub bytes: u64,
    pub created: i64,
}

/// UI 列表用的轻量快照（不含完整文本）
#[derive(Clone)]
pub struct ClipSummary {
    pub id: u64,
    pub kind: ClipKind,
    pub preview: String,
    pub image_path: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub created: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryFile {
    version: u32,
    entries: Vec<ClipEntry>,
}

// ---------- Win32 声明（沿用 integration.rs 的手写 extern 风格，不新增依赖） ----------

type Hwnd = isize;
type Hglobal = isize;

#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: Option<extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: isize,
    h_icon: isize,
    h_cursor: isize,
    hbr_background: isize,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: isize,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: isize,
        instance: isize,
        param: *mut core::ffi::c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn AddClipboardFormatListener(hwnd: Hwnd) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;

    fn OpenClipboard(hwnd: Hwnd) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn SetClipboardData(format: u32, data: Hglobal) -> isize;
    fn IsClipboardFormatAvailable(format: u32) -> i32;

    fn GlobalAlloc(flags: u32, bytes: usize) -> Hglobal;
    fn GlobalFree(handle: Hglobal) -> Hglobal;
    fn GlobalLock(handle: Hglobal) -> *mut core::ffi::c_void;
    fn GlobalUnlock(handle: Hglobal) -> i32;
    fn GlobalSize(handle: Hglobal) -> usize;
}

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const GMEM_MOVEABLE: u32 = 0x0002;
const WM_CLIPBOARDUPDATE: u32 = 0x031D;
/// `HWND_MESSAGE`（-3）：仅消息窗口，不出现在桌面/任务栏
const HWND_MESSAGE: isize = -3;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

// ---------- 剪贴板读写 ----------

enum RawClip {
    Text(String),
    Image { rgba: Vec<u8>, width: u32, height: u32 },
}

/// 打开剪贴板：别的程序可能瞬间占用，最多重试 10 次。
fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(0) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(15));
    }
    false
}

fn read_clipboard() -> Option<RawClip> {
    if !open_clipboard() {
        return None;
    }
    let out = unsafe { read_clipboard_locked() };
    unsafe {
        CloseClipboard();
    }
    out
}

unsafe fn read_clipboard_locked() -> Option<RawClip> {
    if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
        let h = GetClipboardData(CF_UNICODETEXT);
        if h != 0 {
            if let Some(text) = read_unicode_text(h) {
                return Some(RawClip::Text(text));
            }
        }
    }
    for format in [CF_DIB, CF_DIBV5] {
        if IsClipboardFormatAvailable(format) == 0 {
            continue;
        }
        let h = GetClipboardData(format);
        if h == 0 {
            continue;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            continue;
        }
        let got = dib_to_rgba(ptr as *const u8, GlobalSize(h));
        GlobalUnlock(h);
        if let Some((rgba, width, height)) = got {
            return Some(RawClip::Image { rgba, width, height });
        }
    }
    None
}

unsafe fn read_unicode_text(handle: Hglobal) -> Option<String> {
    let ptr = GlobalLock(handle);
    if ptr.is_null() {
        return None;
    }
    let len_u16 = GlobalSize(handle) / 2;
    let slice = std::slice::from_raw_parts(ptr as *const u16, len_u16);
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let text = String::from_utf16_lossy(&slice[..end]);
    GlobalUnlock(handle);
    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
        return None;
    }
    Some(text)
}

/// CF_DIB / CF_DIBV5 → RGBA（只支持 24/32 位 BI_RGB / BI_BITFIELDS，够覆盖截图与常见绘图软件）
unsafe fn dib_to_rgba(ptr: *const u8, len: usize) -> Option<(Vec<u8>, u32, u32)> {
    if ptr.is_null() || len < 40 {
        return None;
    }
    let hdr = std::ptr::read_unaligned(ptr as *const BitmapInfoHeader);
    if hdr.bi_size < 40 || hdr.bi_width <= 0 || hdr.bi_height == 0 {
        return None;
    }
    let bpp = hdr.bi_bit_count;
    if !((bpp == 24 || bpp == 32) && (hdr.bi_compression == BI_RGB || hdr.bi_compression == BI_BITFIELDS)) {
        return None;
    }
    let w = hdr.bi_width as u32;
    let h = hdr.bi_height.unsigned_abs();
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let top_down = hdr.bi_height < 0;
    let bytes_pp = (bpp / 8) as usize;
    let stride = ((w as usize * bytes_pp + 3) / 4) * 4;
    let mut offset = hdr.bi_size as usize;
    // 40 字节 BITMAPINFOHEADER + BI_BITFIELDS：3 个 DWORD 颜色掩码紧跟在头后面，
    // 像素起点是 bi_size + 12。掩码不是标准 0x00RRGGBB 布局时直接跳过——
    // 宁可不记录这张图，也不要静默存一张错位/错色的图。
    if hdr.bi_compression == BI_BITFIELDS && offset < 52 {
        if len < offset + 12 {
            return None;
        }
        let m = ptr.add(offset) as *const u32;
        let masks = (
            std::ptr::read_unaligned(m),
            std::ptr::read_unaligned(m.add(1)),
            std::ptr::read_unaligned(m.add(2)),
        );
        if masks != (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) {
            return None;
        }
        offset += 12;
    }
    if offset + stride * h as usize > len {
        return None;
    }
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let src_y = if top_down { y } else { h as usize - 1 - y };
        let src = ptr.add(offset + src_y * stride);
        for x in 0..w as usize {
            let s = src.add(x * bytes_pp);
            let b = *s;
            let g = *s.add(1);
            let r = *s.add(2);
            let a = if bytes_pp == 4 { *s.add(3) } else { 255 };
            let d = (y * w as usize + x) * 4;
            rgba[d] = r;
            rgba[d + 1] = g;
            rgba[d + 2] = b;
            rgba[d + 3] = a;
        }
    }
    // 32 位 DIB 的 alpha 通道经常全 0（不透明语义），统一按不透明处理
    if bytes_pp == 4 && rgba.chunks_exact(4).all(|p| p[3] == 0) {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Some((rgba, w, h))
}

/// 把文本写回系统剪贴板（不自动粘贴）。
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = utf16.len() * 2;
    unsafe {
        if !take_clipboard()? {
            return Err("剪贴板被其他程序占用".into());
        }
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h == 0 {
            CloseClipboard();
            return Err("内存分配失败".into());
        }
        let p = GlobalLock(h);
        if p.is_null() {
            GlobalFree(h);
            CloseClipboard();
            return Err("内存锁定失败".into());
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), p as *mut u16, utf16.len());
        GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT, h) == 0 {
            GlobalFree(h);
            CloseClipboard();
            return Err("写入剪贴板失败".into());
        }
        CloseClipboard();
    }
    Ok(())
}

/// 把 RGBA 位图写回系统剪贴板（CF_DIB，32 位自底向上）。
pub fn set_clipboard_image(rgba: &[u8], width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return Err("图片数据无效".into());
    }
    let pixel_bytes = width as usize * height as usize * 4;
    let total = 40 + pixel_bytes;
    let mut buf = vec![0u8; total];
    let hdr = BitmapInfoHeader {
        bi_size: 40,
        bi_width: width as i32,
        bi_height: height as i32, // 正数 = 自底向上
        bi_planes: 1,
        bi_bit_count: 32,
        bi_compression: BI_RGB,
        bi_size_image: pixel_bytes as u32,
        bi_x_pels_per_meter: 0,
        bi_y_pels_per_meter: 0,
        bi_clr_used: 0,
        bi_clr_important: 0,
    };
    unsafe {
        std::ptr::write_unaligned(buf.as_mut_ptr() as *mut BitmapInfoHeader, hdr);
    }
    for y in 0..height as usize {
        let src_y = height as usize - 1 - y; // DIB 自底向上
        for x in 0..width as usize {
            let s = (src_y * width as usize + x) * 4;
            let d = 40 + (y * width as usize + x) * 4;
            buf[d] = rgba[s + 2]; // B
            buf[d + 1] = rgba[s + 1]; // G
            buf[d + 2] = rgba[s]; // R
            buf[d + 3] = 255;
        }
    }
    unsafe {
        if !take_clipboard()? {
            return Err("剪贴板被其他程序占用".into());
        }
        let h = GlobalAlloc(GMEM_MOVEABLE, total);
        if h == 0 {
            CloseClipboard();
            return Err("内存分配失败".into());
        }
        let p = GlobalLock(h);
        if p.is_null() {
            GlobalFree(h);
            CloseClipboard();
            return Err("内存锁定失败".into());
        }
        std::ptr::copy_nonoverlapping(buf.as_ptr(), p as *mut u8, total);
        GlobalUnlock(h);
        if SetClipboardData(CF_DIB, h) == 0 {
            GlobalFree(h);
            CloseClipboard();
            return Err("写入剪贴板失败".into());
        }
        CloseClipboard();
    }
    Ok(())
}

/// 打开并清空剪贴板；调用方负责 CloseClipboard（成功时）。
unsafe fn take_clipboard() -> Result<bool, String> {
    if !open_clipboard() {
        return Ok(false);
    }
    if EmptyClipboard() == 0 {
        CloseClipboard();
        return Err("清空剪贴板失败".into());
    }
    Ok(true)
}

// ---------- 历史存储 ----------

pub struct Store {
    dir: PathBuf,
    entries: Vec<ClipEntry>,
    next_id: u64,
}

impl Store {
    pub fn open(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(dir.join("images"));
        let entries = std::fs::read_to_string(dir.join("history.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<HistoryFile>(&s).ok())
            .filter(|f| f.version == HISTORY_VERSION)
            .map(|f| f.entries)
            .unwrap_or_default();
        let next_id = entries.iter().map(|e| e.id + 1).max().unwrap_or(1);
        let mut store = Self { dir, entries, next_id: next_id.max(1) };
        store.enforce_caps();
        store
    }

    fn images_dir(&self) -> PathBuf {
        self.dir.join("images")
    }

    fn save(&self) {
        let file = HistoryFile { version: HISTORY_VERSION, entries: self.entries.clone() };
        let Ok(json) = serde_json::to_string(&file) else { return };
        let path = self.dir.join("history.json");
        let tmp = self.dir.join("history.json.tmp");
        let _ = std::fs::create_dir_all(&self.dir);
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn summaries(&self) -> Vec<ClipSummary> {
        self.entries
            .iter()
            .map(|e| ClipSummary {
                id: e.id,
                kind: e.kind,
                preview: match (&e.kind, &e.text) {
                    (ClipKind::Text, Some(t)) => {
                        let one_line: String = t
                            .chars()
                            .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
                            .take(160)
                            .collect();
                        one_line.trim().to_string()
                    }
                    (ClipKind::Image, _) => format!("图片 {}×{}", e.width, e.height),
                    _ => String::new(),
                },
                image_path: e.image.as_ref().map(|n| self.images_dir().join(n)),
                width: e.width,
                height: e.height,
                bytes: e.bytes,
                created: e.created,
            })
            .collect()
    }

    pub fn text_of(&self, id: u64) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.text.clone())
    }

    /// 记录一段文本：空串/超长跳过，同内容去重并提到最前。返回是否有变化。
    pub fn add_text(&mut self, text: String) -> bool {
        // 有些剪贴板同步工具（本机 flix/RelayDesk 实测）会在每次同步时重复前置 U+FEFF(BOM)，
        // 造成同一段文字被改写成 N 个只差 BOM 的版本。这里统一剥掉前导 BOM 再判重。
        let text = text.trim_start_matches('\u{feff}').to_string();
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return false;
        }
        let hash = hash_bytes(text.as_bytes());
        let now = chrono::Local::now().timestamp();
        // 已存在同内容：复用条目提到最前
        let mut existing: Option<ClipEntry> = None;
        if let Some(pos) = self.entries.iter().position(|e| e.kind == ClipKind::Text && e.hash == hash) {
            if pos == 0 {
                // 已经在最前：只刷新时间戳。这里不能 save()——剪贴板同步工具会反复
                // 重发同一段内容，每次全量重写 history.json 既无意义又卡 UI。
                self.entries[0].created = now;
                return false;
            }
            existing = Some(self.entries.remove(pos));
        }
        let id = existing.as_ref().map(|e| e.id).unwrap_or_else(|| self.alloc_id());
        let entry = ClipEntry {
            id,
            kind: ClipKind::Text,
            hash,
            text: Some(text.clone()),
            image: None,
            width: 0,
            height: 0,
            bytes: text.len() as u64,
            created: now,
        };
        self.entries.insert(0, entry);
        self.enforce_caps();
        self.save();
        true
    }

    /// 记录一张位图：写入 `images/<id>.png`，同内容去重并提到最前。
    pub fn add_image(&mut self, rgba: &[u8], width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
            return false;
        }
        if (width as u64) * (height as u64) > MAX_IMAGE_PIXELS {
            return false;
        }
        let hash = hash_bytes(rgba);
        let now = chrono::Local::now().timestamp();
        // 先只定位旧条目，不急 remove：写盘失败时旧条目必须原样留着，
        // 否则内存里少一条、磁盘 history.json 还有，两边不一致。
        let existing_pos = self
            .entries
            .iter()
            .position(|e| e.kind == ClipKind::Image && e.hash == hash);
        if existing_pos == Some(0) {
            self.entries[0].created = now;
            return false;
        }
        let existing_id = existing_pos.map(|p| self.entries[p].id);
        let existing_name = existing_pos.and_then(|p| self.entries[p].image.clone());
        let id = existing_id.unwrap_or_else(|| self.alloc_id());
        let name = match existing_name {
            Some(n) if self.images_dir().join(&n).exists() => n,
            _ => {
                let name = format!("{id}.png");
                let path = self.images_dir().join(&name);
                let _ = std::fs::create_dir_all(self.images_dir());
                if write_png(&path, rgba, width, height).is_err() {
                    return false;
                }
                name
            }
        };
        if let Some(p) = existing_pos {
            self.entries.remove(p);
        }
        let entry = ClipEntry {
            id,
            kind: ClipKind::Image,
            hash,
            text: None,
            image: Some(name),
            width,
            height,
            bytes: rgba.len() as u64,
            created: now,
        };
        self.entries.insert(0, entry);
        self.enforce_caps();
        self.save();
        true
    }

    pub fn delete(&mut self, id: u64) -> bool {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            self.remove_at(pos, true);
            self.save();
            true
        } else {
            false
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn remove_at(&mut self, pos: usize, delete_file: bool) {
        let entry = self.entries.remove(pos);
        if delete_file {
            if let Some(name) = entry.image {
                let _ = std::fs::remove_file(self.images_dir().join(name));
            }
        }
    }

    fn enforce_caps(&mut self) {
        for (kind, cap) in [(ClipKind::Text, CAP_TEXT), (ClipKind::Image, CAP_IMAGE)] {
            while self.entries.iter().filter(|e| e.kind == kind).count() > cap {
                if let Some(pos) = self.entries.iter().rposition(|e| e.kind == kind) {
                    self.remove_at(pos, true);
                } else {
                    break;
                }
            }
        }
    }
}

fn write_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(w, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(rgba).map_err(std::io::Error::other)?;
    Ok(())
}

/// 读取历史图片文件为 RGBA（面板缩略图 / 回填用）。
pub fn load_png_rgba(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    match info.color_type {
        png::ColorType::Rgba => Some((buf, w, h)),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((out, w, h))
        }
        _ => None,
    }
}

// ---------- 监听线程 ----------

/// 消息线程 → 工作线程的信号通道（wndproc 是裸函数指针，没有闭包状态）
static SIGNAL: OnceLock<Mutex<Sender<()>>> = OnceLock::new();

pub struct Listener {
    pub rx: Receiver<()>,
    pub store: Arc<Mutex<Store>>,
}

/// 启动剪贴板监听：隐藏消息窗收通知，工作线程读剪贴板并落盘。
pub fn spawn(dir: PathBuf, paused: Arc<AtomicBool>) -> Listener {
    let store = Arc::new(Mutex::new(Store::open(dir)));
    let (notify_tx, notify_rx) = mpsc::channel();
    let (signal_tx, signal_rx) = mpsc::channel::<()>();
    let _ = SIGNAL.set(Mutex::new(signal_tx));

    // 隐藏消息窗：只负责接收 WM_CLIPBOARDUPDATE
    thread::Builder::new()
        .name("clipboard-listener".into())
        .spawn(|| {
            unsafe {
                let class_name = wide("RustSeekClipboardListener");
                let instance = GetModuleHandleW(std::ptr::null());
                let class = WndClassExW {
                    cb_size: std::mem::size_of::<WndClassExW>() as u32,
                    style: 0,
                    lpfn_wnd_proc: Some(wndproc),
                    cb_cls_extra: 0,
                    cb_wnd_extra: 0,
                    h_instance: instance,
                    h_icon: 0,
                    h_cursor: 0,
                    hbr_background: 0,
                    lpsz_menu_name: std::ptr::null(),
                    lpsz_class_name: class_name.as_ptr(),
                    h_icon_sm: 0,
                };
                if RegisterClassExW(&class) == 0 {
                    return;
                }
                let title = wide("");
                let hwnd = CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    title.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    0,
                    instance,
                    std::ptr::null_mut(),
                );
                if hwnd == 0 || AddClipboardFormatListener(hwnd) == 0 {
                    return;
                }
                let mut msg = std::mem::zeroed::<Msg>();
                while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        })
        .ok();

    // 工作线程：读剪贴板 + PNG 编码 + 落盘，全部串行，避免多条内容交叉写
    let worker_store = store.clone();
    thread::Builder::new()
        .name("clipboard-worker".into())
        .spawn(move || {
            while signal_rx.recv().is_ok() {
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                // 剪贴板可能刚被写、格式还没就绪：短暂等待后读一次
                thread::sleep(Duration::from_millis(25));
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                let changed = match read_clipboard() {
                    Some(RawClip::Text(t)) => worker_store.lock().map(|mut s| s.add_text(t)).unwrap_or(false),
                    Some(RawClip::Image { rgba, width, height }) => worker_store
                        .lock()
                        .map(|mut s| s.add_image(&rgba, width, height))
                        .unwrap_or(false),
                    None => false,
                };
                if changed {
                    let _ = notify_tx.send(());
                }
            }
        })
        .ok();

    Listener { rx: notify_rx, store }
}

extern "system" fn wndproc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    if msg == WM_CLIPBOARDUPDATE {
        if let Some(sig) = SIGNAL.get() {
            if let Ok(tx) = sig.lock() {
                let _ = tx.send(());
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 缩略图降采样（最近邻）：把最长边压到 `max_side` 以内，避免把 4K 截图整张贴成纹理。
pub fn downscale_rgba(rgba: &[u8], width: u32, height: u32, max_side: u32) -> (Vec<u8>, u32, u32) {
    if width == 0 || height == 0 || max_side == 0 {
        return (rgba.to_vec(), width, height);
    }
    if width <= max_side && height <= max_side {
        return (rgba.to_vec(), width, height);
    }
    let scale = max_side as f32 / width.max(height) as f32;
    let nw = ((width as f32 * scale).round() as u32).max(1);
    let nh = ((height as f32 * scale).round() as u32).max(1);
    let mut out = vec![0u8; nw as usize * nh as usize * 4];
    for y in 0..nh as usize {
        let sy = (y * height as usize / nh as usize).min(height as usize - 1);
        for x in 0..nw as usize {
            let sx = (x * width as usize / nw as usize).min(width as usize - 1);
            let s = (sy * width as usize + sx) * 4;
            let d = (y * nw as usize + x) * 4;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (out, nw, nh)
}

/// 历史目录：`%LOCALAPPDATA%\rust-seek\clipboard`。
pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("rust-seek").join("clipboard"))
}
