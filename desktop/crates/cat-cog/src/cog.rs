//! A parsed COG: resolution levels, internal masks and georeferencing.

use std::sync::Arc;

use crate::error::{CogError, Result};
use crate::source::{BlockReader, RangeSource};
use crate::tiff::{self, tag, Endian, Ifd};

/// One resolution level (full resolution or an overview) or a mask.
#[derive(Clone, Debug)]
pub struct Level {
    pub width: u32,
    pub height: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub samples: u16,
    pub bits: u16,
    /// 1 = uint, 2 = int, 3 = float
    pub sample_format: u16,
    pub compression: u16,
    pub photometric: u16,
    pub planar: u16,
    pub predictor: u16,
    pub extra_samples: Vec<u16>,
    pub offsets: Vec<u64>,
    pub byte_counts: Vec<u64>,
    pub jpeg_tables: Option<Vec<u8>>,
    /// Width of the full-resolution image divided by this level's width.
    pub decimation: f64,
    pub is_mask: bool,
}

impl Level {
    pub fn tiles_across(&self) -> u32 {
        self.width.div_ceil(self.tile_w)
    }
    pub fn tiles_down(&self) -> u32 {
        self.height.div_ceil(self.tile_h)
    }
    pub fn compression_name(&self) -> &'static str {
        compression_name(self.compression)
    }
}

pub fn compression_name(c: u16) -> &'static str {
    match c {
        1 => "none",
        5 => "LZW",
        6 | 7 => "JPEG",
        8 | 32946 => "DEFLATE",
        34887 => "LERC",
        34925 => "LZMA",
        50000 => "ZSTD",
        50001 => "WEBP",
        _ => "unknown",
    }
}

/// Coordinate reference system as far as CAT needs to know it.
#[derive(Clone, Debug, PartialEq)]
pub enum Crs {
    Epsg(u32),
    /// LOCAL_CS / user-defined with no EPSG code. The CAT server treats these
    /// as EPSG:4326 (see `_is_local_cs` in cat/server.py); so do we.
    LocalAsWgs84,
}

impl Crs {
    pub fn epsg(&self) -> u32 {
        match self {
            Crs::Epsg(c) => *c,
            Crs::LocalAsWgs84 => 4326,
        }
    }
}

/// Affine pixel → CRS transform for the full-resolution image
/// (GDAL order: x0, dx, rx, y0, ry, dy).
#[derive(Clone, Copy, Debug)]
pub struct GeoTransform(pub [f64; 6]);

impl GeoTransform {
    pub fn apply(&self, px: f64, py: f64) -> (f64, f64) {
        let g = self.0;
        (g[0] + px * g[1] + py * g[2], g[3] + px * g[4] + py * g[5])
    }
    pub fn invert(&self) -> Option<GeoTransform> {
        let g = self.0;
        let det = g[1] * g[5] - g[2] * g[4];
        if det.abs() < f64::EPSILON * 1e-6 {
            return None;
        }
        let inv_det = 1.0 / det;
        let a = g[5] * inv_det;
        let b = -g[2] * inv_det;
        let d = -g[4] * inv_det;
        let e = g[1] * inv_det;
        Some(GeoTransform([-g[0] * a - g[3] * b, a, b, -g[0] * d - g[3] * e, d, e]))
    }
}

pub struct Cog {
    pub source: Arc<dyn RangeSource>,
    pub endian: Endian,
    pub bigtiff: bool,
    /// Full resolution first, then overviews from fine to coarse.
    pub levels: Vec<Level>,
    /// Internal masks (GDAL "per-dataset" masks), matched to `levels` by size.
    pub masks: Vec<Level>,
    pub crs: Crs,
    pub geotransform: GeoTransform,
    pub nodata: Option<f64>,
    pub gdal_metadata: Option<String>,
    /// Solid background colours around the imagery (pure white and/or pure
    /// black), detected from the corners of the coarsest overview.
    pub background: Vec<[u8; 3]>,
}

