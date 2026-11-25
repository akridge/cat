//! Reading and writing annotation files (GeoJSON) and exports (CSV).

use std::path::Path;

use serde_json::{json, Value};

use crate::Annotation;

pub fn to_feature_collection(items: &[Annotation], meta: Value) -> Value {
    json!({
        "type": "FeatureCollection",
        "cat": meta,
        "features": items.iter().map(|a| a.to_feature()).collect::<Vec<_>>(),
    })
}

pub fn from_feature_collection(v: &Value) -> Vec<Annotation> {
    v.get("features")
        .and_then(|f| f.as_array())
        .map(|f| f.iter().filter_map(Annotation::from_feature).collect())
        .unwrap_or_default()
}

/// Write atomically: a crash mid-save never leaves a truncated file, and the
/// previous version is kept as `.bak`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("bak"));
    }
    std::fs::rename(&tmp, path)
}

pub fn save_geojson(path: &Path, items: &[Annotation], meta: Value) -> std::io::Result<()> {
    let v = to_feature_collection(items, meta);
    write_atomic(path, serde_json::to_string_pretty(&v).unwrap().as_bytes())
}

pub fn load_geojson(path: &Path) -> anyhow::Result<Vec<Annotation>> {
    let text = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text)?;
    Ok(from_feature_collection(&v))
}

/// CSV with one row per annotation: id, type, centroid lon/lat, ground
/// area/length, every property seen, then WKT geometry.
pub fn to_csv(items: &[Annotation]) -> anyhow::Result<String> {
    use crate::geom;
    let mut keys: Vec<String> = Vec::new();
    for a in items {
        for k in a.props.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    let mut w = csv::Writer::from_writer(Vec::new());
    let mut header = vec!["client_uuid".to_string(), "geom_type".into(), "lon".into(), "lat".into(), "area_m2".into(), "length_m".into()];
    header.extend(keys.iter().cloned());
    header.push("wkt".into());
    w.write_record(&header)?;
    for a in items {
        let v = a.geom.vertices();
        let (c, area, len) = match &a.geom {
            crate::Geom::Point(p) => (*p, 0.0, 0.0),
            crate::Geom::Line(l) => (geom::centroid(l), 0.0, geom::length_m(l, false)),
            crate::Geom::Polygon(r) => (geom::centroid(r), geom::area_m2(r), geom::length_m(r, true)),
        };
        let coords = |pts: &[[f64; 2]]| pts.iter().map(|p| format!("{} {}", p[0], p[1])).collect::<Vec<_>>().join(", ");
        let wkt = match &a.geom {
            crate::Geom::Point(p) => format!("POINT ({} {})", p[0], p[1]),
            crate::Geom::Line(_) => format!("LINESTRING ({})", coords(v)),
            crate::Geom::Polygon(_) => {
                let mut r = v.to_vec();
                r.push(v[0]);
                format!("POLYGON (({}))", coords(&r))
            }
        };
        let mut row = vec![a.uuid.clone(), a.kind.as_str().into(), c[0].to_string(), c[1].to_string(), format!("{area:.6}"), format!("{len:.4}")];
        row.extend(keys.iter().map(|k| a.text(k)));
        row.push(wkt);
        w.write_record(&row)?;
    }
    Ok(String::from_utf8(w.into_inner()?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnnKind, Geom};

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cat-core-test-{}", std::process::id()));
        let path = dir.join("a.geojson");
        let mut a = Annotation::new(AnnKind::Rectangle, Geom::Polygon(vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]), Default::default());
        a.props.insert("spcode".into(), json!("PLOB"));
        a.props.insert("server_only_field".into(), json!({"kept": true}));
        save_geojson(&path, std::slice::from_ref(&a), json!({"cog": "x"})).unwrap();
        save_geojson(&path, std::slice::from_ref(&a), json!({"cog": "x"})).unwrap();
        assert!(path.with_extension("bak").exists());
        let b = load_geojson(&path).unwrap();
        assert_eq!(b, vec![a.clone()]);
        let csv = to_csv(&b).unwrap();
        assert!(csv.contains("PLOB") && csv.contains("POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))"));
        std::fs::remove_dir_all(dir).ok();
    }
}
