//! Conformance suite every store implementation must pass.
//!
//! Use [`conformance_tests!`](crate::conformance_tests) to instantiate the
//! suite for a concrete store:
//!
//! ```ignore
//! scion_store::conformance_tests!(my_store_conformance, || MyStore::new());
//! ```

use std::collections::BTreeMap;

use bytes::Bytes;
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, PathComponent, RepoPath,
    Timestamp, Tree, TreeEntry,
};

use crate::{
    ChangeRequestStatus, Grant, LandOutcome, MetadataStore, ObjectStore, Role, SERVER_GRANTS,
    StoreError,
};

const REPO: &str = "conformance";

fn changeset(parents: Vec<ChangesetId>, message: &str) -> Changeset {
    let mut file_changes = BTreeMap::new();
    file_changes.insert(
        RepoPath::new("file.txt").unwrap(),
        FileChange::Tracked {
            content: BlobId::hash(message.as_bytes()),
            kind: FileType::Regular,
            size: 1,
            copy_from: None,
        },
    );
    Changeset {
        repo: REPO.into(),
        parents,
        author: Identity::new("t", "t@example.com"),
        author_date: Timestamp::new(0, 0),
        committer: None,
        committer_date: None,
        message: message.into(),
        file_changes,
        extra: BTreeMap::new(),
    }
}

/// Only [`MetadataStore::create_repo`] creates a repository; every other
/// write names an existing one.
pub async fn repos<S: MetadataStore>(s: &S) {
    let missing =
        |r: Result<_, StoreError>| matches!(r, Err(StoreError::RepoNotFound(n)) if n == REPO);
    let root = changeset(vec![], "root");
    let id = root.id();
    assert!(missing(s.put_changeset(&root).await.map(|_| ())));
    assert!(missing(s.create_ref(REPO, "main", id).await.map(|_| ())));
    assert!(missing(s.move_ref(REPO, "main", 0, id).await.map(|_| ())));
    assert!(missing(
        s.put_derived(REPO, "root_tree", 1, id, vec![1]).await
    ));
    assert!(missing(
        s.put_ingest_checkpoints(REPO, &[(vec![0xaa; 20], id)])
            .await
    ));
    assert!(s.get_changeset(REPO, id).await.unwrap().is_none());

    s.create_repo(REPO).await.unwrap();
    s.create_repo(REPO).await.unwrap();
    s.put_changeset(&root).await.unwrap();
    s.create_ref(REPO, "main", id).await.unwrap();
    assert!(matches!(
        s.move_ref(REPO, "release/1", 0, id).await,
        Err(StoreError::RefNotFound(_))
    ));
}

/// Changesets are content-addressed, idempotent, and track generation.
pub async fn changesets<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    let root = changeset(vec![], "root");
    let root_id = s.put_changeset(&root).await.unwrap();
    assert_eq!(root_id, root.id());
    assert_eq!(s.put_changeset(&root).await.unwrap(), root_id, "idempotent");
    assert_eq!(
        s.get_changeset(REPO, root_id).await.unwrap().as_ref(),
        Some(&root)
    );
    assert_eq!(s.generation(REPO, root_id).await.unwrap(), Some(0));

    let child = changeset(vec![root_id], "child");
    let child_id = s.put_changeset(&child).await.unwrap();
    assert_eq!(s.generation(REPO, child_id).await.unwrap(), Some(1));

    let unknown = ChangesetId::from_bytes([7; 32]);
    assert!(s.get_changeset(REPO, unknown).await.unwrap().is_none());
    assert!(
        s.get_changeset("other-repo", root_id)
            .await
            .unwrap()
            .is_none(),
        "repo-scoped"
    );

    let orphan = changeset(vec![unknown], "orphan");
    assert!(
        matches!(s.put_changeset(&orphan).await, Err(StoreError::UnknownParent(_, p)) if p == unknown)
    );

    // A batch may name parents earlier in it and repeat stored changesets.
    let a = changeset(vec![child_id], "batch a");
    let b = changeset(vec![a.id()], "batch b");
    let merge = changeset(vec![b.id(), root_id], "batch merge");
    s.put_changesets(&[child, a, b.clone(), merge.clone()])
        .await
        .unwrap();
    assert_eq!(s.generation(REPO, b.id()).await.unwrap(), Some(3));
    assert_eq!(s.generation(REPO, merge.id()).await.unwrap(), Some(4));
    assert_eq!(
        s.get_changeset(REPO, merge.id()).await.unwrap().as_ref(),
        Some(&merge)
    );
    assert!(matches!(
        s.put_changesets(&[changeset(vec![unknown], "batched orphan")]).await,
        Err(StoreError::UnknownParent(_, p)) if p == unknown
    ));
    assert_eq!(
        s.get_changesets(REPO, &[merge.id(), unknown, root_id, merge.id()])
            .await
            .unwrap(),
        [Some(merge.clone()), None, Some(root), Some(merge)],
        "in order, repeats and unknown ids included"
    );
    assert_eq!(
        s.get_changesets("other-repo", &[root_id]).await.unwrap(),
        [None],
        "repo-scoped"
    );
}

