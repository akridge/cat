# CAT Desktop (Rust) — Plan

Status: proposal (rev 2) · Scope: a **portable Windows** desktop app for the **database (Oracle) version** of CAT, written in Rust. Oracle and the AI service stay remote behind the CAT server.

## Requirements (confirmed)

| | |
|---|---|
| Platform | **Windows only** (10/11, x64) |
| Distribution | **Portable** — runs from a folder or USB stick, no installer, no admin rights, no Python |
| Database | **Oracle, long term**; will later point at the **production** database. Desktop never talks to Oracle directly |
| Connectivity | Users are **online**; the CAT server is reachable over HTTPS |
| Imagery | All COGs are in an **open (public) GCS bucket** |
| GDAL | **Not required** on the client — annotation only needs to *display* COGs |
| UI | Native annotation is desirable if feasible |

## TL;DR

- **Rust core, no GDAL, no Python.** The app reads COGs straight from the public bucket with HTTP range requests and draws tiles itself in pure Rust.
- **The CAT server stays the gateway to Oracle.** Pointing at prod later is a server config change; the desktop only stores a server URL and a login token.
- **Two-stage UI:**
  - **Stage 1:** a Tauri portable `.exe` that reuses the existing web annotation UI. It ships in weeks, not months.
  - **Stage 2:** a native Rust annotation workstation (egui + GPU), built on the same Rust crates, which replaces the webview annotation screen once it reaches feature parity.
- Things that need GDAL or heavy compute (FileGDB/GPKG export, COG conversion, hillshade/slope, SAM3) stay on the server, which already has endpoints for them.

## Architecture

```
┌──────────── CAT.exe (portable folder) ─────────────────────────────┐
│  UI  ── Stage 1: WebView2 + existing cat/web (Leaflet)             │
│      ── Stage 2: native egui annotation canvas (wgpu)              │
│                                                                    │
│  Rust crates (shared by both stages)                               │
│   cat-cog    COG reader: HTTP range reads from public GCS,         │
│              IFD/overview parsing, tile decode (JPEG/ZSTD/LZW/     │
│              DEFLATE), reprojection → WebMercator, LRU cache       │
│   cat-api    typed client for the CAT server (/api/db/*, auth,     │
│              species, sites, field options, segmentation, exports)│
│   cat-core   Project / Annotation / OverlayLayer models, GeoJSON   │
│   cat-local  Stage 1 only: loopback axum server that serves        │
│              cat/web, local /tiles, and proxies /api/* to server   │
│                                                                    │
│  data\  settings.toml, tile cache, logs (next to the exe)          │
│  token → Windows Credential Manager (per user)                     │
└───────┬─────────────────────────────────────────┬──────────────────┘
        │ HTTPS (bearer token)                    │ HTTPS range GETs (anonymous)
┌───────▼──────────────┐                 ┌────────▼────────────────────┐
│ CAT server (FastAPI) │──▶ Oracle (dev → prod)  │ storage.googleapis.com │
│ auth, projects,      │──▶ SAM3 GPU service     │ public COG bucket      │
│ annotations, exports │                 └─────────────────────────────┘
└──────────────────────┘
```

### Key decisions

**1. No GDAL — a pure-Rust COG reader (`cat-cog`).**
Displaying a COG only needs these steps:
1. Read the TIFF header and IFDs, which hold the tile offsets per overview level.
2. Fetch the needed internal tiles with HTTP `Range` requests from `https://storage.googleapis.com/<bucket>/<path>`. The bucket is public, so no credentials are needed.
3. Decode the tiles.
4. Resample them into 256×256 WebMercator tiles.

