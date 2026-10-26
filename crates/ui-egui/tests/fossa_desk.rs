//! FilmCraft as a Fossa Direct desk, offscreen: a host strip on top, the desk in the rest, a
//! 1280x720 still opened in it as Fossa would. Ignored by default:
//! `FOSSA_DESK_SHOT=out.png [FOSSA_DESK_CLIP=clip.mp4] cargo test --release -p filmcraft-ui-egui --test fossa_desk -- --ignored`.

use filmcraft_ui_egui::fossa_native::Desk;

struct Host {
    desk: Desk,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(24, 22, 34));
        ui.painter().text(strip.left_center() + egui::vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, "Fossa host strip", egui::FontId::proportional(14.0), egui::Color32::WHITE);
        let area = egui::Rect::from_min_max(egui::pos2(full.min.x, strip.max.y), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| self.desk.ui(ui, frame));
    }
}

fn still(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
#[ignore]
fn fossa_desk() {
    let mut h = egui_kittest::Harness::builder().with_accessibility_check(false).allow_missing_glyphs().with_size(egui::vec2(1600.0, 1000.0)).with_accessibility_check(false).allow_missing_glyphs().build_eframe(|cc| Host { desk: Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref()) });
    h.run_steps(4);
    h.state_mut().desk.open("fossa_frame.png", still(1280, 720));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while std::time::Instant::now() < until {
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let png = h.state_mut().desk.grab_png().unwrap();
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (1280, 720));
    assert!(h.state_mut().desk.take_saved().is_empty(), "nothing was saved yet");
    // a video clip (FOSSA_DESK_CLIP=clip.mp4) goes onto the same timeline at the playhead
    if let Ok(clip) = std::env::var("FOSSA_DESK_CLIP") {
        h.state_mut().desk.open("take.mp4", std::fs::read(clip).unwrap());
        h.run_steps(4);
        let png = h.state_mut().desk.grab_png().unwrap();
        assert!(image::load_from_memory(&png).is_ok());
        h.run_steps(8);
    }
    if let Ok(path) = std::env::var("FOSSA_DESK_SHOT") {
        h.render().unwrap().save(path).unwrap();
    }
}
