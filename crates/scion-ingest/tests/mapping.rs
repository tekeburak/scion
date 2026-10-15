//! Commit → changeset mapping, checked against `git diff-tree` on a fixture
//! repository with fixed dates, so the changeset ids can be pinned.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::{fixture, git, git_with};

use scion_ingest::{GIT_SHA, GitRepo, IngestError, ObjectId};
use scion_types::{BlobId, ChangesetId, FileChange, FileType, RepoPath, Timestamp};

const REPO: &str = "mono";

/// `git diff-tree` between `commit`'s first parent and `commit`:
/// path → (new mode, new blob sha), or `None` for a deletion.
fn diff_tree(dir: &Path, commit: &str) -> HashMap<String, Option<(String, String)>> {
    let parents = git(dir, &["rev-list", "--parents", "-n1", commit]);
    let first_parent = parents.split_whitespace().nth(1);
    let mut args = vec!["diff-tree", "-r", "--no-renames", "-z", "--no-commit-id"];
    match first_parent {
        Some(p) => args.extend([p, commit]),
        None => args.extend(["--root", commit]),
    }
    let raw = git(dir, &args);
    let mut out = HashMap::new();
    let mut fields = raw.split('\0');
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            break;
        }
        let path = fields.next().unwrap().to_owned();
        let m: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let entry = if m[4] == "D" {
            None
        } else {
            Some((m[1].to_owned(), m[3].to_owned()))
        };
        out.insert(path, entry);
    }
    out
}

/// Import every commit reachable from `main`; returns git sha → changeset.
fn import(dir: &Path) -> (GitRepo, Vec<ObjectId>, HashMap<ObjectId, ChangesetId>) {
    let repo = GitRepo::open(dir).unwrap();
    let order: Vec<ObjectId> = repo
        .commits("main")
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    let mut imported = HashMap::new();
    for id in &order {
        let mapped = repo.changeset(*id, REPO, &imported).unwrap();
        imported.insert(*id, mapped.changeset.id());
    }
    (repo, order, imported)
}

#[test]
fn file_changes_match_git_diff_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);
    let (repo, order, imported) = import(dir);
    assert_eq!(order.len(), 5);

    for id in &order {
        let mapped = repo.changeset(*id, REPO, &imported).unwrap();
        let cs = &mapped.changeset;
        let expected = diff_tree(dir, &id.to_string());
        assert_eq!(
            cs.file_changes.len(),
            expected.len(),
            "commit {id}: {cs:#?}"
        );
        for (path, want) in &expected {
            let got = &cs.file_changes[&RepoPath::new(path.as_str()).unwrap()];
            match (want, got) {
                (None, FileChange::Deleted) => {}
                (
                    Some((mode, sha)),
                    FileChange::Tracked {
                        content,
                        kind,
                        size,
                        copy_from,
                    },
                ) => {
                    let data = git_with(dir, &["cat-file", "blob", sha], "", None);
                    assert_eq!(*content, BlobId::hash(&data), "{path}");
                    assert_eq!(*size, data.len() as u64, "{path}");
                    let want_kind = match mode.as_str() {
                        "100644" => FileType::Regular,
                        "100755" => FileType::Executable,
                        "120000" => FileType::Symlink,
                        other => panic!("unexpected mode {other}"),
                    };
                    assert_eq!(*kind, want_kind, "{path}");
                    assert_eq!(*copy_from, None);
                    assert!(
                        mapped
                            .blobs
                            .contains(&(*content, ObjectId::from_hex(sha.as_bytes()).unwrap()))
                    );
                }
                (want, got) => panic!("commit {id} {path}: want {want:?}, got {got:?}"),
            }
        }

        // Parents map one to one, first parent first.
        let git_parents: Vec<ChangesetId> =
            git(dir, &["rev-list", "--parents", "-n1", &id.to_string()])
                .split_whitespace()
                .skip(1)
                .map(|p| imported[&ObjectId::from_hex(p.as_bytes()).unwrap()])
                .collect();
        assert_eq!(cs.parents, git_parents);
        assert_eq!(cs.extra[GIT_SHA], id.as_bytes().to_vec());
        assert_eq!(cs.repo, REPO);
        assert_eq!(
            (cs.author.name.as_str(), cs.author.email.as_str()),
            ("Ada", "ada@example.com")
        );
        assert_eq!(cs.author_date, Timestamp::new(1_767_214_800, 180));
        let committer = cs.committer.as_ref().unwrap();
        assert_eq!(
            (committer.name.as_str(), committer.email.as_str()),
            ("Bob", "bob@example.com")
        );
        assert_eq!(cs.committer_date, Some(Timestamp::new(1_767_323_045, 0)));
    }

    // The merge records only what changed against its first parent.
    let merge = repo
        .changeset(*order.last().unwrap(), REPO, &imported)
        .unwrap();
    assert_eq!(merge.changeset.message, "merge side\n");
    assert_eq!(
        merge
            .changeset
            .file_changes
            .keys()
            .map(RepoPath::as_str)
            .collect::<Vec<_>>(),
        ["side.txt"]
    );
}

#[test]
fn changeset_ids_are_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let (_, order, imported) = import(tmp.path());
    let tip = imported[order.last().unwrap()];
    assert_eq!(
        tip.to_hex(),
        "63c4d7b5ad1505d5997ad13698c2d5a60cd98a41dddfbc297e72603f4f94baf4",
        "the mapping changed: every imported changeset id changes with it"
    );
}

#[test]
fn rejects_submodules_and_missing_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);
    let repo = GitRepo::open(dir).unwrap();
    let order = repo.commits("main").unwrap();
    assert!(matches!(
        repo.changeset(order[1].id, REPO, &HashMap::new()),
        Err(IngestError::MissingParent { .. })
    ));

    let head = git(dir, &["rev-parse", "HEAD"]);
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendored", head.trim()),
        ],
    );
    git(dir, &["commit", "-q", "-m", "add a submodule"]);
    let repo = GitRepo::open(dir).unwrap();
    let all = repo.commits("main").unwrap();
    let mut imported = HashMap::new();
    let mut last = None;
    for c in &all {
        match repo.changeset(c.id, REPO, &imported) {
            Ok(m) => {
                imported.insert(c.id, m.changeset.id());
            }
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(IngestError::Unsupported { path, reason, .. }) => {
            assert_eq!(path, "vendored");
            assert!(reason.contains("submodule"), "{reason}");
        }
        other => panic!("expected the submodule to be rejected, got {other:?}"),
    }
}
