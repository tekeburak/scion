# Benchmarks

The headline numbers, measured on a slice of a real repository's history, are in the
[README](../README.md#benchmarks) and come from [`scripts/bench-real`](../scripts/bench-real);
the CSV with every sample is [`bench-real.csv`](bench-real.csv). This page keeps the older
synthetic workloads, which exercise the client library directly against an in-process server.

## Where the time goes

`SCION_TRACE=1 scripts/bench-real 1` makes every Scion process print one line per finished
stage (`crates/scion-proto/src/trace.rs`) and sums them per workload into
`target/bench-real/results-trace.csv`, as `client` (`scn`, or `scion-ingest`, which runs the
shared create path in process) and `server` stages. Spans of concurrent calls add up, so a
stage can exceed its workload's wall time; the critical-path stage is the one to attack.
The baseline below is the 2026-10-21 run in [`bench-real.csv`](bench-real.csv) (medians of 3)
with the stages of one traced run on the same machine; the import rows are from the Phase 10.4
run (mapper on two threads). Rows slower than git:

| Row | scion / git | Most expensive stage | What it is |
| --- | ---: | --- | --- |
| import, cold | 8.7× | blob `put`: 76 s summed over 7,955 uploads in flight; `create` is 4.4 s (wall 11.6 s) | One object-store `PUT` per blob (`store` is one `INSERT` per batch since 10.3); the stack serves about 1,100 `PUT`/s, so blobs alone take about 7 s. See #168. |
| import, warm | 2.2× | `has_blobs`: 24 s summed, one `HEAD` per blob (7,955) | Mapping went from 4.9 s on one thread to about 1.8 s of work on two (10.4: rename scoring counts each file's lines once, delta-base cache); what is left is one `HEAD` per blob for a repository whose blobs another one stored. See #168. |
| path log, cold | 7.4× | `walk`: 102 ms of the 164 ms `PathLog` | One object-store read per path-history hop on an empty memo; `read` (62 ms) was 428 `get_changeset` queries, 32 at a time, and is one `get_changesets` since 10.2. |
| blame, cold | 3.8× | `Blame`: 176 ms | Deriving blame along the file's version chain: changeset and blob reads, one round trip per version, on an empty memo. |
| commit + land | 1.6× | `CreateChangeset`: 12 ms, then `SubmitChangeRequest`: 13 ms | Client round trips: `HasBlobs` (1.3 ms) and `PutBlobs` (7.7 ms, the object-store `PUT`) since 10.5, then `CreateChangeset`, `SubmitChangeRequest` and waiting on the request; inside the create, the derived objects' `store` (9.5 ms) and the skip list (9.2 ms) run alongside the row `insert` (7.5 ms). |

Every other row is already faster than git. The traced run also shows where the next steps
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
