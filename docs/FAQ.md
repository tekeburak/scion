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
sharing an id or submitting a change request for it (`scn land`). See
[Refs and anonymous heads](DATA_MODEL.md#refs-and-anonymous-heads).

## Where do trees live?

Today in the object store, like blobs. The real-repository benchmark showed what that costs
(one object-store round trip per tree on import and per pointer hop on a cold `log` or
`blame`), so trees and history pages are moving to the metadata database, where the reference
design keeps them. They stay immutable and content-addressed; blobs stay in the object store.
See [Deviation: trees in the object store](DATA_MODEL.md#deviation-trees-in-the-object-store).

## Why is a changeset so minimal?

A changeset records only the files that differ from its first parent. Trees, path history,
blame and the ancestry skip list are **derived data**, computed from the changeset and its
parents. Keeping them out of the hashed object means they can be recomputed or replaced without
changing any id or any client.

## Can I use it today?

Not for real work: Phase 7 is still in progress (access control works: every request needs a
valid token and a role in its repository, optionally limited to a directory, people sign in
with GitHub through `scn login` and admins manage roles with `scn access`; fuzzing and the
hermetic simulation are still to come), there are no released binaries yet (Phase 9) and there
is no cache tier yet (Phase 4).
The MVP (Phase 3) is done: `scion-server` serves the object and control gRPC services over
Postgres + S3. `scn` can `init` a repository, `clone --sparse`, `status`, `log`, `diff`,
`blame`, `add`, `commit` (an anonymous head), `land` (a change request the server rebases
onto `main` and lands), `update` (the server rebases the head onto `main`) and `checkout`
any changeset. The README [Getting started](../README.md#getting-started) section runs that
loop on the compose stack with only Docker and the Rust toolchain installed. Change requests
are done (Phase 5): `scn land` submits one (`SubmitChangeRequest`) and waits while the
serial land worker of the repository rebases and lands it, so concurrent heads land from the
CLI with no manual `update`, `scn requests` lists them newest first, and `scn log --landed`
shows which request landed each changeset and when. Merges and reverts run on the server:
`scn merge <rev>` merges a revision such as a `release/*` fix into your head, `scn revert
<rev>` undoes a changeset on it, and `scn update --conflicts` writes conflict markers to
resolve locally instead of refusing. A benchmark of 16 writers landing at once is in the
README (numbers below).
Git import (Phase 6) works and a benchmark script runs it against git on a real
repository; the README's Benchmarks section shows the results. A cold `blame` or path `log` (the first call
for a revision) takes several times git's time; the 6.12 criteria bound how many.
The README status section tracks this.

## Can I bring an existing git repository?

Yes, one way. Create the repository with `scn init <server> <repo>`, then run
`scion-ingest import <git-dir> <repo> [<rev>]`, an operator tool that writes
straight to Postgres and the object store (`DATABASE_URL`, `AWS_*`) through the same create
path as `CreateChangeset`. Each commit reachable from `<rev>` (default `HEAD`) becomes one
changeset with its parents, its files changed against the first parent, renames and copies as
`copy_from` (so `scn blame` follows a moved file), and the git sha in `extra["git-sha"]`. Only
`main` is moved. An interrupted import resumes from its checkpoints, a re-run
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

Partly, once warm. The README's Benchmarks section has two parts. The real-repository
comparison (`scripts/bench-real`, rendered from `docs/bench-real.csv`) is where the figures
below come from. The synthetic table uses generated files, an in-memory store and no cache
tier. The targets are
set in `docs/ROADMAP.md`: import within ten times git's time (6.11); warm `log` and `blame` and
commit + `land` at or below git's time, cold `blame` within 5× git and cold path `log` within
20× (6.12). Import is not expected to match git, because Scion
writes every blob and tree as its own object to a remote store while git writes local packfiles.
The comparison favours git where it can: git runs `status`, `log`, `blame` and `land` in a full
clone, its best case, while `scn` runs them in a sparse clone, its normal case.

The measurements so far, on the 600-commit slice `scripts/bench-real` imports (4 vCPU, the
compose Postgres + RustFS stack, one fresh bucket per run): a cold import, into an empty
bucket, takes 39,879 ms against git's 1,870 ms (21×), where the object store's write rate is
the bound and the 10× criterion is not met yet; a warm import, with the objects already
stored, takes 12,420 ms against 1,782 ms (7.0×). The README quotes the cold row.
`log` and `blame` are measured twice, cold (the first call for a new repository) and warm.
Medians of three runs (the README table): warm `blame` of a 4,738-line file with 44 versions
takes 19 ms against git's 101 ms, and warm path `log` 45 ms against 51 ms. Cold, a path `log`
of 428 changes takes 419 ms against git's 29 ms (14×; the criterion is 20×), because its
anchors let the object reads overlap, and that `blame` 366 ms against 88 ms (4.2×; the
criterion is 5×), because its metadata reads are batched and its diffs run in parallel. A cold
blame is still a few hundred object reads, one history node and one file version per version
of every file the last change touched. A single commit + `land` takes 94 ms against git's
68 ms on this run, above the at-or-below-git target. Sixteen writers landing at once, each
editing a different file, all land in 1,034 ms against 5,086 ms for git's fetch-rebase-push
retry loop (p50 per land 700 ms against 3,427 ms, p99 981 ms against 5,060 ms): the server
rebases each request in turn, so no writer pays another's round trips.

## What are the known limits today?

Beyond missing authentication and cache tiers, these are the gaps a first user hits, each with
the roadmap step that closes it:

- **Incremental import is for mirror repositories.** A re-run of `scion-ingest import` refuses
  with "the upstream history was rewritten" whenever `main` points at a changeset that did not
  come from that git history, including one made with `scn land`.
- **Restart a `scion-server` run outside compose after `docker compose ... down -v`.** The
  server remembers which blobs the bucket already holds; after the bucket is wiped it skips
  uploading them, and later reads fail. The compose service is recreated by `down -v` and has
  no such problem.
- **The first request after a derived type's `VERSION` bump derives that type for the whole
  history.** Memory stays bounded, but on the 600-commit benchmark slice the first path `log`
  after a `path_history` bump takes about 14 s of the 30 s RPC deadline; run
  `scion-server backfill <repo> path_history` (about as long, once) after deploying the bump
  and the first `log` is warm.

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
- Code review beyond change requests: the planned web interface shows and lands change
  requests; a separate review product is a non-goal.
