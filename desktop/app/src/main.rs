//! CAT desktop — native Windows annotation workstation.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod editor;
mod form;
mod map;
mod server;
mod saver;
mod settings;
mod table;
mod tiles;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cat_core::{fields, store};
use catalog::Catalog;
use editor::{Editor, Tool};
use egui::{Color32, RichText};
use form::{Form, FormAction};
use map::{Camera, Nav};
use server::{Server, ServerProfile, SyncStatus};
use settings::{data_dir, Settings};
use tiles::{Layer, LayerState, TileManager};

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../docs/icon.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("CAT — Coral Annotation Tool")
        .with_inner_size([1500.0, 900.0])
        .with_min_inner_size([900.0, 560.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let saved = Settings::load();
    let renderer = match std::env::var("CAT_RENDERER").as_deref() {
        Ok("glow") => eframe::Renderer::Glow,
        Ok(_) => eframe::Renderer::Wgpu,
        Err(_) if saved.renderer == "opengl" => eframe::Renderer::Glow,
        Err(_) => eframe::Renderer::Wgpu,
    };
    // Drawing tools need the line to stick to the cursor: queue one frame,
    // not two (the default favours throughput over input latency).
    let wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        surface: if saved.low_latency { eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY } else { eframe::egui_wgpu::SurfaceConfig::HIGH_THROUGHPUT },
        ..Default::default()
    };
    let options = eframe::NativeOptions { viewport, renderer, wgpu_options, ..Default::default() };
    // Command line: CAT.exe [gs://… or https://… COG URLs] [--zoom N]
    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial: Vec<String> = args.iter().filter(|a| a.contains("://")).cloned().collect();
    let zoom = args.iter().position(|a| a == "--zoom").and_then(|i| args.get(i + 1)).and_then(|z| z.parse::<f64>().ok());
    eframe::run_native(
        "CAT",
        options,
        Box::new(move |cc| {
            let mut app = CatApp::new(cc, initial);
            app.start_zoom = zoom;
            Ok(Box::new(app))
        }),
    )
}

/// Counts time the user is actively working (input within the last 2 min),
/// for the session clock and `annotation_time_seconds`.
type LegendKey = (u64, editor::ColorBy);
type LegendEntry = (Option<String>, Color32, usize);

struct ActiveTimer {
    last_input: Instant,
    secs: f64,
    /// Active time since the last real break (5+ minutes idle).
    since_break: f64,
}

impl ActiveTimer {
    fn tick(&mut self, had_input: bool) {
        if had_input {
            let gap = self.last_input.elapsed().as_secs_f64();
            if gap < 120.0 {
                self.secs += gap;
                self.since_break += gap;
            } else if gap >= 300.0 {
                self.since_break = 0.0;
            }
            self.last_input = Instant::now();
        }
    }
}

#[derive(PartialEq)]
enum SaveState {
    Saved,
    Pending,
    Failed(String),
}

struct CatApp {
    tiles: TileManager,
    catalog: Catalog,
    layers: Vec<Layer>,
    next_id: u64,
    cam: Camera,
    fit_pending: Option<u64>,
    map_rect: egui::Rect,
    hover: Option<(f64, f64)>,
    level: Option<(usize, f64)>,
    url_input: String,
    show_shortcuts: bool,
    errors: Vec<String>,
    start_zoom: Option<f64>,

    settings: Settings,
    editor: Editor,
    form: Form,
    table: table::TableState,
    timer: ActiveTimer,
    /// Layer whose annotations are being edited, and where they're saved.
    active: Option<(u64, PathBuf)>,
    saved_rev: u64,
    last_change: Instant,
    save_state: SaveState,
    restore_view: Option<[f64; 3]>,
    /// Last settings written, to persist only when something changed.
    persisted: String,
    persist_check: Instant,
    saver: saver::Saver,
    annotated: std::collections::HashSet<PathBuf>,
    gfx_info: Option<String>,
    legend_cache: (LegendKey, Vec<LegendEntry>),
    /// Camera animation: (from, to, start).
    fly: Option<(Camera, Camera, Instant)>,
    bookmark_name: String,
    /// Cat buddy: snoozed until this much active time; annotations made this session.
    break_snooze_until: f64,
    session_created: usize,
    software_gpu: bool,
    frame_ms: std::collections::VecDeque<f32>,
    show_perf: bool,
    annotated_at: Instant,
    seen_rev: u64,
    first_unsaved: Instant,
    last_title: String,

    server: Server,
    login: LoginForm,
    show_login: bool,
    show_conflicts: bool,
    project_query: String,
    /// Editor revision last considered for sync, and when it changed.
    sync_rev: u64,
    pending_sync: usize,
}

#[derive(Default)]
struct LoginForm {
    profile: Option<usize>,
    name: String,
    url: String,
    username: String,
    password: String,
    remember: bool,
    error: Option<String>,
    /// Profile being signed in to; saved to settings only on success.
    pending: Option<ServerProfile>,
}

fn annotations_path(uri: &str) -> PathBuf {
    let stem = uri.rsplit('/').next().unwrap_or(uri).trim_end_matches(".tif").trim_end_matches(".tiff");
    let safe: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    data_dir().join("annotations").join(format!("{safe}.geojson"))
}

