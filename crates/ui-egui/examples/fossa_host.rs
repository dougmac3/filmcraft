//! A stand-in for Fossa Direct's window: a host strip on top and FilmCraft's desk in the rest,
//! the way Fossa embeds it. Files named on the command line open in the desk; "Add to shot" writes
//! the frame at the playhead, and anything the desk saves or exports lands in the temp folder.
//!
//! `cargo run --release -p filmcraft-ui-egui --example fossa_host -- clip.mp4 still.png`

use filmcraft_ui_egui::fossa_native::Desk;

struct Host {
    desk: Desk,
    out: std::path::PathBuf,
    note: String,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(24, 22, 34));
        ui.scope_builder(egui::UiBuilder::new().max_rect(strip.shrink2(egui::vec2(12.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.label("Fossa host");
            if ui.button("Add to shot").clicked() {
                self.note = match self.desk.grab_png() {
                    Ok(png) => write(&self.out, "frame.png", &png),
                    Err(e) => e,
                };
            }
            ui.label(&self.note);
        });
        for (name, bytes) in self.desk.take_saved() {
            self.note = write(&self.out, &name, &bytes);
        }
        let area = egui::Rect::from_min_max(egui::pos2(full.min.x, strip.max.y), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| self.desk.ui(ui, frame));
    }
}

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    match std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&p, bytes)) {
        Ok(()) => format!("added {}", p.display()),
        Err(e) => format!("{}: {e}", p.display()),
    }
}

fn main() -> eframe::Result {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 1000.0]), ..Default::default() };
    eframe::run_native(
        "Fossa host",
        options,
        Box::new(move |cc| {
            let mut desk = Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref());
            for f in &files {
                match std::fs::read(f) {
                    Ok(bytes) => desk.open(f, bytes),
                    Err(e) => eprintln!("{f}: {e}"),
                }
            }
            Ok(Box::new(Host { desk, out: std::env::temp_dir().join("fossa_host_shot"), note: String::new() }))
        }),
    )
}