impl Cog {
    /// Open a COG and parse all of its headers.
    pub fn open(source: Arc<dyn RangeSource>) -> Result<Cog> {
        // 64 KiB blocks: a COG keeps all IFDs together at the start of the
        // file, so this is usually one to three requests.
        let reader = BlockReader::new(source.clone(), 64 * 1024);
        let (endian, bigtiff, ifds) = tiff::read_ifds(&reader)?;
        if ifds.is_empty() {
            return Err(CogError::Format("no images".into()));
        }
        let full_w = ifds[0].u64(tag::IMAGE_WIDTH, endian).unwrap_or(0) as f64;

        let mut levels = Vec::new();
        let mut masks = Vec::new();
        for ifd in &ifds {
            let lvl = parse_level(ifd, endian, full_w)?;
            if lvl.is_mask {
                masks.push(lvl);
            } else {
                levels.push(lvl);
            }
        }
        levels.sort_by_key(|l| std::cmp::Reverse(l.width));
        masks.sort_by_key(|l| std::cmp::Reverse(l.width));

        let first = &ifds[0];
        let geotransform = read_geotransform(first, endian)?;
        let crs = read_crs(first, endian);
        let nodata = first
            .get(tag::GDAL_NODATA)
            .and_then(|e| e.string().trim().parse::<f64>().ok());
        let gdal_metadata = first.get(tag::GDAL_METADATA).map(|e| e.string());

        let mut cog =
            Cog { source, endian, bigtiff, levels, masks, crs, geotransform, nodata, gdal_metadata, background: Vec::new() };
        if cog.masks.is_empty() && cog.nodata.is_none() && cog.levels[0].samples >= 3 {
            cog.background = cog.detect_background();
        }
        Ok(cog)
    }

    /// Look at the corners of the coarsest overview. If any is pure white
    /// or pure black, the mosaic was exported with a solid background:
    /// white around the reef, plus black wedges where the export rotated
    /// the image (at coarse levels those wedges blend to grey, which is why
    /// one pure corner is enough). Both colours are then treated as empty.
    fn detect_background(&self) -> Vec<[u8; 3]> {        let level = self.levels.len() - 1;
        let l = &self.levels[level];
        let (ax, ay) = (l.tiles_across() - 1, l.tiles_down() - 1);
        let mut tiles = vec![(0, 0), (ax, 0), (0, ay), (ax, ay)];
        tiles.dedup();
        let opts = crate::DecodeOptions { background_transparent: false };
        let decoded = self.fetch_tiles(level, &tiles, opts);
        let get = |tx: u32, ty: u32, x: u32, y: u32| -> Option<[u8; 3]> {
            let t = decoded.iter().find(|r| r.0 == (tx, ty))?.1.as_ref().ok()?.as_ref()?;
            let (x, y) = (x.min(t.w - 1), y.min(t.h - 1));
            let i = ((y * t.w + x) * 4) as usize;
            Some([t.rgba[i], t.rgba[i + 1], t.rgba[i + 2]])
        };
        // Inset by 8 px: the outermost overview pixels are blended by resampling.
        const INSET: u32 = 8;
        let (w, h) = (l.width.saturating_sub(INSET + 1), l.height.saturating_sub(INSET + 1));
        let (x1, y1) = (w.saturating_sub(ax * l.tile_w), h.saturating_sub(ay * l.tile_h));
        let corners =
            [get(0, 0, INSET, INSET), get(ax, 0, x1, INSET), get(0, ay, INSET, y1), get(ax, ay, x1, y1)];
        let pure = [[255u8; 3], [0u8; 3]];
        if corners.iter().flatten().any(|c| pure.contains(c)) {
            pure.to_vec()
        } else {
            Vec::new()
        }
    }

    pub fn width(&self) -> u32 {
        self.levels[0].width
    }
    pub fn height(&self) -> u32 {
        self.levels[0].height
    }

    /// The mask that matches a level, if the file has internal masks.
    pub fn mask_for(&self, level: usize) -> Option<&Level> {
        let l = &self.levels[level];
        self.masks.iter().find(|m| m.width == l.width && m.height == l.height)
    }

    /// Ground size of one full-resolution pixel, in CRS units.
    pub fn pixel_size(&self) -> (f64, f64) {
        let g = self.geotransform.0;
        ((g[1] * g[1] + g[4] * g[4]).sqrt(), (g[2] * g[2] + g[5] * g[5]).sqrt())
    }

    /// Bounding box of the image in its own CRS: (minx, miny, maxx, maxy).
    pub fn native_bounds(&self) -> (f64, f64, f64, f64) {
        let (w, h) = (self.width() as f64, self.height() as f64);
        let pts = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(x, y)| self.geotransform.apply(x, y));
        let minx = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let maxx = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let miny = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let maxy = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        (minx, miny, maxx, maxy)
    }
}

