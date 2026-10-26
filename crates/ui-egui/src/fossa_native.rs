//! FilmCraft as Fossa Direct's Timeline desk: the editor runs in-process inside a rect of the
//! host's window (never a window of its own). Fossa opens a clip or a still in it as a drop would;
//! what the editor saves or exports comes back through [`Desk::take_saved`] instead of going to
//! disk, as the web bridge offers it as a download.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use eframe::egui_wgpu;
use egui::{Style, Theme, ThemePreference};
use filmcraft_engine::{FsServices, Services, Session, media_browser};
use serde_json::json;

use crate::FilmcraftApp;

/// The filesystem with every write kept in memory and handed to the host. Reads see those files
/// first, then the disk.
#[derive(Default)]
struct HostedFiles {
    written: Mutex<HashMap<String, Vec<u8>>>,
    saved: Mutex<Vec<(String, Vec<u8>)>>,
}

impl HostedFiles {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.written.lock().ok()?.get(path).cloned()
    }
    fn has(&self, path: &str) -> bool {
        self.written.lock().is_ok_and(|w| w.contains_key(path))
    }
}

impl Services for HostedFiles {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.get(path).map_or_else(|| FsServices.read_file(path), Ok)
    }
    // ponytail: a written file is held twice (for reads and for the host) until take_saved; one shared buffer if long exports run out of memory
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        let name = std::path::Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
        self.written.lock().map_err(|_| std::io::Error::other("poisoned"))?.insert(path.to_string(), data.to_vec());
        self.saved.lock().map_err(|_| std::io::Error::other("poisoned"))?.push((name, data.to_vec()));
        Ok(())
    }
    fn write_file_new(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if self.has(path) || std::path::Path::new(path).exists() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        self.write_file(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        self.get(path).map_or_else(|| FsServices.file_size(path), |b| Ok(b.len() as u64))
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        match self.get(path) {
            Some(b) => {
                let a = (offset as usize).min(b.len());
                Ok(b[a..(a + len).min(b.len())].to_vec())
            }
            None => FsServices.read_range(path, offset, len),
        }
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        if self.has(path) { None } else { FsServices.reader(path) }
    }
    fn list_dir(&self, dir: &str) -> Option<std::io::Result<Vec<String>>> {
        FsServices.list_dir(dir)
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        FsServices.file_loader()
    }
    /// Exports are encoded in memory and handed to [`Services::write_file`], so they reach the host.
    fn export_in_memory(&self) -> bool {
        true
    }
    fn list_entries(&self, dir: &str) -> Option<std::io::Result<Vec<media_browser::DirEntry>>> {
        FsServices.list_entries(dir)
    }
    fn volumes(&self) -> Vec<media_browser::Volume> {
        FsServices.volumes()
    }
    fn home_dir(&self) -> Option<String> {
        FsServices.home_dir()
    }
}

/// The editor's own look, kept between frames while the host's is in place.
struct Look {
    dark: Arc<Style>,
    light: Arc<Style>,
    theme: ThemePreference,
}

impl Look {
    fn of(ctx: &egui::Context) -> Self {
        Self { dark: ctx.style_of(Theme::Dark), light: ctx.style_of(Theme::Light), theme: ctx.options(|o| o.theme_preference) }
    }
    fn put(&self, ctx: &egui::Context) {
        ctx.set_style_of(Theme::Dark, self.dark.clone());
        ctx.set_style_of(Theme::Light, self.light.clone());
        ctx.options_mut(|o| o.theme_preference = self.theme);
    }
}

/// FilmCraft embedded in a host window.
pub struct Desk {
    app: FilmcraftApp,
    files: Arc<HostedFiles>,
    /// Where files Fossa opens are put for the media readers (one folder per open).
    inbox: PathBuf,
    opened: u32,
    look: Option<Look>,
    /// The last press was inside the desk: its shortcuts act until a press lands elsewhere.
    focused: bool,
}

