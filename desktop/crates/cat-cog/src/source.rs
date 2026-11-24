//! Byte-range access to a COG.
//!
//! CAT reads imagery only from the public GCS bucket, so the production
//! source is [`HttpSource`] (anonymous HTTPS range requests). [`MemSource`]
//! exists for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{CogError, Result};

/// Random access to the bytes of a remote file.
pub trait RangeSource: Send + Sync {
    /// Read exactly `len` bytes starting at `offset` (fewer only at EOF).
    fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>>;
    /// Human-readable location, for logs and errors.
    fn location(&self) -> &str;
}

/// Turn `gs://bucket/path` into the public HTTPS URL; pass `http(s)://` through.
pub fn resolve_url(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("gs://").or_else(|| uri.strip_prefix("Gs://")) {
        format!("https://storage.googleapis.com/{rest}")
    } else if let Some(rest) = uri.strip_prefix("/vsigs/") {
        format!("https://storage.googleapis.com/{rest}")
    } else {
        uri.to_string()
    }
}

/// Shared HTTP client. Uses the operating system's certificate store, so
/// agency TLS-inspection proxies trusted by Windows work without configuration,
/// and honours `HTTPS_PROXY`.
pub fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("cat-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(16)
        .build()
        .expect("HTTP client")
}

pub struct HttpSource {
    client: reqwest::blocking::Client,
    url: String,
}

impl HttpSource {
    pub fn new(client: reqwest::blocking::Client, uri: &str) -> Self {
        Self { client, url: resolve_url(uri) }
    }
}

impl RangeSource for HttpSource {
    fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let mut last_err = None;
        // Retry transient failures with a short backoff; field networks drop.
        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250 * 2u64.pow(attempt)));
            }
            match self.client.get(&self.url).header("Range", &range).send() {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 206 {
                        return resp.bytes().map(|b| b.to_vec()).map_err(|e| CogError::Http(e.to_string()));
                    }
                    if status.as_u16() == 200 {
                        // Server ignored Range: slice the body ourselves.
                        let body = resp.bytes().map_err(|e| CogError::Http(e.to_string()))?;
                        let start = (offset as usize).min(body.len());
                        let end = ((offset + len) as usize).min(body.len());
                        return Ok(body[start..end].to_vec());
                    }
                    if status.is_server_error() || status.as_u16() == 429 {
                        last_err = Some(CogError::Http(format!("{} for {}", status, self.url)));
                        continue;
                    }
                    return Err(CogError::Http(format!("{} for {}", status, self.url)));
                }
                Err(e) => last_err = Some(CogError::Http(e.to_string())),
            }
        }
        Err(last_err.unwrap_or_else(|| CogError::Http("request failed".into())))
    }

    fn location(&self) -> &str {
        &self.url
    }
}

/// In-memory source, for tests.
pub struct MemSource {
    pub data: Vec<u8>,
    pub name: String,
}

impl RangeSource for MemSource {
    fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let start = (offset as usize).min(self.data.len());
        let end = ((offset + len) as usize).min(self.data.len());
        Ok(self.data[start..end].to_vec())
    }
    fn location(&self) -> &str {
        &self.name
    }
}

/// Block cache used while parsing headers: TIFF metadata is many small,
/// scattered reads, which become a few large aligned requests.
pub struct BlockReader {
    src: Arc<dyn RangeSource>,
    block: u64,
    blocks: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
}

impl BlockReader {
    pub fn new(src: Arc<dyn RangeSource>, block: u64) -> Self {
        Self { src, block, blocks: Mutex::new(HashMap::new()) }
    }

    fn get_block(&self, idx: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.blocks.lock().unwrap().get(&idx) {
            return Ok(b.clone());
        }
        let data = Arc::new(self.src.read_range(idx * self.block, self.block)?);
        self.blocks.lock().unwrap().insert(idx, data.clone());
        Ok(data)
    }

    pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        let end = offset + len;
        let mut pos = offset;
        while pos < end {
            let idx = pos / self.block;
            let b = self.get_block(idx)?;
            let start_in = (pos - idx * self.block) as usize;
            if start_in >= b.len() {
                return Err(CogError::Format(format!("read past end of file at {pos}")));
            }
            let take = ((end - pos) as usize).min(b.len() - start_in);
            out.extend_from_slice(&b[start_in..start_in + take]);
            pos += take as u64;
        }
        Ok(out)
    }
}
