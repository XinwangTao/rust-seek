fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/rust-seek.ico");

        if std::env::var("PROFILE").unwrap_or_default() == "release" {
            println!("cargo:rustc-link-arg=/SUBSYSTEM:WINDOWS");
            println!("cargo:rustc-link-arg=/ENTRY:mainCRTStartup");

            // 嵌入 exe 图标与版本信息（资源管理器 / 任务栏 / 属性页）。
            // 版本号由 winresource 默认 feature "toml" 从 CARGO_PKG_VERSION 自动填充。
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/rust-seek.ico");
            res.set("ProductName", "Rust Seek");
            res.set("FileDescription", "Rust Seek — 高性能文件与文本搜索");
            res.set("OriginalFilename", "rust-seek.exe");
            if let Err(e) = res.compile() {
                panic!("嵌入 Windows 资源失败（图标/版本信息）: {e}");
            }
        }
    }
}
