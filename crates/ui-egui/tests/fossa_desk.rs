//! FilmCraft as a Fossa Direct desk, offscreen: a host strip on top, the desk in the rest, a
//! 1280x720 still opened in it as Fossa would; then driven through the in-process control channel
//! and shown as one panel alone (the Timeline narrowed to a cut). Ignored by default:
//! `FOSSA_DESK_SHOT=out.png FOSSA_PANEL_SHOT=panel.png [FOSSA_DESK_CLIP=clip.mp4] cargo test --release -p filmcraft-ui-egui --test fossa_desk -- --ignored`.

use filmcraft_ui_egui::fossa_native::Desk;
use serde_json::{Value, json};

/// Where the host draws the panel alone (900x320, as an in-chat card would).
const PANEL: egui::Rect = egui::Rect { min: egui::pos2(60.0, 80.0), max: egui::pos2(960.0, 400.0) };

struct Host {
    desk: Desk,
    /// Draw this panel alone at [`PANEL`] instead of the whole desk.
    panel: Option<String>,
    desk_rect: egui::Rect,
}

impl eframe::App for Host {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.desk.raw_input_hook(ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(24, 22, 34));
        ui.painter().text(
            strip.left_center() + egui::vec2(14.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "Fossa host strip",
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        );
        match self.panel.clone() {
            Some(p) => {
                self.desk_rect = PANEL;
                ui.scope_builder(egui::UiBuilder::new().max_rect(PANEL), |ui| self.desk.panel_ui(&p, ui, frame));
            }
            None => {
                let area = egui::Rect::from_min_max(egui::pos2(full.min.x, strip.max.y), full.max);
                self.desk_rect = area;
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| self.desk.ui(ui, frame));
            }
        }
    }
}

