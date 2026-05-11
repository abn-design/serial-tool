mod app;
mod serial_worker;

use app::SerialToolApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Rust 串口工具",
        options,
        Box::new(|cc| Ok(Box::new(SerialToolApp::new(cc)))),
    )
}
