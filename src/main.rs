mod app;
mod clipboard;
mod fastwalk;
mod icon;
mod index;
mod integration;
mod mcp;
mod searcher;
mod updater;
#[allow(dead_code)]
mod usn;

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
    // GUI 是索引的唯一写者：宣告身份并开始心跳，MCP 进程据此只读共享快照。
    index::set_gui_mode();
    integration::touch_gui_alive();
    // GUI 是写者：后台每 30 秒把「有变更就刷新 + 落盘」跑一遍。
    // 否则用户不搜的时候索引一直不更新，只读的 MCP 进程就会看到很旧的快照。
    //
    // 心跳必须在**开工前**打：一轮里可能先跑 20 秒目录巡再写 840MB 缓存，
    // 若只在家里活儿干完才打，两跳间隔 = 30s 睡眠 + 30~50s 干活 > 60s 判活阈值，
    // 8 个只读的 MCP 进程会集体认定「写者已死」，各自全量建一份索引 —— 数 GB 内存，
    // 那正是系统被拖到卡死的路径。
    let _ = std::thread::Builder::new()
        .name("index-keeper".into())
        .spawn(|| loop {
            integration::touch_gui_alive();
            std::thread::sleep(std::time::Duration::from_secs(30));
            index::ensure_fresh(index::auto_refresh_secs());
            index::persist_if_dirty();
            // 干完再跳一次：活儿耗时较长时缩短下一跳的间隔
            integration::touch_gui_alive();
        });
    // 自动更新检查（仅 GUI 模式；--mcp 已在上面提前返回）
    updater::spawn();
    // 启动即在后台预热索引：否则「打开软件后第一次搜索」要等 17–22 秒的全盘
    // 目录 stat 巡（31.7 万目录，过滤驱动下每个 ~1ms）。程序是隐藏启动的，
    // 从点图标到真正开始搜索通常已经过去十几秒，正好用这段时间把巡跑完。
    // 放独立线程、先等 2 秒，避免和窗口首帧抢 I/O；最坏情况与改动前一致。
    let _ = std::thread::Builder::new()
        .name("index-warmup".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            index::ensure_ready();
        });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Rust Seek")
        .with_inner_size([900.0, 600.0])
        .with_min_inner_size([600.0, 400.0])
        .with_visible(!start_hidden);
    if let Some(icon) = icon::egui_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
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
