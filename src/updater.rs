//! 自动更新：GitHub Releases → 系统弹窗确认 → 下载 + SHA-256 校验 → 替换 exe 并重启。
//!
//! 流程见 docs/archify/auto_update_workflow.html（L1，2026-11-04 已确认）：
//!   启动 30s 后查一次，之后每 6h；仅比较版本号，只有用户点「是」才下载；
//!   `--mcp` 进程不进入本模块（main.rs 在 MCP 分支已提前返回）。
//!
//! 每次检查/决策/失败都追加到 `%LOCALAPPDATA%\rust-seek\update.log`，
//! 便于“到底有没有在检查”这类问题直接看日志回答。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/XinwangTao/rust-seek/releases/latest/download/latest.json";
const DEFAULT_FIRST_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const RETRY_MIN: Duration = Duration::from_secs(10 * 60);
const RETRY_MAX: Duration = CHECK_INTERVAL;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// 本轮进程内下载/替换失败过的版本：不再重复弹窗，静默等下一轮。
static SESSION_BLOCKED: Mutex<Option<String>> = Mutex::new(None);

// ---------- 联调开关 ----------

/// 清单地址（可用 RUST_SEEK_UPDATE_MANIFEST 覆盖，便于本地联调）
fn manifest_url() -> String {
    std::env::var("RUST_SEEK_UPDATE_MANIFEST").unwrap_or_else(|_| DEFAULT_MANIFEST_URL.to_string())
}

fn dev_override() -> bool {
    std::env::var_os("RUST_SEEK_UPDATE_MANIFEST").is_some()
}

/// 首查延迟（RUST_SEEK_UPDATE_FIRST_DELAY_SECS 可覆盖，便于本地联调）
fn first_delay() -> Duration {
    std::env::var("RUST_SEEK_UPDATE_FIRST_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_FIRST_DELAY)
}

/// 正式清单与下载地址必须是 https；本地联调（设了 MANIFEST 覆盖）才放开。
fn require_https(url: &str, what: &str) -> Result<(), String> {
    if dev_override() || url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("{what}不是 https，已拒绝：{url}"))
    }
}

// ---------- 日志 ----------

fn log(msg: &str) {
    eprintln!("[updater] {msg}");
    let Some(p) = state_path() else { return };
    let path = p.with_file_name("update.log");
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
        let _ = fs::remove_file(&path);
    }
    let line = format!("{} {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn block_session(version: &str) {
    if let Ok(mut slot) = SESSION_BLOCKED.lock() {
        *slot = Some(version.to_string());
    }
}

fn session_blocked(version: &str) -> bool {
    SESSION_BLOCKED
        .lock()
        .map(|s| s.as_deref() == Some(version))
        .unwrap_or(false)
}

// ---------- 后台线程 ----------

