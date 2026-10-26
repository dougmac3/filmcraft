//! FilmCraft as Fossa Direct's Timeline desk: the editor runs in-process inside a rect of the
//! host's window (never a window of its own). Fossa opens a clip or a still in it as a drop would;
//! what the editor saves or exports comes back through [`Desk::take_saved`] instead of going to
//! disk, as the web bridge offers it as a download.
//!
//! Fossa's chat drives it through the editor's own control channel, in-process ([`Desk::control`],
//! [`Desk::commands`]), and can show one panel of it alone ([`Desk::panel_ui`]) narrowed to what the
//! person is working on ([`Desk::focus`]: a selection and a view, e.g. the Timeline around one cut).
//! Windows, menus, popups and tooltips stay inside the rect the desk (or the panel) is given.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui_wgpu;
use egui::{Style, Theme, ThemePreference};
use filmcraft_engine::{FsServices, Services, Session, media_browser};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::control::{self, ControlRequest, Outcome};
use crate::dock::PanelKind;
use crate::native_dialogs::FileDialog;

// The desktop app's cpal output, so preview playback is heard inside the desk. CpalOut opens its
// stream when playback starts and drops it when playback stops; the desk owns the one CpalOut and
// it goes when the desk does.
#[path = "../../../apps/filmcraft/src/audio.rs"]
mod audio;

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
    ctx: egui::Context,
    files: Arc<HostedFiles>,
    /// Where files Fossa opens are put for the media readers (one folder per open).
    inbox: PathBuf,
    opened: u32,
    look: Option<Look>,
    /// The last press was inside the desk: its shortcuts act until a press lands elsewhere.
    focused: bool,
    /// The desk's input as it stood after its last turn: the next turn's input is built on it, so
    /// the desk's pointer, clicks and drags run on their own state inside its own rect.
    input: Option<egui::InputState>,
}

/// Where Save and Export put their files (then handed to the host by [`Desk::take_saved`]).
fn out_dir() -> PathBuf {
    std::env::temp_dir().join("fossa_filmcraft").join("out")
}

/// `name` as a file in [`out_dir`] (a suggested name may be a full path).
fn out_path(name: &str) -> String {
    let name = Path::new(name).file_name().map_or_else(|| "untitled".to_string(), |n| n.to_string_lossy().into_owned());
    out_dir().join(name).to_string_lossy().into_owned()
}

