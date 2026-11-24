//! Browse orthomosaic COGs in the public bucket.
//!
//! Phase 0 stand-in for the project picker: in Phase 1 projects come from the
//! CAT server (`/api/db/projects`) and their asset URIs point at the same files.

use std::sync::mpsc::{channel, Receiver};

use serde::Deserialize;

pub const BUCKET: &str = "nmfs_odp_pifsc";
pub const ROOT_PREFIX: &str = "PIFSC/ESD/ARP/StRS_Sites_Products/orthomosaic_cog/";

#[derive(Clone, Debug)]
pub struct CogEntry {
    /// Object name inside the bucket.
    pub name: String,
    pub size: u64,
}

impl CogEntry {
    pub fn uri(&self) -> String {
        format!("gs://{BUCKET}/{}", self.name)
    }
    pub fn file_name(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
    /// e.g. "BAK-2089" from "2026_BAK-2089_mos_cog.tif".
    pub fn site(&self) -> String {
        let f = self.file_name();
        f.split('_').nth(1).unwrap_or(f).to_string()
    }
}

#[derive(Deserialize)]
struct ListResp {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    prefixes: Vec<String>,
    #[serde(rename = "nextPageToken")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    name: String,
    size: String,
}

fn list(prefix: &str, delimiter: bool) -> anyhow::Result<(Vec<CogEntry>, Vec<String>)> {
    let client = cat_cog::source::http_client();
    let mut items = Vec::new();
    let mut prefixes = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut req = client
            .get(format!("https://storage.googleapis.com/storage/v1/b/{BUCKET}/o"))
            .query(&[("prefix", prefix), ("maxResults", "1000"), ("fields", "items(name,size),prefixes,nextPageToken")]);
        if delimiter {
            req = req.query(&[("delimiter", "/")]);
        }
        if let Some(t) = &token {
            req = req.query(&[("pageToken", t.as_str())]);
        }
        let resp: ListResp = serde_json::from_slice(&req.send()?.error_for_status()?.bytes()?)?;
        items.extend(
            resp.items
                .into_iter()
                .filter(|i| i.name.ends_with(".tif") || i.name.ends_with(".tiff"))
                .map(|i| CogEntry { size: i.size.parse().unwrap_or(0), name: i.name }),
        );
        prefixes.extend(resp.prefixes);
        match resp.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok((items, prefixes))
}

pub enum CatalogMsg {
    Years(Result<Vec<String>, String>),
    Files(String, Result<Vec<CogEntry>, String>),
}

pub struct Catalog {
    rx: Receiver<CatalogMsg>,
    tx: std::sync::mpsc::Sender<CatalogMsg>,
    pub years: Vec<String>,
    pub year: Option<String>,
    pub files: Vec<CogEntry>,
    pub loading: bool,
    pub error: Option<String>,
    pub filter: String,
}

impl Catalog {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        let c = Self { rx, tx, years: vec![], year: None, files: vec![], loading: true, error: None, filter: String::new() };
        let tx = c.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = list(ROOT_PREFIX, true).map_err(|e| e.to_string()).map(|(_, p)| {
                let mut y: Vec<String> =
                    p.iter().map(|s| s.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()).collect();
                y.sort();
                y.reverse();
                y
            });
            let _ = tx.send(CatalogMsg::Years(r));
            ctx.request_repaint();
        });
        c
    }

    pub fn select_year(&mut self, ctx: &egui::Context, year: String) {
        self.year = Some(year.clone());
        self.files.clear();
        self.loading = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = list(&format!("{ROOT_PREFIX}{year}/"), false).map(|(i, _)| i).map_err(|e| e.to_string());
            let _ = tx.send(CatalogMsg::Files(year, r));
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                CatalogMsg::Years(Ok(y)) => {
                    self.years = y;
                    if let Some(first) = self.years.first().cloned() {
                        self.select_year(ctx, first);
                    } else {
                        self.loading = false;
                    }
                }
                CatalogMsg::Years(Err(e)) | CatalogMsg::Files(_, Err(e)) => {
                    self.error = Some(e);
                    self.loading = false;
                }
                CatalogMsg::Files(year, Ok(f)) => {
                    if self.year.as_deref() == Some(year.as_str()) {
                        self.files = f;
                        self.loading = false;
                        self.error = None;
                    }
                }
            }
        }
    }
}
