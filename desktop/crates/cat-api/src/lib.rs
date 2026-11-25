//! `cat-api` — blocking client for the CAT server (`cat/api/*.py`).
//!
//! Uses the server exactly as it is today: `POST /api/auth/login` returns a
//! session cookie, which this client sends back as a `Cookie` header. No
//! server changes are needed, so the same desktop build works against dev
//! and production deployments.

pub mod sync;

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not signed in or session expired")]
    Unauthorized,
    #[error("you don't have permission for this ({0})")]
    Forbidden(String),
    #[error("server error {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("can't reach the CAT server: {0}")]
    Network(String),
    #[error("unexpected response: {0}")]
    Parse(String),
}

impl ApiError {
    /// Worth retrying later (network trouble, server overloaded/restarting).
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Http { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

/// Network error with its underlying cause ("connection refused",
/// "invalid peer certificate", "dns error" …), which is what helps when
/// diagnosing agency networks and proxies.
fn net_err(e: reqwest::Error) -> ApiError {
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        let t = s.to_string();
        if !msg.contains(&t) {
            msg.push_str(": ");
            msg.push_str(&t);
        }
        src = s.source();
    }
    ApiError::Network(msg)
}

/// Field name → enabled (value, label) options.
pub type OptionLists = Vec<(String, Vec<(String, String)>)>;

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub user_id: i64,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: String,
}