/// Two writers storing the same batch at once both succeed and leave one
/// copy: the importer and a client may upload the same history concurrently.
pub async fn concurrent_changesets<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    let mut parent = s.put_changeset(&changeset(vec![], "root")).await.unwrap();
    // Several rounds, so the two writers overlap in at least one of them.
    for round in 0..8 {
        let mut batch = Vec::new();
        for i in 0..4 {
            let cs = changeset(vec![parent], &format!("round {round} child {i}"));
            parent = cs.id();
            batch.push(cs);
        }
        let (a, b) = tokio::join!(s.put_changesets(&batch), s.put_changesets(&batch));
        a.unwrap();
        b.unwrap();
        let ids: Vec<ChangesetId> = batch.iter().map(Changeset::id).collect();
        assert_eq!(
            s.get_changesets(REPO, &ids).await.unwrap(),
            batch.into_iter().map(Some).collect::<Vec<_>>()
        );
        assert_eq!(
            s.generation(REPO, parent).await.unwrap(),
            Some(4 * round + 4)
        );
    }
}

/// Refs: policy, create-once, CAS semantics, listing.
pub async fn refs<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    let a = ChangesetId::from_bytes([1; 32]);
    let b = ChangesetId::from_bytes([2; 32]);

    assert!(matches!(
        s.create_ref(REPO, "feature/x", a).await,
        Err(StoreError::RefNameNotAllowed(_))
    ));
    assert!(s.get_ref(REPO, "main").await.unwrap().is_none());

    let st = s.create_ref(REPO, "main", a).await.unwrap();
    assert_eq!(st.target, a);
    assert_eq!(st.version, 0);
    assert!(matches!(
        s.create_ref(REPO, "main", a).await,
        Err(StoreError::RefExists(_))
    ));

    assert!(matches!(
        s.move_ref(REPO, "main", 5, b).await,
        Err(StoreError::CasMismatch {
            expected: 5,
            actual: 0
        })
    ));
    let st = s.move_ref(REPO, "main", 0, b).await.unwrap();
    assert_eq!(st.target, b);
    assert_eq!(st.version, 1);
    assert_eq!(s.get_ref(REPO, "main").await.unwrap(), Some(st));
    assert!(matches!(
        s.move_ref(REPO, "nope", 0, b).await,
        Err(StoreError::RefNotFound(_))
    ));

    s.create_ref(REPO, "release/1.0", a).await.unwrap();
    let names: Vec<_> = s
        .list_refs(REPO)
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["main", "release/1.0"]);
    assert_eq!(s.list_refs("other-repo").await.unwrap().len(), 0);
}

/// Derived-data pointers are keyed by (repo, type, version, changeset).
pub async fn derived<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    let cs = ChangesetId::from_bytes([3; 32]);
    assert!(
        s.get_derived(REPO, "root_tree", 1, cs)
            .await
            .unwrap()
            .is_none()
    );
    s.put_derived(REPO, "root_tree", 1, cs, vec![9, 9])
        .await
        .unwrap();
    assert_eq!(
        s.get_derived(REPO, "root_tree", 1, cs).await.unwrap(),
        Some(vec![9, 9])
    );
    let other = ChangesetId::from_bytes([4; 32]);
    s.put_derived_many(
        REPO,
        vec![
            ("root_tree".into(), 1, other, vec![1]),
            ("skip_list".into(), 1, other, vec![2]),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        s.get_derived(REPO, "skip_list", 1, other).await.unwrap(),
        Some(vec![2])
    );
    assert_eq!(
        s.get_derived(REPO, "root_tree", 1, other).await.unwrap(),
        Some(vec![1])
    );
    assert!(
        s.get_derived(REPO, "root_tree", 2, cs)
            .await
            .unwrap()
            .is_none(),
        "versioned"
    );
    assert!(
        s.get_derived(REPO, "blame", 1, cs).await.unwrap().is_none(),
        "typed"
    );
    let unknown = ChangesetId::from_bytes([5; 32]);
    assert_eq!(
        s.get_derived_many(REPO, "root_tree", 1, &[other, unknown, cs])
            .await
            .unwrap(),
        [Some(vec![1]), None, Some(vec![9, 9])],
        "in order"
    );
    assert_eq!(
        s.get_derived_many("other-repo", "root_tree", 1, &[cs])
            .await
            .unwrap(),
        [None],
        "repo-scoped"
    );
}

