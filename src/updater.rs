//! 自动更新：GitHub Releases → 系统弹窗确认 → 下载 + SHA-256 校验 → 替换 exe 并重启。
//!
//! 流程见 docs/archify/auto_update_workflow.html（L1，2026-11-04 已确认）：
//!   启动 30s 后查一次，之后每 6h；仅比较版本号，只有用户点「是」才下载；
//!   `--mcp` 进程不进入本模块（main.rs 在 MCP 分支已提前返回）。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/XinwangTao/rust-seek/releases/latest/download/latest.json";
const DEFAULT_FIRST_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 清单地址（可用 RUST_SEEK_UPDATE_MANIFEST 覆盖，便于本地联调）
fn manifest_url() -> String {
    std::env::var("RUST_SEEK_UPDATE_MANIFEST").unwrap_or_else(|_| DEFAULT_MANIFEST_URL.to_string())
}

/// 首查延迟（RUST_SEEK_UPDATE_FIRST_DELAY_SECS 可覆盖，便于本地联调）
fn first_delay() -> Duration {
    std::env::var("RUST_SEEK_UPDATE_FIRST_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_FIRST_DELAY)
}

/// 后台线程入口：清理上次遗留的 .old，延迟首查，之后定时循环。
pub fn spawn() {
    if std::env::var_os("RUST_SEEK_NO_UPDATE").is_some() {
        return;
    }
    let _ = std::thread::Builder::new().name("updater".into()).spawn(|| {
        cleanup_old();
        std::thread::sleep(first_delay());
        loop {
            if let Err(e) = check_once() {
                eprintln!("[updater] 检查更新失败: {e}");
            }
            std::thread::sleep(CHECK_INTERVAL);
        }
    });
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    assets: std::collections::HashMap<String, Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    url: String,
    sha256: String,
}

fn arch_key() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

fn check_once() -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let body = http_get(&manifest_url())?;
    let manifest: Manifest =
        serde_json::from_slice(&body).map_err(|e| format!("更新清单解析失败: {e}"))?;

    if !is_newer(&manifest.version, current) {
        return Ok(()); // 已是最新：静默等下一轮
    }
    if skipped_version().as_deref() == Some(manifest.version.as_str()) {
        return Ok(()); // 用户此前拒绝过这个版本
    }

    let mut prompt = format!(
        "发现新版本 v{}（当前 v{}）。\n\n是否现在下载并自动更新？\n程序会重启，未保存的搜索条件会丢失。",
        manifest.version, current
    );
    if let Some(n) = manifest.notes.as_ref().filter(|s| !s.trim().is_empty()) {
        let brief: String = n.chars().take(200).collect();
        prompt.push_str("\n\n更新说明：\n");
        prompt.push_str(&brief);
    }
    if !confirm(&prompt, "Rust Seek 更新") {
        save_skipped(&manifest.version);
        return Ok(());
    }

    let asset = manifest
        .assets
        .get(arch_key())
        .ok_or_else(|| format!("更新清单里没有 {} 架构的下载地址", arch_key()))?;

    let tmp = temp_exe_path(&manifest.version);
    download_verified(&asset.url, &asset.sha256, &tmp).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        info_box(&format!("更新失败：{e}\n\n当前版本仍可正常使用。"), "Rust Seek 更新");
        e
    })?;

    replace_and_restart(&tmp)
}

/// 下载到临时文件，边写边算 SHA-256；不一致则报错（不落盘安装）。
fn download_verified(url: &str, expect_sha: &str, dest: &Path) -> Result<(), String> {
    use sha2::Digest;
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    }
    let agent = agent();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("下载失败: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file = fs::File::create(dest).map_err(|e| format!("写入临时文件失败: {e}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0usize;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("下载中断: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入临时文件失败: {e}"))?;
        total += n;
    }
    file.sync_all().ok();
    if total < 1024 * 1024 {
        return Err(format!("下载内容过小（{total} 字节），疑似失败"));
    }
    let got = hex(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expect_sha.trim()) {
        return Err(format!("SHA-256 校验不符（期望 {expect_sha}，实际 {got}）"));
    }
    Ok(())
}

