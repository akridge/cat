//! Planar geometry helpers (screen or projected coordinates) and ground
//! measurements for small areas such as reef plots.

pub type P = [f64; 2];

pub fn dist2(a: P, b: P) -> f64 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
}

/// Distance from `p` to segment `a`–`b`, and the parameter t along it.
pub fn point_segment(p: P, a: P, b: P) -> (f64, f64) {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
    let q = [a[0] + t * dx, a[1] + t * dy];
    (dist2(p, q).sqrt(), t)
}

pub fn point_in_polygon(p: P, ring: &[P]) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from `p` to a polyline (closed if `closed`); also returns the
/// index of the nearest segment.
pub fn point_polyline(p: P, pts: &[P], closed: bool) -> (f64, usize) {
    let mut best = (f64::INFINITY, 0);
    let n = pts.len();
    let segs = if closed { n } else { n.saturating_sub(1) };
    for i in 0..segs {
        let (d, _) = point_segment(p, pts[i], pts[(i + 1) % n]);
        if d < best.0 {
            best = (d, i);
        }
    }
    if n == 1 {
        best = (dist2(p, pts[0]).sqrt(), 0);
    }
    best
}

pub fn signed_area(ring: &[P]) -> f64 {
    let n = ring.len();
    (0..n).map(|i| {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        a[0] * b[1] - b[0] * a[1]
    }).sum::<f64>() / 2.0
}

pub fn centroid(ring: &[P]) -> P {
    let a = signed_area(ring);
    let n = ring.len();
    if a.abs() < 1e-30 || n < 3 {
        let s = ring.iter().fold([0.0, 0.0], |acc, p| [acc[0] + p[0], acc[1] + p[1]]);
        return [s[0] / n.max(1) as f64, s[1] / n.max(1) as f64];
    }
    let mut c = [0.0, 0.0];
    for i in 0..n {
        let (p, q) = (ring[i], ring[(i + 1) % n]);
        let k = p[0] * q[1] - q[0] * p[1];
        c[0] += (p[0] + q[0]) * k;
        c[1] += (p[1] + q[1]) * k;
    }
    [c[0] / (6.0 * a), c[1] / (6.0 * a)]
}

/// Metres per degree of longitude and latitude at `lat` (WGS84, good to
/// well under 0.1% for plot-sized features).
pub fn metres_per_degree(lat: f64) -> (f64, f64) {
    let r = lat.to_radians();
    let m_lat = 111_132.92 - 559.82 * (2.0 * r).cos() + 1.175 * (4.0 * r).cos();
    let m_lon = 111_412.84 * r.cos() - 93.5 * (3.0 * r).cos();
    (m_lon, m_lat)
}

/// Lon/lat vertices → local metres around their first vertex.
pub fn to_local_metres(pts: &[P]) -> Vec<P> {
    let Some(o) = pts.first() else { return Vec::new() };
    let (mx, my) = metres_per_degree(o[1]);
    pts.iter().map(|p| [(p[0] - o[0]) * mx, (p[1] - o[1]) * my]).collect()
}

/// Ground area in m² of a lon/lat polygon.
pub fn area_m2(ring: &[P]) -> f64 {
    signed_area(&to_local_metres(ring)).abs()
}

/// Ground length in metres of a lon/lat polyline (closed adds the last edge).
pub fn length_m(pts: &[P], closed: bool) -> f64 {
    let m = to_local_metres(pts);
    let n = m.len();
    let segs = if closed { n } else { n.saturating_sub(1) };
    (0..segs).map(|i| dist2(m[i], m[(i + 1) % n]).sqrt()).sum()
}

/// Human-friendly area: cm² below 1 m².
pub fn fmt_area(m2: f64) -> String {
    if m2 < 1.0 { format!("{:.1} cm²", m2 * 1e4) } else { format!("{m2:.3} m²") }
}

pub fn fmt_length(m: f64) -> String {
    if m < 1.0 { format!("{:.1} cm", m * 100.0) } else { format!("{m:.3} m") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let sq = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        assert!(point_in_polygon([1.0, 1.0], &sq));
        assert!(!point_in_polygon([3.0, 1.0], &sq));
        assert_eq!(signed_area(&sq), 4.0);
        assert_eq!(centroid(&sq), [1.0, 1.0]);
        assert_eq!(point_segment([1.0, 1.0], [0.0, 0.0], [2.0, 0.0]).0, 1.0);
        assert_eq!(point_polyline([1.0, 3.0], &sq, true).1, 2);
    }

    #[test]
    fn ground_area_at_equator() {
        // 1e-5° × 1e-5° at the equator ≈ 1.113 m × 1.106 m.
        let d = 1e-5;
        let sq = [[0.0, 0.0], [d, 0.0], [d, d], [0.0, d]];
        let a = area_m2(&sq);
        assert!((a - 1.2311).abs() < 0.01, "{a}");
        assert!((length_m(&sq[..2], false) - 1.1132).abs() < 0.001);
    }
}
