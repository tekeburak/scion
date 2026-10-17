# Announcement drafts: the real-repository benchmark (Phase 6.13)

**Status: draft. Not posted anywhere.** Written when Phase 6.13 (#77) and 6.15 (#81) had
merged. Every number below is a median from the README's [Benchmarks](../../README.md#benchmarks)
table (`docs/bench-real.csv`, commit `a0c0c7e`, three runs) or from `docs/FAQ.md`; re-check
them against the README before posting, because 6.16–6.18 may have moved on. Like the README,
these drafts describe the benchmark slice without naming the project it comes from.

## Show HN

**Title:** Show HN: Scion – a server-authoritative VCS on Postgres + S3, benchmarked against git

Scion is an open-source (Apache-2.0) client-server source control system written in Rust, for
one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents.
`scion-server` keeps metadata in Postgres and every blob and tree in an S3-compatible object
store; the `scn` CLI can `clone --sparse`, `status`, `log`, `diff`, `blame`, `commit` and
`land`; and `scion-ingest` imports a git history and verifies every imported tree against
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
Medians: sparse clone 70 ms against git's 547 ms, `status` 10 ms against 226 ms, commit +
`land` 34 ms against 45 ms, warm `blame` 11 ms against 59 ms, warm path `log` 24 ms against
24 ms. Where git wins: import is 5.4× git's time with the objects already in the bucket and
about 19× into an empty bucket, and a cold path `log` is 7.4× and a cold `blame` 3.5× git's,
because each pointer hop is an object-store round trip. What is missing: no authentication
(Phase 7), no cache or daemon tiers (Phase 4), no change requests or land worker (Phase 5),
no `scn init`, `update` or `checkout` yet (6.16–6.17), no Windows, offline mode or git wire
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
Sparse clone (70 vs 547 ms), `status` (10 vs 226 ms), warm `blame` (11 vs 59 ms) and commit +
`land` (34 vs 45 ms) come in at or below git; cold `log` and `blame` (7.4× and 3.5×) and import
(5.4× warm, about 19× into an empty bucket) do not, and the README says why. Getting cold blame
there meant deriving blame per file along its own version chain and reading those versions
concurrently. Cold derivation now holds at most a constant number of changesets in memory, and
`scion-server backfill` derives a type over the whole history ahead of the first request.

Status: the MVP works, git import works, the benchmark is published with its CSV. Next are
`scn init`, `update` and `checkout`, then change requests, auth and the cache tiers. Not
production-ready. Feedback on the derived-data design and the benchmark method is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, metaveriyi Postgres'te ve tüm nesneleri S3 uyumlu
bir depoda tutan sunucu merkezli bir sürüm kontrol sistemi.
Artık gerçek bir açık kaynak reponun 600 commit'lik geçmişini içe aktarıp git ile aynı makinede
karşılaştıran bir benchmark script'i var ve sonuçları CSV'siyle birlikte README'de.
Medyan değerlerle sparse clone 70 ms (git 547 ms), `status` 10 ms (git 226 ms), sıcak `blame`
11 ms (git 59 ms) ve commit + `land` 34 ms (git 45 ms) sürüyor.
Git'in önde olduğu yerler de açıkça yazılı: içe aktarma git'in 5,4 katı (boş bucket'a yaklaşık
19 katı), soğuk `log` 7,4 katı, soğuk `blame` 3,5 katı sürüyor; çünkü her işaretçi adımı nesne
deposuna bir gidiş-dönüş.
Kimlik doğrulama, önbellek katmanları, `scn update` ve change request'ler henüz yok; proje
üretime hazır değil ve çoğu repo için doğru araç hâlâ git.
