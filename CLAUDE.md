# CLAUDE.md

Working rules for this repository. Architecture and data model are **not** described here:
read `docs/ARCHITECTURE.md` and `docs/DATA_MODEL.md`. Before making any change that contradicts
either document, stop and ask.

## Commands

```sh
cargo test --workspace                                   # unit + property + golden tests
cargo test -p scion-store --features integration         # needs the compose stack below
cargo test -p scn --features integration --test e2e_stack  # end to end, same stack
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check                               # fix with: cargo fmt --all
cargo deny check bans licenses sources advisories
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
docker compose -f deploy/docker-compose.yml up -d        # Postgres :5432, S3 (RustFS) :9000
docker compose -f deploy/docker-compose.yml down -v      # reset local data
```

All of the above must pass before a commit. CI runs the same set on Linux, on our own self-hosted
runners (the header of `.github/workflows/ci.yml` says how they behave). A job that failed with
"lost communication with the server" or "received a shutdown signal" was stopped with its Spot
machine, not by the code: rerun it with `gh run rerun <run-id> --failed`, change nothing.

## Invariants (never violate without asking)

Each item is checkable in code review.

1. **No linear scans of the changeset log.** Ancestry, merge-base and "when did X land" use
   `generation`, the `SkipList` derived type, or `change_requests`. A query or loop that walks
   `changesets` ordered by time/generation without a bound is a bug.
2. **No materialized repository copy on any server.** `scion-server`, `scion-cache` and
   `scion-daemon` hold only content-addressed objects and caches. A snapshot is a manifest
   (set of tree/blob ids under `snapshots/{repo}/{cs}.manifest`), never an archive or tarball.
3. **Strict tiering.** `scion-daemon` talks only to `scion-cache`; `scion-cache` talks only to
   `scion-server`. No tier skips a level, even as a fallback.
4. **The changeset is closed.** `Changeset` gets no ref, no tree pointer, no signature field and
   no field that can be derived from other data. New needs become derived-data types.
5. **Hashing uses only `crates/scion-types/src/canonical.rs`.** Never hash a protobuf/prost
   encoding or any `serde` output. The fixtures in `crates/scion-types/tests/golden/` are
   frozen: do not edit or regenerate them. If the encoding must change, bump `FORMAT_VERSION`,
   keep the old decoder, and ask before regenerating.
6. **Server refs are only `main` and `release/*`.** `is_allowed_ref_name` is the single source
   of truth. There is no `scn branch` command and no per-user or per-feature server ref.
7. **Every metadata table has a `repo` column** and every query is scoped by it. One server
   serves many repositories.
8. **Changesets record changes relative to the first parent only.** Do not add second-parent
   diffs, merge-side blame or full-tree snapshots to the changeset.
9. **Objects are immutable and append-only.** No code path overwrites or deletes a blob, tree
   or changeset. Garbage collection is a non-goal.

## Simplicity rules

- No defensive checks between our own services: no extra validation, retry layer or "just in
  case" fallback on internal RPCs. A per-request deadline is not a defensive check; every RPC
  carries one. Validate only at the boundary: client input, bytes read from the object store,
  bytes received over the wire from a client.
- Code used in one place is a function, not a trait, generic or feature flag. Introduce an
  abstraction only when the second concrete use exists in the same PR.
- No configuration option that the roadmap does not call for. A value used once is a constant.
- No scaffolding for non-goals (GC, large-file chunking, ignore files, offline mode, Windows,
  git wire protocol, a review product beyond change-request pages).
- Before adding complexity the roadmap does not mention (new crate, new table, new RPC, new
  derived type, new dependency with a build script), ask first.
- Prefer deleting code to guarding it. A `TODO` must name the roadmap phase that resolves it.

## PR discipline

- One PR = one roadmap step (a single bullet of a single phase). Split anything larger.
- The PR description contains exactly these two lines before anything else:
  - `Invariant kept:` which numbered invariant above the change touches and how it is preserved.
  - `Could be simpler:` where the change could have been simpler and why it is not.
- No direct pushes to `main`. Every change arrives via a PR with green CI.
- Commit messages explain *why*; the diff already shows *what*.
- Reviews read intent and invariants first, lines second. A review comment that asks for a
  defensive check against our own code is answered by pointing at the Simplicity rules.
