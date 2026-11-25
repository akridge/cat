//! The attribute form for the selected annotation.
//!
//! Keyboard-first: after drawing, focus lands in Species; type a few letters,
//! ↑/↓ to choose, Enter to pick, Enter again to finish and draw the next one.
//! Edits apply live (labels and table update as you type) and become one
//! undo step when you leave the annotation.

use std::collections::HashMap;

use cat_core::fields::{self, Field, FieldKind, Section};
use cat_core::species::SpeciesList;
use cat_core::{Annotation, Change};
use egui::{Color32, RichText};
use serde_json::{Map, Value};

use crate::editor::{size_text, Editor};
use crate::settings::Settings;

#[derive(PartialEq, Eq)]
pub enum FormAction {
    None,
    /// Finished with this annotation (Enter / Done).
    Done,
}

/// Properties never copied by "Same as last".
const NOT_COPIED: &[&str] = &["type", "created_at", "updated_at", "annotation_time_seconds", "client_uuid", "id"];

pub struct Form {
    pub fields: Vec<Field>,
    pub species: SpeciesList,
    uuid: Option<String>,
    before: Option<Annotation>,
    texts: HashMap<&'static str, String>,
    errors: HashMap<&'static str, String>,
    highlight: usize,
    show_suggestions: bool,
    focus_species: bool,
    /// Attributes of the last annotation edited, for "Same as last".
    pub last_props: Option<Map<String, Value>>,
    bulk_field: usize,
    bulk_text: String,
    bulk_species: String,
}

impl Form {
    pub fn new() -> Self {
        Self {
            fields: fields::default_fields(),
            species: SpeciesList::bundled(),
            uuid: None,
            before: None,
            texts: HashMap::new(),
            errors: HashMap::new(),
            highlight: 0,
            show_suggestions: false,
            focus_species: false,
            last_props: None,
            bulk_field: 0,
            bulk_text: String::new(),
            bulk_species: String::new(),
        }
    }

    pub fn focus_species(&mut self) {
        self.focus_species = true;
    }

    /// Turn live edits of the annotation being edited into one undo step.
    /// Only attributes are compared and restored: geometry edits made while
    /// the annotation was selected have their own undo steps.
    pub fn commit(&mut self, editor: &mut Editor) {
        let (Some(uuid), Some(before)) = (self.uuid.clone(), self.before.take()) else { return };
        if let Some(a) = editor.get_mut(&uuid) {
            if !same_attributes(&a.props, &before.props) {
                a.touch();
                let after = a.clone();
                let mut before_ann = after.clone();
                before_ann.props = before.props.clone();
                let mut p = after.props.clone();
                for k in NOT_COPIED {
                    p.remove(*k);
                }
                self.last_props = Some(p);
                editor.record("Edit attributes", vec![Change { before: Some(before_ann), after: Some(after) }]);
            }
            self.before = editor.get(&uuid).cloned();
        }
    }

    /// Follow the editor's selection, committing the previous annotation.
    pub fn sync(&mut self, editor: &mut Editor) {
        if self.uuid == editor.selected && self.uuid.as_deref().is_none_or(|u| editor.get(u).is_some()) {
            // Undo/redo may have changed the annotation under us: reload texts.
            if let (Some(u), Some(b)) = (&self.uuid, &self.before) {
                if let Some(a) = editor.get(u) {
                    if !same_attributes(&a.props, &b.props) && !self.editing_live(a) {
                        self.load(a.clone());
                    }
                }
            }
            return;
        }
        self.commit(editor);
        self.uuid = editor.selected.clone();
        self.errors.clear();
        self.show_suggestions = false;
        match editor.selected_ann().cloned() {
            Some(a) => self.load(a),
            None => {
                self.before = None;
                self.texts.clear();
            }
        }
    }

    /// True if the annotation's props are exactly what our texts produce
    /// (i.e. the difference from `before` is our own live editing).
    fn editing_live(&self, a: &Annotation) -> bool {
        self.fields.iter().all(|f| {
            let t = self.texts.get(f.key).cloned().unwrap_or_default();
            match fields::parse_field(f, &t) {
                Ok(v) => a.props.get(f.key).filter(|x| !x.is_null()) == v.as_ref(),
                Err(_) => true,
            }
        })
    }

