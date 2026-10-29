mod app;
mod integration;
mod mcp;
mod searcher;

fn main() -> eframe::Result {
    // `--mcp`：作为 MCP server 跑在 stdio 上，供 AI Agent 调用。
    // 必须放在单实例判断之前——Agent 会按需拉起多个进程，不能被互斥体挡住。
    if std::env::args().any(|a| a == "--mcp") {
        if let Err(e) = mcp::run() {
            eprintln!("rust-seek mcp server error: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // 单实例：重复启动时唤醒已运行实例的搜索窗后直接退出
    if integration::notify_existing_or_claim() {
        return Ok(());
    }
    let start_hidden = std::env::args().any(|a| a == "--hidden");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust Seek")
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([600.0, 400.0])
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native("Rust Seek", options, Box::new(move |cc| {
        // Load Microsoft YaHei for CJK support
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\msyh.ttc") {
            fonts.font_data.insert(
                "msyh".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            // Add as fallback after the default proportional font
            fonts.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("msyh".to_owned());
            fonts.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("msyh".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);
        Ok(Box::new(app::App::new(start_hidden)))
    }))
}