/// File > Open, Import, Link Media, folder pickers: the system dialog on its own thread, so the
/// host's window keeps drawing while it is open. Save and Export name a file in [`out_dir`] at once.
fn run_dialog(dialog: FileDialog) -> Receiver<Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = |s: &str| crate::i18n::t(s).to_string();
    let (media, project) = (t("Media"), t("FilmCraft Project"));
    let dialog = match dialog {
        FileDialog::SaveProject { name } | FileDialog::SaveAs { name, .. } => {
            let _ = tx.send(vec![out_path(&name)]);
            return rx;
        }
        FileDialog::OpenFile { filter, exts } => FileDialog::OpenFile { filter: t(&filter), exts },
        other => other,
    };
    let spawned = std::thread::Builder::new().name("fossa-file-dialog".into()).spawn(move || {
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        let paths: Vec<String> = match dialog {
            FileDialog::OpenFiles { exts } => rfd::FileDialog::new().add_filter(&media, &exts).pick_files().unwrap_or_default().into_iter().map(path).collect(),
            FileDialog::OpenFile { filter, exts } => rfd::FileDialog::new().add_filter(&filter, &exts).pick_file().map(path).into_iter().collect(),
            FileDialog::Relink { exts, .. } => rfd::FileDialog::new().add_filter(&media, &exts).pick_file().map(path).into_iter().collect(),
            FileDialog::OpenProject => rfd::FileDialog::new().add_filter(&project, &["fcproj"]).pick_file().map(path).into_iter().collect(),
            FileDialog::Folder { at } => {
                let d = rfd::FileDialog::new();
                let d = match at {
                    Some(dir) if !dir.is_empty() => d.set_directory(dir),
                    _ => d,
                };
                d.pick_folder().map(path).into_iter().collect()
            }
            FileDialog::SaveProject { .. } | FileDialog::SaveAs { .. } => Vec::new(),
        };
        let _ = tx.send(paths);
    });
    if let Err(e) = spawned {
        // the sender went with the closure: the app takes the dialog as cancelled
        log::warn!("could not start the file dialog thread: {e}");
    }
    rx
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
        app.audio = Some(Box::new(audio::CpalOut::new()));
        // Save and Export name a file instead of asking where: the bytes go to the host
        app.hooks.pick_save = Some(Box::new(|name: &str| Some(out_path(name))));
        app.hooks.pick_save_as = Some(Box::new(|_filter: &str, _exts: &[&str], name: &str| Some(out_path(name))));
        // the other pickers show the system dialog without stopping the frame loop
        app.hooks.run_dialog = Some(Box::new(run_dialog));
        // ponytail: Settings > Labels > Import preset asks this synchronous picker directly (the host's frames wait while it is open); move that button to pick_ui if it matters
        app.hooks.pick_open_file =
            Some(Box::new(|filter: &str, exts: &[&str]| rfd::FileDialog::new().add_filter(filter, exts).pick_file().map(|p| p.to_string_lossy().into_owned())));
        ctx.request_repaint();
        let inbox = std::env::temp_dir().join("fossa_filmcraft").join(format!("in_{}", std::process::id()));
        Self { app, ctx: ctx.clone(), files, inbox, opened: 0, look: None, focused: false, input: None }
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

    /// Run one control-channel request in-process, as the desktop app's TCP channel would
    /// (`engine.execute {command, params}`, `engine.commands`, `ui.set`, `ui.panel.show`,
    /// `ui.click`, ...; see [`crate::control`]). UI input (`ui.click`, `ui.key`, `ui.drag`, ...) is
    /// queued and plays over the next frames. A request that needs the UI to settle first (an
    /// element not on screen yet) fails with a message; ask again after a frame.
    pub fn control(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if matches!(method, "ui.resize" | "app.quit" | "ui.screenshot") {
            return Err(format!("`{method}` acts on a window of its own; a desk has none (use grab_png or the host's capture)"));
        }
        let (req, _reply) = ControlRequest::new(method, params);
        match control::handle(&mut self.app, &self.ctx, &req) {
            Outcome::Done(v) if v["ok"] == true => Ok(v["result"].clone()),
            Outcome::Done(v) => Err(v["error"].as_str().unwrap_or("failed").to_string()),
            Outcome::Retry(msg) => Err(msg),
            Outcome::AfterInput => Ok(Value::Null),
            Outcome::Screenshot { .. } => Err("no screenshots in a desk".into()),
        }
    }

    /// The engine's commands (`engine.commands`), each with its parameters typed as far as the
    /// command registry's parameter notes say: `fields` of `{id, kind, optional, default, min,
    /// max, choices | examples, unit, items | fields, note}`; `kind` is bool, int, number, ticks
    /// (`perSecond` per second), id, string, choice, array, object, several joined by `|`, or
    /// `untyped` where the notes name no type (`any`, free maps, named structs). `typed` is false
    /// when any parameter is untyped; `params` keeps the notes as written.
    pub fn commands(&self) -> Value {
        let s = &self.app.session;
        Value::Array(
            filmcraft_engine::command_specs()
                .iter()
                .map(|c| {
                    let fields = spec_fields(c.params);
                    let typed = (c.params.trim().starts_with('{') || c.params.trim().is_empty()) && fields.iter().all(is_typed);
                    let all: Vec<&str> = s.shortcuts.for_command(c.id).iter().map(|b| b.keys.as_str()).collect();
                    json!({"id": c.id, "label": c.label, "menu": c.menu, "shortcut": s.shortcuts.primary(c.id), "shortcuts": all,
                        "enabled": (c.enabled)(s).is_ok(), "params": c.params, "fields": fields, "typed": typed})
                })
                .collect(),
        )
    }

    /// The panels [`Self::panel_ui`] can draw alone (the ids `ui.panel.show` takes).
    pub fn panels(&self) -> Vec<String> {
        PanelKind::ALL.iter().map(|p| p.id()).collect()
    }

    /// Narrow what the panels show, as the agent's last change left it, to a target. Any of:
    /// `playhead` (s); `clips: [id]` (the selection); `clip: id` with `effect: index | id`
    /// (selected, open in Effect Controls, the clip's other effects folded); `cut: s` (the edit
    /// point nearest that time selected for trimming, `trim`: roll | rippleIn | rippleOut |
    /// trimIn | trimOut, default roll; the Timeline shows `pad` s either side, default 1.5);
    /// `range: [from, to]` (s, the Timeline's view); `ui: {...}` (anything `ui.set` takes).
    pub fn focus(&mut self, target: Value) -> Result<Value, String> {
        let secs = |k: &str| target.get(k).and_then(Value::as_f64);
        let s = &mut self.app.session;
        let run = |s: &mut Session, id: &str, p: Value| s.execute(id, p).map_err(|e| format!("{id}: {e}"));
        if let Some(t) = secs("playhead").or(secs("cut")) {
            run(s, "playhead.set", json!({"seconds": t}))?;
        }
        if let Some(clips) = target.get("clips") {
            run(s, "timeline.select", json!({"clips": clips, "add": false, "toggle": false}))?;
        }
        if let Some(c) = target.get("clip").and_then(Value::as_u64) {
            run(s, "timeline.select", json!({"clips": [c], "add": false, "toggle": false}))?;
            if let Some(e) = target.get("effect") {
                run(s, "effects.select", json!({"clip": c, "effect": e}))?;
                let effects = s.active_sequence().and_then(|q| q.find_item(filmcraft_project::ClipId(c))).map(|(_, it)| it.effects.clone()).unwrap_or_default();
                let at = e.as_u64().map(|i| i as usize).or_else(|| e.as_str().and_then(|id| effects.iter().position(|x| x.effect == id)));
                for i in 0..effects.len() {
                    let key = format!("{c}:{i}");
                    self.app.ui.collapsed_fx.retain(|k| *k != key);
                    if Some(i) != at {
                        self.app.ui.collapsed_fx.push(key);
                    }
                }
            }
        }
        let s = &mut self.app.session;
        let mut range = None;
        if let Some(t) = secs("cut") {
            let kind = target.get("trim").and_then(Value::as_str).unwrap_or("roll");
            run(s, "trim.selectNearest", json!({"kind": kind}))?;
            let pad = secs("pad").unwrap_or(1.5).max(0.05);
            range = Some((t - pad, t + pad));
        }
        if let Some([a, b]) = target
            .get("range")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
            .and_then(|r| <[f64; 2]>::try_from(r).ok())
        {
            range = Some((a, b));
        }
        if let Some((a, b)) = range {
            if !(a.is_finite() && b.is_finite() && b > a) {
                return Err("`range` needs [from, to] seconds with to > from".into());
            }
            self.app.ui.timeline.fit_range = Some((a, b));
        }
        if let Some(u) = target.get("ui") {
            self.control("ui.set", u.clone())?;
        }
        let s = &self.app.session;
        Ok(
            json!({"playhead": s.playhead().seconds(), "selection": s.state.selection.iter().map(|c| c.0).collect::<Vec<_>>(), "editPoints": s.state.edit_points}),
        )
    }

    /// Call from the host's `eframe::App::raw_input_hook`: the input [`Self::control`] queues
    /// (`ui.click`, `ui.drag`, `ui.key`, ...) joins the window's input there, before egui decides
    /// which widget is under the pointer. Without it queued pointer input reaches no widget.
    pub fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        eframe::App::raw_input_hook(&mut self.app, ctx, raw);
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.run(ui, frame, None);
    }

    /// Draw and run only `panel` (one of [`Self::panels`]) inside `ui`'s rect, one frame: no
    /// header, tabs or status bar. Its dialogs and menus stay inside that rect.
    pub fn panel_ui(&mut self, panel: &str, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let Some(p) = PanelKind::from_name(panel) else {
            log::warn!("no FilmCraft panel `{panel}`");
            return;
        };
        self.run(ui, frame, Some(p));
    }

    fn run(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, solo: Option<PanelKind>) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let over = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| rect.contains(p));
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.focused = over;
        }
        self.app.input_active = over || self.focused;
        // The desk's turn runs on its own input: the host's events, with the content rect narrowed
        // to the desk (every Window, Area, menu, popup, tooltip and modal backdrop egui places
        // within ctx.content_rect() stays inside it). The host gets its own input back afterwards.
        let host_in = ctx.input(|i| i.clone());
        let mut raw = host_in.raw.clone();
        let vp = host_in.viewport_rect();
        raw.screen_rect = Some(vp);
        raw.time = Some(host_in.time);
        raw.safe_area_insets = Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
            left: (rect.min.x - vp.min.x).max(0.0),
            right: (vp.max.x - rect.max.x).max(0.0),
            top: (rect.min.y - vp.min.y).max(0.0),
            bottom: (vp.max.y - rect.max.y).max(0.0),
        }));
        let prev = self.input.take().unwrap_or_else(|| host_in.clone());
        let desk_in = prev.begin_pass(raw, false, host_in.pixels_per_point, ctx.options(|o| o.input_options));
        ctx.input_mut(|i| *i = desk_in);
        // the host's look goes back in place when the desk is done; the desk's comes in for its turn
        let host = Look::of(&ctx);
        let zoom = ctx.zoom_factor();
        if let Some(look) = &self.look {
            look.put(&ctx);
        }
        self.app.solo = solo;
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt("filmcraft"), |ui| {
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            eframe::App::logic(&mut self.app, &ctx, frame);
            ui.set_style(ctx.global_style());
            eframe::App::ui(&mut self.app, ui, frame);
        });
        self.app.solo = None;
        self.look = Some(Look::of(&ctx));
        host.put(&ctx);
        if ctx.zoom_factor() != zoom {
            ctx.set_zoom_factor(zoom);
        }
        let after = ctx.input(|i| i.clone());
        let focused = self.focused;
        ctx.input_mut(|i| {
            *i = host_in;
            // keys typed into the desk are the desk's while it has focus
            if focused {
                i.events
                    .retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_)));
            }
        });
        self.input = Some(after);
    }
}