impl User {
    pub fn name(&self) -> &str {
        self.display_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Project {
    pub project_id: i64,
    pub project_name: String,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub cruise: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub annotation_count: Option<i64>,
    #[serde(default)]
    pub owner_display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectPage {
    pub projects: Vec<Project>,
    #[serde(default)]
    pub total_count: i64,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub asset_id: i64,
    pub asset_name: String,
    pub cog_url: String,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub is_active: Option<i64>,
}

/// An annotation row as the server returns it.
#[derive(Clone, Debug, Deserialize)]
pub struct ServerAnnotation {
    pub annotation_id: i64,
    #[serde(default)]
    pub asset_id: Option<i64>,
    #[serde(default)]
    pub feature: Option<Value>,
    #[serde(default)]
    pub geometry: Option<Value>,
    #[serde(default)]
    pub properties: Map<String, Value>,
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub client_uuid: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub creator_display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub project: Project,
    #[serde(default)]
    pub my_role: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub annotations: Vec<ServerAnnotation>,
}

impl Snapshot {
    pub fn can_edit(&self) -> bool {
        matches!(self.my_role.as_str(), "owner" | "editor" | "admin")
    }
}

/// One item of `POST /annotations/batch-sync`.
#[derive(Clone, Debug)]
pub struct SyncItem {
    pub annotation_id: Option<i64>,
    pub asset_id: Option<i64>,
    pub geometry: Value,
    pub properties: Map<String, Value>,
    pub created_by: Option<String>,
    pub version: Option<i64>,
    pub client_uuid: String,
}

impl SyncItem {
    fn to_json(&self) -> Value {
        // Same shape as the web client's normalizeAnnotationForDb().
        let mut v = json!({
            "feature": {"type": "Feature", "geometry": self.geometry, "properties": {}},
            "properties": self.properties,
            "client_uuid": self.client_uuid,
        });
        let o = v.as_object_mut().unwrap();
        if let Some(id) = self.annotation_id {
            o.insert("annotation_id".into(), json!(id));
        }
        if let Some(a) = self.asset_id {
            o.insert("asset_id".into(), json!(a));
        }
        if let Some(c) = &self.created_by {
            o.insert("created_by".into(), json!(c));
        }
        if let Some(ver) = self.version {
            o.insert("version".into(), json!(ver));
        }
        v
    }
}

/// Result of one sync item.
#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Saved(ServerAnnotation),
    /// Someone else changed it (409); the server's current row.
    Conflict(Option<ServerAnnotation>),
    /// Deleted on the server (410).
    Gone,
    Failed { status: u16, detail: String },
}

pub const BATCH_MAX: usize = 100;

pub struct Client {
    base: String,
    http: reqwest::blocking::Client,
    cookie: Mutex<Option<String>>,
}

fn detail_of(v: &Value) -> String {
    match v.get("detail") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o.get("message").and_then(|m| m.as_str()).map(str::to_string).unwrap_or_else(|| Value::Object(o.clone()).to_string()),
        Some(other) => other.to_string(),
        None => v.to_string(),
    }
}

impl Client {
    /// `base` like `https://cat.example.gov` (no trailing `/api`).
    pub fn new(base: &str) -> Client {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("cat-desktop/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("HTTP client");
        Client { base: base.trim().trim_end_matches('/').to_string(), http, cookie: Mutex::new(None) }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Resume a saved session (`name=value` cookie string).
    pub fn set_session(&self, cookie: Option<String>) {
        *self.cookie.lock().unwrap() = cookie;
    }
    pub fn session(&self) -> Option<String> {
        self.cookie.lock().unwrap().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let req = match self.cookie.lock().unwrap().as_ref() {
            Some(c) => req.header("Cookie", c),
            None => req,
        };
        let resp = req.send().map_err(net_err)?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| ApiError::Network(e.to_string()))?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.chars().take(300).collect()));
        match status {
            200..=299 => Ok(body),
            401 => Err(ApiError::Unauthorized),
            403 => Err(ApiError::Forbidden(detail_of(&body))),
            _ => Err(ApiError::Http { status, detail: detail_of(&body) }),
        }
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.send(self.http.get(self.url(path)))
    }
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.post(self.url(path)).json(body))
    }

    fn parse<T: serde::de::DeserializeOwned>(v: Value, key: Option<&str>) -> Result<T> {
        let v = match key {
            Some(k) => v.get(k).cloned().ok_or_else(|| ApiError::Parse(format!("missing '{k}'")))?,
            None => v,
        };
        serde_json::from_value(v).map_err(|e| ApiError::Parse(e.to_string()))
    }

    /// Sign in; keeps the session cookie for later calls.
    pub fn login(&self, username: &str, password: &str) -> Result<User> {
        let resp = self
            .http
            .post(self.url("/api/auth/login"))
            .json(&json!({"username": username, "password": password}))
            .send()
            .map_err(net_err)?;
        let status = resp.status().as_u16();
        let cookie = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|h| h.to_str().ok())
            .filter_map(|s| s.split(';').next())
            .find(|kv| kv.contains('=') && !kv.ends_with('='))
            .map(str::to_string);
        let body: Value = resp.json().unwrap_or(Value::Null);
        match status {
            200..=299 => {
                let cookie = cookie.ok_or_else(|| ApiError::Parse("server did not return a session".into()))?;
                *self.cookie.lock().unwrap() = Some(cookie);
                Self::parse(body, Some("user"))
            }
            401 => Err(ApiError::Http { status, detail: "Invalid username or password".into() }),
            _ => Err(ApiError::Http { status, detail: detail_of(&body) }),
        }
    }

    pub fn logout(&self) {
        let _ = self.post("/api/auth/logout", &json!({}));
        *self.cookie.lock().unwrap() = None;
    }

    pub fn me(&self) -> Result<User> {
        Self::parse(self.get("/api/auth/me")?, Some("user"))
    }

    /// `scope`: "mine" (owned or collaborating) or "all".
    pub fn projects(&self, q: &str, scope: &str, limit: usize, offset: usize) -> Result<ProjectPage> {
        let req = self.http.get(self.url("/api/db/projects")).query(&[
            ("q", q),
            ("scope", scope),
            ("limit", &limit.to_string()),
            ("offset", &offset.to_string()),
            ("sort_by", "updated_at"),
            ("sort_dir", "desc"),
        ]);
        Self::parse(self.send(req)?, None)
    }

    pub fn snapshot(&self, project_id: i64) -> Result<Snapshot> {
        Self::parse(self.get(&format!("/api/db/projects/{project_id}/snapshot"))?, None)
    }

    /// Create/update up to [`BATCH_MAX`] annotations. One outcome per item.
    pub fn batch_sync(&self, project_id: i64, items: &[SyncItem]) -> Result<Vec<SyncOutcome>> {
        let body = json!({"items": items.iter().map(|i| i.to_json()).collect::<Vec<_>>()});
        let v = self.post(&format!("/api/db/projects/{project_id}/annotations/batch-sync"), &body)?;
        let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        if results.len() != items.len() {
            return Err(ApiError::Parse(format!("expected {} results, got {}", items.len(), results.len())));
        }
        Ok(results
            .into_iter()
            .map(|r| {
                let status = r.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
                match status {
                    200 => match r.get("annotation").cloned().map(serde_json::from_value::<ServerAnnotation>) {
                        Some(Ok(a)) => SyncOutcome::Saved(a),
                        _ => SyncOutcome::Failed { status, detail: "missing annotation in response".into() },
                    },
                    409 => SyncOutcome::Conflict(
                        r.get("detail").and_then(|d| d.get("current_annotation")).cloned().and_then(|a| serde_json::from_value(a).ok()),
                    ),
                    410 => SyncOutcome::Gone,
                    _ => SyncOutcome::Failed { status, detail: detail_of(&r) },
                }
            })
            .collect())
    }

    /// Soft-delete annotations. Returns the ids the server confirmed gone
    /// (deleted now, or already deleted: 404).
    pub fn batch_delete(&self, project_id: i64, ids: &[i64]) -> Result<Vec<i64>> {
        let v = self.post(&format!("/api/db/projects/{project_id}/annotations/batch-delete"), &json!({"annotation_ids": ids}))?;
        Ok(v.get("results")
            .and_then(|r| r.as_array())
            .map(|r| {
                r.iter()
                    .filter(|x| matches!(x.get("status").and_then(|s| s.as_u64()), Some(200) | Some(404)))
                    .filter_map(|x| x.get("annotation_id").and_then(|i| i.as_i64()))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Species list as the server serves it (team-lead enable/disable applied).
    pub fn species(&self) -> Result<Vec<cat_core::species::Species>> {
        let v = self.get("/api/coral/species")?;
        let list = v.get("species").and_then(|s| s.as_array()).cloned().unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|s| {
                let code = s.get("code")?.as_str()?.to_uppercase();
                let name = s.get("scientific_name").or_else(|| s.get("taxon_name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
                let inactive = s.get("inactive_flag").and_then(|f| f.as_i64()).unwrap_or(0) != 0;
                let enabled = s.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
                Some(cat_core::species::Species {
                    code,
                    name,
                    genus: s.get("genus").and_then(|g| g.as_str()).unwrap_or("").to_string(),
                    family: s.get("family").and_then(|g| g.as_str()).unwrap_or("").to_string(),
                    rank: String::new(),
                    active: !inactive && enabled,
                })
            })
            .collect())
    }

    /// Field option lists: field name → (value, label) of enabled options.
    pub fn field_options(&self) -> Result<OptionLists> {
        let v = self.get("/api/config/field-options")?;
        let fields = v.get("fields").and_then(|f| f.as_object()).cloned().unwrap_or_default();
        Ok(fields
            .into_iter()
            .map(|(k, opts)| {
                let list = opts
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true))
                            .filter_map(|o| {
                                let val = o.get("value")?.as_str()?.to_string();
                                let label = o.get("label").and_then(|l| l.as_str()).unwrap_or(&val).to_string();
                                Some((val, label))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (k, list)
            })
            .collect())
    }
}