fn still(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

type H = egui_kittest::Harness<'static, Host>;

fn settle(h: &mut H, secs: f64) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
    while std::time::Instant::now() < until {
        for _ in 0..2 {
            step(h);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
}

/// One frame, with the desk's queued control input in the window's input as eframe's
/// raw_input_hook would put it (kittest calls no hook).
fn step(h: &mut H) {
    let ctx = h.ctx.clone();
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().desk.raw_input_hook(&ctx, &mut raw);
    *h.input_mut() = raw;
    h.step();
}

/// The rects of every egui Area (window, menu, popup) shown last frame, but the host's own.
fn shown_areas(h: &H) -> Vec<(egui::Id, egui::Rect)> {
    h.ctx.memory(|m| {
        let visible = m.areas().visible_layer_ids();
        visible.into_iter().filter(|l| l.order != egui::Order::Background).filter_map(|l| m.area_rect(l.id).map(|r| (l.id, r))).collect()
    })
}

/// The person's own click at `pos` (the agent's `ui.click` is refused in a desk).
fn person_click(h: &mut H, pos: egui::Pos2, button: egui::PointerButton) {
    h.event(egui::Event::PointerMoved(pos));
    h.event(egui::Event::PointerButton { pos, button, pressed: true, modifiers: Default::default() });
    h.event(egui::Event::PointerButton { pos, button, pressed: false, modifiers: Default::default() });
}

/// The person pressing `key` with the pointer at `pos` (keys go to the desk under the pointer).
fn person_key(h: &mut H, pos: egui::Pos2, key: egui::Key) {
    h.event(egui::Event::PointerMoved(pos));
    h.key_press(key);
}

fn exec(h: &mut H, command: &str, params: Value) -> Value {
    h.state_mut().desk.control("engine.execute", json!({"command": command, "params": params})).unwrap_or_else(|e| panic!("{command}: {e}"))
}

#[test]
#[ignore]
fn fossa_desk() {
    let mut h: H = egui_kittest::Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(1600.0, 1000.0))
        .build_eframe(|cc| Host { desk: Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref()), panel: None, desk_rect: egui::Rect::NOTHING });
    h.run_steps(4);
    h.state_mut().desk.open("fossa_frame.png", still(1280, 720));
    settle(&mut h, 4.0);
    let png = h.state_mut().desk.grab_png().unwrap();
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (1280, 720));
    assert!(h.state_mut().desk.take_saved().is_empty(), "nothing was saved yet");

    // commands, typed as far as the registry's notes go
    let cmds = h.state().desk.commands();
    let cmds = cmds.as_array().unwrap();
    let typed = cmds.iter().filter(|c| c["typed"] == true).count();
    println!("commands: {} ({typed} typed, {} untyped)", cmds.len(), cmds.len() - typed);
    let export = cmds.iter().find(|c| c["id"] == "file.exportMedia").unwrap();
    let flac = export["fields"].as_array().unwrap().iter().find(|f| f["id"] == "flacLevel").unwrap();
    assert_eq!((flac["kind"].as_str(), flac["min"].as_f64(), flac["max"].as_f64(), flac["default"].as_i64()), (Some("int"), Some(0.0), Some(8.0), Some(5)));
    let roll = cmds.iter().find(|c| c["id"] == "timeline.roll").unwrap();
    assert!(roll["fields"].as_array().unwrap().iter().any(|f| f["id"] == "delta" && f["kind"] == "ticks" && f["optional"] == true));
    assert!(h.state().desk.panels().iter().any(|p| p == "Timeline"));

    // a second shot after the first: a cut between them
    let first = h.state_mut().desk.control("engine.execute", json!({"command": "sequence.inspect"})).unwrap();
    let clip1 = first["video"][0]["items"][0]["clip"].as_u64().expect("first clip id");
    let cut = first["duration"].as_f64().unwrap() / filmcraft_time::TICKS_PER_SECOND as f64;
    println!("first shot ends at {cut} s");
    exec(&mut h, "playhead.set", json!({"seconds": cut}));
    h.state_mut().desk.open("fossa_frame2.png", still(1280, 720));
    settle(&mut h, 2.0);
    // a real edit through the control channel: an effect on the first shot
    exec(&mut h, "effects.apply", json!({"clips": [clip1], "effect": "gaussian_blur"}));
    // narrow to the cut, then trim it (roll by 6 frames)
    let f = h.state_mut().desk.focus(json!({"cut": cut})).unwrap();
    println!("focus: {f}");
    assert!(!f["editPoints"].as_array().unwrap().is_empty(), "the cut is selected for trimming");
    exec(&mut h, "trim.nudge", json!({"frames": 6}));
    let after = exec(&mut h, "sequence.inspect", json!({}));
    let item = &after["video"][0]["items"][0];
    assert_eq!(item["effects"].as_array().unwrap().iter().filter(|e| e["effect"] == "gaussian_blur").count(), 1, "the effect is on the shot");
    assert!(item["end"].as_f64().unwrap() > first["video"][0]["items"][0]["end"].as_f64().unwrap(), "the cut rolled later");
    // preview playback opens the desk's sound output (a machine without one plays silent)
    h.state_mut().desk.control("ui.playback", json!({"action": "play"})).unwrap();
    settle(&mut h, 0.8);
    let state = h.state_mut().desk.control("ui.inspect", json!({})).unwrap();
    println!("playing: {}, audio clock: {}", state["playback"]["playing"], state["playback"]["audioClock"]);
    h.state_mut().desk.control("ui.playback", json!({"action": "stop"})).unwrap();
    exec(&mut h, "playhead.set", json!({"seconds": cut}));
    settle(&mut h, 1.0);
    if let Ok(path) = std::env::var("FOSSA_DESK_SHOT") {
        h.render().unwrap().save(path).unwrap();
    }

    // a dialog opens inside the desk, takes a key and closes
    let desk = h.state().desk_rect;
    h.state_mut().desk.control("ui.menu.invoke", json!({"id": "app.about"})).unwrap();
    settle(&mut h, 0.6);
    let about = filmcraft_ui_egui::fid("About FilmCraft");
    let r = h.ctx.memory(|m| m.area_rect(about)).expect("About is open");
    assert!(desk.contains_rect(r), "About {r:?} inside the desk {desk:?}");
    person_key(&mut h, r.center(), egui::Key::Escape);
    settle(&mut h, 0.6);
    assert!(!shown_areas(&h).iter().any(|(id, _)| *id == about), "Escape closed About");

    // the Timeline alone, 900x320, narrowed to the cut
    h.state_mut().panel = Some("Timeline".into());
    h.state_mut().desk.focus(json!({"cut": cut})).unwrap();
    settle(&mut h, 1.5);
    if let Ok(path) = std::env::var("FOSSA_PANEL_SHOT") {
        let shot = h.render().unwrap();
        let ppp = h.ctx.pixels_per_point();
        let (x, y, w, hh) = ((PANEL.min.x * ppp) as u32, (PANEL.min.y * ppp) as u32, (PANEL.width() * ppp) as u32, (PANEL.height() * ppp) as u32);
        image::imageops::crop_imm(&shot, x, y, w, hh).to_image().save(path).unwrap();
    }
    // a context menu on a clip opens inside the panel's rect and closes on Escape
    let before: Vec<_> = shown_areas(&h).into_iter().map(|(id, _)| id).collect();
    // the second shot (the first one's middle is out of the narrowed view)
    let seq = h.state_mut().desk.control("engine.execute", json!({"command": "sequence.inspect"})).unwrap();
    let clip2 = seq["video"][0]["items"][1]["clip"].as_u64().expect("second clip id");
    let at = h.state_mut().desk.control("ui.timeline.locate", json!({"clip": clip2, "edge": "in"})).unwrap();
    let (x, y) = (at["x"].as_f64().unwrap() + 60.0, at["y"].as_f64().unwrap());
    assert!(PANEL.contains(egui::pos2(x as f32, y as f32)), "{x},{y} in view");
    person_click(&mut h, egui::pos2(x as f32, y as f32), egui::PointerButton::Secondary);
    settle(&mut h, 0.8);
    if let Ok(path) = std::env::var("FOSSA_MENU_SHOT") {
        h.render().unwrap().save(path).unwrap();
    }
    let menus: Vec<_> = shown_areas(&h).into_iter().filter(|(id, _)| !before.contains(id)).collect();
    println!("menus: {menus:?}");
    assert!(!menus.is_empty(), "the clip's context menu opened");
    for (id, r) in &menus {
        assert!(PANEL.expand(0.5).contains_rect(*r), "menu {id:?} {r:?} inside the panel {PANEL:?}");
    }
    person_key(&mut h, egui::pos2(x as f32, y as f32), egui::Key::Escape);
    settle(&mut h, 0.6);
    assert!(shown_areas(&h).iter().all(|(id, _)| !menus.iter().any(|(m, _)| m == id)), "Escape closed the menu");

    // a video clip (FOSSA_DESK_CLIP=clip.mp4) goes onto the same timeline at the playhead
    if let Ok(clip) = std::env::var("FOSSA_DESK_CLIP") {
        h.state_mut().panel = None;
        h.state_mut().desk.open("take.mp4", std::fs::read(clip).unwrap());
        h.run_steps(4);
        let png = h.state_mut().desk.grab_png().unwrap();
        assert!(image::load_from_memory(&png).is_ok());
        h.run_steps(8);
    }
}