/// Ingest checkpoints are per repository and idempotent: the first record of
/// a git sha wins.
pub async fn checkpoints<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    assert_eq!(s.ingest_checkpoints(REPO).await.unwrap(), vec![]);
    let (a, b) = (
        ChangesetId::from_bytes([1; 32]),
        ChangesetId::from_bytes([2; 32]),
    );
    s.put_ingest_checkpoints(REPO, &[(vec![0xaa; 20], a), (vec![0xbb; 20], b)])
        .await
        .unwrap();
    s.put_ingest_checkpoints(REPO, &[(vec![0xaa; 20], b)])
        .await
        .unwrap();
    assert_eq!(
        s.ingest_checkpoints(REPO).await.unwrap(),
        vec![(vec![0xaa; 20], a), (vec![0xbb; 20], b)]
    );
    assert_eq!(
        s.ingest_checkpoints("other").await.unwrap(),
        vec![],
        "scoped by repo"
    );
}

/// Grants are per repository, replace the role of the same principal and
/// prefix, and can be stored under [`SERVER_GRANTS`], which no repository
/// can take.
pub async fn grants<S: MetadataStore>(s: &S) {
    let grant = |principal: &str, path_prefix: &str, role| Grant {
        principal: principal.to_owned(),
        path_prefix: path_prefix.to_owned(),
        role,
    };
    let alice = "github:alice";
    assert!(matches!(
        s.put_grant(REPO, &grant(alice, "", Role::Reader)).await,
        Err(StoreError::RepoNotFound(n)) if n == REPO
    ));
    assert!(matches!(
        s.create_repo(SERVER_GRANTS).await,
        Err(StoreError::ReservedRepoName(_))
    ));
    assert!(matches!(
        s.create_ref(SERVER_GRANTS, "main", ChangesetId::from_bytes([1; 32]))
            .await,
        Err(StoreError::RepoNotFound(_))
    ));

    s.create_repo(REPO).await.unwrap();
    s.create_repo("other").await.unwrap();
    assert_eq!(s.grants_for(REPO, alice).await.unwrap(), vec![]);
    s.put_grant(REPO, &grant(alice, "lib", Role::Writer))
        .await
        .unwrap();
    s.put_grant(REPO, &grant(alice, "", Role::Writer))
        .await
        .unwrap();
    s.put_grant(REPO, &grant(alice, "", Role::Reader))
        .await
        .unwrap();
    s.put_grant(REPO, &grant("sp:ci", "", Role::Admin))
        .await
        .unwrap();
    s.put_grant(SERVER_GRANTS, &grant(alice, "", Role::Admin))
        .await
        .unwrap();
    assert_eq!(
        s.grants_for(REPO, alice).await.unwrap(),
        vec![
            grant(alice, "", Role::Reader),
            grant(alice, "lib", Role::Writer)
        ],
        "sorted by prefix, the later role wins"
    );
    assert_eq!(
        s.grants_for(SERVER_GRANTS, alice).await.unwrap(),
        vec![grant(alice, "", Role::Admin)]
    );
    assert_eq!(
        s.grants_for("other", alice).await.unwrap(),
        vec![],
        "scoped by repo"
    );
    assert_eq!(s.grants_for("missing", alice).await.unwrap(), vec![]);

    assert!(s.delete_grant(REPO, alice, "lib").await.unwrap());
    assert!(!s.delete_grant(REPO, alice, "lib").await.unwrap());
    assert!(!s.delete_grant("other", alice, "").await.unwrap());
    assert_eq!(
        s.grants_for(REPO, alice).await.unwrap(),
        vec![grant(alice, "", Role::Reader)]
    );
    assert_eq!(
        s.grants_for(REPO, "sp:ci").await.unwrap(),
        vec![grant("sp:ci", "", Role::Admin)]
    );
}

