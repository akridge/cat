# CAT Desktop (Rust) — Plan

Status: proposal · Scope: a desktop build of the **database (Oracle) version** of CAT, written in Rust, with the database and AI services remaining remote.

## TL;DR

- **Yes, it's possible**, and the current codebase is well shaped for it.
- Use **Tauri 2** (Rust core + the OS webview). Keep the existing ~31k lines of Leaflet/JS UI; rewrite the *backend* parts that run on the user's machine (tiles, COG conversion, exports, file projects, offline cache) in Rust.
- The database **stays remote behind an HTTPS API** — the desktop app should **not** open a direct Oracle connection.
- SAM3 segmentation stays a remote GPU service; the desktop calls it over HTTP.
- Do it as a strangler migration: a working desktop app in the first phase, more Rust each phase, never a big-bang rewrite.

## What CAT is today (relevant facts)

| Piece | Today | Size |
|---|---|---|
| Web UI | Static HTML + vanilla JS + Leaflet/Leaflet.draw, served by FastAPI | ~31.7k JS, ~8.6k HTML |
| API | FastAPI, ~140 routes (`cat/server.py`, `cat/api/*`) | ~13k Python |
| DB layer | Oracle via `oracledb`; geometry stored as GeoJSON in CLOBs (no SDO_GEOMETRY) | `cat/db/*`, `db_projects.py` (5k) |
| Tiles | TiTiler/rio-tiler over local or `gs://` COGs, LOCAL_CS → EPSG:4326 VRT override | `server.py` |
| Raster tools | hillshade, slope, zonal stats, thumbnails | `raster_tools.py`, `thumbnails.py` |
| Exports | GeoJSON, CSV, Shapefile, GeoPackage, KML, FileGDB | `exports.py`, `db_projects.py` |
| Auth | Cookie session (`cat_session`), bcrypt, sessions table | `cat/api/auth.py`, `cat/db/auth.py` |
| AI | SAM3 sidecar (torch/CUDA), HTTP | `services/sam3-service` |

Things that make this easier:

1. **The UI calls the API with relative, same-origin URLs** (`fetch('/api/...')`, `catFetch`). If the desktop app serves the UI from a local origin and answers `/api/*` there, the JS barely changes.
2. **Annotations already carry a `client_uuid`** (unique index in Oracle) and the UI already uses `/annotations/batch-sync` with retry logic. That is the foundation of offline sync.
3. Geometry is plain GeoJSON text, not Oracle spatial types, so a local SQLite cache can mirror it exactly.

## Target architecture

```
┌──────────────────────── CAT Desktop (Tauri 2) ────────────────────────┐
│  Webview: existing cat/web UI (Leaflet, annotation runtime, QC, etc.) │
│        │  same-origin http://127.0.0.1:<random>/...                   │
│  ┌─────▼──────────── embedded Rust server (axum) ─────────────────┐   │
│  │ /tiles, /info, /bounds, /statistics  → local tile engine (GDAL)│   │
│  │ /api/raster-tools/*, thumbnails      → local (GDAL/Rust)       │   │
│  │ /api/convert, file-mode projects     → local                   │   │
│  │ /api/exports (shp/gpkg/kml/gdb)      → local (GDAL)            │   │
│  │ /api/db/*, /api/auth/*, species, sites → proxy to remote API,  │   │
│  │                                         inject bearer token,   │   │
│  │                                         write-through cache    │   │
│  │ /api/segmentation/*                  → proxy to SAM3 service   │   │
│  └────────┬────────────────────────────────────────┬──────────────┘   │
│     SQLite cache + outbox (offline)          OS keychain (token)      │
└───────────┼────────────────────────────────────────┼──────────────────┘
            │ HTTPS                                  │
   ┌────────▼─────────┐   ┌──────────────┐   ┌──────▼───────────┐
   │ CAT API server    │──▶│ Oracle DB    │   │ SAM3 GPU service │
   │ (FastAPI today,   │   └──────────────┘   └──────────────────┘
   │  optional Rust    │   COGs: local disk, or GCS via range reads
   │  later)           │
   └───────────────────┘
```

### Key decisions

**1. Tauri 2, not a pure-Rust GUI (egui/iced/Slint).**
A native Rust UI means rewriting the map, drawing/editing tools, snapping, lasso, undo, QC tables, compare, reports — essentially all 31k lines of JS — and Rust has no Leaflet/OpenLayers equivalent of that maturity. Tauri gives a native installer, small binary (vs Electron), Rust everywhere off the UI thread, and lets the UI be replaced screen-by-screen later if ever wanted.

