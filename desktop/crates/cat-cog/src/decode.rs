//! Tile decompression and conversion to RGBA8.

use crate::cog::Level;
use crate::error::{CogError, Result};

/// A decoded internal tile, always RGBA8, `w * h * 4` bytes.
///
/// `w`/`h` are the *valid* pixels: edge tiles are padded in the file and the
/// padding is cropped here so callers never draw it.
#[derive(Clone)]
pub struct DecodedTile {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeOptions {
    /// Make the mosaic's background colour transparent. The bucket mosaics
    /// have no mask or nodata value; the area outside the reef is stored as
    /// solid white (or black), detected per file by [`crate::Cog::open`].
    pub background_transparent: bool,
}

/// Decompress one tile's bytes according to the level's compression.
pub fn decompress(level: &Level, data: &[u8], expected: usize) -> Result<Vec<u8>> {
    match level.compression {
        1 => Ok(data.to_vec()),
        50000 => zstd::bulk::decompress(data, expected).map_err(|e| CogError::Decode(format!("zstd: {e}"))),
        5 => {
            // TIFF LZW is MSB-first with "early change".
            let mut dec = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
            dec.decode(data).map_err(|e| CogError::Decode(format!("lzw: {e}")))
        }
        8 | 32946 => miniz_oxide::inflate::decompress_to_vec_zlib(data)
            .map_err(|e| CogError::Decode(format!("deflate: {e:?}"))),
        c => Err(CogError::Unsupported(format!(
            "{} compression ({c}) is not supported yet",
            crate::cog::compression_name(c)
        ))),
    }
}

/// Undo TIFF horizontal differencing (predictor 2) for 8-bit samples.
fn undo_predictor_u8(buf: &mut [u8], row_len: usize, spp: usize) {
    for row in buf.chunks_exact_mut(row_len) {
        for i in spp..row.len() {
            row[i] = row[i].wrapping_add(row[i - spp]);
        }
    }
}

/// Decode a tile of `level` to RGBA8, cropping padding at the image edge.
pub fn decode_tile(
    level: &Level,
    tx: u32,
    ty: u32,
    data: &[u8],
    mask: Option<(&Level, &[u8])>,
    background: &[[u8; 3]],
    opts: DecodeOptions,
) -> Result<DecodedTile> {
    if level.bits != 8 || level.sample_format != 1 {
        return Err(CogError::Unsupported(format!(
            "{}-bit sample format {} (only 8-bit imagery for now)",
            level.bits, level.sample_format
        )));
    }
    if level.planar != 1 && level.samples > 1 {
        return Err(CogError::Unsupported("band-separate (planar) layout".into()));
    }
    let spp = level.samples as usize;
    let (tw, th) = (level.tile_w as usize, level.tile_h as usize);
    let row_len = tw * spp;
    let mut raw = decompress(level, data, row_len * th)?;
    if raw.len() < row_len * th {
        // Some writers truncate the last tile; pad with zeros.
        raw.resize(row_len * th, 0);
    }
    match level.predictor {
        1 => {}
        2 => undo_predictor_u8(&mut raw, row_len, spp),
        p => return Err(CogError::Unsupported(format!("predictor {p}"))),
    }

    let w = (level.width - tx * level.tile_w).min(level.tile_w) as usize;
    let h = (level.height - ty * level.tile_h).min(level.tile_h) as usize;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &raw[y * row_len..y * row_len + w * spp];
        match spp {
            1 => row.iter().for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
            2 => row.chunks_exact(2).for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
            3 => row.chunks_exact(3).for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            _ => row.chunks_exact(spp).for_each(|p| {
                let a = if level.extra_samples.first().is_some_and(|&e| e == 1 || e == 2) { p[3] } else { 255 };
                rgba.extend_from_slice(&[p[0], p[1], p[2], a]);
            }),
        }
    }

    if let Some((mlevel, mdata)) = mask {
        apply_mask(mlevel, mdata, w, h, &mut rgba)?;
    }
    if opts.background_transparent && !background.is_empty() {
        for px in rgba.chunks_exact_mut(4) {
            if background.iter().any(|bg| px[..3] == *bg) {
                px[3] = 0;
            }
        }
    }
    Ok(DecodedTile { w: w as u32, h: h as u32, rgba })
}

/// Apply a GDAL internal mask (1-bit or 8-bit) to the alpha channel.
fn apply_mask(m: &Level, data: &[u8], w: usize, h: usize, rgba: &mut [u8]) -> Result<()> {
    let tw = m.tile_w as usize;
    let row_bytes = if m.bits == 1 { tw.div_ceil(8) } else { tw };
    let raw = decompress(m, data, row_bytes * m.tile_h as usize)?;
    for y in 0..h {
        for x in 0..w {
            let on = if m.bits == 1 {
                raw.get(y * row_bytes + x / 8).is_some_and(|b| b & (0x80 >> (x % 8)) != 0)
            } else {
                raw.get(y * row_bytes + x).is_some_and(|&b| b != 0)
            };
            if !on {
                rgba[(y * w + x) * 4 + 3] = 0;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictor_roundtrip() {
        let mut row = vec![10u8, 20, 30, 1, 1, 1, 2, 2, 2];
        undo_predictor_u8(&mut row, 9, 3);
        assert_eq!(row, vec![10, 20, 30, 11, 21, 31, 13, 23, 33]);
    }
}
