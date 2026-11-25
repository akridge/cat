//! Connection to a CAT server: sign-in, project list, project loading and
//! background annotation sync. All network calls run on worker threads; the
//! UI thread only exchanges messages, so a slow or dead network never
//! freezes the app.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cat_api::sync::{Plan, SyncState};
use cat_api::{ApiError, Client, Project, Snapshot, SyncItem, User};
use cat_core::Annotation;
use serde::{Deserialize, Serialize};

use crate::settings::data_dir;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerProfile {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
}

impl ServerProfile {
    pub fn is_prod(&self) -> bool {
        let n = self.name.to_lowercase();
        n.contains("prod") || n.contains("live")
    }
    fn key(&self) -> String {
        format!("{}|{}", self.url.trim_end_matches('/'), self.username)
    }
}

// --- Remembered sessions (Windows Credential Manager) ---------------------

const KEYRING_SERVICE: &str = "CAT desktop";

pub fn remember_session(p: &ServerProfile, cookie: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, &p.key()) {
        let _ = e.set_password(cookie);
    }
}

pub fn recall_session(p: &ServerProfile) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &p.key()).ok()?.get_password().ok()
}

pub fn forget_session(p: &ServerProfile) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, &p.key()) {
        let _ = e.delete_credential();
    }
}

// --- Messages between UI and workers -----------------------------------

enum Msg {
    SignedIn(Result<User, ApiError>, bool),
    Projects(Result<Vec<Project>, ApiError>),
    Snapshot(Result<Snapshot, ApiError>),
    Species(Vec<cat_core::species::Species>),
    Options(Vec<(String, Vec<(String, String)>)>),
    Synced(SyncRound),
}

struct SyncRound {
    project_id: i64,
    upserts: Vec<(String, u64, SyncItem)>,
    upsert_result: Result<Vec<cat_api::SyncOutcome>, ApiError>,
    deletes: Vec<(String, i64)>,
    delete_result: Result<Vec<i64>, ApiError>,
}

/// Open server project being annotated.
pub struct OpenProject {
    pub project: Project,
    pub can_edit: bool,
    pub role: String,
    pub asset_id: Option<i64>,
    pub sync: SyncState,
    pub journal: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncStatus {
    Idle,
    Syncing(usize),
    /// Network/server trouble: retry at the given time.
    Offline { queued: usize, retry_at: Instant, error: String },
}

pub enum Event {
    SignedIn,
    SignedOut(String),
    ProjectLoaded(Box<Snapshot>),
    ProjectFailed(String),
    SpeciesUpdated(Vec<cat_core::species::Species>),
    OptionsUpdated(Vec<(String, Vec<(String, String)>)>),
    /// Conflicts appeared during sync.
    Conflicts(usize),
}

pub struct Server {
    pub profile: Option<ServerProfile>,
    pub client: Option<Arc<Client>>,
    pub user: Option<User>,
    pub signing_in: bool,
    pub projects: Vec<Project>,
    pub projects_loading: bool,
    pub project_error: Option<String>,
    pub loading_project: Option<i64>,
    pub open: Option<OpenProject>,
    pub status: SyncStatus,
    pub last_synced: Option<Instant>,
    /// Set when a sync round finished (the journal should be rewritten).
    pub round_done: bool,
    backoff: u32,
    in_flight: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
}

fn spawn<F: FnOnce() -> Msg + Send + 'static>(tx: &Sender<Msg>, ctx: &egui::Context, f: F) {
    let tx = tx.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        ctx.request_repaint();
    });
}

pub fn journal_path(base_url: &str, project_id: i64) -> PathBuf {
    let host: String = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    data_dir().join("server").join(host).join(format!("project-{project_id}.json"))
}

#[derive(Serialize, Deserialize)]
struct Journal {
    items: Vec<Annotation>,
    sync: SyncState,
}

pub fn read_journal(path: &std::path::Path) -> Option<(Vec<Annotation>, SyncState)> {
    let text = std::fs::read_to_string(path).ok()?;
    let j: Journal = serde_json::from_str(&text).ok()?;
    Some((j.items, j.sync))
}

