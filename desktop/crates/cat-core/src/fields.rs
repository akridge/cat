//! The annotation form, as data.
//!
//! Keys and value types mirror `buildAnnotationData()` in
//! `cat/web/js/annotation-form.js`, so desktop and web annotations are
//! interchangeable. Option lists are the server's seeded defaults
//! (`cat/db/schema.py`, `cat_annotation_field_options`); once connected to a
//! server, its configured lists replace these.

use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Int,
    Float,
    /// 0 = No, -1 = Yes (the CAT/Oracle convention).
    YesNo,
    /// Pick from options; `free` allows typing other values.
    Choice { free: bool },
    Species,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Survey,
    Colony,
    Condition,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub section: Section,
    /// (value, label)
    pub options: Vec<(String, String)>,
    /// Remembered between annotations ("sticky"), like the web app.
    pub sticky: bool,
    pub hint: &'static str,
}

fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect()
}

fn f(key: &'static str, label: &'static str, kind: FieldKind, section: Section) -> Field {
    Field { key, label, kind, section, options: Vec::new(), sticky: false, hint: "" }
}

pub fn default_fields() -> Vec<Field> {
    use FieldKind::*;
    use Section::*;
    let morph = opts(&[
        ("BR", "BR - Branching"),
        ("CO", "CO - Columnar"),
        ("EN", "EN - Encrusting"),
        ("FO", "FO - Foliaceous"),
        ("FL", "FL - Free-living"),
        ("LA", "LA - Laminar"),
        ("MD", "MD - Mounding"),
        ("MA", "MA - Massive"),
        ("PL", "PL - Plating"),
        ("SM", "SM - Submassive"),
        ("SO", "SO - Solitary"),
        ("TB", "TB - Tabular"),
    ]);
    let juv_sub: Vec<(String, String)> = ["CCAH", "CCAR", "TURFH", "TURFR", "EMA", "PESP", "LOBO", "HARD", "CORAL", "RUB", "HALI"]
        .iter()
        .map(|s| (s.to_string(), s.to_string()))
        .collect();
    let sev = opts(&[("1", "1"), ("2", "2"), ("3", "3"), ("4", "4"), ("5", "5")]);

    let mut v = vec![
        Field { sticky: true, ..f("analyst", "Analyst", Text, Survey) },
        Field { sticky: true, ..f("obs_year", "Year", Int, Survey) },
        Field { sticky: true, ..f("mission_id", "Mission", Text, Survey) },
        Field { sticky: true, ..f("site", "Site", Text, Survey) },
        Field { sticky: true, options: opts(&[("A", "A"), ("B", "B")]), ..f("transect", "Transect", Choice { free: true }, Survey) },
        Field {
            sticky: true,
            options: opts(&[("0", "0"), ("5", "5"), ("10", "10"), ("15", "15")]),
            ..f("segment", "Segment", Choice { free: true }, Survey)
        },
        Field { sticky: true, hint: "m", ..f("seglength", "Seg. length", Float, Survey) },
        Field { sticky: true, hint: "m", ..f("segwidth", "Seg. width", Float, Survey) },
        f("spcode", "Species", Species, Colony),
        Field { options: morph, ..f("morph_code", "Morphology", Choice { free: false }, Colony) },
        f("no_colony", "No colony", YesNo, Colony),
        f("juvenile", "Juvenile", YesNo, Colony),
        Field { options: juv_sub, ..f("juv_substrate", "Juv. substrate", Choice { free: true }, Colony) },
        f("remnant", "Remnant", YesNo, Colony),
        f("fragment", "Fragment", YesNo, Colony),
        f("ex_bound", "Ex. boundary", YesNo, Colony),
        Field { hint: "%", ..f("old_dead", "Old dead", Int, Condition) },
    ];
    for i in 1..=3 {
        let (cause, rd): (&'static str, &'static str) = match i {
            1 => ("rdcause1", "rd_1"),
            2 => ("rdcause2", "rd_2"),
            _ => ("rdcause3", "rd_3"),
        };
        v.push(f(cause, ["RD cause 1", "RD cause 2", "RD cause 3"][i - 1], Text, Condition));
        v.push(Field { hint: "%", ..f(rd, ["Recent dead 1", "Recent dead 2", "Recent dead 3"][i - 1], Int, Condition) });
    }
    for i in 1..=3 {
        let keys: [(&'static str, &'static str, &'static str); 3] =
            [("con_1", "extent_1", "sev_1"), ("con_2", "extent_2", "sev_2"), ("con_3", "extent_3", "sev_3")];
        let (con, ext, s) = keys[i - 1];
        v.push(f(con, ["Condition 1", "Condition 2", "Condition 3"][i - 1], Text, Condition));
        v.push(Field { hint: "%", ..f(ext, ["Extent 1", "Extent 2", "Extent 3"][i - 1], Int, Condition) });
        v.push(Field { options: sev.clone(), ..f(s, ["Severity 1", "Severity 2", "Severity 3"][i - 1], Choice { free: false }, Condition) });
    }
    v
}

/// Convert typed text from the form into the JSON value the server expects.
/// Empty text → `None` (property removed / null).
pub fn parse_value(kind: FieldKind, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match kind {
        FieldKind::Int | FieldKind::YesNo => t
            .parse::<i64>()
            .map(|n| Some(json!(n)))
            .or_else(|_| t.parse::<f64>().map(|x| Some(json!(x.round() as i64))))
            .map_err(|_| "whole number".to_string()),
        FieldKind::Float => t.parse::<f64>().map(|x| Some(json!(x))).map_err(|_| "number".to_string()),
        // The web form upper-cases every text input (v2-defaults.js initAllCaps).
        FieldKind::Species | FieldKind::Choice { .. } | FieldKind::Text => Ok(Some(json!(t.to_uppercase()))),
    }
}

pub fn parse_field(field: &Field, text: &str) -> Result<Option<Value>, String> {
    // Segment is a number on the server even though it's picked from a list.
    if field.key == "segment" || field.key.starts_with("sev_") {
        return parse_value(FieldKind::Int, text);
    }
    parse_value(field.kind, text)
}

/// Pull `obs_year` and `site` out of a bucket mosaic file name such as
/// `2026_BAK-2089_mos_cog.tif`.
pub fn defaults_from_file_name(name: &str) -> Vec<(&'static str, Value)> {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let mut parts = stem.split('_');
    let mut out = Vec::new();
    if let Some(y) = parts.next().and_then(|y| y.parse::<i64>().ok()).filter(|y| (1990..2100).contains(y)) {
        out.push(("obs_year", json!(y)));
        if let Some(site) = parts.next().filter(|s| s.contains('-')) {
            out.push(("site", json!(site.to_uppercase())));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_the_web_form() {
        let fields = default_fields();
        let get = |k: &str| fields.iter().find(|f| f.key == k).unwrap().clone();
        assert_eq!(parse_field(&get("obs_year"), "2026").unwrap(), Some(json!(2026)));
        assert_eq!(parse_field(&get("segment"), "10").unwrap(), Some(json!(10)));
        assert_eq!(parse_field(&get("seglength"), "2.5").unwrap(), Some(json!(2.5)));
        assert_eq!(parse_field(&get("spcode"), "plob").unwrap(), Some(json!("PLOB")));
        assert_eq!(parse_field(&get("analyst"), "Jane Doe").unwrap(), Some(json!("JANE DOE")));
        assert_eq!(parse_field(&get("site"), "").unwrap(), None);
        assert!(parse_field(&get("obs_year"), "abc").is_err());
    }

    #[test]
    fn year_and_site_from_file_name() {
        let d = defaults_from_file_name("2026_BAK-2089_mos_cog.tif");
        assert_eq!(d, vec![("obs_year", json!(2026)), ("site", json!("BAK-2089"))]);
        assert!(defaults_from_file_name("mosaic.tif").is_empty());
    }
}