/// 后台线程入口：清理上次遗留的 .old，延迟首查，之后定时循环。
pub fn spawn() {
    if std::env::var_os("RUST_SEEK_NO_UPDATE").is_some() {
        return;
    }
    let _ = std::thread::Builder::new().name("updater".into()).spawn(|| {
        cleanup_old();
        std::thread::sleep(first_delay());
        let mut backoff = RETRY_MIN;
        loop {
            match check_once() {
                Ok(()) => {
                    backoff = RETRY_MIN;
                    std::thread::sleep(CHECK_INTERVAL);
                }
                Err(e) => {
                    // 网络抖动不该让下一次检查等到 6 小时后：按 10min → 20min → … 退避。
                    // 注意只有“取清单”这类无弹窗的失败会走到这里，下载/替换失败在
                    // check_once 内部已记入 SESSION_BLOCKED，不会引发弹窗风暴。
                    log(&format!("检查更新失败: {e}"));
                    let wait = backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                    std::thread::sleep(wait);
                }
            }
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
    let url = manifest_url();
    require_https(&url, "更新清单地址")?;
    let body = http_get(&url)?;
    let manifest: Manifest =
        serde_json::from_slice(&body).map_err(|e| format!("更新清单解析失败: {e}"))?;

    // 版本号解析不出来时不能说“已是最新”，否则排查时会被日志误导
    if parse_ver(&manifest.version).is_none() {
        return Err(format!("更新清单版本号无法解析：{:?}", manifest.version));
    }
    if !is_newer(&manifest.version, current) {
        if is_newer(current, &manifest.version) {
            log(&format!(
                "清单版本 v{} 低于本机 v{current}（可能补发了旧版本线），忽略",
                manifest.version
            ));
        } else {
            log(&format!("已是最新（本地 v{current}，远端 v{}）", manifest.version));
        }
        return Ok(());
    }
    if skipped_version().as_deref() == Some(manifest.version.as_str()) {
        log(&format!("v{} 此前已被用户拒绝，跳过", manifest.version));
        return Ok(());
    }
    if recently_updated_to(&manifest.version) {
        log(&format!(
            "v{} 已更新过但程序仍是 v{current}（发布包版本号很可能没跟着改），不再重复下载",
            manifest.version
        ));
        return Ok(());
    }
    if session_blocked(&manifest.version) {
        log(&format!("v{} 本轮已失败过，跳过", manifest.version));
        return Ok(());
    }

    // 资产检查必须在弹窗之前：缺架构时先别打扰用户
    let Some(asset) = manifest.assets.get(arch_key()) else {
        log(&format!(
            "更新清单里没有 {} 架构的下载地址，跳过",
            arch_key()
        ));
        return Ok(());
    };
    require_https(&asset.url, "下载地址")?;

    let mut prompt = format!(
        "发现新版本 v{}（当前 v{}）。\n\n是否现在下载并自动更新？\n更新后程序会重启并回到托盘。",
        manifest.version, current
    );
    if let Some(n) = manifest.notes.as_ref().filter(|s| !s.trim().is_empty()) {
        let brief: String = n.chars().take(200).collect();
        prompt.push_str("\n\n更新说明：\n");
        prompt.push_str(&brief);
    }
    log(&format!("发现新版本 v{}（当前 v{current}），等待用户确认", manifest.version));

    match confirm(&prompt, "Rust Seek 更新") {
        Answer::Yes => {}
        Answer::No => {
            save_skipped(&manifest.version);
            log(&format!("用户拒绝 v{}", manifest.version));
            return Ok(());
        }
        Answer::Dismiss => {
            // 点 X 只是“这次别问”，不该等同永久拒绝
            log(&format!("用户暂缓 v{}（关闭了对话框）", manifest.version));
            return Ok(());
        }
    }

    let tmp = temp_exe_path(&manifest.version);
    if let Err(e) = download_verified(&asset.url, &asset.sha256, &tmp) {
        let _ = fs::remove_file(&tmp);
        log(&format!("下载/校验失败: {e}"));
        info_box(&format!("更新失败：{e}\n\n当前版本仍可正常使用。"), "Rust Seek 更新");
        block_session(&manifest.version);
        return Ok(());
    }
    log(&format!("v{} 下载并校验通过，开始替换", manifest.version));

    if let Err(e) = replace_and_restart(&tmp, &manifest.version) {
        let _ = fs::remove_file(&tmp);
        log(&format!("替换失败: {e}"));
        info_box(&format!("更新失败：{e}\n\n当前版本仍可正常使用。"), "Rust Seek 更新");
        block_session(&manifest.version);
    }
    Ok(())
}

// ---------- 下载 / 替换 ----------

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

/// 旧 exe 改名 .old → 新 exe 先落同目录临时名再原子 rename → 延迟拉起新进程 → 当前进程退出。
///
/// 关键：正式路径不能直接写入。写到一半被打断会留下半截 exe，程序就再也打不开了。
fn replace_and_restart(new_exe: &Path, version: &str) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("定位当前程序失败: {e}"))?;
    let name = cur
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rust-seek.exe".into());
    let old = free_old_path(&cur, &name);
    fs::rename(&cur, &old).map_err(|e| format!("旧程序改名失败（可能没有写入权限）: {e}"))?;

    let staged = cur.with_file_name(format!("{name}.new"));
    let _ = fs::remove_file(&staged);
    if let Err(e) = fs::copy(new_exe, &staged) {
        let _ = fs::remove_file(&staged);
        let _ = fs::rename(&old, &cur); // 回滚，保证旧版仍可启动
        return Err(format!("写入新版本失败: {e}"));
    }
    if let Err(e) = fs::rename(&staged, &cur) {
        let _ = fs::remove_file(&staged);
        let _ = fs::rename(&old, &cur);
        return Err(format!("新版本就位失败: {e}"));
    }
    let _ = fs::remove_file(new_exe);
    mark_updated(version);

    // 等本进程退出、单实例互斥体释放后再拉起（隐藏启动，回到托盘待命）
    let cmdline = format!(
        "ping -n 3 127.0.0.1 >nul & start \"\" \"{}\" --hidden",
        cur.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        match std::process::Command::new("cmd")
            .arg("/C")
            .arg(cmdline)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            Ok(_) => {}
            Err(e) => {
                // 排程失败就绝不能退出，否则程序会“更新完就消失”
                return Err(format!(
                    "新版本已就位，但自动重启失败（{e}）。请手动重新打开 Rust Seek。"
                ));
            }
        }
    }
    std::process::exit(0);
}

