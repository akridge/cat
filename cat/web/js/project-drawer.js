/* ============================================================
 * CAT - Project details drawer
 *
 * Opens over the Project Manager when a project row is clicked, and shows
 * the things a one-line row can't: the imagery on a map with its
 * annotations drawn over it, the COGs behind it and what they actually
 * contain, a sample of the annotation table, who has access, and the
 * activity log.
 *
 * Data sources, all of which already existed except where noted:
 *   GET /api/db/projects/{id}                     project record
 *   GET /api/db/projects/{id}/assets              NEW - was snapshot-only,
 *                                                 and snapshot drags every
 *                                                 annotation with it
 *   GET /api/db/projects/{id}/overlay-layers      shapefile overlays
 *   GET /api/db/projects/{id}/annotations?limit=  table sample
 *   GET /api/db/projects/{id}/annotations/geojson?limit=   map outlines
 *   GET /api/db/projects/{id}/collaborators       access list
 *   GET /api/db/projects/{id}/activity            already served, nothing
 *                                                 in the app displayed it
 *   GET /info?url=                                TiTiler: CRS, size, bands
 *
 * Sections load independently and render as they arrive: the activity log
 * being slow shouldn't hold up the map.
 * ============================================================ */
(function () {
  'use strict';

  // Enough rows to show the shape of the data without turning a preview
  // into a table dump; the annotator is one click away for the full set.
  var TABLE_SAMPLE = 25;
  // Outlines are a sketch of coverage, not the working view.
  var MAP_FEATURE_CAP = 2000;

  var overlayEl = null;
  var drawerEl = null;
  var map = null;
  var lastFocused = null;
  var currentProjectId = null;

  // The georeference check needs two things that arrive from different
  // requests: the map's fitted extent, and the project's recorded site
  // coordinates (which live in metadata.visit_info, present on the detail
  // record but NOT on the list row used to seed the header). Hold both and
  // let whichever lands second draw the note.
  var projectRecord = null;
  var mapExtent = null;
  var mapRasterCount = 0;
  var shownRaster = null;

  // ── Utilities ────────────────────────────────────────────────

  function esc(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function $(id) { return document.getElementById(id); }

  function setHtml(id, html) {
    var el = $(id);
    if (el) el.innerHTML = html;
  }

  function skeleton(lines) {
    var out = '';
    for (var i = 0; i < (lines || 3); i++) {
      out += '<div class="pd-skel' + (i % 2 ? ' pd-skel--short' : '') + '"></div>';
    }
    return out;
  }

  function fmtNum(n) {
    return (Number(n) || 0).toLocaleString();
  }

  function fmtDate(value) {
    if (!value) return '—';
    var d = new Date(value);
    return isNaN(d.getTime()) ? String(value) : d.toLocaleString();
  }

  // Reuses the list's relative formatter where the page provides one, so
  // the drawer and the cards behind it never word the same instant
  // differently.
  function rel(value) {
    if (typeof window.relativeTime === 'function') {
      var r = window.relativeTime(value);
      if (r) return r;
    }
    return fmtDate(value);
  }

  // Mirrors toGdalPath() in annotation-runtime-project-layers.js: GDAL
  // reads gs:// through its /vsigs/ virtual filesystem.
  function toGdalPath(path) {
    if (!path) return path;
    return path.indexOf('gs://') === 0 ? '/vsigs/' + path.slice(5) : path;
  }

  function isDem(asset) {
    var hay = [(asset && asset.asset_type) || '', (asset && asset.asset_name) || '',
               (asset && asset.cog_url) || ''].join(' ').toLowerCase();
    return hay.indexOf('dem') !== -1;
  }

  function getJson(url) {
    return fetch(url, { credentials: 'same-origin' }).then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    });
  }

  // ── Shell ────────────────────────────────────────────────────

  function build() {
    if (drawerEl) return;

    overlayEl = document.createElement('div');
    overlayEl.className = 'pd-overlay';
    overlayEl.addEventListener('click', close);

    drawerEl = document.createElement('aside');
    drawerEl.className = 'pd-drawer';
    drawerEl.setAttribute('role', 'dialog');
    drawerEl.setAttribute('aria-modal', 'true');
    drawerEl.setAttribute('aria-labelledby', 'pdTitle');
    drawerEl.innerHTML =
      '<div class="pd-head">' +
        '<div class="pd-head-row">' +
          '<div style="min-width:0; flex:1;">' +
            '<h2 class="pd-title" id="pdTitle">Project</h2>' +
            '<p class="pd-sub" id="pdSub"></p>' +
          '</div>' +
          '<button type="button" class="pd-close" id="pdClose" aria-label="Close details">&times;</button>' +
        '</div>' +
        '<div class="pd-actions" id="pdActions"></div>' +
      '</div>' +
      '<div class="pd-body">' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Preview</div>' +
          '<div class="pd-map" id="pdMap"></div>' +
          '<div id="pdMapNote"></div>' +
        '</div>' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Imagery <span class="pd-count" id="pdAssetCount"></span></div>' +
          '<div id="pdAssets">' + skeleton(2) + '</div>' +
        '</div>' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Overlay layers <span class="pd-count" id="pdOverlayCount"></span></div>' +
          '<div id="pdOverlays">' + skeleton(1) + '</div>' +
        '</div>' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Annotations <span class="pd-count" id="pdAnnCount"></span></div>' +
          '<div id="pdAnnotations">' + skeleton(3) + '</div>' +
        '</div>' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Access</div>' +
          '<div id="pdPeople">' + skeleton(1) + '</div>' +
        '</div>' +
        '<div class="pd-section">' +
          '<div class="pd-section-title">Activity</div>' +
          '<div id="pdActivity">' + skeleton(3) + '</div>' +
        '</div>' +
      '</div>';

    document.body.appendChild(overlayEl);
    document.body.appendChild(drawerEl);

    $('pdClose').addEventListener('click', close);

    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && drawerEl.classList.contains('is-open')) {
        // Let a modal opened FROM the drawer (Share, Layers) take Escape
        // first — closing the drawer out from under it would orphan it.
        var modalOpen = Array.prototype.some.call(
          document.querySelectorAll('.modal-overlay'),
          function (m) { return getComputedStyle(m).display !== 'none'; }
        );
        if (!modalOpen) close();
      }
    });
  }

  function close() {
    if (!drawerEl) return;
    overlayEl.classList.remove('is-open');
    drawerEl.classList.remove('is-open');
    currentProjectId = null;

    // Leaflet keeps window resize listeners and tile requests alive on a
    // hidden map; a drawer opened a dozen times would accumulate a dozen.
    if (map) {
      try { map.remove(); } catch (e) { /* already gone */ }
      map = null;
    }
    if (lastFocused && typeof lastFocused.focus === 'function') lastFocused.focus();
  }
  window.closeProjectDrawer = close;

  // ── Open ─────────────────────────────────────────────────────

  function open(projectId, seedProject) {
    build();
    lastFocused = document.activeElement;
    currentProjectId = projectId;

    projectRecord = seedProject || null;
    mapExtent = null;
    mapRasterCount = 0;
    shownRaster = null;

    // Seed from the row's own data so the header is right immediately,
    // then correct it from the API.
    renderHeader(seedProject || { project_id: projectId });

    setHtml('pdAssets', skeleton(2));
    setHtml('pdOverlays', skeleton(1));
    setHtml('pdAnnotations', skeleton(3));
    setHtml('pdPeople', skeleton(1));
    setHtml('pdActivity', skeleton(3));
    setHtml('pdMapNote', '');
    var mapHost = $('pdMap');
    if (mapHost) mapHost.innerHTML = '';

    overlayEl.classList.add('is-open');
    drawerEl.classList.add('is-open');
    $('pdClose').focus();

    var guard = function (fn) {
      // Every section resolves independently; a response that lands after
      // the user has moved on to another project must not paint into it.
      return function (data) { if (currentProjectId === projectId) fn(data); };
    };

    getJson('/api/db/projects/' + projectId)
      .then(guard(function (d) {
        projectRecord = d.project || seedProject || {};
        renderHeader(projectRecord);
        // The detail record is the only source of the site's recorded
        // coordinates, so the georeference check can only run now.
        updateGeorefNote();
      }))
      .catch(function () { /* header already seeded from the row */ });

    var assetsPromise = getJson('/api/db/projects/' + projectId + '/assets')
      .then(function (d) { return (d && d.assets) || []; })
      .catch(function () { return []; });

    assetsPromise.then(guard(renderAssets));

    getJson('/api/db/projects/' + projectId + '/overlay-layers')
      .then(guard(renderOverlays))
      .catch(guard(function () { setHtml('pdOverlays', '<div class="pd-empty">Could not load overlay layers.</div>'); }));

    getJson('/api/db/projects/' + projectId + '/annotations?limit=' + TABLE_SAMPLE)
      .then(guard(renderAnnotationTable))
      .catch(guard(function () { setHtml('pdAnnotations', '<div class="pd-empty">Could not load annotations.</div>'); }));

    getJson('/api/db/projects/' + projectId + '/collaborators')
      .then(guard(renderPeople))
      .catch(guard(function () { setHtml('pdPeople', '<div class="pd-empty">Could not load access list.</div>'); }));

    getJson('/api/db/projects/' + projectId + '/activity?limit=25')
      .then(guard(renderActivity))
      .catch(guard(function () { setHtml('pdActivity', '<div class="pd-empty">Could not load activity.</div>'); }));

    // The map needs both the rasters and the annotation geometry.
    Promise.all([
      assetsPromise,
      getJson('/api/db/projects/' + projectId + '/annotations/geojson?limit=' + MAP_FEATURE_CAP)
        .catch(function () { return { features: [] }; }),
    ]).then(guard(function (results) {
      renderMap(results[0], results[1]);
    }));
  }
  window.openProjectDrawer = open;

  // ── Header ───────────────────────────────────────────────────

  function renderHeader(project) {
    var id = project.project_id != null ? project.project_id : currentProjectId;
    var title = $('pdTitle');
    if (title) title.textContent = project.project_name || ('Project ' + id);

    var bits = [];
    if (project.site) bits.push('Site ' + project.site);
    if (project.cruise) bits.push(project.cruise);
    if (project.year) bits.push(project.year);
    if (project.region) bits.push(project.region);
    var owner = project.owner_display_name || project.owner_username;
    if (owner) bits.push('Owner: ' + owner);
    var sub = $('pdSub');
    if (sub) sub.textContent = bits.join(' · ') || ('#' + id);

    var safeName = String(project.project_name || '').replace(/'/g, "\\'");
    setHtml('pdActions',
      '<button class="cat-btn cat-btn--primary" style="font-size:12px; padding:6px 12px;" ' +
        'onclick="openDbProject(' + id + ')">Open in annotator</button>' +
      '<button class="cat-btn cat-btn--outline" style="font-size:12px; padding:6px 12px;" ' +
        'onclick="window.open(\'/report?project_id=' + id + '\',\'_blank\')">Report</button>' +
      '<button class="cat-btn cat-btn--outline" style="font-size:12px; padding:6px 12px;" ' +
        'onclick="editDbProject(' + id + ')">Edit</button>' +
      '<button class="cat-btn cat-btn--outline" style="font-size:12px; padding:6px 12px;" ' +
        'onclick="openCollaboratorsModal(' + id + ', \'' + esc(safeName) + '\')">Share</button>'
    );

    if (project.notes) {
      var subEl = $('pdSub');
      if (subEl) subEl.title = project.notes;
    }
  }

  // ── Preview map ──────────────────────────────────────────────

  function renderMap(assets, geojson) {
    var host = $('pdMap');
    if (!host) return;

    if (typeof L === 'undefined') {
      host.innerHTML = '<div class="pd-empty" style="padding:14px;">Map library unavailable.</div>';
      return;
    }

    var features = (geojson && geojson.features) || [];
    var rasters = (assets || []).filter(function (a) { return (a.cog_url || '').trim(); });

    if (!rasters.length && !features.length) {
      host.innerHTML = '<div class="pd-empty" style="padding:14px; color:#cbd5e1;">' +
        'Nothing to preview yet — no imagery and no annotations.</div>';
      return;
    }

    map = L.map(host, {
      attributionControl: false,
      zoomControl: true,
      maxZoom: 30,
    }).setView([0, 0], 2);

    // Draw ONE raster, preferring the orthomosaic — the same choice the
    // card thumbnail makes, so the preview and the card agree. Stacking a
    // DEM over its orthomosaic just hides the photo behind a colour ramp
    // (and doubles the tile requests to do it); the annotator is where you
    // go to toggle between them.
    var preferred = null;
    for (var i = 0; i < rasters.length; i++) {
      if (!isDem(rasters[i])) { preferred = rasters[i]; break; }
    }
    if (!preferred) preferred = rasters[0] || null;

    if (preferred) {
      var demPreview = isDem(preferred);
      var url = '/tiles/WebMercatorQuad/{z}/{x}/{y}.png?url=' +
        encodeURIComponent(toGdalPath(preferred.cog_url)) +
        (demPreview ? '&colormap_name=viridis&rescale=-25,0' : '');
      L.tileLayer(url, { maxNativeZoom: 24, maxZoom: 30 }).addTo(map);
    }
    shownRaster = preferred;

    var bounds = null;

    if (features.length) {
      var layer = L.geoJSON({ type: 'FeatureCollection', features: features }, {
        style: { color: '#00bde3', weight: 1.5, opacity: 0.95, fillOpacity: 0.12 },
        pointToLayer: function (f, latlng) {
          return L.circleMarker(latlng, { radius: 3, color: '#00bde3', weight: 1.5, fillOpacity: 0.5 });
        },
      }).addTo(map);
      try {
        var b = layer.getBounds();
        if (b && b.isValid()) bounds = b;
      } catch (e) { /* degenerate geometry */ }
    }

    // Fall back to the raster's own footprint when there are no annotations
    // to frame yet.
    if (!bounds) {
      var withBounds = rasters.filter(function (a) { return Array.isArray(a.bounds) && a.bounds.length === 4; });
      if (withBounds.length) {
        var bb = withBounds[0].bounds;
        bounds = L.latLngBounds([[bb[1], bb[0]], [bb[3], bb[2]]]);
      }
    }

    if (bounds) {
      map.fitBounds(bounds, { padding: [14, 14], maxZoom: 26 });
    }

    // Leaflet measures the container on creation; the drawer is still
    // sliding in at that point, so the first paint can be a quarter tile
    // wide until something forces a re-measure.
    setTimeout(function () { if (map) map.invalidateSize(); }, 240);

    mapExtent = bounds;
    mapRasterCount = rasters.length;
    updateGeorefNote();
  }

  /**
   * Flag a raster whose georeference disagrees with the site it belongs to.
   *
   * Some COGs in production carry a site-local coordinate frame that was
   * labelled EPSG:4326 — the pixels, the DEM and the annotations all agree
   * with each other, so the preview is correct, but the absolute position
   * is meaningless (observed: several StRS orthomosaics placing a 21 m plot
   * at 0.0001°N). The app's existing areBoundsBogus() can't catch this
   * because it only looks for a literal LOCAL_CS string or TiTiler's
   * global-fallback extent, and this claims to be honest 4326.
   *
   * The check is against the project's own recorded site coordinates rather
   * than a guess about where reefs are, so it only fires when the data
   * actually contradicts itself.
   */
  function updateGeorefNote() {
    var bounds = mapExtent;
    if (!bounds) return;

    var visit = projectRecord && projectRecord.metadata && projectRecord.metadata.visit_info;
    var siteLat = visit ? parseFloat(visit.latitude) : NaN;
    var siteLon = visit ? parseFloat(visit.longitude) : NaN;

    var c = bounds.getCenter();
    var shownName = shownRaster && (shownRaster.asset_name || shownRaster.cog_url || '').split('/').pop();
    var extra = mapRasterCount > 1
      ? ' · ' + (mapRasterCount - 1) + ' other raster' + (mapRasterCount === 2 ? '' : 's') + ' not shown'
      : '';
    var note = '<div class="pd-map-note"><span>📐</span><span>' +
      (shownName ? 'Showing ' + esc(shownName) + extra : 'Annotation outlines only') +
      '</span></div>';

    if (isFinite(siteLat) && isFinite(siteLon)) {
      // Rough great-circle distance is plenty to tell "same reef" from
      // "wrong hemisphere".
      var dLat = (c.lat - siteLat) * 110.54;
      var dLon = (c.lng - siteLon) * 111.32 * Math.cos(siteLat * Math.PI / 180);
      var km = Math.sqrt(dLat * dLat + dLon * dLon);
      if (km > 50) {
        note += '<div class="pd-warn"><strong>Georeference looks wrong.</strong> ' +
          'The imagery places this project at <code>' + c.lat.toFixed(4) + ', ' + c.lng.toFixed(4) +
          '</code>, about ' + Math.round(km).toLocaleString() + ' km from the site\'s recorded position ' +
          '(<code>' + siteLat.toFixed(4) + ', ' + siteLon.toFixed(4) + '</code>). ' +
          'The preview and annotations are still consistent with each other — only the absolute ' +
          'position is affected.</div>';
      }
    }

    setHtml('pdMapNote', note);
  }

  // ── Imagery ──────────────────────────────────────────────────

  function renderAssets(assets) {
    var host = $('pdAssets');
    if (!host) return;

    var countEl = $('pdAssetCount');
    if (countEl) countEl.textContent = assets.length ? assets.length : '';

    if (!assets.length) {
      host.innerHTML = '<div class="pd-empty">No imagery linked to this project yet.</div>';
      return;
    }

    host.innerHTML = assets.map(function (asset, i) {
      var dem = isDem(asset);
      return '<div class="pd-asset">' +
        '<div class="pd-asset-head">' +
          '<span class="pd-asset-name" title="' + esc(asset.asset_name || '') + '">' +
            esc(asset.asset_name || 'Unnamed asset') + '</span>' +
          '<span class="pd-tag' + (dem ? ' pd-tag--dem' : '') + '">' +
            esc(dem ? 'DEM' : (asset.asset_type || 'COG')) + '</span>' +
        '</div>' +
        '<dl class="pd-facts" id="pdAssetFacts' + i + '">' +
          '<dt>Details</dt><dd class="pd-loading">reading…</dd>' +
        '</dl>' +
        '<div class="pd-uri" title="' + esc(asset.cog_url || '') + '">' + esc(asset.cog_url || '') + '</div>' +
      '</div>';
    }).join('');

    // What the raster actually contains comes from the tiler, not the DB
    // row — the row records where it was put, /info records what is in it.
    assets.forEach(function (asset, i) {
      if (!(asset.cog_url || '').trim()) {
        setHtml('pdAssetFacts' + i, '<dt>Source</dt><dd>No COG URL recorded</dd>');
        return;
      }
      getJson('/info?url=' + encodeURIComponent(toGdalPath(asset.cog_url)))
        .then(function (info) { renderAssetFacts(i, asset, info); })
        .catch(function () {
          setHtml('pdAssetFacts' + i,
            '<dt>Details</dt><dd>Could not read this COG — it may be missing or unreadable.</dd>');
        });
    });
  }

  function renderAssetFacts(index, asset, info) {
    var rows = [];
    if (info.width && info.height) {
      rows.push(['Size', fmtNum(info.width) + ' × ' + fmtNum(info.height) + ' px']);
    }
    if (info.count) {
      rows.push(['Bands', info.count + (info.dtype ? ' · ' + info.dtype : '')]);
    }
    if (info.crs) {
      // TiTiler returns an OGC URI; the EPSG code is the useful part.
      var epsg = String(info.crs).match(/EPSG\/\d+\/(\d+)/);
      rows.push(['CRS', epsg ? 'EPSG:' + epsg[1] : String(info.crs)]);
    }
    if (Array.isArray(info.overviews) && info.overviews.length) {
      rows.push(['Overviews', info.overviews.join(', ')]);
    } else {
      rows.push(['Overviews', 'none — tiles and thumbnails will be slow']);
    }
    if (Array.isArray(info.bounds) && info.bounds.length === 4) {
      var b = info.bounds;
      var wM = Math.abs(b[2] - b[0]) * 111320 * Math.cos((b[1] * Math.PI) / 180);
      var hM = Math.abs(b[3] - b[1]) * 110540;
      if (isFinite(wM) && isFinite(hM) && wM > 0) {
        rows.push(['Extent', wM.toFixed(1) + ' × ' + hM.toFixed(1) + ' m']);
      }
      rows.push(['Origin', b[1].toFixed(5) + ', ' + b[0].toFixed(5)]);
    }
    if (asset.source_epsg) rows.push(['Source EPSG', String(asset.source_epsg)]);

    setHtml('pdAssetFacts' + index, rows.map(function (r) {
      return '<dt>' + esc(r[0]) + '</dt><dd>' + esc(r[1]) + '</dd>';
    }).join(''));
  }

  // ── Overlay layers ───────────────────────────────────────────

  function renderOverlays(data) {
    var layers = (data && data.layers) || (data && data.overlay_layers) || [];
    var countEl = $('pdOverlayCount');
    if (countEl) countEl.textContent = layers.length ? layers.length : '';

    if (!layers.length) {
      setHtml('pdOverlays', '<div class="pd-empty">No overlay layers.</div>');
      return;
    }

    setHtml('pdOverlays', layers.map(function (l) {
      var type = l.layer_type || l.type || '';
      return '<div class="pd-asset">' +
        '<div class="pd-asset-head">' +
          '<span class="pd-asset-name">' + esc(l.layer_name || l.name || 'Layer') + '</span>' +
          (type ? '<span class="pd-tag">' + esc(type) + '</span>' : '') +
        '</div>' +
        (l.feature_count != null
          ? '<div class="pd-uri">' + fmtNum(l.feature_count) + ' feature' +
            (Number(l.feature_count) === 1 ? '' : 's') + '</div>'
          : '') +
      '</div>';
    }).join(''));
  }

  // ── Annotation sample ────────────────────────────────────────

  function renderAnnotationTable(data) {
    var rows = (data && data.annotations) || [];
    var total = (data && data.total_count != null) ? data.total_count : null;

    var countEl = $('pdAnnCount');
    if (countEl) {
      countEl.textContent = rows.length
        ? (total != null && total > rows.length
            ? 'showing ' + rows.length + ' of ' + fmtNum(total)
            : fmtNum(rows.length))
        : '';
    }

    if (!rows.length) {
      setHtml('pdAnnotations', '<div class="pd-empty">No annotations yet.</div>');
      return;
    }

    // Columns chosen to answer "is this data any good?" at a glance rather
    // than to reproduce the annotator's 36-column grid.
    var cols = [
      ['id', function (a) { return a.annotation_id; }],
      ['Species', function (a) { return prop(a, 'spcode'); }],
      ['Morph', function (a) { return prop(a, 'morph_code'); }],
      ['Transect', function (a) { return prop(a, 'transect'); }],
      ['Segment', function (a) { return prop(a, 'segment'); }],
      ['Old dead %', function (a) { return prop(a, 'old_dead'); }],
      ['Analyst', function (a) { return prop(a, 'analyst') || a.created_by; }],
      ['Created', function (a) { return a.created_at ? rel(a.created_at) : null; }],
    ];

    var html = '<div class="pd-table-wrap"><table class="pd-table"><thead><tr>' +
      cols.map(function (c) { return '<th>' + esc(c[0]) + '</th>'; }).join('') +
      '</tr></thead><tbody>' +
      rows.map(function (a) {
        return '<tr>' + cols.map(function (c) {
          var v = c[1](a);
          return (v == null || v === '')
            ? '<td class="pd-missing">—</td>'
            : '<td>' + esc(v) + '</td>';
        }).join('') + '</tr>';
      }).join('') +
      '</tbody></table></div>';

    if (rows.length >= TABLE_SAMPLE) {
      html += '<div class="pd-empty">First ' + TABLE_SAMPLE +
        ' annotations. Open the project to see and edit them all.</div>';
    }
    setHtml('pdAnnotations', html);
  }

  function prop(annotation, key) {
    var p = (annotation && annotation.properties) || {};
    // Oracle-imported rows carry SHOUTING keys, app-created ones lowercase.
    var v = p[key];
    if (v == null) v = p[key.toUpperCase()];
    if (v == null) v = annotation[key];
    return (v === '' || v === null || v === undefined) ? null : v;
  }

  // ── Access ───────────────────────────────────────────────────

  function renderPeople(data) {
    var people = (data && data.collaborators) || [];
    if (!people.length) {
      setHtml('pdPeople', '<div class="pd-empty">No collaborators — only the owner has access.</div>');
      return;
    }
    setHtml('pdPeople', '<div class="pd-people">' + people.map(function (p) {
      var name = p.display_name || p.username || ('User ' + p.user_id);
      var role = p.role || 'viewer';
      return '<span class="pd-person' + (role === 'owner' ? ' pd-person--owner' : '') + '">' +
        esc(name) + ' · ' + esc(role) + '</span>';
    }).join('') + '</div>');
  }

  // ── Activity ─────────────────────────────────────────────────

  // The log stores machine actions; these read them back as sentences.
  var ACTION_TEXT = {
    project_created: 'created the project',
    project_updated: 'updated project details',
    annotation_created: 'added an annotation',
    annotation_updated: 'edited an annotation',
    annotation_deleted: 'deleted an annotation',
    annotation_restored: 'restored an annotation',
    annotations_bulk_replaced: 'replaced the annotation set',
    annotations_bulk_created: 'imported annotations',
    asset_added: 'linked imagery',
    collaborator_added: 'shared the project',
    collaborator_removed: 'removed a collaborator',
    collaborator_updated: 'changed a collaborator role',
    overlay_layer_created: 'added an overlay layer',
    overlay_layer_deleted: 'removed an overlay layer',
  };

  function dotClass(action) {
    var a = String(action || '');
    if (a.indexOf('deleted') !== -1 || a.indexOf('removed') !== -1) return ' pd-dot--delete';
    if (a.indexOf('created') !== -1 || a.indexOf('added') !== -1) return ' pd-dot--create';
    if (a.indexOf('collaborator') !== -1) return ' pd-dot--share';
    return '';
  }

  function renderActivity(data) {
    var events = (data && data.activity) || [];
    if (!events.length) {
      setHtml('pdActivity', '<div class="pd-empty">No recorded activity.</div>');
      return;
    }
    setHtml('pdActivity', '<div class="pd-feed">' + events.map(function (e) {
      var who = e.display_name || e.username || 'Someone';
      var what = ACTION_TEXT[e.action] || String(e.action || 'did something').replace(/_/g, ' ');
      return '<div class="pd-event">' +
        '<div class="pd-dot' + dotClass(e.action) + '"></div>' +
        '<div class="pd-event-text">' +
          '<strong>' + esc(who) + '</strong> ' + esc(what) +
          '<div class="pd-event-meta" title="' + esc(fmtDate(e.created_at)) + '">' +
            esc(rel(e.created_at)) + '</div>' +
        '</div>' +
      '</div>';
    }).join('') + '</div>');
  }
})();