Building blocks:
- `async-tiff` (Development Seed's COG reader) or `tiff` for parsing.
- `reqwest` with rustls and HTTP/2 to fetch byte ranges, merging adjacent ranges.
- Decoders: `zune-jpeg` (JPEG), `zstd` (statically linked C, fine for a portable exe) or pure-Rust `ruzstd`, `weezl` (LZW), `miniz_oxide` (DEFLATE), plus the TIFF horizontal predictor. These cover every profile `make_cog.py` produces: zstd for RGB, LZW otherwise, optional JPEG/deflate.
- `proj4rs` (pure Rust) for UTM/geographic → WebMercator. LOCAL_CS rasters get the same "treat as EPSG:4326" override the server applies today.
- `moka` in-memory LRU, plus an optional on-disk tile cache in `data\cache`.
- `image` / `png` / `webp` to encode tiles for the webview in Stage 1. Stage 2 uploads raw RGBA straight to the GPU.

Benefits: tiles no longer go through the server, so server load drops, and a 10–20 MB exe has no native DLL chain to ship.

**2. Oracle stays behind the server — including prod.**
The desktop gets no Oracle client, no database credentials and no port 1521. Moving to the prod database later is a server-side change (DSN and credentials in the server's env). The desktop just picks a server profile, for example `Dev` and `Prod` entries in `settings.toml`.

Server changes needed:
- A bearer-token login, e.g. `POST /api/auth/token` → session token, with `Authorization: Bearer` accepted wherever the `cat_session` cookie is today. This reuses the existing `cat_sessions` table.
- HTTPS everywhere; tighten CORS (no `*` with credentials).
- An `/api/version` handshake so the desktop can warn when the client and server versions are incompatible. The endpoint already exists; extend it with an API version.

A Rust rewrite of the server isn't needed. If it happens later, it would use the `oracle` crate server-side, since `sqlx` has no Oracle support.

**3. Portable Windows packaging.**
- Ship as a **zip of a folder**:
  - `CAT.exe`
  - `data\` with settings, cache and logs
  - optionally `WebView2\` (Stage 1 only, see below)

  No installer, no registry writes, no admin rights.
- **WebView2 (Stage 1):** the Evergreen runtime comes preinstalled on Windows 10/11, which is enough for most machines. For locked-down machines, Tauri's "fixed version" mode can load a WebView2 runtime shipped inside the folder (≈150–200 MB).
- **Stage 2 is fully self-contained:** egui/wgpu draws with DirectX 12, falling back to Vulkan or GL, so the exe is about 15–25 MB with nothing else to install.
- **Updates:** the app checks the server for a newer version, downloads `CAT.exe.new` and swaps it on restart. That works without installers or admin rights.
- **Code-sign** `CAT.exe` (Authenticode) so SmartScreen and endpoint protection don't block it. This is critical in government environments.
- Statically link the MSVC runtime (`+crt-static`) so no VC++ redistributable is needed.

**4. Online-first, with a small safety net.**
Users are online, so a full offline/sync engine is out of scope. Keep a local **outbox**: unsent saves are written to `data\outbox.sqlite` and retried with the existing `/annotations/batch-sync` and `client_uuid` idempotency. A dropped Wi-Fi connection or a server restart then never loses work. The existing JS autosave already does this in memory; this makes it survive an app crash.

**5. What stays server-side.**
| Feature | Where | Why |
|---|---|---|
| FileGDB / GeoPackage export | server (existing `/projects/export/gdb`, `/gpkg`) | needs GDAL |
| GeoJSON / CSV / Shapefile / KML export | **desktop** (`geojson`, `csv`, `shapefile`, `kml` crates) | simple, pure Rust, saves round-trips |
| COG conversion | server / CLI (`make_cog.py`) | COGs are already in the bucket |
| Hillshade, slope, zonal stats | server for now; pure Rust later if wanted | GDAL today |
| SAM3 segmentation | server → GPU service | torch/CUDA |
| Thumbnails | desktop (read the COG's smallest overview) | cheap with `cat-cog` |
| Admin, user management, reports, QC, activity | server pages, opened in the browser (Stage 2) | infrequent; no need to rebuild natively |

## Stage 1 — Portable Tauri app with the existing UI

The existing UI calls the API with relative, same-origin URLs (`fetch('/api/...')`, `catFetch`) and loads tiles by URL. A loopback axum server inside the exe (`cat-local`) therefore lets that JS run nearly unchanged:

- `/` and static files → `cat/web` embedded in the exe (`include_dir` / `rust-embed`).
- `/tiles/...`, `/info`, `/bounds`, `/statistics` → `cat-cog`, using **TiTiler-compatible URL shapes** so the JS is untouched.
- `/api/*` → proxied to the configured CAT server, with the bearer token injected.
- Security:
  - bind to `127.0.0.1` on a random port;
  - require a per-launch secret on every request;
  - check `Host`/`Origin`;
  - no CORS.

## Stage 2 — Native annotation workstation (egui)

**Feasibility:** yes. It is the largest piece of work: the web annotation runtime is about 27k lines of JS plus a 1.2k-line page. Most of that is UI glue, though, and the core is a well-defined set of features:

| Area | Native implementation |
|---|---|
| Map canvas, pan/zoom (up to the current 2000× deep zoom) | custom egui widget on wgpu; tiles from `cat-cog` uploaded as textures |
| Annotation rendering (thousands of polygons) | GPU tessellation (`lyon`) — faster than Leaflet SVG/canvas at high counts |
| Draw point / line / rectangle / polygon, lasso, vertex edit, snapping | `geo` crate for geometry ops and hit-testing; R-tree (`rstar`) for snapping and selection |
| Undo/redo, history restore | command stack in Rust; server history endpoints |
| Attribute form, species autocomplete, field options | egui forms; species/field options cached from the server |
| Overlay layers (shapefile, transects), symbology, layer panel | `cat-api` + `geojson` |
| Measure, bookmarks, timer/sessions, compare (split view) | native |
| SAM3 box/point/text prompts | call server, draw returned masks |
| Keyboard-first workflow (Ctrl+S, hotkeys) | native shortcuts |

Approach: build it inside the same exe as a second window or mode, behind a "Native annotation (beta)" toggle. It reuses `cat-cog`, `cat-api` and `cat-core` unchanged, so the work goes into the UI rather than the plumbing. Once it reaches parity, drop WebView2 and `cat-local`, and open the infrequent pages (admin, reports, QC, activity) in the user's browser on the server.

## Phased plan

| Phase | Duration (1 dev) | Deliverable |
|---|---|---|
| **0 — Spike** | 2–3 wk | `cat-cog` renders tiles from a real bucket COG (zstd RGB, LZW DEM, a LOCAL_CS file). Benchmark against TiTiler. Tauri portable exe opens the existing UI against the dev server. **Exit:** tiles at least as fast as TiTiler; the exe runs from a USB stick on a clean, non-admin Windows account |
| **1 — Portable desktop v1** | 4–6 wk | Token auth on the server, login screen, server profiles (dev/prod), API proxy, local tiles, thumbnails, outbox, self-update, signed zip release |
| **2 — Local exports & polish** | 2–3 wk | GeoJSON/CSV/SHP/KML export on the desktop; GDB/GPKG via server; native save dialogs; logging and crash reports |
| **3 — Native annotation (beta)** | 10–16 wk | egui map canvas, draw/edit tools, attribute form, layers, undo, SAM3, compare; side by side with the web UI |
| **4 — Native by default** | 2–4 wk | Parity sign-off with annotators, WebView2 dependency dropped, admin/report pages open in the browser |

Phase 0 results should reset these estimates. Phases 1–2 alone give a usable, portable desktop app; Phase 3 is the native upgrade.

## Proposed repo layout

```
desktop/
  Cargo.toml                 # workspace
  crates/
    cat-core/                # models, GeoJSON types
    cat-cog/                 # pure-Rust COG reader + tile renderer
    cat-api/                 # CAT server client (auth, projects, annotations…)
    cat-local/               # Stage 1 loopback server + proxy
    cat-native/              # Stage 2 egui annotation UI
  app/                       # CAT.exe (Tauri in Stage 1; eframe in Stage 2)
cat/web/                     # unchanged; served by FastAPI and embedded in CAT.exe
```

CI: GitHub Actions `windows-latest` builds `CAT.exe` (static CRT), signs it and publishes `CAT-<version>-win64-portable.zip` as a release asset.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Unsigned portable exe blocked by SmartScreen or agency endpoint protection | Authenticode signing from day one; check the agency's software-approval process early |
| WebView2 missing or locked down on some machines (Stage 1) | Fixed-version WebView2 in the folder; Stage 2 removes the dependency |
| COG variants the Rust reader doesn't handle (odd compression, band layout, nodata/alpha, CRS) | Inventory the bucket in Phase 0 (script listing compression/CRS/bands per COG); fall back to server tiles for unsupported files |
| Tile latency from the public bucket on slow networks | Merged range reads, HTTP/2, prefetch of neighboring tiles, disk cache |
| Native UI parity takes longer than planned | Ship Stage 1 first; keep the web UI as the fallback until annotators sign off |
| Dev → prod Oracle cutover | Desktop unaffected (server-side change); server profiles in settings; version handshake |

## Open questions

1. Is there an agency code-signing certificate available, or does one need to be requested?
2. Can WebView2 be assumed on target machines (Windows 10/11 with Edge), or must the portable zip carry it?
3. Should the desktop also open local `.tif` files (not just bucket COGs) for ad-hoc viewing? `cat-cog` can read local files with the same code.
4. Which features have the highest priority for the native annotation screen (for ordering Phase 3)?
