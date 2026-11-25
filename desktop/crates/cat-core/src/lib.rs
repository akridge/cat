//! `cat-core` — the annotation model shared by the CAT desktop app.
//!
//! Annotations are stored exactly like the CAT server stores them: a GeoJSON
//! geometry in WGS84 lon/lat plus a flat `properties` object whose keys match
//! the web annotation form (`spcode`, `morph_code`, `obs_year`, …). Unknown
//! properties from the server are preserved untouched.

pub mod annotation;
pub mod fields;
pub mod geom;
pub mod history;
pub mod species;
pub mod store;

pub use annotation::{AnnKind, Annotation, Geom};
pub use history::{Change, History};
