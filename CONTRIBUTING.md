# Contributing

Thanks for your interest. Scion is developed by its maintainer, and issues and pull requests
from outside the project are not accepted for now; the repository is limited to collaborators.
You are welcome to read, build, run and fork it under the license. The rest of this page is
how changes are made inside the project.

The phase table and the single-PR steps in `docs/ROADMAP.md` are the source of truth for what
is being worked on; the README describes what works today. `CLAUDE.md` holds the working rules
every change follows, for people and agents alike, together with `docs/ARCHITECTURE.md` and
`docs/DATA_MODEL.md`.

## Ground rules

- **The numbered invariants in `CLAUDE.md` are not changed in a PR.** A change that would bend
  one starts as an issue, not as code.
- **Hash stability is sacred.** Hashing goes only through `scion_types::canonical`. The golden
  fixtures in `crates/scion-types/tests/golden/` are frozen: do not edit or regenerate them.
  If the encoding must change, bump `FORMAT_VERSION`, keep the old decoder, and open an issue
  first.
- **No I/O in `scion-types` or `scion-core`.** Algorithms are written against the store traits
  and tested with the in-memory store.
- **Bumping a derived type's `VERSION` invalidates its stored values for every repository.**
  Such a PR runs `scion_server::backfill` for that type in its end-to-end test, and its
  description tells operators to run `scion-server backfill <repo> <type>` once it is deployed.
- **Every store implementation passes the conformance suite** (`scion_store::conformance_tests!`).
- **Server-side refs are only `main` and `release/*`** (`is_allowed_ref_name`). Do not add
  branch-like features.
- **Keep it simple.** No defensive checks between our own services, no abstraction with a
  single use, no configuration the roadmap does not call for. See "Simplicity rules" in
  `CLAUDE.md`.

## Workflow

1. Open an issue for anything that touches the data model, the wire protocol or the cache
   tiering, or that adds a crate, table, RPC, derived type or dependency with a build script.
2. One PR is one roadmap step (`Phase N.M: …`) or one self-contained fix. Branch from `main`;
   there are no direct pushes to `main`.
3. The PR description starts with the two lines the PR template asks for:
   `Invariant kept:` and `Could be simpler:`.
4. Commit messages explain *why*; the diff shows *what*.
5. CI must be green. Reviews look at intent and invariants first, lines second.
6. A pull request from a fork is reviewed and merged by the maintainer alone; agents and
   automation never merge, approve or run CI for one.

## Checks

Every command in the "Commands" section of `CLAUDE.md` must pass before a commit; CI runs the
same set on Linux, on self-hosted runners (the header of `.github/workflows/ci.yml` says how
they behave), and macOS is covered by developing on it. The short version:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check bans licenses sources advisories
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

`scion-proto` needs `protoc` on `PATH`; `scion-ingest`'s tests need `git`. Integration tests
need the compose stack in `deploy/docker-compose.yml`.

Figures in `docs/assets/` are generated: edit `scripts/render-assets.py`, run it, and check
with `python3 scripts/render-assets.py --check`.

## Code style

`rustfmt.toml` and the workspace lints in `Cargo.toml` are authoritative. Pedantic clippy is on;
allow a lint locally with a comment explaining why rather than disabling it globally.
