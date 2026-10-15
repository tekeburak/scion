# Roadmap steps

The phase table lives in `README.md`. Phases run in the order 3, 6, 4, 5, 7, 8. When a phase
starts, its single-PR steps are added here as `N.M`, each with one acceptance criterion; PRs
are titled `Phase N.M: …` and land in order. Non-goals in `docs/ARCHITECTURE.md` stay out of
every phase.

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
  workloads, on the same slice and machine, against the public implementations of the
  reference design (see "Performance bar" in `docs/ARCHITECTURE.md`), each pinned to a commit.
  *Accepted when* the CSV has a row per workload for each of them, or the script prints which
  workload one of them cannot express.
- **6.11 Import at least as fast as git.** On the 6.9 slice, `scion-ingest import` takes no
  longer than git takes to receive the same history, still creating changesets one by one
  through the shared create path. *Accepted when* the 6.9 median is at or below git's on the
  same machine, and an ingest test bounds object-store and Postgres round trips per commit.
- **6.12 `log`, `blame` and `land` at least as fast as git.** Path log, cold blame of a large
  frequently edited file, and commit + land on the 6.9 slice. *Accepted when* the 6.9 medians
  are at or below git's on the same machine and cold blame stays well inside the RPC deadline.
- **6.13 Benchmark results in the README.** The CSV from 6.9 is rendered as a table and an SVG
  chart by `scripts/render-assets.py`. *Accepted when* the README shows both, states the
  repository, range and machine, and `scripts/render-assets.py --check` passes.
- **6.14 Docs.** `docs/ARCHITECTURE.md` gets an ingestion section (mapping, what is rejected,
  resume and incremental behaviour) and the README a "Import a git repository" how-to.
  *Accepted when* following the how-to on the compose stack imports the fixture repository and
  `scn clone` of it works.
