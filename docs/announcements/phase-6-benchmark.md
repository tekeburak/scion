# Announcement drafts: the real-repository benchmark (Phase 6.13)

**Status: draft. Not posted anywhere.** Updated when Phase 6 had merged in full, through
6.18 (#85), after the server side of change requests (5.1 to 5.4, #93), and after `scn land`
started submitting change requests (5.5, #99). Every number below is a median from the README's [Benchmarks](../../README.md#benchmarks)
table (`docs/bench-real.csv`, commit `ba08ff9`, three runs); re-check them against the README
before posting. The commit + `land` row was measured at `ba08ff9`, before `scn land` went
through change requests; re-run `scripts/bench-real` and re-check that row before quoting it.
Like the README,
these drafts describe the benchmark slice without naming the project it comes from.

## Show HN

**Title:** Show HN: Scion – a server-authoritative VCS on Postgres + S3, benchmarked against git

Scion is an open-source (Apache-2.0) client-server source control system written in Rust, for
one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata in Postgres and every blob and tree in an S3-compatible object
store; the `scn` CLI can `init`, `clone --sparse`, `status`, `log`, `diff`, `blame`, `commit`,
`land`, `update` (a server-side rebase onto `main`) and `checkout`; and `scion-ingest` imports a git history and verifies every imported tree against
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
Medians: sparse clone 75 ms against git's 646 ms, `status` 11 ms against 292 ms, commit +
`land` 44 ms against 44 ms, warm `blame` 11 ms against 74 ms, warm path `log` 26 ms against
29 ms. Where git wins: import into an empty bucket is 15× git's time (5.3× with the objects
already in the bucket), and a cold path `log` is 8.7× and a cold `blame` 4.0× git's, because
each pointer hop is an object-store round trip. What is missing: no authentication
(Phase 7), no cache or daemon tiers (Phase 4), no listing of change requests, server-side
merge or revert yet (Phase 5), no release binaries yet (Phase 9), no Windows, offline mode or git wire
compatibility. It is one machine and three runs, it is not production-ready, and for most
repositories git is the right tool.

## r/rust

**Title:** Scion, a server-authoritative VCS in Rust: first benchmark against git on a real history

Scion is a Cargo workspace for a client-server VCS aimed at one very large monorepo with
always-online clients: `scion-types` (ids, changesets, a hand-written canonical encoding hashed
with BLAKE3, frozen golden-hash fixtures), `scion-store` (Postgres and S3 behind two small
traits, plus an in-memory implementation for tests), `scion-derived` (root trees, path history,
blame, skip lists), `scion-server` (tonic gRPC), `scion-ingest` (git importer) and the `scn`
CLI.

The new part is `scripts/bench-real`: it rebuilds a pinned 600-commit slice of a real
repository, imports it into git and into Scion on Postgres + an S3-compatible server, and
times each workload with both, checking every checkout byte for byte outside the timers.
Sparse clone (75 vs 646 ms), `status` (11 vs 292 ms), warm `blame` (11 vs 74 ms) and commit +
`land` (44 vs 44 ms) come in at or below git; cold `log` and `blame` (8.7× and 4.0×) and import
(15× into an empty bucket, 5.3× warm) do not, and the README says why. Getting cold blame
there meant deriving blame per file along its own version chain and reading those versions
concurrently. Cold derivation now holds at most a constant number of changesets in memory, and
`scion-server backfill` derives a type over the whole history ahead of the first request.

Status: the MVP works, git import works, the benchmark is published with its CSV, and
`scn init`, `update` and `checkout` make the init → clone → commit → land loop run from a
Docker compose quickstart with no git repository. On the server, change requests now land
through one serial worker per repository: rebased onto `main`, moved in with one
compare-and-swap, conflicts reported by path, and a request interrupted by a restart lands
exactly once, and `scn land` submits the head and waits for that outcome, so two clones land
at the same time with no `update` in between. Next are listing requests, "when did this
land", server-side merge and revert, then auth, release binaries and the cache tiers. Not
production-ready. Feedback on the derived-data design and the benchmark method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi Postgres'te ve tüm nesneleri S3 uyumlu
bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Artık gerçek bir açık kaynak reponun 600 commit'lik geçmişini içe aktarıp git ile aynı makinede
karşılaştıran bir benchmark script'i var ve sonuçları CSV'siyle birlikte README'de.
Medyan değerlerle sparse clone 75 ms (git 646 ms), `status` 11 ms (git 292 ms), sıcak `blame`
11 ms (git 74 ms) ve commit + `land` 44 ms (git 44 ms) sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: boş bucket'a içe aktarma git'in 15 katı (nesneler
zaten varken 5,3 katı), soğuk `log` 8,7 katı, soğuk `blame` 4,0 katı sürüyor; çünkü her
işaretçi adımı nesne deposuna bir gidiş-dönüş.
Kimlik doğrulama ve önbellek katmanları henüz yok; `scn land` artık bir change request açıyor
ve sunucudaki seri land worker onu `main`'e rebase edip işliyor, ama proje üretime hazır değil
ve çoğu repo için doğru araç hâlâ git.
