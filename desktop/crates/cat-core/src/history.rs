//! Undo/redo as a stack of before/after snapshots keyed by annotation uuid.
//! Snapshots (rather than operations) make every edit undoable the same way:
//! draw, delete, vertex drag, attribute change, bulk fill.

use crate::Annotation;

#[derive(Clone, Debug)]
pub struct Change {
    pub before: Option<Annotation>,
    pub after: Option<Annotation>,
}

impl Change {
    pub fn uuid(&self) -> &str {
        self.before.as_ref().or(self.after.as_ref()).map(|a| a.uuid.as_str()).unwrap_or("")
    }
}

#[derive(Clone, Debug)]
pub struct Step {
    pub label: String,
    pub changes: Vec<Change>,
}

#[derive(Default)]
pub struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
}

const MAX_STEPS: usize = 500;

/// Set annotation `uuid` to `value` (None = delete), inserting if new.
pub fn apply(items: &mut Vec<Annotation>, uuid: &str, value: Option<&Annotation>) {
    let pos = items.iter().position(|a| a.uuid == uuid);
    match (pos, value) {
        (Some(i), Some(v)) => items[i] = v.clone(),
        (Some(i), None) => {
            items.remove(i);
        }
        (None, Some(v)) => items.push(v.clone()),
        (None, None) => {}
    }
}

impl History {
    /// Apply changes to `items` and record them as one undoable step.
    pub fn commit(&mut self, items: &mut Vec<Annotation>, label: impl Into<String>, changes: Vec<Change>) {
        let changes: Vec<Change> = changes.into_iter().filter(|c| c.before != c.after).collect();
        if changes.is_empty() {
            return;
        }
        for c in &changes {
            apply(items, c.uuid(), c.after.as_ref());
        }
        self.undo.push(Step { label: label.into(), changes });
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Record changes that were already applied (e.g. a vertex drag).
    pub fn record(&mut self, label: impl Into<String>, changes: Vec<Change>) {
        let changes: Vec<Change> = changes.into_iter().filter(|c| c.before != c.after).collect();
        if !changes.is_empty() {
            self.undo.push(Step { label: label.into(), changes });
            self.redo.clear();
        }
    }

    pub fn undo(&mut self, items: &mut Vec<Annotation>) -> Option<String> {
        let step = self.undo.pop()?;
        for c in step.changes.iter().rev() {
            apply(items, c.uuid(), c.before.as_ref());
        }
        let label = step.label.clone();
        self.redo.push(step);
        Some(label)
    }

    pub fn redo(&mut self, items: &mut Vec<Annotation>) -> Option<String> {
        let step = self.redo.pop()?;
        for c in &step.changes {
            apply(items, c.uuid(), c.after.as_ref());
        }
        let label = step.label.clone();
        self.undo.push(step);
        Some(label)
    }

    pub fn can_undo(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }
    pub fn can_redo(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnnKind, Geom};

    #[test]
    fn undo_redo_roundtrip() {
        let mut items = Vec::new();
        let mut h = History::default();
        let a = Annotation::new(AnnKind::Point, Geom::Point([1.0, 2.0]), Default::default());
        h.commit(&mut items, "Add point", vec![Change { before: None, after: Some(a.clone()) }]);
        let mut b = a.clone();
        b.geom = Geom::Point([3.0, 4.0]);
        h.commit(&mut items, "Move", vec![Change { before: Some(a.clone()), after: Some(b.clone()) }]);
        assert_eq!(items, vec![b.clone()]);
        assert_eq!(h.undo(&mut items).as_deref(), Some("Move"));
        assert_eq!(items, vec![a.clone()]);
        assert_eq!(h.undo(&mut items).as_deref(), Some("Add point"));
        assert!(items.is_empty());
        h.redo(&mut items);
        h.redo(&mut items);
        assert_eq!(items, vec![b]);
    }
}
