# Roadmap

Internal planning document: the phases, their order and the single-PR steps of the current
ones. The README describes what Scion does today and what comes next in product terms and
names no phase; this file is where the numbering lives.

<p align="center">
  <img src="assets/roadmap.svg" width="800" alt="Phases 0 to 3, 6 and 5 done; then phases 7, 9, 4 and 8 planned in that order">
</p>

| Phase | Scope | Status |
|---|---|---|
| 0 | Workspace, CI, local storage stack, docs | done |
| 1 | Data model, canonical encoding, golden hashes, store traits + memory impl | done |
| 1 | Postgres metadata store, S3-compatible object store | done |
| 2 | Derived data (trees, path history, blame, skip lists), diff / merge-base / rebase / merge | done |
| 3 | `scion-server` (gRPC) + `scn` CLI | done |
| 6 | git → Scion ingestion, benchmarks on a real repository, `scn init` / `update` / `checkout`, bounded cold derivation | done |
| 5 | Server-side rebase/merge UX, change requests, serial land worker | done |
| 7 | IAM (GitHub device-flow login, service principals, repo + directory ACLs), fuzzing, hermetic simulation | planned |
| 9 | Release and onboarding: versioned binaries, server image, production compose profile, five-minute quickstart | planned |
| 4 | `scion-cache` and `scion-daemon` tiers, manifest-based seeding | planned |
| 8 | Virtualized checkouts (FUSE / NFS) | planned |

Phases run in table order: git import came right after the server and CLI because testing
and benchmarking against real repositories needs it; the land worker and IAM come before any
release, so a released server handles concurrent writers and authenticates them; the cache
and daemon tiers wait until there is more than one machine to serve. When a phase starts, its
single-PR steps are added below as `N.M`, each with one acceptance criterion; PRs are titled
`Phase N.M: …` and land in order. Non-goals in `docs/ARCHITECTURE.md` stay out of every
phase.

## Phase 6: git → Scion ingestion

`scion-ingest` turns a git repository into Scion changesets. Like `scion-server` it is an
operator tool with direct access to Postgres and the object store (`DATABASE_URL`, `AWS_*`),
and it creates changesets through the same code path as `CreateChangeset`, so eager
derivation and every server-side check apply unchanged. It reads git with `gix` only (see
`docs/ARCHITECTURE.md`); no other crate depends on it. Mapping rules: one changeset per git
commit, `file_changes` relative to the first parent only (invariant 8), the commit's sha in
`extra["git-sha"]` (invariant 4: provenance, not a field), only `main` is created or moved
(invariant 6), progress in the existing `ingest_checkpoints` table (invariant 7).

- **6.1 Crate and commit walk.** `crates/scion-ingest` opens a repository with `gix` and lists
  the commits reachable from a ref, parents before children. *Accepted when* on a fixture
  repository with branches and merges, built by the test with the `git` CLI, the order is a
  valid topological order and the commit set equals `git rev-list <ref>`.
- **6.2 Commit → changeset.** Each commit maps to a `Changeset`: author, committer, dates and
  UTC offsets, message, parents (via the sha → changeset map), modes → `FileType`, file
  changes from the tree diff against the first parent (the empty tree for a root), and
  `extra["git-sha"]`. Submodules, non-UTF-8 or invalid paths and blobs over the per-blob limit
  are rejected with an error naming the commit and path. *Accepted when* for every fixture
  commit the paths and blob contents equal `git diff-tree -r --no-renames <first-parent> <commit>`
  and the resulting changeset ids are pinned by a test.
- **6.3 Import into the stores.** `scion-ingest import <git-dir> <repo>` uploads blobs, creates
  every changeset in order through the shared create path (factored out of `scion-server`),
  then creates `main` or moves it by compare-and-swap. *Accepted when* after importing the
  fixture, `scn clone` of `main` is byte-identical (contents and modes) to `git checkout` of
  the ref, and so is a clone of each first-parent commit by id.
- **6.4 Checkpoints and resume.** Each created changeset is recorded in `ingest_checkpoints`
  in the same flow; a restarted import skips recorded commits. *Accepted when* an import
  stopped after N commits and run again ends at the same `main` id as an uninterrupted run
  and creates exactly the remaining commits.
