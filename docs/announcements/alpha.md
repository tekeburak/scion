# Announcement drafts: the alpha (access control, web interface, current benchmark)

**Status: draft. Not posted anywhere.** Written after Phase 7.11 (#142) and Phase 11.1 (#144)
merged: access control with GitHub sign-in and directory-scoped roles, the hermetic simulation,
fuzz targets for every untrusted-bytes boundary, and the web interface's read-only pages. It
replaces [`phase-6-benchmark.md`](phase-6-benchmark.md), whose numbers and "no access control"
line are out of date. Every number below is a median from the README's
[Benchmarks](../../README.md#benchmarks) table (`docs/bench-real.csv`, scion commit `d3ebd05`,
three runs, 2026-10-21); re-check them against the README before posting. Like the README,
these drafts describe the benchmark slice without naming the project it comes from.

## Show HN

**Title:** Show HN: Scion – server-authoritative source control on Postgres and S3 for agent-driven monorepos

Scion is an open-source (Apache-2.0) client-server version control system written in Rust,
for one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata in Postgres and every blob and tree in an S3-compatible object
store. The `scn` CLI can `init`, `clone --sparse`, `status`, `log`, `diff`, `blame`,
`commit`, `land` (a change request that one serial worker per repository rebases onto `main`
on the server), `update`, `merge`, `revert` and `checkout`; `scion-ingest` imports a git
history and verifies every imported tree against `git ls-tree`. Every request needs a token
signed by the server and a role in the repository, optionally limited to a directory; people
sign in with GitHub (`scn login`) and agents use service tokens. The server also serves a
read-only web interface (files, log, blame, diffs, change requests) under the same roles. It
is an independent project inspired by Anthropic's post "Redesigning source control at
Anthropic", and is not affiliated with Anthropic.

What is different from git: a commit is created on the server, so there is no push, no
packfile and no repository maintenance. A changeset records only the files that changed
against its first parent; trees, path history, blame and the ancestry skip list are derived
data, so a commit costs what it changes (one changed file in a 2,500-file tree rewrites 5 tree
nodes, which a test asserts). The only server refs are `main` and `release/*`; work in progress
is an anonymous head. A seeded, hermetic simulation runs many clients landing, merging and
reverting while land workers stop and the server restarts, and checks that `main` stays linear
and nothing landed is lost or written twice.

The numbers come from `scripts/bench-real`, which rebuilds 600 first-parent commits of a real
open-source repository's `library/` directory and times both tools on one 4-vCPU cloud VM
(compose Postgres + S3, server on loopback; git in a full clone, `scn` in a sparse clone).
Medians: 16 writers each landing a different file at once finish in 594 ms against 3,165 ms for
git's fetch-rebase-push retry loop (p99 per writer 571 ms against 3,138 ms), sparse clone 97 ms
against 502 ms, `status` 8 ms against 172 ms, warm `blame` 11 ms against 45 ms. Where git wins:
a single commit + `land` is 54 ms against 34 ms, because it goes through a change request;
import into an empty bucket takes 17× git's time (5.4× with the objects already stored); and a
cold path `log` is 7.4× and a cold `blame` 3.8× git's, because each pointer hop is an
object-store round trip. What is missing: released binaries, the cache and daemon tiers,
browser sign-in for the web interface, and by design Windows, offline mode and git wire
compatibility. It is alpha software measured on one machine, and for most repositories git is
the right tool.

## r/rust

**Title:** Scion, a server-authoritative VCS in Rust on Postgres + S3: access control, a fuzzed boundary and a benchmark against git

Scion is a Cargo workspace for a client-server VCS aimed at one very large monorepo with
always-online clients: `scion-types` (ids, changesets, a hand-written canonical encoding hashed
with BLAKE3, frozen golden-hash fixtures), `scion-store` (Postgres and S3 behind two small
traits, plus an in-memory implementation for tests), `scion-derived` (root trees, path history,
blame, skip lists), `scion-core` and `scion-diff3` (diff, merge-base, rebase, three-way merge),
`scion-server` (tonic gRPC, the land worker), `scion-web` (server-rendered pages with `maud`,
no JavaScript), `scion-ingest` (git importer with `gix`), `scion-sim` and the `scn` CLI.

Two recent pieces may interest Rust folks. `scion-sim` runs the real server in process on the
memory stores with seeded clients and injected faults (land workers stopped, server restarts),
and 200 seeds run on every `cargo test`. Fuzz targets cover every boundary that reads untrusted
bytes (changeset, tree and derived-object decoding, tokens, the three-way merge and the write
requests), each a plain function in `scion-sim`, so the checked-in corpus replays under stable
`cargo test` and `cargo-fuzz` drives the same code on nightly; the properties go beyond "no
panic": canonical decoding re-encodes to the same bytes, and no write request overwrites a
stored object.

`scripts/bench-real` imports a pinned 600-commit slice of a real repository into git and into
Scion on Postgres + an S3-compatible server and times each workload with both, checking every
checkout byte for byte outside the timers. Sixteen writers landing at once (594 vs 3,165 ms),
sparse clone (97 vs 502 ms), `status` (8 vs 172 ms) and warm `blame` (11 vs 45 ms) come in
below git; a single commit + `land` (54 vs 34 ms), cold path `log` and `blame` (7.4× and 3.8×)
and import (17× into an empty bucket, 5.4× warm) do not, and the README says why. Next is a
performance pass that moves trees and history pages into Postgres, streams the object plane
and makes landing one round trip, then browser sign-in, an HTTP/JSON API and an MCP server for
agents. Alpha, not production-ready; feedback on the derived-data design and the benchmark
method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi Postgres'te ve tüm nesneleri S3 uyumlu
bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Her istek sunucunun imzaladığı bir token ve repoda (istenirse bir dizinle sınırlı) bir rol
istiyor; insanlar GitHub ile `scn login` üzerinden giriyor, sunucu da aynı rollerle salt okunur
bir web arayüzü sunuyor.
Gerçek bir açık kaynak reponun 600 commit'lik geçmişiyle git'e karşı ölçülen medyan değerlerde
aynı anda `land` eden 16 yazar 594 ms'de bitiriyor (git 3.165 ms), sparse clone 97 ms (git
502 ms), `status` 8 ms (git 172 ms), sıcak `blame` 11 ms (git 45 ms) sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: tek bir commit + `land` 54 ms (git 34 ms), boş
bucket'a içe aktarma git'in 17 katı (nesneler zaten varken 5,4 katı), soğuk yol `log`'u 7,4
katı, soğuk `blame` 3,8 katı sürüyor.
Proje alfa aşamasında: hazır derlenmiş sürümler ve önbellek katmanları henüz yok, çoğu repo
için doğru araç hâlâ git.
