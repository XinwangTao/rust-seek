//! 应用图标：编译期嵌入 `assets/rust-seek-64.png`，运行时解码为 RGBA。
//! 同一份图形同时用于 exe 资源（build.rs 用 .ico）、任务栏/窗口图标和托盘图标。

/// 解码 64×64 RGBA 图标；资源损坏时返回 None，由调用方回退。
pub fn rgba() -> Option<(Vec<u8>, u32, u32)> {
    let bytes = include_bytes!("../assets/rust-seek-64.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((buf, info.width, info.height))
}

/// eframe/egui 窗口图标（任务栏、Alt+Tab）。
pub fn egui_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = rgba()?;
    Some(egui::IconData { rgba, width, height })
}
