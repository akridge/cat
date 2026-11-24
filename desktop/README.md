# CAT Desktop (Rust)

Native, portable Windows annotation workstation for CAT. See
[`docs/rust-desktop-plan.md`](../docs/rust-desktop-plan.md) for the full plan.

**Status: Phase 0 (spike).** A native map viewer that streams orthomosaic
COGs straight from the public bucket. There is no annotation yet.

## Layout

| Path | What |
|---|---|
| `crates/cat-cog` | Pure-Rust COG reader (no GDAL): TIFF/BigTIFF + GeoTIFF parsing, HTTP range reads with request merging, ZSTD/LZW/DEFLATE decode, background detection, WebMercator placement |
| `app` | `CAT.exe` — egui/eframe app: map canvas (GPU), bucket browser, layers |

## Build & run

```bash
cd desktop
cargo run --release -p cat-desktop                    # opens the bucket browser
cargo run --release -p cat-desktop -- gs://nmfs_odp_pifsc/PIFSC/ESD/ARP/StRS_Sites_Products/orthomosaic_cog/2026/2026_KIN-5151_mos_cog.tif --zoom 40
```

Windows builds come from GitHub Actions (`.github/workflows/desktop.yml`) as
`CAT-<version>-<sha>-win64-portable.zip`.

`CAT_RENDERER=glow` switches from the default wgpu renderer (DirectX 12 on
Windows) to OpenGL.

## Developer CLI

```bash
cargo run --release -p cat-cog --bin catcog -- inspect  <gs://…tif>   # header, levels, CRS, background
cargo run --release -p cat-cog --bin catcog -- preview  <gs://…tif> 99 out.png   # stitch the coarsest level
cargo run --release -p cat-cog --bin catcog -- bench    <gs://…tif>   # timing for a 1920x1080 view
```

## Tests

`cargo test -p cat-cog` runs offline. It builds a synthetic georeferenced
tiled TIFF in memory and reads it back through the same code path.