pub fn write_journal(path: &std::path::Path, items: &[Annotation], sync: &SyncState) -> std::io::Result<()> {
    let j = Journal { items: items.to_vec(), sync: sync.clone() };
    let text = serde_json::to_string(&j).map_err(std::io::Error::other)?;
    cat_core::store::write_atomic(path, text.as_bytes())
}

impl Server {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        Server {
            profile: None,
            client: None,
            user: None,
            signing_in: false,
            projects: Vec::new(),
            projects_loading: false,
            project_error: None,
            loading_project: None,
            open: None,
            status: SyncStatus::Idle,
            last_synced: None,
            round_done: false,
            backoff: 0,
            in_flight: false,
            tx,
            rx,
            ctx: ctx.clone(),
        }
    }

    pub fn connected(&self) -> bool {
        self.user.is_some()
    }

    /// Sign in with a password (remember = keep the session in the OS keychain).
    pub fn sign_in(&mut self, profile: ServerProfile, password: String, remember: bool) {
        let client = Arc::new(Client::new(&profile.url));
        self.client = Some(client.clone());
        self.profile = Some(profile.clone());
        self.signing_in = true;
        spawn(&self.tx, &self.ctx, move || {
            let r = client.login(&profile.username, &password);
            if let (Ok(_), true, Some(c)) = (&r, remember, client.session()) {
                remember_session(&profile, &c);
            }
            Msg::SignedIn(r, false)
        });
    }

    /// Resume a remembered session at startup, if any.
    pub fn resume(&mut self, profile: ServerProfile) -> bool {
        let Some(cookie) = recall_session(&profile) else { return false };
        let client = Arc::new(Client::new(&profile.url));
        client.set_session(Some(cookie));
        self.client = Some(client.clone());
        self.profile = Some(profile);
        self.signing_in = true;
        spawn(&self.tx, &self.ctx, move || Msg::SignedIn(client.me(), true));
        true
    }

    pub fn sign_out(&mut self) {
        if let (Some(c), Some(p)) = (self.client.clone(), self.profile.clone()) {
            forget_session(&p);
            std::thread::spawn(move || c.logout());
        }
        self.user = None;
        self.client = None;
        self.projects.clear();
        self.open = None;
        self.status = SyncStatus::Idle;
    }

    pub fn refresh_projects(&mut self, q: &str, scope: &str) {
        let Some(c) = self.client.clone() else { return };
        let (q, scope) = (q.to_string(), scope.to_string());
        self.projects_loading = true;
        spawn(&self.tx, &self.ctx, move || Msg::Projects(c.projects(&q, &scope, 200, 0).map(|p| p.projects)));
    }

    pub fn load_project(&mut self, project_id: i64) {
        let Some(c) = self.client.clone() else { return };
        self.loading_project = Some(project_id);
        spawn(&self.tx, &self.ctx, move || Msg::Snapshot(c.snapshot(project_id)));
    }

    fn fetch_reference(&self) {
        let Some(c) = self.client.clone() else { return };
        let c2 = c.clone();
        spawn(&self.tx, &self.ctx, move || Msg::Species(c.species().unwrap_or_default()));
        spawn(&self.tx, &self.ctx, move || Msg::Options(c2.field_options().unwrap_or_default()));
    }

    /// Drain worker messages.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::SignedIn(r, resumed) => {
                    self.signing_in = false;
                    match r {
                        Ok(u) => {
                            self.user = Some(u);
                            self.fetch_reference();
                            out.push(Event::SignedIn);
                        }
                        Err(e) => {
                            if resumed {
                                if let Some(p) = &self.profile {
                                    forget_session(p);
                                }
                            }
                            self.client = None;
                            out.push(Event::SignedOut(if resumed { String::new() } else { e.to_string() }));
                        }
                    }
                }
                Msg::Projects(r) => {
                    self.projects_loading = false;
                    match r {
                        Ok(p) => {
                            self.projects = p;
                            self.project_error = None;
                        }
                        Err(ApiError::Unauthorized) => {
                            self.user = None;
                            out.push(Event::SignedOut("Your session expired. Please sign in again.".into()));
                        }
                        Err(e) => self.project_error = Some(e.to_string()),
                    }
                }
                Msg::Snapshot(r) => {
                    self.loading_project = None;
                    match r {
                        Ok(s) => out.push(Event::ProjectLoaded(Box::new(s))),
                        Err(e) => out.push(Event::ProjectFailed(e.to_string())),
                    }
                }
                Msg::Species(s) if !s.is_empty() => out.push(Event::SpeciesUpdated(s)),
                Msg::Options(o) if !o.is_empty() => out.push(Event::OptionsUpdated(o)),
                Msg::Species(_) | Msg::Options(_) => {}
                Msg::Synced(round) => {
                    self.in_flight = false;
                    self.round_done = true;
                    if let Some(n) = self.finish_round(round) {
                        out.push(Event::Conflicts(n));
                    }
                }
            }
        }
        out
    }

    fn finish_round(&mut self, r: SyncRound) -> Option<usize> {
        let open = self.open.as_mut().filter(|o| o.project.project_id == r.project_id)?;
        let before = open.sync.conflicts.len();
        let mut error = None;
        match r.upsert_result {
            Ok(outcomes) => open.sync.apply_upserts(&r.upserts, outcomes),
            Err(e) => error = Some(e),
        }
        match r.delete_result {
            Ok(ids) => open.sync.apply_deletes(&r.deletes, &ids),
            Err(e) => error = error.or(Some(e)),
        }
        match error {
            Some(ApiError::Unauthorized) => {
                self.status = SyncStatus::Offline { queued: r.upserts.len() + r.deletes.len(), retry_at: Instant::now() + Duration::from_secs(3600), error: "Signed out — sign in again to sync".into() };
                self.user = None;
            }
            Some(e) => {
                self.backoff = (self.backoff + 1).min(6);
                let wait = Duration::from_secs(2u64.pow(self.backoff).min(60));
                self.status = SyncStatus::Offline { queued: r.upserts.len() + r.deletes.len(), retry_at: Instant::now() + wait, error: e.to_string() };
            }
            None => {
                self.backoff = 0;
                self.status = SyncStatus::Idle;
                self.last_synced = Some(Instant::now());
            }
        }
        let after = self.open.as_ref().map(|o| o.sync.conflicts.len()).unwrap_or(0);
        (after > before).then_some(after)
    }

    /// Start a sync round if there is anything to send and nothing in flight.
    pub fn maybe_sync(&mut self, items: &[Annotation], analyst: Option<String>) {
        if self.in_flight || self.user.is_none() {
            return;
        }
        if let SyncStatus::Offline { retry_at, .. } = &self.status {
            if Instant::now() < *retry_at {
                return;
            }
        }
        let (Some(client), Some(open)) = (self.client.clone(), self.open.as_ref()) else { return };
        if !open.can_edit {
            return;
        }
        let created_by = analyst.or_else(|| self.user.as_ref().map(|u| u.name().to_string()));
        let Plan { mut upserts, mut deletes } = open.sync.plan(items, open.asset_id, created_by.as_deref());
        if upserts.is_empty() && deletes.is_empty() {
            if !matches!(self.status, SyncStatus::Offline { .. }) {
                self.status = SyncStatus::Idle;
            }
            return;
        }
        upserts.truncate(cat_api::BATCH_MAX);
        deletes.truncate(500);
        self.status = SyncStatus::Syncing(upserts.len() + deletes.len());
        self.in_flight = true;
        let project_id = open.project.project_id;
        spawn(&self.tx, &self.ctx, move || {
            let items: Vec<SyncItem> = upserts.iter().map(|u| u.2.clone()).collect();
            let upsert_result = if items.is_empty() { Ok(Vec::new()) } else { client.batch_sync(project_id, &items) };
            let ids: Vec<i64> = deletes.iter().map(|d| d.1).collect();
            let delete_result = if ids.is_empty() { Ok(Vec::new()) } else { client.batch_delete(project_id, &ids) };
            Msg::Synced(SyncRound { project_id, upserts, upsert_result, deletes, delete_result })
        });
    }

    pub fn retry_now(&mut self) {
        if let SyncStatus::Offline { retry_at, .. } = &mut self.status {
            *retry_at = Instant::now();
        }
        if let Some(o) = self.open.as_mut() {
            o.sync.failed.clear();
        }
    }
}
