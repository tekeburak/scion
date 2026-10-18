# Benchmarks

The headline numbers, measured on a slice of a real repository's history, are in the
[README](../README.md#benchmarks) and come from [`scripts/bench-real`](../scripts/bench-real);
the CSV with every sample is [`bench-real.csv`](bench-real.csv). This page keeps the older
synthetic workloads, which exercise the client library directly against an in-process server.

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
