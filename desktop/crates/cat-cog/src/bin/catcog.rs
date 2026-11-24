//! Developer CLI for cat-cog: `catcog inspect <gs://…|https://…>`.

use std::sync::Arc;

use cat_cog::source::{http_client, HttpSource};
use cat_cog::{Cog, DecodeOptions, MercBox};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: catcog inspect <uri>\n       catcog preview <uri> <level> <out.png>\n       catcog bench <uri>");
        std::process::exit(2);
    }
    let client = http_client();
    let src = Arc::new(HttpSource::new(client, &args[2]));
    let t = std::time::Instant::now();
    let cog = match Cog::open(src.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    match args[1].as_str() {
        "inspect" => inspect(&cog, t.elapsed()),
        "preview" => preview(&cog, args[3].parse().unwrap_or(usize::MAX), &args[4]),
        "bench" => bench(&cog, t.elapsed()),
        other => eprintln!("unknown command {other}"),
    }
}

fn inspect(cog: &Cog, open_time: std::time::Duration) {
    println!("url        {}", cog.source.location());
    println!("opened in  {:?} ({})", open_time, if cog.bigtiff { "BigTIFF" } else { "TIFF" });
    println!("size       {} x {}", cog.width(), cog.height());
    println!("crs        {:?}", cog.crs);
    println!("transform  {:?}", cog.geotransform.0);
    println!("pixel      {:?}", cog.pixel_size());
    println!("bounds     {:?}", cog.native_bounds());
    println!("nodata     {:?}", cog.nodata);
    println!("background {:?}", cog.background);
    let l = &cog.levels[0];
    println!(
        "bands      {} x {}-bit (format {}), photometric {}, planar {}, extra {:?}",
        l.samples, l.bits, l.sample_format, l.photometric, l.planar, l.extra_samples
    );
    println!("compress   {} ({}), predictor {}", l.compression_name(), l.compression, l.predictor);
    println!("levels:");
    for (i, l) in cog.levels.iter().enumerate() {
        let total: u64 = l.byte_counts.iter().sum();
        println!(
            "  [{i}] {:>6} x {:<6} tile {}x{}  {}x{} tiles  1/{:.0}  {:.1} MB  mask={}",
            l.width, l.height, l.tile_w, l.tile_h, l.tiles_across(), l.tiles_down(),
            l.decimation, total as f64 / 1e6, cog.mask_for(i).is_some()
        );
    }
    if !cog.masks.is_empty() {
        let m = &cog.masks[0];
        println!("mask       {} ({} levels), {}-bit", m.compression_name(), cog.masks.len(), m.bits);
    }
    if let Some(md) = &cog.gdal_metadata {
        println!("gdal md    {}", md.chars().take(400).collect::<String>());
    }
}

/// Stitch every tile of one level into a PNG (visual check of the decoder).
fn preview(cog: &Cog, level: usize, out: &str) {
    let level = level.min(cog.levels.len() - 1);
    let l = &cog.levels[level];
    let tiles: Vec<(u32, u32)> =
        (0..l.tiles_down()).flat_map(|ty| (0..l.tiles_across()).map(move |tx| (tx, ty))).collect();
    let t = std::time::Instant::now();
    let res = cog.fetch_tiles(level, &tiles, DecodeOptions { background_transparent: true });
    println!("level {level}: {} tiles in {:?}", tiles.len(), t.elapsed());
    let (w, h) = (l.width as usize, l.height as usize);
    let mut img = vec![0u8; w * h * 4];
    for ((tx, ty), r) in res {
        match r {
            Ok(Some(tile)) => {
                for y in 0..tile.h as usize {
                    let dy = ty as usize * l.tile_h as usize + y;
                    let dx = tx as usize * l.tile_w as usize;
                    let src = &tile.rgba[y * tile.w as usize * 4..(y + 1) * tile.w as usize * 4];
                    img[(dy * w + dx) * 4..(dy * w + dx) * 4 + src.len()].copy_from_slice(src);
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("tile {tx},{ty}: {e}"),
        }
    }
    let f = std::fs::File::create(out).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&img).unwrap();
    println!("wrote {out} ({w}x{h})");
}

/// Simulate opening a project: coarse overview, then a 1920x1080 view at
/// full resolution in the middle of the mosaic.
fn bench(cog: &Cog, open: std::time::Duration) {
    println!("open (headers)            {:?}", open);
    let last = cog.levels.len() - 1;
    let l = &cog.levels[last];
    let tiles: Vec<(u32, u32)> =
        (0..l.tiles_down()).flat_map(|ty| (0..l.tiles_across()).map(move |tx| (tx, ty))).collect();
    let t = std::time::Instant::now();
    let r = cog.fetch_tiles(last, &tiles, DecodeOptions::default());
    println!("overview L{last} ({} tiles)      {:?}", r.len(), t.elapsed());

    let b = cog.merc_bounds();
    let (cx, cy) = b.center();
    let px = cog.merc_pixel_size();
    for (label, scale) in [("full-res view (L0)", 1.0), ("zoomed-out view (~L2)", 4.0)] {
        let m = px * scale;
        let view = MercBox { min_x: cx - 960.0 * m, max_x: cx + 960.0 * m, min_y: cy - 540.0 * m, max_y: cy + 540.0 * m };
        let level = cog.choose_level(m);
        let tiles = cog.visible_tiles(level, &view);
        let t = std::time::Instant::now();
        let r = cog.fetch_tiles(level, &tiles, DecodeOptions::default());
        let errs = r.iter().filter(|x| x.1.is_err()).count();
        println!("{label:<25} L{level} {} tiles  {:?}  errors={errs}", tiles.len(), t.elapsed());
    }
}
