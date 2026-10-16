# FAQ

Short answers; the normative detail is in [`ARCHITECTURE.md`](ARCHITECTURE.md) and
[`DATA_MODEL.md`](DATA_MODEL.md).

## How is Scion different from git?

Git is distributed: every clone holds the full history and can work offline. Scion is
client-server: the server is authoritative, holds history, and answers questions about it (log,
diff, merge-base, blame) over RPC. A client holds only the objects it has fetched and a sparse
working copy. A commit is created on the server directly, so there is no push, no packfile and
no repository maintenance. Hashes are BLAKE3 over Scion's own canonical encoding, so ids are not
git SHAs. A git history can be imported one way (see below); git wire compatibility is a
non-goal.

## Why are there no branches?

Server refs are only `main` and `release/*`. Everything else is an **anonymous head**: a
changeset id the client remembers, optionally under a local label that never leaves the machine.
At agent scale, thousands of short-lived per-user or per-feature refs would be shared mutable
state on the server; an id is immutable and needs no coordination.
Because objects are append-only, an anonymous head cannot be lost, and sharing work means
sharing an id (or, from Phase 5, a change request). See
[Refs and anonymous heads](DATA_MODEL.md#refs-and-anonymous-heads).

## Why do trees live in the object store?

A tree is immutable and content-addressed like a blob, so it travels the same cache path with no
special case, and the metadata database stays small. The reference design keeps trees in its
metadata database; Scion deliberately does not. The cost is one extra object fetch the first
time a changeset's root tree is resolved. See
[Deviation: trees in the object store](DATA_MODEL.md#deviation-trees-in-the-object-store).

## Why is a changeset so minimal?

A changeset records only the files that differ from its first parent. Trees, path history,
blame and the ancestry skip list are **derived data**, computed from the changeset and its
parents. Keeping them out of the hashed object means they can be recomputed or replaced without
changing any id or any client.

## Can I use it today?

Not for real work: there is no authentication (Phase 7) and no cache tier yet (Phase 4).
The MVP (Phase 3) is done: `scion-server` serves the object and control gRPC services over
Postgres + S3, and `scn` can `clone --sparse`, `status`, `log`, `diff`, `blame`, `add`,
`commit` (an anonymous head) and `land` (fast-forward `main` only; server-side rebase is
Phase 5). Git import (Phase 6) works and a benchmark script runs it against git on a real
repository; its results are not published yet.
The README status section tracks this.

## Can I bring an existing git repository?

Yes, one way. `scion-ingest import <git-dir> <repo> [<rev>]` is an operator tool that writes
straight to Postgres and the object store (`DATABASE_URL`, `AWS_*`) through the same create
path as `CreateChangeset`. Each commit reachable from `<rev>` (default `HEAD`) becomes one
changeset with its parents, its files changed against the first parent, renames and copies as
`copy_from` (so `scn blame` follows a moved file), and the git sha in `extra["git-sha"]`. Only
`main` is created or moved. An interrupted import resumes from its checkpoints, a re-run
imports only new commits, and an upstream history rewrite is refused. Submodules, paths Scion
cannot represent and blobs over the per-blob limit stop the import with the commit and path.
`scion-ingest verify <git-dir> <repo> [--sample <n>]` compares the imported trees with
`git ls-tree -r`. There is no export back to git.

## Does a commit get slower as the repository grows?

It should not, and the tests hold the derivation to that. Deriving a changeset's root tree
rewrites only the directories on the changed paths: one changed file at depth four in a
2,500-file tree rewrites exactly 5 tree nodes
(`rewritten_nodes_bounded_by_depth_not_file_count` in `crates/scion-derived/src/root_tree.rs`).
`PathHistory` rewrites the same 5 nodes and stores them as a single object, one page
(`rewritten_history_nodes_bounded_by_depth` in `crates/scion-derived/src/path_history.rs`). Ancestry and merge-base use generation numbers and the
`SkipList` derived type instead of walking the changeset log.

## Is it faster than git?

Not measured on a real repository yet. The README's benchmark table is synthetic: generated
files, an in-memory store and no cache tier. The real-repository comparison
(`scripts/bench-real`) runs, but its results are not published until Phase 6.13. The targets
are set in `docs/ROADMAP.md`: `log`, `blame` and `land` at least as fast as git (6.12), and
import within ten times git's time (6.11). Import is not expected to match git, because Scion
writes every blob and tree as its own object to a remote store while git writes local packfiles.

## Why Postgres and S3?

They are the two pieces most teams already run and know how to operate. Postgres holds small,
mutable-by-CAS metadata (changesets, refs, derived-data pointers); the object store holds the
bulk immutable bytes (blobs, trees, snapshot manifests). Neither needs repository maintenance,
and both have managed, horizontally scalable equivalents (the store is plain SQL so that
CockroachDB stays a drop-in target, though CI does not test it yet; any S3-compatible service works for objects).

## What is Scion not a good fit for?

- Most repositories. Git's model is the right one unless you have one very large monorepo with
  always-online clients and an agent-driven commit rate.
- Offline work: clients are assumed to be online (non-goal).
- Windows clients (non-goal for now).
- Large binary files: there is a per-blob size limit and no chunking (non-goal).
- Workflows built on many long-lived branches or forks: there are no server branches.
- Reclaiming storage: objects are append-only and garbage collection is a non-goal.
- Code review: there is no review UI (non-goal).
