//! Offline-safe sync between the local annotation set and a CAT project.
//!
//! The local set is always the truth for the user: edits apply instantly and
//! are written to a journal file. A background task repeatedly sends what
//! differs from the last state the server confirmed (by fingerprint), using
//! `batch-sync` (idempotent by `client_uuid`) and `batch-delete`. Nothing is
//! lost if the network drops or the app is closed: the journal is merged
//! with the server snapshot on the next open.

use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

use cat_core::{AnnKind, Annotation, Geom};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{ServerAnnotation, SyncItem, SyncOutcome};

/// Last state the server confirmed for one annotation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Synced {
    pub id: i64,
    pub version: i64,
    /// Fingerprint of the content the server holds.
    pub fp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConflictKind {
    /// Someone else saved a newer version.
    Changed,
    /// Someone else deleted it.
    Deleted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    pub kind: ConflictKind,
    /// The server's current version (for `Changed`).
    pub server: Option<Annotation>,
    pub server_version: Option<i64>,
    pub server_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub synced: BTreeMap<String, Synced>,
    pub conflicts: BTreeMap<String, Conflict>,
    /// Items the server refused for another reason (validation etc.).
    #[serde(default)]
    pub failed: BTreeMap<String, String>,
}

/// Properties that don't count as a change.
const VOLATILE: &[&str] = &["updated_at"];

fn sendable_props(a: &Annotation) -> Map<String, Value> {
    let mut p = a.props.clone();
    p.remove("client_uuid");
    p.insert("type".into(), Value::String(a.kind.as_str().into()));
    p
}

