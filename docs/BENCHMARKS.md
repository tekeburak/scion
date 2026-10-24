# Benchmarks

The headline numbers, measured on a slice of a real repository's history, are in the
[README](../README.md#benchmarks) and come from [`scripts/bench-real`](../scripts/bench-real);
the CSV with every sample is [`bench-real.csv`](bench-real.csv). This page keeps the older
synthetic workloads, which exercise the client library directly against an in-process server.

## Other implementations

`BENCH_PEERS=<dir>[:<dir>...] scripts/bench-real` also runs every workload against checkouts
of other public implementations of the same design, on the same slice and machine. The script
builds each checkout with its own toolchain, picks the adapter its binaries match, and records
its rows under that adapter's neutral label (`impl-a`, `impl-b`), with `unsupported` for a
workload it cannot express; the CSV header records the commit each checkout is at. Without
`BENCH_PEERS` it measures git and Scion only, and the README table says so.

## Larger slice and nightly

`BENCH_SLICE=large scripts/bench-real` runs the same workloads on about 6,000 first-parent
commits of the same project's `compiler/` directory (since 2023-02-01, 2,978 files at the tip),
with a sparse clone of `rustc_middle/`, a path log of `rustc_middle/src` and blame of
`rustc_middle/src/ty/context.rs`; its CSV is [`bench-real-large.csv`](bench-real-large.csv).

The `bench` job in `.github/workflows/ci.yml` runs both slices every night and when started by
hand (`workflow_dispatch`), against its own Postgres and S3 services (`DATABASE_URL` and
`AWS_ENDPOINT` given, so the script starts no compose stack). It keeps each run's CSV as an
artifact, then [`scripts/bench-check.py`](../scripts/bench-check.py) compares it with the CSV
on `main`: a row whose scion / git ratio grew more than 20%, or that Scion failed, turns the
job red. Ratios, not milliseconds, because the CI machine is not the one the CSVs were taken
on; a change of under 5 ms is not counted, since a 6 ms row moves by that much between runs.

How much a ratio moves without a code change, measured with `scripts/bench-check.py` on one
4-vCPU VM (2.80 GHz) on 2026-10-24: three `bench-real 3` runs of the small slice, two on
e1ee623 and one on e3687a5 (whose server differs only in the land worker), differ from each
other by up to 25% on `import warm`, `log warm`, `blame cold` and `log warm client cold`, and
from the CSV on `main`, taken on a 2.10 GHz VM, by up to 64% on the cold `log` and `blame`
rows. A 20% bar on medians of three runs is crossed by that noise alone.

## Where the time goes

`SCION_TRACE=1 scripts/bench-real 1` makes every Scion process print one line per finished
stage (`crates/scion-proto/src/trace.rs`) and sums them per workload into
`target/bench-real/results-trace.csv`, as `client` (`scn`, or `scion-ingest`, which runs the
shared create path in process) and `server` stages. Spans of concurrent calls add up, so a
stage can exceed its workload's wall time; the critical-path stage is the one to attack.
The baseline below is the 2026-10-21 run in [`bench-real.csv`](bench-real.csv) (medians of 3)
with the stages of one traced run on the same machine; the import rows are from the Phase 10.4
run (mapper on two threads), the path log and blame rows from the Phase 10.7 run. Rows slower
than git:

| Row | scion / git | Most expensive stage | What it is |
| --- | ---: | --- | --- |
| import, cold | 8.7× | blob `put`: 76 s summed over 7,955 uploads in flight; `create` is 4.4 s (wall 11.6 s) | One object-store `PUT` per blob (`store` is one `INSERT` per batch since 10.3); the stack serves about 1,100 `PUT`/s, so blobs alone take about 7 s. See #168. |
| import, warm | 2.2× | `has_blobs`: 24 s summed, one `HEAD` per blob (7,955) | Mapping went from 4.9 s on one thread to about 1.8 s of work on two (10.4: rename scoring counts each file's lines once, delta-base cache); what is left is one `HEAD` per blob for a repository whose blobs another one stored. See #168. |
| path log, cold | 4.0× | `walk`: about 60 ms of the 85 ms `PathLog` (10.7) | Every page read now comes from Postgres (10.3), and since 10.7 the segments of the chain and its next anchor advance together, one `get_pages` per step: about 43 queries for 428 changes instead of about 450 single-page queries queued on a 16-connection pool. What is left is the chain's shape: 27 anchors read one after another, then the last segment's 16 steps, each about 1–1.5 ms for up to 230 kB of pages (about 5 MB in all). Since the second 10.7 step pages (and trees) are stored with lz4 instead of Postgres' default pglz, which makes each step's read about a fifth faster (0.95 → 0.75 ms for 16 pages, measured on the bench's own pages), and `read` is one query that returns the stored canonical bytes as they are instead of decoding 428 changesets and encoding them again. Together, measured on `main` and on this change in one session: 101 → 87 ms. |
| blame, cold | 1.03× | `attribute`: about 60 ms, the requested file's own version chain | Until 10.7 the reply waited on the blame node of the changeset that last touched `alloc/src/vec/mod.rs`, which holds every file that changeset changed: 272 versions along 13 chains, each version one object-store `GET` and one diff, with every query waiting behind RustFS. The reply now waits on that file's chain only; a task stores the node afterwards, one file and one version at a time, and the server remembers the reply meanwhile. |
| commit + land | 1.2× | `CommitAndLand`: 23 ms, then `PutBlobs`: 7 ms | Since 10.6 `scn commit --land` is one process and three RPCs (`HasBlobs`, `PutBlobs`, `CommitAndLand`) where `commit` + `land` were two processes and four. Inside `CommitAndLand`, the create is 8 ms (the skip list's 7 ms of ancestor reads on a memo the importer, not the server, filled), then about six Postgres commits in a row (insert, pointers, submit, claim, `main`'s CAS, finish; 1–1.5 ms each); the path history is stored after the reply. `PutBlobs` is RustFS's `PUT` of one small blob. On a warm server and a small repository the whole command is about 25 ms. |

Every other row is already faster than git. Warm path log became one of them in 10.7: a head's history never
changes, so the server remembers each `PathLog` reply (64 MiB, `moka`) and a repeated one
reads nothing from Postgres; what is left of the 18 ms is the `scn` process, which decodes,
hashes and prints 428 changesets (about 1 MB). The object store now also keeps blobs up to
256 KiB in its memo, so a warm `blame` no longer fetches a long source file from S3. The traced run also shows where the next steps
start: a clone's 2,878 RPCs each paid an `auth` grants query (22.5 s summed). Since 10.2 a
listening server memoizes each repository's grants and refs, and every write to them sends a
Postgres notification that drops the entries, so a clone makes one grants query; clone went
from 1,436 to 1,153 ms. Since 10.5 a clone no longer makes 2,330 `GetBlob` and 547 `GetTree`
calls, one per file and directory: `GetObjects` returns the root with every tree beneath it in
one call and the blobs 256 per call, so the full clone is 12 RPCs (`GetRef`, one for the trees,
ten for the blobs) and a counting test holds it there. A sparse clone fetches the trees on the
way to each listed path one level at a time and each listed directory with everything beneath
it at once. `scn commit` hashes its files locally, asks `HasBlobs`, and sends only the missing
ones in one `PutBlobs` stream.

The land worker's own steps have spans too (`claim`, `scope`, `main`, `rebase`, `cas`,
`finish`, and `worker` around one land). In a traced `land 16 writers` run its serial time was
about 16 ms per request: 10.5 ms to land (rebase 6.6 ms, of which creating the rebased changeset
is most), 3.7 ms to claim and 1.8 ms to finish. Two things on that path were not needed. Every
move of `main` dropped the repository's grants memo with its refs, so the worker read the
submitter's grants from Postgres on every request (2 ms); refs and grants now have memos and
notification channels of their own. And the worker derived the path history of each rebased
changeset before moving `main`, which nothing a land waits on reads; a task stores it once the
request is finished. The worker's summed time went from 168 to 129 ms of the run's 358 ms
traced wall time; untraced, over six runs per side on one machine, `land 16 writers` measured
372 ms against 392 ms on `main`, inside that machine's run-to-run noise. What is left per
land is about six Postgres commits in a row, each with its WAL flush.

Since 10.9 two of those commits are off the worker's line: while one request's CAS and finish
are in flight, the worker claims the next request and rebases it onto the `main` that CAS will
make; a lost CAS drops what was prepared. Each finished request is also sent to the clients
waiting on it, where before every finish woke every waiting `CommitAndLand` and each read its
request from Postgres again, on the pool the worker uses; a counting test holds a waiting
client at two reads. In one session, alternating `bench-real 3` runs gave `land 16 writers`
358 and 373 ms on `main` against 342 and 310 ms with the pipeline (0.092× and 0.083× git, p99
0.089× and 0.080×).

Since 10.8 a clean `status` on the sparse clone (322 files, about 70 directories) lists no
directory and reads no file: it makes one `statx` per directory and file, on several threads
once there are more than 128 of either, about 0.7 ms in all. The index records each file's
ctime and inode with its mtime, and each directory's mtime when its listing held only what the
index puts there; a recorded time counts only when it is older than the index file, so an edit
made in the same clock tick as the checkout is still rehashed, and a proptest over random edit
sets checks the result against a full re-hash. The row is now about the cost of starting a
process: `scn status` takes about 3.3 ms when run alone on the benchmark machine, of which
`scn --version` takes 2.1 ms and `/bin/true` 1.4 ms, and `bench-real`'s timer reads 4 ms for an
empty command. So 7 ms against git's 172 ms is 0.04×, and the 0.03× target (about 5 ms) sits
below what the timer measures for any process. Since 10.8, `bench-real` also runs `scn` in its
working copy from its own shell, as git runs with `-C`, instead of through `sh -c "cd … && scn"`,
which had charged every `scn` row in a working copy a second shell.

## Synthetic workloads

The [benchmark runner](../crates/scion-client/examples/benchmark.rs) compares Scion with git on
four synthetic workloads: directory import, a one-file commit, a full clone and a
one-directory clone.

Medians measured 2026-10-15 on a 4-vCPU Intel Xeon @ 2.80 GHz cloud VM with 15 GiB RAM,
Linux 6.18, Rust 1.97.0 and git 2.43.0, at commit `b9a8eb4` plus the runner. Each file holds
4 KiB of seeded pseudorandom bytes, spread across 10 directories. The 1,000-file case uses
seven runs and the 10,000-file case uses five. All times are in milliseconds; lower is better.

| Scenario | Git, 1k files | Scion, 1k | Git, 10k files | Scion, 10k |
| --- | ---: | ---: | ---: | ---: |
| Import directory | 492.4 | 111.8 | 4,021.3 | 1,230.4 |
| Commit one changed file | 28.5 | 1.8\* | 35.5 | 3.5\* |
| Full clone / checkout | 806.4 | 529.0 | 7,371.8 | 5,113.5 |
| One-directory clone / checkout | 417.7 | 144.1 | 1,089.8 | 304.5 |

Read these numbers with their limits:

- **\*Commit is not like-for-like.** The runner predates `scn commit`, so the Scion figure
  times only the server side: `PutBlob` + `CreateChangeset` (with eager derivation) +
  `MoveRef`. `git commit -a` also scans the working copy. Treat this row as a lower bound for
  Scion; the real-repository benchmark's `commit + land` row is the like-for-like one.
- **Storage.** The server runs in-process over loopback gRPC (plain HTTP/2, no TLS) with the
  **in-memory** store, not Postgres + S3. Git clones over the local filesystem (`--no-local`;
  `file://` with `--filter=blob:none --sparse` for the one-directory case).
- **No cache tier.** The client talks to `scion-server` directly. OS page caches are not
  flushed between runs.
- These are synthetic local measurements of this implementation, not production throughput.

What each workload does:

- **Import:** git runs `init` + `add -A` + `commit`. Scion reads every file, uploads its blob
  (64 in flight), then calls `CreateChangeset` and `MoveRef` to create `main`.
- **Full clone:** `git clone --no-local` against `scion_client::clone` with the full profile.
- **One-directory clone:** a git partial clone plus `sparse-checkout set dir0000`, against
  `scion_client::clone` with a one-directory sparse profile. Scion fetches only the trees on
  the path and the blobs beneath it.
- Every checkout is verified byte for byte outside the timers.

Reproduce (needs git and `protoc` on `PATH`):

```sh
cargo run --release -p scion-client --example benchmark -- 100 7    # 1,000 files
cargo run --release -p scion-client --example benchmark -- 1000 5   # 10,000 files
cargo run --release -p scion-client --example benchmark -- 2 1      # quick smoke run
```