// ---------------------------------------------------------------- command parameter notes

/// Split `s` at `sep` outside quotes, brackets, braces and parentheses.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut depth, mut quoted, mut start) = (Vec::new(), 0i32, false, 0);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '{' | '(' if !quoted => depth += 1,
            ']' | '}' | ')' if !quoted => depth -= 1,
            c if c == sep && !quoted && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `text (note)` -> (`text`, note).
fn strip_note(s: &str) -> (&str, Option<&str>) {
    let s = s.trim();
    if s.ends_with(')')
        && let Some(i) = s.find(" (")
    {
        return (s[..i].trim(), Some(s[i + 2..s.len() - 1].trim()));
    }
    (s, None)
}

/// `"name":rest` or `"name"?:rest` -> (name, optional, rest).
fn key_of(part: &str) -> Option<(&str, bool, &str)> {
    let p = part.trim().strip_prefix('"')?;
    let end = p.find('"')?;
    let name = &p[..end];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
        return None;
    }
    let rest = &p[end + 1..];
    let (opt, rest) = match rest.strip_prefix('?') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    Some((name, opt, rest.strip_prefix(':')?))
}

/// The fields a command's parameter note describes (`{"a":type,...}`, alternatives joined by `|`).
fn spec_fields(spec: &str) -> Vec<Value> {
    let groups = split_top(spec.trim(), '|');
    let either = groups.len() > 1;
    let mut out = Vec::new();
    for g in groups {
        let Some(inner) = strip_note(g).0.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else { continue };
        for mut f in object_fields(inner) {
            if either {
                f["optional"] = true.into();
            }
            out.push(f);
        }
    }
    out
}