**2. Embedded loopback HTTP server rather than only Tauri `invoke` commands.**
Because the UI uses relative URLs and Leaflet loads tiles by URL, an axum server bound to `127.0.0.1:<random port>` lets the existing JS run unchanged. Lock it down: loopback only, random port, a per-launch secret (cookie or header) required on every request, and strict `Host`/`Origin` checks so other local web pages can't call it. Tauri `invoke` can still be used for desktop-only things (file dialogs, open folder, updater).

**3. Database stays remote behind an API — no direct Oracle from desktops.**
Direct Oracle would mean shipping Oracle Instant Client (ODPI-C, needed by the Rust `oracle` crate) to every machine, putting DB credentials on laptops, opening port 1521 through firewalls, and losing the server-side permission checks (collaborators, admin). Instead the desktop is a client of the CAT API:
- **Near term:** keep the existing FastAPI server for `/api/db/*`, auth, species, sites. It already exists and works.
- **Server change needed:** add bearer-token auth alongside the cookie (e.g. `POST /api/auth/token` returning a session token; `Authorization: Bearer` accepted wherever the cookie is). Store the token in the OS keychain (`keyring` crate). Require HTTPS.
- **Optional later:** rewrite the server in Rust (axum). Note the Oracle constraint: `sqlx` has no Oracle driver, so a Rust server would use the `oracle` crate (server-side Instant Client is fine) — or this becomes the moment to move to PostgreSQL/PostGIS, which `sqlx` supports well. Not required for the desktop app.

