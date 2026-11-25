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
    Measure,
}

impl Tool {
    pub const ALL: [Tool; 6] = [Tool::Select, Tool::Point, Tool::Line, Tool::Rect, Tool::Polygon, Tool::Measure];
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "⬉ Select",
            Tool::Point => "● Point",
            Tool::Line => "╱ Line",
            Tool::Rect => "▭ Rectangle",
            Tool::Polygon => "⬠ Polygon",
            Tool::Measure => "📏 Measure",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "V",
            Tool::Point => "P",
            Tool::Line => "L",
            Tool::Rect => "R",
            Tool::Polygon => "G",
            Tool::Measure => "M",
        }
    }
    pub fn help(self) -> &'static str {
        match self {
            Tool::Select => "Click a feature to select it. Drag vertices to reshape, drag the small dots to add a vertex, Alt+click a vertex to remove it, drag inside to move.",
            Tool::Point => "Click to place a point.",
            Tool::Line => "Click to add vertices, or Shift+drag to trace freehand. Double-click, right-click or Enter finishes; Backspace/Ctrl+Z removes the last vertex; Esc cancels.",
            Tool::Rect => "Drag to draw a rectangle.",
            Tool::Polygon => "Click to add vertices, or Shift+drag to trace an outline in one stroke. Click the first vertex, double-click, right-click or Enter finishes; Backspace/Ctrl+Z removes the last vertex; Esc cancels.",
            Tool::Measure => "Click to measure distances (each segment and total); 3+ points also show the area. Double-click or Enter ends, Esc clears. Nothing is saved.",
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
    pub color_by: ColorBy,
}

/// What annotation colours mean (same modes and colours as the web app's
/// symbology, annotation-runtime-symbology.js).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ColorBy {
    /// Complete vs needs species.
    #[default]
    Status,
    Species,
    Condition,
    Annotator,
}

impl ColorBy {
    pub const ALL: [ColorBy; 4] = [ColorBy::Status, ColorBy::Species, ColorBy::Condition, ColorBy::Annotator];
    pub fn label(self) -> &'static str {
        match self {
            ColorBy::Status => "Status (complete / needs species)",
            ColorBy::Species => "Species",
            ColorBy::Condition => "Condition (con_1)",
            ColorBy::Annotator => "Annotator",
        }
    }
    pub fn value(self, a: &Annotation) -> Option<String> {
        let v = match self {
            ColorBy::Status => return Some(if a.is_complete() { "Complete".into() } else { "Needs species".into() }),
            ColorBy::Species => a.spcode(),
            ColorBy::Condition => a.text("con_1"),
            ColorBy::Annotator => {
                let c = a.text("created_by");
                if c.is_empty() { a.text("analyst") } else { c }
            }
        };
        let v = v.trim().to_uppercase();
        (!v.is_empty() && v != "-").then_some(v)
    }
}

const PALETTE: [u32; 24] = [
    0xe6194b, 0x3cb44b, 0x4363d8, 0xf58231, 0x911eb4, 0x46f0f0, 0xf032e6, 0xbcf60c, 0x008080, 0x9a6324, 0x800000, 0x808000,
    0x000075, 0xe6beff, 0xfabebe, 0xffd8b1, 0xaaffc3, 0xffe119, 0x1f77b4, 0xd62728, 0x2ca02c, 0x8c564b, 0x17becf, 0x7f7f7f,
];
pub const MISSING: Color32 = Color32::from_rgb(0x9c, 0xa3, 0xaf);