fn object_fields(inner: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for entry in split_top(inner, ',') {
        let parts = split_top(entry.trim(), '|');
        // `"a":x|"b":y`: either parameter
        let either = parts.iter().filter(|p| key_of(p).is_some()).count() > 1;
        let mut cur: Option<(&str, bool, Vec<&str>)> = None;
        for part in parts {
            if let Some((name, opt, rest)) = key_of(part) {
                if let Some(c) = cur.take() {
                    out.push(field(c, either));
                }
                cur = Some((name, opt, vec![rest]));
            } else if let Some(c) = cur.as_mut() {
                c.2.push(part);
            }
        }
        if let Some(c) = cur {
            out.push(field(c, either));
        }
    }
    out
}

fn field((name, opt, alts): (&str, bool, Vec<&str>), either: bool) -> Value {
    let mut f = type_of(&alts);
    f["id"] = name.into();
    if opt || either {
        f["optional"] = true.into();
    }
    f
}

/// The type of one parameter from its alternatives (`ticks`, `"a|b"`, `0..8=5`, `[id]`, `null`...).
fn type_of(alts: &[&str]) -> Value {
    let mut f = json!({"optional": false});
    let (mut kinds, mut choices, mut notes): (Vec<&str>, Vec<String>, Vec<&str>) = (Vec::new(), Vec::new(), Vec::new());
    let mut open = false;
    for a in alts {
        let (a, note) = strip_note(a);
        notes.extend(note);
        let mut a = a.trim();
        if let Some(r) = a.strip_suffix('?') {
            f["optional"] = true.into();
            a = r.trim();
        }
        let parts = split_top(a, '=');
        if let [ty, d] = parts.as_slice() {
            let d = d.trim();
            f["default"] = serde_json::from_str(d).unwrap_or_else(|_| Value::String(d.trim_matches('"').into()));
            a = ty.trim();
        }
        if a.starts_with('"') {
            for c in a.trim_matches('"').split('|') {
                if c == "\u{2026}" || c == "..." {
                    open = true;
                } else {
                    choices.push(c.into());
                }
            }
            continue;
        }
        if a == "null" {
            f["nullable"] = true.into();
            continue;
        }
        if let Some(inner) = a.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            kinds.push("array");
            f["items"] = type_of(&split_top(inner, '|'));
            continue;
        }
        if let Some(inner) = a.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            if split_top(inner, ',').first().is_some_and(|e| key_of(e).is_some()) {
                kinds.push("object");
                f["fields"] = Value::Array(object_fields(inner));
            } else {
                kinds.push("untyped");
            }
            continue;
        }
        if let Some((lo, hi)) = a.split_once("..")
            && let (Ok(l), Ok(h)) = (lo.parse::<f64>(), hi.parse::<f64>())
        {
            kinds.push(if lo.contains('.') || hi.contains('.') || (h - l) <= 1.0 { "number" } else { "int" });
            f["min"] = l.into();
            f["max"] = h.into();
            continue;
        }
        kinds.push(match a {
            "bool" => "bool",
            "str" | "label" => "string",
            "id" | "itemId" | "binId" => "id",
            "ticks" | "mediaTicks" | "timelineTicks" => "ticks",
            "i64" | "int" | "n" | "index" | "frames" => "int",
            "u32" => {
                f["min"] = 0.into();
                "int"
            }
            "f64" | "f32" | "f" | "seconds" | "percent" => "number",
            _ => "untyped",
        });
    }
    let mut uniq: Vec<&str> = Vec::new();
    for k in kinds {
        if !uniq.contains(&k) {
            uniq.push(k);
        }
    }
    let kinds = uniq;
    let kind = if kinds.contains(&"untyped") {
        "untyped".to_string()
    } else if kinds.is_empty() {
        if choices.is_empty() {
            "untyped"
        } else if open {
            "string"
        } else {
            "choice"
        }
        .to_string()
    } else {
        kinds.join("|")
    };
    if !choices.is_empty() {
        // named values next to a type ("V1"|id) or an open list ("cross_dissolve|...") are examples
        f[if kinds.is_empty() && !open { "choices" } else { "examples" }] = choices.into();
    }
    if kind.split('|').any(|k| k == "ticks") {
        f["unit"] = "ticks".into();
        f["perSecond"] = filmcraft_time::TICKS_PER_SECOND.into();
    }
    if !notes.is_empty() {
        f["note"] = notes.join("; ").into();
    }
    f["kind"] = kind.into();
    f
}

fn is_typed(f: &Value) -> bool {
    !f["kind"].as_str().unwrap_or("untyped").contains("untyped")
        && f.get("items").is_none_or(is_typed)
        && f.get("fields").and_then(Value::as_array).is_none_or(|v| v.iter().all(is_typed))
}
