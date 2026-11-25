//! End-to-end test against a real CAT server (Oracle backend).
//! Skipped unless CAT_TEST_SERVER is set, e.g.
//!   CAT_TEST_SERVER=http://127.0.0.1:8000 CAT_TEST_USER=admin CAT_TEST_PASSWORD=… cargo test -p cat-api

use cat_api::sync::{merge_snapshot, SyncState};
use cat_api::Client;
use cat_core::{AnnKind, Annotation, Geom};
use serde_json::{json, Map};

#[test]
fn login_snapshot_sync_roundtrip() {
    let Ok(base) = std::env::var("CAT_TEST_SERVER") else {
        eprintln!("CAT_TEST_SERVER not set; skipping");
        return;
    };
    let user = std::env::var("CAT_TEST_USER").unwrap_or_else(|_| "admin".into());
    let pass = std::env::var("CAT_TEST_PASSWORD").expect("CAT_TEST_PASSWORD");
    let c = Client::new(&base);
    let me = c.login(&user, &pass).expect("login");
    assert_eq!(me.username, user);
    assert_eq!(c.me().unwrap().user_id, me.user_id);

    let page = c.projects("", "mine", 50, 0).expect("projects");
    let project = page.projects.first().expect("at least one project");
    let snap = c.snapshot(project.project_id).expect("snapshot");
    assert!(snap.can_edit(), "role {}", snap.my_role);
    let asset = snap.assets.first().map(|a| a.asset_id);
    let (mut items, mut st, _) = merge_snapshot(&snap.annotations, None);
    let before = items.len();

    // Create two annotations and sync.
    let mut p = Map::new();
    p.insert("spcode".into(), json!("PLOB"));
    let a = Annotation::new(AnnKind::Polygon, Geom::Polygon(vec![[-162.348, 6.393], [-162.3479, 6.393], [-162.3479, 6.3931]]), p.clone());
    let b = Annotation::new(AnnKind::Point, Geom::Point([-162.348, 6.393]), Map::new());
    items.push(a.clone());
    items.push(b.clone());
    sync_all(&c, project.project_id, &items, &mut st, asset);
    assert_eq!(st.pending(&items), 0);

    // Sending the same create again is idempotent (client_uuid).
    let mut fresh = SyncState::default();
    let plan = fresh.plan(std::slice::from_ref(&a), asset, None);
    let out = c.batch_sync(project.project_id, &plan.upserts.iter().map(|x| x.2.clone()).collect::<Vec<_>>()).unwrap();
    fresh.apply_upserts(&plan.upserts, out);
    assert_eq!(fresh.synced[&a.uuid].id, st.synced[&a.uuid].id);

    // Edit one, delete the other, sync, then check the server's view.
    let ia = items.iter().position(|x| x.uuid == a.uuid).unwrap();
    items[ia].props.insert("morph_code".into(), json!("MD"));
    items.retain(|x| x.uuid != b.uuid);
    sync_all(&c, project.project_id, &items, &mut st, asset);
    let snap2 = c.snapshot(project.project_id).unwrap();
    let (items2, st2, _) = merge_snapshot(&snap2.annotations, None);
    assert_eq!(items2.len(), before + 1);
    let got = items2.iter().find(|x| x.uuid == a.uuid).expect("synced annotation");
    assert_eq!(got.text("morph_code"), "MD");
    assert_eq!(got.geom, a.geom);
    assert!(st2.plan(&items2, asset, None).is_empty());

    // A stale version is reported as a conflict, not overwritten.
    let mut stale = st.clone();
    stale.synced.get_mut(&a.uuid).unwrap().version = 1;
    stale.synced.get_mut(&a.uuid).unwrap().fp = 0;
    let mut mine = got.clone();
    mine.props.insert("morph_code".into(), json!("EN"));
    let plan = stale.plan(std::slice::from_ref(&mine), asset, None);
    let out = c.batch_sync(project.project_id, &plan.upserts.iter().map(|x| x.2.clone()).collect::<Vec<_>>()).unwrap();
    stale.apply_upserts(&plan.upserts, out);
    assert!(stale.conflicts.contains_key(&a.uuid), "expected a version conflict");

    // Clean up.
    items.retain(|x| x.uuid != a.uuid);
    sync_all(&c, project.project_id, &items, &mut st, asset);
    let snap3 = c.snapshot(project.project_id).unwrap();
    assert_eq!(snap3.annotations.len(), before);
}

fn sync_all(c: &Client, pid: i64, items: &[Annotation], st: &mut SyncState, asset: Option<i64>) {
    let plan = st.plan(items, asset, Some("cat-api test"));
    if !plan.upserts.is_empty() {
        let out = c.batch_sync(pid, &plan.upserts.iter().map(|x| x.2.clone()).collect::<Vec<_>>()).expect("batch_sync");
        st.apply_upserts(&plan.upserts, out);
    }
    if !plan.deletes.is_empty() {
        let ids: Vec<i64> = plan.deletes.iter().map(|d| d.1).collect();
        let ok = c.batch_delete(pid, &ids).expect("batch_delete");
        st.apply_deletes(&plan.deletes, &ok);
    }
    assert!(st.conflicts.is_empty(), "{:?}", st.conflicts.keys());
    assert!(st.failed.is_empty(), "{:?}", st.failed);
}
