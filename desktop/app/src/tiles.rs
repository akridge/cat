//! Background COG opening and tile loading, with a GPU texture cache.
//!
//! The UI thread says which tiles it *wants* each frame; worker threads fetch
//! only tiles that are still wanted (so fast panning doesn't queue up stale
//! work), and decoded tiles are uploaded as textures a few per frame so the
//! map never stutters.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use cat_cog::source::{http_client, HttpSource};
use cat_cog::{Cog, DecodeOptions, DecodedTile};
use lru::LruCache;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TileKey {
    pub layer: u64,
    pub level: u8,
    pub tx: u32,
    pub ty: u32,
}

pub enum LayerState {
    Opening,
    Ready(Arc<Cog>),
    Failed(String),
}

pub struct Layer {
    pub id: u64,
    pub name: String,
    pub uri: String,
    pub state: LayerState,
    pub visible: bool,
    pub opacity: f32,
}

impl Layer {
    pub fn cog(&self) -> Option<&Arc<Cog>> {
        match &self.state {
            LayerState::Ready(c) => Some(c),
            _ => None,
        }
    }
}

enum Job {
    Open { layer: u64, uri: String },
    Tiles { layer: u64, cog: Arc<Cog>, level: usize, tiles: Vec<(u32, u32)> },
}

enum Done {
    Opened { layer: u64, result: Result<Arc<Cog>, String> },
    Tile { key: TileKey, result: Result<Option<DecodedTile>, String> },
}

/// Max tiles per worker job; small batches keep latency low while still
/// letting cat-cog merge neighbouring byte ranges.
const BATCH: usize = 12;
const WORKERS: usize = 4;
/// Texture uploads per frame (each 512² tile is 1 MB).
const UPLOADS_PER_FRAME: usize = 12;
/// GPU texture cache size in tiles (~1 MB each).
const TEXTURE_CACHE: usize = 320;

pub struct TileManager {
    job_tx: Sender<Job>,
    done_rx: Receiver<Done>,
    wanted: Arc<Mutex<HashSet<TileKey>>>,
    options: Arc<Mutex<DecodeOptions>>,
    /// `None` = tile has no data (sparse) — nothing to draw.
    textures: LruCache<TileKey, Option<egui::TextureHandle>>,
    pending: HashSet<TileKey>,
    failed: HashMap<TileKey, String>,
    opened: Vec<(u64, Result<Arc<Cog>, String>)>,
    ready: Vec<(TileKey, Option<DecodedTile>)>,
    pub last_error: Option<String>,
}