- **6.5 Incremental sync.** Re-running against an updated repository imports only the new
  commits and fast-forwards `main`; an upstream history rewrite fails with the diverging sha
  and changes nothing. *Accepted when* importing at commit X and then at Y yields the same
  changeset ids as one import at Y, and a rewritten upstream is refused.
- **6.6 Renames and copies.** Renames and copies detected on the first-parent diff become
  `copy_from` (path, parent changeset). *Accepted when* after importing a fixture with
  `git mv` plus an edit, `scn blame` on the new path attributes the unmoved lines to the commit
  that wrote them before the rename.
- **6.7 Verify.** `scion-ingest verify <git-dir> <repo>` compares, for every imported commit
  (or a `--sample` of them), the flattened Scion tree with `git ls-tree -r` by path, mode and
  content hash. *Accepted when* it passes on the fixture and reports the exact path when a
  test corrupts one mapping.
- **6.8 Throughput and memory.** Blob uploads run concurrently, already-stored blobs are
  skipped with one `has_blobs` call per commit, and memory stays bounded by the largest commit,
  not the history. *Accepted when* importing a generated 20,000-commit history stays under a
  peak-RSS bound asserted by the test, and per-commit time does not grow with history length.
- **6.9 Real-repository benchmark script.** `scripts/bench-real` imports a pinned subset of a
  real open-source repository (a path-limited first-parent range of `rust-lang/rust`), then
  measures import, clone, sparse clone, `status`, `log`, `blame` and `land` side by side with
  git, writing a CSV. *Accepted when* it runs end to end from a clean checkout with only `git`,
  `protoc`, Docker and the pinned Rust toolchain installed, and records the repository, commit
  range and machine in its output.
- **6.10 Other implementations in the benchmark.** `scripts/bench-real` also runs the same
  workloads, on the same slice and machine, against public implementations of the reference
  design (see "Performance bar" in `docs/ARCHITECTURE.md`). The script contains no repository
  URL or project name: it reads their checkouts from `BENCH_PEERS=<dir>[:<dir>...]`, keys each
  adapter by a neutral label, starts each peer's own stack from its checkout, and records
  "not measured" when the variable is unset. *Accepted when* the CSV has a row per workload for
  each supplied peer (or the workload it cannot express), and a run without `BENCH_PEERS` still
  completes against git alone.
- **6.11 Import within 10× of git.** Parity with git on import is not a goal: git writes local
  packfiles, Scion writes one immutable object per blob and tree to a remote object store. On
  the 6.9 slice, `scion-ingest import` takes at most ten times what git takes to receive the
  same history, with bounded memory, by creating changesets in batches through the shared
  create path and uploading objects concurrently. *Accepted when* the 6.9 median is at or below
  10× git's on the same machine, the 6.8 peak-RSS bound still holds, and an ingest test bounds
  object-store and Postgres round trips per commit.
- **6.12 `log`, `blame` and `land` fast on the 6.9 slice.** A cold request pays one remote
  round trip per pointer hop, which git's local packfiles never do, so cold and warm are
  measured separately. Blame: a derived type that does not read its parents' values skips the
  ancestor walk, and `Blame` is derived per file along that file's own version chain, reading
  the versions concurrently; no new derived type. Path log: every rewritten `PathHistory` node
  carries the id of the node it replaced, so a hop is one object read and no Postgres read.
  *Accepted when*, on the same machine, warm `log` and `blame` and commit + `land` medians are
  at or below git's; cold `blame` of the slice's most edited file is within 5× git and far
  inside the RPC deadline; cold path `log` is within 20× git; and a counting-store test shows
  cold blame of a path changed k times in N commits reads O(k) changesets, not O(N).
- **6.13 Benchmark results in the README.** The CSV from 6.9 is rendered as a table and an SVG
  chart by `scripts/render-assets.py`. *Accepted when* the README shows both, states the
  repository, range and machine, and `scripts/render-assets.py --check` passes.
- **6.14 Docs.** `docs/ARCHITECTURE.md` gets an ingestion section (mapping, what is rejected,
  resume and incremental behaviour) and the README a "Import a git repository" how-to.
  *Accepted when* following the how-to on the compose stack imports the fixture repository and
  `scn clone` of it works.

