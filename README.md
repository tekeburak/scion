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
> Anthropic describes in [*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
> It is not an official Anthropic project and is not affiliated with or endorsed by Anthropic.

<p align="center">
  <a href="docs/ARCHITECTURE.md">Architecture</a> ·
  <a href="docs/DATA_MODEL.md">Data model</a> ·
  <a href="#getting-started">Getting started</a> ·
  <a href="#benchmarks">Benchmarks</a> ·
  <a href="docs/FAQ.md">FAQ</a> ·
  <a href="docs/ROADMAP.md">Roadmap</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

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

- **Postgres and S3, nothing else.** Metadata (changesets, refs, derived data, trees) lives in
  Postgres; blobs live in any S3-compatible object store. No packfiles, no repository
  maintenance, no materialized copy of the repository on any server.
- **A commit costs what it changes.** A changeset records only the files it changed against its
  first parent. Directory trees, path history, blame and the ancestry index are *derived data*,
  computed from that and replaceable without a client change. One changed file in a 2,500-file
  tree rewrites five tree nodes: the directories on its path.
- **Thin, sparse clients.** A working copy holds the directories you asked for; everything
  else is fetched by immutable id, so every object request names content that never changes
  and any tier can answer it from cache. Today clients fetch from the server directly; the
  per-node daemon and regional cache tiers are planned.
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
directory; people sign in with GitHub (`scn login`) and services use tokens from the
operator. Landing goes through change requests and a serial
land worker, and every request can be listed and traced to the changeset it landed;
merges and reverts run on the server. A [web interface](#web-interface), an HTTP/JSON API and
an MCP server serve people and agents. A release workflow builds static Linux binaries and a
server image; the first tagged release, a hosted instance and the cache and daemon tiers follow
(see [What's next](#whats-next)).

What works today:

- **`scion-server`**: the control-plane and object gRPC services over Postgres + S3 — `log`,
  `diff`, `path_log`, `merge_base`, `blame`, `create_changeset`, `move_ref`, `rebase`, `merge`,
  `revert`, change requests — plus `backfill`, which derives a type for a repository's whole history
  ahead of the first request.
- **Change requests and the land worker**: a submitted head is landed by one serial worker
  per repository, in submission order, rebased onto the current `main` on the server and
  moved in with a single compare-and-swap. While one request's swap is in flight, the worker
  already rebases the next onto the `main` that swap will make, and starts that one over if
  the swap is lost, so requests still land strictly in order. A conflict leaves `main`
  untouched and reports the conflicting paths; a request interrupted by a server restart
  resumes and lands exactly once.
- **`scn`**, the CLI: `init` a repository, `clone --sparse`, `status`, `log`, `diff`, `blame`,
  `add`, `commit` (an anonymous head; `--land` also lands it, in the same round trip), `land`
  (submits the head as a change request; the server rebases it onto `main` and lands it, or
  names the conflicting paths), `update` (the
  server rebases your head onto `main`; `--conflicts` writes conflict markers to resolve and
  commit instead of refusing), `requests` (the repository's change requests, newest
  first, by status), `log --landed` (which request landed each changeset, and when), `merge`
  (the server merges a revision such as a `release/*` fix into your head; blame credits the
  fix to the changeset that wrote it), `revert` (the server undoes a changeset on your head,
  refusing if its files changed since; blame keeps crediting the restored lines to their
  authors) and `checkout` of any changeset. A clone fetches objects in batches, every tree of
  a directory it takes whole in one request and blobs 256 at a time, so a 2,330-file clone is
  12 requests; `commit` asks the server which contents it already holds and uploads only the
  rest, in one stream. Two clones landing
  at the same time, with no `update` in between, is the CI end-to-end test. An `https://`
  server address is checked against the public certificate authorities, plus the
  certificates in `SSL_CERT_FILE` for a private one; the server itself speaks plain HTTP/2
  behind a proxy that terminates TLS.
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
  who a token names, until when, and its roles in the repository. Nothing writes without a
  token, over gRPC, the HTTP API or the MCP tools alike, and each of them refuses a blob over
  the 64 MiB per-blob limit with an error that names it.
- **Web interface** (`scion-server --web <addr>`): server-rendered pages for the repositories a
  visitor can read: repository home with its README, directories and files at any ref or
  changeset, log and per-path history, blame, changesets with their line diffs, and change
  requests with their status and conflicts. From a change request's page, writers rebase it
  onto `main` to see whether it still conflicts and land it again, and its submitter or an
  admin rejects it; each page shows only the buttons the visitor's role allows. A server
  admin creates repositories from the repository list, and a repository's settings page lets
  its admins make it public or private and grant and revoke roles. The pages run inside the
  server and check the same roles and directory grants as `scn`. People sign in with
  GitHub in the browser (once the
  OAuth app's `SCION_GITHUB_CLIENT_SECRET` is set; the server keeps no session state), tools
  send `Authorization: Bearer <token>`, and `scn access grant anyone reader` makes a
  repository, or with `--path` one directory, public: readable without signing in.
- **HTTP/JSON API** (`/api/v1` on the web address, [`docs/API.md`](docs/API.md)): the gRPC
  control plane, object reads and blob uploads, for clients that do not speak gRPC, from creating
  a repository and reading trees, blobs, log, diff and blame to uploading blobs, creating
  changesets and landing them, with the same tokens, roles and public reads. A clone, a
  commit and a land need nothing but `curl`.
- **MCP server for agents** (`scion-mcp`, [`docs/MCP.md`](docs/MCP.md)): coding agents list
  repositories, browse trees, read files at any revision, read log, blame, diffs and
  changesets, write files as a new changeset and land it, through Model Context Protocol tools
  and `scion://<repo>/<path>@<revision>` resources, on stdio or streamable HTTP, with the same
  tokens and roles.
- **Output for scripts and agents**: `scn --json <command>` prints one JSON document per
  command, described by a JSON Schema in [`schemas/v1`](schemas/v1); a failure prints an error
  with a stable code (`conflict`, `needs_rebase`, `dirty`, `permission_denied`, …) and the paths
  it is about. `scn explain-conflict` names, for each path that keeps the head from rebasing onto
  `main`, the changesets on both sides that changed it since the merge base. Every binary
  prints its version with `--version`, and a release build also names the commit it was built
  from; `GET /api/v1/version` and the web interface's footer show the server's.
- **Release binaries**: pushing a `v*` tag builds `scn`, `scion-server`, `scion-ingest`
  and `scion-mcp` as static `x86_64` and `aarch64` musl executables, and `scn` for macOS,
  packs one archive per target with a `SHA256SUMS` file and opens a draft GitHub release.
  `scripts/install.sh` picks the archive for the machine, checks it against `SHA256SUMS` and
  installs the binaries into `~/.local/bin`. The same tag pushes the server image,
  `ghcr.io/tekeburak/scion-server:<version>` for `linux/amd64` and `linux/arm64`: the static
  `scion-server` on a distroless base, running as a non-root user, configured by the
  environment variables listed in `crates/scion-server/src/main.rs`. No release has been
  tagged yet; until then, build from source as below.
- **Production compose profile** ([`deploy/compose.prod.yml`](deploy/compose.prod.yml)): the
  server image, Postgres on a volume, your own S3 bucket and Caddy terminating TLS on one
  `https://` address with a Let's Encrypt certificate; `scn` and `scion-mcp` reach the gRPC
  services and browsers the web interface through it. Every secret comes from one env file,
  [`deploy/prod.env.example`](deploy/prod.env.example) lists them. The release workflow brings
  the profile up with a local certificate authority and logs in, inits, clones, commits and
  lands through it.
- **Robustness checks**: a seeded, hermetic simulation runs many clients committing, landing,
  merging and reverting while land workers stop and the server restarts, and checks that
  `main` stays linear and nothing landed is lost or written twice; fuzz targets cover every
  boundary that reads untrusted bytes (changeset, tree and derived-object decoding, tokens,
  the three-way merge and the write requests), and their corpus is replayed on every test run.
- **Benchmarks** against git on a pinned slice of a real repository, from clone and `blame`
  to 16 writers landing at once, with the numbers [below](#benchmarks).

## Getting started

On a machine with Docker (with compose) and the Rust toolchain, from a checkout of this
repository:

```sh
# Postgres, S3 (RustFS) and scion-server on 127.0.0.1:7420, its web interface on
# 127.0.0.1:8080; the first run builds the server
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
scn access grant anyone reader             # make demo public, then open
                                           # http://127.0.0.1:8080/demo in a browser
```

After a code change, rebuild
the server with `docker compose -f deploy/docker-compose.yml up -d --wait --build`.
`docker compose -f deploy/docker-compose.yml down -v` deletes every repository; the next `up`
starts a new server with it. To sign in with GitHub instead of `SCN_TOKEN`, create a GitHub
OAuth app with device flow enabled, start the stack with `SCION_GITHUB_CLIENT_ID` set to its
client id and run `scn login http://127.0.0.1:7420`; the token lands in
`~/.config/scn/tokens/` (mode 0600) and is valid for 30 days. With the app's client secret in
`SCION_GITHUB_CLIENT_SECRET` too (callback URL `http://127.0.0.1:8080/login/callback`), *Sign
in* on the pages does the same in the browser and shows private repositories. A token stays valid
until its `--ttl` runs out, also across `down -v`. A `scion-server` run
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
`scn commit --land -m <message>` commits and lands in one step, with one round trip to the
server for both. `scn land --no-wait` prints the request id at once and
`scn land --status <id>` reads its outcome later; `scn requests --status needs-rebase` lists
only the conflicted requests, and
`scn log --landed` finds the request behind a changeset even when the server rebased it to a
new id.

### Sign in and share a repository

Every request carries a token, and every repository has readers, writers and admins. The
`demo` repository above belongs to `sp:dev`, the principal of `SCN_TOKEN`; here it lets a
second person write under `docs/` only. That person signs in with GitHub (the stack needs
`SCION_GITHUB_CLIENT_ID`, see above); a CI job or an agent uses a service-principal token
from `scion-server token sp:<name> --ttl <duration>` in `SCN_TOKEN` instead.

```sh
cd demo                                    # the working copy from above; you are its admin
scn access grant github:<login> writer --path docs
scn access list                            # sp:dev admin on demo, github:<login> writer on demo:docs/
cd ..

# the second person, in a shell without SCN_TOKEN
unset SCN_TOKEN
export SCN_AUTHOR="Grace <grace@example.com>"
scn login http://127.0.0.1:7420            # prints a URL and a code to enter on GitHub
scn whoami http://127.0.0.1:7420          # github:<login> and its expiry
scn clone http://127.0.0.1:7420 demo shared && cd shared
mkdir docs && echo notes >docs/notes.md
scn add docs/notes.md && scn commit -m "add notes"
scn land                                   # lands: every path is under docs/
echo nope >top.txt && scn add top.txt
scn commit -m "add top.txt"                # refused, naming top.txt: outside docs/
```

The admin's `scn access revoke github:<login> --path docs` takes effect on that person's next
request. A token itself cannot be revoked; it expires (30 days for `scn login`). The rules,
the roles and what each request needs are in
[Identity and access](docs/ARCHITECTURE.md#identity-and-access).

### Import a git repository

`scion-ingest` copies a git history into Scion: one changeset per commit, file changes
against the first parent, renames and copies kept for `blame`, the git sha kept in the
changeset's `extra`, and `main` pointed at the imported tip. It imports into a repository
created with `scn init` and never creates one; the example below uses its own repository,
`imported`, so it also runs on a stack that still holds `demo` from above. The rules, and what it refuses, are in
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

scn init http://127.0.0.1:7420 imported  # an empty repository; import never creates one

scion-ingest import demo imported main   # git dir, Scion repository, git revision
scion-ingest verify demo imported        # every imported tree against git ls-tree

scn clone http://127.0.0.1:7420 imported imported-scn
cd imported-scn && scn log && scn blame README.txt   # "one" to "three" blame to "add notes"
```

Run `import` again after new git commits and it imports only those and fast-forwards `main`;
an interrupted import resumes the same way. If the git history was rewritten under an import,
`import` names the commit `main` is at and writes nothing.

### Run your own server

On a machine with Docker whose DNS name points at it and whose ports 80 and 443 are open,
from a checkout of this repository:

```sh
# no release is tagged yet, so there is no image to pull: build one from this
# checkout and set SCION_VERSION=local in prod.env
docker build -f deploy/scion-server.Dockerfile -t ghcr.io/tekeburak/scion-server:local .
cp deploy/prod.env.example prod.env   # fill in every value; keep it private
docker compose -f deploy/compose.prod.yml --env-file prod.env up -d
# the first admin: sign in with GitHub (needs the OAuth app in prod.env), then
docker compose -f deploy/compose.prod.yml --env-file prod.env exec scion-server \
    scion-server grant '*' github:<your login> admin
scn login https://scion.example.com   # the SCION_DOMAIN from prod.env
scn init https://scion.example.com demo
```

The bucket must already exist; Caddy obtains and renews the certificate itself. Once releases
are tagged, `SCION_VERSION` names one (`ghcr.io/tekeburak/scion-server:<version>`, pulled on
`up`) and the build step goes away. To upgrade, change `SCION_VERSION` in `prod.env` (or
rebuild the `local` image) and run the same `up -d`; Postgres keeps its volume.

## Web interface

`scion-server --web <addr>` serves server-rendered pages from the same process as the gRPC
services; the compose stack above serves them on `http://127.0.0.1:8080`. There is nothing else
to deploy and no JavaScript build. Every page reads through the same role and directory checks
as `scn`, and shows only the buttons the visitor's role allows.

<table>
  <tr>
    <td width="50%"><img src="docs/assets/web-home.png" alt="Repository home: the root directory at main and the rendered README"><br>
      <sub>Repository home: the tree at <code>main</code> or any changeset, and its README.</sub></td>
    <td width="50%"><img src="docs/assets/web-diff.png" alt="A changeset page: its message, author, parent and line diff"><br>
      <sub>A changeset: message, author, first parent and the line diff of each file.</sub></td>
  </tr>
  <tr>
    <td width="50%"><img src="docs/assets/web-blame.png" alt="Blame of src/greet.rs, each line credited to the changeset that wrote it"><br>
      <sub>Blame: each line credited to the changeset that last wrote it.</sub></td>
    <td width="50%"><img src="docs/assets/web-request.png" alt="A change request that conflicts with main, naming src/greet.rs, with rebase, land and reject buttons"><br>
      <sub>A change request that conflicts with <code>main</code>: the paths, and the actions a writer can take.</sub></td>
  </tr>
</table>

The pages cover the repository list (with *Create repository* for a server admin), each
repository's tree and files, log and per-path history, blame, changesets, change requests with
rebase, land and reject, and a settings page where admins make the repository public or private
and grant and revoke roles. Public repositories read without signing in; tools send
`Authorization: Bearer <token>` to the pages and to the HTTP/JSON API under `/api/v1` on the
same address ([`docs/API.md`](docs/API.md)).

## How it works

A **changeset** is the unit of work: commit metadata plus a flat list of files changed relative
to the first parent, each pointing at a content-addressed blob. Changesets are created on the
server and appended to a write-ahead log; there is nothing to push. Directory **trees**, file
history, blame and commit-graph indexes are **derived data**, computed recursively from each
changeset's parents and never stored in the changeset itself. Clients keep a sparse working
copy and resolve every tree and blob by immutable id, so every object request is cacheable and
no tier needs repository maintenance. The design puts a strict cache hierarchy in that path,
per-node daemon → regional cache → authoritative server; today the server answers directly,
and the two cache tiers are next on the roadmap.

<p align="center">
  <img src="docs/assets/changeset.svg" width="800" alt="A changeset: repo, parents, author, committer, message, file_changes and extra; file changes point at blobs; trees, path history, blame and the skip list are derived">
</p>

<p align="center">
  <img src="docs/assets/commit-cost.svg" width="800" alt="Editing one file in the 2,500-file test tree rewrites the 5 directories on its path; the other 7,546 tree nodes keep their ids">
</p>

Where Scion deliberately departs from the design it was inspired by: hashing uses Scion's own
canonical BLAKE3 encoding, never a protobuf encoding; and the server refuses any ref other than
`main` and `release/*`. Trees and history pages, which first lived in the object store, now
sit in the metadata database next to the changesets, as in the reference design
([why](docs/DATA_MODEL.md#trees-in-the-metadata-database)).

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
Given checkouts of other public implementations of the same design (`BENCH_PEERS`, see
[docs/BENCHMARKS.md](docs/BENCHMARKS.md#other-implementations)), the script runs every
workload against them too, on the same machine, and the table lists them under neutral labels;
a workload one of them cannot express reads `unsupported`.
`BENCH_SLICE=large` runs the same workloads on a larger slice, about 6,000 commits of the
same project's `compiler/` directory; its table follows the first. A nightly CI job runs both
slices and fails when a row's scion / git ratio grows more than 20% against these tables.

<p align="center">
  <img src="docs/assets/bench-real.svg" width="100%"
       alt="Median times of Scion and git for import, clone, sparse clone, status, path log, blame, commit + land and 16 concurrent writers landing on a 600-commit slice of a real repository">
</p>

<!-- bench-real:start (scripts/render-assets.py writes this table) -->
| Workload | git, ms | scion, ms | impl-a, ms | impl-b, ms | scion / git |
| --- | ---: | ---: | ---: | ---: | ---: |
| import, cold | 1,386 | 11,629 | 180,789 | unsupported | 8.4× |
| import, warm | 1,365 | 2,580 | 196,170 | unsupported | 1.9× |
| clone | 1,617 | 582 | 1,187 | 249 | 0.36× |
| sparse clone | 513 | 28 | 167 | 36 | 0.05× |
| status | 161 | 7 | 10 | unsupported | 0.04× |
| path log, cold | 22 | 79 | 1,899 | unsupported | 3.6× |
| path log, warm | 22 | 16 | 1,263 | unsupported | 0.73× |
| blame, cold | 49 | 56 | 1,253 | unsupported | 1.1× |
| blame, warm | 57 | 8 | 23 | unsupported | 0.14× |
| path log, server warm, client cold | 23 | 18 | 1,242 | unsupported | 0.78× |
| blame, server warm, client cold | 61 | 9 | 24 | unsupported | 0.15× |
| commit + land | 43 | 52 | 178 | 7 | 1.2× |
| land, 16 writers at once | 3,022 | 295 | 4,943 | unsupported | 0.10× |
| land, 16 writers: p50 per land | 2,045 | 207 | 3,193 | unsupported | 0.10× |
| land, 16 writers: p99 per land | 3,014 | 285 | 4,922 | unsupported | 0.09× |

Repository: library/ of the public repository pinned in scripts/bench-real, first-parent commits since 2026-01-01 up to 91e9c2688eb23af9a5b77f056156e756550c7c99 (600 commits, 2330 files at the tip).  
Machine: Linux 6.18.44-fc-v153 x86_64; Intel(R) Xeon(R) Processor @ 2.10GHz; 4 cpus; 15 GiB.  
Run: git version 2.43.0; scion 52d0dbc; 3 runs; 2026-10-24T05:02:50Z.  
impl-a: another implementation of the design, at 7d445a0744e30f4dcb36a794b0ce42648157e1b9 over CockroachDB v24.3.5 + the same S3.  
impl-b: another implementation of the design, at 00e92ab9c2a9985f39a64636b4b6c5c4bb583916 in memory.
<!-- bench-real:end -->

The larger slice (`BENCH_SLICE=large`; sparse clone of `rustc_middle/`):

<!-- bench-real-large:start (scripts/render-assets.py writes this table) -->
| Workload | git, ms | scion, ms | scion / git |
| --- | ---: | ---: | ---: |
| import, cold | 17,556 | 167,752 | 9.6× |
| import, warm | 17,861 | 31,253 | 1.7× |
| clone | 18,076 | 905 | 0.05× |
| sparse clone | 3,391 | 17 | 0.01× |
| status | 180 | 7 | 0.04× |
| path log, cold | 99 | 296 | 3.0× |
| path log, warm | 107 | 48 | 0.45× |
| blame, cold | 394 | 499 | 1.3× |
| blame, warm | 447 | 12 | 0.03× |
| path log, server warm, client cold | 102 | 56 | 0.55× |
| blame, server warm, client cold | 421 | 13 | 0.03× |
| commit + land | 56 | 62 | 1.1× |
| land, 16 writers at once | 3,859 | 551 | 0.14× |
| land, 16 writers: p50 per land | 2,552 | 367 | 0.14× |
| land, 16 writers: p99 per land | 3,842 | 528 | 0.14× |

Repository: compiler/ of the public repository pinned in scripts/bench-real, first-parent commits since 2023-02-01 up to 91e9c2688eb23af9a5b77f056156e756550c7c99 (6008 commits, 2978 files at the tip).  
Machine: Linux 6.18.44-fc-v153 x86_64; Intel(R) Xeon(R) Processor @ 2.80GHz; 4 cpus; 15 GiB.  
Run: git version 2.43.0; scion e1ee623; 3 runs; 2026-10-24T05:12:42Z.  
Other implementations of the design: not measured in this run.
<!-- bench-real-large:end -->

Read these numbers with their limits:

- **Import** writes one immutable object per blob, tree and history page to a remote object
  store, where git writes a local packfile; parity with git's push is not a goal. The figure to
  quote is the cold row, 8.4× git: into an empty bucket the object store's write rate is the
  bound. Warm, with the objects already stored, it is 1.9× git.
- **Cold `log` and `blame`**, the first call after import with nothing cached on the server,
  take 3.6× and 1.1× git: a path's history is walked in lockstep, one read per step, and blame
  answers from the file's own history. Warm, both are answered from memory: `blame` is 0.14×
  git and a path log 0.73×. Clone is 0.36× git and a sparse clone 0.05×.
- **`status`** lists only the directories whose entries changed, so a clean `status` reads no
  file and the row (0.04× git) is mostly the cost of starting a process; see
  [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).
- **A single commit + land** (`scn commit --land`, one round trip to land) still goes through a
  change request and the serial land worker, so on its own it takes longer than git's local
  commit and push (52 ms against 43 ms). The worker pays off under contention: 16 writers
  landing at once finish in 0.10× git's time, because no writer repeats another's fetch, rebase
  and push, and the worker rebases the next request while the previous one lands.
- **The other implementations** ran on the same machine and stack, at the commits named under
  the table; `unsupported` marks a workload one of them has no command for.
- One machine, three runs, shared cloud VM: medians move by several milliseconds between
  runs. The CSVs with every sample are [docs/bench-real.csv](docs/bench-real.csv) and
  [docs/bench-real-large.csv](docs/bench-real-large.csv); render them with
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

The work in flight and the work planned after it, in order:

- **Wider benchmarks** (in progress). A second, larger slice of the real repository, and a
  nightly run that turns red when any row regresses by more than 20%. It closes a performance
  pass that already batches clone and commit, lands a commit in one round trip, answers a warm
  `log` or `blame` from memory, has `status` list only the directories whose entries changed
  and has the land worker prepare the next change request while the previous one lands.
- **Releases and hosting** (in progress). The first tagged release, release notes generated
  from merged changes, a hosted instance, and a five-minute quickstart from the install script
  to a landed change.
- **Cache tiers** (planned). A regional cache and a per-node daemon in front of the server,
  seeded from snapshot manifests; today clients talk to the server directly.
- **Virtualized checkouts** (planned). Working copies served over FUSE / NFS.

The detailed plan, with acceptance criteria per step, is [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Repository layout

| Area | Crates |
|---|---|
| Format | `scion-types`: ids, changesets, trees, the canonical encoding and golden hashes |
| Storage | `scion-store`: `MetadataStore` / `ObjectStore` traits; memory, Postgres and S3 implementations |
| Algorithms | `scion-derived` (root tree, path history, blame, skip list), `scion-core` (diff, log, merge-base, merge, rebase), `scion-diff3` |
| Network | `scion-proto` (gRPC schema), `scion-server` (control plane, object origin, land worker) |
| Web | `scion-web`: the web interface's server-rendered pages, served in process by `scion-server --web` |
| Agents | `scion-mcp`: the MCP server agents run, a client of the HTTP/JSON API |
| Client | `scion-client`: `.scn/` layout, index, sparse profile, clone, checkout, commit and update; `scn`: the CLI |
| Import | `scion-ingest`: git → Scion importer (`import`, `verify`), checkpoints and incremental sync |
| Testing | `scion-sim`: seeded, hermetic simulation of the server with faults and restarts |

## Development

```sh
# toolchain is pinned by rust-toolchain.toml; scion-proto also needs `protoc` on PATH
# scion-ingest's tests also need `git` on PATH (apt-get install git / brew install git)
# scripts/bench-real also needs openssl and curl (apt-get install openssl curl / brew install openssl curl)
# release archives (static musl binaries, as .github/workflows/release.yml builds them) need zig and
# cargo-zigbuild: pip install ziglang (or brew install zig) && cargo install cargo-zigbuild --locked
#   rustup target add x86_64-unknown-linux-musl && cargo zigbuild --release --locked \
#     --target x86_64-unknown-linux-musl -p scn -p scion-server -p scion-ingest -p scion-mcp
#   macOS scn the same way: rustup target add aarch64-apple-darwin && cargo zigbuild --release \
#     --locked --target aarch64-apple-darwin -p scn
# scripts/install.sh is checked with shellcheck (apt-get install shellcheck / brew install shellcheck):
#   shellcheck -s sh scripts/install.sh scripts/install-test.sh && sh scripts/install-test.sh
# the production compose profile, end to end through Caddy, against a server image tagged
# ghcr.io/tekeburak/scion-server:<version> (needs Docker with compose 2.24+ and curl):
#   shellcheck -s sh scripts/prod-profile-test.sh && sh scripts/prod-profile-test.sh <version> <scn>
cargo test --workspace                     # includes 200 seeds of scion-sim
SCION_SIM_SEED=42 cargo test -p scion-sim --test seeds   # replay one simulation seed
cargo test -p scion-sim --test corpus      # replay the fuzz corpus on stable
cargo fmt --all -- --check                 # fix with: cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
# dependency bans, licenses, sources and advisories (cargo install cargo-deny --locked)
cargo deny check bans licenses sources advisories

# fuzzing needs a nightly toolchain and cargo-fuzz:
#   rustup toolchain install nightly && cargo +nightly install cargo-fuzz --locked
(cd crates/scion-sim/fuzz && cargo +nightly fuzz run request -- -max_total_time=300)

# poke the MCP server by hand with the MCP inspector (needs Node.js: apt-get install nodejs npm /
# brew install node); point it at a server started with --web
npx @modelcontextprotocol/inspector cargo run -q -p scion-mcp -- --api http://127.0.0.1:8080/api/v1

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
- Meta **Sapling / Mononoke / EdenFS**: bonsai changesets (the model Scion's changeset follows),
  derived data, virtual checkouts. Anthropic's post credits Mononoke with pioneering many of the
  techniques its system uses.
- **jj (Jujutsu)**: anonymous heads, operations as first-class data.
- Microsoft **VFS for Git / Scalar**: virtualized and cache-served git.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