/// Change requests are claimed oldest first, finish only from `landing`,
/// and belong to one repository.
pub async fn change_requests<S: MetadataStore>(s: &S) {
    use ChangeRequestStatus::{Landed, Landing, NeedsRebase, Pending, Rejected};
    let cs = |b: u8| ChangesetId::from_bytes([b; 32]);
    s.create_repo(REPO).await.unwrap();
    assert_eq!(s.claim_next_change_request(REPO).await.unwrap(), None);
    let landing = || async {
        s.landing_change_request(REPO)
            .await
            .unwrap()
            .map(|cr| cr.id)
    };

    let mut ids = Vec::new();
    for b in 1..=4 {
        let cr = s
            .submit_change_request(REPO, cs(b), cs(0), "sp:test")
            .await
            .unwrap();
        assert_eq!((cr.head, cr.base, cr.status), (cs(b), cs(0), Pending));
        assert_eq!((cr.landed, cr.conflicts.len()), (None, 0));
        assert_eq!(cr.submitter, "sp:test");
        assert_eq!(
            s.get_change_request(REPO, cr.id).await.unwrap(),
            Some(cr.clone())
        );
        ids.push(cr.id);
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids follow submission");

    // Only a `landing` request finishes; a refused finish changes nothing.
    assert!(matches!(
        s.finish_change_request(REPO, ids[0], LandOutcome::Rejected)
            .await,
        Err(StoreError::ChangeRequestNotLanding {
            status: Pending,
            ..
        })
    ));
    assert_eq!(landing().await, None, "pending is not landing");
    let mut claimed = Vec::new();
    for want in &ids {
        let cr = s.claim_next_change_request(REPO).await.unwrap().unwrap();
        assert_eq!((cr.id, cr.status), (*want, Landing), "oldest first");
        claimed.push(cr);
    }
    assert_eq!(s.claim_next_change_request(REPO).await.unwrap(), None);
    assert_eq!(landing().await, Some(ids[0]), "the oldest landing request");

    let landed = s
        .finish_change_request(REPO, ids[0], LandOutcome::Landed(cs(7)))
        .await
        .unwrap();
    assert_eq!((landed.status, landed.landed), (Landed, Some(cs(7))));
    let conflicts = vec!["a/b.rs".to_owned(), "c.txt".to_owned()];
    let conflicted = s
        .finish_change_request(REPO, ids[1], LandOutcome::NeedsRebase(conflicts.clone()))
        .await
        .unwrap();
    assert_eq!(
        (conflicted.status, &conflicted.conflicts),
        (NeedsRebase, &conflicts)
    );
    let rejected = s
        .finish_change_request(REPO, ids[2], LandOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!((rejected.status, rejected.landed), (Rejected, None));
    for (cr, status) in [
        (&landed, Landed),
        (&conflicted, NeedsRebase),
        (&rejected, Rejected),
    ] {
        assert_eq!(
            s.get_change_request(REPO, cr.id).await.unwrap().as_ref(),
            Some(cr)
        );
        assert!(matches!(
            s.finish_change_request(REPO, cr.id, LandOutcome::Landed(cs(8))).await,
            Err(StoreError::ChangeRequestNotLanding { status: st, .. }) if st == status
        ));
        assert_eq!(
            s.get_change_request(REPO, cr.id).await.unwrap().as_ref(),
            Some(cr)
        );
    }
    assert!(matches!(
        s.finish_change_request(REPO, u64::from(u32::MAX), LandOutcome::Rejected)
            .await,
        Err(StoreError::ChangeRequestNotFound(_))
    ));
    assert_eq!(
        s.get_change_request(REPO, ids[3])
            .await
            .unwrap()
            .map(|cr| cr.status),
        Some(Landing)
    );
    assert_eq!(
        landing().await,
        Some(ids[3]),
        "finished requests are skipped"
    );
}

/// Listing returns the newest requests first, of one status or of any, and
/// only of the repository asked.
pub async fn list_change_requests<S: MetadataStore>(s: &S) {
    use ChangeRequestStatus::{Landed, Landing, NeedsRebase, Pending, Rejected};
    const N: u32 = 1_000;
    s.create_repo(REPO).await.unwrap();
    s.create_repo("conformance-other").await.unwrap();
    let base = ChangesetId::from_bytes([0; 32]);
    let mut model = Vec::new();
    for i in 0..N {
        let mut head = [0; 32];
        head[..4].copy_from_slice(&i.to_be_bytes());
        let cr = s
            .submit_change_request(REPO, ChangesetId::from_bytes(head), base, "sp:test")
            .await
            .unwrap();
        model.push((cr.id, Pending));
    }
    // The oldest 600 finish in turn as landed, needs_rebase and rejected; 10
    // more stay claimed.
    for (i, (id, status)) in model.iter_mut().take(610).enumerate() {
        let cr = s.claim_next_change_request(REPO).await.unwrap().unwrap();
        assert_eq!(cr.id, *id);
        *status = Landing;
        let outcome = match i {
            600.. => continue,
            _ if i % 3 == 0 => LandOutcome::Landed(cr.head),
            _ if i % 3 == 1 => LandOutcome::NeedsRebase(vec!["f".to_owned()]),
            _ => LandOutcome::Rejected,
        };
        *status = s
            .finish_change_request(REPO, *id, outcome)
            .await
            .unwrap()
            .status;
    }

    for (status, limit) in [
        (None, 25),
        (None, N + 1),
        (Some(Pending), 7),
        (Some(Landing), 100),
        (Some(Landed), 5),
        (Some(NeedsRebase), 200),
        (Some(Rejected), 1),
        (Some(Pending), 0),
    ] {
        let listed = s.list_change_requests(REPO, status, limit).await.unwrap();
        let want: Vec<_> = model
            .iter()
            .rev()
            .filter(|(_, st)| status.is_none_or(|want| *st == want))
            .take(limit as usize)
            .copied()
            .collect();
        let got: Vec<_> = listed.iter().map(|cr| (cr.id, cr.status)).collect();
        assert_eq!(got, want, "{status:?}, limit {limit}");
        assert!(
            listed
                .windows(2)
                .all(|w| w[0].submitted_at >= w[1].submitted_at),
            "newest first"
        );
        for cr in &listed {
            assert_eq!(
                s.get_change_request(REPO, cr.id).await.unwrap().as_ref(),
                Some(cr)
            );
        }
    }
    assert_eq!(
        s.list_change_requests("conformance-other", None, N)
            .await
            .unwrap(),
        vec![],
        "scoped by repo"
    );
    assert!(matches!(
        s.list_change_requests("conformance-missing", None, N).await,
        Err(StoreError::RepoNotFound(_))
    ));
}

/// A landed changeset is found from the request's head and from the
/// changeset it landed as; nothing else is.
pub async fn find_landings<S: MetadataStore>(s: &S) {
    const OTHER: &str = "conformance-other";
    let cs = |b: u8| ChangesetId::from_bytes([b; 32]);
    s.create_repo(REPO).await.unwrap();
    s.create_repo(OTHER).await.unwrap();
    let finish = async |head: ChangesetId, outcome: LandOutcome| {
        let cr = s
            .submit_change_request(REPO, head, cs(0), "sp:test")
            .await
            .unwrap();
        assert_eq!(
            s.claim_next_change_request(REPO).await.unwrap().unwrap().id,
            cr.id
        );
        s.finish_change_request(REPO, cr.id, outcome).await.unwrap()
    };
    // 1 lands rebased as 11, 2 lands as itself, 3 conflicts, 1 again has
    // nothing left to land and 4 changes a path its submitter may not.
    let rebased = finish(cs(1), LandOutcome::Landed(cs(11))).await;
    let forward = finish(cs(2), LandOutcome::Landed(cs(2))).await;
    finish(cs(3), LandOutcome::NeedsRebase(vec!["f".to_owned()])).await;
    finish(cs(1), LandOutcome::Rejected).await;
    let outside = vec!["app/b".to_owned()];
    let denied = finish(cs(4), LandOutcome::Denied(outside.clone())).await;
    assert_eq!(
        (denied.status, denied.landed, denied.conflicts),
        (ChangeRequestStatus::Rejected, None, outside),
        "denied paths are kept"
    );
    for cr in [&rebased, &forward] {
        let at = cr.landed_at.expect("landed_at set");
        assert!(at >= cr.submitted_at);
    }

    let asked = [cs(3), cs(11), cs(9), cs(2), cs(1)];
    assert_eq!(
        s.find_landings(REPO, &asked).await.unwrap(),
        [
            (cs(11), rebased.clone()),
            (cs(2), forward.clone()),
            (cs(1), rebased.clone()),
        ],
        "in the order asked, from the head or the landed id"
    );
    assert_eq!(s.find_landings(REPO, &[]).await.unwrap(), []);
    assert_eq!(
        s.find_landings(OTHER, &asked).await.unwrap(),
        [],
        "scoped by repo"
    );
    assert!(matches!(
        s.find_landings("conformance-missing", &asked).await,
        Err(StoreError::RepoNotFound(_))
    ));
}

/// A change request of one repository is invisible from another.
pub async fn change_requests_by_repo<S: MetadataStore>(s: &S) {
    const OTHER: &str = "conformance-other";
    let (head, base) = (
        ChangesetId::from_bytes([1; 32]),
        ChangesetId::from_bytes([0; 32]),
    );
    assert!(matches!(
        s.submit_change_request(REPO, head, base, "sp:test").await,
        Err(StoreError::RepoNotFound(_))
    ));
    s.create_repo(REPO).await.unwrap();
    s.create_repo(OTHER).await.unwrap();
    let open = || async { s.repos_with_open_change_requests().await.unwrap() };
    assert_eq!(open().await, Vec::<String>::new());
    let ours = s
        .submit_change_request(REPO, head, base, "sp:test")
        .await
        .unwrap();
    let theirs = s
        .submit_change_request(OTHER, head, base, "sp:test")
        .await
        .unwrap();
    assert_eq!(s.get_change_request(OTHER, ours.id).await.unwrap(), None);
    assert_eq!(s.get_change_request(REPO, theirs.id).await.unwrap(), None);
    let claimed = s.claim_next_change_request(OTHER).await.unwrap().unwrap();
    assert_eq!(claimed.id, theirs.id);
    assert_eq!(s.landing_change_request(REPO).await.unwrap(), None);
    assert_eq!(s.claim_next_change_request(OTHER).await.unwrap(), None);
    assert_eq!(open().await, [REPO, OTHER], "pending and landing are open");
    assert!(matches!(
        s.finish_change_request(REPO, theirs.id, LandOutcome::Rejected).await,
        Err(StoreError::ChangeRequestNotFound(id)) if id == theirs.id
    ));
    s.finish_change_request(OTHER, theirs.id, LandOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(open().await, [REPO]);
    assert_eq!(
        s.get_change_request(REPO, ours.id)
            .await
            .unwrap()
            .map(|cr| cr.status),
        Some(ChangeRequestStatus::Pending)
    );
}

/// Concurrent claims split the pending requests between them, each exactly
/// once.
pub async fn concurrent_claims<S: MetadataStore>(s: &S) {
    s.create_repo(REPO).await.unwrap();
    let base = ChangesetId::from_bytes([0; 32]);
    let mut submitted = Vec::new();
    for b in 1..=16 {
        let head = ChangesetId::from_bytes([b; 32]);
        submitted.push(
            s.submit_change_request(REPO, head, base, "sp:test")
                .await
                .unwrap()
                .id,
        );
    }
    let claim_all = || async {
        let mut ids = Vec::new();
        while let Some(cr) = s.claim_next_change_request(REPO).await.unwrap() {
            ids.push(cr.id);
        }
        ids
    };
    let (a, b) = tokio::join!(claim_all(), claim_all());
    let mut claimed: Vec<u64> = a.into_iter().chain(b).collect();
    claimed.sort_unstable();
    assert_eq!(claimed, submitted);
}

/// Blobs and trees are content-addressed and idempotent.
pub async fn objects<S: ObjectStore>(s: &S) {
    let data = Bytes::from_static(b"hello, scion");
    let id = s.put_blob(data.clone()).await.unwrap();
    assert_eq!(id, BlobId::hash(&data));
    assert_eq!(s.put_blob(data.clone()).await.unwrap(), id);
    assert_eq!(s.get_blob(id).await.unwrap(), Some(data));

    let missing = BlobId::hash(b"missing");
    assert!(s.get_blob(missing).await.unwrap().is_none());
    assert_eq!(
        s.has_blobs(&[id, missing, id]).await.unwrap(),
        [true, false, true]
    );

    let mut entries = BTreeMap::new();
    entries.insert(
        PathComponent::new("f").unwrap(),
        TreeEntry::Blob {
            id,
            kind: FileType::Regular,
            size: 12,
        },
    );
    let tree = Tree { entries };
    let tid = s.put_tree(&tree).await.unwrap();
    assert_eq!(tid, tree.id());
    assert_eq!(s.put_tree(&tree).await.unwrap(), tid);
    assert_eq!(s.get_tree(tid).await.unwrap(), Some(tree));
    assert!(s.get_tree(Tree::empty().id()).await.unwrap().is_none());
}

/// Instantiate the conformance suite as `#[tokio::test]`s in a module named
/// `$mod`. `$ctor` is an expression evaluating to a future that yields a
/// fresh, empty store; it is evaluated once per test.
///
/// Use [`metadata_conformance_tests!`](crate::metadata_conformance_tests) or
/// [`object_conformance_tests!`](crate::object_conformance_tests) for a store
/// that implements only one of the two traits.
#[macro_export]
macro_rules! conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn repos() {
                $crate::conformance::repos(&$ctor.await).await;
            }
            #[tokio::test]
            async fn changesets() {
                $crate::conformance::changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn concurrent_changesets() {
                $crate::conformance::concurrent_changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn refs() {
                $crate::conformance::refs(&$ctor.await).await;
            }
            #[tokio::test]
            async fn derived() {
                $crate::conformance::derived(&$ctor.await).await;
            }
            #[tokio::test]
            async fn checkpoints() {
                $crate::conformance::checkpoints(&$ctor.await).await;
            }
            #[tokio::test]
            async fn grants() {
                $crate::conformance::grants(&$ctor.await).await;
            }
            #[tokio::test]
            async fn change_requests() {
                $crate::conformance::change_requests(&$ctor.await).await;
            }
            #[tokio::test]
            async fn list_change_requests() {
                $crate::conformance::list_change_requests(&$ctor.await).await;
            }
            #[tokio::test]
            async fn find_landings() {
                $crate::conformance::find_landings(&$ctor.await).await;
            }
            #[tokio::test]
            async fn change_requests_by_repo() {
                $crate::conformance::change_requests_by_repo(&$ctor.await).await;
            }
            #[tokio::test]
            async fn concurrent_claims() {
                $crate::conformance::concurrent_claims(&$ctor.await).await;
            }
            #[tokio::test]
            async fn objects() {
                $crate::conformance::objects(&$ctor.await).await;
            }
        }
    };
}

/// [`MetadataStore`] half of [`conformance_tests!`](crate::conformance_tests).
#[macro_export]
macro_rules! metadata_conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn repos() {
                $crate::conformance::repos(&$ctor.await).await;
            }
            #[tokio::test]
            async fn changesets() {
                $crate::conformance::changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn concurrent_changesets() {
                $crate::conformance::concurrent_changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn refs() {
                $crate::conformance::refs(&$ctor.await).await;
            }
            #[tokio::test]
            async fn derived() {
                $crate::conformance::derived(&$ctor.await).await;
            }
            #[tokio::test]
            async fn checkpoints() {
                $crate::conformance::checkpoints(&$ctor.await).await;
            }
            #[tokio::test]
            async fn grants() {
                $crate::conformance::grants(&$ctor.await).await;
            }
            #[tokio::test]
            async fn change_requests() {
                $crate::conformance::change_requests(&$ctor.await).await;
            }
            #[tokio::test]
            async fn list_change_requests() {
                $crate::conformance::list_change_requests(&$ctor.await).await;
            }
            #[tokio::test]
            async fn find_landings() {
                $crate::conformance::find_landings(&$ctor.await).await;
            }
            #[tokio::test]
            async fn change_requests_by_repo() {
                $crate::conformance::change_requests_by_repo(&$ctor.await).await;
            }
            #[tokio::test]
            async fn concurrent_claims() {
                $crate::conformance::concurrent_claims(&$ctor.await).await;
            }
        }
    };
}

/// [`ObjectStore`] half of [`conformance_tests!`](crate::conformance_tests).
#[macro_export]
macro_rules! object_conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn objects() {
                $crate::conformance::objects(&$ctor.await).await;
            }
        }
    };
}
