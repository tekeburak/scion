# Announcement drafts: the MVP (Phase 3)

**Status: superseded by [`alpha.md`](alpha.md); its numbers and status are out of date. Not
posted anywhere.** Written when Phase 3.10 merged (#28). Every claim
below is checked against the README and `docs/`; re-check them before posting, because
Phase 6 may have moved on. The only numbers used are the synthetic benchmark medians in the
README (in-memory store, not Postgres + S3) and the tree-node count a test asserts.
Real-repository results (Phase 6.13) are not published yet, so they are not quoted here.

## Show HN

**Title:** Show HN: Scion – server-authoritative source control for agent-driven monorepos

Scion is an open-source (Apache-2.0), client-server version control system written in Rust,
for one specific case: a single very large monorepo whose clients (CI jobs, agent sandboxes,
developer machines) are always online and whose commits come mostly from coding agents. The
MVP works end to end: `scion-server` stores metadata in Postgres and every blob and tree in
an S3-compatible object store, and the `scn` CLI can `clone --sparse`, `status`, `log`,
`diff`, `blame`, `add`, `commit` and `land` (fast-forward `main`). It is an independent
project based on the design in Anthropic's post "Redesigning source control at Anthropic",
and is not affiliated with Anthropic.

What is different from git: the server is authoritative and a commit is created on it
directly, so there is no push, no packfile and no repository maintenance. A changeset records
only the files that changed against its first parent. Trees, path history, blame and the
ancestry skip list are derived data, computed from the changeset, so a commit costs what it
changes: one changed file in a 2,500-file tree rewrites 5 tree nodes (a test asserts this).
The only server refs are `main` and `release/*`. All other work is an anonymous head
identified by an immutable id. We also depart from the reference design on purpose: trees
live in the object store rather than the database, and hashing uses our own canonical BLAKE3
encoding.

Numbers, with their limits: on a synthetic 10,000-file workload, importing a directory takes
1,230 ms against git's 4,021 ms, and a one-directory clone takes 305 ms against 1,090 ms.
These are medians on a 4-vCPU cloud VM, with the server in-process and the in-memory store,
not Postgres + S3. The README lists every caveat. What is missing: no authentication (Phase 7),
no cache or daemon tiers yet (Phase 4), no change requests or server-side rebase (Phase 5), no
Windows, no offline mode and no git wire compatibility. A git importer exists. A benchmark
against git on a slice of a real repository's history is in progress, and its results are not
published yet. This is not production-ready, and for most repositories git is the right tool.

## r/rust

**Title:** Scion: a server-authoritative VCS in Rust on Postgres + S3 (MVP, looking for feedback)

I've been building Scion, a client-server source control system for one very large monorepo
with always-online clients and an agent-driven commit rate. It is a Cargo workspace:
`scion-types` (ids, changesets, a hand-written canonical encoding hashed with BLAKE3, and
frozen golden-hash fixtures), `scion-store` (Postgres via SQL and S3 behind two small traits,
plus an in-memory implementation for tests), `scion-derived` (root trees, path history, blame,
skip lists), `scion-server` (tonic gRPC) and the `scn` CLI. CI runs clippy with
`-D warnings`, `cargo deny` and an end-to-end test against Postgres + an S3-compatible server
on Linux and macOS.

A few choices may interest Rust folks. Hashing never touches prost or serde output, only the
canonical encoder, so a dependency upgrade cannot change an id. Derived data is computed
recursively from parents, and the tests check that cost follows the change: one file changed
in a 2,500-file tree rewrites 5 tree nodes. Objects are immutable and append-only, which keeps
the store code small.

Status: the MVP (`clone --sparse`, `log`, `diff`, `blame`, `commit`, `land`) works. Git import
works, and a real-repository benchmark against git is in progress (no published results yet).
Cache tiers, auth and change requests are planned. The synthetic benchmark in the README uses
the in-memory store, and its commit row is not like-for-like with git. Not production-ready.
Feedback on the crate layout and the store traits is very welcome.

## Türkçe (5 cümle)

Scion, ajanların commit attığı ve istemcilerin hep çevrimiçi olduğu tek bir çok büyük monorepo
için Rust ile yazılmış, Apache-2.0 lisanslı, sunucu merkezli bir sürüm kontrol sistemi.
MVP çalışıyor: `scion-server` metaveriyi Postgres'te, blob ve ağaçları S3 uyumlu bir nesne
deposunda tutuyor; `scn` ile `clone --sparse`, `log`, `diff`, `blame`, `commit` ve `land`
yapılabiliyor.
Git'ten farkı şu: commit doğrudan sunucuda oluşuyor, push ya da packfile yok; ağaçlar, blame ve
geçmiş türetilmiş veri olduğu için bir commit yalnız değiştirdiği kadar iş yapıyor (2.500
dosyalık ağaçta tek dosya değişikliği 5 ağaç düğümü yazıyor, bunu bir test doğruluyor).
Sentetik ölçümde 10.000 dosyalık bir dizinin içe aktarılması git'te 4.021 ms, Scion'da
1.230 ms sürüyor; bu ölçüm bellek içi depoyla alındı, gerçek bir repo üzerindeki sonuçlar
henüz yayımlanmadı.
Kimlik doğrulama, önbellek katmanları ve change request'ler henüz yok, yani proje üretime
hazır değil; çoğu repo için doğru araç hâlâ git.
