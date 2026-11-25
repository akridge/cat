# CAT Desktop (Rust) — Plan

Status: proposal (rev 3) · Scope: a **native Rust, portable Windows** annotation workstation for the **Oracle (database) version** of CAT. Oracle and SAM3 stay remote behind the CAT server; all imagery is read from the public COG bucket.

## Requirements (confirmed)

| | |
|---|---|
| Platform | **Windows 10/11 x64 only** |
| Distribution | **Portable** — unzip and run, no installer, no admin rights, no Python. No code signing required |
| UI | **Native Rust**, focused on the **annotation workflow** |
| Priority | **Annotation features + a smooth, easy-to-use experience** |
| Database | **Oracle, long term**, later the **production** DB — always behind the CAT server |
| Connectivity | Users are **online**; CAT server reachable over HTTPS |
| Imagery | COGs on the **open GCS bucket only** — no local files |
| GDAL | **Not needed** on the client |
| Browser | Chrome is the default browser. Secondary web pages open there |

## Progress

**Phase 0 — under way** (`desktop/`)
- `cat-cog`: a pure-Rust COG reader. It parses TIFF/BigTIFF and GeoTIFF, merges neighbouring HTTP range reads, decodes ZSTD/LZW/DEFLATE in parallel, and detects the mosaic background colour.
- `CAT.exe`: a native egui map canvas.
  - Pan and zoom to 64× past native resolution.
  - Overviews are chosen by zoom level, with coarse tiles shown underneath while sharp ones load.
  - Bucket browser, layers with opacity, scale bar, coordinate readout.
- Windows portable-zip build in GitHub Actions; offline unit tests.
- **Bucket survey (2026, all 357 files, 454 GB):** every file is RGB 8-bit, ZSTD, 512 px tiles, 6 overview levels and EPSG:4326, at ~0.5 mm/pixel. None has a mask or nodata; the area outside the reef is pure white, with black wedges on rotated exports. All 357 open without errors.
- **Design change from the survey:** the app draws each COG internal tile as a GPU-textured quad instead of resampling into WebMercator tiles on the CPU. That gives pixel-exact display at any zoom, and UTM support (proj4rs) is deferred until a non-4326 COG appears.
- **Timings from a cloud container behind a proxy:** headers 0.3–0.6 s; coarse overview 0.15 s; a 1920×1080 view at full resolution (15 tiles) 0.55 s. These still need measuring on agency Windows laptops.

**Phase 1 — core annotation, largely done** (`desktop/crates/cat-core`, `cat-api`, `app`)
- **Editor:**
  - Point, line, rectangle and polygon tools; select; vertex drag/insert/remove; move.
  - Snapping to vertices, live area/length while drawing, undo/redo for every edit, species labels.
- **Attribute form:**
  - Same fields and value types as the web form.
  - Keyboard-first species autocomplete (type → Enter → Enter).
  - Sticky survey fields; year and site come from the mosaic or project.
  - "Same as last" and recent species.
- **Table:** sort, filter, click to zoom; complete / needs-species counts.
- **Server mode against the unchanged CAT server**, tested end to end with Oracle Free:
  - Cookie-session sign-in with Dev/Prod profiles; the session is kept in Windows Credential Manager.
  - Project list; opening a project loads its bucket COG assets and annotations.
  - Background sync through `batch-sync`/`batch-delete`.
  - Offline queue with automatic retry.
  - A crash-safe journal, merged with the server state on reopen.
  - Conflict detection with Keep mine / Use server's.
  - Viewer role is read-only.
