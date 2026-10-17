//! `backfill`: derive a type for a history stored without it, as after a
//! `VERSION` bump.

use std::collections::BTreeMap;

use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Deriver, PathHistory, RootTree, SkipList};
use scion_server::{BACKFILL_TYPES, backfill};
use scion_store::{MemoryStore, MetadataStore};
use scion_types::{ChangesetId, RepoPath};

/// A history with a merge, stored with no derived data, `main` at its tip
/// and `release/1` at an older changeset.
async fn underived(store: &MemoryStore) -> Vec<ChangesetId> {
    let mut ids: Vec<ChangesetId> = Vec::new();
    for i in 0..50 {
        let path = RepoPath::new(format!("d{}/f{}", i % 3, i % 7)).unwrap();
        let changes = BTreeMap::from([(path, tracked(format!("v{i}").as_bytes()))]);
        let parents = match i {
            0 => vec![],
            30 => vec![ids[29], ids[10]],
            _ => vec![ids[i - 1]],
        };
        ids.push(
            store
                .put_changeset(&changeset(parents, changes, &i.to_string()))
                .await
                .unwrap(),
        );
    }
    store.create_ref(REPO, "main", ids[49]).await.unwrap();
    store.create_ref(REPO, "release/1", ids[20]).await.unwrap();
    ids
}

#[tokio::test]
async fn derives_every_changeset_reachable_from_the_refs() {
    let store = MemoryStore::with_repo(REPO);
    let ids = underived(&store).await;
    let d = Deriver::new(REPO, &store, &store);
    assert!(
        d.fetch_many::<RootTree>(&ids)
            .await
            .unwrap()
            .iter()
            .all(Option::is_none)
    );

    for ty in BACKFILL_TYPES {
        assert_eq!(backfill(REPO, ty, &store, &store).await.unwrap(), 2);
    }
    assert!(
        d.fetch_many::<RootTree>(&ids)
            .await
            .unwrap()
            .iter()
            .all(Option::is_some)
    );
    assert!(
        d.fetch_many::<PathHistory>(&ids)
            .await
            .unwrap()
            .iter()
            .all(Option::is_some)
    );
    let gens = d.fetch_many::<SkipList>(&ids).await.unwrap();
    assert_eq!(gens[49].as_ref().unwrap().generation, 49);

    // A second run finds everything stored and is a no-op.
    assert_eq!(
        backfill(REPO, "path_history", &store, &store)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn refuses_an_unknown_type_or_repository() {
    let store = MemoryStore::with_repo(REPO);
    underived(&store).await;
    let err = backfill(REPO, "blame", &store, &store).await.unwrap_err();
    assert!(err.contains("root_tree, path_history, skip_list"), "{err}");
    let err = backfill("nope", "root_tree", &store, &store)
        .await
        .unwrap_err();
    assert!(err.contains("has no refs"), "{err}");
}
