# CAT Desktop (Rust)

Native, portable Windows annotation workstation for CAT. See
[`docs/rust-desktop-plan.md`](../docs/rust-desktop-plan.md) for the full plan.

**Status: Phase 1 in progress.** Native map viewer and annotation editor:
draw/edit points, lines, rectangles and polygons; attribute form with species
autocomplete; undo/redo; table; GeoJSON/CSV export. Works in two modes:

- **Server mode** — sign in to a CAT server (Dev/Prod profiles), open a
  project, annotations sync to the Oracle database in the background
  (offline queue, automatic retry, conflict resolution, viewer = read-only).
- **Local mode** — annotate any bucket mosaic without a server; saved to the
  portable `data\` folder.

## Layout

| Path | What |
|---|---|
| `crates/cat-cog` | Pure-Rust COG reader (no GDAL): TIFF/BigTIFF + GeoTIFF parsing, HTTP range reads with request merging, ZSTD/LZW/DEFLATE decode, background detection, WebMercator placement |
| `crates/cat-core` | Annotation model (server-compatible GeoJSON properties), form fields and option lists, bundled coral species search, geometry/measurements, undo history, file store and CSV export |
| `crates/cat-api` | CAT server client (cookie session login, projects, snapshot, batch-sync/delete, species, field options) and the offline-safe sync engine (fingerprints, journal merge, conflicts) |
| `app` | `CAT.exe` — egui/eframe app: map canvas (GPU), editor (tools, snapping, vertex editing), attribute form, table, bucket browser, layers, settings |

## Where data goes

`data\` next to `CAT.exe` (falls back to `%LOCALAPPDATA%\CAT` if not
writable; override with `CAT_DATA_DIR`):

- `settings.json` — sticky form values, recent species, display options, last session
- `annotations\<mosaic>.geojson` — local mode: one file per mosaic, saved automatically (previous version kept as `.bak`)
- `server\<host>\project-<id>.json` — server mode: journal of the project's annotations and sync state, so unsynced work survives crashes and network loss
- Sign-in sessions are kept in Windows Credential Manager ("CAT desktop"), never in these files.

## Build & run

```bash
cd desktop
cargo run --release -p cat-desktop                    # opens the bucket browser
cargo run --release -p cat-desktop -- gs://nmfs_odp_pifsc/PIFSC/ESD/ARP/StRS_Sites_Products/orthomosaic_cog/2026/2026_KIN-5151_mos_cog.tif --zoom 40
```

Windows builds come from GitHub Actions (`.github/workflows/desktop.yml`) as
`CAT-<version>-<sha>-win64-portable.zip`.

`CAT_PERF=1` logs CPU time per frame to stderr (2,000 visible polygons
measure about 5–6 ms per frame in a release build).

`CAT_RENDERER=glow` switches from the default wgpu renderer (DirectX 12 on
Windows) to OpenGL.

## Developer CLI

```bash
cargo run --release -p cat-cog --bin catcog -- inspect  <gs://…tif>   # header, levels, CRS, background
cargo run --release -p cat-cog --bin catcog -- preview  <gs://…tif> 99 out.png   # stitch the coarsest level
cargo run --release -p cat-cog --bin catcog -- bench    <gs://…tif>   # timing for a 1920x1080 view
```

## Tests

`cargo test -p cat-cog -p cat-core` runs offline. cat-cog's test It builds a synthetic georeferenced
tiled TIFF in memory and reads it back through the same code path.

## Testing against a real CAT server

See [`DEV-SERVER.md`](DEV-SERVER.md) to run Oracle Free + the CAT server
locally, then:

```bash
CAT_TEST_SERVER=http://127.0.0.1:8000 CAT_TEST_USER=admin CAT_TEST_PASSWORD=… \
  cargo test -p cat-api --test live_server
```