/// 找一个能用的 `.old` 路径：优先固定名；若它被仍映射旧镜像的 MCP 子进程占用
/// （Windows 上删不掉），退化成带时间戳的名字，别让整次更新因为残留文件失败。
fn free_old_path(cur: &Path, name: &str) -> PathBuf {
    let fixed = cur.with_file_name(format!("{name}.old"));
    if !fixed.exists() {
        return fixed;
    }
    if fs::remove_file(&fixed).is_ok() {
        return fixed;
    }
    log("旧的 .old 文件删不掉（可能仍被 MCP 子进程占用），改用带时间戳的文件名");
    cur.with_file_name(format!("{name}.old-{}", chrono::Utc::now().timestamp()))
}

/// 启动时清理上一轮替换留下的 .old / 半截 .new（此时旧进程已退出，可删）。
fn cleanup_old() {
    let Ok(cur) = std::env::current_exe() else { return };
    let Some(name) = cur.file_name() else { return };
    let name = name.to_string_lossy().into_owned();
    if let Some(dir) = cur.parent() {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.starts_with(&format!("{name}.old")) || fname == format!("{name}.new") {
                let _ = fs::remove_file(entry.path());
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

fn read_state() -> serde_json::Value {
    state_path()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_state(state: &serde_json::Value) {
    let Some(p) = state_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if fs::write(&tmp, state.to_string()).is_ok() && fs::rename(&tmp, &p).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn skipped_version() -> Option<String> {
    read_state()
        .get("skipped")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn save_skipped(version: &str) {
    let mut state = read_state();
    state["skipped"] = serde_json::Value::String(version.to_string());
    write_state(&state);
}

fn mark_updated(version: &str) {
    let mut state = read_state();
    state["last_update"] = serde_json::json!({
        "version": version,
        "at": chrono::Utc::now().timestamp(),
    });
    write_state(&state);
}

/// 已经替换成该版本、但程序自报版本仍是旧的（CI 漏改版本号的典型症状）：
/// 不再重复下载同一份包——否则会变成“每天下载 9MB、永远升不上去”的死循环。
fn recently_updated_to(version: &str) -> bool {
    read_state()
        .get("last_update")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .map(|v| v == version)
        .unwrap_or(false)
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
        // 企业网常见：走系统代理环境变量，否则 HTTPS 直接超时
        .try_proxy_from_env(true)
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

#[derive(PartialEq, Eq)]
enum Answer {
    Yes,
    No,
    Dismiss,
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 是 / 否 确认框。点 X 算「暂缓」，不算拒绝（拒绝会永久跳过该版本，没有撤销入口）。
/// RUST_SEEK_UPDATE_AUTO=1 时不弹窗直接视为同意（仅用于本地联调，
/// 必须同时设了 RUST_SEEK_UPDATE_MANIFEST 才生效，避免被环境变量静默接管）。
#[cfg(windows)]
fn confirm(text: &str, title: &str) -> Answer {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDNO, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    if std::env::var_os("RUST_SEEK_UPDATE_AUTO").is_some() {
        if dev_override() {
            log(&format!(
                "自动确认（联调模式）: {}",
                text.lines().next().unwrap_or("")
            ));
            return Answer::Yes;
        }
        log("忽略 RUST_SEEK_UPDATE_AUTO：未设置 RUST_SEEK_UPDATE_MANIFEST（仅联调允许）");
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
    match r {
        IDYES => Answer::Yes,
        IDNO => Answer::No,
        _ => Answer::Dismiss,
    }
}

#[cfg(not(windows))]
fn confirm(_text: &str, _title: &str) -> Answer {
    Answer::Dismiss
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("5.1.1", "5.1.0"));
        assert!(is_newer("v5.1.1", "5.1.0"));
        assert!(!is_newer("5.1.0", "5.1.0"));
        assert!(!is_newer("5.1.0", "5.1.1"));
        assert!(is_newer("5.2", "5.1.9"));
        assert!(is_newer("6.0.0", "5.99.99"));
        assert!(is_newer("5.1.0.1", "5.1.0"));
        assert!(!is_newer("5.1.0", "5.1.0.1"));
    }

    #[test]
    fn version_parse_edges() {
        assert_eq!(parse_ver("5.1.0"), Some(vec![5, 1, 0]));
        assert_eq!(parse_ver(" v5.1 "), Some(vec![5, 1]));
        assert_eq!(parse_ver("5.1.1-rc1"), Some(vec![5, 1, 1]));
        assert_eq!(parse_ver(""), None);
        assert_eq!(parse_ver("abc"), None);
        // 非法远端版本不应触发更新
        assert!(!is_newer("", "5.1.0"));
        assert!(!is_newer("latest", "5.1.0"));
    }

    #[test]
    fn sha_hex() {
        assert_eq!(hex(&[0x00, 0xff, 0x0a]), "00ff0a");
    }
}