impl CatApp {
    fn new(cc: &eframe::CreationContext<'_>, initial: Vec<String>) -> Self {
        let ctx = &cc.egui_ctx;
        let settings = Settings::load();
        let mut editor = Editor::new();
        editor.style = settings.style.clone();
        editor.snap = settings.snap;
        editor.new_props = settings.sticky.clone();
        let mut tiles = TileManager::new(ctx.clone());
        tiles.set_background_transparent(settings.hide_background);
        let mut app = Self {
            tiles,
            catalog: Catalog::new(ctx),
            layers: Vec::new(),
            next_id: 1,
            cam: Camera::default(),
            fit_pending: None,
            map_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0)),
            hover: None,
            level: None,
            url_input: String::new(),
            show_shortcuts: false,
            errors: Vec::new(),
            start_zoom: None,
            restore_view: None,
            settings,
            editor,
            form: Form::new(),
            table: table::TableState::default(),
            timer: ActiveTimer { last_input: Instant::now(), secs: 0.0, since_break: 0.0 },
            active: None,
            saved_rev: 0,
            last_change: Instant::now(),
            save_state: SaveState::Saved,
            persisted: String::new(),
            persist_check: Instant::now(),
            saver: saver::Saver::new(ctx.clone()),
            annotated: Default::default(),
            gfx_info: None,
            legend_cache: ((u64::MAX, editor::ColorBy::Status), Vec::new()),
            fly: None,
            bookmark_name: String::new(),
            break_snooze_until: 0.0,
            session_created: 0,
            software_gpu: false,
            frame_ms: Default::default(),
            show_perf: false,
            annotated_at: Instant::now() - Duration::from_secs(60),
            seen_rev: 0,
            first_unsaved: Instant::now(),
            last_title: String::new(),
            server: Server::new(ctx),
            login: LoginForm { remember: true, ..Default::default() },
            show_login: false,
            show_conflicts: false,
            project_query: String::new(),
            sync_rev: 0,
            pending_sync: 0,
        };
        // Resume the last server session (token kept in Windows Credential Manager).
        if let Some(p) = app.settings.server.and_then(|i| app.settings.servers.get(i)).cloned() {
            app.server.resume(p);
        }
        if initial.is_empty() && app.settings.last_project.is_some() && app.settings.server.is_some() {
            // The server project reopens after sign-in resumes.
            if let Some(v) = app.settings.last_view {
                app.restore_view = Some(v);
            }
        } else if initial.is_empty() {
            // Pick up where the user left off.
            let last = app.settings.last_layers.clone();
            for (i, uri) in last.into_iter().enumerate() {
                app.open_layer(uri, i == 0);
            }
            if let Some(v) = app.settings.last_view {
                app.restore_view = Some(v);
                app.fit_pending = None;
            }
        } else {
            for (i, uri) in initial.into_iter().enumerate() {
                app.open_layer(uri, i == 0);
            }
        }
        app
    }

    // ---------------------------------------------------------------------
    // Layers and annotation files
    // ---------------------------------------------------------------------

    fn open_layer(&mut self, uri: String, replace: bool) {
        if replace {
            self.close_project();
            self.save_now();
            for l in self.layers.drain(..) {
                self.tiles.forget_layer(l.id);
            }
            self.active = None;
        }
        if let Some(l) = self.layers.iter().find(|l| l.uri == uri) {
            self.fit_pending = Some(l.id);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = uri.rsplit('/').next().unwrap_or(&uri).trim_end_matches(".tif").to_string();
        self.tiles.open(id, uri.clone());
        self.layers.push(Layer { id, name, uri: uri.clone(), state: LayerState::Opening, visible: true, opacity: 1.0 });
        self.fit_pending = Some(id);
        if self.active.is_none() && self.server.open.is_none() {
            self.activate(id);
        }
    }

    /// Make `id` the mosaic being annotated: save the current set, load its own.
    fn activate(&mut self, id: u64) {
        let Some(uri) = self.layers.iter().find(|l| l.id == id).map(|l| l.uri.clone()) else { return };
        self.form.commit(&mut self.editor);
        self.save_now();
        let path = annotations_path(&uri);
        let items = if path.exists() {
            match store::load_geojson(&path) {
                Ok(v) => v,
                Err(e) => {
                    self.errors.push(format!("Could not read {}: {e}", path.display()));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        self.editor.load(items);
        self.saved_rev = self.editor.rev;
        self.save_state = SaveState::Saved;
        // Year and site come from the mosaic name (2026_BAK-2089_mos_cog.tif).
        for (k, v) in fields::defaults_from_file_name(&uri) {
            self.settings.sticky.insert(k.into(), v);
        }
        self.editor.new_props = self.settings.sticky.clone();
        self.editor.timer_mark = self.timer.secs;
        self.active = Some((id, path));
    }

    /// Build the write job for the current annotation set (None = nothing open).
    fn save_job(&self) -> Option<saver::Job> {
        if let Some(open) = &self.server.open {
            // Server project: the journal keeps unsynced work safe on disk.
            let (path, items, sync) = (open.journal.clone(), self.editor.items.clone(), open.sync.clone());
            return Some(Box::new(move || server::write_journal(&path, &items, &sync)));
        }
        let (id, path) = self.active.clone()?;
        let uri = self.layers.iter().find(|l| l.id == id).map(|l| l.uri.clone()).unwrap_or_default();
        let meta = serde_json::json!({"cog": uri, "app": format!("cat-desktop {}", env!("CARGO_PKG_VERSION")), "saved_at": cat_core::annotation::now_iso()});
        let items = self.editor.items.clone();
        Some(Box::new(move || store::save_geojson(&path, &items, meta)))
    }

    /// Save immediately on this thread (project switches, Ctrl+S, exit).
    fn save_now(&mut self) {
        if self.editor.rev == self.saved_rev {
            return;
        }
        if let Some(job) = self.save_job() {
            match job() {
                Ok(()) => {
                    self.saved_rev = self.editor.rev;
                    self.save_state = SaveState::Saved;
                }
                Err(e) => self.save_state = SaveState::Failed(e.to_string()),
            }
        }
    }

    /// Save in the background ~1 s after the last change (at most every 10 s
    /// while changes keep coming), never blocking drawing.
    fn autosave(&mut self) {
        for (rev, r) in self.saver.poll() {
            match r {
                Ok(()) => {
                    self.saved_rev = self.saved_rev.max(rev);
                    if self.saved_rev == self.editor.rev {
                        self.save_state = SaveState::Saved;
                    }
                }
                Err(e) => self.save_state = SaveState::Failed(e),
            }
        }
        if self.editor.rev != self.seen_rev {
            if self.editor.rev != self.saved_rev && self.seen_rev == self.saved_rev {
                self.first_unsaved = Instant::now();
            }
            self.seen_rev = self.editor.rev;
            self.last_change = Instant::now();
        }
        if self.editor.rev == self.saved_rev || self.saver.submitted == self.editor.rev {
            return;
        }
        if !matches!(self.save_state, SaveState::Failed(_)) {
            self.save_state = SaveState::Pending;
        }
        let quiet = self.last_change.elapsed() > Duration::from_millis(1000) && !self.editor.is_drawing();
        let overdue = self.first_unsaved.elapsed() > Duration::from_secs(10);
        if quiet || overdue {
            if let Some(job) = self.save_job() {
                self.saver.submit(self.editor.rev, job);
            }
        }
    }

    /// Save and leave the open server project (back to local mode).
    fn close_project(&mut self) {
        if self.server.open.is_some() {
            self.form.commit(&mut self.editor);
            self.save_now();
            if let Some(o) = &self.server.open {
                let _ = server::write_journal(&o.journal, &self.editor.items, &o.sync);
            }
            self.server.open = None;
            self.settings.last_project = None;
            self.editor.read_only = false;
            self.editor.load(Vec::new());
            self.saved_rev = self.editor.rev;
        }
    }

    fn on_project_loaded(&mut self, snap: cat_api::Snapshot) {
        self.form.commit(&mut self.editor);
        self.save_now();
        self.close_project();
        for l in self.layers.drain(..) {
            self.tiles.forget_layer(l.id);
        }
        self.active = None;
        let base = self.server.client.as_ref().map(|c| c.base().to_string()).unwrap_or_default();
        let journal = server::journal_path(&base, snap.project.project_id);
        let (items, sync, kept) = cat_api::sync::merge_snapshot(&snap.annotations, server::read_journal(&journal));
        let can_edit = snap.can_edit();
        let mut skipped = Vec::new();
        let assets: Vec<cat_api::Asset> = snap.assets.iter().filter(|a| a.is_active.unwrap_or(1) != 0).cloned().collect();
        self.server.open = Some(server::OpenProject {
            project: snap.project.clone(),
            can_edit,
            role: snap.my_role.clone(),
            asset_id: assets.first().map(|a| a.asset_id),
            sync,
            journal,
        });
        for a in &assets {
            let u = a.cog_url.trim();
            if u.starts_with("gs://") || u.starts_with("/vsigs/") || u.starts_with("http://") || u.starts_with("https://") {
                self.open_layer(u.to_string(), false);
            } else {
                skipped.push(a.asset_name.clone());
            }
        }
        if !skipped.is_empty() {
            self.errors.push(format!("These project images are stored on the server's own disk, not in the bucket, so the desktop can't show them: {}", skipped.join(", ")));
        }
        self.editor.read_only = !can_edit;
        self.editor.load(items);
        self.saved_rev = self.editor.rev;
        self.sync_rev = 0;
        if let Some(site) = snap.project.site.clone().filter(|s| !s.is_empty()) {
            self.settings.sticky.insert("site".into(), serde_json::json!(site.to_uppercase()));
        }
        if let Some(y) = snap.project.year {
            self.settings.sticky.insert("obs_year".into(), serde_json::json!(y));
        }
        self.editor.new_props = self.settings.sticky.clone();
        self.editor.timer_mark = self.timer.secs;
        self.settings.last_project = Some(snap.project.project_id);
        if kept > 0 {
            self.editor.toast = Some((format!("Recovered {kept} unsynced change(s) from last session — syncing now"), Instant::now()));
        } else if !can_edit {
            self.editor.toast = Some(("View only — your role on this project is viewer".into(), Instant::now()));
        }
    }

    fn handle_server_events(&mut self) {
        for ev in self.server.poll() {
            match ev {
                server::Event::SignedIn => {
                    if let Some(profile) = self.login.pending.take() {
                        let idx = match self.settings.servers.iter().position(|p| p.url == profile.url && p.name == profile.name) {
                            Some(i) => {
                                self.settings.servers[i] = profile;
                                i
                            }
                            None => {
                                self.settings.servers.push(profile);
                                self.settings.servers.len() - 1
                            }
                        };
                        self.settings.server = Some(idx);
                        self.login.profile = Some(idx);
                    }
                    self.show_login = false;
                    self.login.password.clear();
                    self.login.error = None;
                    let scope = self.settings.project_scope.clone();
                    self.server.refresh_projects("", &scope);
                    if !self.settings.sticky.contains_key("analyst") {
                        if let Some(u) = &self.server.user {
                            self.settings.sticky.insert("analyst".into(), serde_json::json!(u.name().to_uppercase()));
                            self.editor.new_props = self.settings.sticky.clone();
                        }
                    }
                    if let Some(pid) = self.settings.last_project {
                        if self.server.open.is_none() && self.server.loading_project.is_none() {
                            self.server.load_project(pid);
                        }
                    }
                }
                server::Event::SignedOut(msg) => {
                    if !msg.is_empty() {
                        self.login.error = Some(msg);
                        self.show_login = true;
                    }
                }
                server::Event::ProjectLoaded(snap) => self.on_project_loaded(*snap),
                server::Event::ProjectFailed(e) => self.errors.push(format!("Could not open project: {e}")),
                server::Event::SpeciesUpdated(list) => self.form.species = cat_core::species::SpeciesList { items: list },
                server::Event::OptionsUpdated(o) => self.form.apply_options(&o),
                server::Event::Conflicts(n) => {
                    self.show_conflicts = true;
                    self.editor.toast = Some((format!("{n} annotation(s) were changed by someone else — choose which version to keep"), Instant::now()));
                }
            }
        }
    }

    /// Kick off a sync round when local work differs from the server.
    /// Kick off a sync round when local work may differ from the server.
    fn drive_sync(&mut self) {
        if self.server.open.is_none() {
            return;
        }
        let quiet = self.last_change.elapsed() > Duration::from_millis(700) && !self.editor.is_drawing();
        let retry_due = matches!(&self.server.status, SyncStatus::Offline { retry_at, .. } if Instant::now() >= *retry_at);
        let more = self.server.pending > 0 && self.server.status == SyncStatus::Idle;
        if (quiet && self.editor.rev != self.sync_rev) || retry_due || (quiet && more) {
            self.sync_rev = self.editor.rev;
            let analyst = self.settings.sticky.get("analyst").and_then(|v| v.as_str()).map(str::to_string);
            self.server.maybe_sync(&self.editor.items, analyst);
        }
        if self.editor.rev != self.sync_rev && self.server.status == SyncStatus::Idle {
            self.pending_sync = self.server.pending.max(1);
        } else {
            self.pending_sync = self.server.pending;
        }
    }

    fn fit_to(&mut self, id: u64) {
        if let Some(cog) = self.layers.iter().find(|l| l.id == id).and_then(|l| l.cog()) {
            self.cam.fit(self.map_rect, cog.merc_bounds());
            if let Some(z) = self.start_zoom.take() {
                self.cam.mpp /= z;
            }
        }
    }

    fn fit_all(&mut self) {
        let mut b: Option<cat_cog::MercBox> = None;
        for c in self.layers.iter().filter_map(|l| l.cog()) {
            let m = c.merc_bounds();
            b = Some(match b {
                None => m,
                Some(o) => cat_cog::MercBox { min_x: o.min_x.min(m.min_x), min_y: o.min_y.min(m.min_y), max_x: o.max_x.max(m.max_x), max_y: o.max_y.max(m.max_y) },
            });
        }
        if let Some(b) = b {
            self.cam.fit(self.map_rect, b);
        }
    }

    /// Centre on a lon/lat bbox; zoom in only if it doesn't fit comfortably.
    fn zoom_to_bbox(&mut self, b: [f64; 4]) {
        let (x0, y0) = cat_cog::geo::lonlat_to_merc(b[0], b[1]);
        let (x1, y1) = cat_cog::geo::lonlat_to_merc(b[2], b[3]);
        let mb = cat_cog::MercBox { min_x: x0, min_y: y0, max_x: x1, max_y: y1 };
        let (cx, cy) = mb.center();
        let need = (mb.width() / (self.map_rect.width() as f64 * 0.5)).max(mb.height() / (self.map_rect.height() as f64 * 0.5));
        let target = Camera { cx, cy, mpp: self.cam.mpp.max(need) };
        self.fly_to(target);
    }

    /// Smoothly move the camera (eased pan + log-scale zoom).
    fn fly_to(&mut self, to: Camera) {
        self.fly = Some((self.cam, to, Instant::now()));
    }

    fn animate(&mut self, ctx: &egui::Context) {
        let Some((from, to, start)) = self.fly else { return };
        let t = (start.elapsed().as_secs_f64() / 0.35).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: f64, b: f64| a + (b - a) * e;
        self.cam = Camera { cx: lerp(from.cx, to.cx), cy: lerp(from.cy, to.cy), mpp: (lerp(from.mpp.ln(), to.mpp.ln())).exp() };
        if t >= 1.0 {
            self.fly = None;
        } else {
            ctx.request_repaint();
        }
    }

    /// Bookmarks belong to the open project (server) or mosaic (local).
    fn bookmark_key(&self) -> Option<String> {
        if let Some(o) = &self.server.open {
            let base = self.server.client.as_ref().map(|c| c.base().to_string()).unwrap_or_default();
            return Some(format!("{base}#project-{}", o.project.project_id));
        }
        let (id, _) = self.active.as_ref()?;
        self.layers.iter().find(|l| l.id == *id).map(|l| l.uri.clone())
    }

    fn bookmarks_ui(&mut self, ui: &mut egui::Ui) {
        let Some(key) = self.bookmark_key() else { return };
        let list = self.settings.bookmarks.get(&key).cloned().unwrap_or_default();
        egui::CollapsingHeader::new(RichText::new(format!("Bookmarks ({})", list.len())).strong()).id_salt("bookmarks").default_open(!list.is_empty()).show(ui, |ui| {
            let mut go = None;
            let mut del = None;
            for (i, b) in list.iter().enumerate() {
                ui.horizontal(|ui| {
                    let label = if i < 9 { format!("{}  {}", i + 1, b.name) } else { b.name.clone() };
                    if ui.selectable_label(false, label).on_hover_text("Fly to this view (number key)").clicked() {
                        go = Some(i);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").clicked() {
                            del = Some(i);
                        }
                    });
                });
            }
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.bookmark_name).hint_text(format!("View {}", list.len() + 1)).desired_width(130.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("＋ Save view").on_hover_text("Ctrl+B").clicked() || enter {
                    self.save_bookmark();
                }
            });
            if let Some(i) = go {
                self.go_bookmark(i);
            }
            if let (Some(i), Some(v)) = (del, self.settings.bookmarks.get_mut(&key)) {
                v.remove(i);
            }
        });
    }

    fn save_bookmark(&mut self) {
        let Some(key) = self.bookmark_key() else { return };
        let list = self.settings.bookmarks.entry(key).or_default();
        let name = if self.bookmark_name.trim().is_empty() { format!("View {}", list.len() + 1) } else { self.bookmark_name.trim().to_string() };
        list.push(settings::Bookmark { name: name.clone(), view: [self.cam.cx, self.cam.cy, self.cam.mpp] });
        self.bookmark_name.clear();
        self.editor.toast = Some((format!("Saved bookmark '{name}'"), Instant::now()));
    }

    fn go_bookmark(&mut self, i: usize) {
        let Some(b) = self.bookmark_key().and_then(|k| self.settings.bookmarks.get(&k)).and_then(|l| l.get(i)).cloned() else { return };
        self.fly_to(Camera { cx: b.view[0], cy: b.view[1], mpp: b.view[2] });
    }

    fn export(&mut self, csv: bool) {
        self.form.commit(&mut self.editor);
        let stem = self.active.as_ref().and_then(|(_, p)| p.file_stem().map(|s| s.to_string_lossy().to_string())).unwrap_or_else(|| "annotations".into());
        let (ext, name) = if csv { ("csv", "CSV") } else { ("geojson", "GeoJSON") };
        let Some(path) = rfd::FileDialog::new().add_filter(name, &[ext]).set_file_name(format!("{stem}_annotations.{ext}")).save_file() else { return };
        let result = if csv {
            store::to_csv(&self.editor.items).map_err(|e| e.to_string()).and_then(|t| std::fs::write(&path, t).map_err(|e| e.to_string()))
        } else {
            let meta = serde_json::json!({"exported_at": cat_core::annotation::now_iso()});
            store::save_geojson(&path, &self.editor.items, meta).map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => self.editor.toast = Some((format!("Exported {} annotations to {}", self.editor.items.len(), path.display()), Instant::now())),
            Err(e) => self.errors.push(format!("Export failed: {e}")),
        }
    }

    // ---------------------------------------------------------------------
    // UI
    // ---------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.label(RichText::new("🐠 CAT").strong().size(16.0));
            ui.separator();
            ui.menu_button("File", |ui| {
                ui.label("Open COG by URL (gs:// or https://)");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.url_input);
                    if ui.button("Open").clicked() && !self.url_input.trim().is_empty() {
                        let u = self.url_input.trim().to_string();
                        self.open_layer(u, false);
                        ui.close();
                    }
                });
                ui.separator();
                if ui.button("Save now   Ctrl+S").clicked() {
                    self.form.commit(&mut self.editor);
                    self.save_now();
                    ui.close();
                }
                if ui.add_enabled(!self.editor.items.is_empty(), egui::Button::new("Export GeoJSON…   Ctrl+E")).clicked() {
                    ui.close();
                    self.export(false);
                }
                if ui.add_enabled(!self.editor.items.is_empty(), egui::Button::new("Export CSV…")).clicked() {
                    ui.close();
                    self.export(true);
                }
                ui.separator();
                if ui.button("Open data folder").clicked() {
                    let d = data_dir();
                    #[cfg(windows)]
                    let _ = std::process::Command::new("explorer").arg(&d).spawn();
                    #[cfg(not(windows))]
                    let _ = std::process::Command::new("xdg-open").arg(&d).spawn();
                    ui.close();
                }
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                let undo = self.editor.history.can_undo().map(|l| format!("Undo {l}   Ctrl+Z"));
                if ui.add_enabled(undo.is_some(), egui::Button::new(undo.unwrap_or_else(|| "Undo   Ctrl+Z".into()))).clicked() {
                    self.form.commit(&mut self.editor);
                    self.editor.undo();
                    ui.close();
                }
                let redo = self.editor.history.can_redo().map(|l| format!("Redo {l}   Ctrl+Y"));
                if ui.add_enabled(redo.is_some(), egui::Button::new(redo.unwrap_or_else(|| "Redo   Ctrl+Y".into()))).clicked() {
                    self.editor.redo();
                    ui.close();
                }
                ui.separator();
                if ui.add_enabled(self.editor.selected.is_some(), egui::Button::new("Delete selected   Del")).clicked() {
                    self.editor.delete_selected();
                    ui.close();
                }
                if ui.add_enabled(self.editor.selected.is_some() && self.form.last_props.is_some(), egui::Button::new("Same as last   Ctrl+D")).clicked() {
                    self.form.same_as_last(&mut self.editor);
                    ui.close();
                }
                ui.separator();
                ui.checkbox(&mut self.editor.snap, "Snap to vertices   S");
            });
            ui.menu_button("View", |ui| {
                if ui.button("Fit to imagery   F").clicked() {
                    self.fit_all();
                    ui.close();
                }
                ui.checkbox(&mut self.settings.show_form, "Attribute panel   A");
                ui.checkbox(&mut self.settings.show_table, "Annotation table   T");
                ui.separator();
                ui.checkbox(&mut self.editor.style.labels, "Species labels");
                ui.menu_button("Colour annotations by", |ui| {
                    for m in editor::ColorBy::ALL {
                        ui.radio_value(&mut self.editor.style.color_by, m, m.label());
                    }
                });
                ui.checkbox(&mut self.settings.show_legend, "Legend");
                ui.horizontal(|ui| {
                    ui.label("🐱 Break reminder");
                    ui.add(egui::DragValue::new(&mut self.settings.break_minutes).range(0..=180).suffix(" min"));
                    ui.weak("(0 = off)");
                });
                ui.horizontal(|ui| {
                    ui.label("Line width");
                    ui.add(egui::Slider::new(&mut self.editor.style.line_width, 1.0..=8.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Fill");
                    ui.add(egui::Slider::new(&mut self.editor.style.fill_opacity, 0.0..=0.8));
                });
                ui.horizontal(|ui| {
                    ui.label("Colours: complete");
                    ui.color_edit_button_srgb(&mut self.editor.style.complete);
                    ui.label("needs species");
                    ui.color_edit_button_srgb(&mut self.editor.style.incomplete);
                });
                ui.separator();
                let mut bt = self.tiles.background_transparent();
                if ui.checkbox(&mut bt, "Hide mosaic background (white/black border)").changed() {
                    self.tiles.set_background_transparent(bt);
                    self.settings.hide_background = bt;
                }
                ui.separator();
                egui::widgets::global_theme_preference_buttons(ui);
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Performance & graphics…").clicked() {
                    self.show_perf = true;
                    ui.close();
                }
                if ui.button("Keyboard shortcuts   ?").clicked() {
                    self.show_shortcuts = true;
                    ui.close();
                }
                ui.label(format!("CAT desktop {}", env!("CARGO_PKG_VERSION")));
                ui.weak(format!("Data folder: {}", data_dir().display()));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.server_badge(ui));
        });
    }

    fn server_badge(&mut self, ui: &mut egui::Ui) {
        let (text, bg, tip) = match (&self.server.user, &self.server.profile) {
            (Some(u), Some(p)) => (
                format!(" ● {} · {} ", p.name.to_uppercase(), u.name()),
                if p.is_prod() { Color32::from_rgb(190, 40, 40) } else { Color32::from_rgb(40, 110, 160) },
                format!("Signed in to {} as {} ({})", p.url, u.username, u.role),
            ),
            _ if self.server.signing_in => (" Signing in… ".into(), Color32::from_rgb(90, 90, 90), String::new()),
            _ => (" Local mode · Sign in to CAT server ".into(), Color32::from_rgb(70, 70, 70), "Annotations are saved in the data folder next to CAT.exe. Sign in to annotate server projects (saved to the CAT database).".into()),
        };
        let r = ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(bg)).on_hover_text(tip);
        if self.server.connected() {
            let mut sign_out = false;
            egui::Popup::menu(&r).show(|ui| {
                if let Some(o) = &self.server.open {
                    ui.weak(format!("Your role on {}: {}", o.project.project_name, o.role));
                    ui.separator();
                }
                if ui.button("Switch server / account…").clicked() {
                    self.show_login = true;
                }
                if ui.button("Sign out").clicked() {
                    sign_out = true;
                }
            });
            if sign_out {
                self.close_project();
                self.server.sign_out();
                self.settings.server = None;
            }
        } else if r.clicked() {
            self.show_login = true;
        }
    }

    fn projects_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Projects");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    let (q, sc) = (self.project_query.clone(), self.settings.project_scope.clone());
                    self.server.refresh_projects(&q, &sc);
                }
                let mut changed = false;
                changed |= ui.selectable_value(&mut self.settings.project_scope, "all".to_string(), "All").clicked();
                changed |= ui.selectable_value(&mut self.settings.project_scope, "mine".to_string(), "Mine").clicked();
                if changed {
                    let (q, sc) = (self.project_query.clone(), self.settings.project_scope.clone());
                    self.server.refresh_projects(&q, &sc);
                }
            });
        });
        let r = ui.add(egui::TextEdit::singleline(&mut self.project_query).hint_text("🔍 search projects").desired_width(f32::INFINITY));
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || (r.changed() && self.project_query.is_empty()) {
            let (q, sc) = (self.project_query.clone(), self.settings.project_scope.clone());
            self.server.refresh_projects(&q, &sc);
        }
        if self.server.projects_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading projects…");
            });
        }
        if let Some(e) = &self.server.project_error {
            ui.colored_label(Color32::from_rgb(220, 80, 60), e);
        }
        let open_id = self.server.open.as_ref().map(|o| o.project.project_id);
        let mut load = None;
        egui::ScrollArea::vertical().id_salt("projects").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
            for p in &self.server.projects {
                let sub = [p.site.clone().unwrap_or_default(), p.year.map(|y| y.to_string()).unwrap_or_default(), p.region.clone().unwrap_or_default()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                let loading = self.server.loading_project == Some(p.project_id);
                ui.horizontal(|ui| {
                    if ui.selectable_label(open_id == Some(p.project_id), RichText::new(&p.project_name).strong()).on_hover_text(&sub).clicked() {
                        load = Some(p.project_id);
                    }
                    if loading {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(sub);
                    });
                });
            }
            if self.server.projects.is_empty() && !self.server.projects_loading {
                ui.weak("No projects. Try 'All', or create one in the CAT web app.");
            }
        });
        if let Some(id) = load {
            if open_id != Some(id) {
                self.form.commit(&mut self.editor);
                self.save_now();
                self.server.load_project(id);
            }
        }
        ui.separator();
    }

    fn login_window(&mut self, ctx: &egui::Context) {
        if !self.show_login {
            return;
        }
        // Pre-fill from the last-used server profile.
        if self.login.profile.is_none() && self.login.url.is_empty() {
            let idx = self.settings.server.or(if self.settings.servers.is_empty() { None } else { Some(0) });
            if let Some((i, p)) = idx.and_then(|i| self.settings.servers.get(i).map(|p| (i, p.clone()))) {
                self.login.profile = Some(i);
                self.login.name = p.name;
                self.login.url = p.url;
                self.login.username = p.username;
            }
        }
        let mut open = true;
        let mut submit = false;
        let ready = !self.login.url.trim().is_empty() && !self.login.username.trim().is_empty() && !self.login.password.is_empty();
        egui::Window::new("Sign in to a CAT server").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            let servers = self.settings.servers.clone();
            ui.horizontal(|ui| {
                ui.label("Server");
                egui::ComboBox::from_id_salt("server-profile")
                    .selected_text(self.login.profile.and_then(|i| servers.get(i)).map(|p| p.name.clone()).unwrap_or_else(|| "New server…".into()))
                    .show_ui(ui, |ui| {
                        for (i, p) in servers.iter().enumerate() {
                            if ui.selectable_label(self.login.profile == Some(i), format!("{}  ({})", p.name, p.url)).clicked() {
                                self.login.profile = Some(i);
                                self.login.name = p.name.clone();
                                self.login.url = p.url.clone();
                                self.login.username = p.username.clone();
                            }
                        }
                        if ui.selectable_label(self.login.profile.is_none(), "New server…").clicked() {
                            self.login.profile = None;
                            self.login.name.clear();
                            self.login.url.clear();
                        }
                    });
            });
            egui::Grid::new("login").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut self.login.name).hint_text("e.g. Dev or Prod"));
                ui.end_row();
                ui.label("URL");
                ui.add(egui::TextEdit::singleline(&mut self.login.url).hint_text("https://cat.example.gov").desired_width(280.0));
                ui.end_row();
                ui.label("Username");
                ui.text_edit_singleline(&mut self.login.username);
                ui.end_row();
                ui.label("Password");
                let r = ui.add(egui::TextEdit::singleline(&mut self.login.password).password(true));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && ready {
                    submit = true;
                }
                ui.end_row();
            });
            ui.checkbox(&mut self.login.remember, "Keep me signed in on this computer").on_hover_text("The session is stored in Windows Credential Manager, never in a file.");
            if self.login.name.to_lowercase().contains("prod") {
                ui.colored_label(Color32::from_rgb(220, 80, 60), "⚠ Production server — edits go to the live database.");
            }
            if let Some(e) = &self.login.error {
                ui.colored_label(Color32::from_rgb(220, 80, 60), e);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(ready && !self.server.signing_in, egui::Button::new("Sign in")).clicked() {
                    submit = true;
                }
                if self.server.signing_in {
                    ui.spinner();
                }
            });
        });
        if !open {
            self.show_login = false;
        }
        if submit && !self.server.signing_in {
            let mut url = self.login.url.trim().trim_end_matches('/').to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                url = format!("https://{url}");
            }
            let name = if self.login.name.trim().is_empty() { url.trim_start_matches("https://").trim_start_matches("http://").to_string() } else { self.login.name.trim().to_string() };
            let profile = ServerProfile { name, url, username: self.login.username.trim().to_string() };
            self.login.pending = Some(profile.clone());
            self.login.error = None;
            self.close_project();
            let pw = std::mem::take(&mut self.login.password);
            self.server.sign_in(profile, pw, self.login.remember);
        }
    }

    fn conflicts_window(&mut self, ctx: &egui::Context) {
        let n = self.server.open.as_ref().map(|o| o.sync.conflicts.len()).unwrap_or(0);
        if n == 0 {
            self.show_conflicts = false;
            return;
        }
        if !self.show_conflicts {
            return;
        }
        let mut open = true;
        let mut actions: Vec<(String, bool)> = Vec::new();
        egui::Window::new(format!("⚠ {n} annotation(s) changed by someone else"))
            .open(&mut open)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
            .show(ctx, |ui| {
            ui.label("While you were editing, another user changed or deleted these annotations on the server. Choose which version to keep.");
            ui.separator();
            let conflicts: Vec<(String, cat_api::sync::Conflict)> = self.server.open.as_ref().map(|o| o.sync.conflicts.clone().into_iter().collect()).unwrap_or_default();
            egui::Grid::new("conflicts").striped(true).num_columns(4).show(ui, |ui| {
                for (uuid, c) in &conflicts {
                    let mine = self.editor.get(uuid).map(|a| format!("{} {}", a.kind.label(), a.spcode())).unwrap_or_else(|| "(deleted here)".into());
                    let theirs = match c.kind {
                        cat_api::sync::ConflictKind::Deleted => "deleted on server".to_string(),
                        cat_api::sync::ConflictKind::Changed => c.server.as_ref().map(|a| format!("{} {}", a.kind.label(), a.spcode())).unwrap_or_else(|| "changed".into()),
                    };
                    if ui.link(&mine).on_hover_text("Show on map").clicked() {
                        self.editor.selected = Some(uuid.clone());
                    }
                    ui.label(format!("server: {theirs}"));
                    if ui.button("Keep mine").clicked() {
                        actions.push((uuid.clone(), true));
                    }
                    if ui.button("Use server's").clicked() {
                        actions.push((uuid.clone(), false));
                    }
                    ui.end_row();
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Keep all mine").clicked() {
                    actions.extend(conflicts.iter().map(|c| (c.0.clone(), true)));
                }
                if ui.button("Use all server versions").clicked() {
                    actions.extend(conflicts.iter().map(|c| (c.0.clone(), false)));
                }
            });
        });
        if !open {
            self.show_conflicts = false;
        }
        for (uuid, keep_mine) in actions {
            let Some(o) = self.server.open.as_mut() else { break };
            if keep_mine {
                if let Some(new_uuid) = o.sync.keep_mine(&uuid) {
                    if let Some(a) = self.editor.get_mut(&uuid) {
                        a.uuid = new_uuid;
                    }
                }
            } else {
                let replacement = o.sync.use_server(&uuid);
                cat_core::history::apply(&mut self.editor.items, &uuid, replacement.as_ref());
                self.editor.history.clear();
            }
            self.editor.rev += 1;
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(4.0);
        if self.server.connected() {
            self.projects_panel(ui);
        }
        ui.heading("Layers");
        if self.layers.is_empty() {
            ui.weak("Pick an orthomosaic below.");
        }
        let mut remove = None;
        let mut fit = None;
        let mut activate = None;
        let active_id = self.active.as_ref().map(|a| a.0);
        for l in &mut self.layers {
            ui.horizontal(|ui| {
                ui.checkbox(&mut l.visible, "");
                match &l.state {
                    LayerState::Opening => {
                        ui.spinner();
                    }
                    LayerState::Failed(e) => {
                        ui.colored_label(Color32::from_rgb(220, 80, 60), "⚠").on_hover_text(e);
                    }
                    LayerState::Ready(_) => {}
                }
                let is_active = active_id == Some(l.id);
                let in_project = self.server.open.is_some();
                if ui.selectable_label(is_active || in_project, &l.name).on_hover_text(format!("{}\nClick to annotate this mosaic", l.uri)).clicked() && !is_active && !in_project {
                    activate = Some(l.id);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖").on_hover_text("Remove layer").clicked() {
                        remove = Some(l.id);
                    }
                    if ui.small_button("⛶").on_hover_text("Zoom to layer").clicked() {
                        fit = Some(l.id);
                    }
                });
            });
            ui.add(egui::Slider::new(&mut l.opacity, 0.0..=1.0).text("opacity").show_value(false));
        }
        if let Some(id) = activate {
            self.activate(id);
        }
        if let Some(id) = remove {
            if active_id == Some(id) {
                self.form.commit(&mut self.editor);
                self.save_now();
                self.active = None;
                self.editor.load(Vec::new());
            }
            self.layers.retain(|l| l.id != id);
            self.tiles.forget_layer(id);
            if self.active.is_none() {
                if let Some(first) = self.layers.first().map(|l| l.id) {
                    self.activate(first);
                }
            }
        }
        if let Some(id) = fit {
            self.fit_to(id);
        }

        self.bookmarks_ui(ui);
        ui.separator();
        ui.heading(if self.server.connected() { "Bucket mosaics (local annotation)" } else { "Orthomosaics" });
        ui.weak(format!("gs://{}/…/orthomosaic_cog/", catalog::BUCKET));
        ui.horizontal(|ui| {
            let years = self.catalog.years.clone();
            egui::ComboBox::from_id_salt("year").selected_text(self.catalog.year.clone().unwrap_or_else(|| "year".into())).show_ui(ui, |ui| {
                for y in years {
                    if ui.selectable_label(self.catalog.year.as_deref() == Some(y.as_str()), &y).clicked() {
                        self.catalog.select_year(&ctx, y);
                    }
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.catalog.filter).hint_text("🔍 filter, e.g. TUT or 2104"));
        });
        if self.catalog.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading list…");
            });
        }
        if let Some(e) = &self.catalog.error {
            ui.colored_label(Color32::from_rgb(220, 80, 60), e);
        }
        let f = self.catalog.filter.to_uppercase();
        let files: Vec<catalog::CogEntry> = self.catalog.files.iter().filter(|c| f.is_empty() || c.file_name().to_uppercase().contains(&f)).cloned().collect();
        ui.weak(format!("{} mosaics · click to open, Ctrl+click to add", files.len()));
        let open: Vec<String> = self.layers.iter().map(|l| l.uri.clone()).collect();
        // Which mosaics have local annotation files: list the folder every few
        // seconds instead of checking the disk for every row every frame.
        if self.annotated_at.elapsed() > Duration::from_secs(5) {
            self.annotated_at = Instant::now();
            self.annotated = std::fs::read_dir(data_dir().join("annotations"))
                .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
        }
        let annotated = &self.annotated;
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 20.0, files.len(), |ui, range| {
            for c in &files[range] {
                let uri = c.uri();
                let is_open = open.contains(&uri);
                let has_ann = annotated.contains(&annotations_path(&uri));
                let r = ui.horizontal(|ui| {
                    let r = ui.selectable_label(is_open, c.site());
                    if has_ann {
                        ui.label(RichText::new("✎").color(Color32::from_rgb(255, 45, 111))).on_hover_text("Has saved annotations");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format!("{:.1} GB", c.size as f64 / 1e9));
                    });
                    r
                });
                if r.inner.clicked() {
                    clicked = Some((uri, !ui.input(|i| i.modifiers.command)));
                }
            }
        });
        if let Some((uri, replace)) = clicked {
            self.open_layer(uri, replace);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let green = Color32::from_rgb(90, 200, 110);
            let amber = Color32::from_rgb(230, 180, 0);
            let red = Color32::from_rgb(230, 80, 60);
            if let Some(open) = &self.server.open {
                let conflicts = open.sync.conflicts.len();
                let failed = open.sync.failed.len();
                let failed_detail = open.sync.failed.values().take(5).cloned().collect::<Vec<_>>().join("\n");
                let can_edit = open.can_edit;
                let status = self.server.status.clone();
                let mut retry = false;
                match &status {
                    _ if !can_edit => {
                        ui.label(RichText::new("👁 View only").color(amber));
                    }
                    SyncStatus::Syncing(n) => {
                        ui.spinner();
                        ui.label(RichText::new(format!("Syncing {n}…")).color(amber));
                    }
                    SyncStatus::Offline { queued, retry_at, error } => {
                        let secs = retry_at.saturating_duration_since(Instant::now()).as_secs();
                        ui.label(RichText::new(format!("⚠ Offline · {} queued · retry in {secs}s", self.pending_sync.max(*queued))).color(red))
                            .on_hover_text(format!("{error}\nYour work is safe on this computer and will sync automatically."));
                        retry |= ui.small_button("Retry now").clicked();
                        ctx_repaint(ui, 1000);
                    }
                    SyncStatus::Idle if self.pending_sync > 0 => {
                        ui.label(RichText::new(format!("● {} to sync", self.pending_sync)).color(amber));
                    }
                    SyncStatus::Idle => {
                        ui.label(RichText::new("☁ Synced").color(green)).on_hover_text("All annotations are saved in the CAT database");
                    }
                }
                if conflicts > 0 && ui.button(RichText::new(format!("⚠ {conflicts} conflict(s)")).color(red)).clicked() {
                    self.show_conflicts = true;
                }
                if failed > 0 {
                    retry |= ui
                        .button(RichText::new(format!("⚠ {failed} rejected")).color(red))
                        .on_hover_text(format!("The server refused these:\n{failed_detail}\nClick to retry."))
                        .clicked();
                }
                if retry {
                    self.server.retry_now();
                }
            } else {
                match &self.save_state {
                    SaveState::Saved => ui.label(RichText::new("✔ Saved").color(green)),
                    SaveState::Pending => ui.label(RichText::new("● Saving…").color(amber)),
                    SaveState::Failed(e) => ui.label(RichText::new("⚠ Not saved").color(red)).on_hover_text(e.as_str()),
                };
            }
            ui.separator();
            ui.label(format!("⏱ {}", fmt_duration(self.timer.secs))).on_hover_text("Active annotation time this session (pauses after 2 min idle)");
            // Cat buddy: a gentle nudge after a long stretch without a break.
            let mins = self.settings.break_minutes as f64;
            if mins > 0.0 && self.timer.since_break >= mins * 60.0 && self.timer.since_break >= self.break_snooze_until {
                let worked = (self.timer.since_break / 60.0) as u64;
                let cat = if self.timer.since_break > mins * 60.0 * 1.5 { "😾" } else { "😺" };
                if ui
                    .button(RichText::new(format!("{cat} {worked} min without a break — stretch?")).color(Color32::from_rgb(230, 180, 0)))
                    .on_hover_text("Click to snooze. Rests your eyes and keeps IDs sharp. Change the interval in View → Break reminder.")
                    .clicked()
                {
                    self.break_snooze_until = self.timer.since_break + (mins * 30.0).clamp(60.0, 1800.0);
                }
            }
            ui.separator();
            match self.hover {
                Some((lon, lat)) => ui.monospace(format!("{lat:.8}, {lon:.8}")),
                None => ui.monospace("—"),
            };
            ui.separator();
            if let Some((lvl, ratio)) = self.level {
                let pct = 100.0 * ratio;
                let z = if pct < 10.0 { format!("{pct:.1}%") } else { format!("{pct:.0}%") };
                ui.label(format!("zoom {z} · level {lvl}")).on_hover_text("100% = one full-resolution image pixel per screen pixel");
                ui.separator();
            }
            let n = self.tiles.pending_count();
            if n > 0 {
                ui.spinner();
                ui.label(format!("loading {n} tiles"));
            }
            if let Some(e) = &self.tiles.last_error {
                ui.separator();
                ui.colored_label(Color32::from_rgb(220, 80, 60), "⚠ some tiles failed").on_hover_text(e.as_str());
                if ui.small_button("Retry").clicked() {
                    self.tiles.retry_failed();
                }
            }
        });
    }

    /// Map legend (bottom-right): colour mode, values with counts; click a
    /// value to select all its annotations.
    fn legend(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        // Recompute only when annotations change.
        if self.legend_cache.0 != (self.editor.rev, self.editor.style.color_by) {
            self.legend_cache = ((self.editor.rev, self.editor.style.color_by), self.editor.legend());
        }
        let entries = self.legend_cache.1.clone();
        let mut pick = None;
        egui::Area::new(egui::Id::unique("legend"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-10.0, -10.0])
            .constrain_to(rect)
            .order(egui::Order::Middle)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    ui.set_max_width(230.0);
                    egui::ComboBox::from_id_salt("colorby").selected_text(self.editor.style.color_by.label()).width(210.0).show_ui(ui, |ui| {
                        for m in editor::ColorBy::ALL {
                            ui.selectable_value(&mut self.editor.style.color_by, m, m.label());
                        }
                    });
                    let max = 12;
                    for (value, color, n) in entries.iter().take(max) {
                        let label = value.clone().unwrap_or_else(|| "— (none)".into());
                        let name = if self.editor.style.color_by == editor::ColorBy::Species {
                            value.as_ref().and_then(|v| self.form.species.get(v)).map(|s| s.name.clone()).unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let r = ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(dot, 2.0, *color);
                            let r = ui.add(egui::Label::new(RichText::new(&label).monospace()).sense(egui::Sense::click()));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.weak(n.to_string());
                            });
                            r
                        });
                        let r = r.inner.on_hover_text(if name.is_empty() { format!("Click to select all {n}") } else { format!("{name} — click to select all {n}") });
                        if r.clicked() {
                            pick = Some(value.clone());
                        }
                    }
                    if entries.len() > max {
                        ui.weak(format!("+ {} more", entries.len() - max));
                    }
                });
            });
        if let Some(v) = pick {
            self.form.commit(&mut self.editor);
            self.editor.select_value(&v);
            self.settings.show_form = true;
        }
    }

    fn tool_bar(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let area = egui::Area::new(egui::Id::unique("toolbar")).fixed_pos(rect.left_top() + egui::vec2(10.0, 10.0)).order(egui::Order::Foreground);
        area.show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).inner_margin(6.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for t in Tool::ALL {
                        let r = ui.add(egui::Button::selectable(self.editor.tool == t, t.label()));
                        if r.on_hover_text(format!("{}  ({})", t.help(), t.key())).clicked() {
                            self.editor.set_tool(t);
                        }
                    }
                    ui.separator();
                    ui.toggle_value(&mut self.editor.snap, "🧲 Snap").on_hover_text("Snap new vertices to nearby annotation vertices (S)");
                    ui.separator();
                    if ui.add_enabled(self.editor.history.can_undo().is_some(), egui::Button::new("↶")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                        self.form.commit(&mut self.editor);
                        self.editor.undo();
                    }
                    if ui.add_enabled(self.editor.history.can_redo().is_some(), egui::Button::new("↷")).on_hover_text("Redo (Ctrl+Y)").clicked() {
                        self.editor.redo();
                    }
                });
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = |k| egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, k);
        let cmd_shift = |k| egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, k);
        let typing = ctx.egui_wants_keyboard_input();
        let (undo, redo, save, export, same) = ctx.input_mut(|i| {
            (
                !typing && i.consume_shortcut(&cmd(egui::Key::Z)),
                !typing && (i.consume_shortcut(&cmd(egui::Key::Y)) || i.consume_shortcut(&cmd_shift(egui::Key::Z))),
                i.consume_shortcut(&cmd(egui::Key::S)),
                i.consume_shortcut(&cmd(egui::Key::E)),
                i.consume_shortcut(&cmd(egui::Key::D)),
            )
        });
        if undo {
            self.form.commit(&mut self.editor);
            self.editor.undo();
        }
        if redo {
            self.editor.redo();
        }
        if save {
            self.form.commit(&mut self.editor);
            self.save_now();
            self.editor.toast = Some(("Saved".into(), Instant::now()));
        }
        if export && !self.editor.items.is_empty() {
            self.export(false);
        }
        if same {
            self.form.same_as_last(&mut self.editor);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::B))) {
            self.save_bookmark();
        }
        if !typing {
            let keys = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9];
            if let Some(i) = keys.iter().position(|k| ctx.input(|inp| inp.key_pressed(*k) && !inp.modifiers.any())) {
                self.go_bookmark(i);
            }
        }
        if typing {
            return;
        }
        ctx.input(|i| {
            if i.modifiers.command || i.modifiers.alt {
                return;
            }
            for (k, t) in [(egui::Key::V, Tool::Select), (egui::Key::P, Tool::Point), (egui::Key::L, Tool::Line), (egui::Key::R, Tool::Rect), (egui::Key::G, Tool::Polygon), (egui::Key::M, Tool::Measure)] {
                if i.key_pressed(k) {
                    self.editor.set_tool(t);
                }
            }
            if i.key_pressed(egui::Key::S) {
                self.editor.snap = !self.editor.snap;
            }
            if i.key_pressed(egui::Key::N) {
                self.fit_pending = Some(u64::MAX - 1);
            }
            if i.key_pressed(egui::Key::T) {
                self.settings.show_table = !self.settings.show_table;
            }
            if i.key_pressed(egui::Key::A) {
                self.settings.show_form = !self.settings.show_form;
            }
            if i.key_pressed(egui::Key::F) {
                self.fit_pending = Some(u64::MAX);
            }
            if i.key_pressed(egui::Key::Questionmark) {
                self.show_shortcuts = !self.show_shortcuts;
            }
        });
        if self.fit_pending == Some(u64::MAX - 1) {
            self.fit_pending = None;
            self.form.commit(&mut self.editor);
            if let Some(b) = self.editor.next_incomplete() {
                self.zoom_to_bbox(b);
                self.settings.show_form = true;
                self.form.focus_species();
            }
        }
        if self.fit_pending == Some(u64::MAX) {
            self.fit_pending = None;
            self.fit_all();
        }
    }

    fn perf_window(&mut self, ctx: &egui::Context) {
        if !self.show_perf {
            return;
        }
        let mut open = true;
        let mut v: Vec<f32> = self.frame_ms.iter().copied().collect();
        v.sort_by(|a, b| a.total_cmp(b));
        let pick = |q: f32| v.get(((v.len() as f32 - 1.0) * q) as usize).copied().unwrap_or(0.0);
        let dt = ctx.input(|i| i.unstable_dt) * 1000.0;
        egui::Window::new("Performance & graphics").open(&mut open).resizable(false).show(ctx, |ui| {
            egui::Grid::new("perf").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Graphics");
                ui.label(self.gfx_info.clone().unwrap_or_default());
                ui.end_row();
                ui.label("App work per frame");
                ui.monospace(format!("median {:.1} ms · 95% {:.1} ms · worst {:.1} ms", pick(0.5), pick(0.95), pick(1.0)));
                ui.end_row();
                ui.label("Last frame interval");
                ui.monospace(format!("{dt:.1} ms"));
                ui.end_row();
                ui.label("Annotations / tiles loading");
                ui.monospace(format!("{} / {}", self.editor.items.len(), self.tiles.pending_count()));
                ui.end_row();
            });
            if self.software_gpu {
                ui.colored_label(Color32::from_rgb(230, 80, 60), "⚠ Software rendering (no graphics card in use). Update the graphics driver, or try OpenGL below.");
            }
            ui.separator();
            ui.label("Renderer (applies next time CAT starts)");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.settings.renderer, "auto".to_string(), "DirectX 12 / Vulkan (default)");
                ui.radio_value(&mut self.settings.renderer, "opengl".to_string(), "OpenGL (try if drawing is choppy)");
            });
            ui.checkbox(&mut self.settings.low_latency, "Low input latency (line follows the cursor more closely)");
            ui.weak("Under ~8 ms of app work per frame is smooth. If it's low but drawing still lags, the graphics driver or renderer is the cause — try OpenGL.");
        });
        if !open {
            self.show_perf = false;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts").open(&mut self.show_shortcuts).resizable(false).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("keys").striped(true).show(ui, |ui| {
                for (k, v) in [
                    ("V / P / L / R / G / M", "Select / Point / Line / Rectangle / Polygon / Measure"),
                    ("Shift+drag (Line, Polygon)", "Trace freehand; a new outline finishes on release"),
                    ("Right-click", "Finish line or polygon"),
                    ("Ctrl+Z while drawing", "Remove the last vertex"),
                    ("Click", "Add vertex (drawing) · select (Select tool)"),
                    ("Enter / double-click", "Finish line or polygon"),
                    ("Backspace", "Remove last vertex while drawing"),
                    ("Esc", "Cancel drawing · deselect · back to Select"),
                    ("Species: type, ↑↓, Enter", "Pick species; Enter again = done, draw next"),
                    ("Ctrl+D", "Same attributes as last annotation"),
                    ("N", "Next annotation that needs a species"),
                    ("Ctrl+B · 1–9", "Save view as bookmark · fly to bookmark"),
                    ("Shift+click / Shift+drag", "Add to selection / lasso select"),
                    ("Ctrl+A", "Select all (then bulk edit in the panel)"),
                    ("↑ / ↓ over the table", "Previous / next row"),
                    ("Del", "Delete selected annotation"),
                    ("Alt+click vertex", "Remove that vertex"),
                    ("Drag small dot", "Insert a vertex"),
                    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
                    ("S", "Snap to vertices on/off"),
                    ("Ctrl+S / Ctrl+E", "Save now / export GeoJSON"),
                    ("T / A", "Table / attribute panel"),
                    ("Wheel · drag · Space+drag", "Zoom · pan · pan while drawing"),
                    ("Double-click (Select)", "Zoom to selected annotation"),
                    ("F · + / − · arrows", "Fit · zoom · pan"),
                    ("?", "This help"),
                ] {
                    ui.strong(k);
                    ui.label(v);
                    ui.end_row();
                }
            });
        });
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let name = match &self.server.open {
            Some(o) => Some(format!("{} [{}]", o.project.project_name, self.server.profile.as_ref().map(|p| p.name.as_str()).unwrap_or("server"))),
            None => self.active.as_ref().and_then(|(id, _)| self.layers.iter().find(|l| l.id == *id)).map(|l| l.name.clone()),
        };
        let dirty = if self.editor.rev != self.saved_rev { " •" } else { "" };
        let title = match name {
            Some(n) => format!("CAT — {n}{dirty}"),
            None => "CAT — Coral Annotation Tool".into(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }

    /// Write settings (incl. open mosaics and view) if anything changed, so a
    /// crash or forced close still restores the session next time.
    fn persist(&mut self) {
        self.settings.style = self.editor.style.clone();
        self.settings.snap = self.editor.snap;
        self.settings.last_layers = self.layers.iter().map(|l| l.uri.clone()).collect();
        if self.restore_view.is_none() && self.layers.iter().any(|l| l.cog().is_some()) {
            self.settings.last_view = Some([self.cam.cx, self.cam.cy, self.cam.mpp]);
        }
        let now = serde_json::to_string(&self.settings).unwrap_or_default();
        if now != self.persisted {
            self.settings.save();
            self.persisted = now;
        }
    }
}