impl TileManager {
    pub fn new(ctx: egui::Context) -> Self {
        let (job_tx, job_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let wanted: Arc<Mutex<HashSet<TileKey>>> = Arc::default();
        let options = Arc::new(Mutex::new(DecodeOptions { background_transparent: true }));
        let client = http_client();
        for i in 0..WORKERS {
            let job_rx = job_rx.clone();
            let done_tx = done_tx.clone();
            let wanted = wanted.clone();
            let options = options.clone();
            let client = client.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name(format!("tiles-{i}"))
                .spawn(move || loop {
                    let job = match job_rx.lock().unwrap().recv() {
                        Ok(j) => j,
                        Err(_) => return,
                    };
                    match job {
                        Job::Open { layer, uri } => {
                            let src = Arc::new(HttpSource::new(client.clone(), &uri));
                            let result = Cog::open(src)
                                .and_then(|c| c.check_crs().map(|_| c))
                                .map(Arc::new)
                                .map_err(|e| e.to_string());
                            let _ = done_tx.send(Done::Opened { layer, result });
                        }
                        Job::Tiles { layer, cog, level, tiles } => {
                            // Skip tiles the user has already panned away from.
                            let tiles: Vec<(u32, u32)> = {
                                let w = wanted.lock().unwrap();
                                tiles
                                    .into_iter()
                                    .filter(|&(tx, ty)| w.contains(&TileKey { layer, level: level as u8, tx, ty }))
                                    .collect()
                            };
                            let opts = *options.lock().unwrap();
                            let results = if tiles.is_empty() { Vec::new() } else { cog.fetch_tiles(level, &tiles, opts) };
                            for ((tx, ty), r) in results {
                                let key = TileKey { layer, level: level as u8, tx, ty };
                                let _ = done_tx.send(Done::Tile { key, result: r.map_err(|e| e.to_string()) });
                            }
                        }
                    }
                    ctx.request_repaint();
                })
                .expect("spawn tile worker");
        }
        Self {
            job_tx,
            done_rx,
            wanted,
            options,
            textures: LruCache::new(NonZeroUsize::new(TEXTURE_CACHE).unwrap()),
            pending: HashSet::new(),
            failed: HashMap::new(),
            opened: Vec::new(),
            ready: Vec::new(),
            last_error: None,
        }
    }

    pub fn open(&self, layer: u64, uri: String) {
        let _ = self.job_tx.send(Job::Open { layer, uri });
    }

    /// Drain finished work. Returns layers that finished opening.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<(u64, Result<Arc<Cog>, String>)> {
        while let Ok(d) = self.done_rx.try_recv() {
            match d {
                Done::Opened { layer, result } => self.opened.push((layer, result)),
                Done::Tile { key, result } => match result {
                    Ok(t) => self.ready.push((key, t)),
                    Err(e) => {
                        self.pending.remove(&key);
                        self.last_error = Some(e.clone());
                        self.failed.insert(key, e);
                    }
                },
            }
        }
        // Upload a bounded number of textures per frame.
        let n = self.ready.len().min(UPLOADS_PER_FRAME);
        for (key, tile) in self.ready.drain(..n) {
            self.pending.remove(&key);
            let tex = tile.map(|t| {
                let img = egui::ColorImage::from_rgba_unmultiplied([t.w as usize, t.h as usize], &t.rgba);
                ctx.load_texture(format!("t{}-{}-{}-{}", key.layer, key.level, key.tx, key.ty), img, egui::TextureOptions::LINEAR)
            });
            self.textures.put(key, tex);
        }
        if !self.ready.is_empty() {
            ctx.request_repaint();
        }
        std::mem::take(&mut self.opened)
    }

    /// Declare the tiles needed this frame; queue fetches for missing ones.
    pub fn request(&mut self, wanted: Vec<(TileKey, Arc<Cog>)>) {
        {
            let mut w = self.wanted.lock().unwrap();
            w.clear();
            w.extend(wanted.iter().map(|(k, _)| *k));
        }
        // Forget pending tiles nobody wants any more (workers will skip them).
        let still: HashSet<TileKey> = wanted.iter().map(|(k, _)| *k).collect();
        self.pending.retain(|k| still.contains(k));

        type Batch = (Arc<Cog>, Vec<(u32, u32)>);
        let mut batches: HashMap<(u64, u8), Batch> = HashMap::new();
        for (key, cog) in wanted {
            if self.textures.contains(&key) || self.pending.contains(&key) || self.failed.contains_key(&key) {
                continue;
            }
            self.pending.insert(key);
            batches.entry((key.layer, key.level)).or_insert_with(|| (cog, Vec::new())).1.push((key.tx, key.ty));
        }
        for ((layer, level), (cog, tiles)) in batches {
            for chunk in tiles.chunks(BATCH) {
                let _ = self.job_tx.send(Job::Tiles { layer, cog: cog.clone(), level: level as usize, tiles: chunk.to_vec() });
            }
        }
    }

    /// Texture for a tile if loaded (`Some(None)` = loaded but empty).
    pub fn texture(&mut self, key: &TileKey) -> Option<Option<egui::TextureHandle>> {
        self.textures.get(key).cloned()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len() + self.ready.len()
    }

    pub fn background_transparent(&self) -> bool {
        self.options.lock().unwrap().background_transparent
    }

    /// Change decode options; cached tiles are re-fetched.
    pub fn set_background_transparent(&mut self, on: bool) {
        self.options.lock().unwrap().background_transparent = on;
        self.textures.clear();
        self.failed.clear();
    }

    pub fn forget_layer(&mut self, layer: u64) {
        let keys: Vec<TileKey> = self.textures.iter().map(|(k, _)| *k).filter(|k| k.layer == layer).collect();
        for k in keys {
            self.textures.pop(&k);
        }
        self.failed.retain(|k, _| k.layer != layer);
        self.pending.retain(|k| k.layer != layer);
    }

    pub fn retry_failed(&mut self) {
        self.failed.clear();
        self.last_error = None;
    }
}
