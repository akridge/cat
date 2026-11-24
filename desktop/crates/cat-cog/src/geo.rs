//! Coordinate helpers: WGS84 ↔ WebMercator, tile footprints, level choice.
//!
//! The map works in WebMercator metres (EPSG:3857), like the Leaflet UI.
//! Annotations themselves stay in WGS84 lon/lat, as stored on the server.

use crate::cog::{Cog, Crs};
use crate::error::{CogError, Result};

pub const EARTH_RADIUS: f64 = 6_378_137.0;
pub const MAX_LAT: f64 = 85.051_128_779_806_59;

pub fn lonlat_to_merc(lon: f64, lat: f64) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    let x = EARTH_RADIUS * lon.to_radians();
    let y = EARTH_RADIUS * (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln();
    (x, y)
}

pub fn merc_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = (x / EARTH_RADIUS).to_degrees();
    let lat = (2.0 * (y / EARTH_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

/// Axis-aligned box in WebMercator metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MercBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl MercBox {
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
    pub fn intersects(&self, o: &MercBox) -> bool {
        self.min_x < o.max_x && o.min_x < self.max_x && self.min_y < o.max_y && o.min_y < self.max_y
    }
}

impl Cog {
    /// Check the CRS is one we can place on the map.
    pub fn check_crs(&self) -> Result<()> {
        match self.crs {
            Crs::LocalAsWgs84 | Crs::Epsg(4326) | Crs::Epsg(3857) => Ok(()),
            // UTM and others: add proj4rs when the first such COG appears.
            Crs::Epsg(c) => Err(CogError::Projection(format!("EPSG:{c} is not supported yet"))),
        }
    }

    /// Native CRS coordinates → WebMercator metres.
    pub fn native_to_merc(&self, x: f64, y: f64) -> (f64, f64) {
        match self.crs.epsg() {
            3857 => (x, y),
            _ => lonlat_to_merc(x, y),
        }
    }

    /// WebMercator metres → native CRS coordinates.
    pub fn merc_to_native(&self, x: f64, y: f64) -> (f64, f64) {
        match self.crs.epsg() {
            3857 => (x, y),
            _ => merc_to_lonlat(x, y),
        }
    }

    /// Full-resolution pixel → WebMercator metres.
    pub fn pixel_to_merc(&self, px: f64, py: f64) -> (f64, f64) {
        let (x, y) = self.geotransform.apply(px, py);
        self.native_to_merc(x, y)
    }

    pub fn merc_bounds(&self) -> MercBox {
        let (w, h) = (self.width() as f64, self.height() as f64);
        let pts = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(x, y)| self.pixel_to_merc(x, y));
        MercBox {
            min_x: pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
            min_y: pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
            max_x: pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
            max_y: pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
        }
    }

    /// Size of one full-resolution pixel in WebMercator metres (at the image centre).
    pub fn merc_pixel_size(&self) -> f64 {
        let b = self.merc_bounds();
        (b.width() / self.width() as f64).min(b.height() / self.height() as f64)
    }

    /// Pick the coarsest level that is still at least as sharp as the screen.
    /// `merc_per_screen_px` = WebMercator metres covered by one screen pixel.
    pub fn choose_level(&self, merc_per_screen_px: f64) -> usize {
        let full = self.merc_pixel_size();
        let mut best = 0;
        for (i, l) in self.levels.iter().enumerate() {
            if full * l.decimation <= merc_per_screen_px * 1.2 {
                best = i;
            }
        }
        best
    }

    /// Corners of a tile's valid area in WebMercator, in the order
    /// top-left, top-right, bottom-right, bottom-left.
    pub fn tile_corners_merc(&self, level: usize, tx: u32, ty: u32) -> [(f64, f64); 4] {
        let l = &self.levels[level];
        let x0 = (tx * l.tile_w) as f64;
        let y0 = (ty * l.tile_h) as f64;
        let x1 = ((tx + 1) * l.tile_w).min(l.width) as f64;
        let y1 = ((ty + 1) * l.tile_h).min(l.height) as f64;
        // Level pixels → full-resolution pixels (overviews round their size, so
        // scale by each axis' exact ratio to keep edges aligned).
        let sx = self.width() as f64 / l.width as f64;
        let sy = self.height() as f64 / l.height as f64;
        [
            self.pixel_to_merc(x0 * sx, y0 * sy),
            self.pixel_to_merc(x1 * sx, y0 * sy),
            self.pixel_to_merc(x1 * sx, y1 * sy),
            self.pixel_to_merc(x0 * sx, y1 * sy),
        ]
    }

    /// Tiles of `level` that intersect a WebMercator view box.
    pub fn visible_tiles(&self, level: usize, view: &MercBox) -> Vec<(u32, u32)> {
        let Some(inv) = self.geotransform.invert() else { return Vec::new() };
        let l = &self.levels[level];
        let sx = self.width() as f64 / l.width as f64;
        let sy = self.height() as f64 / l.height as f64;
        let corners = [
            (view.min_x, view.min_y),
            (view.max_x, view.min_y),
            (view.min_x, view.max_y),
            (view.max_x, view.max_y),
        ];
        let mut px0 = f64::INFINITY;
        let mut py0 = f64::INFINITY;
        let mut px1 = f64::NEG_INFINITY;
        let mut py1 = f64::NEG_INFINITY;
        for (mx, my) in corners {
            let (nx, ny) = self.merc_to_native(mx, my);
            let (px, py) = inv.apply(nx, ny);
            px0 = px0.min(px / sx);
            py0 = py0.min(py / sy);
            px1 = px1.max(px / sx);
            py1 = py1.max(py / sy);
        }
        if px1 < 0.0 || py1 < 0.0 || px0 > l.width as f64 || py0 > l.height as f64 {
            return Vec::new();
        }
        let tx0 = (px0.max(0.0) / l.tile_w as f64).floor() as u32;
        let ty0 = (py0.max(0.0) / l.tile_h as f64).floor() as u32;
        let tx1 = ((px1 / l.tile_w as f64).floor() as u32).min(l.tiles_across() - 1);
        let ty1 = ((py1 / l.tile_h as f64).floor() as u32).min(l.tiles_down() - 1);
        let mut out = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                out.push((tx, ty));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merc_roundtrip() {
        for &(lon, lat) in &[(-176.476, 0.2059), (-157.8, 21.3), (145.7, 15.2), (0.0, 0.0)] {
            let (x, y) = lonlat_to_merc(lon, lat);
            let (lon2, lat2) = merc_to_lonlat(x, y);
            assert!((lon - lon2).abs() < 1e-10 && (lat - lat2).abs() < 1e-10);
        }
    }
}
