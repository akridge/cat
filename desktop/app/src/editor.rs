//! Annotation editing on the map: draw tools, selection, vertex editing,
//! snapping, undo/redo and rendering.
//!
//! Annotations are kept in WGS84 lon/lat (as on the server); every frame they
//! are projected to screen through WebMercator with f64 precision.

use std::collections::HashMap;
use std::time::Instant;

use cat_cog::geo::{lonlat_to_merc, merc_to_lonlat};
use cat_core::annotation::LonLat;
use cat_core::geom;
use cat_core::{AnnKind, Annotation, Change, Geom, History};
use egui::{pos2, vec2, Color32, Pos2, Rect, Shape, Stroke};
use serde_json::{Map, Value};

use crate::map::Camera;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Point,
    Line,
    Rect,
    Polygon,
}

impl Tool {
    pub const ALL: [Tool; 5] = [Tool::Select, Tool::Point, Tool::Line, Tool::Rect, Tool::Polygon];
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "⬉ Select",
            Tool::Point => "● Point",
            Tool::Line => "╱ Line",
            Tool::Rect => "▭ Rectangle",
            Tool::Polygon => "⬠ Polygon",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "V",
            Tool::Point => "P",
            Tool::Line => "L",
            Tool::Rect => "R",
            Tool::Polygon => "G",
        }
    }
    pub fn help(self) -> &'static str {
        match self {
            Tool::Select => "Click a feature to select it. Drag vertices to reshape, drag the small dots to add a vertex, Alt+click a vertex to remove it, drag inside to move.",
            Tool::Point => "Click to place a point.",
            Tool::Line => "Click to add vertices. Double-click or Enter to finish, Backspace removes the last vertex, Esc cancels.",
            Tool::Rect => "Drag to draw a rectangle.",
            Tool::Polygon => "Click to add vertices. Click the first vertex, double-click or press Enter to finish. Backspace removes the last vertex, Esc cancels.",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Style {
    pub line_width: f32,
    pub fill_opacity: f32,
    pub labels: bool,
    pub complete: [u8; 3],
    pub incomplete: [u8; 3],
}

impl Default for Style {
    fn default() -> Self {
        // Same defaults as the web app: bright pink-red stands out on reef imagery.
        Style { line_width: 2.5, fill_opacity: 0.18, labels: true, complete: [0xff, 0x2d, 0x6f], incomplete: [0xff, 0xd0, 0x00] }
    }
}

enum Drag {
    Vertex { uuid: String, idx: usize, before: Annotation },
    Body { uuid: String, before: Annotation, last: [f64; 2] },
    RectDraw { start: [f64; 2] },
}

/// What the app should react to after a frame.
#[derive(Default)]
pub struct Events {
    /// A new annotation was finished: focus the form's species field.
    pub created: Option<String>,
    /// User asked to zoom to the selection.
    pub zoom_to: Option<[f64; 4]>,
}

pub struct Editor {
    pub tool: Tool,
    pub items: Vec<Annotation>,
    pub history: History,
    pub selected: Option<String>,
    pub snap: bool,
    pub style: Style,
    /// Bumped on every change; the app autosaves when it moves.
    pub rev: u64,
    /// Properties copied onto each new annotation (sticky fields etc.).
    pub new_props: Map<String, Value>,
    /// Active-time seconds at the last finished annotation (per-annotation timing).
    pub timer_mark: f64,
    pub toast: Option<(String, Instant)>,
    drawing: Vec<[f64; 2]>,
    drag: Option<Drag>,
    tri_cache: HashMap<String, (u64, Vec<u32>)>,
    hover_vertex_close: bool,
}

const HANDLE: f32 = 6.0;
const SNAP_PX: f32 = 10.0;

fn ll_to_merc(p: LonLat) -> [f64; 2] {
    let (x, y) = lonlat_to_merc(p[0], p[1]);
    [x, y]
}
fn merc_to_ll(p: [f64; 2]) -> LonLat {
    let (x, y) = merc_to_lonlat(p[0], p[1]);
    [x, y]
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn geom_hash(g: &Geom) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in g.vertices() {
        p[0].to_bits().hash(&mut h);
        p[1].to_bits().hash(&mut h);
    }
    h.finish()
}

impl Editor {
    pub fn new() -> Self {
        Self {
            tool: Tool::Select,
            items: Vec::new(),
            history: History::default(),
            selected: None,
            snap: true,
            style: Style::default(),
            rev: 0,
            new_props: Map::new(),
            timer_mark: 0.0,
            toast: None,
            drawing: Vec::new(),
            drag: None,
            tri_cache: HashMap::new(),
            hover_vertex_close: false,
        }
    }

    /// Replace the annotation set (opening another mosaic).
    pub fn load(&mut self, items: Vec<Annotation>) {
        self.items = items;
        self.history.clear();
        self.selected = None;
        self.drawing.clear();
        self.drag = None;
        self.tri_cache.clear();
        self.rev += 1;
    }

    pub fn is_drawing(&self) -> bool {
        !self.drawing.is_empty() || matches!(self.drag, Some(Drag::RectDraw { .. }))
    }

    pub fn get(&self, uuid: &str) -> Option<&Annotation> {
        self.items.iter().find(|a| a.uuid == uuid)
    }
    pub fn get_mut(&mut self, uuid: &str) -> Option<&mut Annotation> {
        self.items.iter_mut().find(|a| a.uuid == uuid)
    }
    pub fn selected_ann(&self) -> Option<&Annotation> {
        self.selected.as_deref().and_then(|u| self.get(u))
    }

    pub fn set_tool(&mut self, t: Tool) {
        if t != self.tool {
            self.drawing.clear();
            self.drag = None;
            self.tool = t;
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn commit(&mut self, label: &str, changes: Vec<Change>) {
        self.history.commit(&mut self.items, label, changes);
        self.rev += 1;
    }

    /// Record an edit already applied to `items` (form edits, drags).
    pub fn record(&mut self, label: &str, changes: Vec<Change>) {
        self.history.record(label, changes);
        self.rev += 1;
    }

    pub fn undo(&mut self) {
        self.drawing.clear();
        if let Some(l) = self.history.undo(&mut self.items) {
            self.rev += 1;
            self.toast(format!("Undo: {l}"));
            if self.selected.as_deref().is_some_and(|u| self.get(u).is_none()) {
                self.selected = None;
            }
        }
    }
    pub fn redo(&mut self) {
        if let Some(l) = self.history.redo(&mut self.items) {
            self.rev += 1;
            self.toast(format!("Redo: {l}"));
        }
    }

    pub fn delete_selected(&mut self) {
        if let Some(a) = self.selected_ann().cloned() {
            self.commit("Delete annotation", vec![Change { before: Some(a), after: None }]);
            self.selected = None;
            self.toast("Deleted · Ctrl+Z to undo");
        }
    }

    fn finish(&mut self, kind: AnnKind, pts: Vec<[f64; 2]>, active_secs: f64) -> Option<String> {
        let ll: Vec<LonLat> = pts.iter().map(|p| merc_to_ll(*p)).collect();
        let geom = match kind {
            AnnKind::Point => Geom::Point(ll[0]),
            AnnKind::Line => Geom::Line(ll),
            AnnKind::Rectangle | AnnKind::Polygon => Geom::Polygon(ll),
        };
        if !geom.is_valid() {
            return None;
        }
        let mut a = Annotation::new(kind, geom, self.new_props.clone());
        let secs = (active_secs - self.timer_mark).max(0.0);
        a.props.insert("annotation_time_seconds".into(), serde_json::json!((secs * 10.0).round() / 10.0));
        self.timer_mark = active_secs;
        let uuid = a.uuid.clone();
        self.commit(&format!("Add {}", kind.label().to_lowercase()), vec![Change { before: None, after: Some(a) }]);
        self.selected = Some(uuid.clone());
        Some(uuid)
    }

    // ---------------------------------------------------------------------
    // Hit testing (screen space)
    // ---------------------------------------------------------------------

    fn screen_pts(cam: &Camera, rect: Rect, a: &Annotation) -> Vec<Pos2> {
        a.geom.vertices().iter().map(|p| {
            let m = ll_to_merc(*p);
            cam.to_screen(rect, m[0], m[1])
        }).collect()
    }

    /// Topmost annotation under `p` (points/lines by distance, polygons by
    /// containment or edge distance).
    fn hit(&self, cam: &Camera, rect: Rect, p: Pos2) -> Option<String> {
        let pp = [p.x as f64, p.y as f64];
        let mut best: Option<(f64, &str)> = None;
        for a in self.items.iter().rev() {
            let pts: Vec<[f64; 2]> = Self::screen_pts(cam, rect, a).iter().map(|q| [q.x as f64, q.y as f64]).collect();
            let d = match &a.geom {
                Geom::Point(_) => geom::dist2(pp, pts[0]).sqrt(),
                Geom::Line(_) => geom::point_polyline(pp, &pts, false).0,
                Geom::Polygon(_) => {
                    if geom::point_in_polygon(pp, &pts) {
                        // Inside: prefer smaller polygons stacked on top of bigger ones.
                        let area = geom::signed_area(&pts).abs();
                        let edge = geom::point_polyline(pp, &pts, true).0;
                        if edge <= 6.0 { 0.0 } else { 6.0 + area.sqrt() * 1e-3 }
                    } else {
                        geom::point_polyline(pp, &pts, true).0
                    }
                }
            };
            let limit = if matches!(a.geom, Geom::Polygon(_)) { 6.0 + 1e9 } else { 9.0 };
            let ok = match &a.geom {
                Geom::Polygon(_) => d < 7.0 || geom::point_in_polygon(pp, &pts),
                _ => d <= limit,
            };
            if ok && best.is_none_or(|b| d < b.0) {
                best = Some((d, a.uuid.as_str()));
            }
        }
        best.map(|b| b.1.to_string())
    }

    /// Nearest vertex of any annotation other than `exclude`, within SNAP_PX.
    fn snap_target(&self, cam: &Camera, rect: Rect, p: Pos2, exclude: Option<&str>) -> Option<[f64; 2]> {
        let mut best: Option<(f32, [f64; 2])> = None;
        let view = cam.view_box(rect);
        for a in &self.items {
            if Some(a.uuid.as_str()) == exclude {
                continue;
            }
            for v in a.geom.vertices() {
                let m = ll_to_merc(*v);
                if m[0] < view.min_x || m[0] > view.max_x || m[1] < view.min_y || m[1] > view.max_y {
                    continue;
                }
                let d = cam.to_screen(rect, m[0], m[1]).distance(p);
                if d < SNAP_PX && best.is_none_or(|b| d < b.0) {
                    best = Some((d, m));
                }
            }
        }
        // Also snap to the polygon being drawn (closing / adjacent vertices).
        for m in &self.drawing {
            let d = cam.to_screen(rect, m[0], m[1]).distance(p);
            if d < SNAP_PX && best.is_none_or(|b| d < b.0) {
                best = Some((d, *m));
            }
        }
        best.map(|b| b.1)
    }

    fn world(cam: &Camera, rect: Rect, p: Pos2) -> [f64; 2] {
        let (x, y) = cam.to_world(rect, p);
        [x, y]
    }

    /// Called before the map handles input: may claim the primary drag.
    /// Returns whether the map may pan with the primary button.
    pub fn pre_input(&mut self, ui: &egui::Ui, rect: Rect, cam: &Camera) -> bool {
        let (pressed, origin, space, alt) = ui.input(|i| {
            (i.pointer.primary_pressed(), i.pointer.press_origin(), i.key_down(egui::Key::Space), i.modifiers.alt)
        });
        if space {
            return true;
        }
        if pressed && self.drag.is_none() {
            if let Some(o) = origin.filter(|o| rect.contains(*o)) {
                match self.tool {
                    Tool::Rect => self.drag = Some(Drag::RectDraw { start: Self::world(cam, rect, o) }),
                    Tool::Select if !alt => self.start_select_drag(cam, rect, o),
                    _ => {}
                }
            }
        }
        self.drag.is_none()
    }

    fn start_select_drag(&mut self, cam: &Camera, rect: Rect, o: Pos2) {
        let Some(sel) = self.selected_ann().cloned() else { return };
        let pts = Self::screen_pts(cam, rect, &sel);
        if let Some(idx) = pts.iter().position(|p| p.distance(o) <= HANDLE + 2.0) {
            self.drag = Some(Drag::Vertex { uuid: sel.uuid.clone(), idx, before: sel });
            return;
        }
        // Midpoint handles insert a vertex.
        let closed = matches!(sel.geom, Geom::Polygon(_));
        let n = pts.len();
        let segs = match sel.geom {
            Geom::Point(_) => 0,
            Geom::Line(_) => n - 1,
            Geom::Polygon(_) => n,
        };
        for i in 0..segs {
            let mid = pts[i].lerp(pts[(i + 1) % n], 0.5);
            if mid.distance(o) <= HANDLE {
                let mut a = sel.clone();
                let ll = merc_to_ll(Self::world(cam, rect, mid));
                match &mut a.geom {
                    Geom::Line(v) | Geom::Polygon(v) => v.insert(i + 1, ll),
                    Geom::Point(_) => {}
                }
                let _ = closed;
                let uuid = a.uuid.clone();
                if let Some(x) = self.get_mut(&uuid) {
                    *x = a;
                }
                self.drag = Some(Drag::Vertex { uuid, idx: i + 1, before: sel });
                return;
            }
        }
        if self.hit(cam, rect, o).as_deref() == Some(sel.uuid.as_str()) {
            self.drag = Some(Drag::Body { uuid: sel.uuid.clone(), last: Self::world(cam, rect, o), before: sel });
        }
    }

    // ---------------------------------------------------------------------
    // Per-frame input + drawing
    // ---------------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut egui::Ui, resp: &egui::Response, rect: Rect, cam: &mut Camera, active_secs: f64) -> Events {
        let mut ev = Events::default();
        let hover = resp.hover_pos();
        let keys_free = !ui.ctx().egui_wants_keyboard_input();
        let (alt, space) = ui.input(|i| (i.modifiers.alt, i.key_down(egui::Key::Space)));

        // Snap only while placing or dragging vertices.
        let placing = matches!(self.tool, Tool::Point | Tool::Line | Tool::Polygon | Tool::Rect);
        let dragging_vertex = match &self.drag {
            Some(Drag::Vertex { uuid, .. }) => Some(uuid.clone()),
            _ => None,
        };
        let snap = if self.snap && !space && (placing || dragging_vertex.is_some()) {
            hover.and_then(|h| self.snap_target(cam, rect, h, dragging_vertex.as_deref()))
        } else {
            None
        };
        let cursor_world = hover.map(|h| snap.unwrap_or_else(|| Self::world(cam, rect, h)));

        // --- Keyboard ---------------------------------------------------
        if keys_free {
            let (enter, back, esc, del) = ui.input(|i| {
                (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Backspace), i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Delete))
            });
            if enter && !self.drawing.is_empty() {
                self.finish_drawing(active_secs, &mut ev);
            }
            if back && !self.drawing.is_empty() {
                self.drawing.pop();
            } else if (back || del) && self.drawing.is_empty() && self.selected.is_some() {
                self.delete_selected();
            }
            if esc {
                if self.is_drawing() {
                    self.drawing.clear();
                    self.drag = None;
                } else if self.selected.is_some() {
                    self.selected = None;
                } else {
                    self.set_tool(Tool::Select);
                }
            }
        }

        // --- Mouse ------------------------------------------------------
        let clicked = resp.clicked() && !space;
        let double = resp.double_clicked() && !space;
        match self.tool {
            Tool::Point => {
                if clicked {
                    if let Some(w) = cursor_world {
                        ev.created = self.finish(AnnKind::Point, vec![w], active_secs);
                    }
                }
            }
            Tool::Line | Tool::Polygon => {
                if double {
                    // The second click of a double-click also arrived as a click.
                    if self.drawing.len() >= 2 {
                        let n = self.drawing.len();
                        let a = cam.to_screen(rect, self.drawing[n - 1][0], self.drawing[n - 1][1]);
                        let b = cam.to_screen(rect, self.drawing[n - 2][0], self.drawing[n - 2][1]);
                        if a.distance(b) < 4.0 {
                            self.drawing.pop();
                        }
                    }
                    self.finish_drawing(active_secs, &mut ev);
                } else if clicked {
                    if let Some(w) = cursor_world {
                        // Clicking the first vertex closes the polygon.
                        let closes = self.tool == Tool::Polygon && self.drawing.len() >= 3 && {
                            let f = self.drawing[0];
                            hover.is_some_and(|h| cam.to_screen(rect, f[0], f[1]).distance(h) <= HANDLE + 3.0)
                        };
                        if closes {
                            self.finish_drawing(active_secs, &mut ev);
                        } else {
                            self.drawing.push(w);
                        }
                    }
                }
            }
            Tool::Rect => {
                if let (Some(Drag::RectDraw { start }), true) = (&self.drag, resp.drag_stopped()) {
                    let start = *start;
                    self.drag = None;
                    if let Some(end) = cursor_world {
                        let (a, b) = (cam.to_screen(rect, start[0], start[1]), cam.to_screen(rect, end[0], end[1]));
                        if (a.x - b.x).abs() > 3.0 && (a.y - b.y).abs() > 3.0 {
                            let pts = vec![start, [end[0], start[1]], end, [start[0], end[1]]];
                            ev.created = self.finish(AnnKind::Rectangle, pts, active_secs);
                        }
                    }
                } else if !resp.dragged() && !resp.drag_stopped() && matches!(self.drag, Some(Drag::RectDraw { .. })) && !ui.input(|i| i.pointer.primary_down()) {
                    self.drag = None;
                }
            }
            Tool::Select => {
                if clicked {
                    if let Some(h) = hover {
                        if alt {
                            self.alt_click_vertex(cam, rect, h);
                        } else {
                            self.selected = self.hit(cam, rect, h);
                        }
                    }
                }
                if double {
                    if let Some(a) = self.selected_ann() {
                        ev.zoom_to = Some(a.geom.bbox());
                    }
                }
                self.update_select_drag(resp, cam, rect, snap);
            }
        }

        // --- Render -----------------------------------------------------
        let painter = ui.painter_at(rect);
        self.paint(&painter, cam, rect);
        self.paint_drawing(&painter, cam, rect, hover, cursor_world);
        if let Some(s) = snap {
            let p = cam.to_screen(rect, s[0], s[1]);
            painter.circle_stroke(p, 8.0, Stroke::new(2.0, Color32::from_rgb(0, 230, 255)));
        }
        if let Some(h) = hover {
            let icon = match (self.tool, &self.drag) {
                (_, _) if space => egui::CursorIcon::Grab,
                (Tool::Select, Some(_)) => egui::CursorIcon::Grabbing,
                (Tool::Select, None) => {
                    if self.hover_vertex_close { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Default }
                }
                _ => egui::CursorIcon::Crosshair,
            };
            if rect.contains(h) {
                ui.ctx().set_cursor_icon(icon);
            }
        }
        ev
    }

    fn finish_drawing(&mut self, active_secs: f64, ev: &mut Events) {
        let pts = std::mem::take(&mut self.drawing);
        let kind = if self.tool == Tool::Line { AnnKind::Line } else { AnnKind::Polygon };
        let needed = if kind == AnnKind::Line { 2 } else { 3 };
        if pts.len() >= needed {
            ev.created = self.finish(kind, pts, active_secs);
        } else if !pts.is_empty() {
            self.toast(format!("A {} needs at least {needed} vertices", kind.label().to_lowercase()));
        }
    }

    fn alt_click_vertex(&mut self, cam: &Camera, rect: Rect, h: Pos2) {
        let Some(sel) = self.selected_ann().cloned() else { return };
        let pts = Self::screen_pts(cam, rect, &sel);
        if let Some(idx) = pts.iter().position(|p| p.distance(h) <= HANDLE + 2.0) {
            let mut a = sel.clone();
            if let Geom::Line(v) | Geom::Polygon(v) = &mut a.geom {
                v.remove(idx);
            }
            if a.geom.is_valid() {
                a.touch();
                self.commit("Remove vertex", vec![Change { before: Some(sel), after: Some(a) }]);
            } else {
                self.toast("Can't remove: too few vertices left");
            }
        }
    }

    fn update_select_drag(&mut self, resp: &egui::Response, cam: &Camera, rect: Rect, snap: Option<[f64; 2]>) {
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let Some(drag) = self.drag.take() else {
            self.hover_vertex_close = false;
            if let (Some(sel), Some(h)) = (self.selected_ann(), resp.hover_pos()) {
                self.hover_vertex_close = Self::screen_pts(cam, rect, sel).iter().any(|p| p.distance(h) <= HANDLE + 2.0);
            }
            return;
        };
        let released = resp.drag_stopped() || !resp.is_pointer_button_down_on() && !resp.dragged();
        match drag {
            Drag::Vertex { uuid, idx, before } => {
                if let Some(p) = pointer {
                    let w = snap.unwrap_or_else(|| Self::world(cam, rect, p));
                    if let Some(a) = self.get_mut(&uuid) {
                        if let Some(v) = a.geom.vertices_mut().get_mut(idx) {
                            *v = merc_to_ll(w);
                        }
                    }
                }
                if released {
                    if let Some(after) = self.get_mut(&uuid).map(|a| {
                        a.touch();
                        a.clone()
                    }) {
                        let moved = after.geom != before.geom;
                        if moved {
                            let label = if after.geom.vertices().len() > before.geom.vertices().len() { "Add vertex" } else { "Move vertex" };
                            self.record(label, vec![Change { before: Some(before), after: Some(after) }]);
                        } else {
                            // A click on a handle without moving: restore exactly.
                            if let Some(a) = self.get_mut(&uuid) {
                                *a = before;
                            }
                        }
                    }
                } else {
                    self.drag = Some(Drag::Vertex { uuid, idx, before });
                }
            }
            Drag::Body { uuid, before, last } => {
                let mut last = last;
                if let Some(p) = pointer {
                    if resp.dragged() {
                        let w = Self::world(cam, rect, p);
                        let (dx, dy) = (w[0] - last[0], w[1] - last[1]);
                        last = w;
                        if let Some(a) = self.get_mut(&uuid) {
                            for v in a.geom.vertices_mut() {
                                let m = ll_to_merc(*v);
                                *v = merc_to_ll([m[0] + dx, m[1] + dy]);
                            }
                        }
                    }
                }
                if released {
                    if let Some(after) = self.get_mut(&uuid).map(|a| {
                        a.touch();
                        a.clone()
                    }) {
                        if after.geom != before.geom {
                            self.record("Move annotation", vec![Change { before: Some(before), after: Some(after) }]);
                        } else if let Some(a) = self.get_mut(&uuid) {
                            *a = before;
                        }
                    }
                } else {
                    self.drag = Some(Drag::Body { uuid, before, last });
                }
            }
            Drag::RectDraw { .. } => {}
        }
    }

    // ---------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------

    fn triangles(&mut self, a: &Annotation) -> Vec<u32> {
        let h = geom_hash(&a.geom);
        if let Some((hh, t)) = self.tri_cache.get(&a.uuid) {
            if *hh == h {
                return t.clone();
            }
        }
        let flat: Vec<f64> = a.geom.vertices().iter().flat_map(|p| [p[0], p[1]]).collect();
        let t: Vec<u32> = earcutr::earcut(&flat, &[], 2).unwrap_or_default().into_iter().map(|i| i as u32).collect();
        self.tri_cache.insert(a.uuid.clone(), (h, t.clone()));
        t
    }

    fn paint(&mut self, painter: &egui::Painter, cam: &Camera, rect: Rect) {
        let view = cam.view_box(rect);
        let lw = self.style.line_width;
        let items = std::mem::take(&mut self.items);
        let mut labels = Vec::new();
        for a in &items {
            let b = a.geom.bbox();
            let (lo, hi) = (ll_to_merc([b[0], b[1]]), ll_to_merc([b[2], b[3]]));
            if hi[0] < view.min_x || lo[0] > view.max_x || hi[1] < view.min_y || lo[1] > view.max_y {
                continue;
            }
            let selected = self.selected.as_deref() == Some(a.uuid.as_str());
            let base = if a.is_complete() { rgb(self.style.complete) } else { rgb(self.style.incomplete) };
            let color = if selected { Color32::from_rgb(0, 230, 255) } else { base };
            let pts = Self::screen_pts(cam, rect, a);
            let stroke = Stroke::new(if selected { lw + 1.0 } else { lw }, color);
            match &a.geom {
                Geom::Point(_) => {
                    painter.circle(pts[0], 5.5, color, Stroke::new(1.5, Color32::WHITE));
                }
                Geom::Line(_) => {
                    painter.add(Shape::line(pts.clone(), stroke));
                }
                Geom::Polygon(_) => {
                    let tris = self.triangles(a);
                    if !tris.is_empty() {
                        let fill = color.gamma_multiply(if selected { self.style.fill_opacity * 1.6 } else { self.style.fill_opacity });
                        let mut mesh = egui::Mesh::default();
                        for p in &pts {
                            mesh.colored_vertex(*p, fill);
                        }
                        mesh.indices = tris;
                        painter.add(Shape::mesh(mesh));
                    }
                    painter.add(Shape::closed_line(pts.clone(), stroke));
                }
            }
            if self.style.labels {
                let bb = Rect::from_points(&pts);
                let code = a.spcode();
                if !code.is_empty() && (bb.width() > 26.0 || bb.height() > 18.0 || matches!(a.geom, Geom::Point(_))) {
                    let c = match &a.geom {
                        Geom::Polygon(_) => {
                            let r: Vec<[f64; 2]> = pts.iter().map(|p| [p.x as f64, p.y as f64]).collect();
                            let c = geom::centroid(&r);
                            pos2(c[0] as f32, c[1] as f32)
                        }
                        Geom::Point(_) => pts[0] + vec2(0.0, -14.0),
                        Geom::Line(_) => pts[pts.len() / 2],
                    };
                    labels.push((c, code));
                }
            }
            if selected {
                self.paint_handles(painter, a, &pts);
            }
        }
        self.items = items;
        for (c, code) in labels {
            let galley = painter.layout_no_wrap(code, egui::FontId::proportional(12.0), Color32::WHITE);
            let r = Rect::from_center_size(c, galley.size() + vec2(8.0, 2.0));
            painter.rect_filled(r, 3.0, Color32::from_black_alpha(160));
            painter.galley(r.min + vec2(4.0, 1.0), galley, Color32::WHITE);
        }
    }

    fn paint_handles(&self, painter: &egui::Painter, a: &Annotation, pts: &[Pos2]) {
        let n = pts.len();
        let segs = match a.geom {
            Geom::Point(_) => 0,
            Geom::Line(_) => n - 1,
            Geom::Polygon(_) => n,
        };
        for i in 0..segs {
            let mid = pts[i].lerp(pts[(i + 1) % n], 0.5);
            if pts[i].distance(pts[(i + 1) % n]) > 24.0 {
                painter.circle(mid, 3.5, Color32::from_white_alpha(140), Stroke::new(1.0, Color32::from_black_alpha(160)));
            }
        }
        for p in pts {
            painter.rect(Rect::from_center_size(*p, vec2(HANDLE * 1.6, HANDLE * 1.6)), 1.0, Color32::WHITE, Stroke::new(1.5, Color32::from_rgb(0, 150, 200)), egui::StrokeKind::Middle);
        }
    }

    fn paint_drawing(&self, painter: &egui::Painter, cam: &Camera, rect: Rect, hover: Option<Pos2>, cursor_world: Option<[f64; 2]>) {
        let accent = Color32::from_rgb(0, 230, 255);
        let s = |m: &[f64; 2]| cam.to_screen(rect, m[0], m[1]);
        if let Some(Drag::RectDraw { start }) = &self.drag {
            if let Some(end) = cursor_world {
                let r = Rect::from_two_pos(s(start), s(&end));
                painter.rect(r, 0.0, accent.gamma_multiply(0.15), Stroke::new(2.0, accent), egui::StrokeKind::Middle);
                let ll = [merc_to_ll(*start), merc_to_ll([end[0], start[1]]), merc_to_ll(end), merc_to_ll([start[0], end[1]])];
                measure_label(painter, r.right_bottom() + vec2(10.0, 10.0), &format!("{} · {}", geom::fmt_area(geom::area_m2(&ll)), dims(&ll)));
            }
            return;
        }
        if self.drawing.is_empty() {
            return;
        }
        let mut pts: Vec<Pos2> = self.drawing.iter().map(s).collect();
        if let Some(c) = cursor_world {
            pts.push(s(&c));
        }
        if self.tool == Tool::Polygon && pts.len() >= 3 {
            let mut mesh = egui::Mesh::default();
            let flat: Vec<f64> = pts.iter().flat_map(|p| [p.x as f64, p.y as f64]).collect();
            for p in &pts {
                mesh.colored_vertex(*p, accent.gamma_multiply(0.12));
            }
            mesh.indices = earcutr::earcut(&flat, &[], 2).unwrap_or_default().into_iter().map(|i| i as u32).collect();
            painter.add(Shape::mesh(mesh));
            painter.add(Shape::dashed_line(&[*pts.last().unwrap(), pts[0]], Stroke::new(1.5, accent.gamma_multiply(0.6)), 6.0, 4.0));
        }
        painter.add(Shape::line(pts.clone(), Stroke::new(2.0, accent)));
        for (i, p) in self.drawing.iter().map(s).enumerate() {
            let first_closable = i == 0 && self.tool == Tool::Polygon && self.drawing.len() >= 3;
            let r = if first_closable { 7.0 } else { 4.0 };
            let hot = first_closable && hover.is_some_and(|h| h.distance(p) <= HANDLE + 3.0);
            painter.circle(p, r, if hot { accent } else { Color32::WHITE }, Stroke::new(1.5, accent));
        }
        // Live measurement next to the cursor.
        if let (Some(h), Some(c)) = (hover, cursor_world) {
            let mut ll: Vec<LonLat> = self.drawing.iter().map(|m| merc_to_ll(*m)).collect();
            ll.push(merc_to_ll(c));
            let text = if self.tool == Tool::Polygon && ll.len() >= 3 {
                format!("{} · {} vertices", geom::fmt_area(geom::area_m2(&ll)), ll.len())
            } else {
                geom::fmt_length(geom::length_m(&ll, false))
            };
            measure_label(painter, h + vec2(14.0, 14.0), &text);
        }
    }
}

fn dims(r: &[LonLat]) -> String {
    format!("{} × {}", geom::fmt_length(geom::length_m(&r[..2], false)), geom::fmt_length(geom::length_m(&r[1..3], false)))
}

fn measure_label(painter: &egui::Painter, at: Pos2, text: &str) {
    let galley = painter.layout_no_wrap(text.to_string(), egui::FontId::proportional(12.0), Color32::WHITE);
    let r = Rect::from_min_size(at, galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(r, 4.0, Color32::from_black_alpha(190));
    painter.galley(r.min + vec2(5.0, 2.0), galley, Color32::WHITE);
}

/// Ground size of an annotation for display.
pub fn size_text(a: &Annotation) -> String {
    match &a.geom {
        Geom::Point(_) => "—".into(),
        Geom::Line(v) => geom::fmt_length(geom::length_m(v, false)),
        Geom::Polygon(v) => geom::fmt_area(geom::area_m2(v)),
    }
}

/// Sortable size value (m² for polygons, m for lines).
pub fn size_value(a: &Annotation) -> f64 {
    match &a.geom {
        Geom::Point(_) => 0.0,
        Geom::Line(v) => geom::length_m(v, false),
        Geom::Polygon(v) => geom::area_m2(v),
    }
}
