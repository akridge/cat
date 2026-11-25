//! Per-user settings and the portable data folder.
//!
//! Portable layout: everything lives in `data\` next to `CAT.exe`, so the app
//! can run from a USB stick. If that folder isn't writable (e.g. unzipped
//! into Program Files), fall back to `%LOCALAPPDATA%\CAT`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::editor::Style;

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CAT_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        let d = exe_dir.join("data");
        if std::fs::create_dir_all(&d).is_ok() {
            let probe = d.join(".write-test");
            if std::fs::write(&probe, b"ok").is_ok() {
                let _ = std::fs::remove_file(probe);
                return d;
            }
        }
    }
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("."));
    let d = base.join("CAT");
    let _ = std::fs::create_dir_all(&d);
    d
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Sticky form values (analyst, mission, transect, …).
    pub sticky: Map<String, Value>,
    /// Most recently used species codes, newest first (shown first in search).
    pub recent_species: Vec<String>,
    pub style: Style,
    pub snap: bool,
    pub show_table: bool,
    pub show_form: bool,
    pub dark: Option<bool>,
    /// Mosaics open at exit and the view, restored on start.
    pub last_layers: Vec<String>,
    pub last_view: Option<[f64; 3]>,
    pub hide_background: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sticky: Map::new(),
            recent_species: Vec::new(),
            style: Style::default(),
            snap: true,
            show_table: true,
            show_form: true,
            dark: None,
            last_layers: Vec::new(),
            last_view: None,
            hide_background: true,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        data_dir().join("settings.json")
    }
    pub fn load() -> Settings {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = cat_core::store::write_atomic(&Self::path(), t.as_bytes());
        }
    }
    pub fn use_species(&mut self, code: &str) {
        let code = code.trim().to_uppercase();
        if code.is_empty() {
            return;
        }
        self.recent_species.retain(|c| c != &code);
        self.recent_species.insert(0, code);
        self.recent_species.truncate(12);
    }
}
