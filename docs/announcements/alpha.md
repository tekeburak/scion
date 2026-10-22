# Announcement drafts: the alpha (self-hosting, web interface, agent API, current benchmark)

**Status: draft. Not posted anywhere.** Rewritten after Phase 9.8 (#187) merged: on top of
access control, the hermetic simulation and the fuzz targets, Scion now has a web interface
where writers land change requests, an HTTP/JSON API, an MCP server for agents, release
binaries and a server image built on a `v*` tag (none tagged yet), an install script, `https://`
from `scn`, and a production compose profile behind TLS. It replaces the previous version of
this file and [`phase-6-benchmark.md`](phase-6-benchmark.md), whose numbers are out of date.
Every number below is a median from the README's [Benchmarks](../../README.md#benchmarks)
table (`docs/bench-real.csv`, the run taken with Phase 10.2's grants and refs memo, #182, three
runs, 2026-10-22); re-check them against the README before posting. Like the README, these
drafts describe the benchmark slice without naming the project it comes from.

## Show HN

**Title:** Show HN: Scion – server-authoritative source control on Postgres and S3 for agent-driven monorepos

Scion is an open-source (Apache-2.0) client-server version control system written in Rust,
for one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata, trees and history pages in Postgres and blobs in an
S3-compatible object store. The `scn` CLI can `init`, `clone --sparse`, `status`, `log`,
`diff`, `blame`, `commit`, `land` (a change request that one serial worker per repository
rebases onto `main` on the server), `update`, `merge`, `revert` and `checkout`; `scion-ingest`
imports a git history and verifies every imported tree against `git ls-tree`. Every request
needs a token signed by the server and a role in the repository, optionally limited to a
directory; people sign in with GitHub and agents use service tokens. The same server serves a
web interface (files, log, blame, diffs, change requests that writers land or reject), an
HTTP/JSON API and, through `scion-mcp`, MCP tools for agents. It is an independent project
inspired by Anthropic's post "Redesigning source control at Anthropic", and is not affiliated
with Anthropic.

What is different from git: a commit is created on the server, so there is no push, no
packfile and no repository maintenance. A changeset records only the files that changed
against its first parent; trees, path history, blame and the ancestry skip list are derived
data, so a commit costs what it changes (one changed file in a 2,500-file tree rewrites 5 tree
nodes, which a test asserts). The only server refs are `main` and `release/*`; work in progress
is an anonymous head. A seeded, hermetic simulation runs many clients landing, merging and
reverting while land workers stop and the server restarts, and checks that `main` stays linear
and nothing landed is lost or written twice. To run your own, `deploy/compose.prod.yml` brings
up the server image, Postgres and Caddy terminating TLS on one `https://` address.

The numbers come from `scripts/bench-real`, which rebuilds 600 first-parent commits of a real
open-source repository's `library/` directory and times both tools on one 4-vCPU cloud VM
(compose Postgres + S3, server on loopback; git in a full clone, `scn` in a sparse clone).
Medians: 16 writers each landing a different file at once finish in 405 ms against 3,314 ms
for git's fetch-rebase-push retry loop (p99 per writer 396 ms against 3,284 ms), sparse clone
123 ms against 606 ms, `status` 9 ms against 153 ms, warm `blame` 12 ms against 59 ms, and
warm path `log` 23 ms against 23 ms. Where git wins: a single commit + `land` is 47 ms against
45 ms, because it goes through a change request; import into an empty bucket takes 9.0× git's
time (1.9× with the objects already stored); and the first path `log` and `blame` after import,
with nothing cached on the server, take 5.6× and 3.8× git's. What is missing: a tagged release,
a hosted instance, the cache and daemon tiers, and by design Windows, offline mode and git wire
compatibility. It is alpha software measured on one machine, and for most repositories git is
the right tool.

## r/rust

**Title:** Scion, a server-authoritative VCS in Rust on Postgres + S3: a fuzzed boundary, a hermetic simulation and a benchmark against git

Scion is a Cargo workspace for a client-server VCS aimed at one very large monorepo with
always-online clients: `scion-types` (ids, changesets, a hand-written canonical encoding hashed
with BLAKE3, frozen golden-hash fixtures), `scion-store` (Postgres and S3 behind two small
traits, plus an in-memory implementation for tests), `scion-derived` (root trees, path history,
blame, skip lists), `scion-core` and `scion-diff3` (diff, merge-base, rebase, three-way merge),
`scion-server` (tonic gRPC, the land worker), `scion-web` (server-rendered pages with `maud`
and an HTTP/JSON API on `axum`, no JavaScript build), `scion-mcp` (an MCP server on `rmcp`),
`scion-ingest` (git importer with `gix`), `scion-sim` and the `scn` CLI. A `v*` tag builds
static musl binaries for `x86_64` and `aarch64` Linux and `scn` for macOS, all cross-compiled
on Linux runners.

Two pieces may interest Rust folks. `scion-sim` runs the real server in process on the memory
stores with seeded clients and injected faults (land workers stopped, server restarts), and 200
seeds run on every `cargo test`. Fuzz targets cover every boundary that reads untrusted bytes
(changeset, tree and derived-object decoding, tokens, the three-way merge and the write
requests), each a plain function in `scion-sim`, so the checked-in corpus replays under stable
`cargo test` and `cargo-fuzz` drives the same code on nightly; the properties go beyond "no
panic": canonical decoding re-encodes to the same bytes, and no write request overwrites a
stored object.

`scripts/bench-real` imports a pinned 600-commit slice of a real repository into git and into
Scion on Postgres + an S3-compatible server and times each workload with both, checking every
checkout byte for byte outside the timers. Sixteen writers landing at once (405 vs 3,314 ms),
sparse clone (123 vs 606 ms), `status` (9 vs 153 ms) and warm `blame` (12 vs 59 ms) come in
below git, warm path `log` ties it (23 vs 23 ms); a single commit + `land` (47 vs 45 ms), cold
path `log` and `blame` (5.6× and 3.8×) and import (9.0× into an empty bucket, 1.9× warm) do
not, and the README says why. A performance pass is under way: trees and history pages already
moved into Postgres and the server memoizes grants and refs across processes through
`LISTEN`/`NOTIFY`; next, import derives trees concurrently, the object plane streams batches and
landing becomes one round trip. Alpha, not production-ready; feedback on the derived-data
design and the benchmark method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi ve ağaçları Postgres'te, blob'ları S3
uyumlu bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Her istek sunucunun imzaladığı bir token ve repoda (istenirse bir dizinle sınırlı) bir rol
istiyor; insanlar GitHub ile giriyor, sunucu aynı rollerle bir web arayüzü, bir HTTP/JSON API
ve ajanlar için bir MCP sunucusu sunuyor.
Gerçek bir açık kaynak reponun 600 commit'lik geçmişiyle git'e karşı ölçülen medyan değerlerde
aynı anda `land` eden 16 yazar 405 ms'de bitiriyor (git 3.314 ms), sparse clone 123 ms (git
606 ms), `status` 9 ms (git 153 ms), sıcak `blame` 12 ms (git 59 ms) sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: tek bir commit + `land` 47 ms (git 45 ms), boş
bucket'a içe aktarma git'in 9,0 katı (nesneler zaten varken 1,9 katı), soğuk yol `log`'u 5,6
katı, soğuk `blame` 3,8 katı sürüyor.
Proje alfa aşamasında: kendi sunucunu TLS arkasında kurmak mümkün, ama etiketlenmiş bir sürüm,
barındırılan bir örnek ve önbellek katmanları henüz yok; çoğu repo için doğru araç hâlâ git.
