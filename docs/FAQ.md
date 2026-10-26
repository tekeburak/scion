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

## How does Scion relate to Anthropic's post?

Scion is an independent project inspired by the internal system Anthropic describes in
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control);
it is not an Anthropic project and is not affiliated with Anthropic. These documents call that
system "the reference design". Scion follows its shape: an authoritative server over a metadata
database and an object store, changesets that record only what changed against the first
parent, trees and history as derived data, sparse thin clients, a cache hierarchy in front of
the server (planned) and a single `main`. It departs in two places: ids are BLAKE3 over Scion's
own canonical encoding, never a protobuf encoding, and the server accepts no ref other than
`main` and `release/*`. Its metadata database is Postgres where the reference design uses
Spanner. The post credits Meta's Mononoke with pioneering many of the techniques it describes;
Scion's changeset follows Mononoke's bonsai changeset model.

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

To try it, yes; for real work, not yet. Scion is alpha: no release has been tagged, there is no
hosted instance and the cache tiers are not built. What runs today, on the local compose stack
or on your own server:

- **The server and the CLI.** `scion-server` serves the object and control gRPC services over
  Postgres + S3. `scn` can `init` a repository, `clone --sparse`, `status`, `log`, `diff`,
  `blame`, `add`, `commit` (an anonymous head; `--land` also lands it, in one round trip) and
  `checkout` any changeset. The README's [Getting started](../README.md#getting-started) runs
  that loop with only Docker and the Rust toolchain installed.
- **Landing.** `scn land` submits the head as a change request; the repository's serial land
  worker rebases it onto `main` on the server and lands it, so concurrent heads land with no
  manual `update`. `scn requests` lists the requests newest first and `scn log --landed` shows
  which request landed each changeset. `scn merge <rev>` and `scn revert <rev>` run on the
  server, and `scn update --conflicts` writes conflict markers to resolve locally instead of
  refusing.
- **Access control.** Every request needs a token signed by the server and a role in its
  repository, optionally limited to a directory. People sign in with GitHub (`scn login`),
  services use tokens from `scion-server token`, and admins manage roles with `scn access`.
  Every write path (gRPC, `/api/v1` and the MCP tools) refuses an anonymous write and a blob
  over the 64 MiB per-blob limit with an error that names it.
- **The web interface and the agent API.** `scion-server --web <addr>` (on `127.0.0.1:8080`
  in the compose stack) serves pages for files, log, blame, changesets and change requests
  under the same roles as `scn`; writers rebase, land and reject change requests there and
  admins create repositories and manage roles. `/api/v1` ([`docs/API.md`](API.md)) runs the
  whole init → clone → commit → land loop over plain HTTP, `scion-mcp`
  ([`docs/MCP.md`](MCP.md)) serves agents over MCP, and `scn --json` prints machine-readable
  output.
- **Git import.** `scion-ingest` imports a git history (next question), and
  `scripts/bench-real` measures Scion against git on a real repository (see
  [Is it faster than git?](#is-it-faster-than-git)).
- **Running your own server.** `deploy/compose.prod.yml` runs the server image, Postgres and
  Caddy terminating TLS on one `https://` address with a Let's Encrypt certificate; `scn`
  checks `https://` servers against the public certificate authorities plus any in
  `SSL_CERT_FILE`. The README's "Run your own server" section has the commands.
- **Release tooling, not yet used.** A `v*` tag builds static `x86_64` and `aarch64` Linux
  binaries and macOS `scn` into a draft release, and the server image
  `ghcr.io/tekeburak/scion-server`; `scripts/install.sh` installs the binaries after checking
  them against the release's `SHA256SUMS`. Until a tag is pushed, build from source.

A seeded, hermetic simulation and fuzz targets check the server; the fuzz corpus is replayed on
every test run. What is missing is
listed under [What are the known limits today?](#what-are-the-known-limits-today) and, step by
step, in [`docs/ROADMAP.md`](ROADMAP.md) (release and hosting in Phase 9, the cache tiers in
Phase 4).

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
(4 vCPU, the compose Postgres + RustFS stack, one fresh bucket per run, the run behind the
README table, taken on 2026-10-24 after 10.9 pipelined the land worker): a cold import, into
an empty bucket, takes 11,629 ms against git's 1,386 ms (8.4×), where the object store's write
rate is the bound, within the 10× criterion; a warm import, with the objects already stored,
takes 2,580 ms against 1,365 ms (1.9×). The README quotes the cold row. A full clone takes
582 ms against git's 1,617 ms (0.36×) and a sparse clone 28 ms against 513 ms (0.05×); `status`
takes 7 ms against 161 ms (0.04×). `log` and `blame` are measured cold (the first call for a
new repository), warm, and once more from a second clone (server warm, client cold). Warm
`blame` of one file at the tip takes 8 ms against git's 57 ms (0.14×), and warm path `log`
16 ms against 22 ms (0.73×); from a second clone they take 9 ms against 61 ms and 18 ms
against 23 ms. Cold, the path `log` takes 79 ms against git's 22 ms (3.6×; the criterion is
20×) and the `blame` 56 ms against 49 ms (1.1×; the criterion is 5×). A single
`scn commit --land` takes 52 ms against git's 43 ms (1.2×), above the at-or-below-git target:
it is one round trip to land, but still a change request and the land worker. Sixteen writers
landing at once, each editing a different file, all land in 295 ms against 3,022 ms for git's
fetch-rebase-push retry loop (0.10×; p50 per land 207 ms against 2,045 ms, p99 285 ms against
3,014 ms): the server rebases each request in turn, preparing the next while the previous one
lands, so no writer pays another's round trips.

## What are the known limits today?

Beyond the missing cache tiers and released binaries, these are the gaps a first user hits:

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