pub fn fingerprint(a: &Annotation) -> u64 {
    let mut p = sendable_props(a);
    for k in VOLATILE {
        p.remove(*k);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.geom.to_geojson().to_string().hash(&mut h);
    // serde_json with preserve_order: sort keys for a stable hash.
    let sorted: BTreeMap<&String, &Value> = p.iter().collect();
    serde_json::to_string(&sorted).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// Convert a server row into a local annotation.
pub fn from_server(s: &ServerAnnotation) -> Option<Annotation> {
    let geom_v = s.feature.as_ref().and_then(|f| f.get("geometry")).or(s.geometry.as_ref())?;
    let geom = Geom::from_geojson(geom_v)?;
    let mut props = s.properties.clone();
    let uuid = s
        .client_uuid
        .clone()
        .or_else(|| props.get("client_uuid").and_then(|v| v.as_str()).map(str::to_string))
        // Rows created before client_uuid existed get a stable local id.
        .unwrap_or_else(|| format!("srv-{}", s.annotation_id));
    props.remove("client_uuid");
    if !props.contains_key("created_by") {
        if let Some(c) = s.creator_display_name.clone().or_else(|| s.created_by.clone()) {
            props.insert("created_by".into(), Value::String(c));
        }
    }
    let kind = AnnKind::parse(props.get("type").and_then(|v| v.as_str()).unwrap_or(""), &geom);
    Some(Annotation { uuid, kind, geom, props })
}

/// What to send now.
#[derive(Debug, Default)]
pub struct Plan {
    pub upserts: Vec<(String, u64, SyncItem)>,
    pub deletes: Vec<(String, i64)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty()
    }
}

impl SyncState {
    /// Number of local changes the server doesn't have yet.
    pub fn pending(&self, items: &[Annotation]) -> usize {
        let present: HashSet<&str> = items.iter().map(|a| a.uuid.as_str()).collect();
        let changed = items
            .iter()
            .filter(|a| !self.conflicts.contains_key(&a.uuid) && !self.failed.contains_key(&a.uuid))
            .filter(|a| self.synced.get(&a.uuid).is_none_or(|s| s.fp != fingerprint(a)))
            .count();
        let deleted = self.synced.keys().filter(|u| !present.contains(u.as_str()) && !self.conflicts.contains_key(*u)).count();
        changed + deleted
    }

    pub fn plan(&self, items: &[Annotation], asset_id: Option<i64>, created_by: Option<&str>) -> Plan {
        let mut plan = Plan::default();
        let present: HashSet<&str> = items.iter().map(|a| a.uuid.as_str()).collect();
        for a in items {
            if self.conflicts.contains_key(&a.uuid) || self.failed.contains_key(&a.uuid) {
                continue;
            }
            let fp = fingerprint(a);
            let synced = self.synced.get(&a.uuid);
            if synced.is_some_and(|s| s.fp == fp) {
                continue;
            }
            plan.upserts.push((
                a.uuid.clone(),
                fp,
                SyncItem {
                    annotation_id: synced.map(|s| s.id),
                    asset_id,
                    geometry: a.geom.to_geojson(),
                    properties: sendable_props(a),
                    created_by: created_by.map(str::to_string),
                    version: synced.map(|s| s.version),
                    client_uuid: a.uuid.clone(),
                },
            ));
        }
        for (uuid, s) in &self.synced {
            if !present.contains(uuid.as_str()) && !self.conflicts.contains_key(uuid) {
                plan.deletes.push((uuid.clone(), s.id));
            }
        }
        plan
    }

    /// Record the server's answers to a batch of upserts.
    pub fn apply_upserts(&mut self, sent: &[(String, u64, SyncItem)], outcomes: Vec<SyncOutcome>) {
        for ((uuid, fp, _), out) in sent.iter().zip(outcomes) {
            match out {
                SyncOutcome::Saved(row) => {
                    self.synced.insert(uuid.clone(), Synced { id: row.annotation_id, version: row.version.unwrap_or(1), fp: *fp });
                    self.failed.remove(uuid);
                }
                SyncOutcome::Conflict(row) => {
                    let server = row.as_ref().and_then(from_server);
                    self.conflicts.insert(
                        uuid.clone(),
                        Conflict {
                            kind: ConflictKind::Changed,
                            server,
                            server_version: row.as_ref().and_then(|r| r.version),
                            server_id: row.as_ref().map(|r| r.annotation_id),
                        },
                    );
                }
                SyncOutcome::Gone => {
                    self.conflicts.insert(uuid.clone(), Conflict { kind: ConflictKind::Deleted, server: None, server_version: None, server_id: None });
                }
                SyncOutcome::Failed { status, detail } => {
                    // 5xx/timeouts are retried by the caller; others need a human.
                    if !(500..600).contains(&status) {
                        self.failed.insert(uuid.clone(), format!("{status}: {detail}"));
                    }
                }
            }
        }
    }

    pub fn apply_deletes(&mut self, sent: &[(String, i64)], confirmed: &[i64]) {
        for (uuid, id) in sent {
            if confirmed.contains(id) {
                self.synced.remove(uuid);
            }
        }
    }

    /// Resolve a conflict by keeping the local version (overwrite the server).
    /// Returns a replacement uuid when the annotation must be re-created.
    pub fn keep_mine(&mut self, uuid: &str) -> Option<String> {
        let c = self.conflicts.remove(uuid)?;
        match c.kind {
            ConflictKind::Changed => {
                if let (Some(s), Some(v)) = (self.synced.get_mut(uuid), c.server_version) {
                    s.version = v;
                    s.fp = 0; // force resend
                }
                None
            }
            ConflictKind::Deleted => {
                // The deleted server row keeps this client_uuid; re-create under a new one.
                self.synced.remove(uuid);
                Some(uuid_v4())
            }
        }
    }

    /// Resolve a conflict by taking the server's version (or its deletion).
    /// Returns the annotation to put in place of the local one (None = remove).
    pub fn use_server(&mut self, uuid: &str) -> Option<Annotation> {
        let c = self.conflicts.remove(uuid)?;
        match c.kind {
            ConflictKind::Changed => {
                let server = c.server?;
                if let (Some(id), Some(v)) = (c.server_id, c.server_version) {
                    self.synced.insert(uuid.to_string(), Synced { id, version: v, fp: fingerprint(&server) });
                }
                Some(server)
            }
            ConflictKind::Deleted => {
                self.synced.remove(uuid);
                None
            }
        }
    }
}

fn uuid_v4() -> String {
    // cat-core already depends on uuid; go through a fresh annotation id.
    Annotation::new(AnnKind::Point, Geom::Point([0.0, 0.0]), Map::new()).uuid
}

/// Build the local set from a server snapshot, keeping any unsent local
/// work from a previous session's journal.
pub fn merge_snapshot(server: &[ServerAnnotation], journal: Option<(Vec<Annotation>, SyncState)>) -> (Vec<Annotation>, SyncState, usize) {
    let mut items: Vec<Annotation> = Vec::new();
    let mut st = SyncState::default();
    for s in server {
        if let Some(a) = from_server(s) {
            st.synced.insert(a.uuid.clone(), Synced { id: s.annotation_id, version: s.version.unwrap_or(1), fp: fingerprint(&a) });
            items.push(a);
        }
    }
    let mut kept = 0;
    if let Some((jitems, jst)) = journal {
        let jpresent: HashSet<&str> = jitems.iter().map(|a| a.uuid.as_str()).collect();
        for a in &jitems {
            let dirty = jst.synced.get(&a.uuid).is_none_or(|s| s.fp != fingerprint(a));
            if !dirty {
                continue;
            }
            kept += 1;
            match items.iter().position(|x| x.uuid == a.uuid) {
                Some(i) => {
                    let server_v = st.synced.get(&a.uuid).map(|s| s.version);
                    let base_v = jst.synced.get(&a.uuid).map(|s| s.version);
                    if server_v.is_some() && base_v.is_some() && server_v > base_v {
                        // Changed on the server since we last synced: ask the user.
                        let s = st.synced.get(&a.uuid).cloned();
                        st.conflicts.insert(
                            a.uuid.clone(),
                            Conflict { kind: ConflictKind::Changed, server: Some(items[i].clone()), server_version: s.as_ref().map(|s| s.version), server_id: s.map(|s| s.id) },
                        );
                    }
                    items[i] = a.clone();
                }
                None if jst.synced.contains_key(&a.uuid) => {
                    // We had synced it, but it's gone from the server now.
                    st.conflicts.insert(a.uuid.clone(), Conflict { kind: ConflictKind::Deleted, server: None, server_version: None, server_id: None });
                    items.push(a.clone());
                }
                None => items.push(a.clone()), // never reached the server: create
            }
        }
        // Local deletions not yet sent.
        for uuid in jst.synced.keys() {
            if !jpresent.contains(uuid.as_str()) {
                if let Some(i) = items.iter().position(|x| &x.uuid == uuid) {
                    items.remove(i);
                    kept += 1;
                }
            }
        }
    }
    (items, st, kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ann(spcode: &str) -> Annotation {
        let mut p = Map::new();
        p.insert("spcode".into(), json!(spcode));
        Annotation::new(AnnKind::Polygon, Geom::Polygon(vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]), p)
    }

    fn row(a: &Annotation, id: i64, version: i64) -> ServerAnnotation {
        ServerAnnotation {
            annotation_id: id,
            asset_id: None,
            feature: Some(json!({"type": "Feature", "geometry": a.geom.to_geojson(), "properties": {}})),
            geometry: None,
            properties: sendable_props(a),
            version: Some(version),
            client_uuid: Some(a.uuid.clone()),
            created_by: None,
            creator_display_name: None,
        }
    }

    #[test]
    fn plan_create_update_delete() {
        let a = ann("PLOB");
        let mut st = SyncState::default();
        let p = st.plan(std::slice::from_ref(&a), Some(7), None);
        assert_eq!(p.upserts.len(), 1);
        assert!(p.upserts[0].2.annotation_id.is_none());
        st.apply_upserts(&p.upserts, vec![SyncOutcome::Saved(row(&a, 11, 1))]);
        assert!(st.plan(std::slice::from_ref(&a), Some(7), None).is_empty());
        assert_eq!(st.pending(std::slice::from_ref(&a)), 0);

        // updated_at alone is not a change
        let mut b = a.clone();
        b.touch();
        assert!(st.plan(std::slice::from_ref(&b), None, None).is_empty());

        // a real edit is an update with id + version
        b.props.insert("spcode".into(), json!("PMEA"));
        let p = st.plan(std::slice::from_ref(&b), None, None);
        assert_eq!((p.upserts[0].2.annotation_id, p.upserts[0].2.version), (Some(11), Some(1)));

        // removing it locally plans a delete
        let p = st.plan(&[], None, None);
        assert_eq!(p.deletes, vec![(a.uuid.clone(), 11)]);
        st.apply_deletes(&p.deletes, &[11]);
        assert!(st.synced.is_empty());
    }

    #[test]
    fn conflict_resolution() {
        let a = ann("PLOB");
        let mut st = SyncState::default();
        st.synced.insert(a.uuid.clone(), Synced { id: 5, version: 1, fp: 0 });
        let mut theirs = a.clone();
        theirs.props.insert("spcode".into(), json!("MOSP"));
        let p = st.plan(std::slice::from_ref(&a), None, None);
        st.apply_upserts(&p.upserts, vec![SyncOutcome::Conflict(Some(row(&theirs, 5, 3)))]);
        assert_eq!(st.pending(std::slice::from_ref(&a)), 0, "conflicted items wait for the user");
        // keep mine → resend with the server's version
        let mut st2 = st.clone();
        assert!(st2.keep_mine(&a.uuid).is_none());
        let p = st2.plan(std::slice::from_ref(&a), None, None);
        assert_eq!(p.upserts[0].2.version, Some(3));
        // use server → server copy replaces local, nothing to send
        let got = st.use_server(&a.uuid).unwrap();
        assert_eq!(got.spcode(), "MOSP");
        assert!(st.plan(&[got], None, None).is_empty());
    }

    #[test]
    fn journal_merge_keeps_unsent_work() {
        let a = ann("PLOB");
        let b = ann("PMEA");
        let server = vec![row(&a, 1, 1), row(&b, 2, 1)];
        // Journal: a edited offline, b deleted offline, c created offline.
        let mut jst = SyncState::default();
        jst.synced.insert(a.uuid.clone(), Synced { id: 1, version: 1, fp: fingerprint(&a) });
        jst.synced.insert(b.uuid.clone(), Synced { id: 2, version: 1, fp: fingerprint(&b) });
        let mut a2 = a.clone();
        a2.props.insert("morph_code".into(), json!("MD"));
        let c = ann("POSP");
        let (items, st, kept) = merge_snapshot(&server, Some((vec![a2.clone(), c.clone()], jst)));
        assert_eq!(kept, 3);
        assert_eq!(items.len(), 2);
        assert!(items.contains(&a2) && items.contains(&c));
        let p = st.plan(&items, None, None);
        assert_eq!(p.upserts.len(), 2); // a2 update + c create
        assert_eq!(p.deletes, vec![(b.uuid.clone(), 2)]);
        assert!(st.conflicts.is_empty());
    }
}
