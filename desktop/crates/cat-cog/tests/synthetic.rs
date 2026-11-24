//! Offline test: build a tiny tiled, georeferenced TIFF in memory and read it
//! back through the same code path used for the bucket COGs.

use std::sync::Arc;

use cat_cog::source::MemSource;
use cat_cog::{Cog, Crs, DecodeOptions};

const W: u32 = 20;
const H: u32 = 10;
const T: u32 = 16;

fn pixel(x: u32, y: u32) -> [u8; 3] {
    if x >= 15 && y >= 5 { [255, 255, 255] } else { [x as u8 * 10, y as u8 * 20, 100] }
}

/// Classic little-endian TIFF, RGB 8-bit, uncompressed, 16x16 tiles, EPSG:4326.
fn build_tiff() -> Vec<u8> {
    let tiles_x = W.div_ceil(T);
    let mut tile_data: Vec<Vec<u8>> = Vec::new();
    for tx in 0..tiles_x {
        let mut t = Vec::new();
        for y in 0..T {
            for x in 0..T {
                let (gx, gy) = (tx * T + x, y);
                let p = if gx < W && gy < H { pixel(gx, gy) } else { [0, 0, 0] };
                t.extend_from_slice(&p);
            }
        }
        tile_data.push(t);
    }

    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    // Entries: (tag, type, count, values-as-bytes)
    let doubles = |v: &[f64]| v.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>();
    let shorts = |v: &[u16]| v.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>();
    let longs = |v: &[u32]| v.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>();
    let geokeys: Vec<u16> = vec![1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326];
    let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
        (256, 4, 1, longs(&[W])),
        (257, 4, 1, longs(&[H])),
        (258, 3, 3, shorts(&[8, 8, 8])),
        (259, 3, 1, shorts(&[1])),
        (262, 3, 1, shorts(&[2])),
        (277, 3, 1, shorts(&[3])),
        (284, 3, 1, shorts(&[1])),
        (322, 3, 1, shorts(&[T as u16])),
        (323, 3, 1, shorts(&[T as u16])),
        (324, 4, tiles_x, vec![]), // filled below
        (325, 4, tiles_x, longs(&vec![T * T * 3; tiles_x as usize])),
        (33550, 12, 3, doubles(&[1e-6, 1e-6, 0.0])),
        (33922, 12, 6, doubles(&[0.0, 0.0, 0.0, -170.0, 1.0, 0.0])),
        (34735, 3, geokeys.len() as u32, shorts(&geokeys)),
    ];
    let ifd_size = 2 + entries.len() * 12 + 4;
    let mut extra_off = 8 + ifd_size;
    // Lay out: IFD, then out-of-line values, then tile data.
    let mut extra: Vec<u8> = Vec::new();
    let values_len: usize = entries.iter().map(|e| if e.3.len() > 4 { e.3.len() } else { 0 }).sum::<usize>()
        + if tiles_x > 1 { tiles_x as usize * 4 } else { 0 };
    let tiles_start = (8 + ifd_size + values_len) as u32;
    let offsets: Vec<u32> = (0..tiles_x).map(|i| tiles_start + i * T * T * 3).collect();
    entries[9].3 = longs(&offsets);

    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, count, data) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if data.len() <= 4 {
            let mut v = data.clone();
            v.resize(4, 0);
            out.extend_from_slice(&v);
        } else {
            out.extend_from_slice(&(extra_off as u32).to_le_bytes());
            extra.extend_from_slice(data);
            extra_off += data.len();
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&extra);
    assert_eq!(out.len() as u32, tiles_start);
    for t in tile_data {
        out.extend_from_slice(&t);
    }
    out
}

#[test]
fn reads_synthetic_cog() {
    let src = Arc::new(MemSource { data: build_tiff(), name: "mem".into() });
    let cog = Cog::open(src).expect("open");
    assert_eq!((cog.width(), cog.height()), (W, H));
    assert_eq!(cog.crs, Crs::Epsg(4326));
    let (minx, miny, maxx, maxy) = cog.native_bounds();
    assert!((minx + 170.0).abs() < 1e-12 && (maxy - 1.0).abs() < 1e-12);
    assert!((maxx - (-170.0 + 20e-6)).abs() < 1e-12 && (miny - (1.0 - 10e-6)).abs() < 1e-12);

    let res = cog.fetch_tiles(0, &[(0, 0), (1, 0)], DecodeOptions { background_transparent: false });
    let t0 = res[0].1.as_ref().unwrap().as_ref().unwrap();
    let t1 = res[1].1.as_ref().unwrap().as_ref().unwrap();
    // Edge tiles are cropped to the valid image area.
    assert_eq!((t0.w, t0.h), (16, 10));
    assert_eq!((t1.w, t1.h), (4, 10));
    let px = |t: &cat_cog::DecodedTile, x: u32, y: u32| t.rgba[((y * t.w + x) * 4) as usize..][..4].to_vec();
    assert_eq!(px(t0, 3, 2), vec![30, 40, 100, 255]);
    assert_eq!(px(t1, 1, 1), vec![170, 20, 100, 255]); // global x = 17

    // Visible-tile selection: a view over the right edge needs only tile (1,0).
    let b = cog.merc_bounds();
    let view = cat_cog::MercBox { min_x: b.max_x - b.width() * 0.1, max_x: b.max_x, min_y: b.min_y, max_y: b.max_y };
    assert_eq!(cog.visible_tiles(0, &view), vec![(1, 0)]);
}