    fn load(&mut self, a: Annotation) {
        self.texts = self.fields.iter().map(|f| (f.key, a.text(f.key))).collect();
        self.before = Some(a);
    }

    fn set(&mut self, editor: &mut Editor, settings: &mut Settings, field: &Field, text: String) {
        let Some(uuid) = self.uuid.clone() else { return };
        match fields::parse_field(field, &text) {
            Ok(v) => {
                self.errors.remove(field.key);
                if let Some(a) = editor.get_mut(&uuid) {
                    match &v {
                        Some(v) => a.props.insert(field.key.into(), v.clone()),
                        None => a.props.remove(field.key),
                    };
                }
                if field.sticky {
                    match v {
                        Some(v) => settings.sticky.insert(field.key.into(), v),
                        None => settings.sticky.remove(field.key),
                    };
                    editor.new_props = settings.sticky.clone();
                }
                editor.rev += 1;
            }
            Err(e) => {
                self.errors.insert(field.key, e);
            }
        }
        self.texts.insert(field.key, text);
    }

    /// Copy the previous annotation's attributes (except survey timing/ids).
    pub fn same_as_last(&mut self, editor: &mut Editor) {
        let (Some(uuid), Some(last)) = (self.uuid.clone(), self.last_props.clone()) else { return };
        if let Some(a) = editor.get_mut(&uuid) {
            for (k, v) in last {
                a.props.insert(k, v);
            }
            let a = a.clone();
            self.texts = self.fields.iter().map(|f| (f.key, a.text(f.key))).collect();
            editor.rev += 1;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, editor: &mut Editor, settings: &mut Settings) -> FormAction {
        self.sync(editor);
        if editor.multi.len() > 1 {
            self.bulk_ui(ui, editor, settings);
            return FormAction::None;
        }
        let Some(a) = editor.selected_ann().cloned() else {
            ui.add_space(8.0);
            ui.weak("Select or draw an annotation to edit its attributes.");
            ui.add_space(8.0);
            ui.label(RichText::new(editor.tool.help()).italics());
            return FormAction::None;
        };
        let mut action = FormAction::None;
        if editor.read_only {
            ui.colored_label(Color32::from_rgb(230, 180, 0), "👁 View only");
            ui.disable();
        }
        if let Some(by) = a.get("created_by").and_then(|v| v.as_str()) {
            ui.weak(format!("by {by}"));
        }

        ui.horizontal(|ui| {
            ui.heading(a.kind.label());
            ui.label(RichText::new(size_text(&a)).monospace());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if a.is_complete() {
                    ui.label(RichText::new(" ✔ complete ").color(Color32::BLACK).background_color(Color32::from_rgb(120, 220, 140)));
                } else {
                    ui.label(RichText::new(" needs species ").color(Color32::BLACK).background_color(Color32::from_rgb(255, 208, 0)));
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(self.last_props.is_some(), egui::Button::new("⎘ Same as last")).on_hover_text("Copy attributes from the previous annotation (Ctrl+D)").clicked() {
                self.same_as_last(editor);
            }
            if ui.button("🗑 Delete").on_hover_text("Delete (Del)").clicked() {
                editor.delete_selected();
            }
            if ui.button("✔ Done").on_hover_text("Finish and continue (Enter)").clicked() {
                action = FormAction::Done;
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.species_ui(ui, editor, settings) {
                action = FormAction::Done;
            }
            ui.add_space(6.0);
            self.section(ui, editor, settings, Section::Colony, "Colony", true);
            let survey_summary = ["site", "obs_year", "transect", "segment"]
                .iter()
                .map(|k| a.text(k))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            self.section(ui, editor, settings, Section::Survey, &format!("Survey   {survey_summary}"), false);
            self.section(ui, editor, settings, Section::Condition, "Condition", false);
            ui.add_space(8.0);
            ui.weak(format!("id {}", &a.uuid[..8]));
            if let Some(t) = a.get("annotation_time_seconds").and_then(|v| v.as_f64()) {
                ui.weak(format!("annotation time {t:.0} s"));
            }
        });

        action
    }

    /// Species field with ranked autocomplete. Returns true on "Enter to finish".
    fn species_ui(&mut self, ui: &mut egui::Ui, editor: &mut Editor, settings: &mut Settings) -> bool {
        let field = self.fields.iter().find(|f| f.key == "spcode").cloned().expect("spcode field");
        let id = egui::Id::unique("species-input");
        let mut done = false;
        let has_focus = ui.memory(|m| m.has_focus(id));
        let mut text = self.texts.get("spcode").cloned().unwrap_or_default();
        let matches: Vec<(String, String)> = self
            .species
            .search(&text, &settings.recent_species, 8)
            .into_iter()
            .map(|s| (s.code.clone(), s.name.clone()))
            .collect();
        if has_focus && self.show_suggestions && !matches.is_empty() {
            ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    self.highlight = (self.highlight + 1).min(matches.len() - 1);
                }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    self.highlight = self.highlight.saturating_sub(1);
                }
            });
        }
        self.highlight = self.highlight.min(matches.len().saturating_sub(1));