/// Stable colour for a value: FNV-1a of the upper-cased text into the
/// palette — identical to the web app, so colours match across both.
pub fn color_for(value: &str) -> Color32 {
    let mut h: u32 = 2166136261;
    for c in value.trim().to_uppercase().encode_utf16() {
        h ^= c as u32;
        h = h.wrapping_mul(16777619);
    }
    let c = PALETTE[(h as usize) % PALETTE.len()];
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

impl Default for Style {
    fn default() -> Self {
        // Same defaults as the web app: bright pink-red stands out on reef imagery.
        Style { line_width: 2.5, fill_opacity: 0.18, labels: true, complete: [0xff, 0x2d, 0x6f], incomplete: [0xff, 0xd0, 0x00], color_by: ColorBy::Status }
    }
}

enum Drag {
    Vertex { uuid: String, idx: usize, before: Annotation },
    Body { uuid: String, before: Annotation, last: [f64; 2] },
    RectDraw { start: [f64; 2] },
    /// Shift+drag freehand selection (screen points).
    Lasso { pts: Vec<Pos2> },
    /// Shift+drag freehand tracing in the line/polygon tools (WebMercator).
    Trace { pts: Vec<[f64; 2]>, new_shape: bool },
}

/// What the app should react to after a frame.
#[derive(Default)]
pub struct Events {
    /// A new annotation was finished: focus the form's species field.
    pub created: Option<String>,
    /// User asked to zoom to the selection.
    pub zoom_to: Option<[f64; 4]>,
    /// Annotation under the pointer (Select tool), for a tooltip.
    pub hovered: Option<String>,
}

pub struct Editor {
    pub tool: Tool,
    pub items: Vec<Annotation>,
    pub history: History,
    pub selected: Option<String>,
    /// Multi-selection (Shift+click, lasso, Ctrl+A) for bulk edits.
    pub multi: Vec<String>,
    pub snap: bool,
    pub style: Style,
    /// Bumped on every change; the app autosaves when it moves.
    pub rev: u64,
    /// Properties copied onto each new annotation (sticky fields etc.).
    pub new_props: Map<String, Value>,
    /// Active-time seconds at the last finished annotation (per-annotation timing).
    pub timer_mark: f64,
    pub toast: Option<(String, Instant)>,
    /// Viewer role on a server project: look, select, but don't change.
    pub read_only: bool,
    drawing: Vec<[f64; 2]>,
    /// Measure tool points and whether the measurement is finished.
    measure: Vec<[f64; 2]>,
    measure_done: bool,
    drag: Option<Drag>,
    /// Per-annotation WebMercator projection + fill triangles, reused until
    /// the geometry changes (projecting every vertex every frame was the
    /// editor's main per-frame cost).
    proj: HashMap<String, Proj>,
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

struct Proj {
    geom: Geom,
    merc: Vec<[f64; 2]>,
    /// min_x, min_y, max_x, max_y in WebMercator metres.
    bbox: [f64; 4],
    tris: Option<Vec<u32>>,
}

impl Proj {
    fn new(g: &Geom) -> Proj {
        let merc: Vec<[f64; 2]> = g.vertices().iter().map(|p| ll_to_merc(*p)).collect();
        let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for m in &merc {
            bbox = [bbox[0].min(m[0]), bbox[1].min(m[1]), bbox[2].max(m[0]), bbox[3].max(m[1])];
        }
        Proj { geom: g.clone(), merc, bbox, tris: None }
    }
    fn visible(&self, v: &cat_cog::MercBox, pad: f64) -> bool {
        !(self.bbox[2] < v.min_x - pad || self.bbox[0] > v.max_x + pad || self.bbox[3] < v.min_y - pad || self.bbox[1] > v.max_y + pad)
    }
}

#[allow(dead_code)]
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
            multi: Vec::new(),
            snap: true,
            style: Style::default(),
            rev: 0,
            new_props: Map::new(),
            timer_mark: 0.0,
            toast: None,
            read_only: false,
            drawing: Vec::new(),
            measure: Vec::new(),
            measure_done: false,
            drag: None,
            proj: HashMap::new(),
            hover_vertex_close: false,
        }
    }

    /// Replace the annotation set (opening another mosaic or project).
    pub fn load(&mut self, items: Vec<Annotation>) {
        if self.read_only {
            self.tool = Tool::Select;
        }
        self.items = items;
        self.history.clear();
        self.selected = None;
        self.multi.clear();
        self.drawing.clear();
        self.drag = None;
        self.proj.clear();
        self.rev += 1;
    }

    pub fn is_drawing(&self) -> bool {
        !self.drawing.is_empty() || matches!(self.drag, Some(Drag::RectDraw { .. }) | Some(Drag::Trace { .. }))
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
        if self.read_only && t != Tool::Select {
            self.toast("View only — you can't edit this project");
            return;
        }
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
        // Mid-shape, undo means "remove the last vertex", not "lose the shape".
        if !self.drawing.is_empty() {
            self.drawing.pop();
            return;
        }
        if self.tool == Tool::Measure && !self.measure.is_empty() {
            self.measure.pop();
            self.measure_done = false;
            return;
        }
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
        if self.read_only {
            return;
        }
        if let Some(a) = self.selected_ann().cloned() {
            self.commit("Delete annotation", vec![Change { before: Some(a), after: None }]);
            self.selected = None;
            self.toast("Deleted · Ctrl+Z to undo");
        }
    }

    /// Select the next annotation without a species (after the current one).
    pub fn next_incomplete(&mut self) -> Option<[f64; 4]> {
        let start = self.selected.as_ref().and_then(|u| self.items.iter().position(|a| &a.uuid == u)).map(|i| i + 1).unwrap_or(0);
        let n = self.items.len();
        let found = (0..n).map(|k| (start + k) % n.max(1)).find(|&i| !self.items[i].is_complete());
        match found {
            Some(i) => {
                self.multi.clear();
                self.selected = Some(self.items[i].uuid.clone());
                Some(self.items[i].geom.bbox())
            }
            None => {
                self.toast("Every annotation has a species ✔");
                None
            }
        }
    }

    pub fn delete_multi(&mut self) {
        if self.read_only || self.multi.is_empty() {
            return;
        }
        let changes: Vec<Change> = self.multi.iter().filter_map(|u| self.get(u).cloned()).map(|a| Change { before: Some(a), after: None }).collect();
        let n = changes.len();
        self.commit(&format!("Delete {n} annotations"), changes);
        self.multi.clear();
        self.selected = None;
        self.toast(format!("Deleted {n} · Ctrl+Z to undo"));
    }

    /// Set (or clear) one property on every multi-selected annotation.
    pub fn bulk_set(&mut self, key: &str, value: Option<Value>) -> usize {
        if self.read_only {
            return 0;
        }
        let mut changes = Vec::new();
        for u in &self.multi {
            if let Some(a) = self.get(u) {
                let mut b = a.clone();
                match &value {
                    Some(v) => b.props.insert(key.to_string(), v.clone()),
                    None => b.props.remove(key),
                };
                b.touch();
                changes.push(Change { before: Some(a.clone()), after: Some(b) });
            }
        }
        let n = changes.len();
        self.commit(&format!("Set {key} on {n}"), changes);
        n
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

    /// Bring the projection cache in line with `items` (cheap when nothing changed).
    fn refresh_proj(&mut self) {
        for a in &self.items {
            match self.proj.get(&a.uuid) {
                Some(p) if p.geom == a.geom => {}
                _ => {
                    self.proj.insert(a.uuid.clone(), Proj::new(&a.geom));
                }
            }
        }
        if self.proj.len() > self.items.len() + 256 {
            let live: std::collections::HashSet<&str> = self.items.iter().map(|a| a.uuid.as_str()).collect();
            self.proj.retain(|k, _| live.contains(k.as_str()));
        }
    }

    fn screen_pts_of(&self, cam: &Camera, rect: Rect, a: &Annotation) -> Vec<Pos2> {
        match self.proj.get(&a.uuid).filter(|p| p.geom == a.geom) {
            Some(p) => p.merc.iter().map(|m| cam.to_screen(rect, m[0], m[1])).collect(),
            None => Self::screen_pts(cam, rect, a),
        }
    }

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
        let (wx, wy) = cam.to_world(rect, p);
        let pad = 12.0 * cam.mpp;
        let probe = cat_cog::MercBox { min_x: wx, max_x: wx, min_y: wy, max_y: wy };
        for a in self.items.iter().rev() {
            if self.proj.get(&a.uuid).is_some_and(|pr| !pr.visible(&probe, pad)) {
                continue;
            }
            let pts: Vec<[f64; 2]> = self.screen_pts_of(cam, rect, a).iter().map(|q| [q.x as f64, q.y as f64]).collect();
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
        let (wx, wy) = cam.to_world(rect, p);
        let pad = SNAP_PX as f64 * cam.mpp;
        let probe = cat_cog::MercBox { min_x: wx, max_x: wx, min_y: wy, max_y: wy };
        for a in &self.items {
            if Some(a.uuid.as_str()) == exclude {
                continue;
            }
            let Some(pr) = self.proj.get(&a.uuid) else { continue };
            if !pr.visible(&probe, pad) {
                continue;
            }
            for &m in &pr.merc {
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
        let (pressed, origin, space, alt, shift) = ui.input(|i| {
            (i.pointer.primary_pressed(), i.pointer.press_origin(), i.key_down(egui::Key::Space), i.modifiers.alt, i.modifiers.shift)
        });
        if space {
            return true;
        }
        if pressed && self.drag.is_none() {
            if let Some(o) = origin.filter(|o| rect.contains(*o)) {
                match self.tool {
                    Tool::Rect => self.drag = Some(Drag::RectDraw { start: Self::world(cam, rect, o) }),
                    Tool::Select if shift => self.drag = Some(Drag::Lasso { pts: vec![o] }),
                    Tool::Line | Tool::Polygon if shift && !self.read_only => {
                        let new_shape = self.drawing.is_empty();
                        self.drag = Some(Drag::Trace { pts: vec![Self::world(cam, rect, o)], new_shape })
                    }
                    Tool::Select if !alt => self.start_select_drag(cam, rect, o),
                    _ => {}
                }
            }
        }
        self.drag.is_none()
    }

    fn start_select_drag(&mut self, cam: &Camera, rect: Rect, o: Pos2) {
        if self.read_only {
            return;
        }
        let Some(sel) = self.selected_ann().cloned() else { return };
        let pts = self.screen_pts_of(cam, rect, &sel);
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
        self.refresh_proj();
        let hover = resp.hover_pos();
        let keys_free = !ui.ctx().egui_wants_keyboard_input();
        let (alt, space) = ui.input(|i| (i.modifiers.alt, i.key_down(egui::Key::Space)));

        // Snap only while placing or dragging vertices.
        let placing = matches!(self.tool, Tool::Point | Tool::Line | Tool::Polygon | Tool::Rect | Tool::Measure);
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
            } else if (back || del) && self.drawing.is_empty() && self.multi.len() > 1 {
                self.delete_multi();
            } else if (back || del) && self.drawing.is_empty() && self.selected.is_some() {
                self.delete_selected();
            }
            if ui.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::A))) {
                self.multi = self.items.iter().map(|a| a.uuid.clone()).collect();
                self.selected = None;
                self.toast(format!("{} selected · set attributes in the panel, Del to delete", self.multi.len()));
            }
            if esc && !(self.tool == Tool::Measure && !self.measure.is_empty()) {
                if self.is_drawing() {
                    self.drawing.clear();
                    self.drag = None;
                } else if !self.multi.is_empty() {
                    self.multi.clear();
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
            Tool::Line | Tool::Polygon if matches!(self.drag, Some(Drag::Trace { .. })) => {
                if let Some(Drag::Trace { mut pts, new_shape }) = self.drag.take() {
                    if let Some(w) = cursor_world {
                        let last = *pts.last().unwrap();
                        if cam.to_screen(rect, last[0], last[1]).distance(cam.to_screen(rect, w[0], w[1])) >= 3.0 {
                            pts.push(w);
                        }
                    }
                    let down = ui.input(|i| i.pointer.primary_down());
                    if down {
                        self.drag = Some(Drag::Trace { pts, new_shape });
                    } else {
                        // Simplify the stroke to ~1 px, then add it to the shape.
                        let simplified = simplify(&pts, cam.mpp * 1.0);
                        self.drawing.extend(simplified);
                        if new_shape && self.drawing.len() >= if self.tool == Tool::Line { 2 } else { 3 } {
                            self.finish_drawing(active_secs, &mut ev);
                        }
                    }
                }
            }
            Tool::Line | Tool::Polygon => {
                if resp.secondary_clicked() && !self.drawing.is_empty() {
                    self.finish_drawing(active_secs, &mut ev);
                } else if double {
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
            Tool::Measure => {
                if double || (keys_free && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    if self.measure.len() >= 2 {
                        let n = self.measure.len();
                        let a = cam.to_screen(rect, self.measure[n - 1][0], self.measure[n - 1][1]);
                        let b = cam.to_screen(rect, self.measure[n - 2][0], self.measure[n - 2][1]);
                        if a.distance(b) < 4.0 {
                            self.measure.pop();
                        }
                    }
                    self.measure_done = true;
                } else if clicked {
                    if let Some(w) = cursor_world {
                        if self.measure_done {
                            self.measure.clear();
                            self.measure_done = false;
                        }
                        self.measure.push(w);
                    }
                }
                if keys_free && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.measure.clear();
                    self.measure_done = false;
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
                        let shift = ui.input(|i| i.modifiers.shift);
                        if alt {
                            self.alt_click_vertex(cam, rect, h);
                        } else if shift {
                            if let Some(u) = self.hit(cam, rect, h) {
                                if self.multi.is_empty() {
                                    if let Some(s) = self.selected.take() {
                                        self.multi.push(s);
                                    }
                                }
                                match self.multi.iter().position(|m| *m == u) {
                                    Some(i) => {
                                        self.multi.remove(i);
                                    }
                                    None => self.multi.push(u),
                                }
                                if self.multi.len() == 1 {
                                    self.selected = self.multi.pop();
                                }
                            }
                        } else {
                            self.multi.clear();
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
                if self.drag.is_none() {
                    ev.hovered = hover.and_then(|h| self.hit(cam, rect, h)).filter(|u| Some(u) != self.selected.as_ref());
                }
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
        if self.read_only {
            return;
        }
        let Some(sel) = self.selected_ann().cloned() else { return };
        let pts = self.screen_pts_of(cam, rect, &sel);
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
                self.hover_vertex_close = self.screen_pts_of(cam, rect, sel).iter().any(|p| p.distance(h) <= HANDLE + 2.0);
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
            Drag::RectDraw { .. } | Drag::Trace { .. } => {}
            Drag::Lasso { mut pts } => {
                if let Some(p) = pointer {
                    if pts.last().is_none_or(|l| l.distance(p) > 3.0) {
                        pts.push(p);
                    }
                }
                if released {
                    if pts.len() >= 3 {
                        let poly: Vec<[f64; 2]> = pts.iter().map(|p| [p.x as f64, p.y as f64]).collect();
                        let mut hits = Vec::new();
                        for a in &self.items {
                            let sp = self.screen_pts_of(cam, rect, a);
                            let c = match &a.geom {
                                Geom::Polygon(_) => {
                                    let r: Vec<[f64; 2]> = sp.iter().map(|p| [p.x as f64, p.y as f64]).collect();
                                    geom::centroid(&r)
                                }
                                _ => [sp[sp.len() / 2].x as f64, sp[sp.len() / 2].y as f64],
                            };
                            if geom::point_in_polygon(c, &poly) {
                                hits.push(a.uuid.clone());
                            }
                        }
                        for u in hits {
                            if !self.multi.contains(&u) {
                                self.multi.push(u);
                            }
                        }
                        if let Some(s) = self.selected.take() {
                            if !self.multi.contains(&s) {
                                self.multi.insert(0, s);
                            }
                        }
                        if self.multi.len() == 1 {
                            self.selected = self.multi.pop();
                        } else if !self.multi.is_empty() {
                            self.toast(format!("{} selected", self.multi.len()));
                        }
                    }
                } else {
                    self.drag = Some(Drag::Lasso { pts });
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------

    fn triangles(&mut self, a: &Annotation) -> Vec<u32> {
        let p = self.proj.entry(a.uuid.clone()).or_insert_with(|| Proj::new(&a.geom));
        if p.tris.is_none() {
            let flat: Vec<f64> = p.merc.iter().flat_map(|m| [m[0], m[1]]).collect();
            p.tris = Some(earcutr::earcut(&flat, &[], 2).unwrap_or_default().into_iter().map(|i| i as u32).collect());
        }
        p.tris.clone().unwrap_or_default()
    }

    pub fn color_of(&self, a: &Annotation) -> Color32 {
        match self.style.color_by {
            ColorBy::Status => {
                if a.is_complete() { rgb(self.style.complete) } else { rgb(self.style.incomplete) }
            }
            mode => mode.value(a).map(|v| color_for(&v)).unwrap_or(MISSING),
        }
    }

    /// Legend entries for the current colour mode: (value, colour, count).
    pub fn legend(&self) -> Vec<(Option<String>, Color32, usize)> {
        let mut counts: std::collections::BTreeMap<Option<String>, usize> = Default::default();
        for a in &self.items {
            *counts.entry(self.style.color_by.value(a)).or_default() += 1;
        }
        let mut v: Vec<(Option<String>, Color32, usize)> = counts
            .into_iter()
            .map(|(k, n)| {
                let c = match (&k, self.style.color_by) {
                    (Some(s), ColorBy::Status) => if s == "Complete" { rgb(self.style.complete) } else { rgb(self.style.incomplete) },
                    (Some(s), _) => color_for(s),
                    (None, _) => MISSING,
                };
                (k, c, n)
            })
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        v
    }

    /// Select every annotation with this legend value (for bulk edits).
    pub fn select_value(&mut self, value: &Option<String>) {
        let mode = self.style.color_by;
        self.multi = self.items.iter().filter(|a| &mode.value(a) == value).map(|a| a.uuid.clone()).collect();
        self.selected = None;
        if self.multi.len() == 1 {
            self.selected = self.multi.pop();
        }
    }

    fn paint(&mut self, painter: &egui::Painter, cam: &Camera, rect: Rect) {
        let view = cam.view_box(rect);
        let lw = self.style.line_width;
        let items = std::mem::take(&mut self.items);
        let mut labels = Vec::new();
        for a in &items {
            if self.proj.get(&a.uuid).is_some_and(|p| !p.visible(&view, 0.0)) {
                continue;
            }
            let selected = self.selected.as_deref() == Some(a.uuid.as_str()) || self.multi.contains(&a.uuid);
            let base = self.color_of(a);
            let color = if selected { Color32::from_rgb(0, 230, 255) } else { base };
            let pts = self.screen_pts_of(cam, rect, a);
            let stroke = Stroke::new(if selected { lw + 1.0 } else { lw }, color);
            // Zoomed out, a whole annotation can shrink to a few pixels: draw a dot.
            let bb = Rect::from_points(&pts);
            if !matches!(a.geom, Geom::Point(_)) && bb.width().max(bb.height()) < 4.0 {
                painter.circle_filled(bb.center(), 2.0, color);
                continue;
            }
            // Outline through vertices at least ~1 px apart: near-duplicate
            // vertices make razor-sharp joins whose mitres shoot off-screen.
            let outline = thin(&pts, 1.2);
            match &a.geom {
                Geom::Point(_) => {
                    painter.circle(pts[0], 5.5, color, Stroke::new(1.5, Color32::WHITE));
                }
                Geom::Line(_) => {
                    painter.add(Shape::line(outline, stroke));
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
                    if outline.len() >= 3 {
                        painter.add(Shape::closed_line(outline, stroke));
                    }
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
            if selected && self.multi.is_empty() {
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

    fn paint_measure(&self, painter: &egui::Painter, cam: &Camera, rect: Rect, cursor_world: Option<[f64; 2]>) {
        if self.tool != Tool::Measure || self.measure.is_empty() {
            return;
        }
        let amber = Color32::from_rgb(255, 190, 40);
        let mut pts = self.measure.clone();
        if !self.measure_done {
            if let Some(c) = cursor_world {
                pts.push(c);
            }
        }
        let sp: Vec<Pos2> = pts.iter().map(|m| cam.to_screen(rect, m[0], m[1])).collect();
        let ll: Vec<LonLat> = pts.iter().map(|m| merc_to_ll(*m)).collect();
        if ll.len() >= 3 {
            let mut mesh = egui::Mesh::default();
            let flat: Vec<f64> = sp.iter().flat_map(|p| [p.x as f64, p.y as f64]).collect();
            for p in &sp {
                mesh.colored_vertex(*p, amber.gamma_multiply(0.12));
            }
            mesh.indices = earcutr::earcut(&flat, &[], 2).unwrap_or_default().into_iter().map(|i| i as u32).collect();
            painter.add(Shape::mesh(mesh));
            painter.add(Shape::dashed_line(&[*sp.last().unwrap(), sp[0]], Stroke::new(1.0, amber.gamma_multiply(0.7)), 5.0, 4.0));
        }
        painter.add(Shape::line(sp.clone(), Stroke::new(2.5, amber)));
        for w in sp.windows(2).zip(ll.windows(2)) {
            let d = geom::length_m(w.1, false);
            let mid = w.0[0].lerp(w.0[1], 0.5);
            if w.0[0].distance(w.0[1]) > 40.0 {
                measure_label(painter, mid + vec2(4.0, 4.0), &geom::fmt_length(d));
            }
        }
        for p in &sp {
            painter.circle(*p, 4.0, Color32::WHITE, Stroke::new(1.5, amber));
        }
        let total = geom::length_m(&ll, false);
        let mut text = format!("Total {}", geom::fmt_length(total));
        if ll.len() >= 3 {
            text.push_str(&format!(" · area {}", geom::fmt_area(geom::area_m2(&ll))));
        }
        if let Some(last) = sp.last() {
            measure_label(painter, *last + vec2(14.0, -26.0), &text);
        }
    }

    fn paint_drawing(&self, painter: &egui::Painter, cam: &Camera, rect: Rect, hover: Option<Pos2>, cursor_world: Option<[f64; 2]>) {
        let accent = Color32::from_rgb(0, 230, 255);
        let s = |m: &[f64; 2]| cam.to_screen(rect, m[0], m[1]);
        self.paint_measure(painter, cam, rect, cursor_world);
        if let Some(Drag::Trace { pts, .. }) = &self.drag {
            let mut all: Vec<Pos2> = self.drawing.iter().map(s).collect();
            all.extend(pts.iter().map(s));
            painter.add(Shape::line(all, Stroke::new(2.0, accent)));
            return;
        }
        if let Some(Drag::Lasso { pts }) = &self.drag {
            if pts.len() >= 2 {
                let mut closed = pts.clone();
                closed.push(pts[0]);
                painter.add(Shape::dashed_line(&closed, Stroke::new(1.5, accent), 6.0, 4.0));
            }
            return;
        }
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

/// Douglas–Peucker simplification with tolerance `tol` (same units as pts).
fn simplify(pts: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        let mut best = (0.0, 0);
        for i in a + 1..b {
            let d = geom::point_segment(pts[i], pts[a], pts[b]).0;
            if d > best.0 {
                best = (d, i);
            }
        }
        if best.0 > tol {
            keep[best.1] = true;
            stack.push((a, best.1));
            stack.push((best.1, b));
        }
    }
    pts.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// Drop points closer than `min_px` to the previously kept one.
fn thin(pts: &[Pos2], min_px: f32) -> Vec<Pos2> {
    let mut out: Vec<Pos2> = Vec::with_capacity(pts.len());
    for p in pts {
        if out.last().is_none_or(|l| l.distance(*p) >= min_px) {
            out.push(*p);
        }
    }
    out
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
