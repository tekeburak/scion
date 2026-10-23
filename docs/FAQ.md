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

In the metadata database, where the reference design keeps them, since Phase 10.3. They first
lived in the object store, like blobs, and the real-repository benchmark showed what that cost
(one object-store round trip per tree on import and per pointer hop on a cold `log` or
`blame`). They stay immutable and content-addressed; blobs stay in the object store.
See [Trees in the metadata database](DATA_MODEL.md#trees-in-the-metadata-database).

## Why is a changeset so minimal?

A changeset records only the files that differ from its first parent. Trees, path history,
blame and the ancestry skip list are **derived data**, computed from the changeset and its
parents. Keeping them out of the hashed object means they can be recomputed or replaced without
changing any id or any client.

## Can I use it today?

Not for real work yet. Access control is done (Phase 7): every request needs a valid token and
a role in its repository, optionally limited to a directory, people sign in with GitHub
through `scn login`, admins manage roles with `scn access`, and a hermetic simulation and
fuzz targets check the server. But nothing is released yet: every binary reports one version
(`--version`, Phase 9.1) and a `v*` tag builds static `x86_64` and `aarch64` Linux binaries
into a draft release (Phase 9.2) along with macOS `scn` (9.3) and the server image
`ghcr.io/tekeburak/scion-server` (9.5), and `scripts/install.sh` (9.4) installs the binaries
for the machine after checking them against the release's `SHA256SUMS`; but no tag has been
pushed, and there is no hosted instance (Phase 9) or cache tier (Phase 4) yet. Running your
own server is possible: `scn` reaches `https://` servers, checked against the public
certificate authorities plus any in `SSL_CERT_FILE` (9.6), and `deploy/compose.prod.yml` runs
the server image, Postgres and Caddy terminating TLS on one address with a Let's Encrypt
certificate (9.7); the README's "Run your own server" section has the commands. Every write
path (gRPC, `/api/v1` and the MCP tools) refuses an anonymous write and a blob over the
64 MiB per-blob limit with an error that names it (9.8).
The web interface and the agent API are in (Phase 11): `scion-server --web <addr>` (on
`127.0.0.1:8080` in the compose stack) serves server-rendered pages for files, log, blame,
changesets and change requests under the same roles and directory grants as `scn`; writers
rebase, land and reject change requests from the page, admins create repositories and manage
roles there, people sign in with GitHub in the browser, and `scn access grant anyone reader`
makes a repository public. `/api/v1` ([`docs/API.md`](API.md)) runs the whole init → clone →
commit → land loop over plain HTTP, `scion-mcp` ([`docs/MCP.md`](MCP.md)) serves agents over
MCP, and `scn --json` prints machine-readable output. The README's "Web interface" section
has screenshots.
The MVP (Phase 3) is done: `scion-server` serves the object and control gRPC services over
Postgres + S3. `scn` can `init` a repository, `clone --sparse`, `status`, `log`, `diff`,
`blame`, `add`, `commit` (an anonymous head; `--land` also lands it), `land` (a change
request the server rebases onto `main` and lands), `update` (the server rebases the head onto
`main`) and `checkout` any changeset. The README [Getting started](../README.md#getting-started) section runs that
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

Partly, once warm, and well ahead under concurrent landing. The figures below come from the
README's Benchmarks table (`scripts/bench-real`, rendered from `docs/bench-real.csv`); older
synthetic workloads against an in-process server are in [`BENCHMARKS.md`](BENCHMARKS.md). The
targets are set in `docs/ROADMAP.md`: import within ten times git's time (6.11); warm `log` and
`blame` and commit + `land` at or below git's time, cold `blame` within 5× git and cold path
`log` within 20× (6.12); and, in Phase 10, every row a multiple faster than git. Import is not
expected to match git, because Scion writes every blob and tree as its own object to a remote
store while git writes local packfiles. The comparison favours git where it can: git runs
`status`, `log`, `blame` and `land` in a full clone, its best case, while `scn` runs them in a
sparse clone, its normal case.

The measurements, medians of three runs on the 600-commit slice `scripts/bench-real` imports
(4 vCPU, the compose Postgres + RustFS stack, one fresh bucket per run, taken after `status`
began to list only the directories whose entries changed, on several threads, in 10.8): a cold
import, into an empty bucket, takes 11,613 ms against git's 1,379 ms (8.4×), where the object
store's write rate is the bound and the 10× criterion is met; a warm import, with the objects
already stored, takes 2,778 ms against 1,428 ms (1.9×). The README quotes the cold row. A full
clone takes 606 ms against git's 1,729 ms (0.35×) and a sparse clone 62 ms against 564 ms
(0.11×); `status` takes 7 ms against 172 ms (0.04×). `log` and `blame` are measured cold (the
first call for a new repository), warm, and once more from a second clone (server warm, client
cold). Warm `blame` of one file at the tip takes 9 ms against git's 56 ms (0.16×), and warm
path `log` 16 ms against 24 ms (0.67×); from a second clone they take 9 ms against 66 ms and
19 ms against 24 ms. Cold, the path `log` takes 99 ms against git's 25 ms (4.0×; the criterion
is 20×) and the `blame` 61 ms against 51 ms (1.2×; the criterion is 5×). A single
`scn commit --land` takes 45 ms against git's 43 ms (1.0×), just above the at-or-below-git
target: it is one round trip to land, but still a change request and the land worker. Sixteen
writers landing at once, each editing a different file, all land in 409 ms against 3,547 ms for
git's fetch-rebase-push retry loop (p50 per land 275 ms against 2,409 ms, p99 391 ms against
3,536 ms): the server rebases each request in turn, so no writer pays another's round trips.

## What are the known limits today?

Beyond the missing cache tiers and released binaries, these are the gaps a first user hits, each with
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

They are the two pieces most teams already run and know how to operate. Postgres holds the
metadata: changesets, refs (moved by compare-and-swap), grants, change requests, derived-data
pointers, and the immutable trees and history pages (since Phase 10.3); the object store holds
the bulk immutable bytes, the blobs. Neither needs repository maintenance,
and both have managed, horizontally scalable equivalents (the store is plain SQL so that
CockroachDB stays a drop-in target, though CI does not test it yet; any S3-compatible service works for objects).

## What is Scion not a good fit for?

- Most repositories. Git's model is the right one unless you have one very large monorepo with
  always-online clients and an agent-driven commit rate.
- Offline work: clients are assumed to be online (non-goal).
- Windows clients (non-goal for now).
- Large binary files: there is a 64 MiB per-blob limit and no chunking (non-goal).
- Workflows built on many long-lived branches or forks: there are no server branches.
- Reclaiming storage: objects are append-only and garbage collection is a non-goal.
- Code review beyond change requests: the web interface shows change requests with their
  status, heads and conflicts, and members rebase, land or reject them from the page; a
  separate review product is a non-goal.
