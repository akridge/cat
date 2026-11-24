//! Minimal TIFF / BigTIFF directory parser: just what a tiled COG needs.

use std::collections::BTreeMap;

use crate::error::{CogError, Result};
use crate::source::BlockReader;

pub mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const PLANAR_CONFIG: u16 = 284;
    pub const PREDICTOR: u16 = 317;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const EXTRA_SAMPLES: u16 = 338;
    pub const SAMPLE_FORMAT: u16 = 339;
    pub const JPEG_TABLES: u16 = 347;
    pub const MODEL_PIXEL_SCALE: u16 = 33550;
    pub const MODEL_TIEPOINT: u16 = 33922;
    pub const MODEL_TRANSFORMATION: u16 = 34264;
    pub const GEO_KEY_DIRECTORY: u16 = 34735;
    pub const GEO_DOUBLE_PARAMS: u16 = 34736;
    pub const GEO_ASCII_PARAMS: u16 = 34737;
    pub const GDAL_METADATA: u16 = 42112;
    pub const GDAL_NODATA: u16 = 42113;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        match self {
            Endian::Little => u16::from_le_bytes(a),
            Endian::Big => u16::from_be_bytes(a),
        }
    }
    pub fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            Endian::Little => u32::from_le_bytes(a),
            Endian::Big => u32::from_be_bytes(a),
        }
    }
    pub fn u64(self, b: &[u8]) -> u64 {
        let a: [u8; 8] = b[..8].try_into().unwrap();
        match self {
            Endian::Little => u64::from_le_bytes(a),
            Endian::Big => u64::from_be_bytes(a),
        }
    }
    pub fn f32(self, b: &[u8]) -> f32 {
        f32::from_bits(self.u32(b))
    }
    pub fn f64(self, b: &[u8]) -> f64 {
        f64::from_bits(self.u64(b))
    }
}

/// One directory entry with its value bytes already loaded.
#[derive(Clone, Debug)]
pub struct Entry {
    pub typ: u16,
    pub count: u64,
    pub data: Vec<u8>,
}

fn type_size(typ: u16) -> Option<u64> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return None,
    })
}

impl Entry {
    pub fn u64s(&self, e: Endian) -> Vec<u64> {
        let n = self.count as usize;
        match self.typ {
            1 | 7 => self.data.iter().take(n).map(|&v| v as u64).collect(),
            3 => self.data.chunks_exact(2).take(n).map(|c| e.u16(c) as u64).collect(),
            4 | 13 => self.data.chunks_exact(4).take(n).map(|c| e.u32(c) as u64).collect(),
            16 | 18 => self.data.chunks_exact(8).take(n).map(|c| e.u64(c)).collect(),
            _ => Vec::new(),
        }
    }
    pub fn f64s(&self, e: Endian) -> Vec<f64> {
        let n = self.count as usize;
        match self.typ {
            11 => self.data.chunks_exact(4).take(n).map(|c| e.f32(c) as f64).collect(),
            12 => self.data.chunks_exact(8).take(n).map(|c| e.f64(c)).collect(),
            5 => self
                .data
                .chunks_exact(8)
                .take(n)
                .map(|c| e.u32(&c[..4]) as f64 / e.u32(&c[4..]).max(1) as f64)
                .collect(),
            _ => self.u64s(e).into_iter().map(|v| v as f64).collect(),
        }
    }
    pub fn string(&self) -> String {
        let s = String::from_utf8_lossy(&self.data);
        s.trim_end_matches('\0').to_string()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Ifd {
    pub entries: BTreeMap<u16, Entry>,
}

impl Ifd {
    pub fn get(&self, t: u16) -> Option<&Entry> {
        self.entries.get(&t)
    }
    pub fn u64(&self, t: u16, e: Endian) -> Option<u64> {
        self.get(t).and_then(|x| x.u64s(e).first().copied())
    }
    pub fn u64s(&self, t: u16, e: Endian) -> Vec<u64> {
        self.get(t).map(|x| x.u64s(e)).unwrap_or_default()
    }
    pub fn f64s(&self, t: u16, e: Endian) -> Vec<f64> {
        self.get(t).map(|x| x.f64s(e)).unwrap_or_default()
    }
}

/// Parse the header and every IFD in the chain.
pub fn read_ifds(r: &BlockReader) -> Result<(Endian, bool, Vec<Ifd>)> {
    let hdr = r.read(0, 16)?;
    let endian = match &hdr[..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return Err(CogError::Format("not a TIFF file".into())),
    };
    let magic = endian.u16(&hdr[2..]);
    let big = match magic {
        42 => false,
        43 => true,
        _ => return Err(CogError::Format(format!("bad TIFF magic {magic}"))),
    };
    let mut next = if big { endian.u64(&hdr[8..]) } else { endian.u32(&hdr[4..]) as u64 };
    let mut ifds = Vec::new();
    while next != 0 {
        if ifds.len() > 64 {
            return Err(CogError::Format("too many IFDs".into()));
        }
        let (count_sz, entry_sz, off_sz) = if big { (8u64, 20u64, 8u64) } else { (2, 12, 4) };
        let n_raw = r.read(next, count_sz)?;
        let n = if big { endian.u64(&n_raw) } else { endian.u16(&n_raw) as u64 };
        let body = r.read(next + count_sz, n * entry_sz + off_sz)?;
        let mut ifd = Ifd::default();
        for i in 0..n as usize {
            let ent = &body[i * entry_sz as usize..(i + 1) * entry_sz as usize];
            let tag = endian.u16(&ent[0..]);
            let typ = endian.u16(&ent[2..]);
            let count = if big { endian.u64(&ent[4..]) } else { endian.u32(&ent[4..]) as u64 };
            let Some(sz) = type_size(typ) else { continue };
            let total = sz * count;
            let inline_cap = if big { 8 } else { 4 };
            let data = if total <= inline_cap {
                let v = if big { &ent[12..20] } else { &ent[8..12] };
                v[..total as usize].to_vec()
            } else {
                let off = if big { endian.u64(&ent[12..]) } else { endian.u32(&ent[8..]) as u64 };
                r.read(off, total)?
            };
            ifd.entries.insert(tag, Entry { typ, count, data });
        }
        let tail = &body[(n * entry_sz) as usize..];
        next = if big { endian.u64(tail) } else { endian.u32(tail) as u64 };
        ifds.push(ifd);
    }
    Ok((endian, big, ifds))
}
