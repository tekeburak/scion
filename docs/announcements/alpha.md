# Announcement drafts: the alpha (self-hosting, web interface, agent API, current benchmark)

**Status: draft. Not posted anywhere.** Written after Phase 9.8 (#187) merged and checked
against main after Phase 10.9 (#211), the latest merged step: on top of
access control, the hermetic simulation and the fuzz targets, Scion now has a web interface
where writers land change requests, an HTTP/JSON API, an MCP server for agents, release
binaries and a server image built on a `v*` tag (none tagged yet), an install script, `https://`
from `scn`, and a production compose profile behind TLS. It is the only announcement draft;
the earlier MVP and first-benchmark drafts were removed because their numbers were out of date.
Every number below is a median from the README's [Benchmarks](../../README.md#benchmarks)
table (`docs/bench-real.csv`, the run re-taken in #211 with Phase 10.9's pipelined land worker,
three runs, 2026-10-24); re-check them against the README before posting. Like the README,
these drafts describe the benchmark slice without naming the project it comes from.

## Show HN

**Title:** Show HN: Scion – server-authoritative source control on Postgres and S3 for agent-driven monorepos

Scion is an open-source (Apache-2.0) client-server version control system written in Rust,
for one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata, trees and history pages in Postgres and blobs in an
S3-compatible object store. The `scn` CLI can `init`, `clone --sparse`, `status`, `log`,
`diff`, `blame`, `commit`, `land` (a change request that one serial worker per repository
rebases onto `main` on the server; `commit --land` does both in one round trip), `update`,
`merge`, `revert` and `checkout`; `scion-ingest` imports a git history and verifies
every imported tree against `git ls-tree`. Every request needs a token signed by the server
and a role in the repository, optionally limited to a
directory; people sign in with GitHub and agents use service tokens. The same server serves a
web interface (files, log, blame, diffs, change requests that writers land or reject), an
HTTP/JSON API and, through `scion-mcp`, MCP tools for agents. It is an independent project
inspired by Anthropic's post "Redesigning source control at Anthropic"
(https://www.anthropic.com/research/redesigning-source-control), and is not affiliated with
Anthropic. Its changeset follows the bonsai changeset model of Meta's Mononoke, which the post
credits with pioneering many of the techniques it describes.

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
Medians: 16 writers each landing a different file at once finish in 295 ms against 3,022 ms
for git's fetch-rebase-push retry loop (p99 per writer 285 ms against 3,014 ms), a clone
582 ms against 1,617 ms, sparse clone 28 ms against 513 ms, `status` 7 ms against 161 ms, warm
`blame` 8 ms against 57 ms and warm path `log` 16 ms against 22 ms. Where git wins: a single
commit + `land` is 52 ms against 43 ms, because it goes through a change request; import into
an empty bucket takes 8.4× git's time (1.9× with the objects already stored); and the first
path `log` and `blame` after import, with nothing cached on the server, take 3.6× and 1.1×
git's. What is missing: a tagged release,
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
checkout byte for byte outside the timers. Sixteen writers landing at once (295 vs 3,022 ms),
clone (582 vs 1,617 ms), sparse clone (28 vs 513 ms), `status` (7 vs 161 ms), warm `blame`
(8 vs 57 ms) and warm path `log` (16 vs 22 ms) come in below git; a single commit + `land`
(52 vs 43 ms), cold path `log` and `blame` (3.6× and 1.1×) and import (8.4× into an empty
bucket, 1.9× warm) do not, and the README says why. A performance pass is under way:
trees and history pages moved into Postgres, the server memoizes grants and refs across
processes through `LISTEN`/`NOTIFY`, clone and commit move objects in batches, `commit --land`
lands in one round trip, a cold path history is walked in lockstep, warm `log` and `blame`
are answered from memory, `status` lists only changed directories on several threads, and the
land worker rebases the next request while the previous one's compare-and-swap is in flight;
next, a larger benchmark slice and a nightly run that fails on a regression. Alpha, not
production-ready; feedback on the derived-data design and the benchmark method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi ve ağaçları Postgres'te, blob'ları S3
uyumlu bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Her istek sunucunun imzaladığı bir token ve repoda (istenirse bir dizinle sınırlı) bir rol
istiyor; insanlar GitHub ile giriyor, sunucu aynı rollerle bir web arayüzü, bir HTTP/JSON API
ve ajanlar için bir MCP sunucusu sunuyor.
Gerçek bir açık kaynak reponun 600 commit'lik geçmişiyle git'e karşı ölçülen medyan değerlerde
aynı anda `land` eden 16 yazar 295 ms'de bitiriyor (git 3.022 ms), clone 582 ms (git 1.617 ms),
sparse clone 28 ms (git 513 ms), `status` 7 ms (git 161 ms), sıcak `blame` 8 ms (git 57 ms)
sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: tek bir commit + `land` 52 ms (git 43 ms), boş
bucket'a içe aktarma git'in 8,4 katı (nesneler zaten varken 1,9 katı), soğuk yol `log`'u 3,6,
soğuk `blame` 1,1 katı sürüyor.
Proje alfa aşamasında: kendi sunucunu TLS arkasında kurmak mümkün, ama etiketlenmiş bir sürüm,
barındırılan bir örnek ve önbellek katmanları henüz yok; çoğu repo için doğru araç hâlâ git.
