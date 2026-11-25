//! The map canvas: camera, input, COG tile drawing, scale bar.
//!
//! World coordinates are WebMercator metres held in f64. Everything is
//! converted to screen space relative to the camera centre in f64 before
//! dropping to f32, so vertices don't jitter at deep zoom (the bucket
//! mosaics have ~0.5 mm pixels).

use std::sync::Arc;

use cat_cog::geo::{merc_to_lonlat, MercBox};
use cat_cog::Cog;
use egui::{pos2, vec2, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::tiles::{Layer, TileKey, TileManager};

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub cx: f64,
    pub cy: f64,
    /// WebMercator metres per screen point.
    pub mpp: f64,
}

impl Default for Camera {
    fn default() -> Self {
        // Whole Pacific-ish start; replaced by "fit to layer" on first open.
        Self { cx: -17_000_000.0, cy: 1_000_000.0, mpp: 5_000.0 }
    }
}

/// How far past native resolution the user may zoom (the web app allows
/// very deep zoom for pixel-level inspection).
pub const MAX_OVERZOOM: f64 = 64.0;

impl Camera {
    pub fn to_screen(self, rect: Rect, x: f64, y: f64) -> Pos2 {
        let c = rect.center();
        pos2(c.x + ((x - self.cx) / self.mpp) as f32, c.y - ((y - self.cy) / self.mpp) as f32)
    }

    pub fn to_world(self, rect: Rect, p: Pos2) -> (f64, f64) {
        let c = rect.center();
        (self.cx + (p.x - c.x) as f64 * self.mpp, self.cy - (p.y - c.y) as f64 * self.mpp)
    }

    pub fn view_box(&self, rect: Rect) -> MercBox {
        let hw = rect.width() as f64 / 2.0 * self.mpp;
        let hh = rect.height() as f64 / 2.0 * self.mpp;
        MercBox { min_x: self.cx - hw, max_x: self.cx + hw, min_y: self.cy - hh, max_y: self.cy + hh }
    }

    /// Zoom by `factor` (>1 = in) keeping the world point under `anchor` fixed.
    pub fn zoom_at(&mut self, rect: Rect, anchor: Pos2, factor: f64, min_mpp: f64) {
        let (wx, wy) = self.to_world(rect, anchor);
        let new_mpp = (self.mpp / factor).clamp(min_mpp, 50_000.0);
        let f = new_mpp / self.mpp;
        self.cx = wx - (wx - self.cx) * f;
        self.cy = wy - (wy - self.cy) * f;
        self.mpp = new_mpp;
    }

    pub fn fit(&mut self, rect: Rect, b: MercBox) {
        let (cx, cy) = b.center();
        self.cx = cx;
        self.cy = cy;
        let sx = b.width() / (rect.width() as f64 * 0.92);
        let sy = b.height() / (rect.height() as f64 * 0.92);
        self.mpp = sx.max(sy).max(1e-9);
    }
}

/// How the map may use mouse input this frame (the editor decides).
#[derive(Clone, Copy)]
pub struct Nav {
    /// Primary-button drag pans the map (false while the editor owns the drag).
    pub primary_pan: bool,
    /// Double-click zooms in (false while drawing lines/polygons).
    pub double_click_zoom: bool,
}

pub struct MapOutput {
    pub resp: egui::Response,
    pub hover_lonlat: Option<(f64, f64)>,
    /// Level drawn for the top visible layer, and zoom as screen pixels per
    /// full-resolution image pixel.
    pub level: Option<(usize, f64)>,
    pub rect: Rect,
}