        ui.label(RichText::new("Species").strong());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut text)
                .id(id)
                .hint_text("type code or name, e.g. plob / porites")
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Heading),
        );
        if self.focus_species {
            resp.request_focus();
            self.focus_species = false;
            self.show_suggestions = true;
        }
        if resp.changed() {
            // Typing only searches; the annotation changes when a species is
            // picked, Enter is pressed or the field is left.
            self.highlight = 0;
            self.show_suggestions = true;
            self.texts.insert("spcode", text.clone());
        }
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter {
            if self.show_suggestions && !matches.is_empty() {
                // First Enter picks the highlighted suggestion…
                let code = matches[self.highlight].0.clone();
                self.set(editor, settings, &field, code.clone());
                settings.use_species(&code);
                self.show_suggestions = false;
                resp.request_focus();
            } else {
                // …the next one finishes the annotation.
                self.set(editor, settings, &field, text.clone());
                if self.species.get(&text).is_some() {
                    settings.use_species(&text);
                }
                done = true;
            }
        } else if resp.lost_focus() {
            // Left the field (Tab / click elsewhere): keep what was typed.
            self.set(editor, settings, &field, text.clone());
            self.show_suggestions = false;
        }
        let current = self.texts.get("spcode").cloned().unwrap_or_default();
        match self.species.get(&current) {
            Some(s) => {
                ui.label(RichText::new(&s.name).italics().color(ui.visuals().weak_text_color()));
            }
            None if !current.trim().is_empty() && !resp.has_focus() => {
                ui.colored_label(Color32::from_rgb(230, 150, 40), "⚠ not in the species list");
            }
            None => {}
        }
        if (has_focus || resp.has_focus()) && self.show_suggestions && !matches.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                for (i, (code, name)) in matches.iter().enumerate() {
                    let sel = i == self.highlight;
                    let r = ui.add(egui::Button::selectable(sel, RichText::new(format!("{code:<6} {name}")).monospace()).min_size(egui::vec2(ui.available_width(), 0.0)));
                    if r.clicked() {
                        self.set(editor, settings, &field, code.clone());
                        settings.use_species(code);
                        self.show_suggestions = false;
                    }
                }
                ui.weak("↑↓ choose · Enter pick · Enter again to finish");
            });
        }
        // Quick picks: recently used species.
        if !settings.recent_species.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Recent:");
                for code in settings.recent_species.clone().iter().take(8) {
                    if ui.small_button(code).on_hover_text(self.species.get(code).map(|s| s.name.as_str()).unwrap_or("")).clicked() {
                        self.set(editor, settings, &field, code.clone());
                        settings.use_species(code);
                    }
                }
            });
        }
        done
    }

    fn section(&mut self, ui: &mut egui::Ui, editor: &mut Editor, settings: &mut Settings, section: Section, title: &str, open: bool) {
        let fields: Vec<Field> = self.fields.iter().filter(|f| f.section == section && f.kind != FieldKind::Species).cloned().collect();
        egui::CollapsingHeader::new(RichText::new(title).strong()).id_salt(format!("{section:?}")).default_open(open).show(ui, |ui| {
            egui::Grid::new(format!("grid-{section:?}")).num_columns(2).spacing([10.0, 6.0]).striped(false).show(ui, |ui| {
                for f in &fields {
                    let label = ui.label(f.label);
                    if f.sticky {
                        label.on_hover_text("Remembered for the next annotation");
                    }
                    self.field_ui(ui, editor, settings, f);
                    ui.end_row();
                }
            });
        });
    }

    fn field_ui(&mut self, ui: &mut egui::Ui, editor: &mut Editor, settings: &mut Settings, f: &Field) {
        let mut text = self.texts.get(f.key).cloned().unwrap_or_default();
        let err = self.errors.get(f.key).cloned();
        ui.horizontal(|ui| {
            match f.kind {
                FieldKind::YesNo => {
                    let yes = text.trim() == "-1" || text.trim() == "1";
                    let no = text.trim() == "0";
                    if ui.selectable_label(no, "No").clicked() {
                        self.set(editor, settings, f, if no { String::new() } else { "0".into() });
                    }
                    if ui.selectable_label(yes, "Yes").clicked() {
                        self.set(editor, settings, f, if yes { String::new() } else { "-1".into() });
                    }
                }
                FieldKind::Choice { free } => {
                    let shown = f.options.iter().find(|(v, _)| *v == text).map(|(_, l)| l.clone()).unwrap_or_else(|| text.clone());
                    if free {
                        let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(90.0));
                        if r.changed() {
                            self.set(editor, settings, f, text.clone());
                        }
                    }
                    egui::ComboBox::from_id_salt(f.key)
                        .selected_text(if free { String::new() } else if shown.is_empty() { "—".into() } else { shown })
                        .width(if free { 24.0 } else { 150.0 })
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(text.is_empty(), "— (blank)").clicked() {
                                self.set(editor, settings, f, String::new());
                            }
                            for (v, l) in &f.options {
                                if ui.selectable_label(&text == v, l).clicked() {
                                    self.set(editor, settings, f, v.clone());
                                }
                            }
                        });
                }
                _ => {
                    let w = if matches!(f.kind, FieldKind::Int | FieldKind::Float) { 80.0 } else { 150.0 };
                    let mut te = egui::TextEdit::singleline(&mut text).desired_width(w);
                    if err.is_some() {
                        te = te.text_color(Color32::from_rgb(230, 80, 60));
                    }
                    let r = ui.add(te);
                    if r.changed() {
                        self.set(editor, settings, f, text.clone());
                    }
                    if !f.hint.is_empty() {
                        ui.weak(f.hint);
                    }
                }
            }
            if let Some(e) = err {
                ui.colored_label(Color32::from_rgb(230, 80, 60), format!("needs a {e}"));
            }
        });
    }
}