fn fmt_duration(s: f64) -> String {
    let s = s as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

impl eframe::App for CatApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        if self.gfx_info.is_none() {
            self.gfx_info = Some(match frame.wgpu_render_state() {
                Some(rs) => {
                    let i = rs.adapter.get_info();
                    self.software_gpu = i.device_type == eframe::wgpu::DeviceType::Cpu;
                    format!("{} · {:?} · {:?} · driver {}", i.name, i.backend, i.device_type, i.driver_info)
                }
                None => "OpenGL (glow)".into(),
            });
            if self.software_gpu {
                self.errors.push("Windows is drawing CAT without a graphics card (software rendering), which makes the map slow. Help → Performance & graphics has options; updating the graphics driver usually fixes this.".into());
            }
        }
        let ctx = ui.ctx().clone();
        let had_input = ctx.input(|i| !i.events.is_empty() || i.pointer.is_moving());
        self.timer.tick(had_input);
        self.catalog.poll(&ctx);
        for (id, result) in self.tiles.poll(&ctx) {
            if let Some(l) = self.layers.iter_mut().find(|l| l.id == id) {
                l.state = match result {
                    Ok(c) => LayerState::Ready(c),
                    Err(e) => {
                        self.errors.push(format!("{}: {e}", l.name));
                        LayerState::Failed(e)
                    }
                };
            }
            if let Some(v) = self.restore_view.take() {
                self.cam = Camera { cx: v[0], cy: v[1], mpp: v[2] };
                self.fit_pending = None;
            }
            if self.fit_pending == Some(id) {
                self.fit_to(id);
                self.fit_pending = None;
            }
        }
        self.handle_server_events();
        self.shortcuts(&ctx);

        let mut prof = Prof::new();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("side").resizable(true).default_size(260.0).show(ui, |ui| self.side_panel(ui));
        prof.mark("side");
        if self.settings.show_form {
            egui::Panel::right("form").resizable(true).default_size(340.0).min_size(280.0).show(ui, |ui| {
                ui.add_space(4.0);
                if self.form.ui(ui, &mut self.editor, &mut self.settings) == FormAction::Done {
                    self.form.commit(&mut self.editor);
                    self.editor.selected = None;
                    ui.ctx().memory_mut(|m| m.surrender_focus(egui::Id::unique("species-input")));
                }
            });
        }
        prof.mark("form");
        if self.settings.show_table {
            egui::Panel::bottom("table").resizable(true).default_size(200.0).min_size(90.0).show(ui, |ui| {
                ui.add_space(4.0);
                if let Some(u) = table::table_ui(ui, &mut self.editor, &mut self.table, &self.form.species) {
                    if let Some(b) = self.editor.get(&u).map(|a| a.geom.bbox()) {
                        self.zoom_to_bbox(b);
                    }
                }
            });
        }
        prof.mark("table");
        self.animate(&ctx);
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let primary_pan = self.editor.pre_input(ui, rect, &self.cam);
            let nav = Nav { primary_pan, double_click_zoom: matches!(self.editor.tool, Tool::Select) && self.editor.selected.is_none() };
            let out = map::map_ui(ui, &mut self.cam, &self.layers, &mut self.tiles, nav);
            self.map_rect = out.rect;
            self.hover = out.hover_lonlat;
            self.level = out.level;
            prof.mark("map");
            let ev = self.editor.ui(ui, &out.resp, out.rect, &mut self.cam, self.timer.secs);
            prof.mark("editor");
            if ev.created.is_some() {
                self.settings.show_form = true;
                self.form.focus_species();
                self.session_created += 1;
                if self.session_created.is_multiple_of(50) {
                    self.editor.toast = Some((format!("🐱 {} annotations this session — nice work!", self.session_created), Instant::now()));
                }
            }
            if let Some(b) = ev.zoom_to {
                self.zoom_to_bbox(b);
            }
            if let (Some(u), Some(p)) = (&ev.hovered, out.resp.hover_pos()) {
                if let Some(a) = self.editor.get(u) {
                    let code = a.spcode();
                    let name = self.form.species.get(&code).map(|s| s.name.clone()).unwrap_or_default();
                    let mut text = if code.is_empty() { format!("{} · needs species", a.kind.label()) } else { format!("{code}  {name}") };
                    let morph = a.text("morph_code");
                    if !morph.is_empty() {
                        text.push_str(&format!(" · {morph}"));
                    }
                    text.push_str(&format!(" · {}", editor::size_text(a)));
                    let painter = ui.painter_at(out.rect);
                    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(13.0), Color32::WHITE);
                    let r = egui::Rect::from_min_size(p + egui::vec2(16.0, 16.0), galley.size() + egui::vec2(12.0, 6.0));
                    painter.rect_filled(r, 4.0, Color32::from_black_alpha(210));
                    painter.galley(r.min + egui::vec2(6.0, 3.0), galley, Color32::WHITE);
                }
            }
            map::scale_bar(&ui.painter_at(out.rect), out.rect, &self.cam);
            if self.settings.show_legend && !self.editor.items.is_empty() {
                self.legend(ui, out.rect);
            }
            self.tool_bar(ui, out.rect);
            if self.layers.iter().any(|l| matches!(l.state, LayerState::Opening)) {
                ui.put(
                    egui::Rect::from_center_size(out.rect.center(), egui::vec2(240.0, 40.0)),
                    egui::Label::new(RichText::new("⏳ Opening orthomosaic…").size(18.0).color(Color32::WHITE)),
                );
            }
            if let Some((msg, at)) = &self.editor.toast {
                if at.elapsed() < Duration::from_millis(2600) {
                    let p = out.rect.center_bottom() - egui::vec2(0.0, 40.0);
                    let painter = ui.painter_at(out.rect);
                    let galley = painter.layout_no_wrap(msg.clone(), egui::FontId::proportional(14.0), Color32::WHITE);
                    let r = egui::Rect::from_center_size(p, galley.size() + egui::vec2(24.0, 12.0));
                    painter.rect_filled(r, 8.0, Color32::from_black_alpha(200));
                    painter.galley(r.min + egui::vec2(12.0, 6.0), galley, Color32::WHITE);
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        });

        if !self.errors.is_empty() {
            let mut close = false;
            egui::Window::new("Problem").collapsible(false).show(&ctx, |ui| {
                for e in &self.errors {
                    ui.label(e);
                }
                close = ui.button("OK").clicked();
            });
            if close {
                self.errors.clear();
            }
        }
        self.shortcuts_window(&ctx);
        self.login_window(&ctx);
        self.conflicts_window(&ctx);
        self.autosave();
        self.drive_sync();
        if self.server.round_done {
            self.server.round_done = false;
            // Sync state changed: rewrite the journal in the background.
            if let Some(job) = self.save_job() {
                self.saver.submit(self.editor.rev, job);
            }
        }
        if self.save_state == SaveState::Pending {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        prof.mark("rest");
        self.update_title(&ctx);
        if self.persist_check.elapsed() > Duration::from_secs(3) {
            self.persist_check = Instant::now();
            self.persist();
        }
        ctx.request_repaint_after(Duration::from_secs(3));
        self.frame_ms.push_back(frame_start.elapsed().as_secs_f32() * 1000.0);
        if self.frame_ms.len() > 240 {
            self.frame_ms.pop_front();
        }
        self.perf_window(&ctx);
        // CAT_PERF=1: log CPU time per frame (for performance checks).
        if std::env::var_os("CAT_PERF").is_some() {
            eprintln!("frame_ms {:.2} items {} {}", frame_start.elapsed().as_secs_f64() * 1000.0, self.editor.items.len(), prof.report());
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.form.commit(&mut self.editor);
        self.save_now();
        self.persist();
    }
}

fn ctx_repaint(ui: &egui::Ui, ms: u64) {
    ui.ctx().request_repaint_after(Duration::from_millis(ms));
}

/// Section timer for CAT_PERF.
struct Prof {
    last: Instant,
    parts: Vec<(&'static str, f64)>,
}

impl Prof {
    fn new() -> Self {
        Prof { last: Instant::now(), parts: Vec::new() }
    }
    fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.parts.push((name, (now - self.last).as_secs_f64() * 1000.0));
        self.last = now;
    }
    fn report(&self) -> String {
        self.parts.iter().map(|(n, t)| format!("{n}={t:.2}")).collect::<Vec<_>>().join(" ")
    }
}