- **Local mode:** annotate any bucket mosaic without a server; autosave to the portable `data\` folder.
- GeoJSON/CSV export; the last session is restored on start.

## TL;DR

- **One small `CAT.exe` (≈15–25 MB), fully native.** It uses egui with a GPU map canvas (wgpu → DirectX 12). No webview, no runtime to install, no DLLs to ship.
- **Native-first, with no Tauri/webview stage.** The browser engine isn't a constraint, so the effort goes straight into the native annotation screen.
- **Scope is the annotation workstation.** Admin, user management, reports, QC, activity, sites and export pages stay on the web server and open in Chrome from the app's menu. The current web annotation screen stays available as a fallback until the native one reaches parity.
- **Pure-Rust COG reader** streams tiles straight from the public bucket. The CAT server handles only data (projects, annotations, auth) and SAM3.
- **Oracle stays behind the CAT server.** Switching to prod is a server change; the desktop just has `Dev`/`Prod` server profiles.

## Architecture

```
┌──────────────── CAT.exe (portable folder) ─────────────────────────┐
│  app/        eframe window, menus, docking panels, shortcuts       │
│  cat-map     GPU map canvas: tile layer, annotation layer (lyon    │
│              tessellation), overlay layers, deep zoom, hit-testing │
│  cat-edit    tools: point/line/rect/polygon, vertex edit, lasso,   │
│              snapping, measure, bulk draw; undo/redo command stack │
│  cat-cog     pure-Rust COG reader: HTTP range reads from public    │
│              GCS, overviews, JPEG/ZSTD/LZW/DEFLATE decode,         │
│              reprojection (proj4rs), memory + disk tile cache      │
│  cat-api     typed CAT server client; save queue (outbox) with     │
│              client_uuid idempotency; version handshake            │
│  cat-core    Project / Annotation / OverlayLayer / Species models  │
│                                                                    │
│  data\  settings.toml, tile cache, outbox.sqlite, logs, crash dumps│
│  login token → Windows Credential Manager                          │
└───────┬───────────────────────────────────────┬────────────────────┘
        │ HTTPS (bearer token)                  │ HTTPS range GETs (anonymous)
