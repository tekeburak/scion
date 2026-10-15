# Architecture

This document describes the target architecture. Phase numbers refer to the roadmap in
`README.md`; components that do not exist yet are marked.

## Principles

1. **Clients are always online.** The client never needs a complete copy of the repository; it
   fetches what it needs when it needs it.
2. **Client–server asymmetry.** The server answers questions about history and blame, creates
   changesets, and performs merges and rebases. The client lays out files, watches the working
   copy, keeps a bounded object cache, and is otherwise thin.
3. **Everything the client can ask, any job can ask.** All source-control queries are RPCs. CI
   and tooling usually need no working copy at all.
4. **Horizontally scalable, no materialized copy anywhere.** Servers and caches are stateless
   apart from the metadata database and the object store. There are no packfiles and no
   repository maintenance.
5. **Commits are cheap, incremental and lock-free.** The cost of creating a changeset and its
   derived data scales with the size of the change, not the repository. Server-side refs are
   limited to `main` and `release/*`.
6. **The working copy is optional and sparse by design.**

## Components

```
                control plane (changesets, refs, diffs, land)
  clients ───────────────────────────────────────────────────▶ scion-server ──▶ Postgres
   │                                                                │             (metadata)
   │  object plane (trees, blobs)                                   └──────────▶ object store
   ├─ developer machines ───────────────▶ scion-cache (regional) ──▶ scion-server    (blobs, trees,
   └─ CI / cluster jobs ─▶ scion-daemon ─▶ scion-cache (regional) ──▶ scion-server     snapshots)
                           (per node)
```

| Component | Crate | Phase | Role |
|---|---|---|---|
| `scn` | `crates/scn` | 3 | CLI. Talks to `scion-server` for the control plane and to the nearest object tier for trees/blobs. |
| `scion-client` | `crates/scion-client` | 3 | Library behind `scn`: `.scn/` layout, index, sparse profiles, object resolution chain. |
| `scion-daemon` | `crates/scion-daemon` | 4 | Per-node object cache over a Unix socket; seeds from a snapshot manifest; owns the working-copy watcher for fast `status`. |
| `scion-cache` | `crates/scion-cache` | 4 | Regional memory+disk cache. Only talks upstream to `scion-server` (strict tiering). |
| `scion-server` | `crates/scion-server` | 3 | Authoritative. Control-plane RPCs, object origin, derived-data computation, serial land worker. |
| `scion-store` | `crates/scion-store` | 1 | `MetadataStore` and `ObjectStore` traits; memory, Postgres, S3 implementations. |
| `scion-derived` | `crates/scion-derived` | 2 | Derived-data framework and types. |
| `scion-core` | `crates/scion-core` | 2 | diff, merge-base, log, merge, rebase — pure algorithms over the store traits. |
| `scion-types` | `crates/scion-types` | 1 | Ids, changesets, trees, canonical encoding. |
| `scion-ingest` | `crates/scion-ingest` | 6 | git → Scion importer. |
| `scion-sim` | `crates/scion-sim` | 2+ | Hermetic simulation and fuzz harnesses. |

### Two planes, one object protocol

The **control plane** is only served by `scion-server`: creating changesets, reading refs,
diffs, merge-base, blame, submitting and landing change requests.

The **object plane** serves immutable trees and blobs by id. `scion-daemon`, `scion-cache` and
`scion-server` all implement the same `ObjectService`, so a client does not need to know the
topology; configuration decides which tier it talks to. Tiering is nevertheless strictly
enforced (daemon → cache → server) to isolate failure domains: a regional cache outage must not
turn into a thundering herd on the origin.

The negotiation is deliberately simple. The client never tells the server what it already has.
Starting from a root tree id it requests missing trees recursively, then missing blobs. Every
request names a specific immutable object, so any tier can answer from cache, the exchange is
resumable, and there is no delta compression or packfile generation anywhere.

### Storage

- **Metadata** (Postgres; CockroachDB is a drop-in target): the changeset write-ahead log,
  `file_changes`, refs (one row each, moved by compare-and-swap on a version column), derived
  data pointers, change requests, ingestion checkpoints. Schema: `migrations/`.
- **Objects** (S3-compatible via the `object_store` crate; GCS/Azure/local disk also work):
  `blobs/{id}`, `trees/{id}`, `snapshots/{repo}/{changeset}.manifest`. All keys are immutable and
  zstd-compressed.

Trees live in the object store rather than the metadata database. This is a deliberate deviation
from the reference design; `docs/DATA_MODEL.md` explains the trade-off.

The local development stack (`deploy/docker-compose.yml`) uses RustFS rather than MinIO as the
S3-compatible server because MinIO no longer publishes pullable community container images
(Docker Hub and quay.io both refuse the pull as of October 2026), while RustFS is Apache-2.0 and
actively published; the code is backend-agnostic, so any S3-compatible service works.

### Derived data

A changeset records only what changed, so blame, path history and ancestry questions need
indexes. Each derived-data type implements one trait whose `derive` takes the changeset and the
type's value for each parent — the definition is recursive. Types can be computed eagerly on
commit or lazily on first request, and backfilled in the background. Because the client never
sees these structures, they can be replaced without a client change.

Types (phase 2): `RootTree`, `PathHistory`, `Blame`, `SkipList`. Later: copy detection,
basename/suffix indexes for path search.

### Landing changes

There is no CI-gated merge queue and no global repository lock. A change request is landed by a
**per-repository serial worker** inside `scion-server`: it rebases the request's head onto the
current `main` on the server, then moves `main` with a single compare-and-swap. A rebase
conflict marks the request `needs_rebase` with the conflicting paths; nothing retries blindly.
Testing happens post-merge and pre-deploy.

### Snapshots and seeding

A snapshot is a **manifest**, not an archive: the set of tree and blob ids reachable from a
changeset's root tree (optionally per sparse profile), written to the object store. A new node's
daemon fetches the manifest and pulls the listed objects from its regional cache in parallel. No
tier ever holds a materialized copy of the repository.

### Identity and access (phase 7)

Clients authenticate with **GitHub device-flow login** (`scn login`): the CLI shows a device
code, the server exchanges the resulting GitHub identity for a short-lived Scion token, and
every RPC carries that token. CI jobs and agents use **service principals** with tokens
issued by an admin. Tokens are checked once, at the server boundary; tiers below the server
forward them and never decide access themselves. Authorization is repo- and directory-scoped
(`repo` is part of every rule, invariant 7); moving `main` and `release/*` requires admin.

### Performance bar

Every phase-6+ benchmark runs the same workloads against git and against public
implementations of the same reference design, on the same machine. Scion is expected to be at
least as fast on each workload; a regression against any of them is a bug. The workloads cover
import, full and sparse checkout, status, one-file commit, land, log and blame; the table in
`README.md` shows the results and the measurement script stays in the repository.

### Ingestion dependency

`scion-ingest` (phase 6) reads git repositories with `gix` (pure Rust, `zlib-rs`, no network
features); the dependency stays in that crate so no other crate builds it.

## Non-goals (first 12 months)

git wire compatibility (import only); offline operation; a code-review UI; Windows; garbage
collection of unreachable objects; large-file chunking (there is a per-blob size limit); ignore
files (sparse profiles and explicit `add` instead); branch-like server refs.