**4. Tiles rendered locally in Rust via GDAL.**
This is the main reason a desktop app is worth it: field users with local orthomosaics get tiles straight from disk with no server round-trip.
- Use the `gdal` crate (COG reads, overviews, warping to WebMercator, `/vsigs/` + `/vsicurl/` for GCS, VRT for the LOCAL_CS override — same tricks `server.py` uses today) and encode PNG/JPEG/WebP with the `image` crate.
- Keep TiTiler-compatible URL shapes (`/tiles/WebMercatorQuad/{z}/{x}/{y}?url=...`, `/info`, `/bounds`, `/statistics`) so the JS doesn't change.
- In-memory LRU tile cache (`moka`) + open-dataset pool per thread (GDAL handles aren't `Sync`; use a `spawn_blocking` pool of per-thread datasets).
- Pure-Rust alternative to watch: `async-tiff` / `geotiff` crates. Smaller to ship, but reprojection, overview selection and resampling would need to be written; revisit after the spike.

**5. SAM3 stays remote.** torch + CUDA doesn't belong in a desktop installer. The desktop proxies `/api/segmentation/*` to a configured service URL (or the CAT server forwards it). A later option is a small local model via `ort` (ONNX Runtime) for CPU-only point prompts.

**6. Offline-first annotation editing (phase 3).**
SQLite (`rusqlite` or `sqlx`) mirrors the tables the annotation screen needs: projects, assets, annotations, overlay layers/features, species, field options, sites. Writes go to the local DB and an **outbox**; a background task drains the outbox with `/annotations/batch-sync`, keyed by `client_uuid` so retries are idempotent. Conflicts: last-write-wins per annotation with history preserved server-side (the history table already exists); show a conflict badge when the server copy changed since the local base version.

## Rust crates (initial picks)

| Need | Crate |
|---|---|
| App shell, installer, updater | `tauri` 2, `tauri-plugin-updater`, `tauri-plugin-dialog`, `tauri-plugin-shell` |
| Local HTTP | `axum`, `tower-http` (static files, CORS off), `tokio` |
| Proxy to remote API | `reqwest` (rustls), `hyper` |
| Raster / COG / exports | `gdal` (bundled build), `image` |
| Vector geometry | `geo`, `geojson`, `proj` (or via GDAL) |
| Local DB / cache | `rusqlite` (bundled) or `sqlx` (sqlite) |
| Caching | `moka` |
| Secrets | `keyring` |
| Config / logging | `serde`, `figment` or `config`, `tracing`, `tracing-appender` |
| GCS (non-GDAL paths) | `object_store` |

## Phased plan

### Phase 0 — Spike (≈2–3 weeks)
Goal: prove the two risky parts before committing.
- Tauri app that loads `cat/web` from an embedded axum server.
- Rust tile endpoint for one local COG and one `gs://` COG, incl. a LOCAL_CS file. Benchmark against TiTiler (p50/p95 tile latency, CPU, memory) on Windows and macOS.
- Build GDAL into the bundle on **Windows, macOS (arm64 + x64), Linux**. This is the biggest packaging risk; decide here between a bundled/static GDAL build vs. pure-Rust TIFF reading.
- **Exit criteria:** tiles at least as fast as TiTiler; signed installer < ~150 MB that runs on a clean machine without admin rights or Python.

### Phase 1 — Desktop client for the DB version (≈4–6 weeks)
- Rust proxy for `/api/db/*`, `/api/auth/*`, species, sites, field options, segmentation → remote CAT server.
- Server: add token auth endpoint + bearer support; enforce HTTPS; tighten CORS (no `*` with credentials).
- Desktop login screen → token → keychain; "Server URL" setting; sign-out.
- Local tiles for COGs referenced by project assets (local path or `gs://`).
- Packaging: MSI/NSIS (Windows), DMG (macOS, notarized), AppImage/deb; auto-update channel; code signing.
- **Result:** a usable desktop CAT on the Oracle backend, with no Python on the client.

### Phase 2 — Move local work to Rust (≈4–6 weeks)
- COG conversion (`/api/convert`, batch) via GDAL COG driver with progress events to the UI.
- Exports: GeoJSON, CSV, Shapefile, GeoPackage, KML, FileGDB (GDAL `OpenFileGDB` writer) — generated on the desktop from API data, saved via native file dialog.
- Raster tools: hillshade, slope (GDAL DEM processing), zonal stats, thumbnails.
- Shapefile import / overlay upload parsing locally, then pushed to the API.
- File-mode (JSON) projects reimplemented in Rust so one app covers both modes.

### Phase 3 — Offline & sync (≈4–8 weeks)
- SQLite mirror + outbox; "Make available offline" per project (downloads project, annotations, overlay layers, reference data; optionally copies COGs locally).
- Sync status UI, conflict badges, retry/backoff, background sync when online.
- Annotation timer/sessions recorded locally and uploaded.

### Phase 4 — Optional
- Rust rewrite of the remote API server (axum), likely with a PostgreSQL/PostGIS migration.
- Native Rust replacements for performance-critical UI pieces (e.g. a WebGL/wgpu annotation layer for very large polygon counts).
- Local CPU segmentation via ONNX.

Estimates assume one experienced developer; Phase 0 results should reset them.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Bundling GDAL per-OS (esp. Windows) | Solve in Phase 0; use a vendored/static GDAL build in CI; fallback: pure-Rust TIFF reader for tiles, GDAL only for conversion/exports |
| Webview differences (WebView2 vs WKWebView vs WebKitGTK) | Test Leaflet heavy pages on all three early; Linux WebKitGTK is the slowest — consider it lower priority |
| Large COGs over GCS from field networks | Local tile cache on disk; "download COG for offline" option; range-merge settings like today's GDAL env |
| Government IT: code signing, no admin installs, managed endpoints | Per-user installers (NSIS/MSI per-user), signed builds, configurable proxy (`HTTPS_PROXY` honored by `reqwest`) |
| Oracle API surface is big (`db_projects.py` ~5k lines) | Don't rewrite it for the desktop; proxy it. Only cache what the annotation screen needs |
| Security of the local server | Loopback-only, random port, per-launch secret, Host/Origin checks, no CORS |
| Two backends drifting (Python web + Rust desktop) | Keep TiTiler-compatible routes; contract tests that run the same requests against both |

## Proposed repo layout

```
desktop/
  src-tauri/            # Tauri app: windows, updater, menus, invoke commands
  crates/
    cat-core/           # models (Project, Annotation, OverlayLayer...), GeoJSON types
    cat-tiles/          # COG reading, tile rendering, LOCAL_CS handling
    cat-gis/            # COG conversion, exports, raster tools
    cat-local-server/   # axum router: local routes + proxy + auth injection
    cat-sync/           # SQLite cache, outbox, sync engine
cat/web/                # unchanged; served by both FastAPI and the desktop app
```

## Open questions

1. Who uses the desktop build — field teams on laptops (offline matters → Phase 3 earlier) or office analysts on network (Phase 1 is enough)?
2. Where do COGs live for desktop users: local disks, GCS, or both?
3. Is the Oracle backend fixed, or is PostgreSQL/PostGIS an option for the server long term?
4. Required platforms: Windows only, or macOS/Linux too?
5. Will the CAT API server be reachable from outside Cloud Workstations (public HTTPS / VPN)? The desktop app needs a stable HTTPS endpoint.
6. Any agency requirements on code signing, installers, or credential storage?