┌───────▼──────────────┐               ┌────────▼───────────────────┐
│ CAT server (FastAPI) │──▶ Oracle (dev → prod)   storage.googleapis.com │
│ auth, projects,      │──▶ SAM3 GPU service      public COG bucket      │
│ annotations, exports │               └────────────────────────────┘
│ + web pages (Chrome) │
└──────────────────────┘
```

### Technology choices

| Need | Choice | Why |
|---|---|---|
| Window / UI | `eframe` + `egui`, `egui_extras` (tables), `egui_dock` (dockable panels) | Mature, fast and easy to iterate on; good fit for tool-heavy UIs; one exe |
| Map rendering | custom wgpu paint callback inside egui | Full control of deep zoom and thousands of polygons at 60 fps |
| Geometry | `geo`, `geojson`, `lyon` (tessellation), `rstar` (spatial index) | Hit-testing, snapping, lasso, area/length |
| Projection | `proj4rs` (pure Rust) | WebMercator display, UTM-aware measurements |
| COG | `async-tiff` / `tiff`, `reqwest` (rustls, HTTP/2), `zune-jpeg`, `zstd`, `weezl`, `miniz_oxide` | Covers every `make_cog.py` profile |
| Async / cache | `tokio`, `moka`, disk cache in `data\cache` | Smooth panning, instant revisit |
| Local state | `rusqlite` (bundled) for the outbox and caches | Saves survive crashes |
| Secrets | `keyring` (Windows Credential Manager) | No plaintext token |
| Logging | `tracing`, `tracing-appender`; panic hook → crash report | Supportability |
| Build | `+crt-static`, GitHub Actions `windows-latest` → `CAT-<ver>-win64-portable.zip` | No VC++ redistributable needed |

Two precision details to get right from day one:
- **Coordinates as f64**, rendered relative to the camera center. The GPU's f32 math would otherwise make vertices jitter at the 2000× zoom CAT supports today.
- **Annotations stay EPSG:4326 GeoJSON**, exactly like the server stores them. Ring closure and coordinate precision must match the web app so both clients can edit the same data.

## Annotation feature parity (from the current web runtime)

Inventory taken from `cat/web/js/annotation-runtime-*.js`, `v2-*.js`, `annotation-form.js`, `annotation-drawing.js`, `annotation-undo.js`, `annotation-timer.js` and `annotation-sam3.js` (~27k lines). Grouped by priority.

### P0 — Core annotation (pilot release)
- **Login and project picker:** sign in, server profile (Dev/Prod), project list with COG thumbnails, search/filter, recent projects.
- **Map:**
  - project COG layers from the bucket; pan and zoom with mouse, trackpad and keyboard;
  - deep zoom beyond native resolution;
  - scale bar, coordinate readout, layer visibility and opacity.
- **Draw tools:** point, line, rectangle, polygon. Click to add vertices, double-click or Enter to finish, Esc to cancel, Backspace to remove the last vertex.
- **Edit:** select, move vertices, insert/delete vertices, move the whole feature, delete feature.
- **Vertex snapping** to neighboring annotations, so adjacent colonies share edges (pixel tolerance; `annotation-runtime-snapping.js`).
- **Attribute form:**
  - species autocomplete (spcode and scientific name), respecting the project's species filter;
  - field-option dropdowns from the server: morph_code, transect, segment, no_colony, juvenile, remnant, ex_bound, juv_substrate;
  - condition/cause fields, notes;
  - sticky fields (analyst, obs_year, mission_id, site), field defaults, auto-uppercase.
- **Save:**
  - autosave plus Ctrl+S, through the outbox and `/annotations/batch-sync`;
  - a visible status chip (Saved · Saving… · Offline, N queued);
  - unsaved work survives a crash or restart.
- **Undo/redo** (Ctrl+Z / Ctrl+Y / Ctrl+Shift+Z) for draw, edit, delete and attribute changes.
- **Annotation table:** sortable, arrow-key navigation, click to zoom to a feature, completion status.
- **Styling:** complete/incomplete colors, labels on/off.
- **Annotation timer and sessions:** start/end/heartbeat endpoints and time per annotation.

### P1 — Productivity (second release)
- **Bulk draw mode:** draw many shapes now, fill attributes later (`v2-bulk.js`).
- **Lasso selection** and **bulk update/fill** of a column across the selection; batch delete.
- **Table extras:** column visibility, resizable/dockable panel, batch column fill (`v2-table.js`).
- **Overlay layers:** shapefile/transect layers from the server, reordering, styling, generate transect, buffer/clip via the server.
- **Attribute-driven symbology** by species or condition, with an auto legend.
- **Measure tool:** distance and area, sub-meter readout.
- **Bookmarks:** named views stored in project metadata.
- **History panel:** recently deleted items and the change history of a feature; restore.
- **Pop-out windows:** table or form on a second monitor. Native multi-window replaces the current BroadcastChannel popout.
- **Collaborator awareness:** see other users' new annotations by periodic refresh of changes since the last sync.

### P2 — Extras
- **SAM3 AI segmentation:** point/box/text prompts and the tiled grid, through the server; masks become editable polygons.
- **Compare view:** two projects or years side by side, with linked pan/zoom.
- **DEM derivatives:** hillshade/slope layers rendered from server output, zonal stats for a polygon.
- **Cat buddy** break reminder 🐱.
- **Quick exports from the open project** (GeoJSON/CSV/Shapefile/KML locally; GPKG/GDB via the server). The full export page stays on the web.

### Stays on the web (opened in Chrome from the app's menu)
Project creator, user admin, user preferences, species config, sites, QC, activity, report, export, debug stats.

## "Smooth and easy" — UX commitments

These are the goals that make it feel better than the browser version, not just equal to it:

1. **Fast start:** the window appears in under 1 s, the last project reopens at the last view, and the cached token means no login prompt each time.
2. **No blank tiles while panning:**
   - prefetch the next ring of tiles and one zoom level up and down;
   - show the coarser overview while the sharp tile loads;
   - keep a disk cache so a revisited area is instant.
3. **Keyboard-first:**
   - single-key tools (P/L/R/G for point/line/rectangle/polygon, V select, S snap toggle, M measure, B bulk, Space-drag pan);
   - Tab through the form and Enter to save and start the next annotation;
   - a **command palette** (Ctrl+K) for everything else;
   - a cheat sheet on `?`;
   - all shortcuts rebindable in settings.
4. **One-click repeat:** "same as last" to copy the previous annotation's attributes, plus favorite species pinned at the top of the autocomplete.
5. **Never lose work:**
   - every change goes to the local outbox before the network;
   - offline or server hiccups show a banner, not an error dialog;
   - queued saves send automatically.
6. **Clear status:**
   - a status bar with save state, user, server profile (Dev/Prod shown in different colors so nobody edits prod by accident), annotation count and session time.
7. **Dockable, remembered layout:**
   - panels (layers, form, table, history) can dock, float or move to a second monitor;
   - the layout is saved per user;
   - light and dark themes.
8. **High-DPI and multi-monitor correct** by default (egui handles per-monitor DPI).
9. **Self-update:**
   - on start the app checks the server for a new version;
   - one click downloads it, swaps the exe and restarts;
   - no admin rights and no installer.
10. **Helpful errors and support:**
    - plain-language messages;
    - "Copy diagnostics" bundles logs and version info for support.

Note on skipping code signing: Windows SmartScreen will show "Windows protected your PC" the first time a new version runs (users click *More info → Run anyway*). Self-update keeps the same folder, so this happens once per downloaded version at most. If that becomes a nuisance, signing can be added later without code changes.

## Server-side changes (small)

1. **Token login:** `POST /api/auth/token` → session token; accept `Authorization: Bearer` anywhere the `cat_session` cookie is accepted. This reuses the `cat_sessions` table.
2. **Delta endpoint** for collaborator refresh: annotations changed since a timestamp (P1).
3. **`/api/version`** extended with an API version and the latest desktop build URL, used for the compatibility check and self-update.
4. HTTPS, and tighter CORS (no `*` with credentials).
5. **Prod cutover** later is just the server's Oracle DSN and credentials; nothing changes in the desktop.

## Phased plan (one experienced Rust developer)

| Phase | Duration | Deliverable |
|---|---|---|
| **0 — Spike** | 3 wk | `cat-cog` streams tiles from real bucket COGs (zstd RGB, LZW DEM, a LOCAL_CS file). egui map canvas pans and zooms at 60 fps to 2000×. Login via the token endpoint. Portable zip runs on a clean, non-admin Windows account. **Exit:** smoother pan/zoom than the web app on the same machine |
| **1 — Core annotation (P0)** | 8–10 wk | Everything in P0, the outbox, self-update and UX items 1–6. **Pilot with 2–3 annotators** alongside the web app |
| **2 — Productivity (P1)** | 6–8 wk | Bulk draw, lasso, table extras, overlays, symbology, measure, bookmarks, history, pop-outs, collaborator refresh. Fix pilot feedback |
| **3 — Extras & polish (P2)** | 4–6 wk | SAM3, compare, DEM layers, quick exports, command palette, rebindable shortcuts, themes |
| **4 — Default client** | 2 wk | Parity sign-off; the native app becomes the recommended annotation client; the web annotation screen remains for Cloud Workstations and as a fallback |

Total ≈ 6–7 months to full parity, with a usable pilot after ~3 months. Phase 0 results should reset these estimates.

## Repo layout

```
desktop/
  Cargo.toml          # workspace
  app/                # CAT.exe (eframe): windows, menus, docking, settings, updater
  crates/
    cat-core/         # models, GeoJSON (de)serialization matching the server
    cat-cog/          # COG reader + tile pipeline + cache
    cat-api/          # server client, auth, outbox/sync
    cat-map/          # wgpu map canvas, layers, picking
    cat-edit/         # drawing/editing tools, snapping, undo/redo
  tests/              # golden tests: tiles vs TiTiler output; GeoJSON round-trips vs server
cat/                  # existing Python server + web UI (unchanged)
```

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Parity takes longer than planned (27k lines of JS behavior) | Ship P0 to a pilot early; the web app is the fallback; port behavior module by module using the inventory above |
| Bucket COG variants the Rust reader doesn't handle | Phase 0 inventory script over the bucket (compression, bands, CRS, nodata); the server's tiles are a fallback per layer |
| Two clients editing the same data differently | Shared GeoJSON golden tests; the server stays the single source of truth; `client_uuid` idempotency |
| Old GPUs or remote desktop sessions without DX12 | wgpu falls back to Vulkan/GL; a software-rendering fallback flag |
| SmartScreen prompt on unsigned builds | Document *Run anyway*; add signing later if needed |
| Accidental edits on prod | Colored profile badge, confirmation when switching to Prod, read-only mode option |

## Next steps

1. Approve the plan and the native-first scope.
2. Phase 0: scaffold `desktop/` and build the COG reader and map canvas against 2–3 real bucket COGs (bucket names/paths to use?).
3. Add the token-login endpoint on the server (small, independent change).
