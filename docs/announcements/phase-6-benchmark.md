# Announcement drafts: the real-repository benchmark (Phase 6.13)

**Status: draft. Not posted anywhere.** Updated when Phase 6 had merged in full, through
6.18 (#85), and after Phase 5 through 5.11 (#114): change requests and the serial land worker,
`scn land`, `scn requests`, `scn log --landed`, server-side merge and revert, conflict markers
on `update`, and the 16-writer concurrent-land benchmark. Every number below is a median from
the README's [Benchmarks](../../README.md#benchmarks) table (`docs/bench-real.csv`, scion
commit `2c1a06b`, three runs, one full fresh run); re-check them against the README before
posting. Like the README, these drafts describe the benchmark slice without naming the project
it comes from.

## Show HN

**Title:** Show HN: Scion – a server-authoritative VCS on Postgres + S3; 16 concurrent lands in 1/5 of git's time

Scion is an open-source (Apache-2.0) client-server source control system written in Rust, for
one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata in Postgres and every blob and tree in an S3-compatible object
store; the `scn` CLI can `init`, `clone --sparse`, `status`, `log`, `diff`, `blame`, `commit`,
`land`, `update` (a server-side rebase onto `main`), `merge`, `revert` and `checkout`; and `scion-ingest` imports a git history and verifies every imported tree against
`git ls-tree`. It is an independent project based on the design in Anthropic's post
"Redesigning source control at Anthropic", and is not affiliated with Anthropic.

What is different from git: a commit is created on the server, so there is no push, no
packfile and no repository maintenance. A changeset records only the files that changed
against its first parent; trees, path history, blame and the ancestry skip list are derived
data, so a commit costs what it changes (one changed file in a 2,500-file tree rewrites 5 tree
nodes, which a test asserts). The only server refs are `main` and `release/*`. We depart from
the reference design on purpose: trees live in the object store, not the database, and hashing
uses our own canonical BLAKE3 encoding.

The numbers come from `scripts/bench-real`, which rebuilds 600 first-parent commits of a real
open-source repository's `library/` directory and times both tools on one 4-vCPU cloud VM
(compose Postgres + S3, server on loopback; git in a full clone, `scn` in a sparse clone).
Medians: 16 writers each landing a different file at once finish in 1,034 ms against
5,086 ms for git's fetch-rebase-push retry loop (p99 per writer 981 ms against 5,060 ms),
sparse clone 134 ms against git's 694 ms, `status` 13 ms against 263 ms, warm `blame` 19 ms
against 101 ms, warm path `log` 45 ms against 51 ms. Where git wins: a single commit + `land`
is 94 ms against git's 68 ms, because it goes through a change request and the land worker;
import into an empty bucket is 21× git's time (7.0× with the objects already in the bucket);
and a cold path `log` is 14× and a cold `blame` 4.2× git's, because each pointer hop is an
object-store round trip. What is missing: no authentication or access control yet, no cache
or daemon tiers, no release binaries, no Windows, offline mode or git wire compatibility. It is one machine and three runs, it is not production-ready, and for most
repositories git is the right tool.

## r/rust

**Title:** Scion, a server-authoritative VCS in Rust: benchmarked against git on a real history, 16 writers at once

Scion is a Cargo workspace for a client-server VCS aimed at one very large monorepo with
always-online clients: `scion-types` (ids, changesets, a hand-written canonical encoding hashed
with BLAKE3, frozen golden-hash fixtures), `scion-store` (Postgres and S3 behind two small
traits, plus an in-memory implementation for tests), `scion-derived` (root trees, path history,
blame, skip lists), `scion-server` (tonic gRPC), `scion-ingest` (git importer) and the `scn`
CLI.

The new part is `scripts/bench-real`: it rebuilds a pinned 600-commit slice of a real
repository, imports it into git and into Scion on Postgres + an S3-compatible server, and
times each workload with both, checking every checkout byte for byte outside the timers.
Sixteen writers landing at once (1,034 vs 5,086 ms in total), sparse clone (134 vs 694 ms),
`status` (13 vs 263 ms) and warm `blame` (19 vs 101 ms) come in below git; a single commit +
`land` (94 vs 68 ms), cold `log` and `blame` (14× and 4.2×) and import (21× into an empty
bucket, 7.0× warm) do not, and the README says why. Getting cold blame
there meant deriving blame per file along its own version chain and reading those versions
concurrently. Cold derivation now holds at most a constant number of changesets in memory, and
`scion-server backfill` derives a type over the whole history ahead of the first request.

Status: the MVP works, git import works, the benchmark is published with its CSV, and
`scn init`, `update` and `checkout` make the init → clone → commit → land loop run from a
Docker compose quickstart with no git repository. On the server, change requests now land
through one serial worker per repository: rebased onto `main`, moved in with one
compare-and-swap, conflicts reported by path, and a request interrupted by a restart lands
exactly once, and `scn land` submits the head and waits for that outcome, so two clones land
at the same time with no `update` in between; `scn requests` lists them and `scn log
--landed` shows which request landed each changeset and when. Merges (`scn merge`, e.g. a
`release/*` fix into a head) and reverts (`scn revert`) run on the server and keep blame on
the changesets that wrote the lines, and `scn update --conflicts` writes conflict markers
instead of refusing. Next are access control, release binaries and the cache tiers. Not
production-ready. Feedback on the derived-data design and the benchmark method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi Postgres'te ve tüm nesneleri S3 uyumlu
bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Artık gerçek bir açık kaynak reponun 600 commit'lik geçmişini içe aktarıp git ile aynı makinede
karşılaştıran bir benchmark script'i var ve sonuçları CSV'siyle birlikte README'de.
Medyan değerlerle aynı anda `land` eden 16 yazar 1.034 ms'de bitiriyor (git 5.086 ms), sparse
clone 134 ms (git 694 ms), `status` 13 ms (git 263 ms), sıcak `blame` 19 ms (git 101 ms) sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: tek bir commit + `land` 94 ms (git 68 ms), boş
bucket'a içe aktarma git'in 21 katı (nesneler zaten varken 7,0 katı), soğuk `log` 14 katı,
soğuk `blame` 4,2 katı sürüyor; çünkü her işaretçi adımı nesne deposuna bir gidiş-dönüş.
Kimlik doğrulama ve önbellek katmanları henüz yok; `scn land` artık bir change request açıyor
ve sunucudaki seri land worker onu `main`'e rebase edip işliyor, `scn requests` bu istekleri
listeliyor, `scn log --landed` her changeset'i hangi isteğin ne zaman işlediğini gösteriyor,
merge ve revert de sunucuda çalışıyor, ama proje üretime hazır değil
ve çoğu repo için doğru araç hâlâ git.
