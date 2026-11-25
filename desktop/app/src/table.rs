//! Annotation table: sortable, filterable, click a row to select and zoom.

use cat_core::species::SpeciesList;
use cat_core::Annotation;
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::editor::{size_text, size_value, Editor};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Num,
    Species,
    Morph,
    Kind,
    Size,
    Done,
}

pub struct TableState {
    sort: Col,
    asc: bool,
    pub filter: String,
    pub incomplete_only: bool,
    last_selected: Option<String>,
}

impl Default for TableState {
    fn default() -> Self {
        Self { sort: Col::Num, asc: true, filter: String::new(), incomplete_only: false, last_selected: None }
    }
}

/// Returns the uuid of a row the user clicked (to zoom the map to it).
pub fn table_ui(ui: &mut egui::Ui, editor: &mut Editor, st: &mut TableState, species: &SpeciesList) -> Option<String> {
    let total = editor.items.len();
    let done = editor.items.iter().filter(|a| a.is_complete()).count();
    let mut clicked = None;

    ui.horizontal(|ui| {
        ui.strong(format!("Annotations  {total}"));
        ui.label(RichText::new(format!(" {done} complete ")).color(Color32::BLACK).background_color(Color32::from_rgb(120, 220, 140)));
        if total > done {
            ui.label(RichText::new(format!(" {} need species ", total - done)).color(Color32::BLACK).background_color(Color32::from_rgb(255, 208, 0)));
        }
        ui.separator();
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("🔍 filter species / morph").desired_width(160.0));
        ui.checkbox(&mut st.incomplete_only, "Incomplete only");
    });

    let f = st.filter.trim().to_uppercase();
    let mut rows: Vec<(usize, &Annotation)> = editor
        .items
        .iter()
        .enumerate()
        .filter(|(_, a)| !st.incomplete_only || !a.is_complete())
        .filter(|(_, a)| f.is_empty() || a.spcode().contains(&f) || a.text("morph_code").contains(&f))
        .collect();
    let key = |a: &Annotation| match st.sort {
        Col::Species => a.spcode(),
        Col::Morph => a.text("morph_code"),
        Col::Kind => a.kind.as_str().to_string(),
        Col::Done => (a.is_complete() as u8).to_string(),
        _ => String::new(),
    };
    match st.sort {
        Col::Num => {}
        Col::Size => rows.sort_by(|a, b| size_value(a.1).total_cmp(&size_value(b.1))),
        _ => rows.sort_by_key(|r| key(r.1)),
    }
    if !st.asc {
        rows.reverse();
    }

    // Keep the selected row in view when the selection changes on the map.
    let scroll_to = if st.last_selected != editor.selected {
        st.last_selected = editor.selected.clone();
        editor.selected.as_ref().and_then(|u| rows.iter().position(|r| &r.1.uuid == u))
    } else {
        None
    };

    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(44.0))
        .column(Column::initial(70.0))
        .column(Column::initial(200.0).clip(true))
        .column(Column::initial(60.0))
        .column(Column::initial(80.0))
        .column(Column::initial(90.0))
        .column(Column::remainder())
        .min_scrolled_height(0.0);
    if let Some(i) = scroll_to {
        table = table.scroll_to_row(i, Some(egui::Align::Center));
    }
    let mut header_click = None;
    table
        .header(20.0, |mut h| {
            for (col, name) in [
                (Some(Col::Num), "#"),
                (Some(Col::Species), "Species"),
                (None, "Name"),
                (Some(Col::Morph), "Morph"),
                (Some(Col::Kind), "Type"),
                (Some(Col::Size), "Size"),
                (Some(Col::Done), "✔"),
            ] {
                h.col(|ui| {
                    let arrow = if col == Some(st.sort) { if st.asc { " ⏶" } else { " ⏷" } } else { "" };
                    let r = ui.add(egui::Label::new(RichText::new(format!("{name}{arrow}")).strong()).sense(egui::Sense::click()));
                    if let (Some(c), true) = (col, r.clicked()) {
                        header_click = Some(c);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(20.0, rows.len(), |mut row| {
                let (num, a) = rows[row.index()];
                row.set_selected(editor.selected.as_deref() == Some(a.uuid.as_str()) || editor.multi.contains(&a.uuid));
                row.col(|ui| {
                    ui.label(format!("{}", num + 1));
                });
                row.col(|ui| {
                    let code = a.spcode();
                    if code.is_empty() {
                        ui.weak("—");
                    } else {
                        ui.monospace(code);
                    }
                });
                row.col(|ui| {
                    ui.label(RichText::new(species.get(&a.spcode()).map(|s| s.name.as_str()).unwrap_or("")).italics());
                });
                row.col(|ui| {
                    ui.label(a.text("morph_code"));
                });
                row.col(|ui| {
                    ui.label(a.kind.label());
                });
                row.col(|ui| {
                    ui.monospace(size_text(a));
                });
                row.col(|ui| {
                    if a.is_complete() {
                        ui.colored_label(Color32::from_rgb(90, 200, 110), "✔");
                    } else {
                        ui.colored_label(Color32::from_rgb(230, 180, 0), "●");
                    }
                });
                if row.response().clicked() {
                    clicked = Some(a.uuid.clone());
                }
            });
        });
    if let Some(c) = header_click {
        if st.sort == c {
            st.asc = !st.asc;
        } else {
            st.sort = c;
            st.asc = true;
        }
    }
    let (add, up, down, over) = ui.input(|i| {
        (i.modifiers.command || i.modifiers.shift, i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown), i.pointer.hover_pos())
    });
    // ↑/↓ step through rows while the pointer is over the table.
    let hovered = over.is_some_and(|p| ui.max_rect().contains(p));
    if hovered && (up || down) && clicked.is_none() && !rows.is_empty() {
        let cur = editor.selected.as_ref().and_then(|u| rows.iter().position(|r| &r.1.uuid == u));
        let next = match cur {
            Some(i) if down => (i + 1).min(rows.len() - 1),
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        clicked = Some(rows[next].1.uuid.clone());
    }
    if let Some(u) = &clicked {
        if add {
            if editor.multi.is_empty() {
                if let Some(s) = editor.selected.take() {
                    editor.multi.push(s);
                }
            }
            match editor.multi.iter().position(|m| m == u) {
                Some(i) => {
                    editor.multi.remove(i);
                }
                None => editor.multi.push(u.clone()),
            }
            if editor.multi.len() == 1 {
                editor.selected = editor.multi.pop();
            }
            return None;
        }
        editor.multi.clear();
        editor.selected = Some(u.clone());
        st.last_selected = Some(u.clone());
    }
    clicked
}