/// Attribute equality ignoring the automatic `updated_at` timestamp.
fn same_attributes(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let strip = |m: &Map<String, Value>| {
        let mut m = m.clone();
        m.remove("updated_at");
        m
    };
    strip(a) == strip(b)
}

impl Form {
    /// Apply server-configured option lists (team-lead settings).
    pub fn apply_options(&mut self, lists: &[(String, Vec<(String, String)>)]) {
        for (name, opts) in lists {
            let members: Vec<&str> = match name.as_str() {
                "rdcause" => vec!["rdcause1", "rdcause2", "rdcause3"],
                "con" => vec!["con_1", "con_2", "con_3"],
                "sev" => vec!["sev_1", "sev_2", "sev_3"],
                other => vec![other],
            };
            for f in self.fields.iter_mut().filter(|f| members.contains(&f.key)) {
                if opts.is_empty() {
                    continue;
                }
                f.options = opts.clone();
                if f.kind == FieldKind::Text {
                    f.kind = FieldKind::Choice { free: true };
                }
            }
        }
    }
}

impl Form {
    /// Bulk attribute editing for a multi-selection (one undo step per apply).
    fn bulk_ui(&mut self, ui: &mut egui::Ui, editor: &mut Editor, settings: &mut Settings) {
        let n = editor.multi.len();
        let complete = editor.multi.iter().filter_map(|u| editor.get(u)).filter(|a| a.is_complete()).count();
        ui.heading(format!("{n} annotations selected"));
        ui.label(format!("{complete} complete · {} need species", n - complete));
        if editor.read_only {
            ui.colored_label(Color32::from_rgb(230, 180, 0), "👁 View only");
            ui.disable();
        }
        ui.horizontal(|ui| {
            if ui.button(format!("🗑 Delete {n}")).clicked() {
                editor.delete_multi();
            }
            if ui.button("Clear selection (Esc)").clicked() {
                editor.multi.clear();
            }
        });
        ui.separator();

        ui.label(RichText::new("Set species on all").strong());
        ui.add(egui::TextEdit::singleline(&mut self.bulk_species).hint_text("type code or name").desired_width(f32::INFINITY));
        let matches: Vec<(String, String)> =
            self.species.search(&self.bulk_species, &settings.recent_species, 6).into_iter().map(|s| (s.code.clone(), s.name.clone())).collect();
        for (code, name) in matches {
            if ui.add(egui::Button::new(RichText::new(format!("{code:<6} {name}")).monospace()).min_size(egui::vec2(ui.available_width(), 0.0))).clicked() {
                let k = editor.bulk_set("spcode", Some(Value::String(code.clone())));
                settings.use_species(&code);
                editor.toast = Some((format!("Species {code} set on {k}"), std::time::Instant::now()));
                self.bulk_species.clear();
            }
        }
        ui.separator();

        ui.label(RichText::new("Set a field on all").strong());
        let fields: Vec<Field> = self.fields.iter().filter(|f| f.kind != FieldKind::Species).cloned().collect();
        self.bulk_field = self.bulk_field.min(fields.len() - 1);
        egui::ComboBox::from_id_salt("bulk-field").selected_text(fields[self.bulk_field].label).width(200.0).show_ui(ui, |ui| {
            for (i, f) in fields.iter().enumerate() {
                if ui.selectable_label(i == self.bulk_field, f.label).clicked() {
                    self.bulk_field = i;
                    self.bulk_text.clear();
                }
            }
        });
        let f = fields[self.bulk_field].clone();
        let mut apply: Option<String> = None;
        ui.horizontal_wrapped(|ui| match f.kind {
            FieldKind::YesNo => {
                if ui.button("No").clicked() {
                    apply = Some("0".into());
                }
                if ui.button("Yes").clicked() {
                    apply = Some("-1".into());
                }
            }
            FieldKind::Choice { free: false } => {
                for (v, l) in &f.options {
                    if ui.button(l).clicked() {
                        apply = Some(v.clone());
                    }
                }
            }
            _ => {
                ui.add(egui::TextEdit::singleline(&mut self.bulk_text).desired_width(140.0));
                if !f.options.is_empty() {
                    egui::ComboBox::from_id_salt("bulk-opts").selected_text("").width(24.0).show_ui(ui, |ui| {
                        for (v, l) in &f.options {
                            if ui.selectable_label(false, l).clicked() {
                                self.bulk_text = v.clone();
                            }
                        }
                    });
                }
                if ui.button("Apply").clicked() {
                    apply = Some(self.bulk_text.clone());
                }
            }
        });
        if ui.small_button("Clear this field on all").clicked() {
            apply = Some(String::new());
        }
        if let Some(text) = apply {
            match fields::parse_field(&f, &text) {
                Ok(v) => {
                    let k = editor.bulk_set(f.key, v);
                    editor.toast = Some((format!("{} set on {k}", f.label), std::time::Instant::now()));
                }
                Err(e) => editor.toast = Some((format!("{} needs a {e}", f.label), std::time::Instant::now())),
            }
        }
        ui.separator();
        ui.weak("Shift+click adds/removes · Shift+drag lassoes · Ctrl+A selects all · Ctrl+Z undoes a bulk change");
    }
}
