//! CAT desktop — native Windows annotation workstation.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod editor;
mod form;
mod map;
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
    let renderer = match std::env::var("CAT_RENDERER").as_deref() {
        Ok("glow") => eframe::Renderer::Glow,
        _ => eframe::Renderer::Wgpu,
    };
    let options = eframe::NativeOptions { viewport, renderer, ..Default::default() };
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
struct ActiveTimer {
    last_input: Instant,
    secs: f64,
}

impl ActiveTimer {
    fn tick(&mut self, had_input: bool) {
        if had_input {
            let gap = self.last_input.elapsed().as_secs_f64();
            if gap < 120.0 {
                self.secs += gap;
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
            timer: ActiveTimer { last_input: Instant::now(), secs: 0.0 },
            active: None,
            saved_rev: 0,
            last_change: Instant::now(),
            save_state: SaveState::Saved,
            persisted: String::new(),
            persist_check: Instant::now(),
        };
        if initial.is_empty() {
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
        if self.active.is_none() {
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

    fn save_now(&mut self) {
        let Some((id, path)) = self.active.clone() else { return };
        if self.editor.rev == self.saved_rev {
            return;
        }
        let uri = self.layers.iter().find(|l| l.id == id).map(|l| l.uri.clone()).unwrap_or_default();
        let meta = serde_json::json!({"cog": uri, "app": format!("cat-desktop {}", env!("CARGO_PKG_VERSION")), "saved_at": cat_core::annotation::now_iso()});
        match store::save_geojson(&path, &self.editor.items, meta) {
            Ok(()) => {
                self.saved_rev = self.editor.rev;
                self.save_state = SaveState::Saved;
            }
            Err(e) => self.save_state = SaveState::Failed(e.to_string()),
        }
    }

    fn autosave(&mut self) {
        if self.editor.rev != self.saved_rev {
            if self.save_state == SaveState::Saved {
                self.last_change = Instant::now();
            }
            if !matches!(self.save_state, SaveState::Failed(_)) {
                self.save_state = SaveState::Pending;
            }
            // Save shortly after the last change, and never wait more than 10 s.
            if self.last_change.elapsed() > Duration::from_millis(800) && !self.editor.is_drawing() {
                self.save_now();
            }
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
        self.cam.cx = cx;
        self.cam.cy = cy;
        let need = (mb.width() / (self.map_rect.width() as f64 * 0.5)).max(mb.height() / (self.map_rect.height() as f64 * 0.5));
        if need > self.cam.mpp {
            self.cam.mpp = need;
        }
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
                if ui.button("Keyboard shortcuts   ?").clicked() {
                    self.show_shortcuts = true;
                    ui.close();
                }
                ui.label(format!("CAT desktop {}", env!("CARGO_PKG_VERSION")));
                ui.weak(format!("Data folder: {}", data_dir().display()));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(" Local mode · not connected to a CAT server ").color(Color32::WHITE).background_color(Color32::from_rgb(40, 110, 160)))
                    .on_hover_text("Annotations are saved in the data folder next to CAT.exe. Server sync arrives with the CAT server connection.");
            });
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(4.0);
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
                if ui.selectable_label(is_active, &l.name).on_hover_text(format!("{}\nClick to annotate this mosaic", l.uri)).clicked() && !is_active {
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

        ui.separator();
        ui.heading("Orthomosaics");
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
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 20.0, files.len(), |ui, range| {
            for c in &files[range] {
                let uri = c.uri();
                let is_open = open.contains(&uri);
                let has_ann = annotations_path(&uri).exists();
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
            match &self.save_state {
                SaveState::Saved => ui.label(RichText::new("✔ Saved").color(Color32::from_rgb(90, 200, 110))),
                SaveState::Pending => ui.label(RichText::new("● Saving…").color(Color32::from_rgb(230, 180, 0))),
                SaveState::Failed(e) => ui.label(RichText::new("⚠ Not saved").color(Color32::from_rgb(230, 80, 60))).on_hover_text(e.as_str()),
            };
            ui.separator();
            ui.label(format!("⏱ {}", fmt_duration(self.timer.secs))).on_hover_text("Active annotation time this session (pauses after 2 min idle)");
            ui.separator();
            match self.hover {
                Some((lon, lat)) => ui.monospace(format!("{lat:.8}, {lon:.8}")),
                None => ui.monospace("—"),
            };
            ui.separator();
            if let Some((lvl, ratio)) = self.level {
                let pct = 100.0 / ratio.max(1e-12);
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
        if typing {
            return;
        }
        ctx.input(|i| {
            if i.modifiers.command || i.modifiers.alt {
                return;
            }
            for (k, t) in [(egui::Key::V, Tool::Select), (egui::Key::P, Tool::Point), (egui::Key::L, Tool::Line), (egui::Key::R, Tool::Rect), (egui::Key::G, Tool::Polygon)] {
                if i.key_pressed(k) {
                    self.editor.set_tool(t);
                }
            }
            if i.key_pressed(egui::Key::S) {
                self.editor.snap = !self.editor.snap;
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
        if self.fit_pending == Some(u64::MAX) {
            self.fit_pending = None;
            self.fit_all();
        }
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts").open(&mut self.show_shortcuts).resizable(false).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("keys").striped(true).show(ui, |ui| {
                for (k, v) in [
                    ("V / P / L / R / G", "Select / Point / Line / Rectangle / Polygon tool"),
                    ("Click", "Add vertex (drawing) · select (Select tool)"),
                    ("Enter / double-click", "Finish line or polygon"),
                    ("Backspace", "Remove last vertex while drawing"),
                    ("Esc", "Cancel drawing · deselect · back to Select"),
                    ("Species: type, ↑↓, Enter", "Pick species; Enter again = done, draw next"),
                    ("Ctrl+D", "Same attributes as last annotation"),
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

    fn update_title(&self, ctx: &egui::Context) {
        let name = self.active.as_ref().and_then(|(id, _)| self.layers.iter().find(|l| l.id == *id)).map(|l| l.name.clone());
        let dirty = if self.editor.rev != self.saved_rev { " •" } else { "" };
        let title = match name {
            Some(n) => format!("CAT — {n}{dirty}"),
            None => "CAT — Coral Annotation Tool".into(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
        self.shortcuts(&ctx);

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("side").resizable(true).default_size(260.0).show(ui, |ui| self.side_panel(ui));
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
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let primary_pan = self.editor.pre_input(ui, rect, &self.cam);
            let nav = Nav { primary_pan, double_click_zoom: matches!(self.editor.tool, Tool::Select) && self.editor.selected.is_none() };
            let out = map::map_ui(ui, &mut self.cam, &self.layers, &mut self.tiles, nav);
            self.map_rect = out.rect;
            self.hover = out.hover_lonlat;
            self.level = out.level;
            let ev = self.editor.ui(ui, &out.resp, out.rect, &mut self.cam, self.timer.secs);
            if ev.created.is_some() {
                self.settings.show_form = true;
                self.form.focus_species();
            }
            if let Some(b) = ev.zoom_to {
                self.zoom_to_bbox(b);
            }
            map::scale_bar(&ui.painter_at(out.rect), out.rect, &self.cam);
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
        self.autosave();
        if self.save_state == SaveState::Pending {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        self.update_title(&ctx);
        if self.persist_check.elapsed() > Duration::from_secs(3) {
            self.persist_check = Instant::now();
            self.persist();
        }
        ctx.request_repaint_after(Duration::from_secs(3));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.form.commit(&mut self.editor);
        self.save_now();
        self.persist();
    }
}
