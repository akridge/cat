//! Annotation = geometry (WGS84) + flat properties, matching the CAT server.

use serde_json::{json, Map, Value};

pub type LonLat = [f64; 2];

/// Geometry in WGS84 lon/lat. Polygon rings are stored *open* (first vertex
/// not repeated) and closed only when written as GeoJSON.
#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    Point(LonLat),
    Line(Vec<LonLat>),
    Polygon(Vec<LonLat>),
}

impl Geom {
    pub fn vertices(&self) -> &[LonLat] {
        match self {
            Geom::Point(p) => std::slice::from_ref(p),
            Geom::Line(v) | Geom::Polygon(v) => v,
        }
    }
    pub fn vertices_mut(&mut self) -> &mut [LonLat] {
        match self {
            Geom::Point(p) => std::slice::from_mut(p),
            Geom::Line(v) | Geom::Polygon(v) => v,
        }
    }
    pub fn is_valid(&self) -> bool {
        match self {
            Geom::Point(_) => true,
            Geom::Line(v) => v.len() >= 2,
            Geom::Polygon(v) => v.len() >= 3,
        }
    }
    /// (min_lon, min_lat, max_lon, max_lat)
    pub fn bbox(&self) -> [f64; 4] {
        let v = self.vertices();
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for p in v {
            b[0] = b[0].min(p[0]);
            b[1] = b[1].min(p[1]);
            b[2] = b[2].max(p[0]);
            b[3] = b[3].max(p[1]);
        }
        b
    }

    pub fn to_geojson(&self) -> Value {
        match self {
            Geom::Point(p) => json!({"type": "Point", "coordinates": p}),
            Geom::Line(v) => json!({"type": "LineString", "coordinates": v}),
            Geom::Polygon(v) => {
                let mut ring = v.clone();
                if let Some(f) = v.first() {
                    ring.push(*f);
                }
                json!({"type": "Polygon", "coordinates": [ring]})
            }
        }
    }

    pub fn from_geojson(g: &Value) -> Option<Geom> {
        let pt = |v: &Value| -> Option<LonLat> {
            let a = v.as_array()?;
            Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?])
        };
        let pts = |v: &Value| -> Option<Vec<LonLat>> { v.as_array()?.iter().map(pt).collect() };
        let c = g.get("coordinates")?;
        match g.get("type")?.as_str()? {
            "Point" => Some(Geom::Point(pt(c)?)),
            "LineString" => Some(Geom::Line(pts(c)?)),
            "Polygon" => {
                let mut ring = pts(c.as_array()?.first()?)?;
                if ring.len() > 1 && ring.first() == ring.last() {
                    ring.pop();
                }
                Some(Geom::Polygon(ring))
            }
            // Multi* from imports: keep the first part.
            "MultiPolygon" => {
                let mut ring = pts(c.as_array()?.first()?.as_array()?.first()?)?;
                if ring.len() > 1 && ring.first() == ring.last() {
                    ring.pop();
                }
                Some(Geom::Polygon(ring))
            }
            "MultiLineString" => Some(Geom::Line(pts(c.as_array()?.first()?)?)),
            "MultiPoint" => Some(Geom::Point(pt(c.as_array()?.first()?)?)),
            _ => None,
        }
    }
}

/// The drawing tool that made an annotation (the web app's `type` property).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnnKind {
    Point,
    Line,
    Rectangle,
    Polygon,
}

impl AnnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AnnKind::Point => "point",
            AnnKind::Line => "line",
            AnnKind::Rectangle => "rectangle",
            AnnKind::Polygon => "polygon",
        }
    }
    pub fn parse(s: &str, g: &Geom) -> AnnKind {
        match (s, g) {
            ("rectangle", Geom::Polygon(_)) => AnnKind::Rectangle,
            (_, Geom::Point(_)) => AnnKind::Point,
            (_, Geom::Line(_)) => AnnKind::Line,
            _ => AnnKind::Polygon,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            AnnKind::Point => "Point",
            AnnKind::Line => "Line",
            AnnKind::Rectangle => "Rectangle",
            AnnKind::Polygon => "Polygon",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    /// Stable id (`client_uuid` on the server) — the key for sync and undo.
    pub uuid: String,
    pub kind: AnnKind,
    pub geom: Geom,
    /// Flat properties exactly as the server stores them.
    pub props: Map<String, Value>,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Annotation {
    pub fn new(kind: AnnKind, geom: Geom, props: Map<String, Value>) -> Self {
        let uuid = uuid::Uuid::new_v4().to_string();
        let mut a = Annotation { uuid, kind, geom, props };
        a.props.insert("type".into(), json!(kind.as_str()));
        a.props.insert("created_at".into(), json!(now_iso()));
        a
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.props.get(key).filter(|v| !v.is_null())
    }

    /// Property as display text ("" when missing).
    pub fn text(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }

    pub fn spcode(&self) -> String {
        self.text("spcode")
    }

    /// Same rule as the web app: complete once a species code is set.
    pub fn is_complete(&self) -> bool {
        let s = self.spcode();
        let s = s.trim();
        !s.is_empty() && s != "-"
    }

    pub fn touch(&mut self) {
        self.props.insert("updated_at".into(), json!(now_iso()));
    }

    pub fn to_feature(&self) -> Value {
        let mut props = self.props.clone();
        props.insert("client_uuid".into(), json!(self.uuid));
        props.insert("type".into(), json!(self.kind.as_str()));
        json!({"type": "Feature", "id": self.uuid, "geometry": self.geom.to_geojson(), "properties": props})
    }

    pub fn from_feature(f: &Value) -> Option<Annotation> {
        let geom = Geom::from_geojson(f.get("geometry")?)?;
        let mut props = f.get("properties").and_then(|p| p.as_object()).cloned().unwrap_or_default();
        let uuid = props
            .get("client_uuid")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| f.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        props.remove("client_uuid");
        let kind = AnnKind::parse(props.get("type").and_then(|v| v.as_str()).unwrap_or(""), &geom);
        Some(Annotation { uuid, kind, geom, props })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polygon_roundtrip_closes_ring() {
        let a = Annotation::new(
            AnnKind::Polygon,
            Geom::Polygon(vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]),
            Map::new(),
        );
        let f = a.to_feature();
        assert_eq!(f["geometry"]["coordinates"][0].as_array().unwrap().len(), 4);
        assert_eq!(f["properties"]["client_uuid"], a.uuid);
        let b = Annotation::from_feature(&f).unwrap();
        assert_eq!(b.geom, a.geom);
        assert_eq!(b.uuid, a.uuid);
        assert_eq!(b.kind, AnnKind::Polygon);
    }

    #[test]
    fn completeness_matches_web_rule() {
        let mut a = Annotation::new(AnnKind::Point, Geom::Point([0.0, 0.0]), Map::new());
        assert!(!a.is_complete());
        a.props.insert("spcode".into(), json!("-"));
        assert!(!a.is_complete());
        a.props.insert("spcode".into(), json!("PLOB"));
        assert!(a.is_complete());
    }
}