/// 旧 exe 改名 .old → 写入新 exe → 延迟拉起新进程 → 当前进程退出。
fn replace_and_restart(new_exe: &Path) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("定位当前程序失败: {e}"))?;
    let name = cur
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rust-seek.exe".into());
    let old = cur.with_file_name(format!("{name}.old"));
    let _ = fs::remove_file(&old); // 上次遗留的 .old 已不可删时忽略
    fs::rename(&cur, &old).map_err(|e| format!("旧程序改名失败（可能没有写入权限）: {e}"))?;
    if let Err(e) = fs::copy(new_exe, &cur) {
        let _ = fs::rename(&old, &cur); // 回滚，保证旧版仍可启动
        return Err(format!("写入新版本失败: {e}"));
    }
    let _ = fs::remove_file(new_exe);

    // 等本进程退出、单实例互斥体释放后再拉起（隐藏启动，回到托盘待命）
    let cmdline = format!(
        "ping -n 3 127.0.0.1 >nul & start \"\" \"{}\" --hidden",
        cur.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .arg("/C")
            .arg(cmdline)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    std::process::exit(0);
}

/// 启动时清理上一轮替换留下的 .old（此时旧进程已退出，可删）。
fn cleanup_old() {
    if let Ok(cur) = std::env::current_exe() {
        if let Some(name) = cur.file_name() {
            let old = cur.with_file_name(format!("{}.old", name.to_string_lossy()));
            if old.exists() {
                let _ = fs::remove_file(old);
            }
        }
    }
}

// ---------- 版本 / 状态 ----------

fn parse_ver(v: &str) -> Option<Vec<u32>> {
    let s = v.trim().trim_start_matches(['v', 'V']);
    let mut out = Vec::new();
    for part in s.split('.') {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        out.push(digits.parse().ok()?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn is_newer(remote: &str, local: &str) -> bool {
    let (Some(r), Some(l)) = (parse_ver(remote), parse_ver(local)) else {
        return false;
    };
    for i in 0..r.len().max(l.len()) {
        let a = r.get(i).copied().unwrap_or(0);
        let b = l.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

fn state_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("rust-seek").join("update-state.json"))
}

fn skipped_version() -> Option<String> {
    let p = state_path()?;
    let bytes = fs::read(p).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("skipped")?.as_str().map(|s| s.to_string())
}

fn save_skipped(version: &str) {
    let Some(p) = state_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let payload = serde_json::json!({ "skipped": version });
    let tmp = p.with_extension("json.tmp");
    if fs::write(&tmp, payload.to_string()).is_ok() && fs::rename(&tmp, &p).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn temp_exe_path(version: &str) -> PathBuf {
    std::env::temp_dir()
        .join("rust-seek-update")
        .join(format!("rust-seek-{version}.exe"))
}

// ---------- HTTP ----------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(HTTP_TIMEOUT)
        .timeout_write(HTTP_TIMEOUT)
        .user_agent(concat!("rust-seek/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let resp = agent().get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        other => other.to_string(),
    })?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(256 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------- Windows 弹窗 ----------

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 是 / 否 确认框；返回 true 表示用户同意更新。
/// RUST_SEEK_UPDATE_AUTO=1 时不弹窗直接视为同意（仅用于本地联调）。
#[cfg(windows)]
fn confirm(text: &str, title: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    if std::env::var_os("RUST_SEEK_UPDATE_AUTO").is_some() {
        eprintln!("[updater] 自动确认（联调模式）: {}", text.lines().next().unwrap_or(""));
        return true;
    }
    let t = wide(text);
    let c = wide(title);
    let r = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDYES
}

#[cfg(not(windows))]
fn confirm(_text: &str, _title: &str) -> bool {
    false
}

/// 只提示、不打断流程的信息框。
#[cfg(windows)]
fn info_box(text: &str, title: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let t = wide(text);
    let c = wide(title);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn info_box(text: &str, _title: &str) {
    eprintln!("[updater] {text}");
}
