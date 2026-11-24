//! CAT desktop — native Windows annotation workstation (Phase 0: map viewer).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod map;
mod tiles;

use catalog::Catalog;
use egui::{Color32, RichText};
use map::Camera;
use tiles::{Layer, LayerState, TileManager};

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../docs/icon.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("CAT — Coral Annotation Tool")
        .with_inner_size([1400.0, 860.0])
        .with_min_inner_size([800.0, 500.0]);
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
    let zoom = args
        .iter()
        .position(|a| a == "--zoom")
        .and_then(|i| args.get(i + 1))
        .and_then(|z| z.parse::<f64>().ok());
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

struct CatApp {
    tiles: TileManager,
    catalog: Catalog,
    layers: Vec<Layer>,
    next_id: u64,
    cam: Camera,
    /// Fit the view to this layer once it has opened.
    fit_pending: Option<u64>,
    map_rect: egui::Rect,
    hover: Option<(f64, f64)>,
    level: Option<(usize, f64)>,
    url_input: String,
    show_shortcuts: bool,
    errors: Vec<String>,
    /// Extra zoom applied after the first fit (`--zoom N`).
    start_zoom: Option<f64>,
}

impl CatApp {
    fn new(cc: &eframe::CreationContext<'_>, initial: Vec<String>) -> Self {
        let ctx = &cc.egui_ctx;
        let mut app = Self {
            tiles: TileManager::new(ctx.clone()),
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
        };
        for uri in initial {
            app.open_layer(uri, true);
        }
        app
    }

    fn open_layer(&mut self, uri: String, replace: bool) {
        if replace {
            for l in self.layers.drain(..) {
                self.tiles.forget_layer(l.id);
            }
        }
        if let Some(l) = self.layers.iter().find(|l| l.uri == uri) {
            self.fit_pending = Some(l.id);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = uri.rsplit('/').next().unwrap_or(&uri).trim_end_matches(".tif").to_string();
        self.tiles.open(id, uri.clone());
        self.layers.push(Layer { id, name, uri, state: LayerState::Opening, visible: true, opacity: 1.0 });
        self.fit_pending = Some(id);
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
                Some(o) => cat_cog::MercBox {
                    min_x: o.min_x.min(m.min_x),
                    min_y: o.min_y.min(m.min_y),
                    max_x: o.max_x.max(m.max_x),
                    max_y: o.max_y.max(m.max_y),
                },
            });
        }
        if let Some(b) = b {
            self.cam.fit(self.map_rect, b);
        }
    }

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
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("Fit to imagery   F").clicked() {
                    self.fit_all();
                    ui.close();
                }
                let mut bt = self.tiles.background_transparent();
                if ui.checkbox(&mut bt, "Hide mosaic background (white/black border)").changed() {
                    self.tiles.set_background_transparent(bt);
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
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(" Phase 0 · bucket viewer ").color(Color32::WHITE).background_color(Color32::from_rgb(40, 110, 160)));
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
                ui.label(&l.name).on_hover_text(&l.uri);
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
        if let Some(id) = remove {
            self.layers.retain(|l| l.id != id);
            self.tiles.forget_layer(id);
        }
        if let Some(id) = fit {
            self.fit_to(id);
        }

        ui.separator();
        ui.heading("Orthomosaics");
        ui.weak(format!("gs://{}/…/orthomosaic_cog/", catalog::BUCKET));
        ui.horizontal(|ui| {
            let years = self.catalog.years.clone();
            egui::ComboBox::from_id_salt("year")
                .selected_text(self.catalog.year.clone().unwrap_or_else(|| "year".into()))
                .show_ui(ui, |ui| {
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
        let files: Vec<catalog::CogEntry> =
            self.catalog.files.iter().filter(|c| f.is_empty() || c.file_name().to_uppercase().contains(&f)).cloned().collect();
        ui.weak(format!("{} mosaics · click to open, Ctrl+click to add", files.len()));
        let open: Vec<String> = self.layers.iter().map(|l| l.uri.clone()).collect();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 20.0, files.len(), |ui, range| {
            for c in &files[range] {
                let uri = c.uri();
                let is_open = open.contains(&uri);
                let r = ui.horizontal(|ui| {
                    let r = ui.selectable_label(is_open, c.site());
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
            match self.hover {
                Some((lon, lat)) => ui.monospace(format!("{lat:.8}, {lon:.8}")),
                None => ui.monospace("—"),
            };
            ui.separator();
            if let Some((lvl, ratio)) = self.level {
                let pct = 100.0 / ratio.max(1e-12);
                let z = if pct < 10.0 { format!("{pct:.1}%") } else { format!("{pct:.0}%") };
                ui.label(format!("zoom {z} · level {lvl}"))
                    .on_hover_text("100% = one full-resolution image pixel per screen pixel");
                ui.separator();
            }
            let n = self.tiles.pending_count();
            if n > 0 {
                ui.spinner();
                ui.label(format!("loading {n} tiles"));
            } else {
                ui.weak("tiles up to date");
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

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts").open(&mut self.show_shortcuts).resizable(false).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("keys").striped(true).show(ui, |ui| {
                for (k, v) in [
                    ("Mouse wheel / pinch", "Zoom at cursor"),
                    ("Drag (left or middle)", "Pan"),
                    ("Double-click", "Zoom in"),
                    ("Arrow keys", "Pan"),
                    ("+ / −", "Zoom in / out"),
                    ("F", "Fit to imagery"),
                    ("?", "This help"),
                ] {
                    ui.strong(k);
                    ui.label(v);
                    ui.end_row();
                }
            });
        });
    }
}

impl eframe::App for CatApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
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
            if self.fit_pending == Some(id) {
                self.fit_to(id);
                self.fit_pending = None;
            }
        }
        if let Some(id) = self.fit_pending {
            if self.layers.iter().any(|l| l.id == id && l.cog().is_some()) {
                self.fit_to(id);
                self.fit_pending = None;
            }
        }

        if !ctx.egui_wants_keyboard_input() {
            if ctx.input(|i| i.key_pressed(egui::Key::F)) {
                self.fit_all();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Questionmark)) {
                self.show_shortcuts = !self.show_shortcuts;
            }
        }

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("side").resizable(true).default_size(300.0).show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let out = map::map_ui(ui, &mut self.cam, &self.layers, &mut self.tiles);
            self.map_rect = out.rect;
            self.hover = out.hover_lonlat;
            self.level = out.level;
            if self.layers.iter().any(|l| matches!(l.state, LayerState::Opening)) {
                ui.put(
                    egui::Rect::from_center_size(out.rect.center(), egui::vec2(220.0, 40.0)),
                    egui::Label::new(RichText::new("⏳ Opening orthomosaic…").size(18.0).color(Color32::WHITE)),
                );
            }
        });
        if !self.errors.is_empty() {
            let mut close = false;
            egui::Window::new("Problem opening imagery").collapsible(false).show(&ctx, |ui| {
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
    }
}