Steps 6.15–6.18 come from a review of `main` at e0d9929 (issue #67). They make the MVP usable
by one team on one machine before any release work.

- **6.15 Bounded derivation and `backfill`.** `Deriver` drops a changeset and its memo entry
  once every child that needs it is derived, so memory follows the frontier, not the history.
  `scion-server backfill <repo> <type>` derives a type for every changeset reachable from the
  repository's refs, in generation order, resumable; any PR that bumps a derived `VERSION` runs
  it in its e2e test. *Accepted when* a counting-store test shows peak loaded changesets during
  a cold derivation of the 6.8 fixture stay below a constant, and the first `log <path>` after a
  `VERSION` bump completes inside the RPC deadline on the 6.9 slice.
- **6.16 Explicit repository creation.** `scn init <server> <repo>` creates the repository, its
  root changeset and `main` through `CreateChangeset` and `MoveRef`; nothing else creates a
  repository, so `scion-ingest import` and every store write fail with NOT_FOUND on an unknown
  name, and `scn clone` of a just-created repository yields an empty working copy. *Accepted
  when* the e2e test runs init → clone → commit → land with no git repository involved, and a
  misspelled repository name is rejected by every write path.
- **6.17 `scn update` and `scn checkout`.** `scn checkout <rev>` moves the working copy to a
  changeset (refusing with the list of dirty paths unless `--force`). `scn update [rev]`
  (default: `main`) rebases the current head onto the target through `scion_core::rebase`,
  creates the rebased changeset on the server, and applies the resulting tree to the working
  copy; a conflict leaves the working copy untouched and prints the conflicting paths. After
  `update`, `land` is a fast-forward again. *Accepted when* the e2e test has two clones land
  alternately with `update` between, main stays linear, and a proptest over random
  non-overlapping edit pairs shows `update` yields the same tree as `scion_core::merge`.
- **6.18 Idempotent writes and an honest local stack.** `put_changesets` uses `ON CONFLICT DO
  NOTHING` on both inserts and a conformance test stores one batch from two tasks at once;
  `deploy/docker-compose.yml` gains a loopback-only `scion-server` service with its `AWS_*` and
  `DATABASE_URL` wiring, and the README quickstart runs compose → `scn init` → `clone` →
  `commit` → `land` with no git repository, noting that the server restarts after `down -v`
  because its presence memo outlives the bucket. `scripts/bench-real` uses a fresh bucket per
  run and reports import cold and warm as two rows, and the README states which it quotes.
  *Accepted when* the quickstart works verbatim on a clean machine with Docker and the
  toolchain, and the import row in the README is the cold one.

## Phase 5: change requests and the land worker

A change request asks the server to land a head on `main`. One serial
worker per repository inside `scion-server` takes pending requests in submission order,
rebases each onto the current `main` through `scion_core::rebase` (the same path as the
`Rebase` RPC), and moves `main` with one compare-and-swap; there is no CI gate, no global
lock and no blind retry (see "Landing changes" in `docs/ARCHITECTURE.md`). A conflict marks
the request `needs_rebase` with its paths. Requests live in the existing `change_requests`
table (invariant 7: every query is scoped by `repo`), which also answers "when did X land"
(invariant 1). Merges and reverts are server-side, record file changes against the first
parent only (invariant 8), and add no field to `Changeset` (invariant 4). Throughput is the
goal: many agents landing at once must not queue behind each other's client round trips.

- **5.1 Change-request store API.** `MetadataStore` gains `submit_change_request`,
  `get_change_request`, `claim_next_change_request` (oldest `pending` → `landing`, through the
  `change_requests_pending` index) and `finish_change_request` (`landed` with `landed_cs`, or
  `needs_rebase` with conflicts, or `rejected` when the rebase would land nothing), on the
  memory and Postgres
  stores. *Accepted when* the conformance suite covers every transition on both stores, an
  illegal transition is refused, and a request of one repository is invisible from another.
- **5.2 `SubmitChangeRequest` and `GetChangeRequest` RPCs.** Submit validates at the boundary
  (the head exists in the repository) and records `main`'s current changeset as `base_cs`;
  Get returns status, `landed_cs` and conflicts. *Accepted when* a server test submits and
  reads back a request, and an unknown head or repository is NOT_FOUND.
- **5.3 Serial land worker.** One task per repository with pending requests, woken by a submit
  and started for every repository with pending or `landing` requests when the server starts.
  It claims the oldest request, rebases its head onto `main`'s current changeset, creates the
  rebased changesets through the shared create path, and moves `main` by CAS. A lost CAS
  happens only when another writer (`MoveRef`, an import) moved `main`, so it rebases onto the
  new `main`, bounded by how far `main` moved. *Accepted when* 64 non-conflicting requests submitted concurrently
  all land, `main` stays linear, and each request's `landed_cs` contains its change.
- **5.4 Conflicts and crash recovery.** A rebase conflict finishes the request `needs_rebase`
  with the conflicting paths and leaves `main` untouched; a request found `landing` at start
  resumes, and landing is idempotent because rebased changesets are content-addressed (a
  repeated rebase recreates the same ids). *Accepted when* a test with two requests editing
  the same lines lands the first and marks the second with that path, and a test that stops
  the worker between create and CAS lands that request exactly once after a restart.
- **5.5 `scn land` through change requests.** `scn land` submits the current head and waits
  for the outcome, printing the landed changeset or the conflicting paths and the `scn update`
  that resolves them; `--no-wait` prints the request id and `scn land --status <id>` reads it.
  A fast-forward still lands in one round trip through the same worker. *Accepted when* the
  e2e test has two clones land non-overlapping edits at the same time with no `update` between
  and both land.
- **5.6 Listing requests.** `ListChangeRequests(repo, status, limit)` and `scn requests
  [--status <s>] [-n <limit>]` list the newest requests first, through an index on
  `(repo, submitted_at)`, never a scan. *Accepted when* a store test with 1,000 requests
  returns the newest `limit` of a status in submission order, and `scn requests` shows id,
  status, head, landed changeset and age.
- **5.7 "When did X land".** `FindLanding(repo, changeset)` returns the request that landed a
  changeset (as its head or as its rebased `landed_cs`), with the time, through an
  index on `(repo, landed_cs)` and `(repo, head_cs)` added in a new migration; `scn log
  --landed` shows the request id and time on each line. *Accepted when* the lookup is one
  indexed query (a test with 1,000 landed requests checks the plan) and a rebased change is
  found from both its original and its landed id.
- **5.8 Server-side merge.** A `Merge` RPC creates a two-parent changeset from `ours` and
  `theirs` through `scion_core::merge`, with file changes against `ours` only (invariant 8),
  or returns the conflicting paths without creating anything; `scn merge <rev>` merges a
  revision (e.g. a `release/*` fix) into the working head. *Accepted when* the e2e test merges
  a `release/1` change into a head that then lands on `main`, and `blame` of the merged file
  attributes the release lines to their release changeset.
- **5.9 Conflict markers on `update`.** `scn update --conflicts` writes three-way conflict
  markers (from `scion-diff3`) into each conflicting file of the working copy instead of
  refusing, records the rebase target, and the next `scn commit` creates the resolved
  changeset on top of it. *Accepted when* a test with a same-line edit pair resolves through
  markers and `land`s, and plain `scn update` keeps refusing without touching the working copy.
- **5.10 Server-side revert.** A `Revert` RPC creates a changeset on a head that undoes one
  changeset's file changes (refused with the paths changed since), and `scn revert <rev>`
  makes it the working copy's head. *Accepted when* reverting a landed changeset and landing
  the revert restores the previous tree id exactly.
- **5.11 Concurrent-land benchmark.** `scripts/bench-real` gains `land, 16 writers`: 16 clones
  each commit and land one non-overlapping edit at once, against 16 git clones running the
  usual fetch-rebase-push retry loop, recording total time, p50 and p99 per land. *Accepted
  when* the README shows the row and Scion's total for 16 writers is below git's.
- **5.12 Docs.** `docs/ARCHITECTURE.md` "Landing changes" loses its *planned* mark and
  describes the worker, request states, recovery and `FindLanding`; the README gets a "Land
  with change requests" how-to and the status and roadmap figures show Phase 5 done. *Accepted
  when* following the how-to on the compose stack lands two concurrent changes and
  `scripts/render-assets.py --check` passes.

## Phase 9: release and onboarding

Steps are added when the phase starts (after Phase 7, so a released server has
authentication). Scope: versioned `scn`, `scion-server` and `scion-ingest` binaries for Linux
and macOS built by CI on a tag; a `scion-server` container image; a production compose profile
(Postgres, S3 credentials, TLS termination in front); a five-minute quickstart that installs
`scn`, logs in, clones and lands; and release notes generated from merged `Phase N.M:` PRs.
