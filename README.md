<p align="center">
  <a href="https://github.com/tekeburak/scion"><img src="docs/assets/logo.svg" width="112" alt="Scion logo"></a>
</p>

<h1 align="center">Scion</h1>

<p align="center"><b>Server-authoritative source control for very large monorepos whose clients are always
online and whose commits come mostly from coding agents.</b></p>

<p align="center">
  <a href="https://github.com/tekeburak/scion/actions/workflows/ci.yml"><img src="https://github.com/tekeburak/scion/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="LICENSE"><img src="docs/assets/badge-license.svg" alt="license: Apache-2.0"></a>
  <a href="#status"><img src="docs/assets/badge-status.svg" alt="status: alpha"></a>
</p>

> Scion is an independent open-source project inspired by the internal version control system
> Anthropic describes in [*Redesigning source control at Anthropic*](https://www.anthropic.com/engineering/redesigning-source-control).
> It is not an official Anthropic project and is not affiliated with or endorsed by Anthropic.

<p align="center">
  <img src="docs/assets/banner.svg" width="800" alt="Scion: changesets, derived trees and content-addressed blobs behind a single main branch">
</p>

## Why Scion

Git's distributed, offline-first model is the right design for the overwhelming majority of
repositories. It stops being the right design in one specific situation: a single very large
monorepo whose clients (CI jobs, agent sandboxes, research jobs, developer environments) are
effectively always online, and whose commit rate is driven by coding agents rather than people.
There, every client carrying its own copy of the history becomes the bottleneck, and a
centralized design with thin clients and tiered object caches is a better fit.

Scion is that design, built on boring infrastructure:

- **Postgres and S3, nothing else.** Metadata (changesets, refs, derived-data pointers) lives in
  Postgres; blobs and trees live in any S3-compatible object store. No packfiles, no repository
  maintenance, no materialized copy of the repository on any server.
- **A commit costs what it changes.** A changeset records only the files it changed against its
  first parent. Directory trees, path history, blame and the ancestry index are *derived data*,
  computed from that and replaceable without a client change. One changed file in a 2,500-file
  tree rewrites five tree nodes: the directories on its path.
- **Thin, sparse clients.** A working copy holds the directories you asked for and a bounded
  object cache; everything else is fetched by immutable id, which makes every request a
  cacheable GET through the daemon → regional cache → server hierarchy.
- **One `main`.** The only server-side refs are `main` and `release/*`. Work in progress is an
  anonymous head the client tracks; landing is a server-side rebase onto `main`, so there is
  nothing to push and no merge queue to wait in.

<p align="center">
  <img src="docs/assets/architecture.svg" width="800" alt="Architecture: clients send control-plane RPCs to scion-server; objects flow daemon to cache to server; the server stores metadata in Postgres and objects in an S3-compatible store">
</p>

## Status

Scion is **alpha** software. The server, the CLI, git import and the real-repository benchmark
work and are exercised end to end in CI on Postgres + S3. Every request needs a token
signed by the server and a role in the repository it touches, optionally limited to a
directory; people sign in with GitHub (`scn login`) and services use tokens from the operator. Landing goes through change requests and a serial
land worker, and every request can be listed and traced to the changeset it landed;
merges and reverts run on the server. A web interface, released binaries and the cache and
daemon tiers follow (see [What's next](#whats-next)).

What works today:

- **`scion-server`**: the control-plane and object gRPC services over Postgres + S3 — `log`,
  `diff`, `path_log`, `merge_base`, `blame`, `create_changeset`, `move_ref`, `rebase`, `merge`,
  `revert`, change requests — plus `backfill`, which derives a type for a repository's whole history
  ahead of the first request.
- **Change requests and the land worker**: a submitted head is landed by one serial worker
  per repository, in submission order, rebased onto the current `main` on the server and
  moved in with a single compare-and-swap. A conflict leaves `main` untouched and reports the
  conflicting paths; a request interrupted by a server restart resumes and lands exactly once.
- **`scn`**, the CLI: `init` a repository, `clone --sparse`, `status`, `log`, `diff`, `blame`,
  `add`, `commit` (an anonymous head), `land` (submits the head as a change request; the
  server rebases it onto `main` and lands it, or names the conflicting paths), `update` (the
  server rebases your head onto `main`; `--conflicts` writes conflict markers to resolve and
  commit instead of refusing), `requests` (the repository's change requests, newest
  first, by status), `log --landed` (which request landed each changeset, and when), `merge`
  (the server merges a revision such as a `release/*` fix into your head; blame credits the
  fix to the changeset that wrote it), `revert` (the server undoes a changeset on your head,
  refusing if its files changed since; blame keeps crediting the restored lines to their
  authors) and `checkout` of any changeset. Two clones landing
  at the same time, with no `update` in between, is the CI end-to-end test.
- **`scion-ingest`**: imports a git history into Scion (one changeset per commit, renames and
  copies kept for `blame`, the git sha kept in the changeset), resumes from checkpoints, syncs
  incrementally, refuses a rewritten upstream, and `verify` compares every imported tree with
  `git ls-tree`.
- **Access control**: every request carries a token signed by the server, and each repository
  has readers, writers and admins (`scion-server grant`). Reads need `reader`; committing,
  landing, merging and reverting need `writer`; moving `main` or a `release/*` ref directly
  needs `admin`; `scn init` needs a server-wide admin and makes its caller the new repository's
  admin. A grant can be limited to a directory (`--path lib`): its writer can change, land,
  merge and revert only files under it, and its reader reads only those files' history,
  blame and diffs; a refusal names the path. People sign in with GitHub (`scn login
  <server>`, device flow, the token kept per server with mode 0600); CI and agents use
  service-principal tokens from `scion-server token`. A repository's admins grant, revoke and
  list its roles from a working copy (`scn access grant|revoke|list`), and `scn whoami` shows
  who a token names, until when, and its roles in the repository.
- **Benchmarks** against git on a pinned slice of a real repository, from clone and `blame`
  to 16 writers landing at once, with the numbers [below](#benchmarks).

## Getting started

On a machine with Docker (with compose) and the Rust toolchain, from a checkout of this
repository:

```sh
# Postgres, S3 (RustFS) and scion-server on 127.0.0.1:7420; the first run builds the server
docker compose -f deploy/docker-compose.yml up -d --wait
cargo install --locked --path crates/scn   # scion-proto's build needs protoc on PATH
# every request carries a token; the local server signs one for you, and lets
# its principal create repositories
export SCN_TOKEN=$(docker compose -f deploy/docker-compose.yml exec -T scion-server \
    scion-server token sp:dev --ttl 30d)
docker compose -f deploy/docker-compose.yml exec -T scion-server \
    scion-server grant '*' sp:dev admin

scn init http://127.0.0.1:7420 demo        # an empty repository with main at its root
scn clone http://127.0.0.1:7420 demo
cd demo
echo hello >hello.txt
scn add hello.txt
scn commit -m "add hello" --author "Ada <ada@example.com>"   # or set SCN_AUTHOR
scn land                                   # the server lands it on main
scn log
```

After a code change, rebuild
the server with `docker compose -f deploy/docker-compose.yml up -d --wait --build`.
`docker compose -f deploy/docker-compose.yml down -v` deletes every repository; the next `up`
starts a new server with it. To sign in with GitHub instead of `SCN_TOKEN`, create a GitHub
OAuth app with device flow enabled, start the stack with `SCION_GITHUB_CLIENT_ID` set to its
client id and run `scn login http://127.0.0.1:7420`; the token lands in
`~/.config/scn/tokens/` (mode 0600) and is valid for 30 days. A token stays valid until its `--ttl` runs out, also across `down -v`. A `scion-server` run
outside compose needs `SCION_TOKEN_KEY` (`openssl rand -hex 32`) and must be restarted after
`down -v` too: it keeps an in-memory memo of the objects the bucket holds, and that memo
outlives the bucket.

### Land with change requests

`scn land` submits your head as a change request. One land worker per repository takes
requests in the order they arrive, rebases each onto the current `main` on the server and
moves `main` with a single compare-and-swap, so writers never wait on each other's round
trips and nobody has to `update` before landing. Continuing in the `demo` repository from
above, two working copies land at the same moment:

```sh
cd ..                                      # out of the demo working copy
export SCN_AUTHOR="Ada <ada@example.com>"
scn clone http://127.0.0.1:7420 demo alice
scn clone http://127.0.0.1:7420 demo bob
echo "from alice" >alice/alice.txt
echo "from bob" >bob/bob.txt
(cd alice && scn add alice.txt && scn commit -m "add alice.txt")
(cd bob && scn add bob.txt && scn commit -m "add bob.txt")

(cd alice && scn land) & (cd bob && scn land) & wait   # both land; no update in between
(cd bob && scn requests)                   # newest first: id, status, head, landed, age
(cd alice && scn update && scn log --landed -n 2)   # each with the request that landed it
```

The request that arrives second is rebased onto the first and lands as a new changeset;
`scn land` prints both ids (`landed <head> on main as <new id>`) and moves that working copy
to the landed changeset. Only a change to the same lines as one already on `main` is refused:
`scn land` names the conflicting paths, `main` is untouched, and `scn update --conflicts`
writes conflict markers into those files to resolve and `commit` before landing again.
`scn land --no-wait` prints the request id at once and `scn land --status <id>` reads its
outcome later; `scn requests --status needs-rebase` lists only the conflicted requests, and
`scn log --landed` finds the request behind a changeset even when the server rebased it to a
new id.

### Import a git repository

`scion-ingest` copies a git history into Scion: one changeset per commit, file changes
against the first parent, renames and copies kept for `blame`, the git sha kept in the
changeset's `extra`, and `main` pointed at the imported tip. It imports into a repository
created with `scn init` and never creates one. The rules, and what it refuses, are in
[Ingestion](docs/ARCHITECTURE.md#ingestion). On the local compose stack, from a checkout of
this repository:

```sh
# Postgres on :5432, S3 (RustFS) on :9000 and scion-server on :7420, then the clients
docker compose -f deploy/docker-compose.yml up -d --wait
cargo build --release -p scion-ingest -p scn
export PATH=$PWD/target/release:$PATH
export SCN_TOKEN=$(docker compose -f deploy/docker-compose.yml exec -T scion-server \
    scion-server token sp:dev --ttl 30d)
docker compose -f deploy/docker-compose.yml exec -T scion-server \
    scion-server grant '*' sp:dev admin

# scion-ingest works on the stores directly
export DATABASE_URL=postgres://scion:scion@127.0.0.1:5432/scion
export AWS_ENDPOINT=http://127.0.0.1:9000 AWS_ALLOW_HTTP=true AWS_BUCKET=scion-objects
export AWS_ACCESS_KEY_ID=scion AWS_SECRET_ACCESS_KEY=scionscion AWS_REGION=us-east-1

# a two-commit git repository with a rename; any local clone works the same way
cd "$(mktemp -d)"
export GIT_AUTHOR_NAME=Ada GIT_AUTHOR_EMAIL=ada@example.com
export GIT_COMMITTER_NAME=Ada GIT_COMMITTER_EMAIL=ada@example.com
git init -q -b main demo
printf 'one\ntwo\nthree\n' >demo/notes.txt
git -C demo add -A && git -C demo commit -qm "add notes"
git -C demo mv notes.txt README.txt && printf 'four\n' >>demo/README.txt
git -C demo commit -qam "rename notes, add a line"

scn init http://127.0.0.1:7420 demo  # an empty repository; import never creates one

scion-ingest import demo demo main   # git dir, Scion repository, git revision
scion-ingest verify demo demo        # every imported tree against git ls-tree

scn clone http://127.0.0.1:7420 demo demo-scn
cd demo-scn && scn log && scn blame README.txt   # "one" to "three" blame to "add notes"
```

Run `import` again after new git commits and it imports only those and fast-forwards `main`;
an interrupted import resumes the same way. If the git history was rewritten under an import,
`import` names the commit `main` is at and writes nothing.

## How it works

A **changeset** is the unit of work: commit metadata plus a flat list of files changed relative
to the first parent, each pointing at a content-addressed blob. Changesets are created on the
server and appended to a write-ahead log; there is nothing to push. Directory **trees**, file
history, blame and commit-graph indexes are **derived data**, computed recursively from each
changeset's parents and never stored in the changeset itself. Clients keep a bounded local
object cache and a sparse working copy, and resolve every tree and blob by immutable id through
a strict cache hierarchy, per-node daemon → regional cache → authoritative server, so every
request is a cacheable GET and no tier needs repository maintenance.

<p align="center">
  <img src="docs/assets/changeset.svg" width="800" alt="A changeset: repo, parents, author, committer, message, file_changes and extra; file changes point at blobs; trees, path history, blame and the skip list are derived">
</p>

<p align="center">
  <img src="docs/assets/commit-cost.svg" width="800" alt="Editing one file in the 2,500-file test tree rewrites the 5 directories on its path; the other 7,546 tree nodes keep their ids">
</p>

Where Scion deliberately departs from the design it was inspired by: trees live in the object
store rather than the metadata database
([why](docs/DATA_MODEL.md#deviation-trees-in-the-object-store)); hashing uses Scion's own
canonical BLAKE3 encoding, never a protobuf encoding; and the server refuses any ref other than
`main` and `release/*`.

Full details: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (components, planes, tiering,
ingestion), [`docs/DATA_MODEL.md`](docs/DATA_MODEL.md) (the changeset, the canonical encoding,
derived types) and [`docs/FAQ.md`](docs/FAQ.md). The figures are generated by
[`scripts/render-assets.py`](scripts/render-assets.py) (Python standard library only); edit
the script, not the SVGs.

## Benchmarks

[`scripts/bench-real`](scripts/bench-real) rebuilds 600 first-parent commits of a real
project's `library/` directory as a linear history. It imports that history into git and into
Scion on the compose stack (Postgres + an S3-compatible server, with a `scion-server` of its
own on loopback, one fresh bucket and server per run), then times each workload with both
tools. Import is timed twice: cold, into the run's empty bucket, and warm, into a second
repository on the same bucket. git runs its history workloads in a full clone, its best case.
`scn` runs them in a sparse clone of `core/`, its normal case. Every checkout is compared byte
for byte outside the timers. `log` and `blame` are timed twice: cold (the first call after
import, with nothing cached on the server) and warm, and once more from a second sparse clone
(server warm, client cold). For many writers landing at once, 16
clones each edit a different file and then all commit and publish together: git pushes, and
on a rejected push pulls with rebase and pushes again; `scn` lands. The table gives the total
from the first start to the last finish, and the p50 and p99 of the writers' own times.

<p align="center">
  <img src="docs/assets/bench-real.svg" width="100%"
       alt="Median times of Scion and git for import, clone, sparse clone, status, path log, blame, commit + land and 16 concurrent writers landing on a 600-commit slice of a real repository">
</p>

<!-- bench-real:start (scripts/render-assets.py writes this table) -->
| Workload | git, ms | scion, ms | scion / git |
| --- | ---: | ---: | ---: |
| import, cold | 1,237 | 20,961 | 16.9× |
| import, warm | 1,335 | 7,257 | 5.4× |
| clone | 1,649 | 1,277 | 0.77× |
| sparse clone | 502 | 97 | 0.19× |
| status | 172 | 8 | 0.05× |
| path log, cold | 23 | 170 | 7.4× |
| path log, warm | 21 | 21 | 1.0× |
| blame, cold | 49 | 187 | 3.8× |
| blame, warm | 45 | 11 | 0.24× |
| path log, server warm, client cold | 21 | 19 | 0.90× |
| blame, server warm, client cold | 48 | 11 | 0.23× |
| commit + land | 34 | 54 | 1.6× |
| land, 16 writers at once | 3,165 | 594 | 0.19× |
| land, 16 writers: p50 per land | 2,240 | 410 | 0.18× |
| land, 16 writers: p99 per land | 3,138 | 571 | 0.18× |

Repository: library/ of the public repository pinned in scripts/bench-real, first-parent commits since 2026-01-01 up to 91e9c2688eb23af9a5b77f056156e756550c7c99 (600 commits, 2330 files at the tip).  
Machine: Linux 6.18.44-fc-v140 x86_64; Intel(R) Xeon(R) Processor @ 2.10GHz; 4 cpus; 15 GiB.  
Run: git version 2.43.0; scion d3ebd05; 3 runs; 2026-10-21T04:23:20Z.  
Other implementations of the design: not measured in this run.
<!-- bench-real:end -->

Read these numbers with their limits:

- **Import** writes one immutable object per blob, tree and history page to a remote object
  store, where git writes a local packfile; parity with git's push is not a goal. The figure to
  quote is the cold row, 17× git: into an empty bucket the object store's write rate is the
  bound. Warm, with the objects already stored, it is 5.4× git.
- **Cold `log` and `blame`** pay one object-store round trip per pointer hop, which git's
  local packfile never does. Warm, both are at or below git.
- **A single commit + land** goes through a change request and the serial land worker, so on
  its own it takes 1.6× git's local commit and push. The worker pays off under contention: 16
  writers landing at once finish in 0.19× git's time, because no writer repeats another's
  fetch, rebase and push.
- One machine, three runs, shared cloud VM: medians move by several milliseconds between
  runs. The CSV with every sample is [docs/bench-real.csv](docs/bench-real.csv); render it with
  `python3 scripts/render-assets.py`. Older synthetic workloads against an in-process server
  are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

## Scope

- **Target:** one server serving many repositories, with Linux and macOS clients that are
  always online.
- **Not a git replacement.** For a repository that does not have the problem described above,
  git is the right tool; Scion only imports from git and never speaks its wire protocol.
- **Out of scope:** offline operation, Windows, a separate code-review product, garbage
  collection, large-file chunking (there is a per-blob size limit) and ignore files. See
  [`docs/FAQ.md`](docs/FAQ.md#what-is-scion-not-a-good-fit-for).

## What's next

Access control is in place: server-signed tokens, repository roles, directory-scoped grants,
GitHub sign-in and `scn access` for repository admins. Next come fuzzing of every boundary that
reads untrusted bytes and a seeded, hermetic simulation of many clients landing, merging and
reverting while the server restarts. In parallel, a performance pass aims at every benchmark row being a multiple faster than git: trees and history pages move into the metadata
database as the reference design keeps them, the object plane gains streaming batch requests, and
landing becomes one round trip. After that, in order: a web interface (repository, history, blame
and change-request pages with GitHub login), an HTTP/JSON API and an MCP server so coding agents
can read, commit and land through tools; released binaries, a server image, a production
compose profile and a hosted instance; the regional cache and per-node daemon tiers with
manifest-based seeding; and virtualized checkouts over FUSE / NFS. The detailed plan, with
acceptance criteria per step, is [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Repository layout

| Area | Crates |
|---|---|
| Format | `scion-types`: ids, changesets, trees, the canonical encoding and golden hashes |
| Storage | `scion-store`: `MetadataStore` / `ObjectStore` traits; memory, Postgres and S3 implementations |
| Algorithms | `scion-derived` (root tree, path history, blame, skip list), `scion-core` (diff, log, merge-base, merge, rebase), `scion-diff3` |
| Network | `scion-proto` (gRPC schema), `scion-server` (control plane, object origin, land worker) |
| Client | `scion-client`: `.scn/` layout, index, sparse profile, clone, checkout, commit and update; `scn`: the CLI |
| Import | `scion-ingest`: git → Scion importer (`import`, `verify`), checkpoints and incremental sync |
| Testing | `scion-sim`: seeded, hermetic simulation of the server with faults and restarts |

## Development

```sh
# toolchain is pinned by rust-toolchain.toml; scion-proto also needs `protoc` on PATH
# scion-ingest's tests also need `git` on PATH (apt-get install git / brew install git)
# scripts/bench-real also needs openssl and curl (apt-get install openssl curl / brew install openssl curl)
cargo test --workspace                     # includes 200 seeds of scion-sim
SCION_SIM_SEED=42 cargo test -p scion-sim --test seeds   # replay one simulation seed
cargo clippy --workspace --all-targets -- -D warnings

# local storage backends (Postgres + an S3-compatible server) for integration tests; the
# tests run their own server, so the compose scion-server service is not needed
docker compose -f deploy/docker-compose.yml up -d --wait postgres rustfs bucket-init
cargo test -p scion-store --features integration
cargo test -p scn --features integration --test e2e_stack   # scn + scion-server end to end

# Scion against git on a pinned slice of a real repository's history (needs network, git
# and Docker; the script header names the slice);
# writes target/bench-real/results.csv
scripts/bench-real 3
```

CI runs the same commands on Linux, on self-hosted runners (the header of
`.github/workflows/ci.yml` says how they behave); macOS is covered by developing on it.
Contributions go through pull requests; see [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
working rules in [`CLAUDE.md`](CLAUDE.md), which every change is reviewed against.

Golden-hash fixtures live in `crates/scion-types/tests/golden/`. They are the format
specification's executable form; see `docs/DATA_MODEL.md` before touching them.

## Prior art

- Google **Piper** (and CitC): a centralized monorepo on Spanner.
- Meta **Sapling / Mononoke / EdenFS**: bonsai changesets, derived data, virtual checkouts.
- **jj (Jujutsu)**: anonymous heads, operations as first-class data.
- Microsoft **VFS for Git / Scalar**: virtualized and cache-served git.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