/// Draw the map and handle navigation input.
pub fn map_ui(ui: &mut egui::Ui, cam: &mut Camera, layers: &[Layer], tiles: &mut TileManager, nav: Nav) -> MapOutput {
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

    // Finest zoom allowed: MAX_OVERZOOM past the sharpest open layer.
    let min_mpp = layers
        .iter()
        .filter_map(|l| l.cog())
        .map(|c| c.merc_pixel_size() / MAX_OVERZOOM)
        .fold(f64::INFINITY, f64::min)
        .clamp(1e-7, 1.0);

    // --- Navigation -------------------------------------------------------
    if (nav.primary_pan && resp.dragged_by(egui::PointerButton::Primary)) || resp.dragged_by(egui::PointerButton::Middle) {
        let d = resp.drag_delta();
        cam.cx -= d.x as f64 * cam.mpp;
        cam.cy += d.y as f64 * cam.mpp;
    }
    if let Some(hover) = resp.hover_pos() {
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
        let mut factor = zoom as f64;
        if scroll.y != 0.0 {
            factor *= (scroll.y as f64 * 0.0025).exp();
        }
        if (factor - 1.0).abs() > 1e-6 {
            cam.zoom_at(rect, hover, factor, min_mpp);
        }
    }
    if nav.double_click_zoom && resp.double_clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            cam.zoom_at(rect, p, 2.0, min_mpp);
        }
    }
    if !ui.ctx().egui_wants_keyboard_input() {
        let step = 120.0 * cam.mpp;
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) { cam.cx -= step }
            if i.key_pressed(egui::Key::ArrowRight) { cam.cx += step }
            if i.key_pressed(egui::Key::ArrowUp) { cam.cy += step }
            if i.key_pressed(egui::Key::ArrowDown) { cam.cy -= step }
        });
        let (zin, zout) = ui.input(|i| {
            (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals), i.key_pressed(egui::Key::Minus))
        });
        if zin { cam.zoom_at(rect, rect.center(), 2.0, min_mpp) }
        if zout { cam.zoom_at(rect, rect.center(), 0.5, min_mpp) }
    }

    // --- Tiles --------------------------------------------------------------
    let view = cam.view_box(rect);
    let ppp = ui.ctx().pixels_per_point() as f64;
    let mut wanted: Vec<(TileKey, Arc<Cog>)> = Vec::new();
    let mut level_info = None;
    for layer in layers.iter().filter(|l| l.visible) {
        let Some(cog) = layer.cog() else { continue };
        if !cog.merc_bounds().intersects(&view) {
            continue;
        }
        let target = cog.choose_level(cam.mpp / ppp);
        let coarsest = cog.levels.len() - 1;
        // Zoom relative to native resolution: 100% = one image pixel per screen pixel.
        level_info = Some((target, cog.merc_pixel_size() / (cam.mpp / ppp)));
        let tint = Color32::WHITE.gamma_multiply(layer.opacity);

        // Coarse levels first so there's always something under the sharp tiles.
        for level in (target..=coarsest).rev() {
            let need = level == target || level == coarsest;
            for (tx, ty) in cog.visible_tiles(level, &view) {
                let key = TileKey { layer: layer.id, level: level as u8, tx, ty };
                match tiles.texture(&key) {
                    Some(Some(tex)) => {
                        let c = cog.tile_corners_merc(level, tx, ty).map(|(x, y)| cam.to_screen(rect, x, y));
                        let mut mesh = Mesh::with_texture(tex.id());
                        let uv = [pos2(0.0, 0.0), pos2(1.0, 0.0), pos2(1.0, 1.0), pos2(0.0, 1.0)];
                        for i in 0..4 {
                            mesh.vertices.push(egui::epaint::Vertex { pos: c[i], uv: uv[i], color: tint });
                        }
                        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                        painter.add(Shape::mesh(mesh));
                    }
                    Some(None) => {}
                    None if need => wanted.push((key, cog.clone())),
                    None => {}
                }
            }
        }
        // Outline of the mosaic footprint.
        let b = cog.merc_bounds();
        let r = Rect::from_two_pos(cam.to_screen(rect, b.min_x, b.max_y), cam.to_screen(rect, b.max_x, b.min_y));
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_white_alpha(40)), egui::StrokeKind::Outside);
    }
    // Load the centre of the screen first.
    let (vcx, vcy) = view.center();
    wanted.sort_by(|a, b| {
        let da = tile_dist(&a.1, &a.0, vcx, vcy);
        let db = tile_dist(&b.1, &b.0, vcx, vcy);
        b.0.level.cmp(&a.0.level).then(da.total_cmp(&db))
    });
    tiles.request(wanted);

    MapOutput {
        resp: resp.clone(),
        hover_lonlat: resp.hover_pos().map(|p| {
            let (x, y) = cam.to_world(rect, p);
            merc_to_lonlat(x, y)
        }),
        level: level_info,
        rect,
    }
}

fn tile_dist(cog: &Cog, k: &TileKey, cx: f64, cy: f64) -> f64 {
    let c = cog.tile_corners_merc(k.level as usize, k.tx, k.ty);
    let mx = (c[0].0 + c[2].0) / 2.0;
    let my = (c[0].1 + c[2].1) / 2.0;
    (mx - cx).powi(2) + (my - cy).powi(2)
}

/// Ground-distance scale bar, bottom-left.
pub fn scale_bar(painter: &egui::Painter, rect: Rect, cam: &Camera) {
    let (_, lat) = merc_to_lonlat(cam.cx, cam.cy);
    let ground_per_pt = cam.mpp * lat.to_radians().cos();
    let target = ground_per_pt * 120.0;
    let pow = 10f64.powf(target.log10().floor());
    let nice = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * pow).filter(|v| *v <= target).fold(pow, f64::max);
    let len = (nice / ground_per_pt) as f32;
    let label = if nice >= 1000.0 {
        format!("{} km", nice / 1000.0)
    } else if nice >= 1.0 {
        format!("{nice} m")
    } else if nice >= 0.01 {
        format!("{} cm", (nice * 100.0 * 1000.0).round() / 1000.0)
    } else {
        format!("{} mm", (nice * 1000.0 * 1000.0).round() / 1000.0)
    };
    let o = rect.left_bottom() + vec2(14.0, -16.0);
    let bg = Rect::from_min_size(o + vec2(-6.0, -22.0), Vec2::new(len + 12.0, 30.0));
    painter.rect_filled(bg, 4.0, Color32::from_black_alpha(140));
    let s = Stroke::new(2.0, Color32::WHITE);
    painter.line_segment([o, o + vec2(len, 0.0)], s);
    painter.line_segment([o, o + vec2(0.0, -6.0)], s);
    painter.line_segment([o + vec2(len, 0.0), o + vec2(len, -6.0)], s);
    painter.text(o + vec2(len / 2.0, -8.0), egui::Align2::CENTER_BOTTOM, label, egui::FontId::proportional(12.0), Color32::WHITE);
}