fn parse_level(ifd: &Ifd, e: Endian, full_w: f64) -> Result<Level> {
    let width = ifd.u64(tag::IMAGE_WIDTH, e).unwrap_or(0) as u32;
    let height = ifd.u64(tag::IMAGE_LENGTH, e).unwrap_or(0) as u32;
    let subfile = ifd.u64(tag::NEW_SUBFILE_TYPE, e).unwrap_or(0);
    let tile_w = ifd.u64(tag::TILE_WIDTH, e).unwrap_or(0) as u32;
    let tile_h = ifd.u64(tag::TILE_LENGTH, e).unwrap_or(0) as u32;
    if tile_w == 0 || tile_h == 0 {
        return Err(CogError::Unsupported("image is striped, not tiled (not a COG)".into()));
    }
    let bits = ifd.u64s(tag::BITS_PER_SAMPLE, e);
    let photometric = ifd.u64(tag::PHOTOMETRIC, e).unwrap_or(1) as u16;
    Ok(Level {
        width,
        height,
        tile_w,
        tile_h,
        samples: ifd.u64(tag::SAMPLES_PER_PIXEL, e).unwrap_or(1) as u16,
        bits: bits.first().copied().unwrap_or(1) as u16,
        sample_format: ifd.u64(tag::SAMPLE_FORMAT, e).unwrap_or(1) as u16,
        compression: ifd.u64(tag::COMPRESSION, e).unwrap_or(1) as u16,
        photometric,
        planar: ifd.u64(tag::PLANAR_CONFIG, e).unwrap_or(1) as u16,
        predictor: ifd.u64(tag::PREDICTOR, e).unwrap_or(1) as u16,
        extra_samples: ifd.u64s(tag::EXTRA_SAMPLES, e).into_iter().map(|v| v as u16).collect(),
        offsets: ifd.u64s(tag::TILE_OFFSETS, e),
        byte_counts: ifd.u64s(tag::TILE_BYTE_COUNTS, e),
        jpeg_tables: ifd.get(tag::JPEG_TABLES).map(|x| x.data.clone()),
        decimation: if width > 0 { full_w / width as f64 } else { 1.0 },
        // NewSubfileType bit 2 = transparency mask; GDAL masks are 1-bit.
        is_mask: subfile & 4 != 0 || photometric == 4,
    })
}

fn read_geotransform(ifd: &Ifd, e: Endian) -> Result<GeoTransform> {
    let m = ifd.f64s(tag::MODEL_TRANSFORMATION, e);
    let mut gt = if m.len() >= 16 {
        GeoTransform([m[3], m[0], m[1], m[7], m[4], m[5]])
    } else {
        let scale = ifd.f64s(tag::MODEL_PIXEL_SCALE, e);
        let tie = ifd.f64s(tag::MODEL_TIEPOINT, e);
        if scale.len() < 2 || tie.len() < 6 {
            return Err(CogError::Format("missing georeferencing (no tiepoint/pixel scale)".into()));
        }
        GeoTransform([
            tie[3] - tie[0] * scale[0],
            scale[0],
            0.0,
            tie[4] + tie[1] * scale[1],
            0.0,
            -scale[1],
        ])
    };
    // GTRasterTypeGeoKey (1025) = 2 → PixelIsPoint: shift to pixel corners.
    if geokey(ifd, e, 1025) == Some(2) {
        let g = gt.0;
        gt.0[0] = g[0] - 0.5 * g[1] - 0.5 * g[2];
        gt.0[3] = g[3] - 0.5 * g[4] - 0.5 * g[5];
    }
    Ok(gt)
}

/// Look up a SHORT-valued GeoKey.
fn geokey(ifd: &Ifd, e: Endian, key: u64) -> Option<u64> {
    let dir = ifd.u64s(tag::GEO_KEY_DIRECTORY, e);
    if dir.len() < 4 {
        return None;
    }
    let n = dir[3] as usize;
    for i in 0..n {
        let k = &dir.get(4 + i * 4..8 + i * 4)?;
        if k[0] == key && k[1] == 0 {
            return Some(k[3]);
        }
    }
    None
}

fn read_crs(ifd: &Ifd, e: Endian) -> Crs {
    let model = geokey(ifd, e, 1024);
    let projected = geokey(ifd, e, 3072).filter(|&c| c > 1 && c < 32767);
    let geographic = geokey(ifd, e, 2048).filter(|&c| c > 1 && c < 32767);
    match (model, projected, geographic) {
        (Some(1), Some(c), _) => Crs::Epsg(c as u32),
        (Some(2), _, Some(c)) => Crs::Epsg(c as u32),
        (None, Some(c), _) => Crs::Epsg(c as u32),
        (None, None, Some(c)) => Crs::Epsg(c as u32),
        _ => Crs::LocalAsWgs84,
    }
}