/// The agent's requests reach files only in the desk's own folder (or the folders the host
/// names); its made-up clicks are refused.
#[test]
#[ignore]
fn fossa_desk_files() {
    let ctx = egui::Context::default();
    let mut desk = Desk::new(&ctx, None);
    let folder = desk.folder().expect("the desk has its own folder").to_path_buf();
    assert!(folder.file_name().unwrap().to_string_lossy().starts_with("fossa-filmcraft-"), "{}", folder.display());
    desk.open("fossa_frame.png", still(320, 180));
    let outside = std::env::temp_dir().join(format!("fc-desk-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    let at = |d: &std::path::Path, n: &str| d.join(n).to_string_lossy().into_owned();
    let exec = |desk: &mut Desk, command: &str, params: Value| desk.control("engine.execute", json!({"command": command, "params": params}));

    // outside: refused, for writes and reads
    let e = exec(&mut desk, "file.saveAs", json!({"path": at(&outside, "leak.fcproj")})).unwrap_err();
    assert!(e.contains("automation path rejected"), "{e}");
    let e = exec(&mut desk, "file.exportMedia", json!({"path": at(&outside, "leak.wav"), "format": "wav", "wait": true})).unwrap_err();
    assert!(e.contains("automation path rejected"), "{e}");
    std::fs::write(outside.join("secret.png"), still(16, 16)).unwrap();
    let e = exec(&mut desk, "file.import", json!({"paths": [at(&outside, "secret.png")]})).unwrap_err();
    assert!(e.contains("automation path rejected"), "{e}");
    assert!(desk.take_saved().is_empty(), "nothing went out");

    // inside the desk's folder: works, and the file comes back to the host
    exec(&mut desk, "file.saveAs", json!({"path": at(&folder, "cut.fcproj")})).unwrap();
    exec(&mut desk, "file.exportEdl", json!({"path": at(&folder, "cut.edl")})).unwrap();
    let saved: Vec<String> = desk.take_saved().into_iter().map(|(n, _)| n).collect();
    assert!(saved.iter().any(|n| n == "cut.fcproj") && saved.iter().any(|n| n == "cut.edl"), "{saved:?}");

    // the host can name other folders
    desk.set_file_roots(&[&outside], &[&outside]).unwrap();
    exec(&mut desk, "file.saveAs", json!({"path": at(&outside, "there.fcproj")})).unwrap();
    assert!(exec(&mut desk, "file.saveAs", json!({"path": at(&folder, "cut2.fcproj")})).is_err(), "the desk's folder is no longer named");

    // made-up input is refused: it would play out unconfined in the person's frames
    for m in ["ui.click", "ui.key", "ui.drag", "ui.type", "ui.move", "ui.scroll"] {
        let e = desk.control(m, json!({"x": 10, "y": 10, "key": "Escape", "text": "x"})).unwrap_err();
        assert!(e.contains("use engine.execute"), "{m}: {e}");
    }
    let _ = std::fs::remove_dir_all(outside);
}