impl Desk {
    /// `render_state` is the host window's wgpu state (eframe::Frame::wgpu_render_state()), for editors that paint with wgpu.
    pub fn new(ctx: &egui::Context, render_state: Option<&egui_wgpu::RenderState>) -> Self {
        crate::FOSSA_HOSTED.store(true, Ordering::Relaxed);
        let files = Arc::new(HostedFiles::default());
        let mut app = FilmcraftApp::new(Session::new(files.clone()));
        // a recovery offer belongs to the standalone app's auto-saves, not to a desk
        app.dialog = None;
        if let Some(rs) = render_state {
            app.set_wgpu(rs.clone());
        }
        // Save and Export name a file instead of asking where: the bytes go to the host
        let out = std::env::temp_dir().join("fossa_filmcraft").join("out");
        let o = out.clone();
        app.hooks.pick_save = Some(Box::new(move |name: &str| Some(o.join(name).to_string_lossy().into_owned())));
        let o = out;
        app.hooks.pick_save_as = Some(Box::new(move |_filter: &str, _exts: &[&str], name: &str| {
            let name = std::path::Path::new(name).file_name().map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned());
            Some(o.join(name).to_string_lossy().into_owned())
        }));
        ctx.request_repaint();
        let inbox = std::env::temp_dir().join("fossa_filmcraft").join(format!("in_{}", std::process::id()));
        Self { app, files, inbox, opened: 0, look: None, focused: false }
    }

    /// Open a file Fossa hands in (a PNG/JPEG frame, a clip, a project file), as a drop would:
    /// a project opens; a clip or still goes into the project and onto the timeline at the
    /// playhead (a new sequence the clip's size and rate when none is open).
    pub fn open(&mut self, name: &str, bytes: Vec<u8>) {
        if let Err(e) = self.try_open(name, bytes) {
            log::warn!("open {name}: {e}");
            self.app.status(format!("{name}: {e}"));
        }
    }

    fn try_open(&mut self, name: &str, bytes: Vec<u8>) -> Result<(), String> {
        let safe: String = std::path::Path::new(name)
            .file_name()
            .map(|n| n.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect())
            .filter(|n: &String| !n.trim_matches('.').is_empty())
            .unwrap_or_else(|| "file".into());
        self.opened += 1;
        let dir = self.inbox.join(self.opened.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(&safe);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        let path = path.to_string_lossy().into_owned();
        let s = &mut self.app.session;
        if safe.to_ascii_lowercase().ends_with(".fcproj") {
            return s.execute("file.open", json!({"path": path})).map(|_| ()).map_err(|e| e.to_string());
        }
        let v = s.execute("file.import", json!({"paths": [path]})).map_err(|e| e.to_string())?;
        let item = v["items"].as_array().and_then(|a| a.first()).and_then(|i| i.as_u64()).ok_or("not a clip or still FilmCraft can read")?;
        if s.state.active_sequence.is_none() {
            s.execute("file.newSequenceFromClip", json!({"items": [item]})).map_err(|e| e.to_string())?;
        } else {
            s.execute("timeline.place", json!({"item": item})).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Files the editor saved or exported since the last call, (file name, bytes), for Fossa to add to the shot.
    pub fn take_saved(&mut self) -> Vec<(String, Vec<u8>)> {
        self.files.saved.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// The frame at the playhead of the open sequence, full size, as a PNG, for Fossa's "Add to shot" button.
    pub fn grab_png(&mut self) -> Result<Vec<u8>, String> {
        let s = &self.app.session;
        let seq = s.state.active_sequence.ok_or("no sequence is open on the timeline")?;
        let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
        let _ = filmcraft_media::pending::take();
        let opts = filmcraft_render::RenderOptions { scale: 1.0, captions: true, ..Default::default() };
        let img = filmcraft_render::render_sequence(&s.project, seq, s.playhead(), opts, &provider);
        if filmcraft_media::pending::take() {
            return Err("the frame is still loading; try again in a moment".into());
        }
        filmcraft_export::still::encode(&img, filmcraft_export::still::StillFormat::Png, 8).map_err(|e| e.to_string())
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let over = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| rect.contains(p));
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.focused = over;
        }
        self.app.input_active = over || self.focused;
        // the host's look goes back in place when the desk is done; the desk's comes in for its turn
        let host = Look::of(&ctx);
        let zoom = ctx.zoom_factor();
        if let Some(look) = &self.look {
            look.put(&ctx);
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt("filmcraft"), |ui| {
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            eframe::App::logic(&mut self.app, &ctx, frame);
            ui.set_style(ctx.global_style());
            eframe::App::ui(&mut self.app, ui, frame);
        });
        self.look = Some(Look::of(&ctx));
        host.put(&ctx);
        if ctx.zoom_factor() != zoom {
            ctx.set_zoom_factor(zoom);
        }
    }
}
