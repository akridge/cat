//! Batched tile fetching: neighbouring tiles are merged into a few large
//! HTTP range requests, fetched in parallel, then decoded in parallel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::cog::Cog;
use crate::decode::{decode_tile, DecodeOptions, DecodedTile};
use crate::error::Result;

/// Merge ranges whose gap is at most this many bytes. Fetching a little
/// unused data is far cheaper than another round trip.
const MAX_GAP: u64 = 256 * 1024;
/// Cap a single merged request so one slow response can't stall everything.
const MAX_MERGED: u64 = 16 * 1024 * 1024;
/// Parallel HTTP requests per batch.
const PARALLEL: usize = 8;

#[derive(Clone, Copy)]
struct Want {
    idx: usize,
    offset: u64,
    len: u64,
    is_mask: bool,
}

struct Group {
    start: u64,
    end: u64,
    members: Vec<Want>,
}

/// Raw bytes per requested tile: (image, mask).
type Raw = (Option<Vec<u8>>, Option<Vec<u8>>);
type Slot = Mutex<Option<TileResult>>;

pub type TileResult = ((u32, u32), Result<Option<DecodedTile>>);

impl Cog {
    /// Fetch and decode tiles of one level. `Ok(None)` = sparse tile (no data).
    pub fn fetch_tiles(&self, level: usize, tiles: &[(u32, u32)], opts: DecodeOptions) -> Vec<TileResult> {
        let lvl = &self.levels[level];
        let mask = self.mask_for(level);
        let across = lvl.tiles_across();

        let mut wants = Vec::new();
        for (i, &(tx, ty)) in tiles.iter().enumerate() {
            let k = (ty * across + tx) as usize;
            if let (Some(&o), Some(&n)) = (lvl.offsets.get(k), lvl.byte_counts.get(k)) {
                if n > 0 {
                    wants.push(Want { idx: i, offset: o, len: n, is_mask: false });
                }
            }
            if let Some(m) = mask {
                if let (Some(&o), Some(&n)) = (m.offsets.get(k), m.byte_counts.get(k)) {
                    if n > 0 {
                        wants.push(Want { idx: i, offset: o, len: n, is_mask: true });
                    }
                }
            }
        }
        wants.sort_by_key(|w| w.offset);

        let mut groups: Vec<Group> = Vec::new();
        for w in wants {
            let end = w.offset + w.len;
            match groups.last_mut() {
                Some(g) if w.offset <= g.end + MAX_GAP && end.max(g.end) - g.start <= MAX_MERGED => {
                    g.end = g.end.max(end);
                    g.members.push(w);
                }
                _ => groups.push(Group { start: w.offset, end, members: vec![w] }),
            }
        }

        let raws: Vec<Mutex<Result<Raw>>> = (0..tiles.len()).map(|_| Mutex::new(Ok((None, None)))).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..PARALLEL.min(groups.len()) {
                s.spawn(|| loop {
                    let gi = next.fetch_add(1, Ordering::Relaxed);
                    let Some(g) = groups.get(gi) else { break };
                    match self.source.read_range(g.start, g.end - g.start) {
                        Ok(buf) => {
                            for m in &g.members {
                                let a = (m.offset - g.start) as usize;
                                let b = (a + m.len as usize).min(buf.len());
                                let mut slot = raws[m.idx].lock().unwrap();
                                if let Ok(r) = slot.as_mut() {
                                    let bytes = buf[a.min(b)..b].to_vec();
                                    if m.is_mask { r.1 = Some(bytes) } else { r.0 = Some(bytes) }
                                }
                            }
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            for m in &g.members {
                                *raws[m.idx].lock().unwrap() = Err(crate::CogError::Http(msg.clone()));
                            }
                        }
                    }
                });
            }
        });

        // Decode in parallel (zstd + RGBA expansion is CPU-bound).
        let results: Vec<Slot> = (0..tiles.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(tiles.len().max(1));
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(tx, ty)) = tiles.get(i) else { break };
                    let raw = std::mem::replace(&mut *raws[i].lock().unwrap(), Ok((None, None)));
                    let res = raw.and_then(|(img, msk)| match img {
                        None => Ok(None),
                        Some(img) => {
                            let m = match (mask, msk.as_deref()) {
                                (Some(ml), Some(md)) => Some((ml, md)),
                                _ => None,
                            };
                            decode_tile(lvl, tx, ty, &img, m, &self.background, opts).map(Some)
                        }
                    });
                    *results[i].lock().unwrap() = Some(((tx, ty), res));
                });
            }
        });
        results.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
    }
}
